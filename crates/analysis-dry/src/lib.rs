//! Language-neutral duplicate-code detection over normalized token streams.
//!
//! Language adapters own token normalization. This crate preserves maximally
//! extended exact-region matching and also groups reliable functions through
//! bounded normalized-token shingle/Dice comparison.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::{Display, Formatter, Result as FormatResult};

use reporigor_core::{
    count_as_f64, normalize_repository_path, stable_id, DryConfig, FunctionRecord, TokenRecord,
    DRY_DEFAULT_MAX_CANDIDATE_WORK, DRY_DEFAULT_MAX_FINGERPRINT_BUCKETS, DRY_DEFAULT_MAX_TOTAL_WINDOWS,
    DRY_HARD_MAX_CANDIDATE_WORK, DRY_HARD_MAX_FINGERPRINT_BUCKETS, DRY_HARD_MAX_TOTAL_WINDOWS,
};
use serde::{Deserialize, Serialize};

/// The smallest supported matching window.
pub const MIN_TOKENS: usize = 4;

/// Stable rule identifier used for clone-group identities.
pub const CLONE_RULE_ID: &str = "dry.clone";

/// Algorithm identifier for maximally extended exact token clones.
pub const EXACT_ALGORITHM: &str = "normalized-token-exact-v1";

/// Algorithm identifier for function-level shingle/Dice clone groups.
pub const SHINGLE_DICE_ALGORITHM: &str = "normalized-token-shingle-dice-v1";

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

impl Display for WorkBudgetKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> FormatResult {
        const NAMES: [&str; 3] = ["total windows", "fingerprint buckets", "candidate work"];
        formatter.write_str(NAMES[*self as usize])
    }
}

/// A half-open token range containing one occurrence of a duplicate.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Location {
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Adapter-provided stable symbol for function-level clone occurrences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_symbol: Option<String>,
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

/// One canonical duplicate group.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Duplicate {
    pub token_count: usize,
    pub locations: Vec<Location>,
    /// Stable identity shared by every occurrence in this clone group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone_group_id: Option<String>,
    /// Minimum accepted Sørensen-Dice similarity within the group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f64>,
    /// Minimum recursive AST statement count among grouped functions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement_count: Option<u32>,
    /// Deterministic algorithm that produced the group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<String>,
}

/// Invalid duplicate-analysis limits.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DryError {
    #[error("min_tokens must be at least {MIN_TOKENS}, got {provided}")]
    MinTokens { provided: usize },
    #[error("max_groups must be greater than zero")]
    MaxGroups,
    #[error("max_occurrences_per_window must be at least 2, got {provided}")]
    MaxOccurrencesPerWindow { provided: usize },
    #[error("min_statements must be greater than zero, got {provided}")]
    MinStatements { provided: usize },
    #[error("similarity_threshold must be finite and in (0, 1], got {provided}")]
    SimilarityThreshold { provided: f64 },
    #[error("shingle_tokens must be in 1..={min_tokens}, got {provided}")]
    ShingleTokens { provided: usize, min_tokens: usize },
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

#[derive(Clone, Copy)]
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

    fn append(self, token: Self) -> Self {
        Self {
            first: self
                .first
                .wrapping_mul(WINDOW_BASE_FIRST)
                .wrapping_add(token.first),
            second: self
                .second
                .wrapping_mul(WINDOW_BASE_SECOND)
                .wrapping_add(token.second),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CandidateWork {
    used: usize,
    limit: usize,
}

impl CandidateWork {
    fn consume(&mut self, amount: usize) -> Result<(), DryError> {
        let required = self.used.saturating_add(amount);
        ensure_within_budget(WorkBudgetKind::CandidateWork, self.limit, required)?;
        self.used = required;
        Ok(())
    }
}

const fn candidate_work(limit: usize) -> CandidateWork {
    CandidateWork { used: 0, limit }
}

/// Normalized tokens keyed by root-relative source path.
pub type TokenMap = BTreeMap<String, Vec<TokenRecord>>;

#[derive(Debug, Clone, Copy)]
struct ExactOptions {
    min_tokens: usize,
    max_groups: usize,
    max_occurrences_per_window: usize,
}

const fn exact_options(
    min_tokens: usize,
    max_groups: usize,
    max_occurrences_per_window: usize,
) -> ExactOptions {
    ExactOptions {
        min_tokens,
        max_groups,
        max_occurrences_per_window,
    }
}

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

/// Analyze reliable function records for exact and near-miss clone groups.
///
/// Functions are compared through deterministic normalized-token shingles and
/// multiset Sørensen-Dice similarity. Records without reliable structural
/// metrics are ignored by this entry point; use [`analyze_with_functions`] to
/// retain legacy exact-region detection for those parts of a repository.
///
/// # Errors
///
/// Returns [`DryError`] for invalid thresholds or when a configured work
/// budget is exhausted. No partial groups are returned on budget failure.
pub fn analyze_functions(
    functions: &[reporigor_core::FunctionRecord],
    config: &DryConfig,
) -> Result<Vec<Duplicate>, DryError> {
    Ok(limit_groups(
        checked_function_groups(functions, config)?,
        config.max_groups,
    ))
}

/// Analyze function-level near clones and retain legacy exact-region clones.
///
/// Exact token groups are retained because whole-function Dice comparison
/// cannot soundly supersede repeated blocks within a function or shared blocks
/// inside otherwise dissimilar functions. Exact occurrences are consolidated
/// into canonical multi-occurrence clone groups.
///
/// # Errors
///
/// Returns [`DryError`] for invalid thresholds or when either bounded analysis
/// path exhausts a configured work budget. No partial groups are returned.
pub fn analyze_with_functions(
    tokens: &TokenMap,
    functions: &[FunctionRecord],
    config: &DryConfig,
) -> Result<Vec<Duplicate>, DryError> {
    let mut groups = checked_function_groups(functions, config)?;
    let exact = exact_options(config.min_tokens, usize::MAX, config.max_occurrences_per_window)
        .find_with_hasher(tokens, DryWorkBudget::from(config), stable_token_fingerprint)?;
    groups.extend(exact_fallback_groups(exact, tokens));
    Ok(limit_groups(groups, config.max_groups))
}

fn checked_function_groups(
    functions: &[FunctionRecord],
    config: &DryConfig,
) -> Result<Vec<Duplicate>, DryError> {
    validate_enhanced_limits(config)?;
    find_function_duplicates(functions, config)
}

fn limit_groups(mut groups: Vec<Duplicate>, max_groups: usize) -> Vec<Duplicate> {
    groups.sort_by(compare_duplicates);
    groups.truncate(max_groups);
    groups
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
    let options = exact_options(min_tokens, max_groups, max_occurrences_per_window);
    validate_limits(
        options.min_tokens,
        options.max_groups,
        options.max_occurrences_per_window,
        budget,
    )?;
    options.find_with_hasher(tokens, budget, stable_token_fingerprint)
}

fn validate_limits(
    min_tokens: usize,
    max_groups: usize,
    max_occurrences_per_window: usize,
    budget: DryWorkBudget,
) -> Result<(), DryError> {
    validate_selection_limits(min_tokens, max_groups, max_occurrences_per_window)?;
    for (kind, provided, hard_limit) in budget_limits(budget) {
        validate_budget(kind, BudgetCheck::Configured { provided, hard_limit })?;
    }
    Ok(())
}

fn validate_selection_limits(
    min_tokens: usize,
    max_groups: usize,
    max_occurrences_per_window: usize,
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
    Ok(())
}

const fn budget_limits(budget: DryWorkBudget) -> [(WorkBudgetKind, usize, usize); 3] {
    [
        (
            WorkBudgetKind::TotalWindows,
            budget.max_total_windows,
            DRY_HARD_MAX_TOTAL_WINDOWS,
        ),
        (
            WorkBudgetKind::FingerprintBuckets,
            budget.max_fingerprint_buckets,
            DRY_HARD_MAX_FINGERPRINT_BUCKETS,
        ),
        (
            WorkBudgetKind::CandidateWork,
            budget.max_candidate_work,
            DRY_HARD_MAX_CANDIDATE_WORK,
        ),
    ]
}

fn validate_enhanced_limits(config: &DryConfig) -> Result<(), DryError> {
    validate_limits(
        config.min_tokens,
        config.max_groups,
        config.max_occurrences_per_window,
        DryWorkBudget::from(config),
    )?;
    validate_statement_limit(config.min_statements)?;
    validate_similarity(config.similarity_threshold)?;
    validate_shingle_size(config.shingle_tokens, config.min_tokens)
}

fn validate_statement_limit(min_statements: usize) -> Result<(), DryError> {
    if min_statements == 0 {
        return Err(DryError::MinStatements {
            provided: min_statements,
        });
    }
    Ok(())
}

fn validate_similarity(similarity: f64) -> Result<(), DryError> {
    if !similarity.is_finite() || similarity <= 0.0 || similarity > 1.0 {
        return Err(DryError::SimilarityThreshold { provided: similarity });
    }
    Ok(())
}

fn validate_shingle_size(shingle_tokens: usize, min_tokens: usize) -> Result<(), DryError> {
    if shingle_tokens == 0 || shingle_tokens > min_tokens {
        return Err(DryError::ShingleTokens {
            provided: shingle_tokens,
            min_tokens,
        });
    }
    Ok(())
}

fn ensure_within_budget(kind: WorkBudgetKind, limit: usize, required: usize) -> Result<(), DryError> {
    validate_budget(kind, BudgetCheck::Consumed { limit, required })
}

#[derive(Debug, Clone, Copy)]
enum BudgetCheck {
    Configured { provided: usize, hard_limit: usize },
    Consumed { limit: usize, required: usize },
}

fn validate_budget(kind: WorkBudgetKind, check: BudgetCheck) -> Result<(), DryError> {
    match check {
        BudgetCheck::Configured { provided, hard_limit } if provided == 0 || provided > hard_limit => {
            Err(DryError::InvalidWorkBudget {
                kind,
                provided,
                hard_limit,
            })
        }
        BudgetCheck::Consumed { limit, required } if required > limit => Err(DryError::WorkBudgetExceeded {
            kind,
            limit,
            required,
        }),
        _ => Ok(()),
    }
}

impl ExactOptions {
    fn find_with_hasher<F>(
        self,
        token_map: &TokenMap,
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
            self.min_tokens,
            self.max_occurrences_per_window,
            budget,
            &token_hasher,
        )?;
        let candidates = build_candidates(
            &files,
            &token_sets,
            &windows,
            self.min_tokens,
            budget.max_candidate_work,
        )?;
        Ok(select_candidates(candidates, self.max_groups))
    }
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
    let initial_capacity = fingerprint_index_capacity(required_windows, budget)?;
    // Avoid eagerly reserving the entire repository-controlled allowance.
    // The map still grows to the configured, immutable-capped limit as needed.
    let mut windows: HashMap<Fingerprint, Vec<Occurrence>> = HashMap::with_capacity(initial_capacity);

    for (file_index, tokens) in token_sets.iter().enumerate() {
        for_each_fingerprinted_window(
            tokens,
            min_tokens,
            |token| token_hasher(&token.value),
            |start, key| {
                let bucket_count = windows.len();
                let occurrences = match windows.entry(key) {
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let required = bucket_count.saturating_add(1);
                        ensure_within_budget(
                            WorkBudgetKind::FingerprintBuckets,
                            budget.max_fingerprint_buckets,
                            required,
                        )?;
                        entry.insert(Vec::new())
                    }
                };
                if occurrences.len() < max_occurrences_per_window {
                    occurrences.push(Occurrence { file_index, start });
                }
                Ok(())
            },
        )?;
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
    CandidateBuilder::new(files, token_sets, min_tokens, max_candidate_work).build(windows)
}

type Diagonal = (usize, usize, bool, usize);
type ExactCandidateKey = (String, usize, String, usize, usize);

struct CandidateBuilder<'a> {
    files: &'a [String],
    token_sets: &'a [&'a [TokenRecord]],
    min_tokens: usize,
    candidates: BTreeMap<ExactCandidateKey, Duplicate>,
    extended_regions: BTreeMap<Diagonal, Vec<(usize, usize)>>,
    work: CandidateWork,
}

impl<'a> CandidateBuilder<'a> {
    fn new(
        files: &'a [String],
        token_sets: &'a [&'a [TokenRecord]],
        min_tokens: usize,
        max_candidate_work: usize,
    ) -> Self {
        Self {
            files,
            token_sets,
            min_tokens,
            candidates: BTreeMap::new(),
            extended_regions: BTreeMap::new(),
            work: candidate_work(max_candidate_work),
        }
    }

    fn build(mut self, windows: &HashMap<Fingerprint, Vec<Occurrence>>) -> Result<Vec<Duplicate>, DryError> {
        let mut buckets: Vec<_> = windows
            .iter()
            .filter(|(_, occurrences)| occurrences.len() > 1)
            .collect();
        buckets.sort_unstable_by_key(|(fingerprint, _)| **fingerprint);
        for (_, occurrences) in buckets {
            self.process_bucket(occurrences)?;
        }
        let mut candidates: Vec<_> = self.candidates.into_values().collect();
        candidates.sort_by(compare_duplicates);
        Ok(candidates)
    }

    fn process_bucket(&mut self, occurrences: &[Occurrence]) -> Result<(), DryError> {
        for left_index in 0..occurrences.len() {
            for right_index in left_index + 1..occurrences.len() {
                self.process_pair(occurrences[left_index], occurrences[right_index])?;
            }
        }
        Ok(())
    }

    fn process_pair(&mut self, left: Occurrence, right: Occurrence) -> Result<(), DryError> {
        // Charge dispatch even when a forced fingerprint collision fails the
        // exact comparison, so collisions cannot bypass the work budget.
        self.work.consume(1)?;
        let diagonal = diagonal_key(left, right);
        if self.already_extended(diagonal, left.start) {
            return Ok(());
        }
        let left_tokens = self.token_sets[left.file_index];
        let right_tokens = self.token_sets[right.file_index];
        let pair = token_pair(left_tokens, left.start, right_tokens, right.start);
        if !pair.windows_equal(self.min_tokens, &mut self.work)? {
            return Ok(());
        }
        let region = self.extend_region(diagonal, left, right)?;
        self.insert_region(left, right, region);
        Ok(())
    }

    fn already_extended(&self, diagonal: Diagonal, start: usize) -> bool {
        self.extended_regions.get(&diagonal).is_some_and(|regions| {
            regions
                .iter()
                .any(|(first, last)| *first <= start && start <= *last)
        })
    }

    fn extend_region(
        &mut self,
        diagonal: Diagonal,
        left: Occurrence,
        right: Occurrence,
    ) -> Result<(usize, usize, usize), DryError> {
        let pair = token_pair(
            self.token_sets[left.file_index],
            left.start,
            self.token_sets[right.file_index],
            right.start,
        );
        let region = extend_match(pair, self.min_tokens, &mut self.work)?;
        let last_window = region.0.saturating_add(region.2.saturating_sub(self.min_tokens));
        remember_extended_region(
            self.extended_regions.entry(diagonal).or_default(),
            region.0,
            last_window,
        );
        Ok(region)
    }

    fn insert_region(&mut self, left: Occurrence, right: Occurrence, region: (usize, usize, usize)) {
        let (left_start, right_start, size) = region;
        let left_tokens = self.token_sets[left.file_index];
        let right_tokens = self.token_sets[right.file_index];
        let first = location(&self.files[left.file_index], left_tokens, left_start, size);
        let second = location(&self.files[right.file_index], right_tokens, right_start, size);
        let Some((first, second)) = ordered_disjoint_locations(first, second) else {
            return;
        };
        let evidence = normalized_evidence(
            left_tokens[left_start..left_start + size]
                .iter()
                .map(|token| token.value.as_str()),
        );
        let clone_group_id = exact_clone_group_id(&[first.clone(), second.clone()], &evidence);
        let key = exact_candidate_key(&first, &second, size);
        self.candidates
            .entry(key)
            .or_insert_with(|| exact_duplicate(size, vec![first, second], clone_group_id));
    }
}

fn ordered_disjoint_locations(mut first: Location, mut second: Location) -> Option<(Location, Location)> {
    if overlaps(&first, &second) {
        return None;
    }
    if location_key(&second) < location_key(&first) {
        std::mem::swap(&mut first, &mut second);
    }
    Some((first, second))
}

fn exact_candidate_key(first: &Location, second: &Location, size: usize) -> ExactCandidateKey {
    (
        first.file.clone(),
        first.start_token,
        second.file.clone(),
        second.start_token,
        size,
    )
}

fn exact_clone_group_id(locations: &[Location], evidence: &str) -> String {
    stable_clone_group_id(
        locations
            .iter()
            .map(|location| CloneIdentity {
                path: canonical_path(&location.file),
                symbol: location
                    .stable_symbol
                    .clone()
                    .unwrap_or_else(|| "<file-region>".to_owned()),
                evidence: evidence.to_owned(),
            })
            .collect(),
    )
}

fn exact_duplicate(token_count: usize, locations: Vec<Location>, clone_group_id: String) -> Duplicate {
    Duplicate {
        token_count,
        locations,
        clone_group_id: Some(clone_group_id),
        similarity: Some(1.0),
        statement_count: None,
        algorithm: Some(EXACT_ALGORITHM.to_owned()),
    }
}

fn diagonal_key(left: Occurrence, right: Occurrence) -> (usize, usize, bool, usize) {
    let (forward, distance) = if left.start <= right.start {
        (true, right.start - left.start)
    } else {
        (false, left.start - right.start)
    };
    (left.file_index, right.file_index, forward, distance)
}

fn remember_extended_region(regions: &mut Vec<(usize, usize)>, mut start: usize, mut end: usize) {
    let mut retained = Vec::with_capacity(regions.len().saturating_add(1));
    for (existing_start, existing_end) in regions.drain(..) {
        if existing_end.saturating_add(1) < start || end.saturating_add(1) < existing_start {
            retained.push((existing_start, existing_end));
        } else {
            start = start.min(existing_start);
            end = end.max(existing_end);
        }
    }
    retained.push((start, end));
    retained.sort_unstable();
    *regions = retained;
}

fn select_candidates(candidates: Vec<Duplicate>, max_groups: usize) -> Vec<Duplicate> {
    select_non_redundant(candidates, max_groups, contained_by)
}

fn select_non_redundant<F>(groups: Vec<Duplicate>, limit: usize, redundant: F) -> Vec<Duplicate>
where
    F: Fn(&Duplicate, &Duplicate) -> bool,
{
    let mut selected = Vec::new();
    for group in groups {
        if selected.iter().any(|existing| redundant(&group, existing)) {
            continue;
        }
        selected.push(group);
        if selected.len() == limit {
            break;
        }
    }
    selected
}

#[derive(Debug)]
struct EligibleFunction<'a> {
    record: &'a FunctionRecord,
    path: String,
    symbol: String,
    evidence: String,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ShingleIdentity {
    fingerprint: Fingerprint,
    variant: usize,
}

#[derive(Debug, Clone, Copy)]
struct FunctionShingle {
    function_index: usize,
    start: usize,
}

#[derive(Debug)]
struct ShingleBucket {
    representative: FunctionShingle,
    functions: Vec<usize>,
}

type FunctionShingleSets = Vec<Vec<ShingleIdentity>>;
type FunctionShingleIndex = HashMap<Fingerprint, Vec<ShingleBucket>>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CloneIdentity {
    path: String,
    symbol: String,
    evidence: String,
}

#[derive(Debug)]
struct DisjointSet {
    parents: Vec<usize>,
}

impl DisjointSet {
    fn new(size: usize) -> Self {
        Self {
            parents: (0..size).collect(),
        }
    }

    fn root(&mut self, node: usize) -> usize {
        let mut root = node;
        while self.parents[root] != root {
            root = self.parents[root];
        }
        let mut current = node;
        while self.parents[current] != current {
            let parent = self.parents[current];
            self.parents[current] = root;
            current = parent;
        }
        root
    }

    fn union(&mut self, left: usize, right: usize) {
        let left_root = self.root(left);
        let right_root = self.root(right);
        if left_root == right_root {
            return;
        }
        let (root, child) = if left_root < right_root {
            (left_root, right_root)
        } else {
            (right_root, left_root)
        };
        self.parents[child] = root;
    }
}

fn find_function_duplicates(
    functions: &[FunctionRecord],
    config: &DryConfig,
) -> Result<Vec<Duplicate>, DryError> {
    let eligible = eligible_functions(functions, config);
    let budget = DryWorkBudget::from(config);
    let mut candidate_work = candidate_work(budget.max_candidate_work);
    let (shingles, index) = build_function_shingle_index(
        &eligible,
        config.shingle_tokens,
        config.max_occurrences_per_window,
        budget,
        &mut candidate_work,
    )?;
    let candidates = function_candidate_pairs(
        &index,
        &shingles,
        config.similarity_threshold,
        &mut candidate_work,
    )?;

    let (mut components, accepted) = accept_function_pairs(
        eligible.len(),
        candidates,
        &shingles,
        config.similarity_threshold,
        &mut candidate_work,
    )?;
    let members = component_members(eligible.len(), &mut components);
    let similarities = component_similarities(accepted, &mut components);
    Ok(function_groups(&eligible, members, &similarities))
}

type AcceptedPair = (usize, usize, f64);

fn accept_function_pairs(
    function_count: usize,
    candidates: BTreeSet<(usize, usize)>,
    shingles: &[Vec<ShingleIdentity>],
    threshold: f64,
    work: &mut CandidateWork,
) -> Result<(DisjointSet, Vec<AcceptedPair>), DryError> {
    let mut components = DisjointSet::new(function_count);
    let mut accepted = Vec::new();
    for (left, right) in candidates {
        let similarity = dice_similarity(&shingles[left], &shingles[right], threshold, work)?;
        if similarity >= threshold {
            components.union(left, right);
            accepted.push((left, right, similarity));
        }
    }
    Ok((components, accepted))
}

fn component_members(function_count: usize, components: &mut DisjointSet) -> BTreeMap<usize, Vec<usize>> {
    let mut members = BTreeMap::<usize, Vec<usize>>::new();
    for index in 0..function_count {
        let root = components.root(index);
        members.entry(root).or_default().push(index);
    }
    members
}

fn component_similarities(accepted: Vec<AcceptedPair>, components: &mut DisjointSet) -> BTreeMap<usize, f64> {
    let mut component_similarity = BTreeMap::<usize, f64>::new();
    for (left, _, similarity) in accepted {
        let root = components.root(left);
        component_similarity
            .entry(root)
            .and_modify(|minimum| *minimum = minimum.min(similarity))
            .or_insert(similarity);
    }
    component_similarity
}

fn function_groups(
    eligible: &[EligibleFunction<'_>],
    members: BTreeMap<usize, Vec<usize>>,
    similarities: &BTreeMap<usize, f64>,
) -> Vec<Duplicate> {
    let mut groups = Vec::new();
    for (root, member_indices) in members {
        if member_indices.len() < 2 {
            continue;
        }
        let Some(similarity) = similarities.get(&root).copied() else {
            continue;
        };
        groups.push(function_group(eligible, &member_indices, similarity));
    }
    groups.sort_by(compare_duplicates);
    select_maximal_exact_groups(groups)
}

fn select_maximal_exact_groups(groups: Vec<Duplicate>) -> Vec<Duplicate> {
    select_non_redundant(groups, usize::MAX, exact_group_contained_by)
}

fn exact_group_contained_by(candidate: &Duplicate, existing: &Duplicate) -> bool {
    candidate.token_count < existing.token_count
        && candidate.locations.iter().all(|inner| {
            existing
                .locations
                .iter()
                .any(|outer| location_contains(outer, inner))
        })
}

fn function_group(eligible: &[EligibleFunction<'_>], members: &[usize], similarity: f64) -> Duplicate {
    let token_count = members
        .iter()
        .map(|index| eligible[*index].record.normalized_tokens.len())
        .min()
        .unwrap_or(0);
    let statement_count = members
        .iter()
        .map(|index| eligible[*index].record.statement_count)
        .min();
    let mut locations: Vec<_> = members
        .iter()
        .map(|index| function_location(&eligible[*index]))
        .collect();
    locations.sort_by(compare_location_output);
    let identities = members
        .iter()
        .map(|index| CloneIdentity {
            path: eligible[*index].path.clone(),
            symbol: eligible[*index].symbol.clone(),
            evidence: eligible[*index].evidence.clone(),
        })
        .collect();
    Duplicate {
        token_count,
        locations,
        clone_group_id: Some(stable_clone_group_id(identities)),
        similarity: Some(similarity),
        statement_count,
        algorithm: Some(SHINGLE_DICE_ALGORITHM.to_owned()),
    }
}

fn eligible_functions<'a>(functions: &'a [FunctionRecord], config: &DryConfig) -> Vec<EligibleFunction<'a>> {
    let mut eligible: Vec<_> = functions
        .iter()
        .filter(|function| {
            reliable_function(function)
                && function.normalized_tokens.len() >= config.min_tokens
                && usize::try_from(function.statement_count).unwrap_or(usize::MAX) >= config.min_statements
        })
        .map(|record| EligibleFunction {
            record,
            path: canonical_path(&record.file),
            symbol: effective_symbol(record),
            evidence: normalized_evidence(record.normalized_tokens.iter().map(String::as_str)),
        })
        .collect();
    eligible.sort_by(|left, right| {
        (&left.path, &left.symbol, &left.evidence)
            .cmp(&(&right.path, &right.symbol, &right.evidence))
            .then_with(|| left.record.start_line.cmp(&right.record.start_line))
            .then_with(|| left.record.end_line.cmp(&right.record.end_line))
    });
    let mut physical_functions = BTreeSet::new();
    eligible.retain(|function| {
        physical_functions.insert((
            function.path.clone(),
            function.record.start_line,
            function.record.end_line,
            function.evidence.clone(),
        ))
    });
    eligible
}

fn reliable_function(function: &FunctionRecord) -> bool {
    function.structural_metrics_reliable
        && !function.normalized_tokens.is_empty()
        && function.start_line > 0
        && function.end_line >= function.start_line
}

fn effective_symbol(function: &FunctionRecord) -> String {
    if function.stable_symbol.is_empty() {
        function.name.clone()
    } else {
        function.stable_symbol.clone()
    }
}

fn function_location(function: &EligibleFunction<'_>) -> Location {
    Location {
        file: function.record.file.clone(),
        start_line: function.record.start_line,
        end_line: function.record.end_line,
        stable_symbol: Some(function.symbol.clone()),
        start_token: 0,
        end_token: function.record.normalized_tokens.len(),
    }
}

fn build_function_shingle_index(
    functions: &[EligibleFunction<'_>],
    shingle_tokens: usize,
    max_occurrences_per_window: usize,
    budget: DryWorkBudget,
    candidate_work: &mut CandidateWork,
) -> Result<(FunctionShingleSets, FunctionShingleIndex), DryError> {
    let required_windows = functions.iter().fold(0_usize, |total, function| {
        total.saturating_add(
            function
                .record
                .normalized_tokens
                .len()
                .saturating_sub(shingle_tokens)
                .saturating_add(1),
        )
    });
    let initial_capacity = fingerprint_index_capacity(required_windows, budget)?;
    let mut index = HashMap::<Fingerprint, Vec<ShingleBucket>>::with_capacity(initial_capacity);
    let mut function_shingles = vec![Vec::new(); functions.len()];
    let mut exact_bucket_count = 0_usize;
    let mut comparator = FunctionShingleComparator {
        functions,
        work: candidate_work,
    };

    for (function_index, function) in functions.iter().enumerate() {
        let values = &function.record.normalized_tokens;
        for_each_string_window_fingerprint(values, shingle_tokens, |start, fingerprint| {
            let variants = index.entry(fingerprint).or_default();
            let mut variant = None;
            for (candidate_variant, bucket) in variants.iter().enumerate() {
                if comparator.equal(
                    bucket.representative,
                    FunctionShingle {
                        function_index,
                        start,
                    },
                    shingle_tokens,
                )? {
                    variant = Some(candidate_variant);
                    break;
                }
            }
            let variant = if let Some(variant) = variant {
                variant
            } else {
                let required = exact_bucket_count.saturating_add(1);
                ensure_within_budget(
                    WorkBudgetKind::FingerprintBuckets,
                    budget.max_fingerprint_buckets,
                    required,
                )?;
                exact_bucket_count = required;
                variants.push(ShingleBucket {
                    representative: FunctionShingle {
                        function_index,
                        start,
                    },
                    functions: Vec::new(),
                });
                variants.len() - 1
            };
            let bucket = &mut variants[variant];
            if bucket.functions.last().copied() != Some(function_index)
                && bucket.functions.len() < max_occurrences_per_window
            {
                bucket.functions.push(function_index);
            }
            function_shingles[function_index].push(ShingleIdentity { fingerprint, variant });
            Ok(())
        })?;
        function_shingles[function_index].sort_unstable();
    }
    Ok((function_shingles, index))
}

struct FunctionShingleComparator<'slice, 'record, 'work> {
    functions: &'slice [EligibleFunction<'record>],
    work: &'work mut CandidateWork,
}

impl FunctionShingleComparator<'_, '_, '_> {
    fn equal(
        &mut self,
        left: FunctionShingle,
        right: FunctionShingle,
        size: usize,
    ) -> Result<bool, DryError> {
        let left_tokens = &self.functions[left.function_index].record.normalized_tokens;
        let right_tokens = &self.functions[right.function_index].record.normalized_tokens;
        let Some((left_window, right_window)) = left_tokens
            .get(left.start..left.start + size)
            .zip(right_tokens.get(right.start..right.start + size))
        else {
            return Ok(false);
        };
        sequences_equal(left_window, right_window, self.work, PartialEq::eq)
    }
}

fn sequences_equal<T, U, F>(
    left: &[T],
    right: &[U],
    work: &mut CandidateWork,
    equal: F,
) -> Result<bool, DryError>
where
    F: Fn(&T, &U) -> bool,
{
    for (first, second) in left.iter().zip(right) {
        work.consume(1)?;
        if !equal(first, second) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn function_candidate_pairs(
    index: &HashMap<Fingerprint, Vec<ShingleBucket>>,
    shingles: &[Vec<ShingleIdentity>],
    threshold: f64,
    work: &mut CandidateWork,
) -> Result<BTreeSet<(usize, usize)>, DryError> {
    let mut fingerprints: Vec<_> = index.iter().collect();
    fingerprints.sort_unstable_by_key(|(fingerprint, _)| **fingerprint);
    let mut collector = PairCollector {
        shingles,
        threshold,
        work,
        pairs: BTreeSet::new(),
    };
    for (_, variants) in fingerprints {
        for bucket in variants {
            collector.add_bucket(bucket)?;
        }
    }
    Ok(collector.pairs)
}

struct PairCollector<'a> {
    shingles: &'a [Vec<ShingleIdentity>],
    threshold: f64,
    work: &'a mut CandidateWork,
    pairs: BTreeSet<(usize, usize)>,
}

impl PairCollector<'_> {
    fn add_bucket(&mut self, bucket: &ShingleBucket) -> Result<(), DryError> {
        for left in 0..bucket.functions.len() {
            for right in left + 1..bucket.functions.len() {
                let pair = (bucket.functions[left], bucket.functions[right]);
                if can_reach_dice(
                    self.shingles[pair.0].len(),
                    self.shingles[pair.1].len(),
                    self.threshold,
                ) && self.pairs.insert(pair)
                {
                    // A pair that shares many shingles is one candidate. Charge
                    // it once rather than once per shared shingle.
                    self.work.consume(1)?;
                }
            }
        }
        Ok(())
    }
}

fn dice_similarity(
    left: &[ShingleIdentity],
    right: &[ShingleIdentity],
    threshold: f64,
    work: &mut CandidateWork,
) -> Result<f64, DryError> {
    let mut left_index = 0;
    let mut right_index = 0;
    let mut shared = 0_usize;
    let total = left.len().saturating_add(right.len());
    while left_index < left.len() && right_index < right.len() {
        if !remaining_can_reach_threshold(
            (left.len(), left_index),
            (right.len(), right_index),
            shared,
            total,
            threshold,
        ) {
            return Ok(0.0);
        }
        work.consume(1)?;
        advance_dice_indices(
            left[left_index].cmp(&right[right_index]),
            &mut left_index,
            &mut right_index,
            &mut shared,
        );
    }
    if total == 0 {
        return Ok(0.0);
    }
    let shared = u32::try_from(shared).unwrap_or(u32::MAX);
    let total = u32::try_from(total).unwrap_or(u32::MAX);
    Ok(2.0 * f64::from(shared) / f64::from(total))
}

fn remaining_can_reach_threshold(
    left: (usize, usize),
    right: (usize, usize),
    shared: usize,
    total: usize,
    threshold: f64,
) -> bool {
    let maximum_shared =
        shared.saturating_add(left.0.saturating_sub(left.1).min(right.0.saturating_sub(right.1)));
    2.0 * count_as_f64(maximum_shared) / count_as_f64(total) >= threshold
}

fn advance_dice_indices(
    ordering: std::cmp::Ordering,
    left: &mut usize,
    right: &mut usize,
    shared: &mut usize,
) {
    match ordering {
        std::cmp::Ordering::Less => *left += 1,
        std::cmp::Ordering::Greater => *right += 1,
        std::cmp::Ordering::Equal => {
            *shared += 1;
            *left += 1;
            *right += 1;
        }
    }
}

fn can_reach_dice(left: usize, right: usize, threshold: f64) -> bool {
    let total = left.saturating_add(right);
    if total == 0 {
        return false;
    }
    2.0 * count_as_f64(left.min(right)) / count_as_f64(total) >= threshold
}

fn for_each_string_window_fingerprint<A>(tokens: &[String], size: usize, accept: A) -> Result<(), DryError>
where
    A: FnMut(usize, Fingerprint) -> Result<(), DryError>,
{
    for_each_fingerprinted_window(tokens, size, |token| stable_token_fingerprint(token), accept)
}

#[derive(Debug)]
struct ExactGroup {
    token_count: usize,
    evidence: String,
    locations: Vec<Location>,
}

fn exact_fallback_groups(exact: Vec<Duplicate>, tokens: &TokenMap) -> Vec<Duplicate> {
    let mut grouped = BTreeMap::<(usize, String), ExactGroup>::new();
    for duplicate in exact {
        let Some(first) = duplicate.locations.first() else {
            continue;
        };
        let Some(evidence) = exact_location_evidence(tokens, first) else {
            continue;
        };
        let key = (duplicate.token_count, evidence.clone());
        let group = grouped.entry(key).or_insert_with(|| ExactGroup {
            token_count: duplicate.token_count,
            evidence,
            locations: Vec::new(),
        });
        group.locations.extend(duplicate.locations);
    }

    let mut groups = Vec::new();
    for (_, mut group) in grouped {
        group.locations.sort_by(compare_location_output);
        group.locations.dedup_by(|left, right| {
            left.file == right.file
                && left.start_token == right.start_token
                && left.end_token == right.end_token
        });
        if group.locations.len() < 2 {
            continue;
        }
        let clone_group_id = exact_clone_group_id(&group.locations, &group.evidence);
        groups.push(exact_duplicate(
            group.token_count,
            group.locations,
            clone_group_id,
        ));
    }
    groups.sort_by(compare_duplicates);
    select_independent_exact_groups(groups)
}

/// Keep maximal exact groups from spending the report budget on shifted
/// variants of regions that an earlier, larger group already represents.
///
/// A later group is independently reportable only when at least two of its
/// occurrences have reportable line spans disjoint from every occurrence
/// already retained in the same file. Exact token variants on the same source
/// lines are indistinguishable in the public location contract, so they cannot
/// consume separate report slots. One new occurrence plus an overlapping
/// occurrence is evidence for the already reported clone family, not a second
/// independent family. Groups are considered in canonical longest-first order.
fn select_independent_exact_groups(mut groups: Vec<Duplicate>) -> Vec<Duplicate> {
    groups.sort_by(compare_duplicates);
    let mut covered = BTreeMap::<String, Vec<(u32, u32)>>::new();
    let mut selected = Vec::new();
    for group in groups {
        if independent_occurrence_count(&group, &covered) < 2 {
            continue;
        }
        remember_group_occurrences(&group, &mut covered);
        selected.push(group);
    }
    selected
}

fn independent_occurrence_count(group: &Duplicate, covered: &BTreeMap<String, Vec<(u32, u32)>>) -> usize {
    group
        .locations
        .iter()
        .filter(|location| {
            covered.get(&location.file).is_none_or(|intervals| {
                intervals
                    .iter()
                    .all(|(start, end)| location.end_line < *start || *end < location.start_line)
            })
        })
        .count()
}

fn remember_group_occurrences(group: &Duplicate, covered: &mut BTreeMap<String, Vec<(u32, u32)>>) {
    for location in &group.locations {
        covered
            .entry(location.file.clone())
            .or_default()
            .push((location.start_line, location.end_line));
    }
}

fn exact_location_evidence(tokens: &TokenMap, location: &Location) -> Option<String> {
    let file_tokens = tokens.get(&location.file)?;
    let values = file_tokens.get(location.start_token..location.end_token)?;
    Some(normalized_evidence(
        values.iter().map(|token| token.value.as_str()),
    ))
}

fn normalized_evidence<'a>(tokens: impl IntoIterator<Item = &'a str>) -> String {
    encoded_components(tokens)
}

fn stable_clone_group_id(mut identities: Vec<CloneIdentity>) -> String {
    identities.sort();
    let paths = encoded_components(identities.iter().map(|identity| identity.path.as_str()));
    let symbols = encoded_components(identities.iter().map(|identity| identity.symbol.as_str()));
    let evidence = encoded_components(identities.iter().map(|identity| identity.evidence.as_str()));
    stable_id(CLONE_RULE_ID, &paths, &symbols, &evidence)
}

fn encoded_components<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    let mut encoded = String::new();
    for value in values {
        encoded.push_str(&value.len().to_string());
        encoded.push(':');
        encoded.push_str(value);
    }
    encoded
}

fn canonical_path(path: &str) -> String {
    normalize_repository_path(path).unwrap_or_else(|_| path.replace('\\', "/"))
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

fn fingerprint_capacity(required_windows: usize, bucket_limit: usize) -> usize {
    required_windows.min(bucket_limit).min(16_384)
}

fn fingerprint_index_capacity(required_windows: usize, budget: DryWorkBudget) -> Result<usize, DryError> {
    ensure_within_budget(
        WorkBudgetKind::TotalWindows,
        budget.max_total_windows,
        required_windows,
    )?;
    Ok(fingerprint_capacity(
        required_windows,
        budget.max_fingerprint_buckets,
    ))
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

fn for_each_fingerprinted_window<T, F, A>(
    tokens: &[T],
    size: usize,
    token_hasher: F,
    mut accept: A,
) -> Result<(), DryError>
where
    F: Fn(&T) -> Fingerprint,
    A: FnMut(usize, Fingerprint) -> Result<(), DryError>,
{
    if tokens.len() < size {
        return Ok(());
    }

    // Each token value is read and fingerprinted exactly once. Subsequent
    // window fingerprints are O(1) rolling updates.
    let token_fingerprints: Vec<_> = tokens.iter().map(token_hasher).collect();
    let first_power = wrapping_pow(WINDOW_BASE_FIRST, size - 1);
    let second_power = wrapping_pow(WINDOW_BASE_SECOND, size - 1);
    let mut window = Fingerprint::ZERO;
    for token in &token_fingerprints[..size] {
        window = window.append(*token);
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

#[derive(Debug, Clone, Copy)]
struct TokenPair<'a> {
    left: &'a [TokenRecord],
    left_start: usize,
    right: &'a [TokenRecord],
    right_start: usize,
}

const fn token_pair<'a>(
    left: &'a [TokenRecord],
    left_start: usize,
    right: &'a [TokenRecord],
    right_start: usize,
) -> TokenPair<'a> {
    TokenPair {
        left,
        left_start,
        right,
        right_start,
    }
}

impl TokenPair<'_> {
    fn windows_equal(self, size: usize, work: &mut CandidateWork) -> Result<bool, DryError> {
        let Some((left_window, right_window)) = self
            .left
            .get(self.left_start..self.left_start + size)
            .zip(self.right.get(self.right_start..self.right_start + size))
        else {
            return Ok(false);
        };
        sequences_equal(left_window, right_window, work, |first, second| {
            first.value == second.value
        })
    }

    fn matching_before(self, work: &mut CandidateWork) -> Result<usize, DryError> {
        let mut before = 0;
        while self.left_start > before && self.right_start > before {
            work.consume(1)?;
            if self.left[self.left_start - before - 1].value
                != self.right[self.right_start - before - 1].value
            {
                break;
            }
            before += 1;
        }
        Ok(before)
    }

    fn matching_after(self, minimum: usize, work: &mut CandidateWork) -> Result<usize, DryError> {
        let mut after = minimum;
        while self.left_start + after < self.left.len() && self.right_start + after < self.right.len() {
            work.consume(1)?;
            if self.left[self.left_start + after].value != self.right[self.right_start + after].value {
                break;
            }
            after += 1;
        }
        Ok(after)
    }
}

fn extend_match(
    pair: TokenPair<'_>,
    minimum: usize,
    work: &mut CandidateWork,
) -> Result<(usize, usize, usize), DryError> {
    let before = pair.matching_before(work)?;
    let after = pair.matching_after(minimum, work)?;
    Ok((
        pair.left_start - before,
        pair.right_start - before,
        before + after,
    ))
}

fn location(file: &str, tokens: &[TokenRecord], start: usize, size: usize) -> Location {
    Location {
        file: file.to_owned(),
        start_line: tokens[start].line,
        end_line: tokens[start + size - 1].line,
        stable_symbol: None,
        start_token: start,
        end_token: start + size,
    }
}

fn location_key(location: &Location) -> (&str, usize, usize) {
    (location.file.as_str(), location.start_token, location.end_token)
}

fn overlaps(first: &Location, second: &Location) -> bool {
    same_file(first, second)
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

    location_contains(outer_first, first) && location_contains(outer_second, second)
}

fn location_contains(outer: &Location, inner: &Location) -> bool {
    same_file(outer, inner) && outer.start_token <= inner.start_token && inner.end_token <= outer.end_token
}

fn same_file(first: &Location, second: &Location) -> bool {
    first.file == second.file
}

fn compare_duplicates(left: &Duplicate, right: &Duplicate) -> std::cmp::Ordering {
    let mut ordering = right.token_count.cmp(&left.token_count);
    for (left_location, right_location) in left.locations.iter().zip(&right.locations) {
        ordering = ordering.then_with(|| compare_location_output(left_location, right_location));
    }
    ordering
        .then_with(|| left.locations.len().cmp(&right.locations.len()))
        .then_with(|| left.clone_group_id.cmp(&right.clone_group_id))
}

fn compare_location_output(left: &Location, right: &Location) -> std::cmp::Ordering {
    location_output_key(left).cmp(&location_output_key(right))
}

fn location_output_key(location: &Location) -> (String, &Option<String>, u32, u32, usize, usize) {
    (
        canonical_path(&location.file),
        &location.stable_symbol,
        location.start_line,
        location.end_line,
        location.start_token,
        location.end_token,
    )
}

#[cfg(test)]
mod tests {
    use reporigor_core::{Language, SymbolVisibility};

    use super::*;

    struct TestSuite;
    type TestCase = fn();

    fn records(values: &[&str]) -> Vec<TokenRecord> {
        records_with_line_offset(values, 0)
    }

    fn token_map(entries: &[(&str, &[&str])]) -> TokenMap {
        collect_token_map(entries, |(name, values)| ((*name).to_owned(), records(values)))
    }

    fn encoded_values(encoded: &str) -> Vec<&str> {
        encoded.split('|').collect()
    }

    fn encoded_token_map(encoded: &str) -> TokenMap {
        encoded
            .split(';')
            .map(|entry| {
                let (file, values) = entry
                    .split_once(':')
                    .unwrap_or_else(|| panic!("invalid token-map fixture: {entry}"));
                (file.to_owned(), records(&values.split(',').collect::<Vec<_>>()))
            })
            .collect()
    }

    fn collect_token_map<T, F>(entries: &[T], build: F) -> TokenMap
    where
        F: Fn(&T) -> (String, Vec<TokenRecord>),
    {
        entries.iter().map(build).collect()
    }

    fn records_with_line_offset(values: &[&str], line_offset: u32) -> Vec<TokenRecord> {
        values
            .iter()
            .enumerate()
            .map(|(index, value)| TokenRecord {
                value: (*value).to_owned(),
                line: line_offset.saturating_add(u32::try_from(index + 1).unwrap_or(u32::MAX)),
                index,
            })
            .collect()
    }

    fn function(
        file: &str,
        name: &str,
        stable_symbol: &str,
        start_line: u32,
        statement_count: u32,
        values: &[&str],
    ) -> FunctionRecord {
        let mut function = FunctionRecord::new(
            Language::Rust,
            name,
            file,
            start_line,
            start_line.saturating_add(3),
            1,
        );
        function.stable_symbol = stable_symbol.to_owned();
        function.statement_count = statement_count;
        function.normalized_tokens = values.iter().map(|value| (*value).to_owned()).collect();
        function.visibility = SymbolVisibility::Private;
        function.structural_metrics_reliable = true;
        function
    }

    fn enhanced_config() -> DryConfig {
        DryConfig {
            min_tokens: 4,
            min_statements: 2,
            similarity_threshold: 0.75,
            shingle_tokens: 1,
            max_groups: 50,
            max_occurrences_per_window: 100,
            ..DryConfig::default()
        }
    }

    fn exact_config() -> DryConfig {
        DryConfig {
            similarity_threshold: 1.0,
            ..enhanced_config()
        }
    }

    struct ExactDetector;

    impl ExactDetector {
        fn detected(tokens: &TokenMap, min_tokens: usize, max_groups: usize, limit: usize) -> Vec<Duplicate> {
            match find_duplicates(tokens, min_tokens, max_groups, limit) {
                Ok(duplicates) => duplicates,
                Err(error) => panic!("duplicate detection failed: {error}"),
            }
        }
    }

    struct BudgetDetector;

    impl BudgetDetector {
        fn bounded_duplicates(tokens: &TokenMap, budget: DryWorkBudget) -> Result<Vec<Duplicate>, DryError> {
            find_duplicates_with_budget(tokens, 4, 10, 100, budget)
        }
    }

    impl TestSuite {
        fn test_location(file: &str, start_token: usize, end_token: usize) -> Location {
            Location {
                file: file.to_owned(),
                start_line: u32::try_from(start_token.saturating_add(1)).unwrap_or(u32::MAX),
                end_line: u32::try_from(end_token).unwrap_or(u32::MAX),
                stable_symbol: None,
                start_token,
                end_token,
            }
        }

        fn exact_test_group(token_count: usize, spans: &[(&str, usize, usize)]) -> Duplicate {
            exact_duplicate(
                token_count,
                spans
                    .iter()
                    .map(|(file, start, end)| Self::test_location(file, *start, *end))
                    .collect(),
                "test-clone-group".to_owned(),
            )
        }

        fn encoded_test_group(token_count: usize, encoded_spans: &str) -> Duplicate {
            let spans = encoded_spans
                .split('|')
                .map(|span| {
                    let mut fields = span.split(',');
                    let file = fields.next().unwrap_or_else(|| panic!("missing fixture file"));
                    let number = |value: Option<&str>| {
                        value
                            .unwrap_or_else(|| panic!("missing fixture span"))
                            .parse::<usize>()
                            .unwrap_or_else(|error| panic!("fixture span: {error}"))
                    };
                    (file, number(fields.next()), number(fields.next()))
                })
                .collect::<Vec<_>>();
            Self::exact_test_group(token_count, &spans)
        }

        fn analyzed_functions(functions: &[FunctionRecord], config: &DryConfig) -> Vec<Duplicate> {
            analyze_functions(functions, config)
                .unwrap_or_else(|error| panic!("function clone analysis failed: {error}"))
        }

        fn exact_function_groups(functions: &[FunctionRecord]) -> Vec<Duplicate> {
            Self::analyzed_functions(functions, &exact_config())
        }

        fn integrated_groups(
            tokens: &TokenMap,
            functions: &[FunctionRecord],
            config: &DryConfig,
        ) -> Vec<Duplicate> {
            analyze_with_functions(tokens, functions, config)
                .unwrap_or_else(|error| panic!("integrated clone analysis failed: {error}"))
        }

        fn exact_integrated_groups(tokens: &TokenMap, functions: &[FunctionRecord]) -> Vec<Duplicate> {
            Self::integrated_groups(tokens, functions, &exact_config())
        }

        fn only_group(groups: &[Duplicate]) -> &Duplicate {
            assert_eq!(groups.len(), 1);
            &groups[0]
        }
    }

    struct Assertions;

    impl Assertions {
        fn assert_group_shape(group: &Duplicate, expected: (usize, usize, Option<u32>, Option<f64>, &str)) {
            assert_eq!(
                (
                    group.token_count,
                    group.locations.len(),
                    group.statement_count,
                    group.similarity,
                    group.algorithm.as_deref(),
                ),
                (expected.0, expected.1, expected.2, expected.3, Some(expected.4))
            );
            assert!(group.clone_group_id.is_some());
        }
    }

    struct FunctionAssertions;

    impl FunctionAssertions {
        fn assert_function_observation(
            functions: &[FunctionRecord],
            expected: (usize, usize, Option<u32>, Option<f64>, &str),
        ) {
            let groups = TestSuite::exact_function_groups(functions);
            Assertions::assert_group_shape(TestSuite::only_group(&groups), expected);
        }
    }

    struct BudgetAssertions;

    impl BudgetAssertions {
        fn error(
            actual: Result<Vec<Duplicate>, DryError>,
            expected: Option<(WorkBudgetKind, usize, usize)>,
        ) -> (WorkBudgetKind, usize, usize) {
            let actual = match actual {
                Err(DryError::WorkBudgetExceeded {
                    kind,
                    limit,
                    required,
                }) => (kind, limit, required),
                other => panic!("expected work-budget error, got {other:?}"),
            };
            if let Some(expected) = expected {
                assert_eq!(actual, expected);
            }
            actual
        }
    }

    struct IdentityAssertions;

    impl IdentityAssertions {
        fn group_id(groups: &[Duplicate]) -> &str {
            TestSuite::only_group(groups)
                .clone_group_id
                .as_deref()
                .unwrap_or_else(|| panic!("clone group has no identity"))
        }
    }

    struct AlgorithmAssertions;

    impl AlgorithmAssertions {
        fn required_algorithm_group<'a>(groups: &'a [Duplicate], algorithm: &str) -> &'a Duplicate {
            groups
                .iter()
                .find(|group| group.algorithm.as_deref() == Some(algorithm))
                .unwrap_or_else(|| panic!("expected clone algorithm {algorithm}"))
        }
    }

    struct FunctionErrorAssertions;

    impl FunctionErrorAssertions {
        fn required(functions: &[FunctionRecord], config: &DryConfig) -> DryError {
            ErrorAssertions::required(analyze_functions(functions, config), "function-analysis")
        }
    }

    struct ErrorAssertions;

    impl ErrorAssertions {
        fn required<T>(result: Result<T, DryError>, analysis: &str) -> DryError {
            match result {
                Err(error) => error,
                Ok(_) => panic!("expected {analysis} error"),
            }
        }
    }

    struct ExactAssertions;

    impl ExactAssertions {
        fn assert_exact_observation(groups: &[Duplicate], expected: &str) {
            use std::fmt::Write as _;

            let mut actual = String::new();
            for group in groups {
                assert_eq!(group.algorithm.as_deref(), Some(EXACT_ALGORITHM));
                assert_eq!(group.similarity, Some(1.0));
                assert!(group.clone_group_id.is_some());
                let _ = write!(actual, "{}", group.token_count);
                for location in &group.locations {
                    let _ = write!(
                        actual,
                        "|{}:{}-{}:{}-{}",
                        location.file,
                        location.start_token,
                        location.end_token,
                        location.start_line,
                        location.end_line,
                    );
                }
            }
            assert_eq!(actual, expected);
        }
    }

    fn function_pair(left: &[&str], right: &[&str]) -> Vec<FunctionRecord> {
        [
            ("src/a.rs", "left", "crate::left()", 1, left),
            ("src/b.rs", "right", "crate::right()", 10, right),
        ]
        .into_iter()
        .map(|(file, name, symbol, line, values)| function(file, name, symbol, line, 2, values))
        .collect()
    }

    fn matching_function_pair(values: &[&str]) -> Vec<FunctionRecord> {
        function_pair(values, values)
    }

    fn threshold_boundary_functions() -> Vec<FunctionRecord> {
        let values = ["fn", "ID", "return", "NUM"];
        let mut functions = matching_function_pair(&values);
        functions.push(function(
            "src/c.rs",
            "too_short",
            "crate::too_short()",
            20,
            2,
            &["fn", "ID", "return"],
        ));
        functions.push(function(
            "src/d.rs",
            "too_few_statements",
            "crate::too_few_statements()",
            30,
            1,
            &values,
        ));
        functions
    }

    fn moved_identity_pair(left_line: u32, right_line: u32, reverse: bool) -> Vec<FunctionRecord> {
        let values = ["call", "ID", "(", "NUM", ")"];
        let mut functions = matching_function_pair(&values);
        functions[0].start_line = left_line;
        functions[0].end_line = left_line.saturating_add(3);
        "src/z.rs".clone_into(&mut functions[1].file);
        functions[1].start_line = right_line;
        functions[1].end_line = right_line.saturating_add(3);
        if reverse {
            functions.reverse();
        }
        functions
    }

    fn overloaded_functions() -> Vec<FunctionRecord> {
        let values = ["match", "ID", "return", "NUM"];
        [
            ("src/lib.rs", "crate::convert(i32)", 10),
            ("src/lib.rs", "crate::convert(&str)", 30),
            ("src/other.rs", "crate::other::convert()", 50),
        ]
        .into_iter()
        .map(|(file, symbol, line)| function(file, "convert", symbol, line, 3, &values))
        .collect()
    }

    fn repeated_token_map(files: &[&str], values: &[&str]) -> TokenMap {
        collect_token_map(files, |file| ((*file).to_owned(), records(values)))
    }

    fn encoded_repeated_token_map(files: &str, values: &str) -> TokenMap {
        repeated_token_map(&encoded_values(files), &encoded_values(values))
    }

    fn collision_token_map() -> TokenMap {
        encoded_token_map("a.py:a,b,c,d;b.py:w,x,y,z")
    }

    fn offset_token_map(values: &[&str], files: &[(&str, u32)]) -> TokenMap {
        collect_token_map(files, |(file, offset)| {
            ((*file).to_owned(), records_with_line_offset(values, *offset))
        })
    }

    const fn budget_with(kind: WorkBudgetKind, limit: usize) -> DryWorkBudget {
        let mut budget = DryWorkBudget {
            max_total_windows: DRY_DEFAULT_MAX_TOTAL_WINDOWS,
            max_fingerprint_buckets: DRY_DEFAULT_MAX_FINGERPRINT_BUCKETS,
            max_candidate_work: DRY_DEFAULT_MAX_CANDIDATE_WORK,
        };
        match kind {
            WorkBudgetKind::TotalWindows => budget.max_total_windows = limit,
            WorkBudgetKind::FingerprintBuckets => budget.max_fingerprint_buckets = limit,
            WorkBudgetKind::CandidateWork => budget.max_candidate_work = limit,
        }
        budget
    }

    fn config_with_budget(kind: WorkBudgetKind, limit: usize) -> DryConfig {
        let budget = budget_with(kind, limit);
        let mut config = enhanced_config();
        config.max_total_windows = budget.max_total_windows;
        config.max_fingerprint_buckets = budget.max_fingerprint_buckets;
        config.max_candidate_work = budget.max_candidate_work;
        config
    }

    struct DetectorContracts;

    impl DetectorContracts {
        fn exact_region_detection_contracts() {
            let cases: Vec<(TokenMap, usize, &str)> = vec![
                (
                    encoded_token_map("a.py:left,a,b,c,d,e,tail-a;b.py:right,a,b,c,d,e,tail-b"),
                    100,
                    "5|a.py:1-6:2-6|b.py:1-6:2-6",
                ),
                (
                    encoded_repeated_token_map("a.py", "a|b|c|d|separator|a|b|c|d"),
                    100,
                    "4|a.py:0-4:1-4|a.py:5-9:6-9",
                ),
                (encoded_token_map("a.py:x,x,x,x,x,x,x"), 100, ""),
                (
                    encoded_repeated_token_map("a.py|b.py|c.py", "a|b|c|d"),
                    2,
                    "4|a.py:0-4:1-4|b.py:0-4:1-4",
                ),
            ];

            for (tokens, occurrence_limit, expected) in cases {
                let groups = ExactDetector::detected(&tokens, 4, 10, occurrence_limit);
                ExactAssertions::assert_exact_observation(&groups, expected);
            }
        }

        fn verifies_values_when_fingerprints_collide() {
            let tokens = collision_token_map();

            let duplicates =
                match exact_options(4, 10, 100)
                    .find_with_hasher(&tokens, DryWorkBudget::default(), |_| Fingerprint::ZERO)
                {
                    Ok(duplicates) => duplicates,
                    Err(error) => panic!("forced-collision analysis failed: {error}"),
                };

            assert!(duplicates.is_empty());
        }

        fn rolling_fingerprints_match_direct_polynomial_evaluation() {
            let values = encoded_values("alpha|ID|+|NUM|omega|return|ID");
            let tokens = records(&values);
            let mut rolling = Vec::new();
            if let Err(error) = for_each_fingerprinted_window(
                &tokens,
                4,
                |token| stable_token_fingerprint(&token.value),
                |_, key| {
                    rolling.push(key);
                    Ok(())
                },
            ) {
                panic!("rolling fingerprints failed: {error}");
            }

            let direct: Vec<_> = tokens
                .windows(4)
                .map(|window| {
                    window.iter().fold(Fingerprint::ZERO, |value, token| {
                        value.append(stable_token_fingerprint(&token.value))
                    })
                })
                .collect();

            assert_eq!(rolling, direct);
        }

        fn rolling_index_hashes_each_token_once_instead_of_each_window() {
            use std::cell::Cell;

            let tokens = encoded_token_map("large.py:one,two,three,four,five,six,seven,eight,nine,ten");
            let calls = Cell::new(0_usize);
            let result =
                exact_options(8, 10, 100).find_with_hasher(&tokens, DryWorkBudget::default(), |value| {
                    calls.set(calls.get().saturating_add(1));
                    stable_token_fingerprint(value)
                });

            if let Err(error) = result {
                panic!("rolling-index analysis failed: {error}");
            }
            assert_eq!(calls.get(), 10);
        }

        fn maximal_group_selection_contracts() {
            let outer = TestSuite::encoded_test_group(10, "a.py,2,12|b.py,20,30");
            let inner = TestSuite::encoded_test_group(4, "a.py,5,9|b.py,23,27");

            assert!(contained_by(&inner, &outer));
            assert!(!contained_by(&outer, &inner));
            assert_eq!(
                select_maximal_exact_groups(vec![outer.clone(), inner.clone()]),
                vec![outer.clone()]
            );

            let mut uncovered = inner;
            uncovered.locations.push(TestSuite::test_location("c.py", 40, 44));

            assert!(!exact_group_contained_by(&uncovered, &outer));
            assert_eq!(select_maximal_exact_groups(vec![outer, uncovered]).len(), 2);
        }

        fn overlapping_exact_variants_do_not_consume_independent_group_slots() {
            let primary = TestSuite::encoded_test_group(10, "a.py,0,10|b.py,20,30");
            let one_new = TestSuite::encoded_test_group(8, "a.py,2,10|b.py,22,30|c.py,40,48");
            let two_new = TestSuite::encoded_test_group(7, "a.py,3,10|d.py,50,57|e.py,60,67");
            let separate = TestSuite::encoded_test_group(6, "f.py,70,76|g.py,80,86");

            let expected = vec![primary.clone(), two_new.clone(), separate.clone()];
            assert_eq!(
                select_independent_exact_groups(vec![separate, two_new, one_new, primary]),
                expected
            );
        }

        fn output_order_and_group_limit_are_deterministic() {
            let mut tokens = encoded_repeated_token_map("a.py|b.py", "a|b|c|d|e|f");
            tokens.extend(encoded_repeated_token_map("c.py|d.py", "q|r|s|t|u"));

            let first = ExactDetector::detected(&tokens, 4, 1, 100);
            let second = ExactDetector::detected(&tokens, 4, 1, 100);

            assert_eq!(first, second);
            ExactAssertions::assert_exact_observation(&first, "6|a.py:0-6:1-6|b.py:0-6:1-6");
        }

        fn dice_similarity_equal_to_threshold_is_accepted() {
            let left = encoded_values("a|b|c|d");
            let right = encoded_values("a|b|c|x");
            let functions = function_pair(&left, &right);
            let mut config = enhanced_config();
            config.similarity_threshold = 0.75;

            let boundary = TestSuite::analyzed_functions(&functions, &config);
            assert_eq!(TestSuite::only_group(&boundary).similarity, Some(0.75));

            config.similarity_threshold = 0.750_000_001;
            let above = TestSuite::analyzed_functions(&functions, &config);
            assert!(above.is_empty());
        }

        fn clone_group_id_and_order_ignore_input_order_and_line_movement() {
            let before = moved_identity_pair(10, 80, true);
            let after = moved_identity_pair(210, 480, false);

            let before_groups = TestSuite::exact_function_groups(&before);
            let after_groups = TestSuite::exact_function_groups(&after);
            let before_group = TestSuite::only_group(&before_groups);
            let after_group = TestSuite::only_group(&after_groups);

            assert_eq!(
                IdentityAssertions::group_id(&before_groups),
                IdentityAssertions::group_id(&after_groups)
            );
            assert_eq!(
                (
                    before_group.locations[0].file.as_str(),
                    after_group.locations[0].file.as_str(),
                ),
                ("src/a.rs", "src/a.rs")
            );
        }

        fn function_group_shape_and_identity_contracts() {
            FunctionAssertions::assert_function_observation(
                &threshold_boundary_functions(),
                (4, 2, Some(2), Some(1.0), SHINGLE_DICE_ALGORITHM),
            );

            let functions = overloaded_functions();
            let groups = TestSuite::exact_function_groups(&functions);
            let group = TestSuite::only_group(&groups);

            Assertions::assert_group_shape(group, (4, 3, Some(3), Some(1.0), SHINGLE_DICE_ALGORITHM));
            let symbols: BTreeSet<_> = group
                .locations
                .iter()
                .filter_map(|location| location.stable_symbol.as_deref())
                .collect();
            assert_eq!(symbols.len(), 3);
        }

        fn aliased_physical_function_is_not_compared_with_itself() {
            let values = ["match", "ID", "return", "NUM"];
            let mut aliases = matching_function_pair(&values);
            "src/shared.rs".clone_into(&mut aliases[0].file);
            "src/shared.rs".clone_into(&mut aliases[1].file);
            aliases[1].start_line = aliases[0].start_line;
            aliases[1].end_line = aliases[0].end_line;

            assert!(TestSuite::exact_function_groups(&aliases).is_empty());
        }

        fn enhanced_analysis_respects_every_work_budget() {
            let functions = matching_function_pair(&["a", "b", "c", "d"]);
            let cases = [
                (WorkBudgetKind::CandidateWork, 1, 2),
                (WorkBudgetKind::TotalWindows, 7, 8),
                (WorkBudgetKind::FingerprintBuckets, 1, 2),
            ];
            for (kind, limit, required) in cases {
                let config = config_with_budget(kind, limit);
                BudgetAssertions::error(
                    analyze_functions(&functions, &config),
                    Some((kind, limit, required)),
                );
            }
        }

        fn enhanced_entry_retains_and_consolidates_unmapped_exact_regions() {
            let tokens = encoded_repeated_token_map("src/a.rs|src/b.rs|src/c.rs", "a|b|c|d");
            let groups = TestSuite::exact_integrated_groups(&tokens, &[]);

            ExactAssertions::assert_exact_observation(
                &groups,
                "4|src/a.rs:0-4:1-4|src/b.rs:0-4:1-4|src/c.rs:0-4:1-4",
            );
        }

        fn exact_fallback_group_id_ignores_unrelated_line_movement() {
            let values = ["a", "b", "c", "d"];
            let before = offset_token_map(&values, &[("src/a.rs", 0), ("src/b.rs", 10)]);
            let after = offset_token_map(&values, &[("src/a.rs", 100), ("src/b.rs", 210)]);
            let before_groups = TestSuite::exact_integrated_groups(&before, &[]);
            let after_groups = TestSuite::exact_integrated_groups(&after, &[]);

            assert_eq!(
                IdentityAssertions::group_id(&before_groups),
                IdentityAssertions::group_id(&after_groups)
            );
        }

        fn reliable_function_clone_does_not_hide_the_exact_region_pair() {
            let values = ["a", "b", "c", "d"];
            let tokens = repeated_token_map(&["src/a.rs", "src/b.rs"], &values);
            let functions = matching_function_pair(&values);

            let groups = TestSuite::exact_integrated_groups(&tokens, &functions);

            assert_eq!(groups.len(), 2);
            let function_group =
                AlgorithmAssertions::required_algorithm_group(&groups, SHINGLE_DICE_ALGORITHM);
            assert_eq!(function_group.statement_count, Some(2));
            assert!(groups
                .iter()
                .any(|group| group.algorithm.as_deref() == Some(EXACT_ALGORITHM)));
        }

        fn exact_repeated_block_inside_one_reliable_function_is_retained() {
            let values = encoded_values("prefix|a|b|c|d|middle|a|b|c|d|suffix");
            let tokens = token_map(&[("src/lib.rs", &values)]);
            let mut enclosing = function("src/lib.rs", "enclosing", "crate::enclosing()", 1, 8, &values);
            enclosing.end_line = 11;
            let groups = TestSuite::integrated_groups(&tokens, &[enclosing], &enhanced_config());

            let exact = AlgorithmAssertions::required_algorithm_group(&groups, EXACT_ALGORITHM);
            assert_eq!((exact.token_count, exact.locations.len()), (4, 2));
        }

        fn enhanced_limits_reject_invalid_statement_similarity_and_shingle_values() {
            let functions = Vec::new();
            let mut config = enhanced_config();
            let error = |config: &DryConfig| FunctionErrorAssertions::required(&functions, config);

            config.min_statements = 0;
            let min_statement_error = error(&config);

            config.min_statements = 1;
            config.similarity_threshold = f64::NAN;
            let similarity_error = error(&config);

            config.similarity_threshold = 1.0;
            config.shingle_tokens = 5;
            let shingle_error = error(&config);

            assert_eq!(min_statement_error, DryError::MinStatements { provided: 0 });
            assert!(matches!(
                similarity_error,
                DryError::SimilarityThreshold { provided } if provided.is_nan()
            ));
            assert_eq!(
                shingle_error,
                DryError::ShingleTokens {
                    provided: 5,
                    min_tokens: 4,
                },
            );
        }

        fn validates_all_limits() {
            let tokens = TokenMap::new();
            let actual = [
                exact_options(3, 10, 100),
                exact_options(4, 0, 100),
                exact_options(4, 10, 1),
            ]
            .map(|options| {
                ErrorAssertions::required(
                    find_duplicates(
                        &tokens,
                        options.min_tokens,
                        options.max_groups,
                        options.max_occurrences_per_window,
                    ),
                    "exact-analysis",
                )
            });
            assert_eq!(
                actual,
                [
                    DryError::MinTokens { provided: 3 },
                    DryError::MaxGroups,
                    DryError::MaxOccurrencesPerWindow { provided: 1 },
                ]
            );
        }

        fn rejects_work_budgets_above_immutable_hard_limits() {
            let tokens = TokenMap::new();
            let provided = DRY_HARD_MAX_TOTAL_WINDOWS.saturating_add(1);
            let budget = budget_with(WorkBudgetKind::TotalWindows, provided);

            assert_eq!(
                BudgetDetector::bounded_duplicates(&tokens, budget),
                Err(DryError::InvalidWorkBudget {
                    kind: WorkBudgetKind::TotalWindows,
                    provided,
                    hard_limit: DRY_HARD_MAX_TOTAL_WINDOWS,
                })
            );
        }

        fn work_budget_exhaustion_contracts() {
            let tokens = token_map(&[("a.py", &["a", "b", "c", "d", "e"])]);
            for kind in [WorkBudgetKind::TotalWindows, WorkBudgetKind::FingerprintBuckets] {
                let budget = budget_with(kind, 1);
                BudgetAssertions::error(
                    BudgetDetector::bounded_duplicates(&tokens, budget),
                    Some((kind, 1, 2)),
                );
            }

            let tokens = collision_token_map();
            let budget = budget_with(WorkBudgetKind::CandidateWork, 1);

            let result = exact_options(4, 10, 100).find_with_hasher(&tokens, budget, |_| Fingerprint::ZERO);
            BudgetAssertions::error(result, Some((WorkBudgetKind::CandidateWork, 1, 2)));

            let repeated = vec!["x"; 200];
            let tokens = token_map(&[("a.py", repeated.as_slice())]);
            let budget = budget_with(WorkBudgetKind::CandidateWork, 1_000);

            let first = BudgetDetector::bounded_duplicates(&tokens, budget);
            let second = BudgetDetector::bounded_duplicates(&tokens, budget);
            assert_eq!(first, second);
            let (kind, limit, _) = BudgetAssertions::error(first, None);
            assert_eq!((kind, limit), (WorkBudgetKind::CandidateWork, 1_000));

            let tokens = encoded_repeated_token_map("a.py|b.py", "a|b|c|d|e|f");
            let budget = budget_with(WorkBudgetKind::CandidateWork, 5);

            let (kind, limit, _) =
                BudgetAssertions::error(BudgetDetector::bounded_duplicates(&tokens, budget), None);
            assert_eq!((kind, limit), (WorkBudgetKind::CandidateWork, 5));
        }

        fn serialized_contract_excludes_internal_token_offsets() {
            let duplicate = TestSuite::exact_test_group(4, &[("src/a.py", 10, 14)]);

            let value = match serde_json::to_value(duplicate) {
                Ok(value) => value,
                Err(error) => panic!("serialization failed: {error}"),
            };

            assert_eq!(value["token_count"], 4);
            assert_eq!(value["locations"][0]["file"], "src/a.py");
            assert!(value["locations"][0].get("start_token").is_none());
            assert!(value["locations"][0].get("end_token").is_none());
        }

        fn run_enhanced_contracts() {
            Self::dice_similarity_equal_to_threshold_is_accepted();
            Self::clone_group_id_and_order_ignore_input_order_and_line_movement();
            Self::function_group_shape_and_identity_contracts();
            Self::aliased_physical_function_is_not_compared_with_itself();
            Self::enhanced_analysis_respects_every_work_budget();
            Self::enhanced_entry_retains_and_consolidates_unmapped_exact_regions();
            Self::exact_fallback_group_id_ignores_unrelated_line_movement();
            Self::reliable_function_clone_does_not_hide_the_exact_region_pair();
            Self::exact_repeated_block_inside_one_reliable_function_is_retained();
            Self::enhanced_limits_reject_invalid_statement_similarity_and_shingle_values();
        }

        fn run() {
            let cases = [
                Self::exact_region_detection_contracts as fn(),
                Self::verifies_values_when_fingerprints_collide as TestCase,
                Self::rolling_fingerprints_match_direct_polynomial_evaluation,
                Self::rolling_index_hashes_each_token_once_instead_of_each_window as TestCase,
                Self::maximal_group_selection_contracts as fn(),
                Self::overlapping_exact_variants_do_not_consume_independent_group_slots,
                Self::output_order_and_group_limit_are_deterministic,
                Self::run_enhanced_contracts as TestCase,
                Self::validates_all_limits as TestCase,
                Self::rejects_work_budgets_above_immutable_hard_limits as fn(),
                Self::work_budget_exhaustion_contracts as TestCase,
                Self::serialized_contract_excludes_internal_token_offsets,
            ];
            for case in cases {
                case();
            }
        }
    }

    #[test]
    fn detector_contracts() {
        DetectorContracts::run();
    }

    #[test]
    fn defensive_branch_contracts() {
        let mut work = candidate_work(1);
        assert_eq!(dice_similarity(&[], &[], 1.0, &mut work), Ok(0.0));

        let mut accepted = false;
        let short_window = for_each_fingerprinted_window(
            &["only"],
            2,
            |_| Fingerprint::ZERO,
            |_, _| {
                accepted = true;
                Ok(())
            },
        );
        assert_eq!(short_window, Ok(()));
        assert!(!accepted);

        let tokens = token_map(&[("a.py", &["a", "b", "c", "d"])]);
        let invalid_groups = vec![
            TestSuite::exact_test_group(4, &[]),
            TestSuite::exact_test_group(4, &[("missing.py", 0, 4)]),
            TestSuite::exact_test_group(4, &[("a.py", 0, 4), ("a.py", 0, 4)]),
        ];
        assert!(exact_fallback_groups(invalid_groups, &tokens).is_empty());
    }
}
