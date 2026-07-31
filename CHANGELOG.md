# Changelog

## [0.3.0] — 2026-08-01

### Added
- **Operating points, closing RFC-0008 N4** — the last requirement that
  specification listed as unmet, and the only one on the profile's "still
  needed" list that is arithmetic rather than hardware.

  `TimingBudget` proves a configuration admissible for a given execution cost.
  That cost is not a property of the code: it is a property of the code running
  at a particular core frequency with particular flash wait states. Change
  either and every stage time changes with them, so the proof does not travel.

  This is why power management is a correctness problem in this system rather
  than a comfort one. Dropping the core clock to save battery is the ordinary
  thing to do on a wearable; doing it under a chain that proved its deadline at
  full speed silently invalidates the proof, and the failure appears as a
  missed sample rather than as an error.

  A transition is therefore a **re-admission**: `AdmittedPoint::transition_to`
  closes the budget at the destination before the device may arrive there, and
  a destination that does not close is refused **with the receiver unchanged**
  — a half-applied transition would leave the device at a frequency whose
  deadline nothing has proved.

- `OperatingPoint::measured` for points whose cost has been measured, and
  `::modelled` for those scaled from the reference. A measured figure always
  wins, and `AdmittedPoint::is_measured` makes the difference legible, because
  a caller on a modelled point is running on an argument rather than on
  evidence.

### Notes on the model, stated because it is optimistic in one direction
- Execution scales inversely with the core clock, which is conservative for
  memory-bound work — halving the clock leaves such work nearly unchanged while
  the model predicts a doubling — and **not** conservative for work paced by a
  peripheral with its own clock. That asymmetry is why measured points exist.
- Blocking and interference are passed through **unscaled**. Their relationship
  to the core clock is not this model's to guess, and inventing one would be
  optimistic in the unsafe direction.
- Wait states are charged as a **difference** from the reference. The published
  694.2 µs is a measurement taken at five wait states, so those stalls are
  already inside it; the first draft added them again and disagreed with its own
  anchor by twelve per cent. A model that cannot reproduce the point it was
  calibrated at is not calibrated to anything, and a test now asserts it does.

All notable changes to axonos-hal are recorded here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.1] — 2026-08-01

### Added
- Licence texts (`LICENSE-APACHE`, `LICENSE-MIT`), the `NOTICE` that
  Apache-2.0 section 4(d) obliges a redistributor to retain, `CITATION.cff`
  with the author listed first, and this changelog.

  The crate declared `Apache-2.0 OR MIT` in its manifest and its README from
  the first release and shipped neither text. A dual-licence declaration
  without the licences does not grant what it announces: a reader who wants to
  depend on this had nothing to read, and Apache-2.0's attribution clause
  cannot be honoured against a `NOTICE` that does not exist. That is a defect
  in what the repository *is*, not in what it does, which is why it is recorded
  here rather than quietly added.

  No code changed. The version moves because the artefact a consumer receives
  is materially different: it can now be depended on under the terms it always
  claimed.

---

<sub>**axonos-hal v0.2.1** · © 2026 Denis Yermakou · Apache-2.0 OR MIT ·
authored for [The AxonOS Project](https://axonos.org) · connect@axonos.org</sub>
