use std::{
    env,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, SystemTime},
};

use analysis_crap::discover_coverage_report;
use analysis_mutate::{run_command, CommandOutcome, CommandSpec, MutationError};
use anyhow::{anyhow, bail, Context, Result};
use clap::{error::ErrorKind, Args, CommandFactory, FromArgMatches, Parser};
use reporigor_core::{read_optional_bounded_utf8_file_within, Language, PROJECT_METADATA_MAX_BYTES};

use crate::args::{
    parse_min_tokens, parse_nonnegative_finite, parse_occurrence_limit, parse_positive,
    parse_positive_duration, BackendArg, CommonPath, CrapArgs, CrapInputArgs, CrapThresholdArgs, DryArgs,
    FormatArg, MutateArgs,
};
use crate::{run, write_terminal_error, Cli, Command};

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
        let (family, suffix) = legacy_family(basename)?;
        let language = legacy_language(suffix)?;
        Some(Self {
            alias: canonical_alias(basename)?,
            family,
            language,
        })
    }
}

const LEGACY_ALIAS_NAMES: &str = "crap4bash,crap4c,crap4cpp,crap4objc,crap4python,crap4rust,crap4swift,crap4ts,dry4bash,dry4c,dry4cpp,dry4objc,dry4python,dry4rust,dry4swift,dry4ts,mutate4bash,mutate4c,mutate4cpp,mutate4objc,mutate4python,mutate4rust,mutate4swift,mutate4ts";

fn canonical_alias(value: &str) -> Option<&'static str> {
    LEGACY_ALIAS_NAMES
        .split(',')
        .find(|candidate| *candidate == value)
}

fn legacy_family(basename: &str) -> Option<(Family, &str)> {
    [
        (Family::Crap, "crap4"),
        (Family::Dry, "dry4"),
        (Family::Mutate, "mutate4"),
    ]
    .into_iter()
    .find_map(|(family, prefix)| basename.strip_prefix(prefix).map(|suffix| (family, suffix)))
}

fn legacy_language(suffix: &str) -> Option<Language> {
    const SUFFIXES: [&str; 8] = ["bash", "c", "cpp", "objc", "python", "rust", "swift", "ts"];
    SUFFIXES
        .iter()
        .position(|candidate| *candidate == suffix)
        .map(|index| Language::ALL[index])
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

impl LegacyPlan {
    fn new(command: LegacyCommand, cli: Cli) -> Self {
        Self {
            alias: command.alias,
            cli,
            coverage: None,
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Args)]
struct LegacyCommonArgs {
    #[arg(value_name = "PATH_FRAGMENT")]
    filters: Vec<String>,

    #[arg(long, default_value = ".")]
    root: PathBuf,

    #[command(flatten)]
    features: LegacyFeatureArgs,

    #[command(flatten)]
    output: LegacyOutputArgs,
}

#[derive(Debug, Args)]
struct LegacyFeatureArgs {
    #[arg(long, value_delimiter = ',', conflicts_with = "all_features")]
    features: Vec<String>,

    #[arg(long, conflicts_with = "all_features")]
    no_default_features: bool,

    #[arg(long)]
    all_features: bool,
}

#[derive(Debug, Args)]
struct LegacyOutputArgs {
    #[arg(long)]
    include_tests: bool,

    #[arg(long = "json")]
    json_output: bool,
}

macro_rules! legacy_parser {
    ($name:ident, $command:literal, $description:literal, { $($field:tt)* }) => {
        #[derive(Debug, Parser)]
        #[command(name = $command, version, about = $description)]
        #[allow(clippy::struct_excessive_bools)]
        struct $name {
            $($field)*
        }
    };
}

legacy_parser!(LegacyCrapArgs, "legacy-crap", "Compatibility interface for crap4* commands", {
    #[command(flatten)]
    common: LegacyCommonArgs,

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

    #[arg(
        long,
        value_name = "SCORE",
        value_parser = parse_nonnegative_finite,
        allow_negative_numbers = true
    )]
    fail_over: Option<f64>,
});

legacy_parser!(LegacyDryArgs, "legacy-dry", "Compatibility interface for dry4* commands", {
    #[command(flatten)]
    common: LegacyCommonArgs,

    #[arg(long, default_value_t = 30, value_parser = parse_min_tokens)]
    min_tokens: usize,

    #[arg(long, default_value_t = 50, value_parser = parse_positive_usize)]
    max_groups: usize,

    #[arg(long, default_value_t = 100, value_parser = parse_occurrence_limit)]
    max_occurrences_per_window: usize,

    #[arg(long)]
    fail: bool,
});

legacy_parser!(LegacyMutateArgs, "legacy-mutate", "Compatibility interface for mutate4* commands", {
    #[command(flatten)]
    common: LegacyCommonArgs,

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

    #[arg(long = "manifest", visible_alias = "report")]
    report: Option<PathBuf>,

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
});

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

fn common_cli(
    command: LegacyCommand,
    options: LegacyCommonArgs,
    allow_parse_errors: bool,
    operation: Command,
) -> Cli {
    Cli {
        config: None,
        language: vec![command.language],
        backend: BackendArg::Auto,
        allow_project_exec: command.language == Language::Rust,
        include_tests: options.output.include_tests,
        allow_parse_errors,
        filters: options.filters,
        features: options.features.features,
        no_default_features: options.features.no_default_features,
        all_features: options.features.all_features,
        cargo: None,
        format: if options.output.json_output {
            FormatArg::Json
        } else {
            FormatArg::Text
        },
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
        arguments.common.root.join(&requested_coverage)
    };
    let preparation = (!arguments.no_test).then(|| CoveragePreparation {
        root: arguments.common.root.clone(),
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
        input: CrapInputArgs {
            common: CommonPath {
                path: arguments.common.root.clone(),
            },
            threshold: CrapThresholdArgs {
                coverage: Some(coverage),
                // Legacy commands only enabled the CRAP quality gate when
                // supplied. MAX preserves the unified-gate behavior.
                fail_over: Some(arguments.fail_over.unwrap_or(f64::MAX)),
            },
        },
        allow_missing_coverage: arguments.allow_missing_coverage,
        allow_empty: arguments.allow_empty,
    });
    let mut plan = LegacyPlan::new(
        command,
        common_cli(command, arguments.common, arguments.allow_parse_errors, operation),
    );
    plan.coverage = preparation;
    plan
}

fn dry_plan(command: LegacyCommand, arguments: LegacyDryArgs) -> LegacyPlan {
    let operation = Command::Dry(DryArgs {
        common: CommonPath {
            path: arguments.common.root.clone(),
        },
        min_tokens: Some(arguments.min_tokens),
        max_groups: Some(arguments.max_groups),
        max_occurrences_per_window: Some(arguments.max_occurrences_per_window),
        fail: arguments.fail,
    });
    LegacyPlan::new(command, common_cli(command, arguments.common, false, operation))
}

fn mutate_plan(
    command: LegacyCommand,
    arguments: LegacyMutateArgs,
) -> std::result::Result<LegacyPlan, clap::Error> {
    reject_obsolete_mutation_modes(&arguments)?;
    let root = arguments.common.root.clone();
    let execute = !arguments.list;
    let test_command =
        mutation_test_command(command.language, &root, execute, arguments.test_command.clone());
    let validation_command = mutation_validation_command(
        command.language,
        &root,
        arguments.no_validate,
        arguments.validate_command.clone(),
    );
    let warnings = mutation_compatibility_warnings(command.language, execute, &arguments);
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
    let mut plan = LegacyPlan::new(command, common_cli(command, arguments.common, false, operation));
    plan.warnings = warnings;
    Ok(plan)
}

fn reject_obsolete_mutation_modes(arguments: &LegacyMutateArgs) -> std::result::Result<(), clap::Error> {
    let obsolete = [
        arguments.scan,
        arguments.update_manifest,
        arguments.since_last_run,
        arguments.mutate_all,
    ]
    .into_iter()
    .any(std::convert::identity);
    if obsolete {
        return Err(clap::Error::raw(
            ErrorKind::InvalidValue,
            "--scan, --update-manifest, --since-last-run, and --mutate-all use the old Rust manifest protocol and cannot be translated; use `reporigor mutate --list` or `reporigor mutate --run`",
        ));
    }
    Ok(())
}

fn mutation_test_command(
    language: Language,
    root: &Path,
    execute: bool,
    explicit: Option<String>,
) -> Option<String> {
    if execute {
        explicit.or_else(|| detect_command(language, root, AutoCommand::Test))
    } else {
        explicit
    }
}

fn mutation_validation_command(
    language: Language,
    root: &Path,
    disabled: bool,
    explicit: Option<String>,
) -> Option<String> {
    if disabled {
        None
    } else {
        explicit.or_else(|| detect_command(language, root, AutoCommand::Validation))
    }
}

fn mutation_compatibility_warnings(
    language: Language,
    execute: bool,
    arguments: &LegacyMutateArgs,
) -> Vec<String> {
    [
        arguments.report.is_some().then_some(
            "--manifest/--report is accepted for migration but unified reports are written to stdout; redirect stdout or use --json",
        ),
        arguments
            .verbose
            .then_some("--verbose is no longer needed; operational diagnostics already use stderr"),
        arguments
            .fail_on_survivors
            .then_some("--fail-on-survivors is now the default"),
        (language == Language::Rust && execute).then_some(
            "the unified engine does not use mutate4rust's embedded differential manifest; this run considers every filtered mutation",
        ),
    ]
    .into_iter()
    .flatten()
    .map(str::to_owned)
    .collect()
}

fn execute_plan(mut plan: LegacyPlan) -> Result<u8> {
    for warning in &plan.warnings {
        write_legacy_error(&format!("{}: compatibility warning: {warning}", plan.alias));
    }
    match plan.coverage.take() {
        Some(preparation) => execute_coverage_plan(plan, &preparation),
        None => run(plan.cli),
    }
}

fn execute_coverage_plan(mut plan: LegacyPlan, preparation: &CoveragePreparation) -> Result<u8> {
    let read_session = legacy_read_session(&preparation.root)?;
    let report = prepare_coverage(preparation)?;
    apply_legacy_coverage(&mut plan.cli.command, report);
    crate::run_with_read_session(plan.cli, Some(read_session))
}

fn legacy_read_session(root: &Path) -> Result<analysis_mutate::MutationReadSession> {
    let session = analysis_mutate::MutationReadSession::begin(root)?;
    crate::refuse_pending_mutation(&session)?;
    Ok(session)
}

fn apply_legacy_coverage(command: &mut Command, report: PathBuf) {
    if let Command::Crap(arguments) = command {
        arguments.input.threshold.coverage = Some(report);
    }
}

fn write_legacy_error(message: &str) {
    write_terminal_error(message);
}

fn prepare_coverage(preparation: &CoveragePreparation) -> Result<PathBuf> {
    canonical_coverage_root(preparation).and_then(|root| prepare_coverage_at_root(preparation, &root))
}

fn canonical_coverage_root(preparation: &CoveragePreparation) -> Result<PathBuf> {
    preparation
        .root
        .canonicalize()
        .with_context(|| format!("cannot resolve project root {}", preparation.root.display()))
}

fn required_coverage_command(preparation: &CoveragePreparation) -> Result<&str> {
    preparation
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
        })
}

fn prepare_coverage_at_root(preparation: &CoveragePreparation, root: &Path) -> Result<PathBuf> {
    required_coverage_command(preparation)
        .and_then(|command| run_coverage_test(preparation, root, command))
        .and_then(|(started, outcome, cancelled)| {
            finish_coverage(preparation, root, started, &outcome, cancelled)
        })
}

fn run_coverage_test(
    preparation: &CoveragePreparation,
    root: &Path,
    command: &str,
) -> Result<(SystemTime, CommandOutcome, bool)> {
    let started = SystemTime::now();
    let cancellation = analysis_mutate::process_cancellation_token();
    let outcome = {
        let _signal_scope = analysis_mutate::cooperative_cancellation_scope();
        run_command(
            &CommandSpec::shell(command),
            root,
            preparation.timeout,
            COMMAND_OUTPUT_LIMIT,
            &cancellation,
        )?
    };
    Ok((started, outcome, cancellation.is_cancelled()))
}

fn finish_coverage(
    preparation: &CoveragePreparation,
    root: &Path,
    started: SystemTime,
    outcome: &CommandOutcome,
    cancelled: bool,
) -> Result<PathBuf> {
    validate_coverage_outcome(outcome, cancelled)
        .and_then(|()| locate_coverage_report(preparation, outcome, root))
        .and_then(|report| validate_coverage_freshness(&report, started).map(|()| report))
}

fn validate_coverage_outcome(outcome: &CommandOutcome, cancelled: bool) -> Result<()> {
    validate_coverage_cancellation(cancelled)
        .and_then(|()| validate_coverage_timeout(outcome))
        .and_then(|()| validate_coverage_exit(outcome))
}

fn validate_coverage_cancellation(cancelled: bool) -> Result<()> {
    if cancelled {
        return Err(MutationError::Cancelled.into());
    }
    Ok(())
}

fn validate_coverage_timeout(outcome: &CommandOutcome) -> Result<()> {
    if outcome.timed_out {
        bail!(
            "coverage test command timed out after {:.3}s",
            outcome.duration_seconds
        );
    }
    Ok(())
}

fn validate_coverage_exit(outcome: &CommandOutcome) -> Result<()> {
    if !outcome.success() {
        bail!(
            "coverage test command exited with {:?}\n{}",
            outcome.exit_code,
            outcome.output.trim()
        );
    }
    Ok(())
}

fn locate_coverage_report(
    preparation: &CoveragePreparation,
    outcome: &CommandOutcome,
    root: &Path,
) -> Result<PathBuf> {
    let requested = if preparation.requested.is_absolute() {
        preparation.requested.clone()
    } else {
        root.join(&preparation.requested)
    };
    discover_coverage_report(&requested)
        .or_else(|error| {
            coverage_path_from_output(&outcome.output, root)
                .filter(|path| path.is_file())
                .ok_or(error)
        })
        .map_err(Into::into)
}

fn validate_coverage_freshness(report: &Path, started: SystemTime) -> Result<()> {
    let stale = report
        .metadata()
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| coverage_is_stale(modified, started));
    if stale {
        bail!("coverage report is stale: {}", report.display());
    }
    Ok(())
}

fn coverage_is_stale(modified: SystemTime, started: SystemTime) -> bool {
    modified + Duration::from_secs(1) < started
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
    const PATHS: [&str; 8] = [
        "target/coverage",
        "target/coverage/lcov.info",
        "target/coverage/lcov.info",
        "target/coverage/lcov.info",
        "target/coverage/coverage.json",
        "target/coverage/lcov.info",
        "target/coverage/coverage.json",
        "target/coverage/coverage-final.json",
    ];
    PathBuf::from(PATHS[language as usize])
}

fn default_coverage_command(language: Language) -> Option<&'static str> {
    const COMMANDS: &str = "kcov target/coverage bats tests



coverage run -m pytest && coverage json -o target/coverage/coverage.json
cargo llvm-cov --workspace --lcov --output-path target/coverage/lcov.info

npx --no-install vitest run --coverage --coverage.reporter=json --coverage.reportsDirectory=target/coverage";
    indexed_optional_command(COMMANDS, language)
}

#[derive(Clone, Copy)]
enum AutoCommand {
    Test,
    Validation,
}

fn detect_command(language: Language, root: &Path, purpose: AutoCommand) -> Option<String> {
    match purpose {
        AutoCommand::Test => detect_test_command(language, root),
        AutoCommand::Validation => detect_validation_command(language, root),
    }
}

fn detect_test_command(language: Language, root: &Path) -> Option<String> {
    const COMMANDS: &str = "bats tests



python -m pytest -q
cargo test --workspace
swift test
npm test";
    if language.is_c_family() {
        detect_c_family_test(root)
    } else {
        indexed_optional_command(COMMANDS, language).map(str::to_owned)
    }
}

fn indexed_optional_command(commands: &'static str, language: Language) -> Option<&'static str> {
    commands
        .lines()
        .nth(language as usize)
        .filter(|command| !command.is_empty())
}

fn detect_validation_command(language: Language, root: &Path) -> Option<String> {
    const COMMANDS: [Option<&str>; 8] = [
        None,
        None,
        None,
        None,
        None,
        Some("cargo check --workspace"),
        Some("swift build"),
        None,
    ];
    if language.is_c_family() {
        return detect_c_family_validation(root);
    }
    if language == Language::TypeScript {
        return root
            .join("node_modules/.bin/tsc")
            .is_file()
            .then(|| "./node_modules/.bin/tsc --noEmit".to_string());
    }
    COMMANDS[language as usize].map(str::to_owned)
}

fn detect_c_family_test(root: &Path) -> Option<String> {
    if root.join("build/CTestTestfile.cmake").is_file() {
        return Some("ctest --test-dir build --output-on-failure".to_string());
    }
    has_make_target(root, &root.join("Makefile"), "test").then(|| "make test".to_string())
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

fn parse_positive_usize(value: &str) -> std::result::Result<usize, String> {
    parse_positive(value)
}

fn exit_code(value: i32) -> ExitCode {
    u8::try_from(value).map_or(ExitCode::FAILURE, ExitCode::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY_LANGUAGE_SUFFIXES: &str = "bash,c,cpp,objc,python,rust,swift,ts";
    type IntegrationResult = Result<(), Box<dyn std::error::Error>>;

    fn translate_strings(alias: &str, arguments: &[&str]) -> std::result::Result<LegacyPlan, clap::Error> {
        let command = LegacyCommand::parse(OsStr::new(alias)).ok_or_else(|| {
            clap::Error::raw(ErrorKind::InvalidValue, format!("unknown legacy alias {alias}"))
        })?;
        let arguments = arguments.iter().map(OsString::from).collect::<Vec<_>>();
        translate(command, &arguments)
    }

    fn translate_encoded(alias: &str, arguments: &str) -> std::result::Result<LegacyPlan, clap::Error> {
        translate_strings(alias, &arguments.split('|').collect::<Vec<_>>())
    }

    fn mutation_arguments(plan: LegacyPlan) -> MutateArgs {
        match plan.cli.command {
            Command::Mutate(arguments) => arguments,
            _ => panic!("expected mutation command"),
        }
    }

    fn translated_language(
        alias: &str,
        arguments: &str,
        language: Language,
    ) -> Result<LegacyPlan, clap::Error> {
        let plan = translate_encoded(alias, arguments)?;
        assert_eq!(plan.cli.language, vec![language]);
        Ok(plan)
    }

    fn translated_mutation(
        alias: &str,
        arguments: &[&str],
        language: Language,
    ) -> Result<(FormatArg, MutateArgs), clap::Error> {
        let plan = translate_strings(alias, arguments)?;
        assert_eq!(plan.cli.language, vec![language]);
        let format = plan.cli.format;
        Ok((format, mutation_arguments(plan)))
    }

    fn assert_parse_error(alias: &str, arguments: &[&str], kind: ErrorKind, message: &str) {
        let Err(error) = translate_strings(alias, arguments) else {
            panic!("{alias} unexpectedly accepted {arguments:?}");
        };
        assert_eq!(error.kind(), kind);
        assert!(error.to_string().contains(message), "{alias}: {error}");
    }

    fn assert_no_detected_c_test(root: &Path) -> Result<(), clap::Error> {
        let root = root.to_string_lossy();
        let plan = translate_strings("mutate4c", &["--root", &root])?;
        let arguments = mutation_arguments(plan);
        assert!(arguments.run);
        assert!(arguments.test_command.is_none());
        Ok(())
    }

    #[test]
    fn recognizes_exactly_the_supported_alias_shapes() {
        for family in ["crap", "dry", "mutate"] {
            for language in LEGACY_LANGUAGE_SUFFIXES.split(',') {
                assert!(LegacyCommand::parse(OsStr::new(&format!("{family}4{language}"))).is_some());
            }
        }
        assert!(LegacyCommand::parse(OsStr::new("crap4javascript")).is_none());
        assert!(LegacyCommand::parse(OsStr::new("reporigor")).is_none());
    }

    #[test]
    fn crap_python_translates_filters_coverage_and_global_flags() -> Result<(), clap::Error> {
        let plan = translated_language(
            "crap4python",
            "pkg/one|pkg/two|--root|project|--coverage|coverage.json|--no-test|--allow-parse-errors|--json|--fail-over|9.5",
            Language::Python,
        )?;
        assert_eq!(plan.cli.filters, ["pkg/one", "pkg/two"]);
        assert!(plan.cli.allow_parse_errors);
        assert!(!plan.cli.allow_project_exec);
        assert_eq!(plan.cli.format, FormatArg::Json);
        assert!(plan.coverage.is_none());
        let Command::Crap(arguments) = plan.cli.command else {
            panic!("expected CRAP command");
        };
        assert_eq!(arguments.input.common.path, Path::new("project"));
        assert_eq!(
            arguments.input.threshold.coverage.as_deref(),
            Some(Path::new("project/coverage.json"))
        );
        assert_eq!(arguments.input.threshold.fail_over, Some(9.5));
        Ok(())
    }

    #[test]
    fn dry_rust_preserves_feature_and_gate_options() -> Result<(), clap::Error> {
        let plan = translated_language(
            "dry4rust",
            "src/parser|--features|serde,cli|--no-default-features|--min-tokens|16|--max-groups|7|--fail",
            Language::Rust,
        )?;
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
        let (_, arguments) = translated_mutation(
            "mutate4swift",
            &["Sources", "--max-mutants", "4"],
            Language::Swift,
        )?;
        assert!(arguments.run);
        assert!(!arguments.list);
        assert_eq!(arguments.test_command.as_deref(), Some("swift test"));
        assert_eq!(arguments.validation_command.as_deref(), Some("swift build"));
        assert_eq!(arguments.max_mutants, Some(4));
        Ok(())
    }

    #[test]
    fn mutate_list_is_read_only_and_needs_no_detected_test_command() -> Result<(), clap::Error> {
        let (format, arguments) = translated_mutation(
            "mutate4c",
            &["--root", "missing", "--list", "--json"],
            Language::C,
        )?;
        assert_eq!(format, FormatArg::Json);
        assert!(!arguments.run);
        assert!(arguments.list);
        assert!(arguments.test_command.is_none());
        Ok(())
    }

    #[test]
    fn mutate_no_validate_is_preserved_as_an_explicit_override() -> Result<(), clap::Error> {
        let plan = translate_strings("mutate4python", &["--no-validate"])?;
        let arguments = mutation_arguments(plan);
        assert!(arguments.no_validate);
        assert!(arguments.validation_command.is_none());
        Ok(())
    }

    #[test]
    fn rust_manifest_protocol_modes_fail_loudly() {
        assert_parse_error(
            "mutate4rust",
            &["--scan"],
            ErrorKind::InvalidValue,
            "old Rust manifest protocol",
        );
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
        for language in LEGACY_LANGUAGE_SUFFIXES.split(',') {
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
        assert_parse_error(
            "crap4objc",
            &["--help"],
            ErrorKind::DisplayHelp,
            "Usage: crap4objc",
        );
    }

    #[test]
    fn swift_implicit_coverage_command_is_rejected_before_execution() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::Builder::new().prefix("swift-coverage-").tempdir()?;
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
        assert_no_detected_c_test(directory.path())?;
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn escaping_makefile_symlink_never_autodetects_a_test_command() -> IntegrationResult {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        std::fs::write(outside.path().join("Makefile"), "test:\n\t@true\n")?;
        symlink(outside.path().join("Makefile"), directory.path().join("Makefile"))?;
        assert_no_detected_c_test(directory.path())?;
        Ok(())
    }
}
