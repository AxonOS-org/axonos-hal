// SPDX-License-Identifier: Apache-2.0 OR MIT
// SPDX-FileCopyrightText: 2026 Denis Yermakou <connect@axonos.org>
//
// Part of the AxonOS Hardware Abstraction Layer. Dual-licensed Apache-2.0 OR
// MIT at your option; see LICENSE-APACHE and LICENSE-MIT. Authored by Denis
// Yermakou for The AxonOS Project — https://axonos.org

//! Operating points, and the rule that a budget closed at one says nothing
//! about another — RFC-0008 N4.
//!
//! [`TimingBudget`] proves a configuration admissible for a *given* execution
//! cost. That cost is not a property of the code: it is a property of the code
//! running on a particular core frequency, with particular memory wait states,
//! with the cache in a particular configuration. Change any of those and every
//! stage time changes with them, which means the proof does not travel.
//!
//! This is exactly why power management is a correctness problem in this system
//! rather than a comfort one. Dropping the core clock to save battery is the
//! ordinary thing to do on a wearable device; doing it under a chain that has
//! proved its deadline at full speed silently invalidates the proof, and the
//! failure appears as a missed sample rather than as an error.
//!
//! So a transition is not a setting here. It is a **re-admission**: the budget
//! is closed again at the destination before the device is allowed to arrive
//! there, and a destination whose budget does not close is refused while the
//! system stays where it is. RFC-0008 N4 requires exactly this, and it is the
//! last requirement that specification lists as unmet.
//!
//! ## The scaling model, and its limits
//!
//! Execution time is scaled from a reference point by the ratio of core clocks,
//! plus a flash wait-state term:
//!
//! ```text
//! C(p) = C_ref · (f_ref / f_p) + w_p · fetch_cycles / f_p
//! ```
//!
//! This is a **model**, and it is optimistic in a way worth stating plainly: it
//! assumes the code is compute-bound. Work dominated by memory latency does not
//! scale with the core clock at all, so halving the clock leaves it nearly
//! unchanged in wall time while this model predicts a doubling — a prediction
//! that is *conservative* in the safe direction. Work dominated by a peripheral
//! that keeps its own clock is the opposite and is **not** conservative, which
//! is why [`OperatingPoint::measured`] exists: a point with a measured
//! execution figure ignores the model entirely.
//!
//! An implementation that has measured every point it can reach should use
//! measured values everywhere. The model is for the points nobody has measured
//! yet, and its only job is to make their absence visible rather than
//! convenient.

use crate::timing::{BudgetError, Stage, TimingBudget};

/// One reachable hardware configuration.
///
/// The tuple RFC-0008 M5 names as an operating point: core frequency, memory
/// wait states, and the cache configuration. Anything that changes an execution
/// time and is not one of these belongs in a measured figure, not in the model.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OperatingPoint {
    /// A name for logs and for the refusal message.
    pub name: &'static str,
    /// Core frequency in kilohertz. Kilohertz rather than hertz so a `u32`
    /// covers the whole plausible range without an awkward unit.
    pub core_khz: u32,
    /// Flash wait states at this frequency.
    pub wait_states: u8,
    /// Measured execution time for the whole task set at this point, in
    /// nanoseconds; `0` means "not measured, scale it from the reference".
    ///
    /// A measured figure always wins. The model exists to make an unmeasured
    /// point visible, not to make it acceptable.
    pub measured_ns: u32,
}

impl OperatingPoint {
    /// A point whose execution cost has been measured. The model is not
    /// consulted for it.
    pub const fn measured(name: &'static str, core_khz: u32, wait_states: u8, ns: u32) -> Self {
        Self {
            name,
            core_khz,
            wait_states,
            measured_ns: ns,
        }
    }

    /// A point whose cost will be scaled from the reference.
    pub const fn modelled(name: &'static str, core_khz: u32, wait_states: u8) -> Self {
        Self {
            name,
            core_khz,
            wait_states,
            measured_ns: 0,
        }
    }

    /// Whether this point's cost is measured rather than modelled.
    pub const fn is_measured(&self) -> bool {
        self.measured_ns != 0
    }
}

/// The reference platform, at which the published figures were taken.
///
/// STM32F407 at 168 MHz with five flash wait states — the configuration
/// RFC-0001's measurements were made in. Every modelled point is scaled from
/// here, so a mistake in this constant is a mistake everywhere at once, which
/// is why it is a named constant rather than a literal at a call site.
pub const REFERENCE: OperatingPoint = OperatingPoint::measured(
    "F407 @168 MHz",
    168_000,
    5,
    crate::timing::CANONICAL_EXECUTION_NS,
);

/// Instruction-fetch cycles charged per wait state **relative to the
/// reference**, per sample period.
///
/// The reference figure of 694.2 µs is a measurement taken at five wait states,
/// so those stalls are already inside it. The model therefore charges only the
/// difference from five, which is why the reference reproduces itself exactly
/// when run through the model — a property the tests assert, because a model
/// that cannot reproduce its own anchor is not calibrated to anything.
///
/// The magnitude is derived rather than guessed: 694.2 µs at 168 MHz is
/// ~116 600 cycles, and a conservative one fetch stall per eight instruction
/// cycles puts each wait state near 2 900 cycles of that total.
pub const FETCH_CYCLES_PER_WAIT_STATE: u64 = 2_900;

/// Why a transition was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransitionError {
    /// The destination's budget does not close. The system stays where it is.
    ///
    /// Carries the underlying reason so the refusal says *why* the destination
    /// is inadmissible, not merely that it is.
    BudgetWouldNotClose {
        /// The destination that was refused.
        to: &'static str,
        /// Why its budget failed.
        cause: BudgetError,
    },
    /// The point's core frequency is zero, which is not a frequency.
    InvalidPoint {
        /// The offending point.
        name: &'static str,
    },
}

/// Execution cost of the task set at a point, in nanoseconds.
///
/// Measured where a measurement exists; scaled from [`REFERENCE`] otherwise.
/// Returns `None` on a zero frequency, and saturates rather than overflowing on
/// an absurdly slow one — a point that would take longer than a `u32` of
/// nanoseconds is refused by the budget anyway, and reaching that check is more
/// useful than wrapping into a plausible small number on the way.
pub fn execution_ns_at(point: &OperatingPoint, reference_ns: u32) -> Option<u32> {
    if point.core_khz == 0 {
        return None;
    }
    if point.is_measured() {
        return Some(point.measured_ns);
    }
    // Compute term: scales inversely with the clock.
    let compute = (reference_ns as u64 * REFERENCE.core_khz as u64) / point.core_khz as u64;
    // Wait-state term, as a *difference* from the reference rather than an
    // absolute cost. The reference figure is a measurement taken at five wait
    // states, so those stalls are already inside it; adding them again would
    // count them twice and make the model disagree with its own anchor by
    // twelve per cent. What the model owes is only the delta: a point with
    // fewer wait states is cheaper than the reference, one with more is dearer,
    // and both are charged at the destination's cycle length because a stall is
    // a fixed number of cycles and a cycle is longer when the clock is slower.
    let delta_ws = point.wait_states as i64 - REFERENCE.wait_states as i64;
    let stall_ns =
        (delta_ws * FETCH_CYCLES_PER_WAIT_STATE as i64 * 1_000_000) / point.core_khz as i64;
    let total = (compute as i64).saturating_add(stall_ns).max(0) as u64;
    Some(if total > u32::MAX as u64 {
        u32::MAX
    } else {
        total as u32
    })
}

/// Close the budget at a specific operating point.
///
/// The same admission test as [`TimingBudget::close`], with the execution cost
/// taken at the destination rather than at the reference. A caller that closes
/// once and then changes frequency has a proof about a machine it is no longer
/// running on.
pub fn close_at(
    point: &OperatingPoint,
    sps: u32,
    tasks: &[Stage],
    jitter_p999_ns: u32,
    max_utilisation_ppm: u32,
    blocking_and_interference_ns: u32,
) -> Result<TimingBudget, BudgetError> {
    let mut reference_sum: u32 = 0;
    let mut i = 0;
    while i < tasks.len() {
        reference_sum = reference_sum.saturating_add(tasks[i].wcet_ns);
        i += 1;
    }
    let scaled = match execution_ns_at(point, reference_sum) {
        Some(v) => v,
        None => return Err(BudgetError::UnsupportedRate { sps }),
    };
    // The scaled figure replaces the stage table's sum. Blocking and
    // interference are *not* scaled: they are bus and interrupt effects whose
    // relationship to the core clock is not this model's to guess, and
    // understating them would be optimistic in the unsafe direction.
    let one = [Stage {
        name: "task set at this operating point",
        wcet_ns: scaled,
    }];
    TimingBudget::close(
        sps,
        &one,
        jitter_p999_ns,
        max_utilisation_ppm,
        blocking_and_interference_ns,
    )
}

/// A device that may only be at a point whose budget closed.
///
/// The type carries the proof: there is no constructor that does not close a
/// budget, and no transition that does not close it again. A caller cannot hold
/// one of these while sitting at an inadmissible point, which is the same
/// discipline `configure` applies to `TimingBudget` one level down.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AdmittedPoint {
    point: OperatingPoint,
    budget: TimingBudget,
    transitions: u32,
}

impl AdmittedPoint {
    /// Admit the reference point, or refuse.
    pub fn reference(
        sps: u32,
        tasks: &[Stage],
        jitter_p999_ns: u32,
        max_utilisation_ppm: u32,
        blocking_and_interference_ns: u32,
    ) -> Result<Self, TransitionError> {
        Self::at(
            &REFERENCE,
            sps,
            tasks,
            jitter_p999_ns,
            max_utilisation_ppm,
            blocking_and_interference_ns,
        )
    }

    /// Admit a specific point, or refuse with the reason.
    pub fn at(
        point: &OperatingPoint,
        sps: u32,
        tasks: &[Stage],
        jitter_p999_ns: u32,
        max_utilisation_ppm: u32,
        blocking_and_interference_ns: u32,
    ) -> Result<Self, TransitionError> {
        if point.core_khz == 0 {
            return Err(TransitionError::InvalidPoint { name: point.name });
        }
        match close_at(
            point,
            sps,
            tasks,
            jitter_p999_ns,
            max_utilisation_ppm,
            blocking_and_interference_ns,
        ) {
            Ok(budget) => Ok(Self {
                point: *point,
                budget,
                transitions: 0,
            }),
            Err(cause) => Err(TransitionError::BudgetWouldNotClose {
                to: point.name,
                cause,
            }),
        }
    }

    /// Move to another point, re-closing the budget there first.
    ///
    /// On refusal the receiver is unchanged and the system stays where it is —
    /// which is the whole point. A transition that half-applies leaves the
    /// device at a frequency whose deadline nothing has proved.
    pub fn transition_to(
        &self,
        point: &OperatingPoint,
        tasks: &[Stage],
        jitter_p999_ns: u32,
        max_utilisation_ppm: u32,
        blocking_and_interference_ns: u32,
    ) -> Result<Self, TransitionError> {
        let mut next = Self::at(
            point,
            self.budget.sps(),
            tasks,
            jitter_p999_ns,
            max_utilisation_ppm,
            blocking_and_interference_ns,
        )?;
        next.transitions = self.transitions.saturating_add(1);
        Ok(next)
    }

    /// The point currently occupied.
    pub const fn point(&self) -> &OperatingPoint {
        &self.point
    }

    /// The budget proved at this point.
    pub const fn budget(&self) -> &TimingBudget {
        &self.budget
    }

    /// Transitions taken since admission. Exposed because a system that changes
    /// operating point constantly is paying a re-admission each time, and the
    /// count is what makes that visible.
    pub const fn transitions(&self) -> u32 {
        self.transitions
    }

    /// Whether the current point's cost is measured rather than modelled.
    ///
    /// A caller running on a modelled point is running on an argument, not on
    /// evidence, and RFC-0003 requires that difference to be legible.
    pub const fn is_measured(&self) -> bool {
        self.point.is_measured()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timing::{
        CANONICAL_BLOCKING_AND_INTERFERENCE_NS, CANONICAL_EXECUTION_NS, CANONICAL_JITTER_P999_NS,
        CANONICAL_TASKS, DEFAULT_MAX_UTILISATION_PPM,
    };

    fn admit(point: &OperatingPoint, sps: u32) -> Result<AdmittedPoint, TransitionError> {
        AdmittedPoint::at(
            point,
            sps,
            CANONICAL_TASKS,
            CANONICAL_JITTER_P999_NS,
            DEFAULT_MAX_UTILISATION_PPM,
            CANONICAL_BLOCKING_AND_INTERFERENCE_NS,
        )
    }

    #[test]
    fn the_model_reproduces_its_own_anchor() {
        // A model that cannot reproduce the point it was calibrated at is not
        // calibrated to anything. The reference is measured, so this asserts
        // the modelled path lands on the same figure when told to model it.
        let modelled = OperatingPoint::modelled("F407 @168 MHz, modelled", 168_000, 5);
        let got = execution_ns_at(&modelled, CANONICAL_EXECUTION_NS).unwrap();
        let error = (got as i64 - CANONICAL_EXECUTION_NS as i64).abs();
        assert!(
            error * 100 / CANONICAL_EXECUTION_NS as i64 <= 3,
            "modelled {got} vs measured {CANONICAL_EXECUTION_NS}, {error} ns apart"
        );
    }

    #[test]
    fn halving_the_clock_roughly_doubles_execution() {
        let half = OperatingPoint::modelled("F407 @84 MHz", 84_000, 3);
        let full = execution_ns_at(&REFERENCE, CANONICAL_EXECUTION_NS).unwrap();
        let slow = execution_ns_at(&half, CANONICAL_EXECUTION_NS).unwrap();
        assert!(slow > full * 19 / 10, "expected ~2x, got {slow} vs {full}");
        assert!(slow < full * 22 / 10);
    }

    #[test]
    fn wait_states_cost_more_at_a_lower_clock() {
        // The term nobody expects: a stall is a fixed number of cycles, and a
        // cycle is longer when the clock is slower, so wait states do not
        // become free on the way down.
        let fast = OperatingPoint::modelled("fast, 5 ws", 168_000, 5);
        let fast_none = OperatingPoint::modelled("fast, 0 ws", 168_000, 0);
        let slow = OperatingPoint::modelled("slow, 5 ws", 42_000, 5);
        let slow_none = OperatingPoint::modelled("slow, 0 ws", 42_000, 0);
        let fast_cost = execution_ns_at(&fast, CANONICAL_EXECUTION_NS).unwrap()
            - execution_ns_at(&fast_none, CANONICAL_EXECUTION_NS).unwrap();
        let slow_cost = execution_ns_at(&slow, CANONICAL_EXECUTION_NS).unwrap()
            - execution_ns_at(&slow_none, CANONICAL_EXECUTION_NS).unwrap();
        assert!(
            slow_cost > fast_cost * 3,
            "{slow_cost} should dwarf {fast_cost}"
        );
    }

    #[test]
    fn a_measured_point_ignores_the_model_entirely() {
        // 84 MHz with a measured figure that happens to be *better* than the
        // model predicts. Evidence outranks an argument.
        let measured = OperatingPoint::measured("84 MHz, measured", 84_000, 3, 900_000);
        assert_eq!(
            execution_ns_at(&measured, CANONICAL_EXECUTION_NS),
            Some(900_000)
        );
        assert!(measured.is_measured());
    }

    #[test]
    fn the_reference_point_is_admitted_at_the_canonical_rate() {
        let a = admit(&REFERENCE, 250).unwrap();
        assert_eq!(a.point().name, REFERENCE.name);
        assert!(a.is_measured());
        assert_eq!(a.transitions(), 0);
    }

    #[test]
    fn dropping_the_clock_can_make_the_same_rate_inadmissible() {
        // The defect N4 exists for. 250 SPS is comfortable at 168 MHz; the same
        // configuration at 21 MHz is not, and nothing in the old design would
        // have noticed the difference.
        let slow = OperatingPoint::modelled("F407 @21 MHz", 21_000, 0);
        match admit(&slow, 250) {
            Err(TransitionError::BudgetWouldNotClose { to, cause }) => {
                assert_eq!(to, "F407 @21 MHz");
                assert!(
                    matches!(
                        cause,
                        BudgetError::InsufficientMargin { .. } | BudgetError::DeadlineMissed { .. }
                    ),
                    "unexpected cause {cause:?}"
                );
            }
            other => panic!("a tenfold clock drop must be refused, got {other:?}"),
        }
    }

    #[test]
    fn a_refused_transition_leaves_the_system_where_it_was() {
        let here = admit(&REFERENCE, 250).unwrap();
        let bad = OperatingPoint::modelled("far too slow", 8_000, 7);
        assert!(here
            .transition_to(
                &bad,
                CANONICAL_TASKS,
                CANONICAL_JITTER_P999_NS,
                DEFAULT_MAX_UTILISATION_PPM,
                CANONICAL_BLOCKING_AND_INTERFERENCE_NS
            )
            .is_err());
        // The receiver is untouched: a half-applied transition would leave the
        // device at a frequency whose deadline nothing has proved.
        assert_eq!(here.point().name, REFERENCE.name);
        assert_eq!(here.transitions(), 0);
    }

    #[test]
    fn an_admissible_transition_is_taken_and_counted() {
        let here = admit(&REFERENCE, 250).unwrap();
        let modest = OperatingPoint::modelled("F407 @120 MHz", 120_000, 3);
        let there = here
            .transition_to(
                &modest,
                CANONICAL_TASKS,
                CANONICAL_JITTER_P999_NS,
                DEFAULT_MAX_UTILISATION_PPM,
                CANONICAL_BLOCKING_AND_INTERFERENCE_NS,
            )
            .unwrap();
        assert_eq!(there.point().name, "F407 @120 MHz");
        assert_eq!(there.transitions(), 1);
        assert!(
            !there.is_measured(),
            "a modelled point must not claim to be measured"
        );
        assert!(
            there.budget().wcrt_ns() > here.budget().wcrt_ns(),
            "a slower clock must cost more, not less"
        );
    }

    #[test]
    fn a_zero_frequency_is_refused_as_a_point_not_as_a_budget() {
        let dead = OperatingPoint::modelled("stopped", 0, 0);
        assert_eq!(execution_ns_at(&dead, CANONICAL_EXECUTION_NS), None);
        assert!(matches!(
            admit(&dead, 250),
            Err(TransitionError::InvalidPoint { .. })
        ));
    }

    #[test]
    fn blocking_and_interference_are_not_scaled_by_this_model() {
        // Bus and interrupt effects have no stated relationship to the core
        // clock, and inventing one would be optimistic in the unsafe
        // direction. The residual is passed through unchanged, so the same
        // figure appears in a budget at any frequency.
        let a = admit(&REFERENCE, 250).unwrap();
        let b = admit(&OperatingPoint::modelled("F407 @120 MHz", 120_000, 3), 250).unwrap();
        let scaled_exec_a = a.budget().wcrt_ns()
            - CANONICAL_JITTER_P999_NS as u64
            - CANONICAL_BLOCKING_AND_INTERFERENCE_NS as u64;
        let scaled_exec_b = b.budget().wcrt_ns()
            - CANONICAL_JITTER_P999_NS as u64
            - CANONICAL_BLOCKING_AND_INTERFERENCE_NS as u64;
        assert!(
            scaled_exec_b > scaled_exec_a,
            "only the execution term moves"
        );
    }

    #[test]
    fn transitions_are_deterministic() {
        let run = || {
            let a = admit(&REFERENCE, 250).unwrap();
            let p = OperatingPoint::modelled("F407 @120 MHz", 120_000, 3);
            a.transition_to(
                &p,
                CANONICAL_TASKS,
                CANONICAL_JITTER_P999_NS,
                DEFAULT_MAX_UTILISATION_PPM,
                CANONICAL_BLOCKING_AND_INTERFERENCE_NS,
            )
            .unwrap()
            .budget()
            .wcrt_ns()
        };
        assert_eq!(run(), run());
    }
}
