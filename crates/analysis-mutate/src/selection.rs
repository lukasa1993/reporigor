use std::collections::BTreeSet;

use reporigor_core::{MutationCandidate, MutationOperator};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MutationSelectionError {
    #[error("the fixed mutation operator set must not be empty")]
    EmptyOperators,
    #[error("mutation candidate {id} in {file} has no stable fingerprint")]
    MissingFingerprint { id: u64, file: String },
    #[error("duplicate stable mutation fingerprint {0}")]
    DuplicateFingerprint(String),
}

/// Filter a mutation inventory to the configured fixed operator set and order
/// it deterministically by seed and stable structural fingerprint.
///
/// This function does not execute mutants and does not apply a maximum. The
/// existing crash-safe executor remains the sole owner of execution and its
/// `max_mutants` policy, which means candidates beyond that maximum retain the
/// standard `ignored` status.
///
/// # Errors
///
/// Returns an error when no operators are configured or when a selected
/// candidate has a missing or duplicate stable fingerprint.
pub fn select_candidates(
    candidates: &[MutationCandidate],
    operators: &[MutationOperator],
    seed: u64,
) -> Result<Vec<MutationCandidate>, MutationSelectionError> {
    CandidateSelector::new(operators, seed)?.select(candidates)
}

struct CandidateSelector<'a> {
    allowed: BTreeSet<&'a str>,
    seed: u64,
    fingerprints: BTreeSet<String>,
    selected: Vec<MutationCandidate>,
}

impl<'a> CandidateSelector<'a> {
    fn new(operators: &'a [MutationOperator], seed: u64) -> Result<Self, MutationSelectionError> {
        if operators.is_empty() {
            return Err(MutationSelectionError::EmptyOperators);
        }
        Ok(Self {
            allowed: operators.iter().map(|operator| operator.as_str()).collect(),
            seed,
            fingerprints: BTreeSet::new(),
            selected: Vec::new(),
        })
    }

    fn select(
        mut self,
        candidates: &[MutationCandidate],
    ) -> Result<Vec<MutationCandidate>, MutationSelectionError> {
        for candidate in candidates {
            self.consider(candidate)?;
        }
        Ok(self.into_sorted())
    }

    fn consider(&mut self, candidate: &MutationCandidate) -> Result<(), MutationSelectionError> {
        if !self.allowed.contains(candidate.operator.as_str()) {
            return Ok(());
        }
        self.accept_fingerprint(candidate)?;
        self.selected.push(candidate.clone());
        Ok(())
    }

    fn accept_fingerprint(&mut self, candidate: &MutationCandidate) -> Result<(), MutationSelectionError> {
        if candidate.fingerprint.is_empty() {
            return Err(MutationSelectionError::MissingFingerprint {
                id: candidate.id,
                file: candidate.file.clone(),
            });
        }
        if self.fingerprints.insert(candidate.fingerprint.clone()) {
            Ok(())
        } else {
            Err(MutationSelectionError::DuplicateFingerprint(
                candidate.fingerprint.clone(),
            ))
        }
    }

    fn into_sorted(mut self) -> Vec<MutationCandidate> {
        let seed = self.seed;
        self.selected.sort_by(|left, right| {
            Self::seeded_key(seed, &left.fingerprint)
                .cmp(&Self::seeded_key(seed, &right.fingerprint))
                .then_with(|| left.file.cmp(&right.file))
                .then_with(|| left.stable_symbol.cmp(&right.stable_symbol))
                .then_with(|| left.fingerprint.cmp(&right.fingerprint))
        });
        self.selected
    }

    fn seeded_key(seed: u64, fingerprint: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(seed.to_be_bytes());
        hasher.update(u64::try_from(fingerprint.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(fingerprint.as_bytes());
        hasher.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use reporigor_core::MutationCandidate;

    use super::*;
    use crate::test_support::{candidate as test_candidate, COMPARISON_TEXT};

    fn candidate(id: u64, operator: &str, fingerprint: &str) -> MutationCandidate {
        test_candidate(id, "src/lib.rs", operator, fingerprint, COMPARISON_TEXT)
    }

    fn selected(
        inventory: &[MutationCandidate],
        operators: &[MutationOperator],
        seed: u64,
    ) -> Vec<MutationCandidate> {
        select_candidates(inventory, operators, seed)
            .unwrap_or_else(|error| panic!("selection failed: {error}"))
    }

    #[test]
    fn operator_filter_and_seed_order_are_stable() {
        let inventory = vec![
            candidate(1, "comparison", "aaa"),
            candidate(2, "logical", "bbb"),
            candidate(3, "arithmetic", "ccc"),
        ];
        let operators = vec![MutationOperator::Comparison, MutationOperator::Logical];
        let first = selected(&inventory, &operators, 7);
        let second = selected(&inventory, &operators, 7);
        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|candidate| candidate.operator != "arithmetic"));

        let mut permuted = inventory.clone();
        permuted.reverse();
        let from_permutation = selected(&permuted, &operators, 7);
        assert_eq!(first, from_permutation);

        let baseline_order = selected(&inventory, &operators, 0);
        assert!((1..=100).any(|seed| {
            select_candidates(&inventory, &operators, seed).is_ok_and(|selected| selected != baseline_order)
        }));
    }

    #[test]
    fn invalid_sets_and_unstable_fingerprints_fail_closed() {
        assert!(matches!(
            select_candidates(&[], &[], 0),
            Err(MutationSelectionError::EmptyOperators)
        ));
        assert!(matches!(
            select_candidates(
                &[candidate(1, "comparison", "")],
                &[MutationOperator::Comparison],
                0,
            ),
            Err(MutationSelectionError::MissingFingerprint { .. })
        ));

        let duplicate = candidate(2, "comparison", "same");
        assert!(matches!(
            select_candidates(
                &[candidate(1, "comparison", "same"), duplicate],
                &[MutationOperator::Comparison],
                0,
            ),
            Err(MutationSelectionError::DuplicateFingerprint(_))
        ));
    }

    #[test]
    fn selected_survivor_inventory_has_unique_stable_fingerprints() {
        let selected = select_candidates(
            &[
                candidate(1, "comparison", "survivor-a"),
                candidate(2, "comparison", "survivor-b"),
            ],
            &[MutationOperator::Comparison],
            42,
        )
        .unwrap_or_else(|error| panic!("selection: {error}"));
        let survivor_fingerprints = selected
            .iter()
            .map(|candidate| candidate.fingerprint.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(survivor_fingerprints.len(), selected.len());
        assert!(survivor_fingerprints
            .iter()
            .all(|fingerprint| !fingerprint.is_empty()));
    }
}
