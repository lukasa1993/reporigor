//! Language-neutral duplicate-code detection over normalized token streams.
//!
//! Language adapters own token normalization. This crate groups matching token
//! windows, verifies every hash match against the original token values, and
//! reports maximally extended, non-overlapping clone pairs.

use std::collections::{BTreeMap, HashMap};

use reporigor_core::{
    DryConfig, TokenRecord, DRY_DEFAULT_MAX_CANDIDATE_WORK, DRY_DEFAULT_MAX_FINGERPRINT_BUCKETS,
    DRY_DEFAULT_MAX_TOTAL_WINDOWS, DRY_HARD_MAX_CANDIDATE_WORK, DRY_HARD_MAX_FINGERPRINT_BUCKETS,
    DRY_HARD_MAX_TOTAL_WINDOWS,
};
use serde::{Deserialize, Serialize};

/// The smallest supported matching window.
pub const MIN_TOKENS: usize = 4;

/// Immutable and configurable work limits for one duplicate-analysis run.
///
/// Repository configuration may lower these limits or raise the defaults up
/// to the compiled hard ceilings. It can never disable the ceilings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DryWorkBudget {
    pub max_total_windows: usize,
    pub max_fingerprint_buckets: usize,
    pub max_candidate_work: usize,
}

impl Default for DryWorkBudget {
    fn default() -> Self {
        Self {
            max_total_windows: DRY_DEFAULT_MAX_TOTAL_WINDOWS,
            max_fingerprint_buckets: DRY_DEFAULT_MAX_FINGERPRINT_BUCKETS,
            max_candidate_work: DRY_DEFAULT_MAX_CANDIDATE_WORK,
        }
    }
}

impl From<&DryConfig> for DryWorkBudget {
    fn from(config: &DryConfig) -> Self {
        Self {
            max_total_windows: config.max_total_windows,
            max_fingerprint_buckets: config.max_fingerprint_buckets,
            max_candidate_work: config.max_candidate_work,
        }
    }
}

/// Work category used by typed budget diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkBudgetKind {
    TotalWindows,
    FingerprintBuckets,
    CandidateWork,
}

impl std::fmt::Display for WorkBudgetKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TotalWindows => "total windows",
            Self::FingerprintBuckets => "fingerprint buckets",
            Self::CandidateWork => "candidate work",
        })
    }
}

/// A half-open token range containing one occurrence of a duplicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Internal half-open token offset used for overlap and containment checks.
    ///
    /// Token offsets were not part of the stable Rust DRY JSON contract, so
    /// they deliberately remain absent from serialized reports.
    #[serde(skip)]
    pub start_token: usize,
    /// Internal exclusive end token offset.
    #[serde(skip)]
    pub end_token: usize,
}

/// One duplicate group. The generic engine currently emits clone pairs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Duplicate {
    pub token_count: usize,
    pub locations: Vec<Location>,
}

/// Invalid duplicate-analysis limits.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DryError {
    #[error("min_tokens must be at least {MIN_TOKENS}, got {provided}")]
    MinTokens { provided: usize },
    #[error("max_groups must be greater than zero")]
    MaxGroups,
    #[error("max_occurrences_per_window must be at least 2, got {provided}")]
    MaxOccurrencesPerWindow { provided: usize },
    #[error("{kind} budget must be in 1..={hard_limit}, got {provided}")]
    InvalidWorkBudget {
        kind: WorkBudgetKind,
        provided: usize,
        hard_limit: usize,
    },
    #[error("{kind} budget exceeded: limit {limit}, required at least {required}")]
    WorkBudgetExceeded {
        kind: WorkBudgetKind,
        limit: usize,
        required: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Occurrence {
    file_index: usize,
    start: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Fingerprint {
    first: u64,
    second: u64,
}

impl Fingerprint {
    const ZERO: Self = Self { first: 0, second: 0 };
}

#[derive(Debug, Clone, Copy)]
struct CandidateWork {
    used: usize,
    limit: usize,
}

impl CandidateWork {
    const fn new(limit: usize) -> Self {
        Self { used: 0, limit }
    }

    fn consume(&mut self, amount: usize) -> Result<(), DryError> {
        let required = self.used.saturating_add(amount);
        if required > self.limit {
            return Err(DryError::WorkBudgetExceeded {
                kind: WorkBudgetKind::CandidateWork,
                limit: self.limit,
                required,
            });
        }
        self.used = required;
        Ok(())
    }
}

/// Normalized tokens keyed by root-relative source path.
pub type TokenMap = BTreeMap<String, Vec<TokenRecord>>;

/// Analyze normalized tokens using the shared DRY configuration.
///
/// # Errors
///
/// Returns [`DryError`] when any configured analysis limit is invalid or a
/// configured total-window, fingerprint-bucket, or candidate-work budget is
/// exhausted.
pub fn analyze(tokens: &TokenMap, config: &DryConfig) -> Result<Vec<Duplicate>, DryError> {
    find_duplicates_with_budget(
        tokens,
        config.min_tokens,
        config.max_groups,
        config.max_occurrences_per_window,
        DryWorkBudget::from(config),
    )
}

/// Find duplicate token ranges across and within files.
///
/// Files are consumed in `BTreeMap` order and windows in source order. This
/// makes occurrence caps and the final result deterministic. A hash match is
/// only a candidate: token values are compared before a clone is accepted.
///
/// # Errors
///
/// Returns [`DryError`] when `min_tokens` is less than [`MIN_TOKENS`], when
/// `max_groups` is zero, when `max_occurrences_per_window` is less than two, or
/// when one of the default fail-closed work budgets is exhausted.
pub fn find_duplicates(
    tokens: &TokenMap,
    min_tokens: usize,
    max_groups: usize,
    max_occurrences_per_window: usize,
) -> Result<Vec<Duplicate>, DryError> {
    find_duplicates_with_budget(
        tokens,
        min_tokens,
        max_groups,
        max_occurrences_per_window,
        DryWorkBudget::default(),
    )
}

/// Find duplicate token ranges under explicit fail-closed work budgets.
///
/// Fingerprinting hashes each normalized token once and then derives every
/// fixed-size window in constant time with two independent rolling `u64`
/// lanes. Candidate windows are always compared against the original token
/// values before they are accepted.
///
/// # Errors
///
/// Returns [`DryError`] for invalid analysis limits, an unsafe work budget, or
/// when the input exceeds a configured total-window, fingerprint-bucket, or
/// candidate-work budget.
pub fn find_duplicates_with_budget(
    tokens: &TokenMap,
    min_tokens: usize,
    max_groups: usize,
    max_occurrences_per_window: usize,
    budget: DryWorkBudget,
) -> Result<Vec<Duplicate>, DryError> {
    validate_limits(min_tokens, max_groups, max_occurrences_per_window, budget)?;
    find_duplicates_with_token_hasher(
        tokens,
        min_tokens,
        max_groups,
        max_occurrences_per_window,
        budget,
        stable_token_fingerprint,
    )
}

fn validate_limits(
    min_tokens: usize,
    max_groups: usize,
    max_occurrences_per_window: usize,
    budget: DryWorkBudget,
) -> Result<(), DryError> {
    if min_tokens < MIN_TOKENS {
        return Err(DryError::MinTokens { provided: min_tokens });
    }
    if max_groups == 0 {
        return Err(DryError::MaxGroups);
    }
    if max_occurrences_per_window < 2 {
        return Err(DryError::MaxOccurrencesPerWindow {
            provided: max_occurrences_per_window,
        });
    }
    validate_work_budget(
        WorkBudgetKind::TotalWindows,
        budget.max_total_windows,
        DRY_HARD_MAX_TOTAL_WINDOWS,
    )?;
    validate_work_budget(
        WorkBudgetKind::FingerprintBuckets,
        budget.max_fingerprint_buckets,
        DRY_HARD_MAX_FINGERPRINT_BUCKETS,
    )?;
    validate_work_budget(
        WorkBudgetKind::CandidateWork,
        budget.max_candidate_work,
        DRY_HARD_MAX_CANDIDATE_WORK,
    )?;
    Ok(())
}

fn validate_work_budget(kind: WorkBudgetKind, provided: usize, hard_limit: usize) -> Result<(), DryError> {
    if provided == 0 || provided > hard_limit {
        return Err(DryError::InvalidWorkBudget {
            kind,
            provided,
            hard_limit,
        });
    }
    Ok(())
}

fn find_duplicates_with_token_hasher<F>(
    token_map: &TokenMap,
    min_tokens: usize,
    max_groups: usize,
    max_occurrences_per_window: usize,
    budget: DryWorkBudget,
    token_hasher: F,
) -> Result<Vec<Duplicate>, DryError>
where
    F: Fn(&str) -> Fingerprint,
{
    let files: Vec<_> = token_map.keys().cloned().collect();
    let token_sets: Vec<_> = token_map.values().map(Vec::as_slice).collect();
    let windows = build_window_index(
        &token_sets,
        min_tokens,
        max_occurrences_per_window,
        budget,
        &token_hasher,
    )?;
    let candidates = build_candidates(
        &files,
        &token_sets,
        &windows,
        min_tokens,
        budget.max_candidate_work,
    )?;
    Ok(select_candidates(candidates, max_groups))
}

fn build_window_index<F>(
    token_sets: &[&[TokenRecord]],
    min_tokens: usize,
    max_occurrences_per_window: usize,
    budget: DryWorkBudget,
    token_hasher: &F,
) -> Result<HashMap<Fingerprint, Vec<Occurrence>>, DryError>
where
    F: Fn(&str) -> Fingerprint,
{
    let required_windows = count_windows(token_sets, min_tokens);
    if required_windows > budget.max_total_windows {
        return Err(DryError::WorkBudgetExceeded {
            kind: WorkBudgetKind::TotalWindows,
            limit: budget.max_total_windows,
            required: required_windows,
        });
    }
    // Avoid eagerly reserving the entire repository-controlled allowance.
    // The map still grows to the configured, immutable-capped limit as needed.
    let initial_capacity = required_windows.min(budget.max_fingerprint_buckets).min(16_384);
    let mut windows: HashMap<Fingerprint, Vec<Occurrence>> = HashMap::with_capacity(initial_capacity);

    for (file_index, tokens) in token_sets.iter().enumerate() {
        for_each_window_fingerprint(tokens, min_tokens, token_hasher, |start, key| {
            let bucket_count = windows.len();
            let occurrences = match windows.entry(key) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let required = bucket_count.saturating_add(1);
                    if required > budget.max_fingerprint_buckets {
                        return Err(DryError::WorkBudgetExceeded {
                            kind: WorkBudgetKind::FingerprintBuckets,
                            limit: budget.max_fingerprint_buckets,
                            required,
                        });
                    }
                    entry.insert(Vec::new())
                }
            };
            if occurrences.len() < max_occurrences_per_window {
                occurrences.push(Occurrence { file_index, start });
            }
            Ok(())
        })?;
    }
    Ok(windows)
}

fn build_candidates(
    files: &[String],
    token_sets: &[&[TokenRecord]],
    windows: &HashMap<Fingerprint, Vec<Occurrence>>,
    min_tokens: usize,
    max_candidate_work: usize,
) -> Result<Vec<Duplicate>, DryError> {
    let mut candidates = BTreeMap::new();
    let mut candidate_buckets: Vec<_> = windows
        .iter()
        .filter(|(_, occurrences)| occurrences.len() > 1)
        .collect();
    candidate_buckets.sort_unstable_by_key(|(fingerprint, _)| **fingerprint);
    let mut candidate_work = CandidateWork::new(max_candidate_work);
    for (_, occurrences) in candidate_buckets {
        for left_index in 0..occurrences.len() {
            for right_index in left_index + 1..occurrences.len() {
                // Account for pair dispatch even when the first token differs,
                // so a forced-collision bucket cannot bypass the work budget.
                candidate_work.consume(1)?;
                let left = occurrences[left_index];
                let right = occurrences[right_index];
                let left_tokens = token_sets[left.file_index];
                let right_tokens = token_sets[right.file_index];

                // Fingerprints only select a small candidate set. This exact
                // comparison is required for correctness under hash collision.
                if !windows_equal(
                    left_tokens,
                    left.start,
                    right_tokens,
                    right.start,
                    min_tokens,
                    &mut candidate_work,
                )? {
                    continue;
                }

                let (left_start, right_start, size) = extend_match(
                    left_tokens,
                    left.start,
                    right_tokens,
                    right.start,
                    min_tokens,
                    &mut candidate_work,
                )?;
                let mut first = location(&files[left.file_index], left_tokens, left_start, size);
                let mut second = location(&files[right.file_index], right_tokens, right_start, size);

                if overlaps(&first, &second) {
                    continue;
                }
                if location_key(&second) < location_key(&first) {
                    std::mem::swap(&mut first, &mut second);
                }

                let key = (
                    first.file.clone(),
                    first.start_token,
                    second.file.clone(),
                    second.start_token,
                    size,
                );
                candidates.entry(key).or_insert(Duplicate {
                    token_count: size,
                    locations: vec![first, second],
                });
            }
        }
    }

    let mut candidates: Vec<_> = candidates.into_values().collect();
    candidates.sort_by(compare_duplicates);
    Ok(candidates)
}

fn select_candidates(candidates: Vec<Duplicate>, max_groups: usize) -> Vec<Duplicate> {
    let mut selected = Vec::new();
    for candidate in candidates {
        if selected.iter().any(|existing| contained_by(&candidate, existing)) {
            continue;
        }
        selected.push(candidate);
        if selected.len() == max_groups {
            break;
        }
    }
    selected
}

fn count_windows(token_sets: &[&[TokenRecord]], min_tokens: usize) -> usize {
    token_sets.iter().fold(0_usize, |total, tokens| {
        let windows = if tokens.len() < min_tokens {
            0
        } else {
            tokens.len() - min_tokens + 1
        };
        total.saturating_add(windows)
    })
}

const WINDOW_BASE_FIRST: u64 = 0x9e37_79b1_85eb_ca87;
const WINDOW_BASE_SECOND: u64 = 0xc2b2_ae3d_27d4_eb4f;
const TOKEN_SEED_FIRST: u64 = 0xcbf2_9ce4_8422_2325;
const TOKEN_SEED_SECOND: u64 = 0x6a09_e667_f3bc_c909;
const TOKEN_MULTIPLIER_FIRST: u64 = 0x0000_0100_0000_01b3;
const TOKEN_MULTIPLIER_SECOND: u64 = 0x1656_67b1_9e37_79f9;

fn stable_token_fingerprint(value: &str) -> Fingerprint {
    Fingerprint {
        first: fingerprint_token_lane(value.as_bytes(), TOKEN_SEED_FIRST, TOKEN_MULTIPLIER_FIRST),
        second: fingerprint_token_lane(value.as_bytes(), TOKEN_SEED_SECOND, TOKEN_MULTIPLIER_SECOND),
    }
}

fn fingerprint_token_lane(bytes: &[u8], seed: u64, multiplier: u64) -> u64 {
    let mut value = seed ^ (bytes.len() as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
    for byte in bytes {
        value ^= u64::from(*byte);
        value = value.wrapping_mul(multiplier);
        value ^= value >> 29;
    }
    // Finalize short tokens too; adapters intentionally normalize many tokens
    // to tiny values such as `ID`, `NUM`, and punctuation.
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
    value ^= value >> 33;
    value
}

fn for_each_window_fingerprint<F, A>(
    tokens: &[TokenRecord],
    size: usize,
    token_hasher: &F,
    mut accept: A,
) -> Result<(), DryError>
where
    F: Fn(&str) -> Fingerprint,
    A: FnMut(usize, Fingerprint) -> Result<(), DryError>,
{
    if tokens.len() < size {
        return Ok(());
    }

    // Each token value is read and fingerprinted exactly once. Subsequent
    // window fingerprints are O(1) rolling updates.
    let token_fingerprints: Vec<_> = tokens.iter().map(|token| token_hasher(&token.value)).collect();
    let first_power = wrapping_pow(WINDOW_BASE_FIRST, size - 1);
    let second_power = wrapping_pow(WINDOW_BASE_SECOND, size - 1);
    let mut window = Fingerprint::ZERO;
    for token in &token_fingerprints[..size] {
        window.first = window
            .first
            .wrapping_mul(WINDOW_BASE_FIRST)
            .wrapping_add(token.first);
        window.second = window
            .second
            .wrapping_mul(WINDOW_BASE_SECOND)
            .wrapping_add(token.second);
    }
    accept(0, window)?;

    for start in 1..=tokens.len() - size {
        let outgoing = token_fingerprints[start - 1];
        let incoming = token_fingerprints[start + size - 1];
        window.first = window
            .first
            .wrapping_sub(outgoing.first.wrapping_mul(first_power))
            .wrapping_mul(WINDOW_BASE_FIRST)
            .wrapping_add(incoming.first);
        window.second = window
            .second
            .wrapping_sub(outgoing.second.wrapping_mul(second_power))
            .wrapping_mul(WINDOW_BASE_SECOND)
            .wrapping_add(incoming.second);
        accept(start, window)?;
    }
    Ok(())
}

fn wrapping_pow(mut base: u64, mut exponent: usize) -> u64 {
    let mut result = 1_u64;
    while exponent > 0 {
        if exponent & 1 == 1 {
            result = result.wrapping_mul(base);
        }
        base = base.wrapping_mul(base);
        exponent >>= 1;
    }
    result
}

fn windows_equal(
    left: &[TokenRecord],
    left_start: usize,
    right: &[TokenRecord],
    right_start: usize,
    size: usize,
    work: &mut CandidateWork,
) -> Result<bool, DryError> {
    let Some((left_window, right_window)) = left
        .get(left_start..left_start + size)
        .zip(right.get(right_start..right_start + size))
    else {
        return Ok(false);
    };
    for (first, second) in left_window.iter().zip(right_window) {
        work.consume(1)?;
        if first.value != second.value {
            return Ok(false);
        }
    }
    Ok(true)
}

fn extend_match(
    left: &[TokenRecord],
    left_start: usize,
    right: &[TokenRecord],
    right_start: usize,
    minimum: usize,
    work: &mut CandidateWork,
) -> Result<(usize, usize, usize), DryError> {
    let mut before = 0;
    while left_start > before && right_start > before {
        work.consume(1)?;
        if left[left_start - before - 1].value != right[right_start - before - 1].value {
            break;
        }
        before += 1;
    }

    let mut after = minimum;
    while left_start + after < left.len() && right_start + after < right.len() {
        work.consume(1)?;
        if left[left_start + after].value != right[right_start + after].value {
            break;
        }
        after += 1;
    }

    Ok((left_start - before, right_start - before, before + after))
}

fn location(file: &str, tokens: &[TokenRecord], start: usize, size: usize) -> Location {
    Location {
        file: file.to_owned(),
        start_line: tokens[start].line,
        end_line: tokens[start + size - 1].line,
        start_token: start,
        end_token: start + size,
    }
}

fn location_key(location: &Location) -> (&str, usize, usize) {
    (location.file.as_str(), location.start_token, location.end_token)
}

fn overlaps(first: &Location, second: &Location) -> bool {
    first.file == second.file
        && !(first.end_token <= second.start_token || second.end_token <= first.start_token)
}

fn contained_by(candidate: &Duplicate, existing: &Duplicate) -> bool {
    let (Some(first), Some(second), Some(outer_first), Some(outer_second)) = (
        candidate.locations.first(),
        candidate.locations.get(1),
        existing.locations.first(),
        existing.locations.get(1),
    ) else {
        return false;
    };

    first.file == outer_first.file
        && second.file == outer_second.file
        && outer_first.start_token <= first.start_token
        && first.end_token <= outer_first.end_token
        && outer_second.start_token <= second.start_token
        && second.end_token <= outer_second.end_token
}

fn compare_duplicates(left: &Duplicate, right: &Duplicate) -> std::cmp::Ordering {
    right
        .token_count
        .cmp(&left.token_count)
        .then_with(|| compare_locations(left.locations.first(), right.locations.first()))
        .then_with(|| compare_locations(left.locations.get(1), right.locations.get(1)))
}

fn compare_locations(left: Option<&Location>, right: Option<&Location>) -> std::cmp::Ordering {
    left.map(location_key).cmp(&right.map(location_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(values: &[&str]) -> Vec<TokenRecord> {
        values
            .iter()
            .enumerate()
            .map(|(index, value)| TokenRecord {
                value: (*value).to_owned(),
                line: u32::try_from(index + 1).unwrap_or(u32::MAX),
                index,
            })
            .collect()
    }

    fn token_map(entries: &[(&str, &[&str])]) -> TokenMap {
        entries
            .iter()
            .map(|(name, values)| ((*name).to_owned(), records(values)))
            .collect()
    }

    fn detected(tokens: &TokenMap, min_tokens: usize, max_groups: usize, limit: usize) -> Vec<Duplicate> {
        match find_duplicates(tokens, min_tokens, max_groups, limit) {
            Ok(duplicates) => duplicates,
            Err(error) => panic!("duplicate detection failed: {error}"),
        }
    }

    fn with_budget(
        tokens: &TokenMap,
        min_tokens: usize,
        max_groups: usize,
        limit: usize,
        budget: DryWorkBudget,
    ) -> Result<Vec<Duplicate>, DryError> {
        find_duplicates_with_budget(tokens, min_tokens, max_groups, limit, budget)
    }

    #[test]
    fn finds_and_maximally_extends_cross_file_clones() {
        let tokens = token_map(&[
            ("a.py", &["left", "a", "b", "c", "d", "e", "tail-a"]),
            ("b.py", &["right", "a", "b", "c", "d", "e", "tail-b"]),
        ]);

        let duplicates = detected(&tokens, 4, 10, 100);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].token_count, 5);
        assert_eq!(duplicates[0].locations[0].start_token, 1);
        assert_eq!(duplicates[0].locations[0].end_token, 6);
        assert_eq!(duplicates[0].locations[0].start_line, 2);
        assert_eq!(duplicates[0].locations[0].end_line, 6);
        assert_eq!(duplicates[0].locations[1].file, "b.py");
    }

    #[test]
    fn verifies_values_when_fingerprints_collide() {
        let tokens = token_map(&[("a.py", &["a", "b", "c", "d"]), ("b.py", &["w", "x", "y", "z"])]);

        let duplicates =
            match find_duplicates_with_token_hasher(&tokens, 4, 10, 100, DryWorkBudget::default(), |_| {
                Fingerprint::ZERO
            }) {
                Ok(duplicates) => duplicates,
                Err(error) => panic!("forced-collision analysis failed: {error}"),
            };

        assert!(duplicates.is_empty());
    }

    #[test]
    fn rolling_fingerprints_match_direct_polynomial_evaluation() {
        let tokens = records(&["alpha", "ID", "+", "NUM", "omega", "return", "ID"]);
        let mut rolling = Vec::new();
        if let Err(error) = for_each_window_fingerprint(&tokens, 4, &stable_token_fingerprint, |_, key| {
            rolling.push(key);
            Ok(())
        }) {
            panic!("rolling fingerprints failed: {error}");
        }

        let direct: Vec<_> = tokens
            .windows(4)
            .map(|window| {
                window.iter().fold(Fingerprint::ZERO, |mut value, token| {
                    let token = stable_token_fingerprint(&token.value);
                    value.first = value
                        .first
                        .wrapping_mul(WINDOW_BASE_FIRST)
                        .wrapping_add(token.first);
                    value.second = value
                        .second
                        .wrapping_mul(WINDOW_BASE_SECOND)
                        .wrapping_add(token.second);
                    value
                })
            })
            .collect();

        assert_eq!(rolling, direct);
    }

    #[test]
    fn rolling_index_hashes_each_token_once_instead_of_each_window() {
        use std::cell::Cell;

        let tokens = token_map(&[(
            "large.py",
            &[
                "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
            ],
        )]);
        let calls = Cell::new(0_usize);
        let result =
            find_duplicates_with_token_hasher(&tokens, 8, 10, 100, DryWorkBudget::default(), |value| {
                calls.set(calls.get().saturating_add(1));
                stable_token_fingerprint(value)
            });

        if let Err(error) = result {
            panic!("rolling-index analysis failed: {error}");
        }
        assert_eq!(calls.get(), 10);
    }

    #[test]
    fn finds_non_overlapping_same_file_clone() {
        let tokens = token_map(&[("a.py", &["a", "b", "c", "d", "separator", "a", "b", "c", "d"])]);

        let duplicates = detected(&tokens, 4, 10, 100);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].locations[0].start_token, 0);
        assert_eq!(duplicates[0].locations[1].start_token, 5);
    }

    #[test]
    fn rejects_overlapping_same_file_clone() {
        let tokens = token_map(&[("a.py", &["x", "x", "x", "x", "x", "x", "x"])]);

        let duplicates = detected(&tokens, 4, 10, 100);

        assert!(duplicates.is_empty());
    }

    #[test]
    fn suppresses_clone_contained_by_selected_clone() {
        let outer = Duplicate {
            token_count: 10,
            locations: vec![
                Location {
                    file: "a.py".to_owned(),
                    start_line: 1,
                    end_line: 10,
                    start_token: 2,
                    end_token: 12,
                },
                Location {
                    file: "b.py".to_owned(),
                    start_line: 1,
                    end_line: 10,
                    start_token: 20,
                    end_token: 30,
                },
            ],
        };
        let inner = Duplicate {
            token_count: 4,
            locations: vec![
                Location {
                    file: "a.py".to_owned(),
                    start_line: 4,
                    end_line: 7,
                    start_token: 5,
                    end_token: 9,
                },
                Location {
                    file: "b.py".to_owned(),
                    start_line: 4,
                    end_line: 7,
                    start_token: 23,
                    end_token: 27,
                },
            ],
        };

        assert!(contained_by(&inner, &outer));
        assert!(!contained_by(&outer, &inner));
    }

    #[test]
    fn output_order_and_group_limit_are_deterministic() {
        let tokens = token_map(&[
            ("a.py", &["a", "b", "c", "d", "e", "f"]),
            ("b.py", &["a", "b", "c", "d", "e", "f"]),
            ("c.py", &["q", "r", "s", "t", "u"]),
            ("d.py", &["q", "r", "s", "t", "u"]),
        ]);

        let first = detected(&tokens, 4, 1, 100);
        let second = detected(&tokens, 4, 1, 100);

        assert_eq!(first, second);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].token_count, 6);
        assert_eq!(first[0].locations[0].file, "a.py");
    }

    #[test]
    fn occurrence_limit_keeps_earliest_deterministic_occurrences() {
        let tokens = token_map(&[
            ("a.py", &["a", "b", "c", "d"]),
            ("b.py", &["a", "b", "c", "d"]),
            ("c.py", &["a", "b", "c", "d"]),
        ]);

        let duplicates = detected(&tokens, 4, 10, 2);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].locations[0].file, "a.py");
        assert_eq!(duplicates[0].locations[1].file, "b.py");
    }

    #[test]
    fn validates_all_limits() {
        let tokens = TokenMap::new();

        assert_eq!(
            find_duplicates(&tokens, 3, 10, 100),
            Err(DryError::MinTokens { provided: 3 })
        );
        assert_eq!(find_duplicates(&tokens, 4, 0, 100), Err(DryError::MaxGroups));
        assert_eq!(
            find_duplicates(&tokens, 4, 10, 1),
            Err(DryError::MaxOccurrencesPerWindow { provided: 1 })
        );
    }

    #[test]
    fn rejects_work_budgets_above_immutable_hard_limits() {
        let tokens = TokenMap::new();
        let budget = DryWorkBudget {
            max_total_windows: DRY_HARD_MAX_TOTAL_WINDOWS.saturating_add(1),
            ..DryWorkBudget::default()
        };

        assert_eq!(
            with_budget(&tokens, 4, 10, 100, budget),
            Err(DryError::InvalidWorkBudget {
                kind: WorkBudgetKind::TotalWindows,
                provided: DRY_HARD_MAX_TOTAL_WINDOWS.saturating_add(1),
                hard_limit: DRY_HARD_MAX_TOTAL_WINDOWS,
            })
        );
    }

    #[test]
    fn total_window_budget_fails_closed_without_partial_results() {
        let tokens = token_map(&[("a.py", &["a", "b", "c", "d", "e"])]);
        let budget = DryWorkBudget {
            max_total_windows: 1,
            ..DryWorkBudget::default()
        };

        assert_eq!(
            with_budget(&tokens, 4, 10, 100, budget),
            Err(DryError::WorkBudgetExceeded {
                kind: WorkBudgetKind::TotalWindows,
                limit: 1,
                required: 2,
            })
        );
    }

    #[test]
    fn fingerprint_bucket_budget_fails_in_deterministic_source_order() {
        let tokens = token_map(&[("a.py", &["a", "b", "c", "d", "e"])]);
        let budget = DryWorkBudget {
            max_fingerprint_buckets: 1,
            ..DryWorkBudget::default()
        };

        assert_eq!(
            with_budget(&tokens, 4, 10, 100, budget),
            Err(DryError::WorkBudgetExceeded {
                kind: WorkBudgetKind::FingerprintBuckets,
                limit: 1,
                required: 2,
            })
        );
    }

    #[test]
    fn forced_collisions_consume_exact_comparison_work() {
        let tokens = token_map(&[("a.py", &["a", "b", "c", "d"]), ("b.py", &["w", "x", "y", "z"])]);
        let budget = DryWorkBudget {
            max_candidate_work: 1,
            ..DryWorkBudget::default()
        };

        let result = find_duplicates_with_token_hasher(&tokens, 4, 10, 100, budget, |_| Fingerprint::ZERO);
        assert_eq!(
            result,
            Err(DryError::WorkBudgetExceeded {
                kind: WorkBudgetKind::CandidateWork,
                limit: 1,
                required: 2,
            })
        );
    }

    #[test]
    fn repetitive_worst_case_hits_candidate_budget_deterministically() {
        let repeated = vec!["x"; 200];
        let tokens = token_map(&[("a.py", repeated.as_slice())]);
        let budget = DryWorkBudget {
            max_candidate_work: 1_000,
            ..DryWorkBudget::default()
        };

        let first = with_budget(&tokens, 4, 10, 100, budget);
        let second = with_budget(&tokens, 4, 10, 100, budget);
        assert_eq!(first, second);
        assert!(matches!(
            first,
            Err(DryError::WorkBudgetExceeded {
                kind: WorkBudgetKind::CandidateWork,
                limit: 1_000,
                ..
            })
        ));
    }

    #[test]
    fn exact_match_extension_is_also_candidate_budgeted() {
        let tokens = token_map(&[
            ("a.py", &["a", "b", "c", "d", "e", "f"]),
            ("b.py", &["a", "b", "c", "d", "e", "f"]),
        ]);
        let budget = DryWorkBudget {
            max_candidate_work: 5,
            ..DryWorkBudget::default()
        };

        assert!(matches!(
            with_budget(&tokens, 4, 10, 100, budget),
            Err(DryError::WorkBudgetExceeded {
                kind: WorkBudgetKind::CandidateWork,
                ..
            })
        ));
    }

    #[test]
    fn serialized_contract_excludes_internal_token_offsets() {
        let duplicate = Duplicate {
            token_count: 4,
            locations: vec![Location {
                file: "src/a.py".to_owned(),
                start_line: 2,
                end_line: 5,
                start_token: 10,
                end_token: 14,
            }],
        };

        let value = match serde_json::to_value(duplicate) {
            Ok(value) => value,
            Err(error) => panic!("serialization failed: {error}"),
        };

        assert_eq!(value["token_count"], 4);
        assert_eq!(value["locations"][0]["file"], "src/a.py");
        assert!(value["locations"][0].get("start_token").is_none());
        assert!(value["locations"][0].get("end_token").is_none());
    }
}
