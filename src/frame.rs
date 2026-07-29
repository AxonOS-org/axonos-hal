//! One instant of the body, as it arrives from the converter.
//!
//! Everything in this module is fixed-size and integer-only. There is no
//! floating point anywhere in the acquisition path — not because floats are
//! slow on an M4F, but because a WCET you can state requires arithmetic whose
//! cost does not depend on its operands, and because two devices must agree
//! on a sample bit-for-bit or the conformance suite is measuring nothing.

/// Channels in a frame. Fixed by the canonical acquisition front end
/// (ADS1299, eight differential channels sampled simultaneously).
///
/// A constant rather than a generic parameter: the number is part of the
/// device contract, and making it configurable would let a build silently
/// disagree with the wire format every other implementation speaks.
pub const CHANNELS: usize = 8;

/// Raw code range of a 24-bit two's-complement converter.
pub const CODE_MIN: i32 = -(1 << 23);
/// Largest positive code a 24-bit two's-complement converter can produce.
pub const CODE_MAX: i32 = (1 << 23) - 1;

/// Analogue front-end configuration needed to turn codes into volts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Frontend {
    /// Reference voltage in microvolts (ADS1299 internal reference: 4 500 000).
    pub vref_uv: u32,
    /// Programmable gain (ADS1299: 1, 2, 4, 6, 8, 12, 24).
    pub gain: u8,
}

impl Frontend {
    /// The canonical AxonOS front end: internal 4.5 V reference, gain 24.
    ///
    /// At this setting one code is ~22.35 nV and full scale is ±187.5 mV,
    /// which is the range EEG actually lives in — cortical signals are tens of
    /// microvolts, and the headroom above them is for electrode offset.
    pub const CANONICAL: Self = Self {
        vref_uv: 4_500_000,
        gain: 24,
    };

    /// Reject a configuration the converter cannot be put into, rather than
    /// silently producing scaled nonsense from it.
    pub const fn is_valid(&self) -> bool {
        self.vref_uv > 0 && matches!(self.gain, 1 | 2 | 4 | 6 | 8 | 12 | 24)
    }
}

/// Sign-extend a 24-bit two's-complement value into `i32`.
///
/// The converter ships three bytes; the sign bit is bit 23, not bit 31. Read
/// them as a plain unsigned integer and every negative sample — that is, half
/// of a zero-centred biosignal — becomes a value near 16.7 million. It is the
/// single most common defect in ADC glue code, it survives casual testing
/// because the waveform still *looks* like something, and it is why this is a
/// named, tested function instead of an inline cast.
#[inline]
pub const fn sign_extend_24(raw: u32) -> i32 {
    let v = (raw & 0x00FF_FFFF) as i32;
    if v & 0x0080_0000 != 0 {
        v | !0x00FF_FFFF_u32 as i32
    } else {
        v
    }
}

/// Assemble a 24-bit code from three bytes in the converter's MSB-first order.
#[inline]
pub const fn code_from_be_bytes(b: [u8; 3]) -> i32 {
    sign_extend_24(((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32)
}

/// Convert one code to nanovolts, in integer arithmetic.
///
/// `nV = code × 2 × Vref / (gain × 2^24)`, carried in `i64` because the
/// intermediate product reaches ~7.6 × 10^16 at full scale — comfortably
/// inside `i64`, catastrophically outside `i32`. Truncation toward zero is
/// deliberate and symmetric: rounding a biosignal introduces a bias that a
/// later DC blocker cannot distinguish from electrode drift.
#[inline]
pub const fn code_to_nanovolts(code: i32, fe: Frontend) -> i64 {
    // vref_uv × 1000 = vref in nanovolts
    let numerator = (code as i64) * 2 * (fe.vref_uv as i64) * 1_000;
    let denominator = (fe.gain as i64) * (1i64 << 24);
    numerator / denominator
}

/// Per-channel electrode contact state, one bit per channel.
///
/// Carried in-band with the sample it describes rather than polled separately,
/// because a downstream stage needs to know that *this* sample came off a
/// lifted electrode, not that some electrode was lifted at some point.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct LeadOff(pub u8);

impl LeadOff {
    /// Is this channel's electrode reporting loss of contact?
    #[inline]
    pub const fn channel(&self, ch: usize) -> bool {
        ch < CHANNELS && (self.0 >> ch) & 1 == 1
    }
    /// Any channel off-contact.
    #[inline]
    pub const fn any(&self) -> bool {
        self.0 != 0
    }
    /// How many channels are off-contact.
    #[inline]
    pub const fn count(&self) -> u32 {
        self.0.count_ones()
    }
}

/// One simultaneous sample across all channels.
///
/// `Copy` and 48 bytes: it moves through the pipeline by value, so no stage
/// can hold a reference into a ring buffer that another stage is about to
/// overwrite. That class of bug does not exist here because the type makes it
/// unrepresentable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SampleFrame {
    /// Monotonic frame counter from the device, wrapping at `u32::MAX`.
    ///
    /// The sequence is the only mechanism that can prove nothing was lost —
    /// a timestamp cannot, because a dropped frame and a late frame look
    /// identical on the clock.
    pub seq: u32,
    /// Capture instant in microseconds on the kernel's monotonic clock.
    pub t_us: u64,
    /// Sign-extended 24-bit codes, one per channel.
    pub codes: [i32; CHANNELS],
    /// Electrode contact state for this sample.
    pub lead_off: LeadOff,
}

impl SampleFrame {
    /// An all-zero frame at a given instant. Useful for tests and for the
    /// first frame of a stream; never used to paper over a missing sample.
    pub const fn zeroed(seq: u32, t_us: u64) -> Self {
        Self {
            seq,
            t_us,
            codes: [0; CHANNELS],
            lead_off: LeadOff(0),
        }
    }

    /// Channel value in nanovolts.
    #[inline]
    pub const fn nanovolts(&self, ch: usize, fe: Frontend) -> i64 {
        if ch >= CHANNELS {
            return 0;
        }
        code_to_nanovolts(self.codes[ch], fe)
    }

    /// True when any channel sits at a rail — the converter is saturated and
    /// the sample carries no information about the signal, only about the
    /// amplifier.
    ///
    /// Distinct from lead-off: a lifted electrode and a saturated amplifier
    /// need different responses, and a pipeline that conflates them will
    /// happily filter, feature-extract and classify a flat rail.
    #[inline]
    pub fn saturated(&self) -> bool {
        self.codes.iter().any(|&c| c <= CODE_MIN || c >= CODE_MAX)
    }

    /// Frames dropped between `prev` and `self`, accounting for wraparound.
    ///
    /// Returns `None` when `self` is not after `prev` — a device that
    /// rewinds its sequence is faulty, and the caller must be told that
    /// rather than handed a plausible number.
    pub fn gap_since(&self, prev: &SampleFrame) -> Option<u32> {
        let delta = self.seq.wrapping_sub(prev.seq);
        if delta == 0 {
            return None;
        }
        // A "gap" larger than half the counter space is far more likely to be
        // a rewind than a genuine loss of two billion frames.
        if delta > u32::MAX / 2 {
            return None;
        }
        Some(delta - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FE: Frontend = Frontend::CANONICAL;

    #[test]
    fn sign_extension_covers_the_whole_range() {
        assert_eq!(sign_extend_24(0x00_0000), 0);
        assert_eq!(sign_extend_24(0x7F_FFFF), CODE_MAX);
        assert_eq!(sign_extend_24(0x80_0000), CODE_MIN);
        assert_eq!(sign_extend_24(0xFF_FFFF), -1);
        assert_eq!(sign_extend_24(0xFF_FFFE), -2);
    }

    #[test]
    fn the_classic_defect_is_actually_prevented() {
        // read as unsigned this is 16_777_215; as a sample it is -1
        let naive = 0x00FF_FFFF_u32 as i32;
        assert_eq!(naive, 16_777_215);
        assert_eq!(sign_extend_24(0x00FF_FFFF), -1);
    }

    #[test]
    fn bytes_arrive_msb_first() {
        assert_eq!(code_from_be_bytes([0x00, 0x00, 0x01]), 1);
        assert_eq!(code_from_be_bytes([0xFF, 0xFF, 0xFF]), -1);
        assert_eq!(code_from_be_bytes([0x80, 0x00, 0x00]), CODE_MIN);
        assert_eq!(code_from_be_bytes([0x7F, 0xFF, 0xFF]), CODE_MAX);
    }

    #[test]
    fn one_code_is_about_22_nanovolts_at_canonical_gain() {
        // (2 × 4.5 V / 24) / 2^24 = 22.35 nV
        assert_eq!(code_to_nanovolts(1, FE), 22);
        assert_eq!(code_to_nanovolts(1000, FE), 22_351);
        assert_eq!(code_to_nanovolts(-1000, FE), -22_351);
    }

    #[test]
    fn full_scale_is_plus_minus_187_millivolts() {
        let fs = code_to_nanovolts(CODE_MAX, FE);
        // 4.5 / 24 = 187.5 mV = 187_500_000 nV
        assert!((fs - 187_500_000).abs() < 100, "full scale was {fs} nV");
        assert_eq!(code_to_nanovolts(CODE_MIN, FE), -187_500_000);
    }

    #[test]
    fn conversion_does_not_overflow_at_the_extremes() {
        for gain in [1u8, 2, 4, 6, 8, 12, 24] {
            let fe = Frontend {
                vref_uv: 4_500_000,
                gain,
            };
            let hi = code_to_nanovolts(CODE_MAX, fe);
            let lo = code_to_nanovolts(CODE_MIN, fe);
            assert!(hi > 0 && lo < 0, "gain {gain} produced {lo}..{hi}");
        }
    }

    #[test]
    fn truncation_is_symmetric_about_zero() {
        // an asymmetric rounding rule would inject DC a filter cannot remove
        for code in [1, 7, 33, 1001, 65_535] {
            assert_eq!(code_to_nanovolts(code, FE), -code_to_nanovolts(-code, FE));
        }
    }

    #[test]
    fn gain_of_one_still_resolves() {
        let fe = Frontend {
            vref_uv: 4_500_000,
            gain: 1,
        };
        assert_eq!(code_to_nanovolts(1, fe), 536);
    }

    #[test]
    fn invalid_frontends_are_rejected() {
        assert!(Frontend::CANONICAL.is_valid());
        assert!(!Frontend {
            vref_uv: 0,
            gain: 24
        }
        .is_valid());
        assert!(!Frontend {
            vref_uv: 4_500_000,
            gain: 0
        }
        .is_valid());
        assert!(!Frontend {
            vref_uv: 4_500_000,
            gain: 3
        }
        .is_valid()); // not an ADS1299 step
    }

    #[test]
    fn lead_off_reads_per_channel() {
        let l = LeadOff(0b0000_1001);
        assert!(l.channel(0) && l.channel(3));
        assert!(!l.channel(1) && !l.channel(7));
        assert!(!l.channel(99), "out of range must not panic or read true");
        assert_eq!(l.count(), 2);
        assert!(l.any());
        assert!(!LeadOff(0).any());
    }

    #[test]
    fn saturation_is_distinct_from_lead_off() {
        let mut f = SampleFrame::zeroed(1, 0);
        assert!(!f.saturated());
        f.codes[4] = CODE_MAX;
        assert!(f.saturated(), "a railed channel is saturated");
        assert!(!f.lead_off.any(), "and that says nothing about contact");
    }

    #[test]
    fn a_gap_is_counted_exactly() {
        let a = SampleFrame::zeroed(10, 0);
        assert_eq!(SampleFrame::zeroed(11, 1).gap_since(&a), Some(0));
        assert_eq!(SampleFrame::zeroed(14, 1).gap_since(&a), Some(3));
    }

    #[test]
    fn a_gap_survives_counter_wraparound() {
        let a = SampleFrame::zeroed(u32::MAX - 1, 0);
        assert_eq!(SampleFrame::zeroed(u32::MAX, 1).gap_since(&a), Some(0));
        // MAX-1 → 1 crosses the wrap: MAX and 0 were both missed
        assert_eq!(SampleFrame::zeroed(1, 1).gap_since(&a), Some(2));
    }

    #[test]
    fn a_rewind_is_reported_as_unknown_not_as_a_huge_gap() {
        let a = SampleFrame::zeroed(100, 0);
        assert_eq!(SampleFrame::zeroed(99, 1).gap_since(&a), None);
        assert_eq!(SampleFrame::zeroed(100, 1).gap_since(&a), None);
    }

    #[test]
    fn out_of_range_channel_reads_zero_rather_than_panicking() {
        let f = SampleFrame::zeroed(1, 0);
        assert_eq!(f.nanovolts(CHANNELS, FE), 0);
    }

    #[test]
    fn a_frame_is_small_and_copyable() {
        // u32 + u64 + [i32; 8] + u8, aligned to 8
        assert_eq!(core::mem::size_of::<SampleFrame>(), 48);
        let a = SampleFrame::zeroed(1, 2);
        let b = a; // Copy — no borrow into a ring buffer is possible
        assert_eq!(a, b);
    }
}
