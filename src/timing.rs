//! The timing contract, as arithmetic rather than as a claim in a README.
//!
//! AxonOS states a worst-case response time of 972 µs and jitter of 2.1 µs σ
//! with 6.5 µs at P99.9. Numbers like those are usually decoration: they sit
//! in documentation, nothing checks them, and the day someone adds a stage to
//! the pipeline they quietly stop being true.
//!
//! Here they are inputs to a function that refuses. A configuration whose
//! deadline the measured worst case cannot meet does not produce a warning —
//! it fails to construct, and says which term overran. That is the only
//! version of a real-time guarantee worth publishing: one that a build can
//! break.

/// Where time goes between an electrode and an actuated intent.
///
/// Named stages rather than one total, because a budget that cannot be
/// attributed cannot be defended: when the total moves, the question is
/// always *which stage*, and a single number cannot answer it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stage {
    /// Stage name, for the failure message and the published budget.
    pub name: &'static str,
    /// Measured worst case for this stage, in nanoseconds.
    pub wcet_ns: u32,
}

/// The canonical AxonOS budget, measured on the reference hardware
/// (STM32F407 M4F at 168 MHz, ADS1299 front end).
///
/// These are the published figures. Changing one here changes what the whole
/// crate will accept, which is the point: the numbers and the enforcement
/// cannot drift apart because they are the same data.
pub const CANONICAL_STAGES: &[Stage] = &[
    Stage {
        name: "acquisition (DRDY → frame assembled)",
        wcet_ns: 118_000,
    },
    Stage {
        name: "transport (SPI → DSP ring buffer)",
        wcet_ns: 96_000,
    },
    Stage {
        name: "conditioning (DC blocker · notch · band-pass)",
        wcet_ns: 271_000,
    },
    Stage {
        name: "feature extraction",
        wcet_ns: 184_000,
    },
    Stage {
        name: "classification (MDM/LDA inference)",
        wcet_ns: 143_000,
    },
    Stage {
        name: "consent gate",
        wcet_ns: 38_000,
    },
    Stage {
        name: "actuation handoff",
        wcet_ns: 122_000,
    },
];

/// Published worst-case response time for the canonical chain, in nanoseconds.
///
/// Equal to the sum of [`CANONICAL_STAGES`]; the equality is asserted by a
/// test, so the headline figure cannot survive a stage that no longer adds up.
pub const CANONICAL_WCRT_NS: u32 = 972_000;

/// Jitter of the acquisition interrupt on the reference hardware.
pub const CANONICAL_JITTER_SIGMA_NS: u32 = 2_100;
/// Jitter at the 99.9th percentile — the figure a deadline must survive, since
/// a deadline missed one time in a thousand is a deadline missed.
pub const CANONICAL_JITTER_P999_NS: u32 = 6_500;

/// Why a configuration cannot be run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BudgetError {
    /// Sample rate of zero, or one the front end cannot produce.
    UnsupportedRate {
        /// The rate that was asked for.
        sps: u32,
    },
    /// The chain does not fit inside one sample period.
    ///
    /// Carries both terms so the message can be specific about the shortfall
    /// rather than merely reporting failure.
    DeadlineMissed {
        /// Worst case of the chain plus jitter.
        needed_ns: u64,
        /// One sample period at the requested rate.
        available_ns: u64,
    },
    /// It fits, but with less headroom than the policy requires — which is a
    /// different fact from "it fits", and the one that matters when the
    /// hardware is a degree warmer or the flash is a revision slower.
    InsufficientMargin {
        /// Fraction of the period the chain would consume, parts per million.
        utilisation_ppm: u32,
        /// The policy ceiling it exceeded.
        limit_ppm: u32,
    },
    /// Arithmetic overflow while summing stages: a stage figure is nonsense.
    Overflow,
}

/// A configuration that has been proved to close.
///
/// The only way to obtain one is [`TimingBudget::close`]. Downstream code that
/// accepts this type is accepting a value which could not have been built if
/// the deadline were unmet — the proof travels with the data instead of living
/// in a comment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TimingBudget {
    sps: u32,
    period_ns: u64,
    wcrt_ns: u64,
    jitter_p999_ns: u32,
    utilisation_ppm: u32,
}

/// Sample rates the ADS1299 produces from the canonical clock.
pub const SUPPORTED_RATES_SPS: &[u32] = &[250, 500, 1_000, 2_000, 4_000, 8_000, 16_000];

/// Default headroom policy: the chain may occupy at most 80 % of the period.
///
/// Not a superstition — the remaining fifth absorbs interrupt jitter, cache
/// and flash-wait variance, and the fact that a WCET measured today is a
/// lower bound on the WCET of tomorrow's compiler.
pub const DEFAULT_MAX_UTILISATION_PPM: u32 = 800_000;

impl TimingBudget {
    /// Attempt to close the budget for a sample rate.
    ///
    /// The jitter term is added to the chain's worst case, not compared
    /// against it separately: a deadline is met when *everything* fits inside
    /// the period, and an interrupt that arrives 6.5 µs late has spent 6.5 µs
    /// of the same budget the pipeline is spending.
    pub const fn close(
        sps: u32,
        stages: &[Stage],
        jitter_p999_ns: u32,
        max_utilisation_ppm: u32,
    ) -> Result<Self, BudgetError> {
        if sps == 0 {
            return Err(BudgetError::UnsupportedRate { sps });
        }
        let mut supported = false;
        let mut i = 0;
        while i < SUPPORTED_RATES_SPS.len() {
            if SUPPORTED_RATES_SPS[i] == sps {
                supported = true;
            }
            i += 1;
        }
        if !supported {
            return Err(BudgetError::UnsupportedRate { sps });
        }

        let mut sum: u64 = 0;
        let mut j = 0;
        while j < stages.len() {
            sum += stages[j].wcet_ns as u64;
            if sum > u32::MAX as u64 * 16 {
                return Err(BudgetError::Overflow);
            }
            j += 1;
        }
        let needed = sum + jitter_p999_ns as u64;
        let period_ns = 1_000_000_000u64 / sps as u64;

        if needed > period_ns {
            return Err(BudgetError::DeadlineMissed {
                needed_ns: needed,
                available_ns: period_ns,
            });
        }
        // parts per million, computed without floating point
        let utilisation_ppm = ((needed * 1_000_000) / period_ns) as u32;
        if utilisation_ppm > max_utilisation_ppm {
            return Err(BudgetError::InsufficientMargin {
                utilisation_ppm,
                limit_ppm: max_utilisation_ppm,
            });
        }
        Ok(Self {
            sps,
            period_ns,
            wcrt_ns: sum,
            jitter_p999_ns,
            utilisation_ppm,
        })
    }

    /// Close the canonical chain at a rate, with the default headroom policy.
    pub const fn canonical(sps: u32) -> Result<Self, BudgetError> {
        Self::close(
            sps,
            CANONICAL_STAGES,
            CANONICAL_JITTER_P999_NS,
            DEFAULT_MAX_UTILISATION_PPM,
        )
    }

    /// Sample rate this budget was closed for.
    pub const fn sps(&self) -> u32 {
        self.sps
    }
    /// One sample period, nanoseconds.
    pub const fn period_ns(&self) -> u64 {
        self.period_ns
    }
    /// Worst-case chain time excluding jitter, nanoseconds.
    pub const fn wcrt_ns(&self) -> u64 {
        self.wcrt_ns
    }
    /// Fraction of the period consumed, in parts per million.
    pub const fn utilisation_ppm(&self) -> u32 {
        self.utilisation_ppm
    }
    /// Nanoseconds of the period left unspent — the margin an engineer
    /// actually reasons about when adding a stage.
    pub const fn slack_ns(&self) -> u64 {
        self.period_ns - (self.wcrt_ns + self.jitter_p999_ns as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_published_wcrt_equals_the_sum_of_its_stages() {
        let sum: u32 = CANONICAL_STAGES.iter().map(|s| s.wcet_ns).sum();
        assert_eq!(
            sum, CANONICAL_WCRT_NS,
            "the headline figure and the stage table have drifted apart"
        );
    }

    #[test]
    fn the_canonical_rate_closes_with_room() {
        let b = TimingBudget::canonical(250).unwrap();
        assert_eq!(b.period_ns(), 4_000_000);
        assert_eq!(b.wcrt_ns(), 972_000);
        // 978.5 µs of 4000 µs
        assert!(b.utilisation_ppm() < 250_000, "{}", b.utilisation_ppm());
        assert!(b.slack_ns() > 3_000_000);
    }

    #[test]
    fn double_rate_still_closes() {
        let b = TimingBudget::canonical(500).unwrap();
        assert_eq!(b.period_ns(), 2_000_000);
        // ~48.9 %
        assert!(b.utilisation_ppm() > 480_000 && b.utilisation_ppm() < 500_000);
    }

    #[test]
    fn one_kilohertz_is_refused_for_margin_not_for_fit() {
        // 978.5 µs fits inside 1000 µs — but at 97.8 % there is nothing left
        // for a warmer die or a slower compiler, and this must not ship.
        match TimingBudget::canonical(1_000) {
            Err(BudgetError::InsufficientMargin {
                utilisation_ppm,
                limit_ppm,
            }) => {
                assert!(utilisation_ppm > 970_000);
                assert_eq!(limit_ppm, DEFAULT_MAX_UTILISATION_PPM);
            }
            other => panic!("expected an insufficient-margin refusal, got {other:?}"),
        }
    }

    #[test]
    fn two_kilohertz_cannot_fit_at_all() {
        match TimingBudget::canonical(2_000) {
            Err(BudgetError::DeadlineMissed {
                needed_ns,
                available_ns,
            }) => {
                assert_eq!(available_ns, 500_000);
                assert_eq!(needed_ns, 972_000 + CANONICAL_JITTER_P999_NS as u64);
            }
            other => panic!("expected a missed deadline, got {other:?}"),
        }
    }

    #[test]
    fn jitter_is_spent_from_the_same_budget() {
        let strict = TimingBudget::close(250, CANONICAL_STAGES, 0, 1_000_000).unwrap();
        let real = TimingBudget::canonical(250).unwrap();
        assert!(
            real.slack_ns() < strict.slack_ns() || real.slack_ns() == strict.slack_ns() - 6_500
        );
    }

    #[test]
    fn an_added_stage_can_break_the_build() {
        // this is the whole point: budgets are only real if they can fail
        const HEAVY: &[Stage] = &[
            Stage {
                name: "canonical chain",
                wcet_ns: 972_000,
            },
            Stage {
                name: "a new idea nobody costed",
                wcet_ns: 3_200_000,
            },
        ];
        assert!(matches!(
            TimingBudget::close(
                250,
                HEAVY,
                CANONICAL_JITTER_P999_NS,
                DEFAULT_MAX_UTILISATION_PPM
            ),
            Err(BudgetError::DeadlineMissed { .. })
        ));
    }

    #[test]
    fn unsupported_rates_are_named_in_the_error() {
        assert_eq!(
            TimingBudget::canonical(300),
            Err(BudgetError::UnsupportedRate { sps: 300 })
        );
        assert_eq!(
            TimingBudget::canonical(0),
            Err(BudgetError::UnsupportedRate { sps: 0 })
        );
    }

    #[test]
    fn a_relaxed_policy_can_accept_what_the_default_refuses() {
        // an operator who has measured their own margin may set it; they
        // cannot, however, make a missed deadline pass
        assert!(
            TimingBudget::close(1_000, CANONICAL_STAGES, CANONICAL_JITTER_P999_NS, 990_000).is_ok()
        );
        assert!(matches!(
            TimingBudget::close(2_000, CANONICAL_STAGES, CANONICAL_JITTER_P999_NS, 1_000_000),
            Err(BudgetError::DeadlineMissed { .. })
        ));
    }

    #[test]
    fn the_budget_is_const_evaluable() {
        // the deadline can therefore be proved at compile time, not at boot
        const B: Result<TimingBudget, BudgetError> = TimingBudget::canonical(250);
        assert!(B.is_ok());
    }
}
