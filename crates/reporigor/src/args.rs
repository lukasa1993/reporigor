use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use reporigor_core::{checked_duration_from_secs_f64, Language};

#[derive(Debug, Parser)]
#[command(name = "reporigor", version, about, propagate_version = true)]
// Clap flag structs intentionally model independent switches as booleans.
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    /// Configuration file. Defaults to reporigor.toml or .reporigor.toml at the project root.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Restrict analysis to one or more languages.
    #[arg(long, global = true, value_delimiter = ',')]
    pub language: Vec<Language>,

    /// Select project-aware, generic, or automatic backend routing.
    #[arg(long, global = true, value_enum, default_value_t = BackendArg::Auto)]
    pub backend: BackendArg,

    /// Permit analysis backends to execute existing project toolchains.
    #[arg(long, global = true)]
    pub allow_project_exec: bool,

    /// Include test sources in static analysis and mutation discovery.
    #[arg(long, global = true)]
    pub include_tests: bool,

    /// Continue with valid subtrees while reporting syntax errors.
    #[arg(long, global = true)]
    pub allow_parse_errors: bool,

    /// Restrict source paths by case-sensitive substring. Multiple filters use OR logic.
    #[arg(long = "filter", global = true)]
    pub filters: Vec<String>,

    /// Cargo features used by the native Rust adapter.
    #[arg(long, global = true, value_delimiter = ',')]
    pub features: Vec<String>,

    /// Disable Cargo default features.
    #[arg(long, global = true)]
    pub no_default_features: bool,

    /// Enable every Cargo feature.
    #[arg(long, global = true, conflicts_with_all = ["features", "no_default_features"])]
    pub all_features: bool,

    /// Explicit Cargo executable for the native Rust adapter.
    #[arg(long, global = true)]
    pub cargo: Option<PathBuf>,

    /// Report format.
    #[arg(long, global = true, value_enum, default_value_t = FormatArg::Text)]
    pub format: FormatArg,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum BackendArg {
    Auto,
    Native,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    Text,
    Json,
    Sarif,
    MutationJson,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Calculate function complexity, coverage, and CRAP scores.
    Crap(CrapArgs),
    /// Find normalized duplicate token sequences.
    Dry(DryArgs),
    /// List or execute syntax-aware mutations.
    Mutate(MutateArgs),
    /// Run CRAP, duplication, and mutation discovery in one pass.
    Check(CheckArgs),
    /// Show language and external-provider availability.
    Providers(ProviderArgs),
}

#[derive(Debug, Args)]
pub struct CommonPath {
    /// Project root.
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

#[derive(Debug, Args)]
pub struct CrapArgs {
    #[command(flatten)]
    pub common: CommonPath,

    /// Existing LCOV, Cobertura, coverage.py, Istanbul, or LLVM export report.
    #[arg(long)]
    pub coverage: Option<PathBuf>,

    /// Quality gate; fail when any score is strictly greater than this value.
    #[arg(
        long,
        value_name = "SCORE",
        value_parser = parse_nonnegative_finite,
        allow_negative_numbers = true
    )]
    pub fail_over: Option<f64>,

    #[arg(long)]
    pub allow_missing_coverage: bool,

    #[arg(long)]
    pub allow_empty: bool,
}

#[derive(Debug, Args)]
pub struct DryArgs {
    #[command(flatten)]
    pub common: CommonPath,

    #[arg(long, value_parser = parse_min_tokens)]
    pub min_tokens: Option<usize>,

    #[arg(long, value_parser = parse_positive)]
    pub max_groups: Option<usize>,

    #[arg(long, value_parser = parse_occurrence_limit)]
    pub max_occurrences_per_window: Option<usize>,

    #[arg(long)]
    pub fail: bool,
}

#[derive(Debug, Args)]
// These are independent safety/exit-policy switches exposed by the CLI.
#[allow(clippy::struct_excessive_bools)]
pub struct MutateArgs {
    #[command(flatten)]
    pub common: CommonPath,

    /// Explicitly restore a source left by an interrupted mutation and exit.
    #[arg(
        long,
        conflicts_with_all = [
            "list",
            "run",
            "test_command",
            "validation_command",
            "no_validate",
            "timeout",
            "max_mutants",
            "skip_baseline",
            "allow_survivors",
            "allow_compile_errors"
        ]
    )]
    pub recover: bool,

    /// List mutation candidates without changing or testing source files.
    #[arg(long, conflicts_with = "run")]
    pub list: bool,

    /// Execute selected mutants. Without this flag, mutation is read-only.
    #[arg(long, conflicts_with = "list")]
    pub run: bool,

    #[arg(long)]
    pub test_command: Option<String>,

    #[arg(long = "validate-command")]
    pub validation_command: Option<String>,

    /// Disable mutation validation, including a configured validation command.
    #[arg(long, conflicts_with = "validation_command")]
    pub no_validate: bool,

    #[arg(long, value_parser = parse_positive_duration)]
    pub timeout: Option<Duration>,

    #[arg(long, value_parser = parse_positive)]
    pub max_mutants: Option<usize>,

    #[arg(long)]
    pub skip_baseline: bool,

    #[arg(long)]
    pub allow_survivors: bool,

    #[arg(long)]
    pub allow_compile_errors: bool,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    #[command(flatten)]
    pub common: CommonPath,

    #[arg(long)]
    pub coverage: Option<PathBuf>,

    #[arg(
        long,
        value_parser = parse_nonnegative_finite,
        allow_negative_numbers = true
    )]
    pub fail_over: Option<f64>,

    #[arg(long, value_parser = parse_min_tokens)]
    pub min_tokens: Option<usize>,

    /// Execute mutations as part of check. The default only inventories them.
    #[arg(long)]
    pub run_mutations: bool,

    #[arg(long)]
    pub test_command: Option<String>,
}

#[derive(Debug, Args)]
pub struct ProviderArgs {
    #[command(flatten)]
    pub common: CommonPath,

    /// Execute bounded version/configuration probes in addition to static discovery.
    #[arg(long)]
    pub preflight: bool,
}

fn parse_min_tokens(value: &str) -> Result<usize, String> {
    parse_at_least(value, 4, "min-tokens")
}

fn parse_positive(value: &str) -> Result<usize, String> {
    parse_at_least(value, 1, "value")
}

fn parse_occurrence_limit(value: &str) -> Result<usize, String> {
    parse_at_least(value, 2, "max-occurrences-per-window")
}

fn parse_at_least(value: &str, minimum: usize, name: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|error| format!("invalid {name} {value:?}: {error}"))?;
    if parsed < minimum {
        return Err(format!("{name} must be at least {minimum}"));
    }
    Ok(parsed)
}

pub(crate) fn parse_nonnegative_finite(value: &str) -> Result<f64, String> {
    let parsed = value
        .parse::<f64>()
        .map_err(|error| format!("invalid number {value:?}: {error}"))?;
    if !parsed.is_finite() || parsed < 0.0 {
        return Err("value must be a non-negative finite number".to_string());
    }
    Ok(parsed)
}

pub(crate) fn parse_positive_duration(value: &str) -> Result<Duration, String> {
    let parsed = value
        .parse::<f64>()
        .map_err(|error| format!("invalid number {value:?}: {error}"))?;
    checked_duration_from_secs_f64(parsed).map_err(|error| format!("timeout {error}"))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Cli;

    #[test]
    fn mutation_mode_and_limits_are_validated_during_argument_parsing() {
        assert!(Cli::try_parse_from(["reporigor", "mutate", ".", "--list", "--run"]).is_err());
        assert!(Cli::try_parse_from(["reporigor", "mutate", ".", "--recover"]).is_ok());
        assert!(
            Cli::try_parse_from(["reporigor", "mutate", ".", "--recover", "--test-command", "true",])
                .is_err()
        );
        assert!(Cli::try_parse_from(["reporigor", "mutate", ".", "--run", "--max-mutants", "0"]).is_err());
        for timeout in ["NaN", "0", "1e300", "5e-324"] {
            assert!(
                Cli::try_parse_from(["reporigor", "mutate", ".", "--timeout", timeout]).is_err(),
                "{timeout} must fail during argument parsing"
            );
        }
    }

    #[test]
    fn crap_threshold_must_be_finite_and_nonnegative() {
        assert!(Cli::try_parse_from(["reporigor", "crap", ".", "--fail-over", "NaN"]).is_err());
        assert!(Cli::try_parse_from(["reporigor", "check", ".", "--fail-over=-1"]).is_err());
        assert!(Cli::try_parse_from(["reporigor", "crap", ".", "--fail-over", "0"]).is_ok());
    }

    #[test]
    fn no_validate_overrides_only_the_validation_command() {
        assert!(Cli::try_parse_from([
            "reporigor",
            "mutate",
            ".",
            "--no-validate",
            "--validate-command",
            "cargo check",
        ])
        .is_err());
        assert!(Cli::try_parse_from(["reporigor", "mutate", ".", "--no-validate"]).is_ok());
    }

    #[test]
    fn project_execution_requires_an_explicit_global_flag() {
        let default = Cli::try_parse_from(["reporigor", "check", "."])
            .unwrap_or_else(|error| panic!("default CLI: {error}"));
        assert!(!default.allow_project_exec);

        let allowed = Cli::try_parse_from(["reporigor", "check", ".", "--allow-project-exec"])
            .unwrap_or_else(|error| panic!("explicit project execution: {error}"));
        assert!(allowed.allow_project_exec);
    }
}
