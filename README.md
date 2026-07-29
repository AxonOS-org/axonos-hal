<div align="center">

# axonos-hal

### The contract between AxonOS and silicon.

[![Tests](https://img.shields.io/badge/tests-52%20passing-0d7a5f?style=flat-square)](#verification)
[![no_std](https://img.shields.io/badge/no__std-yes-0a4a8f?style=flat-square)](#constraints)
[![unsafe](https://img.shields.io/badge/unsafe-forbidden-0a4a8f?style=flat-square)](#constraints)
[![Allocation](https://img.shields.io/badge/allocation-none-0a4a8f?style=flat-square)](#constraints)
[![Rust](https://img.shields.io/badge/Built%20with-Rust-CE422B?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-Apache--2.0%20OR%20MIT-475569?style=flat-square)](#licensing)

</div>

---

Above this line AxonOS is portable Rust with machine-checked bounds. Below it
there is an ADS1299, an SPI bus, an interrupt line and a great deal of physics.

This crate is the seam — and it exists because the seam was the weakest part of
the stack. Everything above it had proofs; the boundary to the converter was an
acquisition bridge the ecosystem table itself marked **partial**. A system whose
kernel is formally bounded and whose first millimetre is unverified is bounded
in the wrong place.

## The rule the crate is built around

> **No sample is ever silently invented, and no loss is ever silently absorbed.**

A driver that returns a zeroed frame on a bus timeout, or repeats the last
sample when the converter is not ready, produces a signal that looks perfectly
healthy to every stage above it. Filters smooth it. Feature extraction
summarises it. A classifier decides on it. A consent gate authorises an action
from it. Nothing downstream can recover information the acquisition layer threw
away — so this layer never throws any away. It reports, it counts, and it lets
the caller decide.

## Three things, and nothing else

### 1. What a sample is

`SampleFrame` — 48 bytes, `Copy`, eight channels of sign-extended 24-bit codes,
a monotonic sequence number, a monotonic timestamp, and per-channel electrode
contact carried **in band** with the sample it describes.

The sequence number is the only mechanism that can prove nothing was lost. A
timestamp cannot: a dropped frame and a late frame look identical on the clock.

```rust
let gap = frame.gap_since(&previous);   // Some(0) = contiguous, Some(n) = n lost
                                        // None = the device rewound; it is faulty
```

Sign extension from 24 bits is a named, tested function rather than an inline
cast, because reading three bytes as an unsigned integer turns every negative
sample — half of a zero-centred biosignal — into a value near 16.7 million. The
waveform still *looks* like something, which is why the defect survives casual
testing.

### 2. What time the chain is allowed to take

AxonOS publishes a worst-case response time of 972 µs and jitter of 2.1 µs σ,
6.5 µs at P99.9. Numbers like those are usually decoration: they sit in
documentation, nothing checks them, and the day someone adds a pipeline stage
they quietly stop being true.

Here they are inputs to a function that refuses:

```rust
TimingBudget::canonical(250)   // Ok — 24.5 % of a 4 ms period
TimingBudget::canonical(500)   // Ok — 48.9 %
TimingBudget::canonical(1_000) // Err(InsufficientMargin { 978_500 ppm, limit 800_000 })
TimingBudget::canonical(2_000) // Err(DeadlineMissed { needed 978_500 ns, available 500_000 })
```

`AcquisitionDevice::configure` takes a `TimingBudget` by value, and the only way
to obtain one is to have closed it. **A device cannot be started into a
configuration whose deadline is unmeetable** — not by policy, by construction.

The budget is `const`, so the deadline can be proved at compile time rather than
discovered at boot. The published 972 µs figure and the per-stage table are the
same data, and a test asserts they agree: the headline cannot survive a stage
that no longer adds up.

### 3. What happens when the hardware misbehaves

Every degradation is a distinct, named, counted event — `Overrun { lost }`,
`Desync { got, expected }`, `Integrity`, `BusTimeout`, `DeviceFault { status }`,
`NotConfigured`, `NotReady`. Each answers two separate questions:

| | `is_recoverable()` | `breaks_continuity()` |
|:--|:--|:--|
| `BusTimeout` | yes | **no** — no sample was destroyed |
| `Overrun { lost }` | yes | **yes** — a filter's state assumes an unbroken stream |
| `DeviceFault` | **no** | — the converter lost its reference; re-initialise |

An overrun carries its cost as a mandatory field. An overrun without a count is
indistinguishable from a healthy stream in every log and every metric, and it is
the failure most likely to be discovered by a clinician rather than an engineer.

`Diagnostics` counts frames, losses, overruns, desyncs, integrity failures,
lifted electrodes and saturated channels, all with saturating arithmetic — a
counter that wraps to zero during a long session reports a healthy device at
exactly the wrong moment.

## Testable without hardware

`sim::SimDevice` implements the same trait deterministically from a seed,
including the faults. Same seed, same call sequence, byte-identical frames on
any machine, forever. A failing test becomes a permanent artefact instead of a
story about a bad afternoon — and it is what lets the conformance suite state
that two independent implementations agree, which you cannot do against a
physical electrode.

```rust
let budget = TimingBudget::canonical(250)?;
let mut dev = SimDevice::new(seed, FaultProfile::FIELD);
dev.configure(budget, Frontend::CANONICAL)?;
```

`FaultProfile::FIELD` is a plausible bad session: an electrode lifting partway
through, periodic overruns, the occasional corrupt frame. Faults are scheduled
by period rather than probability, because a test that fails one run in fifty
teaches a team to re-run CI instead of reading it.

## Verification

52 tests, all green, no hardware:

- **Frame arithmetic** — sign extension across the full 24-bit range, the
  unsigned-cast defect explicitly demonstrated as prevented, MSB-first byte
  assembly, ±187.5 mV full scale at canonical gain, one code ≈ 22.35 nV,
  overflow-free at every ADS1299 gain step, symmetric truncation about zero
  (an asymmetric rule injects DC that no downstream filter can distinguish
  from electrode drift), gap detection across counter wraparound, sequence
  rewind reported as unknown rather than as a two-billion-frame loss.
- **Timing** — the published WCRT equals the sum of its stages; 250 and 500 SPS
  close; 1 kSPS is refused for margin; 2 kSPS cannot fit at all; an added
  uncosted stage breaks the build; the budget is `const`-evaluable.
- **Degradation** — recoverability and continuity classified independently;
  overruns count events and frames separately; counters saturate; `NotReady`
  is not recorded as a fault.
- **Simulation** — determinism, reproducibility across `reset()`, samples
  inside full scale, faults on schedule, and the integration property below.

The integration test states the guarantee directly: it reconstructs frame loss
from **sequence numbers alone**, without consulting the diagnostics, and asserts
the two agree. If they ever disagree, one of them is lying and the stream cannot
be trusted either way.

## Constraints

`#![no_std]` · `#![forbid(unsafe_code)]` · `#![deny(missing_docs)]` · no
allocation · no floating point anywhere in the sample path · `panic = "abort"`
in release.

Integer-only arithmetic is not an aesthetic preference. A worst-case execution
time you can state requires arithmetic whose cost does not depend on its
operands, and two implementations must agree on a sample bit for bit or the
conformance suite is measuring nothing.

## Where it sits

```
electrodes → [ axonos-hal ] → axonos-signal-pipeline → axonos-kernel
                                                    → axonos-consent → axonos-protocol
```

Everything above the HAL is portable. Everything below it is silicon. This is
where AxonOS stops being software and becomes a device.

## Licensing

Apache-2.0 OR MIT, matching the AxonOS core.

---

<div align="center">

**© The AxonOS Project / Denis Yermakou**

[axonos.org](https://axonos.org) · [medium.com/@AxonOS](https://medium.com/@AxonOS) · connect@axonos.org · security@axonos.org

</div>
