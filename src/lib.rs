//! # axonos-hal
//!
//! **The contract between AxonOS and silicon.**
//!
//! Above this line the system is portable Rust with machine-checked bounds.
//! Below it there is an ADS1299, an SPI bus, an interrupt line and a great
//! deal of physics. This crate is the seam, and it exists because the seam was
//! the weakest part of the stack: everything above it had proofs, and the
//! boundary to the converter was a forked acquisition bridge marked *partial*.
//!
//! Three things are defined here, and nothing else:
//!
//! 1. **What a sample is** — [`SampleFrame`], fixed-size, integer-only,
//!    carrying its own sequence number so loss is detectable rather than
//!    inferable.
//! 2. **What time the chain is allowed to take** — [`TimingBudget`], which
//!    refuses to construct for a configuration whose deadline the measured
//!    worst case cannot meet.
//! 3. **What happens when the hardware misbehaves** — [`AcqError`], where
//!    every degradation is a distinct, counted, named event.
//!
//! ## The rule this crate is built around
//!
//! **No sample is ever silently invented, and no loss is ever silently
//! absorbed.**
//!
//! A driver that returns a zeroed frame on a bus timeout, or that quietly
//! repeats the last sample when the converter is not ready, produces a signal
//! that looks perfectly healthy to every stage above it. Filters smooth it,
//! feature extraction summarises it, a classifier decides on it, and a consent
//! gate authorises an action from it. Nothing downstream can recover
//! information the acquisition layer threw away, so this layer never throws
//! any away: it reports, it counts, and it lets the caller decide.
//!
//! ## No allocation, no panics, no floating point
//!
//! `#![no_std]`, `#![forbid(unsafe_code)]`, every buffer fixed at compile
//! time, and no floating point anywhere in the sample path — a worst-case
//! execution time you can state requires arithmetic whose cost does not depend
//! on its operands, and two implementations must agree on a sample bit for bit
//! or the conformance suite is measuring nothing.
//!
//! ## Testable without hardware
//!
//! `sim::SimDevice` (behind the default `sim` feature) implements the same
//! [`AcquisitionDevice`] trait deterministically from a seed, including the
//! faults — overruns, desync, lifted electrodes, saturation.
//!
//! The reference is deliberately unlinked rather than conditionally linked:
//! a firmware build turns the feature off, and documentation whose links only
//! resolve in one configuration is documentation that breaks in the other. CI, the conformance vectors and every
//! contributor without a board exercise the identical code path that firmware
//! does. A driver that only works when a board is attached is a driver nobody
//! can review.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod frame;
pub mod operating;
pub mod timing;

#[cfg(feature = "sim")]
pub mod sim;

pub use frame::{
    code_from_be_bytes, code_to_nanovolts, sign_extend_24, Frontend, LeadOff, SampleFrame,
    CHANNELS, CODE_MAX, CODE_MIN,
};
pub use timing::{
    BudgetError, Stage, TimingBudget, CANONICAL_BLOCKING_AND_INTERFERENCE_NS,
    CANONICAL_EXECUTION_NS, CANONICAL_TASKS, CANONICAL_WCRT_MEASURED_NS,
};

/// Something the hardware did that the caller must know about.
///
/// Every variant is a fact with a consequence, not a generic failure. A caller
/// that matches on these can respond correctly; a caller handed `Option::None`
/// cannot tell a quiet bus from a lifted electrode from a dead converter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AcqError {
    /// No new sample yet. Not an error — the expected answer when polling
    /// faster than the converter produces, and the caller should simply wait.
    NotReady,
    /// Samples were produced faster than they were collected, and `lost`
    /// frames are gone.
    ///
    /// The count is mandatory. An overrun without a count is
    /// indistinguishable from a healthy stream in every log and every metric,
    /// and it is the failure most likely to be discovered by a clinician
    /// rather than an engineer.
    Overrun {
        /// Frames irrecoverably lost.
        lost: u32,
    },
    /// The frame's sequence did not follow the previous one, and the reason is
    /// not a countable overrun — the device restarted, or the bus resynced
    /// mid-frame.
    Desync {
        /// Sequence the device sent.
        got: u32,
        /// Sequence the host expected.
        expected: u32,
    },
    /// The frame failed its integrity check. The bytes arrived; they are not
    /// trustworthy.
    Integrity,
    /// The bus transaction did not complete in its allotted time.
    BusTimeout,
    /// The converter reports an internal fault (reference loss, supply out of
    /// range, oscillator failure).
    DeviceFault {
        /// Device-specific status word, passed through unmodified so that a
        /// field report can be decoded against the datasheet.
        status: u32,
    },
    /// The device is not configured, or was configured with a request it
    /// cannot honour.
    NotConfigured,
}

impl AcqError {
    /// Whether the stream can continue after this, or the device must be
    /// re-initialised.
    ///
    /// The distinction exists so a supervisor does not tear down a session for
    /// a transient the next sample period will clear, and does not keep
    /// feeding a pipeline from a converter that has lost its reference.
    pub const fn is_recoverable(&self) -> bool {
        matches!(
            self,
            AcqError::NotReady
                | AcqError::Overrun { .. }
                | AcqError::Integrity
                | AcqError::BusTimeout
        )
    }

    /// Whether a sample was destroyed by this event. Used by the pipeline to
    /// decide whether continuity assumptions still hold — a filter's state is
    /// only meaningful over an unbroken stream.
    pub const fn breaks_continuity(&self) -> bool {
        matches!(
            self,
            AcqError::Overrun { .. } | AcqError::Desync { .. } | AcqError::Integrity
        )
    }
}

/// Running count of everything that went wrong, and of everything that went
/// right, since the stream started.
///
/// Cheap enough to update on every frame at 250 SPS and mandatory for the same
/// reason the errors are explicit: a device that has dropped four frames in an
/// hour and a device that has dropped four thousand look identical without it.
/// Saturating arithmetic throughout — a counter that wraps to zero during a
/// long session reports a healthy device at exactly the wrong moment.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Diagnostics {
    /// Frames delivered to the caller.
    pub frames: u64,
    /// Frames known to have been lost, summed across all overruns.
    pub frames_lost: u64,
    /// Overrun events (distinct from the number of frames they cost).
    pub overruns: u32,
    /// Sequence discontinuities not explained by an overrun.
    pub desyncs: u32,
    /// Frames rejected by their integrity check.
    pub integrity_failures: u32,
    /// Bus transactions that did not complete in time.
    pub bus_timeouts: u32,
    /// Device-reported internal faults.
    pub device_faults: u32,
    /// Frames in which at least one channel was off-contact.
    pub lead_off_frames: u64,
    /// Frames in which at least one channel was railed.
    pub saturated_frames: u64,
}

impl Diagnostics {
    /// Fold one error into the counters.
    pub fn record_error(&mut self, e: &AcqError) {
        match e {
            AcqError::Overrun { lost } => {
                self.overruns = self.overruns.saturating_add(1);
                self.frames_lost = self.frames_lost.saturating_add(*lost as u64);
            }
            AcqError::Desync { .. } => self.desyncs = self.desyncs.saturating_add(1),
            AcqError::Integrity => {
                self.integrity_failures = self.integrity_failures.saturating_add(1)
            }
            AcqError::BusTimeout => self.bus_timeouts = self.bus_timeouts.saturating_add(1),
            AcqError::DeviceFault { .. } => {
                self.device_faults = self.device_faults.saturating_add(1)
            }
            AcqError::NotReady | AcqError::NotConfigured => {}
        }
    }

    /// Fold one delivered frame into the counters.
    pub fn record_frame(&mut self, f: &SampleFrame) {
        self.frames = self.frames.saturating_add(1);
        if f.lead_off.any() {
            self.lead_off_frames = self.lead_off_frames.saturating_add(1);
        }
        if f.saturated() {
            self.saturated_frames = self.saturated_frames.saturating_add(1);
        }
    }

    /// Delivered frames as a fraction of frames the device produced, in parts
    /// per million. `1_000_000` means nothing was lost.
    ///
    /// Integer arithmetic, and the denominator is delivered + lost rather than
    /// an elapsed-time estimate: an estimate would make the figure depend on
    /// clock accuracy, which is one of the things it exists to detect.
    pub const fn integrity_ppm(&self) -> u32 {
        let produced = self.frames + self.frames_lost;
        if produced == 0 {
            return 1_000_000;
        }
        ((self.frames * 1_000_000) / produced) as u32
    }
}

/// A source of samples.
///
/// Deliberately small. A HAL that grows convenience methods becomes a place
/// where behaviour hides, and behaviour that hides in a hardware layer is
/// behaviour nobody re-verifies when the hardware changes. Filtering,
/// buffering and policy belong above this line.
pub trait AcquisitionDevice {
    /// Bring the device into a known state at the rate and gain a closed
    /// budget was proved for.
    ///
    /// Taking [`TimingBudget`] by value rather than a bare sample rate is the
    /// enforcement: the only way to obtain one is to have closed it, so a
    /// device cannot be started into a configuration whose deadline is
    /// unmeetable.
    fn configure(&mut self, budget: TimingBudget, frontend: Frontend) -> Result<(), AcqError>;

    /// Take the next sample, or say precisely what happened instead.
    fn read_frame(&mut self) -> Result<SampleFrame, AcqError>;

    /// Counters since the stream started.
    fn diagnostics(&self) -> Diagnostics;

    /// Stop sampling and release the bus. Idempotent.
    fn stop(&mut self) -> Result<(), AcqError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recoverability_is_classified_deliberately() {
        assert!(AcqError::NotReady.is_recoverable());
        assert!(AcqError::Overrun { lost: 3 }.is_recoverable());
        assert!(AcqError::BusTimeout.is_recoverable());
        assert!(!AcqError::DeviceFault { status: 0x21 }.is_recoverable());
        assert!(!AcqError::Desync {
            got: 5,
            expected: 4
        }
        .is_recoverable());
        assert!(!AcqError::NotConfigured.is_recoverable());
    }

    #[test]
    fn continuity_and_recoverability_are_different_questions() {
        // an overrun is survivable but breaks the stream a filter assumes
        let e = AcqError::Overrun { lost: 1 };
        assert!(e.is_recoverable() && e.breaks_continuity());
        // a timeout costs no sample
        assert!(AcqError::BusTimeout.is_recoverable());
        assert!(!AcqError::BusTimeout.breaks_continuity());
        // NotReady is not a fault at all
        assert!(!AcqError::NotReady.breaks_continuity());
    }

    #[test]
    fn an_overrun_must_carry_its_cost() {
        let mut d = Diagnostics::default();
        d.record_error(&AcqError::Overrun { lost: 7 });
        d.record_error(&AcqError::Overrun { lost: 2 });
        assert_eq!(d.overruns, 2);
        assert_eq!(d.frames_lost, 9, "events and frames are counted separately");
    }

    #[test]
    fn integrity_is_reported_against_frames_produced() {
        let mut d = Diagnostics::default();
        for _ in 0..999 {
            d.record_frame(&SampleFrame::zeroed(0, 0));
        }
        d.record_error(&AcqError::Overrun { lost: 1 });
        assert_eq!(d.integrity_ppm(), 999_000);
    }

    #[test]
    fn a_stream_with_nothing_yet_is_not_reported_as_broken() {
        assert_eq!(Diagnostics::default().integrity_ppm(), 1_000_000);
    }

    #[test]
    fn lead_off_and_saturation_are_counted_apart() {
        let mut d = Diagnostics::default();
        let mut f = SampleFrame::zeroed(1, 0);
        f.lead_off = LeadOff(0b1);
        d.record_frame(&f);
        let mut g = SampleFrame::zeroed(2, 0);
        g.codes[0] = CODE_MIN;
        d.record_frame(&g);
        assert_eq!((d.lead_off_frames, d.saturated_frames, d.frames), (1, 1, 2));
    }

    #[test]
    fn counters_saturate_rather_than_wrapping_to_health() {
        let mut d = Diagnostics {
            overruns: u32::MAX,
            ..Default::default()
        };
        d.record_error(&AcqError::Overrun { lost: 1 });
        assert_eq!(d.overruns, u32::MAX, "must not wrap to zero mid-session");
    }

    #[test]
    fn not_ready_is_not_recorded_as_a_fault() {
        let mut d = Diagnostics::default();
        d.record_error(&AcqError::NotReady);
        assert_eq!(d, Diagnostics::default());
    }
}
