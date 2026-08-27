use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    checked_duration_from_secs_f64, read_bounded_utf8_file, read_bounded_utf8_file_within,
    validate_dry_work_limit, validate_max_source_bytes, BackendPreference, CoreError,
    DRY_DEFAULT_MAX_CANDIDATE_WORK, DRY_DEFAULT_MAX_FINGERPRINT_BUCKETS, DRY_DEFAULT_MAX_TOTAL_WINDOWS,
    DRY_HARD_MAX_CANDIDATE_WORK, DRY_HARD_MAX_FINGERPRINT_BUCKETS, DRY_HARD_MAX_TOTAL_WINDOWS,
    PROJECT_METADATA_MAX_BYTES,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoRigorConfig {
    pub backend: BackendPreference,
    pub include_tests: bool,
    pub allow_parse_errors: bool,
    pub max_source_bytes: usize,
    pub crap: CrapConfig,
    pub dry: DryConfig,
    pub mutation: MutationConfig,
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
        let (path, explicitly_selected) = if let Some(path) = explicit {
            (Some(path.to_path_buf()), true)
        } else {
            (discover_config_path(root)?, false)
        };
        let Some(path) = path else {
            return Ok((Self::default(), None));
        };
        let contents = if explicitly_selected {
            read_bounded_utf8_file(&path, PROJECT_METADATA_MAX_BYTES)?
        } else {
            read_bounded_utf8_file_within(root, &path, PROJECT_METADATA_MAX_BYTES)?
        };
        let config: Self = toml::from_str(&contents)
            .map_err(|error| CoreError::Config(format!("{}: {error}", path.display())))?;
        config
            .validate()
            .map_err(|message| CoreError::Config(format!("{}: {message}", path.display())))?;
        Ok((config, Some(path)))
    }

    /// Validate configuration values before analysis starts.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message for limits or commands that cannot
    /// produce a meaningful, bounded analysis.
    pub fn validate(&self) -> Result<(), String> {
        validate_max_source_bytes(self.max_source_bytes)?;
        if !self.crap.fail_over.is_finite() || self.crap.fail_over < 0.0 {
            return Err("crap.fail_over must be a non-negative finite number".to_string());
        }
        if self.dry.min_tokens < 4 {
            return Err("dry.min_tokens must be at least 4".to_string());
        }
        if self.dry.max_groups == 0 {
            return Err("dry.max_groups must be greater than zero".to_string());
        }
        if self.dry.max_occurrences_per_window < 2 {
            return Err("dry.max_occurrences_per_window must be at least 2".to_string());
        }
        validate_dry_work_limit(
            "dry.max_total_windows",
            self.dry.max_total_windows,
            DRY_HARD_MAX_TOTAL_WINDOWS,
        )?;
        validate_dry_work_limit(
            "dry.max_fingerprint_buckets",
            self.dry.max_fingerprint_buckets,
            DRY_HARD_MAX_FINGERPRINT_BUCKETS,
        )?;
        validate_dry_work_limit(
            "dry.max_candidate_work",
            self.dry.max_candidate_work,
            DRY_HARD_MAX_CANDIDATE_WORK,
        )?;
        checked_duration_from_secs_f64(self.mutation.timeout_seconds)
            .map_err(|error| format!("mutation.timeout_seconds {error}"))?;
        if self.mutation.max_mutants == Some(0) {
            return Err("mutation.max_mutants must be greater than zero when set".to_string());
        }
        for (name, command) in [
            ("mutation.test_command", self.mutation.test_command.as_deref()),
            (
                "mutation.validation_command",
                self.mutation.validation_command.as_deref(),
            ),
        ] {
            if command.is_some_and(|value| value.trim().is_empty()) {
                return Err(format!("{name} must not be empty when set"));
            }
        }
        Ok(())
    }
}

fn discover_config_path(root: &Path) -> Result<Option<PathBuf>, CoreError> {
    for name in ["reporigor.toml", ".reporigor.toml"] {
        let candidate = root.join(name);
        match fs::symlink_metadata(&candidate) {
            Ok(_) => return Ok(Some(candidate)),
            Err(source) if source.kind() == ErrorKind::NotFound => {}
            Err(source) => {
                return Err(CoreError::Read {
                    path: candidate.display().to_string(),
                    source,
                });
            }
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CrapConfig {
    pub fail_over: f64,
    pub allow_missing_coverage: bool,
    pub allow_empty: bool,
}

impl Default for CrapConfig {
    fn default() -> Self {
        Self {
            fail_over: 6.0,
            allow_missing_coverage: false,
            allow_empty: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DryConfig {
    pub min_tokens: usize,
    pub max_groups: usize,
    pub max_occurrences_per_window: usize,
    pub max_total_windows: usize,
    pub max_fingerprint_buckets: usize,
    pub max_candidate_work: usize,
    pub fail: bool,
}

impl Default for DryConfig {
    fn default() -> Self {
        Self {
            min_tokens: 30,
            max_groups: 50,
            max_occurrences_per_window: 100,
            max_total_windows: DRY_DEFAULT_MAX_TOTAL_WINDOWS,
            max_fingerprint_buckets: DRY_DEFAULT_MAX_FINGERPRINT_BUCKETS,
            max_candidate_work: DRY_DEFAULT_MAX_CANDIDATE_WORK,
            fail: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MutationConfig {
    pub timeout_seconds: f64,
    pub max_mutants: Option<usize>,
    pub test_command: Option<String>,
    pub validation_command: Option<String>,
}

impl Default for MutationConfig {
    fn default() -> Self {
        Self {
            timeout_seconds: 120.0,
            max_mutants: None,
            test_command: None,
            validation_command: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::RepoRigorConfig;
    use crate::{CoreError, PROJECT_METADATA_MAX_BYTES};

    #[test]
    fn defaults_are_valid_and_match_the_documented_timeout() {
        let config = RepoRigorConfig::default();
        assert!(config.validate().is_ok());
        assert!((config.mutation.timeout_seconds - 120.0).abs() < f64::EPSILON);
    }

    #[test]
    fn invalid_limits_and_blank_commands_are_rejected() {
        let mut config = RepoRigorConfig::default();
        config.dry.min_tokens = 3;
        assert!(config.validate().is_err());

        config = RepoRigorConfig::default();
        config.mutation.test_command = Some("  ".to_string());
        assert!(config.validate().is_err());

        config = RepoRigorConfig::default();
        config.max_source_bytes = crate::MAX_SOURCE_BYTES_HARD_LIMIT + 1;
        let Err(message) = config.validate() else {
            panic!("unsafe source limit was unexpectedly accepted");
        };
        assert!(message.contains("immutable"));
    }

    #[test]
    fn dry_repository_budgets_cannot_disable_or_raise_hard_limits() {
        let mut config = RepoRigorConfig::default();
        config.dry.max_total_windows = 0;
        assert!(config.validate().is_err());

        config = RepoRigorConfig::default();
        config.dry.max_candidate_work = crate::DRY_HARD_MAX_CANDIDATE_WORK.saturating_add(1);
        let Err(message) = config.validate() else {
            panic!("unsafe DRY work budget was unexpectedly accepted");
        };
        assert!(message.contains("immutable"));
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
        let Err(error) = toml::from_str::<RepoRigorConfig>("surprise = true") else {
            panic!("unknown keys must be actionable instead of silently ignored");
        };
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn auto_configuration_rejects_sparse_oversized_files() {
        let root = TempDir::new().unwrap_or_else(|error| panic!("fixture: {error}"));
        let path = root.path().join("reporigor.toml");
        let file = fs::File::create(&path).unwrap_or_else(|error| panic!("config: {error}"));
        file.set_len(PROJECT_METADATA_MAX_BYTES + 1)
            .unwrap_or_else(|error| panic!("sparse length: {error}"));

        let Err(error) = RepoRigorConfig::discover(root.path(), None) else {
            panic!("oversized auto configuration must be rejected");
        };
        assert!(matches!(error, CoreError::FileTooLarge { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn auto_configuration_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap_or_else(|error| panic!("root: {error}"));
        let outside = TempDir::new().unwrap_or_else(|error| panic!("outside: {error}"));
        let target = outside.path().join("outside.toml");
        fs::write(&target, "include_tests = true\n").unwrap_or_else(|error| panic!("target: {error}"));
        symlink(&target, root.path().join("reporigor.toml"))
            .unwrap_or_else(|error| panic!("symlink: {error}"));

        let Err(error) = RepoRigorConfig::discover(root.path(), None) else {
            panic!("escaping auto configuration must be rejected");
        };
        assert!(matches!(error, CoreError::UnsafePath { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_configuration_outside_root_remains_user_selectable() {
        let root = TempDir::new().unwrap_or_else(|error| panic!("root: {error}"));
        let outside = TempDir::new().unwrap_or_else(|error| panic!("outside: {error}"));
        let target = outside.path().join("selected.toml");
        fs::write(&target, "include_tests = true\n").unwrap_or_else(|error| panic!("config: {error}"));

        let (config, selected) = RepoRigorConfig::discover(root.path(), Some(&target))
            .unwrap_or_else(|error| panic!("explicit config: {error}"));
        assert!(config.include_tests);
        assert_eq!(selected.as_deref(), Some(target.as_path()));
    }
}
