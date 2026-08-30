use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bounded_file::optional_symlink_metadata;
use crate::{
    checked_duration_from_secs_f64, read_bounded_utf8_file, read_bounded_utf8_file_within,
    validate_dry_work_limit, validate_max_source_bytes, BackendPreference, CoreError,
    DRY_DEFAULT_MAX_CANDIDATE_WORK, DRY_DEFAULT_MAX_FINGERPRINT_BUCKETS, DRY_DEFAULT_MAX_TOTAL_WINDOWS,
    DRY_HARD_MAX_CANDIDATE_WORK, DRY_HARD_MAX_FINGERPRINT_BUCKETS, DRY_HARD_MAX_TOTAL_WINDOWS,
    PROJECT_METADATA_MAX_BYTES,
};

macro_rules! config_record {
    ($vis:vis struct $name:ident { $($field_vis:vis $field:ident: $field_type:ty),* $(,)? }) => {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        $vis struct $name {
            $($field_vis $field: $field_type),*
        }
    };
}

config_record! {
    pub struct RepoRigorConfig {
        pub backend: BackendPreference,
        pub include_tests: bool,
        pub allow_parse_errors: bool,
        pub max_source_bytes: usize,
        pub crap: CrapConfig,
        pub dry: DryConfig,
        pub mutation: MutationConfig,
        pub kiss: KissConfig,
        pub yagni: YagniConfig,
        pub architecture: ArchitectureConfig,
        pub cohesion: CohesionConfig,
        pub baseline: BaselineConfig,
    }
}

impl Default for RepoRigorConfig {
    fn default() -> Self {
        Self {
            backend: BackendPreference::Auto,
            include_tests: false,
            allow_parse_errors: false,
            max_source_bytes: 8 * 1024 * 1024,
            crap: CrapConfig::default(),
            dry: DryConfig::default(),
            mutation: MutationConfig::default(),
            kiss: KissConfig::default(),
            yagni: YagniConfig::default(),
            architecture: ArchitectureConfig::default(),
            cohesion: CohesionConfig::default(),
            baseline: BaselineConfig::default(),
        }
    }
}

impl RepoRigorConfig {
    /// Load an explicit configuration or discover one below `root`.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected file cannot be read or parsed, or
    /// when one of its values fails validation.
    pub fn discover(root: &Path, explicit: Option<&Path>) -> Result<(Self, Option<PathBuf>), CoreError> {
        let (config, path) = discover_unvalidated_config(root, explicit)?;
        validate_complete_config(&config)
            .map_err(|message| discovered_config_error(path.as_deref(), &message))?;
        Ok((config, path))
    }

    /// Validate configuration values before analysis starts.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message for limits or commands that cannot
    /// produce a meaningful, bounded analysis.
    pub fn validate(&self) -> Result<(), String> {
        validate_complete_config(self)
    }
}

fn discover_unvalidated_config(
    root: &Path,
    explicit: Option<&Path>,
) -> Result<(RepoRigorConfig, Option<PathBuf>), CoreError> {
    let Some(selection) = select_config(root, explicit)? else {
        return Ok((RepoRigorConfig::default(), None));
    };
    let contents = read_selected_config(root, &selection)?;
    let path = selection.path;
    let config = parse_selected_config(&path, &contents)?;
    Ok((config, Some(path)))
}

fn discovered_config_error(path: Option<&Path>, message: &str) -> CoreError {
    if let Some(path) = path {
        CoreError::Config(format!("{}: {message}", path.display()))
    } else {
        CoreError::Config(format!("default configuration: {message}"))
    }
}

fn validate_complete_config(config: &RepoRigorConfig) -> Result<(), String> {
    validate_analysis_config(config)?;
    validate_structural_sections(config)
}

fn validate_analysis_config(config: &RepoRigorConfig) -> Result<(), String> {
    validate_max_source_bytes(config.max_source_bytes)?;
    validate_crap_config(&config.crap)?;
    validate_dry_config(&config.dry)?;
    validate_mutation_config(&config.mutation)
}

struct ConfigSelection {
    path: PathBuf,
    explicitly_selected: bool,
}

fn select_config(root: &Path, explicit: Option<&Path>) -> Result<Option<ConfigSelection>, CoreError> {
    if let Some(path) = explicit {
        return Ok(Some(ConfigSelection {
            path: path.to_path_buf(),
            explicitly_selected: true,
        }));
    }
    Ok(discover_config_path(root)?.map(|path| ConfigSelection {
        path,
        explicitly_selected: false,
    }))
}

fn read_selected_config(root: &Path, selection: &ConfigSelection) -> Result<String, CoreError> {
    if selection.explicitly_selected {
        read_bounded_utf8_file(&selection.path, PROJECT_METADATA_MAX_BYTES)
    } else {
        read_bounded_utf8_file_within(root, &selection.path, PROJECT_METADATA_MAX_BYTES)
    }
}

fn parse_selected_config(path: &Path, contents: &str) -> Result<RepoRigorConfig, CoreError> {
    toml::from_str(contents).map_err(|error| CoreError::Config(format!("{}: {error}", path.display())))
}

fn validate_crap_config(config: &CrapConfig) -> Result<(), String> {
    if config.fail_over.is_finite() && config.fail_over >= 0.0 {
        Ok(())
    } else {
        Err("crap.fail_over must be a non-negative finite number".to_string())
    }
}

fn validate_dry_config(config: &DryConfig) -> Result<(), String> {
    validate_dry_shape(config)?;
    validate_dry_limits(config)
}

fn validate_dry_shape(config: &DryConfig) -> Result<(), String> {
    validate_minimum_usize("dry.min_tokens", config.min_tokens, 4)?;
    validate_nonzero_usize("dry.min_statements", config.min_statements)?;
    validate_open_closed_unit_interval("dry.similarity_threshold", config.similarity_threshold)?;
    validate_shingle_width(config)
}

fn validate_dry_limits(config: &DryConfig) -> Result<(), String> {
    validate_nonzero_usize("dry.max_groups", config.max_groups)?;
    validate_minimum_usize(
        "dry.max_occurrences_per_window",
        config.max_occurrences_per_window,
        2,
    )?;
    validate_dry_work_limits(config)
}

fn validate_shingle_width(config: &DryConfig) -> Result<(), String> {
    if (1..=config.min_tokens).contains(&config.shingle_tokens) {
        Ok(())
    } else {
        Err("dry.shingle_tokens must be in 1..=dry.min_tokens".to_string())
    }
}

fn validate_dry_work_limits(config: &DryConfig) -> Result<(), String> {
    validate_dry_work_limit(
        "dry.max_total_windows",
        config.max_total_windows,
        DRY_HARD_MAX_TOTAL_WINDOWS,
    )?;
    validate_dry_work_limit(
        "dry.max_fingerprint_buckets",
        config.max_fingerprint_buckets,
        DRY_HARD_MAX_FINGERPRINT_BUCKETS,
    )?;
    validate_dry_work_limit(
        "dry.max_candidate_work",
        config.max_candidate_work,
        DRY_HARD_MAX_CANDIDATE_WORK,
    )
}

fn validate_mutation_config(config: &MutationConfig) -> Result<(), String> {
    checked_duration_from_secs_f64(config.timeout_seconds)
        .map_err(|error| format!("mutation.timeout_seconds {error}"))?;
    validate_optional_nonzero("mutation.max_mutants", config.max_mutants)?;
    validate_unit_interval("mutation.minimum_score", config.minimum_score)?;
    validate_mutation_operators(&config.operators)?;
    validate_serial_workers(config.workers)?;
    validate_mutation_commands(config)
}

fn validate_optional_nonzero(name: &str, value: Option<usize>) -> Result<(), String> {
    if value == Some(0) {
        Err(format!("{name} must be greater than zero when set"))
    } else {
        Ok(())
    }
}

fn validate_mutation_operators(operators: &[MutationOperator]) -> Result<(), String> {
    if operators.is_empty() {
        return Err("mutation.operators must not be empty".to_string());
    }
    let unique = operators.iter().copied().collect::<BTreeSet<_>>();
    if unique.len() == operators.len() {
        Ok(())
    } else {
        Err("mutation.operators must not contain duplicates".to_string())
    }
}

fn validate_serial_workers(workers: usize) -> Result<(), String> {
    if workers == 1 {
        Ok(())
    } else {
        Err("mutation.workers must be exactly 1 for the crash-safe serial executor".to_string())
    }
}

fn validate_mutation_commands(config: &MutationConfig) -> Result<(), String> {
    validate_optional_command("mutation.test_command", config.test_command.as_deref())?;
    validate_optional_command(
        "mutation.validation_command",
        config.validation_command.as_deref(),
    )
}

fn validate_optional_command(name: &str, command: Option<&str>) -> Result<(), String> {
    if command.is_some_and(|value| value.trim().is_empty()) {
        Err(format!("{name} must not be empty when set"))
    } else {
        Ok(())
    }
}

fn validate_nonzero_usize(name: &str, value: usize) -> Result<(), String> {
    validate_minimum_usize(name, value, 1)
}

fn validate_minimum_usize(name: &str, value: usize, minimum: usize) -> Result<(), String> {
    if value >= minimum {
        Ok(())
    } else if minimum == 1 {
        Err(format!("{name} must be greater than zero"))
    } else {
        Err(format!("{name} must be at least {minimum}"))
    }
}

fn validate_open_closed_unit_interval(name: &str, value: f64) -> Result<(), String> {
    if value.is_finite() && value > 0.0 && value <= 1.0 {
        Ok(())
    } else {
        Err(format!("{name} must be finite and in (0, 1]"))
    }
}

fn validate_structural_sections(config: &RepoRigorConfig) -> Result<(), String> {
    validate_kiss_and_yagni(config)?;
    validate_architecture_config(&config.architecture)?;
    validate_unit_interval("cohesion.minimum", config.cohesion.minimum)?;
    validate_baseline_path(&config.baseline.path)
}

fn validate_kiss_and_yagni(config: &RepoRigorConfig) -> Result<(), String> {
    validate_nonzero_u32(
        "kiss.maximum_cyclomatic_complexity",
        config.kiss.maximum_cyclomatic_complexity,
    )?;
    validate_unique_strings("yagni.entry_points", &config.yagni.entry_points)
}

fn validate_architecture_config(config: &ArchitectureConfig) -> Result<(), String> {
    validate_forbidden_edges(&config.forbidden_edges)?;
    validate_architecture_modules(config)?;
    validate_architecture_layers(&config.layers)?;
    validate_contract_configuration(config)
}

fn validate_nonzero_u32(name: &str, value: u32) -> Result<(), String> {
    if value == 0 {
        Err(format!("{name} must be greater than zero"))
    } else {
        Ok(())
    }
}

fn validate_forbidden_edges(edges: &[String]) -> Result<(), String> {
    validate_unique_strings("architecture.forbidden_edges", edges)?;
    for edge in edges {
        validate_forbidden_edge(edge)?;
    }
    Ok(())
}

fn validate_forbidden_edge(edge: &str) -> Result<(), String> {
    let Some((source, destination)) = edge.split_once("->") else {
        return Err(format!(
            "architecture.forbidden_edges entry must use source->destination: {edge}"
        ));
    };
    if valid_architecture_pattern(source)
        && valid_architecture_pattern(destination)
        && !destination.contains("->")
    {
        Ok(())
    } else {
        Err(format!(
            "architecture.forbidden_edges entry must contain one non-empty canonical edge: {edge}"
        ))
    }
}

fn valid_architecture_pattern(value: &str) -> bool {
    !value.is_empty() && value.trim() == value && value.matches('*').count() <= 1
}

fn validate_architecture_modules(config: &ArchitectureConfig) -> Result<(), String> {
    for (name, values) in [
        ("architecture.domain_modules", &config.domain_modules),
        (
            "architecture.infrastructure_modules",
            &config.infrastructure_modules,
        ),
        ("architecture.interface_modules", &config.interface_modules),
        (
            "architecture.implementation_modules",
            &config.implementation_modules,
        ),
    ] {
        validate_architecture_patterns(name, values)?;
    }
    Ok(())
}

fn validate_architecture_patterns(name: &str, values: &[String]) -> Result<(), String> {
    validate_unique_strings(name, values)?;
    if values.iter().all(|value| valid_architecture_pattern(value)) {
        Ok(())
    } else {
        Err(format!("{name} entries may contain at most one '*' wildcard"))
    }
}

fn validate_architecture_layers(layers: &BTreeMap<String, u32>) -> Result<(), String> {
    let patterns = layers.keys().cloned().collect::<Vec<_>>();
    validate_unique_strings("architecture.layers", &patterns)?;
    if patterns.iter().any(|value| !valid_architecture_pattern(value)) {
        return Err("architecture.layers keys may contain at most one '*' wildcard".to_string());
    }
    Ok(())
}

fn validate_contract_configuration(config: &ArchitectureConfig) -> Result<(), String> {
    validate_unique_strings("architecture.contract_traits", &config.contract_traits)?;
    if config.contract_traits.iter().any(|value| value.contains('*')) {
        return Err("architecture.contract_traits entries must be exact stable trait symbols".to_string());
    }
    validate_contract_test_marker(&config.contract_test_marker)
}

fn validate_contract_test_marker(marker: &str) -> Result<(), String> {
    if crate::model::canonical_identity_field(marker) {
        Ok(())
    } else {
        Err("architecture.contract_test_marker must be non-empty and canonical".to_string())
    }
}

fn validate_unit_interval(name: &str, value: f64) -> Result<(), String> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(format!("{name} must be finite and in [0, 1]"))
    }
}

fn validate_unique_strings(name: &str, values: &[String]) -> Result<(), String> {
    let mut unique = BTreeSet::new();
    for value in values {
        if !crate::model::canonical_identity_field(value) {
            return Err(format!("{name} entries must be non-empty and canonical"));
        }
        if !unique.insert(value) {
            return Err(format!("{name} must not contain duplicates"));
        }
    }
    Ok(())
}

fn validate_baseline_path(path: &Path) -> Result<(), String> {
    let path = path
        .to_str()
        .ok_or_else(|| "baseline.path must be valid UTF-8".to_string())?;
    crate::normalize_repository_path(path)
        .map(|_| ())
        .map_err(|error| format!("invalid baseline.path: {error}"))
}

fn discover_config_path(root: &Path) -> Result<Option<PathBuf>, CoreError> {
    for name in ["reporigor.toml", ".reporigor.toml"] {
        let candidate = root.join(name);
        if optional_symlink_metadata(&candidate)?.is_some() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

config_record! {
    pub struct CrapConfig {
        pub fail_over: f64,
        pub allow_missing_coverage: bool,
        pub unreported_as_zero: bool,
        pub allow_empty: bool,
    }
}

impl Default for CrapConfig {
    fn default() -> Self {
        Self {
            fail_over: 6.0,
            allow_missing_coverage: false,
            unreported_as_zero: false,
            allow_empty: false,
        }
    }
}

config_record! {
    pub struct DryConfig {
        pub min_tokens: usize,
        pub min_statements: usize,
        pub similarity_threshold: f64,
        pub shingle_tokens: usize,
        pub max_groups: usize,
        pub max_occurrences_per_window: usize,
        pub max_total_windows: usize,
        pub max_fingerprint_buckets: usize,
        pub max_candidate_work: usize,
        pub fail: bool,
    }
}

impl Default for DryConfig {
    fn default() -> Self {
        Self {
            min_tokens: 30,
            min_statements: 5,
            similarity_threshold: 0.92,
            // Four preserves configurations that used the historical minimum
            // token boundary before shingle width became configurable.
            shingle_tokens: 4,
            max_groups: 50,
            max_occurrences_per_window: 100,
            max_total_windows: DRY_DEFAULT_MAX_TOTAL_WINDOWS,
            max_fingerprint_buckets: DRY_DEFAULT_MAX_FINGERPRINT_BUCKETS,
            max_candidate_work: DRY_DEFAULT_MAX_CANDIDATE_WORK,
            fail: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MutationOperator {
    BooleanLiteral,
    Comparison,
    Logical,
    Arithmetic,
}

impl MutationOperator {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BooleanLiteral => "boolean-literal",
            Self::Comparison => "comparison",
            Self::Logical => "logical",
            Self::Arithmetic => "arithmetic",
        }
    }
}

config_record! {
    pub struct MutationConfig {
        pub timeout_seconds: f64,
        pub minimum_score: f64,
        pub operators: Vec<MutationOperator>,
        pub seed: u64,
        pub workers: usize,
        pub max_mutants: Option<usize>,
        pub test_command: Option<String>,
        pub validation_command: Option<String>,
    }
}

impl Default for MutationConfig {
    fn default() -> Self {
        Self {
            timeout_seconds: 120.0,
            minimum_score: 0.8,
            operators: vec![
                MutationOperator::BooleanLiteral,
                MutationOperator::Comparison,
                MutationOperator::Logical,
                MutationOperator::Arithmetic,
            ],
            seed: 76_412_026,
            workers: 1,
            max_mutants: None,
            test_command: None,
            validation_command: None,
        }
    }
}

config_record! {
    pub struct KissConfig {
        pub maximum_cyclomatic_complexity: u32,
        pub maximum_nesting_depth: u32,
        pub maximum_function_statements: u32,
        pub maximum_parameters: u32,
        pub maximum_module_dependencies: u32,
    }
}

impl Default for KissConfig {
    fn default() -> Self {
        Self {
            maximum_cyclomatic_complexity: 12,
            maximum_nesting_depth: 5,
            maximum_function_statements: 60,
            maximum_parameters: 6,
            maximum_module_dependencies: 16,
        }
    }
}

config_record! {
    pub struct YagniConfig {
        pub maximum_unused_private_functions: usize,
        pub maximum_unused_modules: usize,
        pub maximum_unused_production_dependencies: usize,
        pub maximum_unreachable_statements: usize,
        pub maximum_unused_feature_flags: usize,
        pub maximum_unreferenced_crate_exports: usize,
        pub entry_points: Vec<String>,
    }
}

impl Default for YagniConfig {
    fn default() -> Self {
        Self {
            maximum_unused_private_functions: 0,
            maximum_unused_modules: 0,
            maximum_unused_production_dependencies: 0,
            maximum_unreachable_statements: 0,
            maximum_unused_feature_flags: 0,
            maximum_unreferenced_crate_exports: 0,
            entry_points: vec!["main".to_string(), "build.rs".to_string()],
        }
    }
}

config_record! {
    pub struct ArchitectureConfig {
        pub maximum_module_fan_out: usize,
        pub forbidden_edges: Vec<String>,
        pub domain_modules: Vec<String>,
        pub infrastructure_modules: Vec<String>,
        pub interface_modules: Vec<String>,
        pub implementation_modules: Vec<String>,
        pub layers: BTreeMap<String, u32>,
        pub contract_traits: Vec<String>,
        pub contract_test_marker: String,
    }
}

impl Default for ArchitectureConfig {
    fn default() -> Self {
        Self {
            maximum_module_fan_out: 12,
            forbidden_edges: Vec::new(),
            domain_modules: Vec::new(),
            infrastructure_modules: Vec::new(),
            interface_modules: Vec::new(),
            implementation_modules: Vec::new(),
            layers: BTreeMap::new(),
            contract_traits: Vec::new(),
            contract_test_marker: "reporigor_contract".to_string(),
        }
    }
}

config_record! {
    pub struct CohesionConfig {
        pub minimum: f64,
    }
}

impl Default for CohesionConfig {
    fn default() -> Self {
        Self { minimum: 0.1 }
    }
}

config_record! {
    pub struct BaselineConfig {
        pub enabled: bool,
        pub path: PathBuf,
    }
}

impl Default for BaselineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: PathBuf::from("reporigor-baseline.json"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{MutationOperator, RepoRigorConfig};
    use crate::PROJECT_METADATA_MAX_BYTES;

    fn assert_valid(config: &RepoRigorConfig) {
        assert!(config.validate().is_ok());
    }

    fn assert_invalid(config: &RepoRigorConfig) {
        assert!(config.validate().is_err());
    }

    fn assert_discovery_error(root: &Path, expected: crate::path_io::ExpectedCoreError) {
        crate::path_io::assert_core_error(RepoRigorConfig::discover(root, None), expected);
    }

    fn validation_error(config: &RepoRigorConfig) -> String {
        match config.validate() {
            Ok(()) => panic!("invalid configuration was unexpectedly accepted"),
            Err(message) => message,
        }
    }

    fn write_and_discover(root: &Path, name: &str, contents: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, contents).unwrap_or_else(|error| panic!("write {name}: {error}"));
        let selected =
            super::discover_config_path(root).unwrap_or_else(|error| panic!("discover {name}: {error}"));
        assert_eq!(selected.as_deref(), Some(path.as_path()));
        path
    }

    fn external_config_fixture(name: &str) -> (TempDir, TempDir, PathBuf) {
        crate::path_io::external_test_file(name, "include_tests = true\n")
    }

    fn parsed_config(contents: &str) -> RepoRigorConfig {
        toml::from_str(contents).unwrap_or_else(|error| panic!("configuration fixture: {error}"))
    }

    #[test]
    fn defaults_are_valid_and_match_the_documented_timeout() {
        let config = RepoRigorConfig::default();
        assert!(config.validate().is_ok());
        assert!((config.mutation.timeout_seconds - 120.0).abs() < f64::EPSILON);
        assert_eq!(config.dry.min_statements, 5);
        assert!((config.dry.similarity_threshold - 0.92).abs() < f64::EPSILON);
        assert_eq!(config.mutation.workers, 1);
        assert_eq!(config.kiss.maximum_cyclomatic_complexity, 12);
        assert!(!config.crap.unreported_as_zero);
        assert!(!config.baseline.enabled);
    }

    #[test]
    fn unreported_coverage_policy_is_explicit_and_backward_compatible() {
        let historical = parsed_config("[crap]\nfail_over = 6.0\n");
        assert!(!historical.crap.unreported_as_zero);

        let fail_closed = parsed_config("[crap]\nunreported_as_zero = true\n");
        assert!(fail_closed.crap.unreported_as_zero);
    }

    #[test]
    fn mutation_operator_names_cover_every_configured_variant() {
        let names = [
            (MutationOperator::BooleanLiteral, "boolean-literal"),
            (MutationOperator::Comparison, "comparison"),
            (MutationOperator::Logical, "logical"),
            (MutationOperator::Arithmetic, "arithmetic"),
        ];
        for (operator, expected) in names {
            assert_eq!(operator.as_str(), expected);
        }
    }

    #[test]
    fn implicit_config_discovery_is_optional_and_prefers_the_primary_name() {
        let root = TempDir::new().unwrap_or_else(|error| panic!("fixture: {error}"));
        let absent = super::discover_config_path(root.path())
            .unwrap_or_else(|error| panic!("absent config discovery: {error}"));
        assert_eq!(absent, None);

        write_and_discover(root.path(), ".reporigor.toml", "include_tests = true\n");
        write_and_discover(root.path(), "reporigor.toml", "include_tests = false\n");
    }

    #[test]
    fn invalid_limits_and_blank_commands_are_rejected() {
        let mut config = RepoRigorConfig::default();
        config.dry.min_tokens = 3;
        assert_invalid(&config);

        config = RepoRigorConfig::default();
        config.mutation.test_command = Some("  ".to_string());
        assert_invalid(&config);

        config = RepoRigorConfig::default();
        config.max_source_bytes = crate::MAX_SOURCE_BYTES_HARD_LIMIT + 1;
        let message = validation_error(&config);
        assert!(message.contains("immutable"));
    }

    #[test]
    fn dry_repository_budgets_cannot_disable_or_raise_hard_limits() {
        let mut config = RepoRigorConfig::default();
        config.dry.max_total_windows = 0;
        assert_invalid(&config);

        config = RepoRigorConfig::default();
        config.dry.max_candidate_work = crate::DRY_HARD_MAX_CANDIDATE_WORK.saturating_add(1);
        let message = validation_error(&config);
        assert!(message.contains("immutable"));
    }

    #[test]
    fn deterministic_dry_boundaries_are_validated() {
        let mut config = RepoRigorConfig::default();
        config.dry.min_statements = 0;
        assert_invalid(&config);

        config = RepoRigorConfig::default();
        config.dry.similarity_threshold = 1.0;
        config.dry.shingle_tokens = config.dry.min_tokens;
        assert_valid(&config);

        for threshold in [0.0, -0.1, 1.1, f64::NAN] {
            config = RepoRigorConfig::default();
            config.dry.similarity_threshold = threshold;
            assert_invalid(&config);
        }

        config = RepoRigorConfig::default();
        config.dry.shingle_tokens = config.dry.min_tokens + 1;
        assert_invalid(&config);
    }

    #[test]
    fn historical_minimum_token_config_inherits_a_compatible_shingle_width() {
        let config: RepoRigorConfig = toml::from_str("[dry]\nmin_tokens = 4\n")
            .unwrap_or_else(|error| panic!("historical config: {error}"));
        assert_eq!(config.dry.shingle_tokens, 4);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn mutation_score_operator_and_serial_worker_boundaries_are_validated() {
        for (score, valid) in [
            (0.0, true),
            (1.0, true),
            (-0.1, false),
            (1.1, false),
            (f64::INFINITY, false),
        ] {
            let mut config = RepoRigorConfig::default();
            config.mutation.minimum_score = score;
            assert_eq!(config.validate().is_ok(), valid);
        }

        let mut config = RepoRigorConfig::default();
        config.mutation.workers = 2;
        assert_invalid(&config);

        config = RepoRigorConfig::default();
        config.mutation.operators.clear();
        assert_invalid(&config);

        config = RepoRigorConfig::default();
        config.mutation.operators = vec![MutationOperator::Logical, MutationOperator::Logical];
        assert_invalid(&config);

        assert!(toml::from_str::<RepoRigorConfig>("[mutation]\noperators = [\"unrestricted\"]\n").is_err());
    }

    #[test]
    fn structural_rule_config_rejects_ambiguous_patterns_and_probabilities() {
        let mut config = RepoRigorConfig::default();
        config.kiss.maximum_cyclomatic_complexity = 0;
        assert_invalid(&config);

        config = RepoRigorConfig::default();
        config.yagni.entry_points = vec!["main".to_string(), "main".to_string()];
        assert_invalid(&config);

        config = RepoRigorConfig::default();
        config.architecture.forbidden_edges = vec!["domain infrastructure".to_string()];
        assert_invalid(&config);

        config = RepoRigorConfig::default();
        config.architecture.forbidden_edges = vec!["domain-*->infrastructure-*".to_string()];
        config.architecture.layers.insert("domain-*".to_string(), 0);
        config
            .architecture
            .layers
            .insert("infrastructure-*".to_string(), 1);
        config.cohesion.minimum = 1.0;
        assert_valid(&config);

        config.architecture.layers.clear();
        config.architecture.layers.insert("domain-**".to_string(), 0);
        assert_invalid(&config);

        config = RepoRigorConfig::default();
        config.architecture.contract_traits = vec!["crate::*".to_string()];
        assert_invalid(&config);

        config = RepoRigorConfig::default();
        config.cohesion.minimum = 1.01;
        assert_invalid(&config);
    }

    #[test]
    fn baseline_path_is_a_safe_repository_relative_native_report() {
        let mut config = RepoRigorConfig::default();
        config.baseline.enabled = true;
        config.baseline.path = "artifacts/reporigor-baseline.json".into();
        assert!(config.validate().is_ok());

        for path in [
            "",
            ".",
            "../baseline.json",
            "/tmp/baseline.json",
            r"C:\baseline.json",
        ] {
            config.baseline.path = path.into();
            assert!(config.validate().is_err(), "{path:?} must be rejected");
        }
    }

    #[test]
    fn checked_in_example_parses_and_validates() {
        let config = toml::from_str::<RepoRigorConfig>(include_str!("../../../reporigor.example.toml"))
            .unwrap_or_else(|error| panic!("example config: {error}"));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn overflowing_and_subnanosecond_timeouts_are_rejected() {
        for timeout in [1.0e300, f64::from_bits(1)] {
            let mut config = RepoRigorConfig::default();
            config.mutation.timeout_seconds = timeout;
            assert!(
                config.validate().is_err(),
                "{timeout:?} must not pass configuration validation"
            );
        }
    }

    #[test]
    fn serialized_extreme_timeouts_are_rejected_after_parsing() {
        for timeout in ["1e300", "5e-324"] {
            let source = format!("[mutation]\ntimeout_seconds = {timeout}\n");
            let Ok(config) = toml::from_str::<RepoRigorConfig>(&source) else {
                panic!("{timeout} must parse as an f64 so validation can report its range");
            };
            assert!(
                config.validate().is_err(),
                "{timeout} must not pass configuration validation"
            );
        }
    }

    #[test]
    fn unknown_configuration_keys_are_rejected() {
        let error = crate::path_io::require_error(
            toml::from_str::<RepoRigorConfig>("surprise = true"),
            "unknown keys must be actionable instead of silently ignored",
        );
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn auto_configuration_rejects_sparse_oversized_files() {
        let (root, _path) =
            crate::path_io::sparse_test_file("reporigor.toml", PROJECT_METADATA_MAX_BYTES + 1);

        assert_discovery_error(root.path(), crate::path_io::ExpectedCoreError::FileTooLarge);
    }

    #[cfg(unix)]
    #[test]
    fn auto_configuration_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let (root, _outside, target) = external_config_fixture("outside.toml");
        symlink(&target, root.path().join("reporigor.toml"))
            .unwrap_or_else(|error| panic!("symlink: {error}"));

        assert_discovery_error(root.path(), crate::path_io::ExpectedCoreError::UnsafePath);
    }

    #[cfg(unix)]
    #[test]
    fn explicit_configuration_outside_root_remains_user_selectable() {
        let (root, _outside, target) = external_config_fixture("selected.toml");

        let (config, selected) = RepoRigorConfig::discover(root.path(), Some(&target))
            .unwrap_or_else(|error| panic!("explicit config: {error}"));
        assert!(config.include_tests);
        assert_eq!(selected.as_deref(), Some(target.as_path()));
    }
}
