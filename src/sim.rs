//! A converter that does not exist, behaving exactly like one that does.
//!
//! Every implementation of [`AcquisitionDevice`](crate::AcquisitionDevice)
//! that needs a board is an implementation nobody can review, nobody can run
//! in CI, and nobody can reproduce a bug in. This one needs a seed.
//!
//! It is deterministic in the strong sense: the same seed and the same call
//! sequence produce byte-identical frames on any machine, forever. That makes
//! a failing test a permanent artefact rather than a story about a bad
//! afternoon, and it is what lets the conformance suite state that two
//! independent implementations agree — you cannot compare two stacks against a
//! physical electrode.
//!
//! It also simulates the failures, on schedule, because the degradation paths
//! are the ones that never get exercised on a bench and always get exercised
//! in the field.

use crate::{
    frame::{Frontend, LeadOff, SampleFrame, CHANNELS, CODE_MAX, CODE_MIN},
    timing::TimingBudget,
    AcqError, AcquisitionDevice, Diagnostics,
};

/// Faults the simulator will inject, and how often.
///
/// Periods rather than probabilities: a test that fails one run in fifty is
/// worse than no test, because it teaches the team to re-run CI instead of
/// reading it.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct FaultProfile {
    /// Emit an overrun every N frames, losing `overrun_lost` frames. 0 = never.
    pub overrun_every: u32,
    /// Frames destroyed by each overrun.
    pub overrun_lost: u32,
    /// Report an integrity failure every N frames. 0 = never.
    pub integrity_every: u32,
    /// Lift electrodes matching this mask from `lead_off_after` onward.
    pub lead_off_mask: u8,
    /// Frame index at which the electrodes lift.
    pub lead_off_after: u32,
    /// Rail channel 0 every N frames. 0 = never.
    pub saturate_every: u32,
}

impl FaultProfile {
    /// A device behaving perfectly. The baseline every other profile is read
    /// against.
    pub const CLEAN: Self = Self {
        overrun_every: 0,
        overrun_lost: 0,
        integrity_every: 0,
        lead_off_mask: 0,
        lead_off_after: 0,
        saturate_every: 0,
    };

    /// A plausible bad session: a lifted electrode partway through, an
    /// occasional overrun, the odd corrupt frame.
    pub const FIELD: Self = Self {
        overrun_every: 500,
        overrun_lost: 2,
        integrity_every: 997,
        lead_off_mask: 0b0000_0100,
        lead_off_after: 1_200,
        saturate_every: 0,
    };
}

/// Deterministic sample source.
pub struct SimDevice {
    seed: u64,
    state: u64,
    seq: u32,
    t_us: u64,
    period_us: u64,
    frontend: Frontend,
    faults: FaultProfile,
    diag: Diagnostics,
    configured: bool,
    produced: u32,
}

impl SimDevice {
    /// Create a simulator. Nothing is produced until [`AcquisitionDevice::configure`]
    /// succeeds — the same as real silicon, and the same refusal path.
    pub const fn new(seed: u64, faults: FaultProfile) -> Self {
        Self {
            seed,
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
            seq: 0,
            t_us: 0,
            period_us: 0,
            frontend: Frontend::CANONICAL,
            faults,
            diag: Diagnostics {
                frames: 0,
                frames_lost: 0,
                overruns: 0,
                desyncs: 0,
                integrity_failures: 0,
                bus_timeouts: 0,
                device_faults: 0,
                lead_off_frames: 0,
                saturated_frames: 0,
            },
            configured: false,
            produced: 0,
        }
    }

    /// Return to the initial state. The same seed replays the same stream,
    /// which is what makes a captured failure re-runnable.
    pub fn reset(&mut self) {
        let (seed, faults) = (self.seed, self.faults);
        *self = Self::new(seed, faults);
    }

    /// xorshift64*, chosen because it is small, has no dependencies, and its
    /// output is fixed by the specification rather than by a library version.
    /// This is not a source of randomness for anything that matters; it is a
    /// source of *repeatable* numbers.
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A plausible EEG-scale code: a slow wander plus noise, in the tens of
    /// microvolts, well inside full scale.
    fn next_code(&mut self, ch: usize) -> i32 {
        let r = self.next_u64();
        // ±~40 µV at canonical gain is roughly ±1800 codes
        let noise = ((r >> 32) as u32 % 3_600) as i32 - 1_800;
        // a per-channel slow component so channels are not identical
        let wander = (((self.produced as i32) / 64) % 400) - 200 + (ch as i32 * 17);
        noise + wander
    }
}

impl AcquisitionDevice for SimDevice {
    fn configure(&mut self, budget: TimingBudget, frontend: Frontend) -> Result<(), AcqError> {
        if !frontend.is_valid() {
            return Err(AcqError::NotConfigured);
        }
        self.period_us = budget.period_ns() / 1_000;
        self.frontend = frontend;
        self.configured = true;
        Ok(())
    }

    fn read_frame(&mut self) -> Result<SampleFrame, AcqError> {
        if !self.configured {
            return Err(AcqError::NotConfigured);
        }
        let n = self.produced;
        self.produced = self.produced.wrapping_add(1);

        // Faults first, and the sequence still advances past the frames an
        // overrun destroyed — that is precisely what makes the loss visible
        // to gap_since() downstream instead of invisible.
        if self.faults.overrun_every != 0 && n != 0 && n % self.faults.overrun_every == 0 {
            let lost = self.faults.overrun_lost;
            self.seq = self.seq.wrapping_add(lost);
            self.t_us += self.period_us * lost as u64;
            let e = AcqError::Overrun { lost };
            self.diag.record_error(&e);
            return Err(e);
        }
        if self.faults.integrity_every != 0 && n != 0 && n % self.faults.integrity_every == 0 {
            let e = AcqError::Integrity;
            self.diag.record_error(&e);
            return Err(e);
        }

        let mut codes = [0i32; CHANNELS];
        for (ch, c) in codes.iter_mut().enumerate() {
            *c = self.next_code(ch);
        }
        if self.faults.saturate_every != 0 && n != 0 && n % self.faults.saturate_every == 0 {
            codes[0] = if n % (self.faults.saturate_every * 2) == 0 {
                CODE_MAX
            } else {
                CODE_MIN
            };
        }
        let lead_off = if self.faults.lead_off_mask != 0 && n >= self.faults.lead_off_after {
            LeadOff(self.faults.lead_off_mask)
        } else {
            LeadOff(0)
        };

        let f = SampleFrame {
            seq: self.seq,
            t_us: self.t_us,
            codes,
            lead_off,
        };
        self.seq = self.seq.wrapping_add(1);
        self.t_us += self.period_us;
        self.diag.record_frame(&f);
        Ok(f)
    }

    fn diagnostics(&self) -> Diagnostics {
        self.diag
    }

    fn stop(&mut self) -> Result<(), AcqError> {
        self.configured = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timing::TimingBudget;

    fn started(faults: FaultProfile) -> SimDevice {
        let mut d = SimDevice::new(42, faults);
        d.configure(TimingBudget::canonical(250).unwrap(), Frontend::CANONICAL)
            .unwrap();
        d
    }

    #[test]
    fn nothing_is_produced_before_configuration() {
        let mut d = SimDevice::new(1, FaultProfile::CLEAN);
        assert_eq!(d.read_frame(), Err(AcqError::NotConfigured));
    }

    #[test]
    fn an_invalid_frontend_is_refused() {
        let mut d = SimDevice::new(1, FaultProfile::CLEAN);
        let b = TimingBudget::canonical(250).unwrap();
        assert_eq!(
            d.configure(
                b,
                Frontend {
                    vref_uv: 4_500_000,
                    gain: 3
                }
            ),
            Err(AcqError::NotConfigured)
        );
    }

    #[test]
    fn the_same_seed_replays_the_same_stream() {
        let mut a = started(FaultProfile::CLEAN);
        let mut b = started(FaultProfile::CLEAN);
        for _ in 0..500 {
            assert_eq!(a.read_frame(), b.read_frame());
        }
    }

    #[test]
    fn a_different_seed_gives_a_different_stream() {
        let mut a = started(FaultProfile::CLEAN);
        let mut b = SimDevice::new(43, FaultProfile::CLEAN);
        b.configure(TimingBudget::canonical(250).unwrap(), Frontend::CANONICAL)
            .unwrap();
        let x = a.read_frame().unwrap();
        let y = b.read_frame().unwrap();
        assert_ne!(x.codes, y.codes);
    }

    #[test]
    fn reset_makes_a_failure_reproducible() {
        let mut d = started(FaultProfile::CLEAN);
        let first: [SampleFrame; 8] = core::array::from_fn(|_| d.read_frame().unwrap());
        d.reset();
        d.configure(TimingBudget::canonical(250).unwrap(), Frontend::CANONICAL)
            .unwrap();
        let again: [SampleFrame; 8] = core::array::from_fn(|_| d.read_frame().unwrap());
        assert_eq!(first, again);
    }

    #[test]
    fn timestamps_advance_by_exactly_one_period() {
        let mut d = started(FaultProfile::CLEAN);
        let a = d.read_frame().unwrap();
        let b = d.read_frame().unwrap();
        assert_eq!(b.t_us - a.t_us, 4_000, "250 SPS is a 4 ms period");
        assert_eq!(b.seq, a.seq + 1);
    }

    #[test]
    fn samples_sit_inside_full_scale() {
        let mut d = started(FaultProfile::CLEAN);
        for _ in 0..2_000 {
            let f = d.read_frame().unwrap();
            for &c in &f.codes {
                assert!(c > CODE_MIN && c < CODE_MAX);
            }
            assert!(!f.saturated());
        }
    }

    #[test]
    fn channels_are_not_copies_of_each_other() {
        let mut d = started(FaultProfile::CLEAN);
        let f = d.read_frame().unwrap();
        assert!(f.codes.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn an_overrun_is_visible_to_the_gap_detector() {
        let mut d = started(FaultProfile {
            overrun_every: 10,
            overrun_lost: 3,
            ..FaultProfile::CLEAN
        });
        let mut prev = d.read_frame().unwrap();
        let mut saw_overrun = false;
        let mut recovered_gap = None;
        for _ in 0..24 {
            match d.read_frame() {
                Ok(f) => {
                    if saw_overrun && recovered_gap.is_none() {
                        recovered_gap = f.gap_since(&prev);
                    }
                    prev = f;
                }
                Err(AcqError::Overrun { lost }) => {
                    assert_eq!(lost, 3);
                    saw_overrun = true;
                }
                Err(e) => panic!("unexpected {e:?}"),
            }
        }
        assert!(saw_overrun);
        assert_eq!(
            recovered_gap,
            Some(3),
            "the sequence must expose the loss the overrun caused"
        );
    }

    #[test]
    fn diagnostics_agree_with_what_was_delivered() {
        let mut d = started(FaultProfile {
            overrun_every: 100,
            overrun_lost: 2,
            ..FaultProfile::CLEAN
        });
        let mut ok = 0u64;
        for _ in 0..1_000 {
            if d.read_frame().is_ok() {
                ok += 1;
            }
        }
        let diag = d.diagnostics();
        assert_eq!(diag.frames, ok);
        assert_eq!(diag.frames_lost, diag.overruns as u64 * 2);
        assert!(diag.integrity_ppm() < 1_000_000);
    }

    #[test]
    fn a_lifted_electrode_appears_on_schedule_and_stays() {
        let mut d = started(FaultProfile {
            lead_off_mask: 0b100,
            lead_off_after: 5,
            ..FaultProfile::CLEAN
        });
        for i in 0..12 {
            let f = d.read_frame().unwrap();
            assert_eq!(f.lead_off.channel(2), i >= 5, "frame {i}");
        }
        assert!(d.diagnostics().lead_off_frames >= 7);
    }

    #[test]
    fn saturation_is_injected_and_counted() {
        let mut d = started(FaultProfile {
            saturate_every: 5,
            ..FaultProfile::CLEAN
        });
        for _ in 0..20 {
            let _ = d.read_frame();
        }
        assert!(d.diagnostics().saturated_frames >= 3);
    }

    #[test]
    fn the_field_profile_produces_a_recoverable_but_unhealthy_stream() {
        let mut d = started(FaultProfile::FIELD);
        let mut delivered = 0;
        for _ in 0..3_000 {
            match d.read_frame() {
                Ok(_) => delivered += 1,
                Err(e) => assert!(e.is_recoverable(), "{e:?} should not end a session"),
            }
        }
        let diag = d.diagnostics();
        assert!(delivered > 2_900);
        assert!(diag.overruns > 0 && diag.integrity_failures > 0);
        assert!(diag.lead_off_frames > 0);
        assert!(diag.integrity_ppm() > 990_000 && diag.integrity_ppm() < 1_000_000);
    }

    #[test]
    fn stop_is_idempotent_and_halts_the_stream() {
        let mut d = started(FaultProfile::CLEAN);
        d.read_frame().unwrap();
        d.stop().unwrap();
        d.stop().unwrap();
        assert_eq!(d.read_frame(), Err(AcqError::NotConfigured));
    }
}
