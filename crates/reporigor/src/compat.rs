use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use analysis_crap::discover_coverage_report;
use analysis_mutate::{run_command, CommandSpec, MutationError};
use anyhow::{anyhow, bail, Context, Result};
use clap::{error::ErrorKind, CommandFactory, FromArgMatches, Parser};
use reporigor_core::{read_optional_bounded_utf8_file_within, Language, PROJECT_METADATA_MAX_BYTES};
use reporigor_reporting::escape_terminal_text;

use crate::args::{
    parse_nonnegative_finite, parse_positive_duration, BackendArg, CommonPath, CrapArgs, DryArgs, FormatArg,
    MutateArgs,
};
use crate::{run, Cli, Command};

const COMMAND_OUTPUT_LIMIT: usize = 1024 * 1024;
const LEGACY_CRAP_TIMEOUT: &str = "1800";
const LEGACY_MUTATE_TIMEOUT: &str = "120";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Crap,
    Dry,
    Mutate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LegacyCommand {
    alias: &'static str,
    family: Family,
    language: Language,
}

impl LegacyCommand {
    fn parse(value: &OsStr) -> Option<Self> {
        let basename = Path::new(value).file_name()?.to_str()?;
        let basename = basename.strip_suffix(".exe").unwrap_or(basename);
        let (family, suffix) = if let Some(suffix) = basename.strip_prefix("crap4") {
            (Family::Crap, suffix)
        } else if let Some(suffix) = basename.strip_prefix("dry4") {
            (Family::Dry, suffix)
        } else if let Some(suffix) = basename.strip_prefix("mutate4") {
            (Family::Mutate, suffix)
        } else {
            return None;
        };
        let language = match suffix {
            "bash" => Language::Bash,
            "c" => Language::C,
            "cpp" => Language::Cpp,
            "objc" => Language::ObjectiveC,
            "python" => Language::Python,
            "rust" => Language::Rust,
            "swift" => Language::Swift,
            "ts" => Language::TypeScript,
            _ => return None,
        };
        Some(Self {
            alias: alias_for(family, language),
            family,
            language,
        })
    }
}

const fn alias_for(family: Family, language: Language) -> &'static str {
    match (family, language) {
        (Family::Crap, Language::Bash) => "crap4bash",
        (Family::Crap, Language::C) => "crap4c",
        (Family::Crap, Language::Cpp) => "crap4cpp",
        (Family::Crap, Language::ObjectiveC) => "crap4objc",
        (Family::Crap, Language::Python) => "crap4python",
        (Family::Crap, Language::Rust) => "crap4rust",
        (Family::Crap, Language::Swift) => "crap4swift",
        (Family::Crap, Language::TypeScript) => "crap4ts",
        (Family::Dry, Language::Bash) => "dry4bash",
        (Family::Dry, Language::C) => "dry4c",
        (Family::Dry, Language::Cpp) => "dry4cpp",
        (Family::Dry, Language::ObjectiveC) => "dry4objc",
        (Family::Dry, Language::Python) => "dry4python",
        (Family::Dry, Language::Rust) => "dry4rust",
        (Family::Dry, Language::Swift) => "dry4swift",
        (Family::Dry, Language::TypeScript) => "dry4ts",
        (Family::Mutate, Language::Bash) => "mutate4bash",
        (Family::Mutate, Language::C) => "mutate4c",
        (Family::Mutate, Language::Cpp) => "mutate4cpp",
        (Family::Mutate, Language::ObjectiveC) => "mutate4objc",
        (Family::Mutate, Language::Python) => "mutate4python",
        (Family::Mutate, Language::Rust) => "mutate4rust",
        (Family::Mutate, Language::Swift) => "mutate4swift",
        (Family::Mutate, Language::TypeScript) => "mutate4ts",
    }
}

#[derive(Debug)]
struct CoveragePreparation {
    root: PathBuf,
    requested: PathBuf,
    command: Option<String>,
    missing_command_hint: Option<&'static str>,
    timeout: Duration,
}

#[derive(Debug)]
struct LegacyPlan {
    alias: &'static str,
    cli: Cli,
    coverage: Option<CoveragePreparation>,
    warnings: Vec<String>,
}

#[derive(Debug, Parser)]
#[command(
    name = "legacy-crap",
    version,
    about = "Compatibility interface for crap4* commands"
)]
#[allow(clippy::struct_excessive_bools)]
struct LegacyCrapArgs {
    #[arg(value_name = "PATH_FRAGMENT")]
    filters: Vec<String>,

    #[arg(long, default_value = ".")]
    root: PathBuf,

    #[arg(long, value_delimiter = ',', conflicts_with = "all_features")]
    features: Vec<String>,

    #[arg(long, conflicts_with = "all_features")]
    no_default_features: bool,

    #[arg(long)]
    all_features: bool,

    #[arg(long)]
    coverage: Option<PathBuf>,

    #[arg(long)]
    test_command: Option<String>,

    #[arg(long, default_value = LEGACY_CRAP_TIMEOUT, value_parser = parse_positive_duration)]
    timeout: Duration,

    #[arg(long)]
    no_test: bool,

    #[arg(long)]
    allow_missing_coverage: bool,

    #[arg(long)]
    allow_empty: bool,

    #[arg(long)]
    allow_parse_errors: bool,

    #[arg(long)]
    include_tests: bool,

    #[arg(long = "json")]
    json_output: bool,

    #[arg(
        long,
        value_name = "SCORE",
        value_parser = parse_nonnegative_finite,
        allow_negative_numbers = true
    )]
    fail_over: Option<f64>,
}

#[derive(Debug, Parser)]
#[command(
    name = "legacy-dry",
    version,
    about = "Compatibility interface for dry4* commands"
)]
#[allow(clippy::struct_excessive_bools)]
struct LegacyDryArgs {
    #[arg(value_name = "PATH_FRAGMENT")]
    filters: Vec<String>,

    #[arg(long, default_value = ".")]
    root: PathBuf,

    #[arg(long, value_delimiter = ',', conflicts_with = "all_features")]
    features: Vec<String>,

    #[arg(long, conflicts_with = "all_features")]
    no_default_features: bool,

    #[arg(long)]
    all_features: bool,

    #[arg(long, default_value_t = 30, value_parser = parse_min_tokens)]
    min_tokens: usize,

    #[arg(long, default_value_t = 50, value_parser = parse_positive_usize)]
    max_groups: usize,

    #[arg(long, default_value_t = 100, value_parser = parse_occurrence_limit)]
    max_occurrences_per_window: usize,

    #[arg(long)]
    include_tests: bool,

    #[arg(long = "json")]
    json_output: bool,

    #[arg(long)]
    fail: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = "legacy-mutate",
    version,
    about = "Compatibility interface for mutate4* commands"
)]
#[allow(clippy::struct_excessive_bools)]
struct LegacyMutateArgs {
    #[arg(value_name = "PATH_FRAGMENT")]
    filters: Vec<String>,

    #[arg(long, default_value = ".")]
    root: PathBuf,

    #[arg(long, value_delimiter = ',', conflicts_with = "all_features")]
    features: Vec<String>,

    #[arg(long, conflicts_with = "all_features")]
    no_default_features: bool,

    #[arg(long)]
    all_features: bool,

    #[arg(long)]
    test_command: Option<String>,

    #[arg(long)]
    validate_command: Option<String>,

    #[arg(long)]
    no_validate: bool,

    #[arg(long, default_value = LEGACY_MUTATE_TIMEOUT, value_parser = parse_positive_duration)]
    timeout: Duration,

    #[arg(long, value_parser = parse_positive_usize)]
    max_mutants: Option<usize>,

    #[arg(long)]
    list: bool,

    #[arg(long)]
    skip_baseline: bool,

    #[arg(long)]
    include_tests: bool,

    #[arg(long = "manifest", visible_alias = "report")]
    report: Option<PathBuf>,

    #[arg(long = "json")]
    json_output: bool,

    #[arg(long)]
    fail_on_survivors: bool,

    #[arg(long)]
    allow_survivors: bool,

    #[arg(long)]
    allow_compile_errors: bool,

    #[arg(long)]
    verbose: bool,

    #[arg(long)]
    scan: bool,

    #[arg(long)]
    update_manifest: bool,

    #[arg(long)]
    since_last_run: bool,

    #[arg(long)]
    mutate_all: bool,
}

/// Detect a legacy multicall name and execute it through the unified engine.
///
/// A direct `crap4python ...` invocation is detected from `argv[0]`. For
/// debugging and migration automation, `reporigor crap4python ...` is also
/// accepted. `None` means the normal unified CLI should parse the arguments.
#[must_use]
pub fn entry_from_env() -> Option<ExitCode> {
    let arguments: Vec<OsString> = env::args_os().collect();
    let executable = arguments.first()?;
    if let Some(command) = LegacyCommand::parse(executable) {
        return Some(execute(command, &arguments[1..]));
    }
    let command = arguments.get(1).and_then(|value| LegacyCommand::parse(value))?;
    Some(execute(command, &arguments[2..]))
}

fn execute(command: LegacyCommand, arguments: &[OsString]) -> ExitCode {
    let plan = match translate(command, arguments) {
        Ok(plan) => plan,
        Err(error) => {
            let exit = error.exit_code();
            let _ = error.print();
            return exit_code(exit);
        }
    };
    match execute_plan(plan) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            write_legacy_error(&format!("{}: {error:#}", command.alias));
            ExitCode::FAILURE
        }
    }
}

fn translate(command: LegacyCommand, arguments: &[OsString]) -> std::result::Result<LegacyPlan, clap::Error> {
    match command.family {
        Family::Crap => parse_named::<LegacyCrapArgs>(command.alias, arguments)
            .map(|arguments| crap_plan(command, arguments)),
        Family::Dry => parse_named::<LegacyDryArgs>(command.alias, arguments)
            .map(|arguments| dry_plan(command, arguments)),
        Family::Mutate => parse_named::<LegacyMutateArgs>(command.alias, arguments)
            .and_then(|arguments| mutate_plan(command, arguments)),
    }
}

fn parse_named<T>(alias: &'static str, arguments: &[OsString]) -> std::result::Result<T, clap::Error>
where
    T: CommandFactory + FromArgMatches,
{
    let parser_arguments = std::iter::once(OsString::from(alias)).chain(arguments.iter().cloned());
    let matches = T::command().name(alias).try_get_matches_from(parser_arguments)?;
    T::from_arg_matches(&matches)
}

#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn common_cli(
    command: LegacyCommand,
    filters: Vec<String>,
    features: Vec<String>,
    no_default_features: bool,
    all_features: bool,
    include_tests: bool,
    allow_parse_errors: bool,
    json: bool,
    operation: Command,
) -> Cli {
    Cli {
        config: None,
        language: vec![command.language],
        backend: BackendArg::Auto,
        allow_project_exec: command.language == Language::Rust,
        include_tests,
        allow_parse_errors,
        filters,
        features,
        no_default_features,
        all_features,
        cargo: None,
        format: if json { FormatArg::Json } else { FormatArg::Text },
        command: operation,
    }
}

fn crap_plan(command: LegacyCommand, arguments: LegacyCrapArgs) -> LegacyPlan {
    let requested_coverage = arguments
        .coverage
        .unwrap_or_else(|| default_coverage(command.language));
    let coverage = if requested_coverage.is_absolute() {
        requested_coverage.clone()
    } else {
        arguments.root.join(&requested_coverage)
    };
    let preparation = (!arguments.no_test).then(|| CoveragePreparation {
        root: arguments.root.clone(),
        requested: requested_coverage,
        missing_command_hint: (command.language == Language::Swift).then_some(
            "SwiftPM's legacy --show-codecov-path produces LLVM profdata, not a loadable report; pass --test-command that writes LCOV/LLVM JSON, or use --no-test --coverage <report>",
        ),
        command: arguments
            .test_command
            .or_else(|| default_coverage_command(command.language).map(str::to_owned)),
        timeout: arguments.timeout,
    });
    let operation = Command::Crap(CrapArgs {
        common: CommonPath { path: arguments.root },
        coverage: Some(coverage),
        // Legacy commands only enabled the CRAP quality gate when this flag
        // was supplied. MAX preserves that behavior through the unified gate.
        fail_over: Some(arguments.fail_over.unwrap_or(f64::MAX)),
        allow_missing_coverage: arguments.allow_missing_coverage,
        allow_empty: arguments.allow_empty,
    });
    LegacyPlan {
        alias: command.alias,
        cli: common_cli(
            command,
            arguments.filters,
            arguments.features,
            arguments.no_default_features,
            arguments.all_features,
            arguments.include_tests,
            arguments.allow_parse_errors,
            arguments.json_output,
            operation,
        ),
        coverage: preparation,
        warnings: Vec::new(),
    }
}

fn dry_plan(command: LegacyCommand, arguments: LegacyDryArgs) -> LegacyPlan {
    let operation = Command::Dry(DryArgs {
        common: CommonPath { path: arguments.root },
        min_tokens: Some(arguments.min_tokens),
        max_groups: Some(arguments.max_groups),
        max_occurrences_per_window: Some(arguments.max_occurrences_per_window),
        fail: arguments.fail,
    });
    LegacyPlan {
        alias: command.alias,
        cli: common_cli(
            command,
            arguments.filters,
            arguments.features,
            arguments.no_default_features,
            arguments.all_features,
            arguments.include_tests,
            false,
            arguments.json_output,
            operation,
        ),
        coverage: None,
        warnings: Vec::new(),
    }
}

fn mutate_plan(
    command: LegacyCommand,
    arguments: LegacyMutateArgs,
) -> std::result::Result<LegacyPlan, clap::Error> {
    if arguments.scan || arguments.update_manifest || arguments.since_last_run || arguments.mutate_all {
        return Err(clap::Error::raw(
            ErrorKind::InvalidValue,
            "--scan, --update-manifest, --since-last-run, and --mutate-all use the old Rust manifest protocol and cannot be translated; use `reporigor mutate --list` or `reporigor mutate --run`",
        ));
    }
    let root = arguments.root;
    let execute = !arguments.list;
    let test_command = if execute {
        arguments
            .test_command
            .or_else(|| detect_test_command(command.language, &root))
    } else {
        arguments.test_command
    };
    let validation_command = if arguments.no_validate {
        None
    } else {
        arguments
            .validate_command
            .or_else(|| detect_validation_command(command.language, &root))
    };
    let mut warnings = Vec::new();
    if arguments.report.is_some() {
        warnings.push(
            "--manifest/--report is accepted for migration but unified reports are written to stdout; redirect stdout or use --json"
                .to_string(),
        );
    }
    if arguments.verbose {
        warnings
            .push("--verbose is no longer needed; operational diagnostics already use stderr".to_string());
    }
    if arguments.fail_on_survivors {
        warnings.push("--fail-on-survivors is now the default".to_string());
    }
    if command.language == Language::Rust && execute {
        warnings.push(
            "the unified engine does not use mutate4rust's embedded differential manifest; this run considers every filtered mutation"
                .to_string(),
        );
    }
    let operation = Command::Mutate(MutateArgs {
        common: CommonPath { path: root },
        recover: false,
        list: arguments.list,
        run: execute,
        test_command,
        validation_command,
        no_validate: arguments.no_validate,
        timeout: Some(arguments.timeout),
        max_mutants: arguments.max_mutants,
        skip_baseline: arguments.skip_baseline,
        allow_survivors: arguments.allow_survivors,
        allow_compile_errors: arguments.allow_compile_errors,
    });
    Ok(LegacyPlan {
        alias: command.alias,
        cli: common_cli(
            command,
            arguments.filters,
            arguments.features,
            arguments.no_default_features,
            arguments.all_features,
            arguments.include_tests,
            false,
            arguments.json_output,
            operation,
        ),
        coverage: None,
        warnings,
    })
}

fn execute_plan(mut plan: LegacyPlan) -> Result<u8> {
    for warning in &plan.warnings {
        write_legacy_error(&format!("{}: compatibility warning: {warning}", plan.alias));
    }
    if let Some(preparation) = plan.coverage.take() {
        let read_session = analysis_mutate::MutationReadSession::begin(&preparation.root)?;
        crate::refuse_pending_mutation(&read_session)?;
        let report = prepare_coverage(&preparation)?;
        if let Command::Crap(arguments) = &mut plan.cli.command {
            arguments.coverage = Some(report);
        }
        return crate::run_with_read_session(plan.cli, Some(read_session));
    }
    run(plan.cli)
}

fn write_legacy_error(message: &str) {
    let _result = writeln!(io::stderr().lock(), "{}", escape_terminal_text(message));
}

fn prepare_coverage(preparation: &CoveragePreparation) -> Result<PathBuf> {
    let root = preparation
        .root
        .canonicalize()
        .with_context(|| format!("cannot resolve project root {}", preparation.root.display()))?;
    let command = preparation
        .command
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{}",
                preparation
                    .missing_command_hint
                    .unwrap_or("--test-command is required for this language unless --no-test is used",)
            )
        })?;
    let started = SystemTime::now();
    let cancellation = crate::signals::process_cancellation_token();
    let outcome = {
        let _signal_scope = crate::signals::cooperative_cancellation_scope();
        run_command(
            &CommandSpec::shell(command),
            &root,
            preparation.timeout,
            COMMAND_OUTPUT_LIMIT,
            &cancellation,
        )?
    };
    if cancellation.is_cancelled() {
        return Err(MutationError::Cancelled.into());
    }
    if outcome.timed_out {
        bail!(
            "coverage test command timed out after {:.3}s",
            outcome.duration_seconds
        );
    }
    if !outcome.success() {
        bail!(
            "coverage test command exited with {:?}\n{}",
            outcome.exit_code,
            outcome.output.trim()
        );
    }
    let requested = if preparation.requested.is_absolute() {
        preparation.requested.clone()
    } else {
        root.join(&preparation.requested)
    };
    let report = discover_coverage_report(&requested).or_else(|error| {
        coverage_path_from_output(&outcome.output, &root)
            .filter(|path| path.is_file())
            .ok_or(error)
    })?;
    if let Ok(modified) = report.metadata().and_then(|metadata| metadata.modified()) {
        let freshness_grace = Duration::from_secs(1);
        if modified + freshness_grace < started {
            bail!("coverage report is stale: {}", report.display());
        }
    }
    Ok(report)
}

fn coverage_path_from_output(output: &str, root: &Path) -> Option<PathBuf> {
    output
        .split_whitespace()
        .rev()
        .map(|token| token.trim_matches(|character: char| "'\"[](),".contains(character)))
        .filter(|token| {
            Path::new(token)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        })
        .map(PathBuf::from)
        .map(|path| if path.is_absolute() { path } else { root.join(path) })
        .find(|path| path.is_file())
}

fn default_coverage(language: Language) -> PathBuf {
    let path = match language {
        Language::Bash => "target/coverage",
        Language::C | Language::Cpp | Language::ObjectiveC | Language::Rust => "target/coverage/lcov.info",
        Language::Python | Language::Swift => "target/coverage/coverage.json",
        Language::TypeScript => "target/coverage/coverage-final.json",
    };
    PathBuf::from(path)
}

const fn default_coverage_command(language: Language) -> Option<&'static str> {
    match language {
        Language::Bash => Some("kcov target/coverage bats tests"),
        Language::C | Language::Cpp | Language::ObjectiveC | Language::Swift => None,
        Language::Python => {
            Some("coverage run -m pytest && coverage json -o target/coverage/coverage.json")
        }
        Language::Rust => {
            Some("cargo llvm-cov --workspace --lcov --output-path target/coverage/lcov.info")
        }
        Language::TypeScript => Some(
            "npx --no-install vitest run --coverage --coverage.reporter=json --coverage.reportsDirectory=target/coverage",
        ),
    }
}

fn detect_test_command(language: Language, root: &Path) -> Option<String> {
    match language {
        Language::Bash => Some("bats tests".to_string()),
        Language::Python => Some("python -m pytest -q".to_string()),
        Language::Rust => Some("cargo test --workspace".to_string()),
        Language::Swift => Some("swift test".to_string()),
        Language::TypeScript => Some("npm test".to_string()),
        Language::C | Language::Cpp | Language::ObjectiveC => detect_c_family_test(root),
    }
}

fn detect_c_family_test(root: &Path) -> Option<String> {
    if root.join("build/CTestTestfile.cmake").is_file() {
        return Some("ctest --test-dir build --output-on-failure".to_string());
    }
    has_make_target(root, &root.join("Makefile"), "test").then(|| "make test".to_string())
}

fn detect_validation_command(language: Language, root: &Path) -> Option<String> {
    match language {
        Language::Rust => Some("cargo check --workspace".to_string()),
        Language::Swift => Some("swift build".to_string()),
        // Avoid the legacy `npx tsc` default because npx may download code.
        // Project-local tsc is safe to use when it is already installed.
        Language::TypeScript if root.join("node_modules/.bin/tsc").is_file() => {
            Some("./node_modules/.bin/tsc --noEmit".to_string())
        }
        Language::C | Language::Cpp | Language::ObjectiveC => detect_c_family_validation(root),
        Language::Bash | Language::Python | Language::TypeScript => None,
    }
}

fn detect_c_family_validation(root: &Path) -> Option<String> {
    if root.join("build/build.ninja").is_file() {
        Some("ninja -C build".to_string())
    } else if root.join("build/CMakeCache.txt").is_file() {
        Some("cmake --build build".to_string())
    } else if root.join("Makefile").is_file() {
        Some("make -s".to_string())
    } else {
        None
    }
}

fn has_make_target(root: &Path, path: &Path, target: &str) -> bool {
    read_optional_bounded_utf8_file_within(root, path, PROJECT_METADATA_MAX_BYTES)
        .ok()
        .flatten()
        .is_some_and(|contents| {
            contents.lines().any(|line| {
                line.split_once(':')
                    .is_some_and(|(candidate, _)| candidate.trim() == target)
            })
        })
}

fn parse_min_tokens(value: &str) -> std::result::Result<usize, String> {
    parse_at_least(value, 4, "min-tokens")
}

fn parse_positive_usize(value: &str) -> std::result::Result<usize, String> {
    parse_at_least(value, 1, "value")
}

fn parse_occurrence_limit(value: &str) -> std::result::Result<usize, String> {
    parse_at_least(value, 2, "max-occurrences-per-window")
}

fn parse_at_least(value: &str, minimum: usize, name: &str) -> std::result::Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|error| format!("invalid {name} {value:?}: {error}"))?;
    if parsed < minimum {
        return Err(format!("{name} must be at least {minimum}"));
    }
    Ok(parsed)
}

fn exit_code(value: i32) -> ExitCode {
    u8::try_from(value).map_or(ExitCode::FAILURE, ExitCode::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate_strings(alias: &str, arguments: &[&str]) -> std::result::Result<LegacyPlan, clap::Error> {
        let command = LegacyCommand::parse(OsStr::new(alias)).ok_or_else(|| {
            clap::Error::raw(ErrorKind::InvalidValue, format!("unknown legacy alias {alias}"))
        })?;
        let arguments = arguments.iter().map(OsString::from).collect::<Vec<_>>();
        translate(command, &arguments)
    }

    #[test]
    fn recognizes_exactly_the_supported_alias_shapes() {
        for family in ["crap", "dry", "mutate"] {
            for language in ["bash", "c", "cpp", "objc", "python", "rust", "swift", "ts"] {
                assert!(LegacyCommand::parse(OsStr::new(&format!("{family}4{language}"))).is_some());
            }
        }
        assert!(LegacyCommand::parse(OsStr::new("crap4javascript")).is_none());
        assert!(LegacyCommand::parse(OsStr::new("reporigor")).is_none());
    }

    #[test]
    fn crap_python_translates_filters_coverage_and_global_flags() -> Result<(), clap::Error> {
        let plan = translate_strings(
            "crap4python",
            &[
                "pkg/one",
                "pkg/two",
                "--root",
                "project",
                "--coverage",
                "coverage.json",
                "--no-test",
                "--allow-parse-errors",
                "--json",
                "--fail-over",
                "9.5",
            ],
        )?;
        assert_eq!(plan.cli.language, vec![Language::Python]);
        assert_eq!(plan.cli.filters, ["pkg/one", "pkg/two"]);
        assert!(plan.cli.allow_parse_errors);
        assert!(!plan.cli.allow_project_exec);
        assert_eq!(plan.cli.format, FormatArg::Json);
        assert!(plan.coverage.is_none());
        let Command::Crap(arguments) = plan.cli.command else {
            panic!("expected CRAP command");
        };
        assert_eq!(arguments.common.path, Path::new("project"));
        assert_eq!(
            arguments.coverage.as_deref(),
            Some(Path::new("project/coverage.json"))
        );
        assert_eq!(arguments.fail_over, Some(9.5));
        Ok(())
    }

    #[test]
    fn dry_rust_preserves_feature_and_gate_options() -> Result<(), clap::Error> {
        let plan = translate_strings(
            "dry4rust",
            &[
                "src/parser",
                "--features",
                "serde,cli",
                "--no-default-features",
                "--min-tokens",
                "16",
                "--max-groups",
                "7",
                "--fail",
            ],
        )?;
        assert_eq!(plan.cli.language, vec![Language::Rust]);
        assert_eq!(plan.cli.features, ["serde", "cli"]);
        assert!(plan.cli.no_default_features);
        assert!(plan.cli.allow_project_exec);
        let Command::Dry(arguments) = plan.cli.command else {
            panic!("expected DRY command");
        };
        assert_eq!(arguments.min_tokens, Some(16));
        assert_eq!(arguments.max_groups, Some(7));
        assert!(arguments.fail);
        Ok(())
    }

    #[test]
    fn mutate_swift_defaults_to_execution_and_old_commands() -> Result<(), clap::Error> {
        let plan = translate_strings("mutate4swift", &["Sources", "--max-mutants", "4"])?;
        assert_eq!(plan.cli.language, vec![Language::Swift]);
        let Command::Mutate(arguments) = plan.cli.command else {
            panic!("expected mutation command");
        };
        assert!(arguments.run);
        assert!(!arguments.list);
        assert_eq!(arguments.test_command.as_deref(), Some("swift test"));
        assert_eq!(arguments.validation_command.as_deref(), Some("swift build"));
        assert_eq!(arguments.max_mutants, Some(4));
        Ok(())
    }

    #[test]
    fn mutate_list_is_read_only_and_needs_no_detected_test_command() -> Result<(), clap::Error> {
        let plan = translate_strings("mutate4c", &["--root", "missing", "--list", "--json"])?;
        let Command::Mutate(arguments) = plan.cli.command else {
            panic!("expected mutation command");
        };
        assert!(!arguments.run);
        assert!(arguments.list);
        assert!(arguments.test_command.is_none());
        assert_eq!(plan.cli.format, FormatArg::Json);
        Ok(())
    }

    #[test]
    fn mutate_no_validate_is_preserved_as_an_explicit_override() -> Result<(), clap::Error> {
        let plan = translate_strings("mutate4python", &["--no-validate"])?;
        let Command::Mutate(arguments) = plan.cli.command else {
            panic!("expected mutation command");
        };
        assert!(arguments.no_validate);
        assert!(arguments.validation_command.is_none());
        Ok(())
    }

    #[test]
    fn rust_manifest_protocol_modes_fail_loudly() {
        let Err(error) = translate_strings("mutate4rust", &["--scan"]) else {
            panic!("scan must be rejected");
        };
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
        assert!(error.to_string().contains("old Rust manifest protocol"));
    }

    #[test]
    fn compatibility_numeric_validation_matches_unified_limits() {
        assert!(translate_strings("dry4python", &["--min-tokens", "3"]).is_err());
        assert!(translate_strings("mutate4python", &["--max-mutants", "0"]).is_err());
        for alias in ["crap4python", "mutate4python"] {
            for timeout in ["NaN", "0", "1e300", "5e-324"] {
                assert!(
                    translate_strings(alias, &["--timeout", timeout]).is_err(),
                    "{alias} must reject timeout {timeout}"
                );
            }
        }
    }

    #[test]
    fn every_crap_alias_rejects_invalid_thresholds_during_argument_parsing() {
        for language in ["bash", "c", "cpp", "objc", "python", "rust", "swift", "ts"] {
            let alias = format!("crap4{language}");
            for invalid in ["-1", "NaN", "inf"] {
                let Err(error) = translate_strings(&alias, &["--fail-over", invalid]) else {
                    panic!("{alias} accepted invalid CRAP threshold {invalid}");
                };
                assert_eq!(error.kind(), ErrorKind::ValueValidation, "{alias}: {invalid}");
                assert!(
                    error
                        .to_string()
                        .contains("value must be a non-negative finite number"),
                    "{alias}: {invalid}: {error}"
                );
            }
        }
    }

    #[test]
    fn help_uses_the_invoked_multicall_name() {
        let Err(error) = translate_strings("crap4objc", &["--help"]) else {
            panic!("help must exit parsing");
        };
        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        assert!(error.to_string().contains("Usage: crap4objc"));
    }

    #[test]
    fn swift_implicit_coverage_command_is_rejected_before_execution() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        let preparation = CoveragePreparation {
            root: directory.path().to_path_buf(),
            requested: PathBuf::from("target/coverage/coverage.json"),
            command: default_coverage_command(Language::Swift).map(str::to_owned),
            missing_command_hint: Some(
                "SwiftPM's legacy --show-codecov-path produces LLVM profdata, not a loadable report",
            ),
            timeout: Duration::from_secs(1),
        };
        let Err(error) = prepare_coverage(&preparation) else {
            panic!("implicit Swift coverage must be rejected");
        };
        assert!(error.to_string().contains("LLVM profdata"));
        Ok(())
    }

    #[test]
    fn oversized_makefile_never_autodetects_a_test_command() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let makefile = std::fs::File::create(directory.path().join("Makefile"))?;
        makefile.set_len(PROJECT_METADATA_MAX_BYTES + 1)?;
        let root = directory.path().to_string_lossy().into_owned();

        let plan = translate_strings("mutate4c", &["--root", &root])?;
        let Command::Mutate(arguments) = plan.cli.command else {
            panic!("expected mutation command");
        };
        assert!(arguments.run);
        assert!(arguments.test_command.is_none());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn escaping_makefile_symlink_never_autodetects_a_test_command() -> Result<(), Box<dyn std::error::Error>>
    {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        std::fs::write(outside.path().join("Makefile"), "test:\n\t@true\n")?;
        symlink(outside.path().join("Makefile"), directory.path().join("Makefile"))?;
        let root = directory.path().to_string_lossy().into_owned();

        let plan = translate_strings("mutate4c", &["--root", &root])?;
        let Command::Mutate(arguments) = plan.cli.command else {
            panic!("expected mutation command");
        };
        assert!(arguments.run);
        assert!(arguments.test_command.is_none());
        Ok(())
    }
}
