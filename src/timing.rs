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

/// The admitted task set, as published in RFC-0001.
///
/// These four figures are L2 evidence: measured on the STM32F407 reference
/// platform over a 12-hour continuous run of 10.8 M epochs. They are the only
/// per-task execution times this project has published, and this table
/// reproduces them rather than inventing a finer split.
///
/// An earlier revision of this crate carried a seven-entry decomposition
/// documented as measured. It was not: it was an invention that summed to the
/// right total. RFC-0003 exists to forbid exactly that, and RFC-0008 D1
/// records the correction.
pub const CANONICAL_TASKS: &[Stage] = &[
    Stage { name: "signal pipeline (Kalman \u{2192} FIR \u{2192} notch \u{2192} artifact \u{2192} CSP \u{2192} LDA)", wcet_ns: 640_200 },
    Stage { name: "consent service", wcet_ns: 12_000 },
    Stage { name: "attestation (HMAC-SHA256 over event)", wcet_ns: 18_000 },
    Stage { name: "network egress (BLE intent publish)", wcet_ns: 24_000 },
];

/// Sum of [`CANONICAL_TASKS`]: total execution time per sample, nanoseconds.
pub const CANONICAL_EXECUTION_NS: u32 = 694_200;

/// Measured end-to-end worst-case *response* time, nanoseconds (RFC-0001, L2).
///
/// This is deliberately not called a WCET. It is a response time, and it
/// already contains the jitter, blocking and interference that the execution
/// figures above do not: 972 000 \u2212 694 200 = **277 800 ns**, of which at
/// most 6 500 ns is release jitter. The remaining ~271 \u00b5s is blocking and
/// interference, present in the measurement and absent from every admission
/// test written before RFC-0008 \u00a74a.
///
/// Because those terms have not been measured separately, this crate admits
/// configurations against the measured response time directly, and does not
/// pretend to a decomposition it does not have.
pub const CANONICAL_WCRT_MEASURED_NS: u32 = 972_000;

/// Blocking plus interference, inferred rather than measured.
///
/// Inferred quantities are named as such. This is the residual of the two
/// published figures after jitter, and it is exposed so that a caller can see
/// the size of what is not yet accounted: it is 28 % of the worst case.
pub const CANONICAL_BLOCKING_AND_INTERFERENCE_NS: u32 =
    CANONICAL_WCRT_MEASURED_NS - CANONICAL_EXECUTION_NS - CANONICAL_JITTER_P999_NS;

/// Deprecated alias retained for one release. Use
/// [`CANONICAL_WCRT_MEASURED_NS`], whose name states which quantity it is.
#[deprecated(
    since = "0.2.0",
    note = "renamed: this is a response time, not an execution time"
)]
pub const CANONICAL_WCRT_NS: u32 = CANONICAL_WCRT_MEASURED_NS;

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

/// The published headroom policy: **U_max = 0.25** (RFC-0001, which records the
/// admitted task set at U = 0.174 with 0.076 remaining).
///
/// An earlier revision of this crate used 0.80, a figure with no published
/// basis. The consequence was not theoretical: it admitted 500 SPS, which the
/// published policy refuses at U = 0.347. A ceiling chosen for plausibility is
/// discovered by the first configuration it wrongly admits (RFC-0008 W4, D2).
pub const DEFAULT_MAX_UTILISATION_PPM: u32 = 250_000;

impl TimingBudget {
    /// Attempt to close the budget for a sample rate.
    ///
    /// The jitter term is added to the chain's worst case, not compared
    /// against it separately: a deadline is met when *everything* fits inside
    /// the period, and an interrupt that arrives 6.5 µs late has spent 6.5 µs
    /// of the same budget the pipeline is spending.
    /// `blocking_and_interference_ns` is mandatory, including when it is zero:
    /// RFC-0008 N3 requires every term of R = J + B + C + I to be declared, and
    /// a term omitted from an API cannot be declared at all.
    pub const fn close(
        sps: u32,
        stages: &[Stage],
        jitter_p999_ns: u32,
        max_utilisation_ppm: u32,
        blocking_and_interference_ns: u32,
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
        let needed = sum + jitter_p999_ns as u64 + blocking_and_interference_ns as u64;
        let period_ns = 1_000_000_000u64 / sps as u64;

        if needed > period_ns {
            return Err(BudgetError::DeadlineMissed {
                needed_ns: needed,
                available_ns: period_ns,
            });
        }
        // The utilisation test is on execution alone: the published ceiling is
        // a policy about how much of the period the task set may claim, not
        // about the response time, and conflating them would compare a figure
        // against a ceiling that was never set for it.
        let utilisation_ppm = ((sum * 1_000_000) / period_ns) as u32;
        if utilisation_ppm > max_utilisation_ppm {
            return Err(BudgetError::InsufficientMargin {
                utilisation_ppm,
                limit_ppm: max_utilisation_ppm,
            });
        }
        Ok(Self {
            sps,
            period_ns,
            wcrt_ns: needed,
            jitter_p999_ns,
            utilisation_ppm,
        })
    }

    /// Close the canonical configuration at a rate.
    ///
    /// Two independent tests, because they answer different questions and the
    /// published figures make them disagree. The deadline test asks whether the
    /// measured response fits the period; the utilisation test asks whether the
    /// admitted task set fits the published headroom policy. At 500 SPS the
    /// first passes and the second does not, and the configuration is
    /// inadmissible (RFC-0008 §7).
    pub const fn canonical(sps: u32) -> Result<Self, BudgetError> {
        Self::close(
            sps,
            CANONICAL_TASKS,
            CANONICAL_JITTER_P999_NS,
            DEFAULT_MAX_UTILISATION_PPM,
            CANONICAL_BLOCKING_AND_INTERFERENCE_NS,
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
    /// Worst-case **response** time: J + B + C + I, nanoseconds.
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
        // wcrt_ns is the full response — J, B, C and I — so jitter must not be
        // added a second time. Doing so was the O1 error in v0.1.1.
        self.period_ns - self.wcrt_ns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_table_reproduces_the_published_sum() {
        let sum: u32 = CANONICAL_TASKS.iter().map(|s| s.wcet_ns).sum();
        assert_eq!(sum, CANONICAL_EXECUTION_NS, "RFC-0001 publishes 694.2 us");
        assert_eq!(
            CANONICAL_TASKS.len(),
            4,
            "four tasks are published, not seven"
        );
    }

    #[test]
    fn the_gap_between_execution_and_measured_response_is_exposed() {
        // The whole point of RFC-0008 4a: B + I is not zero, and the published
        // figures say how large it is.
        assert_eq!(
            CANONICAL_BLOCKING_AND_INTERFERENCE_NS,
            972_000 - 694_200 - 6_500
        );
        assert_eq!(CANONICAL_BLOCKING_AND_INTERFERENCE_NS, 271_300);
        let share = CANONICAL_BLOCKING_AND_INTERFERENCE_NS as f64 / 972_000.0;
        assert!(
            share > 0.27,
            "unaccounted terms are {:.1}% of the worst case",
            share * 100.0
        );
    }

    #[test]
    fn the_canonical_rate_is_admitted_at_the_published_utilisation() {
        let b = TimingBudget::canonical(250).unwrap();
        assert_eq!(b.period_ns(), 4_000_000);
        // RFC-0001 publishes U = 0.174 for this task set at 250 SPS
        assert_eq!(b.utilisation_ppm(), 173_550);
        assert!(b.utilisation_ppm() < DEFAULT_MAX_UTILISATION_PPM);
        // and the response, which carries every term, still fits comfortably
        assert_eq!(b.wcrt_ns(), 972_000);
    }

    #[test]
    fn five_hundred_sps_is_now_refused_and_this_is_the_correction() {
        // v0.1.1 admitted this at a self-chosen 80% ceiling. The published
        // ceiling is 0.25 and the task set claims 0.347 of the period.
        match TimingBudget::canonical(500) {
            Err(BudgetError::InsufficientMargin {
                utilisation_ppm,
                limit_ppm,
            }) => {
                assert_eq!(utilisation_ppm, 347_100);
                assert_eq!(limit_ppm, 250_000);
            }
            other => panic!("500 SPS must be refused by the published ceiling, got {other:?}"),
        }
    }

    #[test]
    fn the_deadline_test_and_the_ceiling_answer_different_questions() {
        // At 500 SPS the response fits the period — 972 us inside 2000 us —
        // and the configuration is still inadmissible. Conflating the two
        // tests would report a missed deadline that did not happen.
        let r = 972_000u64;
        assert!(r < 2_000_000, "the deadline itself is met at 500 SPS");
        assert!(matches!(
            TimingBudget::canonical(500),
            Err(BudgetError::InsufficientMargin { .. })
        ));
    }

    #[test]
    fn one_kilohertz_is_refused_by_the_ceiling_too() {
        match TimingBudget::canonical(1_000) {
            Err(BudgetError::InsufficientMargin {
                utilisation_ppm, ..
            }) => {
                assert_eq!(utilisation_ppm, 694_200);
            }
            other => panic!("expected the ceiling to refuse 1 kSPS, got {other:?}"),
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
                assert_eq!(needed_ns, 972_000, "every term of R is in the figure");
            }
            other => panic!("expected a missed deadline, got {other:?}"),
        }
    }

    #[test]
    fn blocking_and_interference_are_a_mandatory_argument() {
        // RFC-0008 N3: a term omitted from the API cannot be declared. An
        // implementation may pass zero, and by doing so asserts something
        // about its whole system.
        let with_zero = TimingBudget::close(250, CANONICAL_TASKS, 6_500, 250_000, 0).unwrap();
        let with_real = TimingBudget::canonical(250).unwrap();
        assert!(with_zero.slack_ns() > with_real.slack_ns());
        assert_eq!(with_real.slack_ns() + 271_300, with_zero.slack_ns());
    }

    #[test]
    fn an_added_task_can_break_the_build() {
        const HEAVY: &[Stage] = &[
            Stage {
                name: "published set",
                wcet_ns: 694_200,
            },
            Stage {
                name: "an idea nobody costed",
                wcet_ns: 3_200_000,
            },
        ];
        assert!(matches!(
            TimingBudget::close(250, HEAVY, 6_500, 250_000, 271_300),
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
    fn a_declared_policy_may_differ_but_must_be_declared() {
        // An operator with their own measured margin may set it; they cannot
        // make a missed deadline pass.
        assert!(TimingBudget::close(500, CANONICAL_TASKS, 6_500, 400_000, 271_300).is_ok());
        assert!(matches!(
            TimingBudget::close(2_000, CANONICAL_TASKS, 6_500, 1_000_000, 271_300),
            Err(BudgetError::DeadlineMissed { .. })
        ));
    }

    #[test]
    fn the_budget_is_const_evaluable() {
        const B: Result<TimingBudget, BudgetError> = TimingBudget::canonical(250);
        assert!(B.is_ok());
    }
}
