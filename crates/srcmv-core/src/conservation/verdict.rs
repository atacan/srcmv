//! Verdict derivation and machine-checkable ledger invariants.
//!
//! Implements §9.1's strictly ordered derivation and the §9.2 identity. The
//! engine recomputes totals from per-region outcomes so a bookkeeping bug
//! fails loudly instead of fabricating a verdict, and debug builds assert
//! both the ledger identity and the per-head-file consumption guarantees.

use super::{InconclusiveReason, Totals, Verdict};

/// Derives the verdict in §9.1's strict order.
///
/// `reasons` holds every fatal condition observed by the run (§9.3); any
/// entry forces [`Verdict::Inconclusive`]. Otherwise a positive
/// unmatched-substantial term fails the audit, formatting-only bytes fail it
/// under `count_formatting`, and everything else passes.
#[must_use]
pub(crate) fn derive_verdict(
    reasons: &[InconclusiveReason],
    totals: &Totals,
    count_formatting: bool,
) -> Verdict {
    if !reasons.is_empty() {
        return Verdict::Inconclusive;
    }
    if totals.unmatched_substantial_bytes > 0 {
        return Verdict::Fail;
    }
    if count_formatting && totals.formatting_only_bytes > 0 {
        return Verdict::Fail;
    }
    Verdict::Pass
}

/// Returns whether the §9.2 ledger identity holds exactly.
///
/// Every removed byte must land in exactly one of the seven ledger terms.
#[must_use]
pub(crate) fn ledger_identity_holds(totals: &Totals) -> bool {
    let conserved = totals
        .conserved_exact_bytes
        .saturating_add(totals.conserved_fragments_bytes);
    let partial = conserved
        .saturating_add(totals.unmatched_substantial_bytes)
        .saturating_add(totals.insubstantial_residue_bytes);
    let partial = partial
        .saturating_add(totals.formatting_only_bytes)
        .saturating_add(totals.ambiguous_common_bytes);
    let sum = partial.saturating_add(totals.unanalyzed_bytes);
    sum == totals.removed_bytes && conserved == totals.conserved_bytes
}

/// Deduplicates and orders inconclusive reasons canonically.
#[must_use]
pub(crate) fn canonical_reasons(reasons: Vec<InconclusiveReason>) -> Vec<InconclusiveReason> {
    let mut ordered = reasons;
    ordered.sort_unstable();
    ordered.dedup();
    ordered
}

#[cfg(test)]
mod tests {
    use super::{canonical_reasons, derive_verdict, ledger_identity_holds};
    use crate::{InconclusiveReason, Totals};

    fn totals(removed: u64, terms: [u64; 7]) -> Totals {
        Totals {
            removed_regions: 1,
            removed_bytes: removed,
            conserved_exact_bytes: terms[0],
            conserved_fragments_bytes: terms[1],
            insubstantial_residue_bytes: terms[2],
            unmatched_substantial_bytes: terms[3],
            formatting_only_bytes: terms[4],
            ambiguous_common_bytes: terms[5],
            unanalyzed_bytes: terms[6],
            conserved_bytes: terms[0] + terms[1],
            new_bytes: 0,
            pairing_ambiguous_regions: 0,
            ambiguous_common_regions: 0,
        }
    }

    #[test]
    fn verdict_should_follow_the_documented_truth_table_for_every_reason() {
        let clean = totals(10, [10, 0, 0, 0, 0, 0, 0]);
        assert_eq!(derive_verdict(&[], &clean, false), crate::Verdict::Pass);
        assert_eq!(derive_verdict(&[], &clean, true), crate::Verdict::Pass);

        let unmatched = totals(42, [0, 0, 0, 42, 0, 0, 0]);
        assert_eq!(derive_verdict(&[], &unmatched, false), crate::Verdict::Fail);

        let formatting = totals(6, [0, 0, 0, 0, 6, 0, 0]);
        assert_eq!(
            derive_verdict(&[], &formatting, false),
            crate::Verdict::Pass
        );
        assert_eq!(derive_verdict(&[], &formatting, true), crate::Verdict::Fail);

        // Every single reason forces Inconclusive regardless of the ledger.
        for reason in [
            InconclusiveReason::WorkBudgetExceeded,
            InconclusiveReason::MemoryBudgetExceeded,
            InconclusiveReason::UnreadableInputSkipped,
            InconclusiveReason::StrictUniquenessAmbiguity,
            InconclusiveReason::AmbiguousCommon,
            InconclusiveReason::ReportTruncated,
        ] {
            assert_eq!(
                derive_verdict(&[reason], &unmatched, false),
                crate::Verdict::Inconclusive
            );
            assert_eq!(
                derive_verdict(&[reason], &formatting, true),
                crate::Verdict::Inconclusive
            );
        }

        // Reasons dominate an otherwise-failing ledger.
        let mixed = totals(50, [8, 0, 2, 30, 6, 4, 0]);
        assert_eq!(
            derive_verdict(&[InconclusiveReason::AmbiguousCommon], &mixed, false),
            crate::Verdict::Inconclusive
        );
    }

    #[test]
    fn ledger_identity_should_hold_exactly_when_terms_partition_the_removed_bytes() {
        let full = totals(100, [40, 20, 15, 10, 9, 5, 1]);
        assert!(ledger_identity_holds(&full));

        let with_ambiguous_and_unanalyzed = totals(64, [0, 16, 0, 0, 0, 32, 16]);
        assert!(ledger_identity_holds(&with_ambiguous_and_unanalyzed));

        let broken = totals(64, [0, 16, 0, 0, 0, 32, 15]);
        assert!(!ledger_identity_holds(&broken));

        let zero = totals(0, [0, 0, 0, 0, 0, 0, 0]);
        assert!(ledger_identity_holds(&zero));
    }

    #[test]
    fn reasons_should_dedupe_and_order_canonically() {
        let ordered = canonical_reasons(vec![
            InconclusiveReason::AmbiguousCommon,
            InconclusiveReason::WorkBudgetExceeded,
            InconclusiveReason::AmbiguousCommon,
        ]);
        assert_eq!(
            ordered,
            vec![
                InconclusiveReason::WorkBudgetExceeded,
                InconclusiveReason::AmbiguousCommon
            ]
        );
        assert_eq!(
            InconclusiveReason::WorkBudgetExceeded.as_str(),
            "work_budget_exceeded"
        );
        assert_eq!(
            InconclusiveReason::MemoryBudgetExceeded.as_str(),
            "memory_budget_exceeded"
        );
        assert_eq!(
            InconclusiveReason::UnreadableInputSkipped.as_str(),
            "unreadable_input_skipped"
        );
        assert_eq!(
            InconclusiveReason::StrictUniquenessAmbiguity.as_str(),
            "strict_uniqueness_ambiguity"
        );
        assert_eq!(
            InconclusiveReason::AmbiguousCommon.as_str(),
            "ambiguous_common"
        );
        assert_eq!(
            InconclusiveReason::ReportTruncated.as_str(),
            "report_truncated"
        );
    }
}
