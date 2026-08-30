mod args;
mod compat;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fmt::Write as FmtWrite;
use std::io::{self, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::time::Duration;

use adapter_clang::{AstDumpStatus, ClangAdapter};
use adapter_project::{
    discover_mutation_providers, preflight_mutation_providers, ImportFormat, MutationProviderStatus,
    ProjectAdapter, ProviderInventory, ProviderResolution, ProviderStatus,
};
use adapter_rust::{CargoOptions, RustAdapter};
use adapter_tree_sitter::TreeSitterBackend;
use analysis_crap::{analyze_path_with_policy, analyze_with_policy, CrapAnalysis};
use analysis_mutate::{
    cooperative_cancellation_scope, process_cancellation_token, recover_active, select_candidates,
    CancellationToken, CommandSpec, MutationError, MutationExecutionSession, MutationExecutor, MutationMode,
    MutationOptions, MutationReadSession, MutationRun, RecoveryAction,
};
use analysis_quality::{
    analyze_duplicates as analyze_dry, analyze_rule_duplicates as analyze_rule_dry, analyze_rules,
    apply_baseline_with_incomplete_rules, BaselineComparison, Duplicate, OmittedCheck, QualityAnalysis,
    QualityInput,
};
use anyhow::{anyhow, bail, Context, Result};
use reporigor_core::{
    checked_duration_from_secs_f64, read_bounded_utf8_file_within, resolve_optional_regular_file_within,
    stable_id, AnalysisRequest, AnalysisSnapshot, BackendPreference, BaselineConfig, CoreError, Diagnostic,
    FileAnalysis, FunctionRecord, Language, MutationCandidate, ProjectContext, ProjectKind, RepoRigorConfig,
    RuleResult, Severity, SourceBudget, SourceFile, SyntaxBackend,
};
use reporigor_reporting::{
    escape_terminal_text, CrapReport, DryReport, MutationReport, MutationThresholds, ReportCommand,
    ReportContext, ReportEnvelope, RuleReport, REPORT_SCHEMA_VERSION,
};

pub use analysis_mutate::install_signal_handlers;
pub use args::{BackendArg, Cli, Command, FormatArg};
pub use compat::entry_from_env as legacy_entry_from_env;

const EXIT_OK: u8 = 0;
const EXIT_OPERATIONAL: u8 = 1;
const EXIT_QUALITY: u8 = 2;

/// Write escaped diagnostic text to stderr without panicking on a closed pipe.
pub fn write_terminal_error(message: &str) {
    let _result = writeln!(io::stderr().lock(), "{}", escape_terminal_text(message));
}

/// Execute one parsed command and return its stable process exit code.
///
/// # Errors
///
/// Returns an error for invalid roots/configuration, unavailable required
/// backends, parser failures, unsafe mutation requests, and report failures.
pub fn run(cli: Cli) -> Result<u8> {
    run_with_read_session(cli, None)
}

pub(crate) fn run_with_read_session(
    cli: Cli,
    inherited_read_session: Option<MutationReadSession>,
) -> Result<u8> {
    let (cli, root, format) = prepare_cli_root(cli)?;
    if let Some(exit) = requested_recovery(&cli.command, &root, format, inherited_read_session.as_ref())? {
        return Ok(exit);
    }
    run_nonrecovery_command(cli, root, format, inherited_read_session)
}

fn prepare_cli_root(cli: Cli) -> Result<(Cli, PathBuf, FormatArg)> {
    let format = cli.format;
    validate_format(&cli.command, format)?;
    let root = command_root(&cli.command);
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve project root {}", root.display()))?;
    Ok((cli, root, format))
}

fn run_nonrecovery_command(
    cli: Cli,
    root: PathBuf,
    format: FormatArg,
    inherited_read_session: Option<MutationReadSession>,
) -> Result<u8> {
    let read_session = prepare_read_session(&cli.command, &root, inherited_read_session)?;
    let (config, _) = RepoRigorConfig::discover(&root, cli.config.as_deref())?;
    let request = analysis_request(&cli, &config, root);
    let allow_project_exec = cli.allow_project_exec;
    validate_project_execution_policy(&cli.command, &request, allow_project_exec)?;
    let cargo_options = CargoOptions {
        features: cli.features.clone(),
        all_features: cli.all_features,
        no_default_features: cli.no_default_features,
        cargo: cli.cargo.as_deref().map(anchor_executable).or_else(resolve_cargo),
    };
    let result = dispatch_command(
        cli.command,
        &request,
        &config,
        cargo_options,
        format,
        allow_project_exec,
    );
    drop(read_session);
    result
}

fn requested_recovery(
    command: &Command,
    root: &Path,
    format: FormatArg,
    inherited: Option<&MutationReadSession>,
) -> Result<Option<u8>> {
    if !matches!(command, Command::Mutate(arguments) if arguments.recover) {
        return Ok(None);
    }
    if inherited.is_some() {
        bail!("mutation recovery cannot run inside a read-only analysis session");
    }
    run_recovery(root, format).map(Some)
}

fn prepare_read_session(
    command: &Command,
    root: &Path,
    inherited: Option<MutationReadSession>,
) -> Result<Option<MutationReadSession>> {
    if !command_requires_read_session(command) {
        reject_inherited_execution_session(inherited.as_ref())?;
        return Ok(None);
    }
    let session = read_session_for_root(root, inherited)?;
    refuse_pending_mutation(&session)?;
    Ok(Some(session))
}

fn reject_inherited_execution_session(inherited: Option<&MutationReadSession>) -> Result<()> {
    if inherited.is_some() {
        bail!("an executing mutation command cannot reuse a read-only analysis session");
    }
    Ok(())
}

fn read_session_for_root(root: &Path, inherited: Option<MutationReadSession>) -> Result<MutationReadSession> {
    let Some(session) = inherited else {
        return MutationReadSession::begin(root).map_err(Into::into);
    };
    if session.root() != root {
        bail!(
            "read-only mutation session for {} cannot analyze {}",
            session.root().display(),
            root.display()
        );
    }
    Ok(session)
}

fn dispatch_command(
    command: Command,
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    cargo_options: CargoOptions,
    format: FormatArg,
    allow_project_exec: bool,
) -> Result<u8> {
    match command {
        Command::Crap(arguments) => run_crap(
            request,
            config,
            cargo_options,
            &arguments,
            format,
            allow_project_exec,
        ),
        Command::Dry(arguments) => run_dry(
            request,
            config,
            cargo_options,
            &arguments,
            format,
            allow_project_exec,
        ),
        Command::Mutate(arguments) => run_mutate(
            request,
            config,
            cargo_options,
            &arguments,
            format,
            allow_project_exec,
        ),
        Command::Check(arguments) => run_check(
            request,
            config,
            cargo_options,
            &arguments,
            format,
            allow_project_exec,
        ),
        Command::Providers(arguments) => run_providers(request, arguments.preflight, format),
    }
}

fn command_requires_read_session(command: &Command) -> bool {
    match command {
        Command::Crap(_) | Command::Dry(_) | Command::Providers(_) => true,
        Command::Check(arguments) => !arguments.run_mutations,
        Command::Mutate(arguments) => !arguments.run,
    }
}

pub(crate) fn refuse_pending_mutation(session: &MutationReadSession) -> Result<()> {
    if let Some(pending) = session.pending_mutation()? {
        bail!(
            "pending mutation recovery journal {} means the project may still contain an active mutant; run `reporigor mutate --recover \"{}\"` before analysis",
            pending.journal.display(),
            pending.root.display()
        );
    }
    Ok(())
}

fn run_recovery(root: &Path, format: FormatArg) -> Result<u8> {
    validate_recovery_format(format)?;
    let recovery = recover_active(root)?;
    let rendered = render_recovery(root, recovery, format)?;
    write_stdout(&rendered)?;
    Ok(EXIT_OK)
}

fn validate_recovery_format(format: FormatArg) -> Result<()> {
    if !matches!(format, FormatArg::Text | FormatArg::Json) {
        bail!("mutation recovery supports only --format text or --format json");
    }
    Ok(())
}

fn render_recovery(root: &Path, recovery: RecoveryAction, format: FormatArg) -> Result<String> {
    if format == FormatArg::Json {
        return render_recovery_json(root, recovery);
    }
    Ok(format!("mutation recovery: {}\n", recovery_detail(recovery)))
}

fn render_recovery_json(root: &Path, recovery: RecoveryAction) -> Result<String> {
    let document = serde_json::json!({
        "root": root,
        "recovery": recovery,
    });
    let mut rendered = serde_json::to_string_pretty(&document)?;
    rendered.push('\n');
    Ok(rendered)
}

fn recovery_detail(recovery: RecoveryAction) -> &'static str {
    match recovery {
        RecoveryAction::None => "no pending mutation journal",
        RecoveryAction::AlreadyClean => "source was already clean; removed the pending journal",
        RecoveryAction::Restored => "restored the original source and removed the pending journal",
    }
}

fn validate_project_execution_policy(
    command: &Command,
    request: &AnalysisRequest,
    allow_project_exec: bool,
) -> Result<()> {
    if request.backend == BackendPreference::Native
        && !allow_project_exec
        && !matches!(command, Command::Providers(_))
    {
        bail!(
            "--backend native may execute existing project toolchains; rerun with --allow-project-exec to grant that permission"
        );
    }
    Ok(())
}

fn validate_format(command: &Command, format: FormatArg) -> Result<()> {
    match (command, format) {
        (Command::Mutate(_), FormatArg::Sarif) => {
            bail!("mutate does not produce SARIF findings; use --format json or mutation-json")
        }
        (Command::Crap(_) | Command::Dry(_), FormatArg::MutationJson) => {
            bail!("mutation-json is available only for mutate and check")
        }
        (Command::Providers(_), FormatArg::Sarif | FormatArg::MutationJson) => {
            bail!("providers supports only --format text or --format json")
        }
        _ => Ok(()),
    }
}

fn command_root(command: &Command) -> &Path {
    match command {
        Command::Crap(arguments) => &arguments.input.common.path,
        Command::Dry(arguments) => &arguments.common.path,
        Command::Mutate(arguments) => &arguments.common.path,
        Command::Check(arguments) => &arguments.input.common.path,
        Command::Providers(arguments) => &arguments.common.path,
    }
}

fn analysis_request(cli: &Cli, config: &RepoRigorConfig, root: PathBuf) -> AnalysisRequest {
    AnalysisRequest {
        root,
        languages: cli.language.iter().copied().collect(),
        filters: cli.filters.clone(),
        include_tests: cli.include_tests || config.include_tests,
        allow_parse_errors: cli.allow_parse_errors || config.allow_parse_errors,
        max_source_bytes: config.max_source_bytes,
        backend: match cli.backend {
            BackendArg::Auto => config.backend,
            BackendArg::Native => BackendPreference::Native,
            BackendArg::Generic => BackendPreference::Generic,
        },
    }
}

fn run_crap(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    cargo: CargoOptions,
    arguments: &args::CrapArgs,
    format: FormatArg,
    allow_project_exec: bool,
) -> Result<u8> {
    emit_completed_analysis(
        CompletedAnalysis::Crap(complete_crap(
            request,
            config,
            cargo,
            arguments,
            allow_project_exec,
        )?),
        format,
        request.max_source_bytes,
    )
}

enum CompletedAnalysis {
    Crap(CompletedCrap),
    Check(CompletedCheck),
}

fn emit_completed_analysis(
    completed: CompletedAnalysis,
    format: FormatArg,
    max_source_bytes: usize,
) -> Result<u8> {
    let (report, snapshot, exit) = match completed {
        CompletedAnalysis::Crap(completed) => (completed.report, completed.snapshot, completed.exit),
        CompletedAnalysis::Check(completed) => (completed.report, completed.snapshot, completed.exit),
    };
    emit_report(&report, format, &snapshot, max_source_bytes)?;
    Ok(exit)
}

struct CompletedCrap {
    snapshot: AnalysisSnapshot,
    report: ReportEnvelope,
    exit: u8,
}

fn complete_crap(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    cargo: CargoOptions,
    arguments: &args::CrapArgs,
    allow_project_exec: bool,
) -> Result<CompletedCrap> {
    let snapshot = analyze_project(request, cargo, allow_project_exec)?;
    let allow_empty = crap_allow_empty(arguments, config);
    ensure_crap_sources_selected(request, &snapshot, allow_empty)?;
    let threshold = arguments
        .input
        .threshold
        .fail_over
        .unwrap_or(config.crap.fail_over);
    let analysis = crap_analysis(
        request,
        &snapshot,
        arguments.input.threshold.coverage.as_deref(),
        config.crap.unreported_as_zero,
    )?;
    let missing = analysis.missing_coverage();
    let over = analysis.over_threshold(threshold);
    let empty = analysis.functions.is_empty();
    let report = ReportEnvelope::crap(
        ReportContext::from_snapshot(&request.root, &snapshot),
        CrapReport::from_analysis(analysis, threshold),
    );
    let exit = crap_exit(
        empty,
        allow_empty,
        missing,
        crap_allow_missing_coverage(arguments, config),
        over,
    );
    Ok(CompletedCrap {
        snapshot,
        report,
        exit,
    })
}

fn crap_allow_empty(arguments: &args::CrapArgs, config: &RepoRigorConfig) -> bool {
    arguments.allow_empty || config.crap.allow_empty
}

fn crap_allow_missing_coverage(arguments: &args::CrapArgs, config: &RepoRigorConfig) -> bool {
    arguments.allow_missing_coverage || config.crap.allow_missing_coverage
}

fn ensure_crap_sources_selected(
    request: &AnalysisRequest,
    snapshot: &AnalysisSnapshot,
    allow_empty: bool,
) -> Result<()> {
    if snapshot.files.is_empty() && !allow_empty {
        return no_sources_selected(request);
    }
    Ok(())
}

fn crap_exit(empty: bool, allow_empty: bool, missing: usize, allow_missing: bool, over: usize) -> u8 {
    if crap_input_invalid(empty, allow_empty, missing, allow_missing) {
        return EXIT_OPERATIONAL;
    }
    if over > 0 {
        EXIT_QUALITY
    } else {
        EXIT_OK
    }
}

fn crap_input_invalid(empty: bool, allow_empty: bool, missing: usize, allow_missing: bool) -> bool {
    (empty && !allow_empty) || (missing > 0 && !allow_missing)
}

fn run_dry(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    cargo: CargoOptions,
    arguments: &args::DryArgs,
    format: FormatArg,
    allow_project_exec: bool,
) -> Result<u8> {
    let snapshot = analyze_project(request, cargo, allow_project_exec)?;
    ensure_sources_selected(request, &snapshot)?;
    let min_tokens = arguments.min_tokens.unwrap_or(config.dry.min_tokens);
    let dry_config = standalone_dry_config(config, arguments, min_tokens);
    let duplicates = analyze_dry(&snapshot.tokens, &snapshot.functions, &dry_config)?;
    let findings = !duplicates.is_empty();
    let report = ReportEnvelope::dry(
        ReportContext::from_snapshot(&request.root, &snapshot),
        DryReport::new(duplicates, min_tokens),
    );
    emit_report(&report, format, &snapshot, request.max_source_bytes)?;
    Ok(if dry_gate_fails(findings, arguments.fail, config.dry.fail) {
        EXIT_QUALITY
    } else {
        EXIT_OK
    })
}

fn standalone_dry_config(
    config: &RepoRigorConfig,
    arguments: &args::DryArgs,
    min_tokens: usize,
) -> reporigor_core::DryConfig {
    let mut dry_config = config.dry.clone();
    dry_config.min_tokens = min_tokens;
    dry_config.shingle_tokens = dry_config.shingle_tokens.min(min_tokens);
    dry_config.max_groups = arguments.max_groups.unwrap_or(config.dry.max_groups);
    dry_config.max_occurrences_per_window = arguments
        .max_occurrences_per_window
        .unwrap_or(config.dry.max_occurrences_per_window);
    dry_config
}

fn dry_gate_fails(findings: bool, argument_fail: bool, configured_fail: bool) -> bool {
    findings && (argument_fail || configured_fail)
}

fn run_mutate(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    cargo: CargoOptions,
    arguments: &args::MutateArgs,
    format: FormatArg,
    allow_project_exec: bool,
) -> Result<u8> {
    let (snapshot, report_section) =
        standalone_mutation_report(request, config, cargo, arguments, allow_project_exec)?;
    let exit = mutation_exit(
        &report_section,
        arguments.allow_survivors,
        arguments.allow_compile_errors,
    );
    let report = ReportEnvelope::mutate(
        ReportContext::from_snapshot(&request.root, &snapshot),
        report_section,
    );
    emit_report(&report, format, &snapshot, request.max_source_bytes)?;
    Ok(exit)
}

struct PreparedMutation {
    snapshot: AnalysisSnapshot,
    selected: Vec<MutationCandidate>,
    session: Option<MutationExecutionSession>,
    options: MutationOptions,
    cancellation: CancellationToken,
}

fn standalone_mutation_report(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    cargo: CargoOptions,
    arguments: &args::MutateArgs,
    allow_project_exec: bool,
) -> Result<(AnalysisSnapshot, MutationReport)> {
    let session = mutation_execution_session(arguments.run, &request.root)?;
    let cancellation = process_cancellation_token();
    let options =
        standalone_mutation_options(request, config, arguments, arguments.run, cancellation.clone())?;
    let (snapshot, selected) = standalone_mutation_inventory(request, config, cargo, allow_project_exec)?;
    let prepared = PreparedMutation {
        snapshot,
        selected,
        session,
        options,
        cancellation,
    };
    execute_prepared_mutation(&request.root, prepared)
}

fn standalone_mutation_inventory(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    cargo: CargoOptions,
    allow_project_exec: bool,
) -> Result<(AnalysisSnapshot, Vec<MutationCandidate>)> {
    let snapshot = analyze_project(request, cargo, allow_project_exec)?;
    ensure_sources_selected(request, &snapshot)?;
    let selected = select_candidates(
        &snapshot.mutations,
        &config.mutation.operators,
        config.mutation.seed,
    )?;
    Ok((snapshot, selected))
}

fn execute_prepared_mutation(
    root: &Path,
    prepared: PreparedMutation,
) -> Result<(AnalysisSnapshot, MutationReport)> {
    let executor = MutationExecutor::new(root, prepared.options)?;
    let run = execute_mutation_run(
        &executor,
        &prepared.selected,
        prepared.session.as_ref(),
        &prepared.cancellation,
    )?;
    Ok((prepared.snapshot, MutationReport::from_run(run)))
}

fn mutation_execution_session(execute: bool, root: &Path) -> Result<Option<MutationExecutionSession>> {
    if execute {
        MutationExecutionSession::begin(root)
            .map(Some)
            .map_err(Into::into)
    } else {
        Ok(None)
    }
}

fn standalone_mutation_options(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    arguments: &args::MutateArgs,
    execute: bool,
    cancellation: CancellationToken,
) -> Result<MutationOptions> {
    let test_command = standalone_test_command(arguments, config, execute)?;
    Ok(MutationOptions {
        mode: mutation_mode(execute),
        test_command: test_command.map(CommandSpec::shell),
        validation_command: standalone_validation_command(arguments, config),
        timeout: standalone_mutation_timeout(arguments, config)?,
        run_baseline: execute && !arguments.skip_baseline,
        max_mutants: arguments.max_mutants.or(config.mutation.max_mutants),
        max_source_bytes: request.max_source_bytes,
        cancellation,
        ..MutationOptions::default()
    })
}

fn standalone_test_command(
    arguments: &args::MutateArgs,
    config: &RepoRigorConfig,
    execute: bool,
) -> Result<Option<String>> {
    let command = arguments
        .test_command
        .clone()
        .or_else(|| config.mutation.test_command.clone());
    if execute && command.as_ref().is_none_or(|value| value.trim().is_empty()) {
        bail!("--run requires --test-command or mutation.test_command in reporigor.toml");
    }
    Ok(command)
}

fn standalone_validation_command(
    arguments: &args::MutateArgs,
    config: &RepoRigorConfig,
) -> Option<CommandSpec> {
    if arguments.no_validate {
        return None;
    }
    arguments
        .validation_command
        .clone()
        .or_else(|| config.mutation.validation_command.clone())
        .map(CommandSpec::shell)
}

fn standalone_mutation_timeout(arguments: &args::MutateArgs, config: &RepoRigorConfig) -> Result<Duration> {
    arguments
        .timeout
        .map_or_else(|| positive_duration(config.mutation.timeout_seconds), Ok)
}

fn mutation_mode(execute: bool) -> MutationMode {
    if execute {
        MutationMode::Execute
    } else {
        MutationMode::List
    }
}

fn execute_mutation_run(
    executor: &MutationExecutor,
    mutations: &[MutationCandidate],
    session: Option<&MutationExecutionSession>,
    cancellation: &CancellationToken,
) -> Result<MutationRun> {
    let run = match session {
        Some(session) => {
            let _signal_scope = cooperative_cancellation_scope();
            executor.run_in_session(mutations, session)?
        }
        None => executor.run(mutations)?,
    };
    if cancellation.is_cancelled() {
        return Err(MutationError::Cancelled.into());
    }
    Ok(run)
}

fn run_check(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    cargo: CargoOptions,
    arguments: &args::CheckArgs,
    format: FormatArg,
    allow_project_exec: bool,
) -> Result<u8> {
    let context = prepare_check_context(request, config, cargo, arguments, allow_project_exec)?;
    let (crap, duplicates, rule_duplicates) = check_static_analyses(request, config, arguments, &context)?;
    let mutation_run = check_mutation_run(request, &context)?;
    let prepared = PreparedCheck {
        context,
        crap,
        duplicates,
        rule_duplicates,
        mutation_run,
    };
    emit_completed_analysis(
        CompletedAnalysis::Check(finish_check(request, config, arguments, prepared)?),
        format,
        request.max_source_bytes,
    )
}

struct CheckContext {
    analysis_scope: String,
    snapshot: AnalysisSnapshot,
    threshold: f64,
    min_tokens: usize,
    effective_config: RepoRigorConfig,
    mutation_session: Option<MutationExecutionSession>,
    mutation_options: MutationOptions,
    cancellation: CancellationToken,
}

struct PreparedCheck {
    context: CheckContext,
    crap: CrapAnalysis,
    duplicates: Vec<Duplicate>,
    rule_duplicates: Vec<Duplicate>,
    mutation_run: MutationRun,
}

struct CompletedCheck {
    snapshot: AnalysisSnapshot,
    report: ReportEnvelope,
    exit: u8,
}

fn prepare_check_context(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    cargo: CargoOptions,
    arguments: &args::CheckArgs,
    allow_project_exec: bool,
) -> Result<CheckContext> {
    let analysis_scope = check_scope_fingerprint(request, config, &cargo, arguments, allow_project_exec)?;
    let mutation_session = mutation_execution_session(arguments.run_mutations, &request.root)?;
    let cancellation = process_cancellation_token();
    let mutation_options = check_mutation_options(request, config, arguments, cancellation.clone())?;
    let snapshot = check_snapshot(request, cargo, allow_project_exec)?;
    let threshold = arguments
        .input
        .threshold
        .fail_over
        .unwrap_or(config.crap.fail_over);
    let (effective_config, min_tokens) = effective_check_config(config, arguments, threshold);
    Ok(CheckContext {
        analysis_scope,
        snapshot,
        threshold,
        min_tokens,
        effective_config,
        mutation_session,
        mutation_options,
        cancellation,
    })
}

fn check_snapshot(
    request: &AnalysisRequest,
    cargo: CargoOptions,
    allow_project_exec: bool,
) -> Result<AnalysisSnapshot> {
    let snapshot = analyze_project(request, cargo, allow_project_exec)?;
    ensure_sources_selected(request, &snapshot)?;
    Ok(snapshot)
}

fn check_static_analyses(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    arguments: &args::CheckArgs,
    context: &CheckContext,
) -> Result<(CrapAnalysis, Vec<Duplicate>, Vec<Duplicate>)> {
    let crap = crap_analysis(
        request,
        &context.snapshot,
        arguments.input.threshold.coverage.as_deref(),
        config.crap.unreported_as_zero,
    )?;
    let duplicates = analyze_dry(
        &context.snapshot.tokens,
        &context.snapshot.functions,
        &context.effective_config.dry,
    )?;
    let rule_duplicates = check_rule_duplicates(&context.snapshot, &context.effective_config)?;
    Ok((crap, duplicates, rule_duplicates))
}

fn check_rule_duplicates(snapshot: &AnalysisSnapshot, config: &RepoRigorConfig) -> Result<Vec<Duplicate>> {
    // The display cap must not hide a clone from the integrated gate.
    let mut rule_dry_config = config.dry.clone();
    rule_dry_config.max_groups = usize::MAX;
    analyze_rule_dry(&snapshot.functions, &rule_dry_config).map_err(Into::into)
}

fn check_mutation_run(request: &AnalysisRequest, context: &CheckContext) -> Result<MutationRun> {
    let selected = select_candidates(
        &context.snapshot.mutations,
        &context.effective_config.mutation.operators,
        context.effective_config.mutation.seed,
    )?;
    let executor = MutationExecutor::new(&request.root, context.mutation_options.clone())?;
    execute_mutation_run(
        &executor,
        &selected,
        context.mutation_session.as_ref(),
        &context.cancellation,
    )
}

fn finish_check(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    arguments: &args::CheckArgs,
    mut prepared: PreparedCheck,
) -> Result<CompletedCheck> {
    let mut quality = check_quality_analysis(&prepared)?;
    apply_parse_recovery_omissions(&mut quality, prepared.context.snapshot.parse_errors);
    let analysis_scope = std::mem::take(&mut prepared.context.analysis_scope);
    let (rules, quality_gate_passed) = checked_rule_report(request, config, analysis_scope, quality)?;
    Ok(build_completed_check(
        request,
        arguments,
        prepared,
        rules,
        quality_gate_passed,
    ))
}

fn check_quality_analysis(prepared: &PreparedCheck) -> Result<QualityAnalysis> {
    analyze_rules(QualityInput {
        config: &prepared.context.effective_config,
        functions: &prepared.crap.functions,
        duplicates: &prepared.rule_duplicates,
        mutations: &prepared.mutation_run.results,
        repository: &prepared.context.snapshot.repository,
    })
    .map_err(|error| anyhow!(error))
}

fn build_completed_check(
    request: &AnalysisRequest,
    arguments: &args::CheckArgs,
    prepared: PreparedCheck,
    rules: RuleReport,
    quality_gate_passed: bool,
) -> CompletedCheck {
    let crap = CrapReport::from_analysis(prepared.crap, prepared.context.threshold);
    let dry = DryReport::new(prepared.duplicates, prepared.context.min_tokens);
    let mutation = MutationReport::from_run(prepared.mutation_run);
    let mutation_exit = check_mutation_exit(arguments.run_mutations, &mutation);
    let report = ReportEnvelope::check(
        ReportContext::from_snapshot(&request.root, &prepared.context.snapshot),
        Some(crap),
        Some(dry),
        Some(mutation),
        Some(rules),
    );
    CompletedCheck {
        snapshot: prepared.context.snapshot,
        report,
        exit: check_exit(mutation_exit, quality_gate_passed),
    }
}

fn effective_check_config(
    config: &RepoRigorConfig,
    arguments: &args::CheckArgs,
    threshold: f64,
) -> (RepoRigorConfig, usize) {
    let min_tokens = arguments.min_tokens.unwrap_or(config.dry.min_tokens);
    let mut effective = config.clone();
    effective.crap.fail_over = threshold;
    effective.dry.min_tokens = min_tokens;
    effective.dry.shingle_tokens = effective.dry.shingle_tokens.min(min_tokens);
    (effective, min_tokens)
}

fn checked_rule_report(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    analysis_scope: String,
    mut quality: QualityAnalysis,
) -> Result<(RuleReport, bool)> {
    let previous_rules = load_baseline_rules(request, config, &analysis_scope)?;
    let incomplete_rules = quality
        .omitted
        .iter()
        .map(|omission| omission.rule_id.clone())
        .collect();
    let has_omissions = !quality.omitted.is_empty();
    let mut comparison = apply_baseline_with_incomplete_rules(
        &mut quality.results,
        previous_rules.as_deref(),
        config.baseline.enabled,
        &incomplete_rules,
    )
    .map_err(|error| anyhow!(error))?;
    // A baseline may acknowledge measured debt, but it cannot turn missing
    // evidence into a passing check.
    require_complete_quality_gate(&mut comparison, has_omissions);
    let gate_passed = comparison.gate_passed;
    let report = RuleReport::with_baseline(
        quality,
        config.baseline.enabled,
        configured_baseline_path(config)?,
        analysis_scope,
        &comparison,
    )?;
    Ok((report, gate_passed))
}

fn require_complete_quality_gate(comparison: &mut BaselineComparison, has_omissions: bool) {
    if has_omissions {
        comparison.gate_passed = false;
    }
}

fn check_mutation_exit(run_mutations: bool, mutation: &MutationReport) -> u8 {
    if run_mutations {
        mutation_exit(mutation, false, false)
    } else {
        EXIT_OK
    }
}

fn check_exit(mutation_exit: u8, quality_gate_passed: bool) -> u8 {
    if mutation_exit == EXIT_OPERATIONAL {
        return EXIT_OPERATIONAL;
    }
    if quality_gate_passed {
        EXIT_OK
    } else {
        EXIT_QUALITY
    }
}

fn check_mutation_options(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    arguments: &args::CheckArgs,
    cancellation: CancellationToken,
) -> Result<MutationOptions> {
    if arguments.run_mutations {
        let test_command = arguments
            .test_command
            .clone()
            .or_else(|| config.mutation.test_command.clone())
            .filter(|command| !command.trim().is_empty())
            .ok_or_else(|| anyhow!("--run-mutations requires --test-command or configured command"))?;
        let mut options = MutationOptions::execute(CommandSpec::shell(test_command));
        options.validation_command = config.mutation.validation_command.clone().map(CommandSpec::shell);
        options.timeout = positive_duration(config.mutation.timeout_seconds)?;
        options.max_mutants = config.mutation.max_mutants;
        options.max_source_bytes = request.max_source_bytes;
        options.cancellation = cancellation;
        Ok(options)
    } else {
        let mut options = MutationOptions::list();
        options.max_source_bytes = request.max_source_bytes;
        Ok(options)
    }
}

fn apply_parse_recovery_omissions(quality: &mut QualityAnalysis, parse_errors: usize) {
    if parse_errors == 0 {
        return;
    }
    let reason = "one or more selected files used parse-error recovery, so disappeared source-derived baseline rows cannot be classified as resolved";
    for rule_id in "crap.maximum,dry.clone,kiss.cyclomatic-complexity,kiss.nesting-depth,kiss.function-statements,kiss.parameter-count,cohesion.module,mutation.score,mutation.surviving-mutant"
        .split(',')
    {
        quality.omitted.push(OmittedCheck {
            rule_id: rule_id.to_string(),
            reason: reason.to_string(),
        });
    }
}

fn configured_baseline_path(config: &RepoRigorConfig) -> Result<Option<String>> {
    if !config.baseline.enabled {
        return Ok(None);
    }
    config
        .baseline
        .path
        .to_str()
        .map(|path| Some(path.to_string()))
        .ok_or_else(|| anyhow!("baseline path must be valid UTF-8"))
}

fn load_baseline_rules(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    expected_scope: &str,
) -> Result<Option<Vec<RuleResult>>> {
    if !config.baseline.enabled {
        return Ok(None);
    }
    load_enabled_baseline_rules(request, config, expected_scope).map(Some)
}

fn load_enabled_baseline_rules(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    expected_scope: &str,
) -> Result<Vec<RuleResult>> {
    let path = request.root.join(&config.baseline.path);
    let report = read_baseline_report(request, &path)?;
    extract_baseline_rules(report, &path, expected_scope)
}

fn read_baseline_report(request: &AnalysisRequest, path: &Path) -> Result<ReportEnvelope> {
    let max_bytes = u64::try_from(request.max_source_bytes)
        .map_err(|_| anyhow!("configured source byte limit cannot bound the baseline report"))?;
    let contents = read_bounded_utf8_file_within(&request.root, path, max_bytes)
        .with_context(|| format!("failed to read RepoRigor baseline report {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse RepoRigor baseline report {}", path.display()))
}

fn extract_baseline_rules(
    report: ReportEnvelope,
    path: &Path,
    expected_scope: &str,
) -> Result<Vec<RuleResult>> {
    validate_baseline_identity(&report, path)?;
    let rules = report.results.rules.ok_or_else(|| {
        anyhow!(
            "RepoRigor baseline report {} has no integrated rule results",
            path.display()
        )
    })?;
    validate_baseline_scope(rules.analysis_scope.as_deref(), path, expected_scope)?;
    Ok(rules.results)
}

fn validate_baseline_identity(report: &ReportEnvelope, path: &Path) -> Result<()> {
    if report.schema_version != REPORT_SCHEMA_VERSION
        || report.tool.name != "reporigor"
        || report.command != ReportCommand::Check
    {
        bail!(
            "baseline {} must be an ordinary RepoRigor schema-v{} check report",
            path.display(),
            REPORT_SCHEMA_VERSION
        );
    }
    Ok(())
}

fn validate_baseline_scope(actual_scope: Option<&str>, path: &Path, expected_scope: &str) -> Result<()> {
    if actual_scope != Some(expected_scope) {
        bail!(
            "RepoRigor baseline report {} was produced with a different analysis scope; regenerate it explicitly with the exact intended check selection and thresholds",
            path.display()
        );
    }
    Ok(())
}

fn check_scope_fingerprint(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    cargo: &CargoOptions,
    arguments: &args::CheckArgs,
    allow_project_exec: bool,
) -> Result<String> {
    let mut scoped_config = config.clone();
    scoped_config.baseline = BaselineConfig::default();
    let mut filters = request.filters.clone();
    filters.sort();
    filters.dedup();
    let mut features = cargo.features.clone();
    features.sort();
    features.dedup();
    let coverage = arguments.input.threshold.coverage.as_ref().map(|path| {
        path.strip_prefix(&request.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    });
    let scope = serde_json::json!({
        "version": 1,
        "request": {
            "languages": request.languages,
            "filters": filters,
            "include_tests": request.include_tests,
            "allow_parse_errors": request.allow_parse_errors,
            "max_source_bytes": request.max_source_bytes,
            "backend": request.backend,
            "allow_project_exec": allow_project_exec,
        },
        "config": scoped_config,
        "cargo": {
            "features": features,
            "all_features": cargo.all_features,
            "no_default_features": cargo.no_default_features,
        },
        "check": {
            "coverage": coverage,
            "fail_over": arguments.input.threshold.fail_over,
            "min_tokens": arguments.min_tokens,
            "run_mutations": arguments.run_mutations,
            "test_command": arguments.test_command,
        },
    });
    let evidence = serde_json::to_string(&scope).context("failed to serialize deterministic check scope")?;
    Ok(stable_id(
        "reporigor.check.analysis-scope",
        "Cargo.toml",
        "reporigor check",
        &evidence,
    ))
}

fn run_providers(request: &AnalysisRequest, preflight: bool, format: FormatArg) -> Result<u8> {
    if !matches!(format, FormatArg::Text | FormatArg::Json) {
        bail!("providers supports only --format text or --format json");
    }
    let adapter = ProjectAdapter::default();
    let resolution = project_provider_resolution(&adapter, request, preflight)?;
    let mutation = mutation_provider_inventory(&request.root, preflight)?;
    emit_providers(&resolution, &mutation, format)?;
    Ok(EXIT_OK)
}

fn project_provider_resolution(
    adapter: &ProjectAdapter,
    request: &AnalysisRequest,
    preflight: bool,
) -> Result<ProviderResolution> {
    if preflight {
        adapter.preflight(request).map_err(Into::into)
    } else {
        adapter.discover(request).map_err(Into::into)
    }
}

fn mutation_provider_inventory(root: &Path, preflight: bool) -> Result<ProviderInventory> {
    if preflight {
        preflight_mutation_providers(root).map_err(Into::into)
    } else {
        discover_mutation_providers(root).map_err(Into::into)
    }
}

fn emit_providers(
    resolution: &ProviderResolution,
    mutation: &ProviderInventory,
    format: FormatArg,
) -> Result<()> {
    if format == FormatArg::Json {
        let mut rendered = render_providers_json(resolution, mutation)?;
        rendered.push('\n');
        write_stdout(&rendered)?;
    } else {
        write_stdout(&render_provider_tables(resolution, mutation)?)?;
    }
    Ok(())
}

fn render_providers_json(resolution: &ProviderResolution, mutation: &ProviderInventory) -> Result<String> {
    let mut document = serde_json::to_value(resolution)?;
    let fields = document
        .as_object_mut()
        .ok_or_else(|| anyhow!("project provider resolution must serialize as an object"))?;
    fields.insert("mutation".to_string(), serde_json::to_value(mutation)?);
    serde_json::to_string_pretty(&document).map_err(Into::into)
}

fn analyze_project(
    request: &AnalysisRequest,
    cargo: CargoOptions,
    allow_project_exec: bool,
) -> Result<AnalysisSnapshot> {
    let project_adapter = ProjectAdapter::default();
    let provider_resolution = analysis_provider_resolution(&project_adapter, request, allow_project_exec)?;
    enforce_native_project_providers(request, &provider_resolution)?;
    let mut snapshot = initial_analysis_snapshot(request, &provider_resolution, allow_project_exec);
    let mut sources = provider_resolution.context.sources.clone();
    route_rust_sources(
        request,
        cargo,
        allow_project_exec,
        &provider_resolution.context,
        &mut sources,
        &mut snapshot,
    )?;
    let clang_authoritative_files =
        route_clang_sources(request, allow_project_exec, &mut sources, &mut snapshot)?;
    analyze_generic_sources(request, sources, &clang_authoritative_files, &mut snapshot)?;
    snapshot.assign_mutation_ids();
    Ok(snapshot)
}

fn analysis_provider_resolution(
    adapter: &ProjectAdapter,
    request: &AnalysisRequest,
    allow_project_exec: bool,
) -> Result<ProviderResolution> {
    if request.backend == BackendPreference::Generic || !allow_project_exec {
        adapter.discover(request).map_err(Into::into)
    } else {
        adapter.preflight(request).map_err(Into::into)
    }
}

fn enforce_native_project_providers(
    request: &AnalysisRequest,
    resolution: &ProviderResolution,
) -> Result<()> {
    if request.backend == BackendPreference::Native {
        enforce_project_providers(resolution)?;
    }
    Ok(())
}

fn initial_analysis_snapshot(
    request: &AnalysisRequest,
    resolution: &ProviderResolution,
    allow_project_exec: bool,
) -> AnalysisSnapshot {
    let mut snapshot = AnalysisSnapshot::default();
    merge_context_metadata(&mut snapshot, &resolution.context);
    if request.backend == BackendPreference::Auto && !allow_project_exec {
        snapshot.diagnostics.push(fallback_diagnostic(
            "project-router",
            "project execution is disabled by default; using filesystem-only discovery and generic syntax analysis; pass --allow-project-exec to permit existing project toolchains"
                .to_string(),
        ));
    }
    snapshot
}

fn route_rust_sources(
    request: &AnalysisRequest,
    cargo: CargoOptions,
    allow_project_exec: bool,
    context: &ProjectContext,
    sources: &mut Vec<SourceFile>,
    snapshot: &mut AnalysisSnapshot,
) -> Result<()> {
    if !rust_native_requested(request, sources, allow_project_exec) {
        return Ok(());
    }
    if !context.kinds.contains(&ProjectKind::Cargo) {
        return handle_missing_rust_project(request, snapshot);
    }
    let Some((rust, generic_files)) = analyze_native_rust(request, cargo, sources, snapshot)? else {
        return Ok(());
    };
    merge_snapshot(snapshot, rust);
    sources.retain(|source| source.language != Language::Rust || generic_files.contains(&source.relative));
    Ok(())
}

fn rust_native_requested(
    request: &AnalysisRequest,
    sources: &[SourceFile],
    allow_project_exec: bool,
) -> bool {
    allow_project_exec
        && request.backend != BackendPreference::Generic
        && sources.iter().any(|source| source.language == Language::Rust)
}

fn handle_missing_rust_project(request: &AnalysisRequest, snapshot: &mut AnalysisSnapshot) -> Result<()> {
    if request.backend == BackendPreference::Native {
        return Err(CoreError::BackendUnavailable {
            backend: "rust-native".to_string(),
            message: "Rust sources require Cargo.toml in --backend native mode".to_string(),
        }
        .into());
    }
    snapshot.diagnostics.push(fallback_diagnostic(
        "rust-router",
        "Cargo.toml is unavailable; using generic Rust syntax analysis".to_string(),
    ));
    Ok(())
}

fn analyze_native_rust(
    request: &AnalysisRequest,
    cargo: CargoOptions,
    sources: &[SourceFile],
    snapshot: &mut AnalysisSnapshot,
) -> Result<Option<(AnalysisSnapshot, BTreeSet<String>)>> {
    match RustAdapter::new(cargo).analyze_project(request) {
        Ok(mut rust) => {
            let generic_files = prepare_rust_fallback(request, sources, &mut rust)?;
            Ok(Some((rust, generic_files)))
        }
        Err(error) => native_error_fallback(
            request,
            snapshot,
            error,
            "rust-router",
            "native Rust analysis failed; using generic syntax backend",
        ),
    }
}

fn prepare_rust_fallback(
    request: &AnalysisRequest,
    sources: &[SourceFile],
    rust: &mut AnalysisSnapshot,
) -> Result<BTreeSet<String>> {
    let fallback_count = rust
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.backend == "rust-native" && diagnostic.fallback_used)
        .count();
    if fallback_count == 0 {
        return Ok(BTreeSet::new());
    }
    if request.backend == BackendPreference::Native {
        bail!("native Rust analysis requires generic fallback for {fallback_count} selected source file(s)");
    }
    let mut generic_files = rust_fallback_files(rust);
    if generic_files.len() < fallback_count {
        generic_files.extend(
            sources
                .iter()
                .filter(|source| source.language == Language::Rust)
                .map(|source| source.relative.clone()),
        );
    }
    retain_rust_fallback_files(rust, &generic_files, fallback_count);
    Ok(generic_files)
}

fn rust_fallback_files(rust: &AnalysisSnapshot) -> BTreeSet<String> {
    rust.diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.backend == "rust-native" && diagnostic.fallback_used)
        .filter_map(|diagnostic| diagnostic.location.as_ref().map(|location| location.file.clone()))
        .collect()
}

fn retain_rust_fallback_files(
    rust: &mut AnalysisSnapshot,
    generic_files: &BTreeSet<String>,
    fallback_count: usize,
) {
    rust.files
        .retain(|source| !generic_files.contains(&source.relative));
    rust.functions
        .retain(|function| !generic_files.contains(&function.file));
    rust.tokens.retain(|file, _tokens| !generic_files.contains(file));
    rust.mutations
        .retain(|mutation| !generic_files.contains(&mutation.file));
    rust.parse_errors = rust.parse_errors.saturating_sub(fallback_count);
    rust.repository.module_graph_reliable = false;
    rust.repository.identifier_counts_reliable = false;
    rust.repository.feature_inventory_reliable = false;
    rust.repository.trait_inventory_reliable = false;
    rust.repository.test_inventory_reliable = false;
    rust.repository.unreachable_inventory_reliable = false;
}

fn route_clang_sources(
    request: &AnalysisRequest,
    allow_project_exec: bool,
    sources: &mut Vec<SourceFile>,
    snapshot: &mut AnalysisSnapshot,
) -> Result<BTreeSet<String>> {
    let inspect_database = clang_database_should_be_inspected(request, sources);
    let database = discover_clang_database(request, allow_project_exec, inspect_database)?;
    if !clang_should_be_routed(request, sources, database.is_some()) || !allow_project_exec {
        return Ok(BTreeSet::new());
    }
    route_clang_tools(request, sources, snapshot, database, resolve_clang())
}

fn clang_database_should_be_inspected(request: &AnalysisRequest, sources: &[SourceFile]) -> bool {
    has_c_family_sources(sources)
        || (request.backend == BackendPreference::Native
            && (request.languages.is_empty()
                || request.languages.iter().any(|language| language.is_c_family())))
}

fn has_c_family_sources(sources: &[SourceFile]) -> bool {
    sources.iter().any(|source| source.language.is_c_family())
}

fn discover_clang_database(
    request: &AnalysisRequest,
    allow_project_exec: bool,
    inspect_database: bool,
) -> Result<Option<PathBuf>> {
    if request.backend == BackendPreference::Generic || !allow_project_exec || !inspect_database {
        return Ok(None);
    }
    ClangAdapter::discover(&request.root).map_err(Into::into)
}

fn clang_should_be_routed(request: &AnalysisRequest, sources: &[SourceFile], database_found: bool) -> bool {
    request.backend != BackendPreference::Generic
        && (has_c_family_sources(sources) || (request.backend == BackendPreference::Native && database_found))
}

fn route_clang_tools(
    request: &AnalysisRequest,
    sources: &mut Vec<SourceFile>,
    snapshot: &mut AnalysisSnapshot,
    database: Option<PathBuf>,
    compiler: Option<PathBuf>,
) -> Result<BTreeSet<String>> {
    match (database, compiler) {
        (Some(_), Some(compiler)) => analyze_native_clang(request, sources, snapshot, compiler),
        (Some(_), None) => missing_clang_resource(request, snapshot, MissingClangResource::Compiler),
        (None, _) => missing_clang_resource(request, snapshot, MissingClangResource::Database),
    }
}

#[derive(Clone, Copy)]
enum MissingClangResource {
    Compiler,
    Database,
}

impl MissingClangResource {
    const fn messages(self) -> (&'static str, &'static str) {
        match self {
            Self::Compiler => (
                "Clang is not available through a trusted absolute PATH entry",
                "Clang is not available through a trusted absolute PATH entry; using generic C-family syntax analysis",
            ),
            Self::Database => (
                "C-family native analysis requires an existing compile_commands.json",
                "no compile_commands.json found; using generic C-family syntax analysis",
            ),
        }
    }
}

fn missing_clang_resource(
    request: &AnalysisRequest,
    snapshot: &mut AnalysisSnapshot,
    missing: MissingClangResource,
) -> Result<BTreeSet<String>> {
    let (native_message, fallback_message) = missing.messages();
    if request.backend == BackendPreference::Native {
        return Err(CoreError::BackendUnavailable {
            backend: "clang".to_string(),
            message: native_message.to_string(),
        }
        .into());
    }
    snapshot
        .diagnostics
        .push(fallback_diagnostic("clang-router", fallback_message.to_string()));
    Ok(BTreeSet::new())
}

fn analyze_native_clang(
    request: &AnalysisRequest,
    sources: &mut Vec<SourceFile>,
    snapshot: &mut AnalysisSnapshot,
    compiler: PathBuf,
) -> Result<BTreeSet<String>> {
    match ClangAdapter::new(compiler).analyze_project_with_provenance(request) {
        Ok(analysis) => integrate_clang_analysis(request, sources, snapshot, analysis),
        Err(error) => native_error_fallback(
            request,
            snapshot,
            error,
            "clang-router",
            "Clang project validation failed; using generic syntax backend",
        ),
    }
}

fn native_error_fallback<T: Default>(
    request: &AnalysisRequest,
    snapshot: &mut AnalysisSnapshot,
    error: CoreError,
    backend: &str,
    message: &str,
) -> Result<T> {
    if native_error_requires_propagation(&error, request.backend) {
        return Err(error.into());
    }
    snapshot
        .diagnostics
        .push(fallback_diagnostic(backend, format!("{message}: {error}")));
    Ok(T::default())
}

fn native_error_requires_propagation(error: &CoreError, backend: BackendPreference) -> bool {
    matches!(error, CoreError::SourceTooLarge { .. }) || backend == BackendPreference::Native
}

fn integrate_clang_analysis(
    request: &AnalysisRequest,
    sources: &mut Vec<SourceFile>,
    snapshot: &mut AnalysisSnapshot,
    mut analysis: adapter_clang::ClangAnalysis,
) -> Result<BTreeSet<String>> {
    let failed_units = analysis
        .translation_units
        .iter()
        .filter(|unit| unit.source.is_some() && !matches!(unit.status, AstDumpStatus::Analyzed))
        .collect::<Vec<_>>();
    let authoritative = clang_authoritative_files(&analysis);
    if request.backend == BackendPreference::Native {
        integrate_native_clang_sources(sources, &analysis, &failed_units, &authoritative)?;
    } else {
        integrate_auto_clang_sources(sources, snapshot, &analysis, &failed_units, &authoritative);
    }
    analysis.snapshot.files.clear();
    merge_snapshot(snapshot, analysis.snapshot);
    Ok(authoritative)
}

fn clang_authoritative_files(analysis: &adapter_clang::ClangAnalysis) -> BTreeSet<String> {
    analysis
        .translation_units
        .iter()
        .filter(|unit| matches!(unit.status, AstDumpStatus::Analyzed))
        .filter_map(|unit| unit.source.as_ref())
        .map(|source| source.relative.clone())
        .collect()
}

fn integrate_native_clang_sources(
    sources: &mut Vec<SourceFile>,
    analysis: &adapter_clang::ClangAnalysis,
    failed_units: &[&adapter_clang::AstTranslationUnit],
    authoritative: &BTreeSet<String>,
) -> Result<()> {
    if authoritative.is_empty() {
        bail!("native Clang analysis produced no successfully analyzed selected translation units");
    }
    if !failed_units.is_empty() {
        bail!("one or more selected Clang translation units failed native AST analysis");
    }
    sources.retain(|source| !source.language.is_c_family());
    sources.extend(analysis.project.context.sources.clone());
    Ok(())
}

fn integrate_auto_clang_sources(
    sources: &mut Vec<SourceFile>,
    snapshot: &mut AnalysisSnapshot,
    analysis: &adapter_clang::ClangAnalysis,
    failed_units: &[&adapter_clang::AstTranslationUnit],
    authoritative: &BTreeSet<String>,
) {
    sources.extend(analysis.project.context.sources.clone());
    let failed_files = diagnose_failed_clang_units(snapshot, failed_units, authoritative);
    diagnose_unrepresented_clang_sources(snapshot, sources, authoritative, &failed_files);
}

fn diagnose_failed_clang_units(
    snapshot: &mut AnalysisSnapshot,
    failed_units: &[&adapter_clang::AstTranslationUnit],
    authoritative: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut failed_files = BTreeSet::new();
    for unit in failed_units {
        let Some(source) = unit.source.as_ref() else {
            continue;
        };
        if authoritative.contains(&source.relative) {
            continue;
        }
        failed_files.insert(source.relative.clone());
        snapshot.diagnostics.push(fallback_diagnostic(
            "clang-router",
            format!(
                "native Clang AST analysis did not complete for {} (database entry {}); using generic syntax functions and complexity",
                source.relative, unit.command_index
            ),
        ));
    }
    failed_files
}

fn diagnose_unrepresented_clang_sources(
    snapshot: &mut AnalysisSnapshot,
    sources: &[SourceFile],
    authoritative: &BTreeSet<String>,
    failed_files: &BTreeSet<String>,
) {
    for source in sources.iter().filter(|source| source.language.is_c_family()) {
        if authoritative.contains(&source.relative) || failed_files.contains(&source.relative) {
            continue;
        }
        snapshot.diagnostics.push(fallback_diagnostic(
            "clang-router",
            format!(
                "no successfully analyzed selected compilation-database entry represents {}; using generic syntax functions and complexity",
                source.relative
            ),
        ));
    }
}

fn analyze_generic_sources(
    request: &AnalysisRequest,
    mut sources: Vec<SourceFile>,
    clang_authoritative_files: &BTreeSet<String>,
    snapshot: &mut AnalysisSnapshot,
) -> Result<()> {
    sources.sort_by(|left, right| left.relative.cmp(&right.relative));
    sources.dedup_by(|left, right| left.relative == right.relative && left.language == right.language);
    let generic = TreeSitterBackend::new();
    for source in sources {
        analyze_generic_source(request, generic, &source, clang_authoritative_files, snapshot)?;
    }
    Ok(())
}

fn analyze_generic_source(
    request: &AnalysisRequest,
    generic: TreeSitterBackend,
    source: &SourceFile,
    clang_authoritative_files: &BTreeSet<String>,
    snapshot: &mut AnalysisSnapshot,
) -> Result<()> {
    let clang_native = clang_authoritative_files.contains(&source.relative);
    let mut syntax_request = request.clone();
    if clang_native {
        syntax_request.allow_parse_errors = true;
    }
    let mut file = generic.analyze_file(&request.root, source, &syntax_request)?;
    if clang_native {
        merge_clang_structural_facts(snapshot, &file);
        file.functions.clear();
    }
    snapshot.push(file);
    Ok(())
}

fn merge_clang_structural_facts(snapshot: &mut AnalysisSnapshot, generic: &FileAnalysis) {
    let candidate_sets = generic
        .functions
        .iter()
        .map(|structural| clang_structural_candidates(&snapshot.functions, structural))
        .collect::<Vec<_>>();
    let unique_claims = unique_structural_claims(&candidate_sets);
    for (structural, candidates) in generic.functions.iter().zip(candidate_sets) {
        merge_unique_structural_fact(snapshot, structural, &candidates, &unique_claims);
    }
}

fn unique_structural_claims(candidate_sets: &[Vec<usize>]) -> BTreeMap<usize, usize> {
    let mut unique_claims = BTreeMap::new();
    for candidates in candidate_sets {
        if let [index] = candidates.as_slice() {
            *unique_claims.entry(*index).or_default() += 1;
        }
    }
    unique_claims
}

fn merge_unique_structural_fact(
    snapshot: &mut AnalysisSnapshot,
    structural: &FunctionRecord,
    candidates: &[usize],
    unique_claims: &BTreeMap<usize, usize>,
) {
    let [index] = candidates else {
        return;
    };
    if unique_claims.get(index) != Some(&1) {
        return;
    }
    copy_structural_fact(&mut snapshot.functions[*index], structural);
}

fn copy_structural_fact(native: &mut FunctionRecord, structural: &FunctionRecord) {
    native.nesting_depth = structural.nesting_depth;
    native.statement_count = structural.statement_count;
    native.parameter_count = structural.parameter_count;
    native.normalized_tokens.clone_from(&structural.normalized_tokens);
    native.references.clone_from(&structural.references);
    native
        .coverage_excluded_ranges
        .clone_from(&structural.coverage_excluded_ranges);
    native.visibility = structural.visibility;
    native.structural_metrics_reliable = structural.structural_metrics_reliable;
    native.production = structural.production;
    native.entry_point = structural.entry_point;
}

fn clang_structural_candidates(native: &[FunctionRecord], structural: &FunctionRecord) -> Vec<usize> {
    let mut candidates = native
        .iter()
        .enumerate()
        .filter(|(_, candidate)| {
            candidate.file == structural.file
                && (candidate.name == structural.name
                    || function_leaf(&candidate.name) == function_leaf(&structural.name))
                && candidate.start_line <= structural.end_line
                && structural.start_line <= candidate.end_line
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    if candidates.len() > 1 {
        let exact_names = candidates
            .iter()
            .copied()
            .filter(|index| native[*index].name == structural.name)
            .collect::<Vec<_>>();
        if !exact_names.is_empty() {
            candidates = exact_names;
        }
    }
    if candidates.len() > 1 {
        let Some(signature) = generic_parameter_signature(&structural.stable_symbol) else {
            return Vec::new();
        };
        candidates.retain(|index| {
            clang_parameter_signature(&native[*index].stable_symbol).as_ref() == Some(&signature)
        });
    }
    candidates
}

fn generic_parameter_signature(stable_symbol: &str) -> Option<Vec<String>> {
    let symbol = stable_symbol
        .split_once('#')
        .map_or(stable_symbol, |(base, _)| base);
    let expected_close = symbol.strip_suffix(')')?.len();
    let (open, close) = parenthesis_groups(symbol)?
        .into_iter()
        .find(|(_, close)| *close == expected_close)?;
    canonical_parameter_signature(&symbol[open + 1..close])
}

fn clang_parameter_signature(stable_symbol: &str) -> Option<Vec<String>> {
    let type_evidence = clang_type_evidence(stable_symbol)?;
    let (open, close) = clang_parameter_group(type_evidence)?;
    let clause = type_evidence[open + 1..close].trim();
    if clause.starts_with('*') || clause.starts_with('&') {
        return None;
    }
    canonical_parameter_signature(clause)
}

fn clang_type_evidence(stable_symbol: &str) -> Option<&str> {
    let (_, evidence) = stable_symbol.rsplit_once("[type:")?;
    evidence.strip_suffix(']')
}

fn clang_parameter_group(type_evidence: &str) -> Option<(usize, usize)> {
    let groups = parenthesis_groups(type_evidence)?;
    let spaced = groups
        .iter()
        .copied()
        .filter(|(open, _)| {
            type_evidence[..*open]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        })
        .collect::<Vec<_>>();
    Some(match spaced.as_slice() {
        [group] => *group,
        [] if groups.len() == 1 => groups[0],
        _ => return None,
    })
}

#[derive(Default)]
struct ParenthesisScan {
    depth: usize,
    start: Option<usize>,
    groups: Vec<(usize, usize)>,
}

impl ParenthesisScan {
    fn observe(&mut self, index: usize, character: char) -> Option<()> {
        match character {
            '(' => self.open(index),
            ')' => self.close(index),
            _ => Some(()),
        }
    }

    fn open(&mut self, index: usize) -> Option<()> {
        if self.depth == 0 {
            self.start = Some(index);
        }
        self.depth = self.depth.checked_add(1)?;
        Some(())
    }

    fn close(&mut self, index: usize) -> Option<()> {
        self.depth = self.depth.checked_sub(1)?;
        if self.depth == 0 {
            self.groups.push((self.start.take()?, index));
        }
        Some(())
    }
}

fn parenthesis_groups(value: &str) -> Option<Vec<(usize, usize)>> {
    let mut scan = ParenthesisScan::default();
    for (index, character) in value.char_indices() {
        scan.observe(index, character)?;
    }
    (scan.depth == 0).then_some(scan.groups)
}

fn canonical_parameter_signature(clause: &str) -> Option<Vec<String>> {
    let mut state = ParameterSignatureState::default();
    for token in parameter_tokens(clause) {
        state.consume(token)?;
    }
    state.finish()
}

#[derive(Default)]
struct ParameterSignatureState {
    signature: Vec<String>,
    nesting: [usize; 4],
    skipping_default: bool,
}

impl ParameterSignatureState {
    fn consume(&mut self, token: String) -> Option<()> {
        if self.skipping_default {
            return self.consume_default(token);
        }
        if token == "=" && signature_top_level(&self.nesting) {
            self.skipping_default = true;
            return Some(());
        }
        update_signature_nesting(&token, &mut self.nesting)?;
        if token != "LOCAL" {
            self.signature.push(token);
        }
        Some(())
    }

    fn consume_default(&mut self, token: String) -> Option<()> {
        update_signature_nesting(&token, &mut self.nesting)?;
        if token == "," && signature_top_level(&self.nesting) {
            self.signature.push(token);
            self.skipping_default = false;
        }
        Some(())
    }

    fn finish(self) -> Option<Vec<String>> {
        signature_top_level(&self.nesting).then_some(self.signature)
    }
}

fn parameter_tokens(clause: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut characters = clause.chars().peekable();
    while let Some(character) = characters.next() {
        if character.is_whitespace() {
            continue;
        }
        tokens.push(parameter_token(character, &mut characters));
    }
    tokens
}

fn parameter_token(first: char, characters: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut token = first.to_string();
    if !signature_identifier_character(first) {
        return token;
    }
    while characters
        .peek()
        .is_some_and(|character| signature_identifier_character(*character))
    {
        token.push(characters.next().unwrap_or_default());
    }
    token
}

fn signature_identifier_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

fn signature_top_level(nesting: &[usize; 4]) -> bool {
    nesting.iter().all(|depth| *depth == 0)
}

fn update_signature_nesting(token: &str, nesting: &mut [usize; 4]) -> Option<()> {
    let pairs = [("(", ")"), ("[", "]"), ("{", "}"), ("<", ">")];
    let Some((index, opening)) = pairs.iter().enumerate().find_map(|(index, pair)| {
        (pair.0 == token)
            .then_some((index, true))
            .or_else(|| (pair.1 == token).then_some((index, false)))
    }) else {
        return Some(());
    };
    if opening {
        nesting[index] = nesting[index].saturating_add(1);
    } else {
        nesting[index] = nesting[index].checked_sub(1)?;
    }
    Some(())
}

fn function_leaf(name: &str) -> &str {
    let after_colons = name.rsplit_once("::").map_or(name, |(_, leaf)| leaf);
    after_colons
        .rsplit_once('.')
        .map_or(after_colons, |(_, leaf)| leaf)
}

fn crap_analysis(
    request: &AnalysisRequest,
    snapshot: &AnalysisSnapshot,
    coverage: Option<&Path>,
    unreported_as_zero: bool,
) -> Result<CrapAnalysis> {
    match coverage {
        Some(path) => analyze_path_with_policy(
            &request.root,
            snapshot.functions.clone(),
            path,
            unreported_as_zero,
        )
        .with_context(|| format!("failed to load coverage from {}", path.display())),
        None => Ok(analyze_with_policy(
            &request.root,
            snapshot.functions.clone(),
            None,
            unreported_as_zero,
        )),
    }
}

fn emit_report(
    report: &ReportEnvelope,
    format: FormatArg,
    snapshot: &AnalysisSnapshot,
    max_source_bytes: usize,
) -> Result<()> {
    let mut rendered = render_report(report, format, snapshot, max_source_bytes)?;
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    write_stdout(&rendered)
}

fn render_report(
    report: &ReportEnvelope,
    format: FormatArg,
    snapshot: &AnalysisSnapshot,
    max_source_bytes: usize,
) -> Result<String> {
    match format {
        FormatArg::Text => Ok(report.to_human()),
        FormatArg::Json => report.to_pretty_json().map_err(Into::into),
        FormatArg::Sarif => report.to_sarif_json().map_err(Into::into),
        FormatArg::MutationJson => render_mutation_elements(report, snapshot, max_source_bytes),
    }
}

fn render_mutation_elements(
    report: &ReportEnvelope,
    snapshot: &AnalysisSnapshot,
    max_source_bytes: usize,
) -> Result<String> {
    mutation_sources(report.root(), snapshot, max_source_bytes)
        .and_then(|sources| render_mutation_sources(report, &sources))
}

fn render_mutation_sources(report: &ReportEnvelope, sources: &BTreeMap<String, String>) -> Result<String> {
    let thresholds = MutationThresholds::new(60, 80).map_err(anyhow::Error::from)?;
    report
        .to_mutation_elements_json(sources, thresholds)
        .map_err(Into::into)
}

fn ensure_sources_selected(request: &AnalysisRequest, snapshot: &AnalysisSnapshot) -> Result<()> {
    if snapshot.files.is_empty() {
        no_sources_selected(request)
    } else {
        Ok(())
    }
}

fn no_sources_selected<T>(request: &AnalysisRequest) -> Result<T> {
    bail!(
        "no source files were selected under {}; check --language, --filter, --include-tests, and ignore rules",
        request.root.display()
    )
}

fn write_stdout(rendered: &str) -> Result<()> {
    match io::stdout().lock().write_all(rendered.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error).context("failed to write command output"),
    }
}

fn mutation_sources(
    root: &Path,
    snapshot: &AnalysisSnapshot,
    max_source_bytes: usize,
) -> Result<BTreeMap<String, String>> {
    let files = mutation_files(&snapshot.mutations);
    observe_mutation_source_budget(root, &files, max_source_bytes)?;
    read_mutation_sources(root, files, max_source_bytes)
}

fn mutation_files(mutations: &[MutationCandidate]) -> BTreeMap<String, Vec<&MutationCandidate>> {
    let mut files = BTreeMap::<String, Vec<&MutationCandidate>>::new();
    for mutation in mutations {
        files.entry(mutation.file.clone()).or_default().push(mutation);
    }
    files
}

fn observe_mutation_source_budget(
    root: &Path,
    files: &BTreeMap<String, Vec<&MutationCandidate>>,
    max_source_bytes: usize,
) -> Result<()> {
    let mut budget = SourceBudget::new(max_source_bytes)?;
    for relative in files.keys() {
        observe_mutation_source(root, relative, &mut budget)?;
    }
    Ok(())
}

fn observe_mutation_source(root: &Path, relative: &str, budget: &mut SourceBudget) -> Result<()> {
    let path = root.join(relative);
    let canonical = resolve_optional_regular_file_within(root, &path)?
        .ok_or_else(|| anyhow!("mutation source {} does not exist", path.display()))?;
    let metadata = canonical.metadata().map_err(|source| CoreError::Read {
        path: path.display().to_string(),
        source,
    })?;
    budget.observe(&path, metadata.len()).map_err(Into::into)
}

fn read_mutation_sources(
    root: &Path,
    files: BTreeMap<String, Vec<&MutationCandidate>>,
    max_source_bytes: usize,
) -> Result<BTreeMap<String, String>> {
    let max_source_bytes = u64::try_from(max_source_bytes).unwrap_or(u64::MAX);
    let mut sources = BTreeMap::new();
    for (relative, mutations) in files {
        let source = read_mutation_source(root, &relative, &mutations, max_source_bytes)?;
        sources.insert(relative, source);
    }
    Ok(sources)
}

fn read_mutation_source(
    root: &Path,
    relative: &str,
    mutations: &[&MutationCandidate],
    max_source_bytes: u64,
) -> Result<String> {
    let path = root.join(relative);
    let source = read_bounded_utf8_file_within(root, &path, max_source_bytes)
        .with_context(|| format!("failed to read mutation source {}", path.display()))?;
    validate_mutation_source(relative, &source, mutations)?;
    Ok(source)
}

fn validate_mutation_source(relative: &str, source: &str, mutations: &[&MutationCandidate]) -> Result<()> {
    for mutation in mutations {
        let Some(actual) = source.get(mutation.start_byte..mutation.end_byte) else {
            bail!(
                "mutation candidate {} for {relative} has an invalid UTF-8 byte span {}..{}",
                mutation.id,
                mutation.start_byte,
                mutation.end_byte
            );
        };
        if actual != mutation.original {
            bail!(
                "mutation candidate {} for {relative} is stale: byte span {}..{} no longer matches its original text",
                mutation.id,
                mutation.start_byte,
                mutation.end_byte
            );
        }
    }
    Ok(())
}

fn mutation_exit(report: &MutationReport, allow_survivors: bool, allow_compile_errors: bool) -> u8 {
    let summary = &report.summary;
    if mutation_has_operational_error(summary, allow_compile_errors) {
        EXIT_OPERATIONAL
    } else if !allow_survivors && summary.survived > 0 {
        EXIT_QUALITY
    } else {
        EXIT_OK
    }
}

fn mutation_has_operational_error(
    summary: &reporigor_reporting::MutationSummary,
    allow_compile_errors: bool,
) -> bool {
    [summary.invalid, summary.runtime_error, summary.timeout]
        .into_iter()
        .any(|count| count > 0)
        || (!allow_compile_errors && summary.compile_error > 0)
}

fn positive_duration(seconds: f64) -> Result<Duration> {
    checked_duration_from_secs_f64(seconds).map_err(|error| anyhow!("timeout {error}"))
}

fn merge_snapshot(target: &mut AnalysisSnapshot, mut other: AnalysisSnapshot) {
    target.files.append(&mut other.files);
    for backend in other.backends {
        if !target.backends.iter().any(|existing| existing.id == backend.id) {
            target.backends.push(backend);
        }
    }
    target.functions.append(&mut other.functions);
    target.tokens.append(&mut other.tokens);
    target.mutations.append(&mut other.mutations);
    target.repository.merge(other.repository);
    target.diagnostics.append(&mut other.diagnostics);
    target.parse_errors += other.parse_errors;
}

fn merge_context_metadata(snapshot: &mut AnalysisSnapshot, context: &ProjectContext) {
    for backend in &context.backends {
        if !snapshot.backends.iter().any(|existing| existing.id == backend.id) {
            snapshot.backends.push(backend.clone());
        }
    }
    snapshot.diagnostics.extend(context.diagnostics.clone());
}

fn fallback_diagnostic(backend: &str, message: String) -> Diagnostic {
    Diagnostic {
        severity: Severity::Warning,
        backend: backend.to_string(),
        message,
        location: None,
        fallback_used: true,
    }
}

fn enforce_project_providers(resolution: &ProviderResolution) -> Result<()> {
    let unavailable: Vec<_> = resolution
        .inventory
        .iter()
        .filter(|status| status.required_for_native && status.applicable && !status.available)
        .map(|status| {
            format!(
                "{} ({})",
                status.id,
                status
                    .reason
                    .as_deref()
                    .unwrap_or("provider prerequisite is unavailable")
            )
        })
        .collect();
    if unavailable.is_empty() {
        Ok(())
    } else {
        bail!(
            "--backend native requires available project providers: {}",
            unavailable.join(", ")
        )
    }
}

fn render_provider_tables(resolution: &ProviderResolution, inventory: &ProviderInventory) -> Result<String> {
    let mut rendered = String::new();
    render_provider_table(&mut rendered, resolution)?;
    rendered.push('\n');
    render_mutation_provider_table(&mut rendered, inventory)?;
    Ok(rendered)
}

fn render_provider_table(rendered: &mut String, resolution: &ProviderResolution) -> std::fmt::Result {
    writeln!(
        rendered,
        "PROVIDER  ROLE  APPLICABLE  AVAILABLE  EXECUTABLE  VERSION  FALLBACK"
    )?;
    for status in &resolution.inventory {
        render_provider_status(rendered, ProviderDisplay::Project(status))?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ProviderDisplay<'a> {
    Project(&'a ProviderStatus),
    Mutation(&'a MutationProviderStatus),
}

struct ProviderIdentity<'a> {
    id: &'a str,
    applicable: bool,
    available: bool,
    executable: Option<&'a Path>,
    version: Option<&'a str>,
    reason: Option<&'a str>,
    hint: Option<&'a str>,
}

trait ProviderIdentitySource {
    fn provider_identity(&self) -> ProviderIdentity<'_>;
}

macro_rules! provider_identity_source {
    ($provider:ty) => {
        impl ProviderIdentitySource for $provider {
            fn provider_identity(&self) -> ProviderIdentity<'_> {
                ProviderIdentity {
                    id: self.id.as_str(),
                    applicable: self.applicable,
                    available: self.available,
                    executable: self.executable.as_deref(),
                    version: self.version.as_deref(),
                    reason: self.reason.as_deref(),
                    hint: self.hint.as_deref(),
                }
            }
        }
    };
}

provider_identity_source!(ProviderStatus);
provider_identity_source!(MutationProviderStatus);

fn display_identity(status: ProviderDisplay<'_>) -> ProviderIdentity<'_> {
    match status {
        ProviderDisplay::Project(status) => status.provider_identity(),
        ProviderDisplay::Mutation(status) => status.provider_identity(),
    }
}

fn render_provider_status(rendered: &mut String, status: ProviderDisplay<'_>) -> std::fmt::Result {
    let identity = display_identity(status);
    let (second, mode, tail) = match status {
        ProviderDisplay::Project(status) => (
            provider_role(status.required_for_native).to_owned(),
            None,
            escape_terminal_text(status.fallback.as_deref().unwrap_or("-")),
        ),
        ProviderDisplay::Mutation(status) => (
            status.default.to_string(),
            Some(mutation_provider_mode(status.execution_enabled).to_owned()),
            status
                .import_formats
                .iter()
                .copied()
                .map(mutation_import_name)
                .collect::<Vec<_>>()
                .join(","),
        ),
    };
    let mut columns = provider_identity_columns(
        identity.id,
        identity.applicable,
        identity.available,
        identity.executable,
        identity.version,
    );
    columns.insert(1, second);
    if let Some(mode) = mode {
        columns.insert(4, mode);
    }
    columns.push(tail);
    render_provider_row(rendered, &columns)?;
    render_provider_details(rendered, identity.reason, identity.hint)
}

fn provider_identity_columns(
    id: &str,
    applicable: bool,
    available: bool,
    executable: Option<&Path>,
    version: Option<&str>,
) -> Vec<String> {
    vec![
        escape_terminal_text(id),
        applicable.to_string(),
        available.to_string(),
        escape_terminal_text(&display_executable(executable)),
        escape_terminal_text(version.unwrap_or("-")),
    ]
}

fn render_provider_row(rendered: &mut String, columns: &[String]) -> std::fmt::Result {
    writeln!(rendered, "{}", columns.join("  "))
}

fn provider_role(required: bool) -> &'static str {
    if required {
        "required"
    } else {
        "optional"
    }
}

fn render_provider_details(
    rendered: &mut String,
    reason: Option<&str>,
    hint: Option<&str>,
) -> std::fmt::Result {
    for (label, value) in [("reason", reason), ("hint", hint)] {
        if let Some(value) = value {
            writeln!(rendered, "  {label}: {}", escape_terminal_text(value))?;
        }
    }
    Ok(())
}

fn render_mutation_provider_table(rendered: &mut String, inventory: &ProviderInventory) -> std::fmt::Result {
    writeln!(
        rendered,
        "MUTATION_PROVIDER  DEFAULT  APPLICABLE  AVAILABLE  MODE  EXECUTABLE  VERSION  IMPORTS"
    )?;
    for status in &inventory.providers {
        render_provider_status(rendered, ProviderDisplay::Mutation(status))?;
    }
    Ok(())
}

fn display_executable(executable: Option<&Path>) -> String {
    executable.map_or_else(|| "-".to_owned(), |path| path.display().to_string())
}

fn mutation_import_name(format: ImportFormat) -> &'static str {
    match format {
        ImportFormat::MutationTestingElementsV1 => "mte-v1",
        ImportFormat::MutationTestingElementsV2 => "mte-v2",
        ImportFormat::CargoMutantsOutcomes => "cargo-mutants-outcomes",
        ImportFormat::MuterJson => "muter-json",
    }
}

fn mutation_provider_mode(execution_enabled: bool) -> &'static str {
    if execution_enabled {
        "execute"
    } else {
        "import-only"
    }
}

fn resolve_cargo() -> Option<PathBuf> {
    env::var_os("CARGO")
        .map(PathBuf::from)
        .map(|path| anchor_executable(&path))
        .filter(|path| path.is_file())
        .or_else(|| find_on_path(OsStr::new(if cfg!(windows) { "cargo.exe" } else { "cargo" })))
        .or_else(|| cargo_from_directory_env("CARGO_HOME", "bin"))
        .or_else(|| {
            cargo_from_directory_env(if cfg!(windows) { "USERPROFILE" } else { "HOME" }, ".cargo/bin")
        })
}

fn cargo_from_directory_env(variable: &str, relative_bin: &str) -> Option<PathBuf> {
    env::var_os(variable)
        .map(PathBuf::from)
        .map(|directory| directory.join(relative_bin).join(cargo_executable_name()))
        .map(|path| anchor_executable(&path))
        .filter(|path| path.is_file())
}

const fn cargo_executable_name() -> &'static str {
    if cfg!(windows) {
        "cargo.exe"
    } else {
        "cargo"
    }
}

fn find_on_path(program: &OsStr) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|value| find_on_search_path(program, &value))
}

fn find_on_search_path(program: &OsStr, search_path: &OsStr) -> Option<PathBuf> {
    env::split_paths(search_path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(program))
        .find_map(|candidate| candidate.is_file().then(|| anchor_executable(&candidate)))
}

fn anchor_executable(path: &Path) -> PathBuf {
    let anchored = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    };
    let Some(file_name) = anchored.file_name() else {
        return anchored;
    };
    anchored
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .map_or(anchored.clone(), |parent| parent.join(file_name))
}

fn resolve_clang() -> Option<PathBuf> {
    find_on_path(OsStr::new(if cfg!(windows) { "clang.exe" } else { "clang" }))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;

    use clap::Parser;
    use reporigor_core::{SourceFile, SymbolVisibility};

    use super::*;

    fn string_tokens(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn assert_format_validation(arguments: &str, accepted: bool) {
        let cli = Cli::try_parse_from(arguments.split('|'))
            .unwrap_or_else(|error| panic!("fixture CLI must parse: {error}"));
        assert_eq!(validate_format(&cli.command, cli.format).is_ok(), accepted);
    }

    fn mutation_source_error(root: &Path, snapshot: &AnalysisSnapshot, limit: usize) -> String {
        match mutation_sources(root, snapshot, limit) {
            Ok(_) => panic!("mutation projection unexpectedly succeeded"),
            Err(error) => format!("{error:#}"),
        }
    }

    fn assert_mutation_source_failure(
        root: &Path,
        snapshot: &AnalysisSnapshot,
        limit: usize,
        expected: &str,
    ) {
        assert!(mutation_source_error(root, snapshot, limit).contains(expected));
    }

    fn joined_search_path<const N: usize>(entries: [PathBuf; N], context: &str) -> OsString {
        env::join_paths(entries).unwrap_or_else(|error| panic!("{context}: {error}"))
    }

    #[test]
    fn clang_structural_merge_matches_same_line_overloads_by_signature() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        let relative = "same-line.cpp";
        fs::write(
            root.path().join(relative),
            "struct Widget { int run(int value) { return value; } int run(double value) { return value > 0.0; } };\n",
        )
        .unwrap_or_else(|error| panic!("source fixture: {error}"));
        let source = SourceFile {
            path: PathBuf::from(relative),
            relative: relative.to_string(),
            language: Language::Cpp,
            generated: false,
            test: false,
        };
        let generic = TreeSitterBackend::new()
            .analyze_file(
                root.path(),
                &source,
                &AnalysisRequest::new(root.path().to_path_buf()),
            )
            .unwrap_or_else(|error| panic!("Tree-sitter fixture: {error}"));
        assert_eq!(generic.functions.len(), 2);
        assert!(generic
            .functions
            .iter()
            .all(|function| function.start_line == 1 && function.end_line == 1));

        let mut native = generic
            .functions
            .iter()
            .map(|structural| {
                let double = structural.stable_symbol.contains("double");
                let mut function = structural.clone();
                function.stable_symbol = format!(
                    "{}[type:int ({})]",
                    function.name,
                    if double { "double" } else { "int" }
                );
                function.complexity = if double { 17 } else { 11 };
                function.nesting_depth = 0;
                function.statement_count = 0;
                function.parameter_count = 0;
                function.normalized_tokens.clear();
                function.references.clear();
                function.coverage_excluded_ranges.clear();
                function.visibility = SymbolVisibility::Unknown;
                function.structural_metrics_reliable = false;
                function.production = false;
                function.entry_point = false;
                function
            })
            .collect::<Vec<_>>();
        assert!(native
            .iter()
            .all(|function| !function.structural_metrics_reliable));
        native.reverse();
        let mut snapshot = AnalysisSnapshot {
            functions: native,
            ..AnalysisSnapshot::default()
        };

        merge_clang_structural_facts(&mut snapshot, &generic);

        for function in &snapshot.functions {
            let double = function.stable_symbol.contains("(double)]");
            let expected = generic
                .functions
                .iter()
                .find(|structural| structural.stable_symbol.contains("double") == double)
                .unwrap_or_else(|| panic!("matching structural overload"));
            assert_eq!(function.normalized_tokens, expected.normalized_tokens);
            assert_eq!(function.statement_count, expected.statement_count);
            assert_eq!(function.parameter_count, expected.parameter_count);
            assert_eq!(
                function.structural_metrics_reliable,
                expected.structural_metrics_reliable
            );
            assert_eq!(function.complexity, if double { 17 } else { 11 });
            assert_eq!(
                function.stable_symbol,
                format!(
                    "{}[type:int ({})]",
                    function.name,
                    if double { "double" } else { "int" }
                )
            );
        }
    }

    #[test]
    fn clang_structural_merge_leaves_signature_ties_unenriched() {
        let mut structural = FunctionRecord::new(Language::Cpp, "Widget::run", "same-line.cpp", 1, 1, 3);
        structural.stable_symbol = "Widget::run(int LOCAL)".to_string();
        structural.nesting_depth = 2;
        structural.statement_count = 4;
        structural.parameter_count = 1;
        structural.normalized_tokens = vec!["return".to_string(), "LOCAL".to_string(), ";".to_string()];
        structural.visibility = SymbolVisibility::Public;
        structural.structural_metrics_reliable = true;
        let native = |stable_symbol: &str, complexity| FunctionRecord {
            stable_symbol: stable_symbol.to_string(),
            complexity,
            normalized_tokens: Vec::new(),
            statement_count: 0,
            structural_metrics_reliable: false,
            ..structural.clone()
        };
        let mut snapshot = AnalysisSnapshot {
            functions: vec![
                native("Widget::run[type:int (int)]", 7),
                native("Widget::run[type:int (int) const]", 9),
            ],
            ..AnalysisSnapshot::default()
        };
        let generic = FileAnalysis {
            source: SourceFile {
                path: PathBuf::from("same-line.cpp"),
                relative: "same-line.cpp".to_string(),
                language: Language::Cpp,
                generated: false,
                test: false,
            },
            backend: TreeSitterBackend::new().info(),
            functions: vec![structural],
            tokens: Vec::new(),
            mutations: Vec::new(),
            diagnostics: Vec::new(),
            parse_errors: 0,
        };

        merge_clang_structural_facts(&mut snapshot, &generic);

        assert!(snapshot
            .functions
            .iter()
            .all(|function| function.normalized_tokens.is_empty()));
        assert_eq!(snapshot.functions[0].complexity, 7);
        assert_eq!(snapshot.functions[1].complexity, 9);
    }

    #[test]
    fn mutation_exit_preserves_infrastructure_precedence() {
        let report = MutationReport {
            summary: reporigor_reporting::MutationSummary {
                survived: 1,
                timeout: 1,
                ..reporigor_reporting::MutationSummary::default()
            },
            run: None,
            mutants: Vec::new(),
        };
        assert_eq!(mutation_exit(&report, false, false), EXIT_OPERATIONAL);
    }

    #[test]
    fn parameter_signature_helpers_cover_nested_defaults_and_malformed_groups() {
        let canonical = string_tokens(&"int|,|Vec|<|>|,|bool".split('|').collect::<Vec<_>>());
        assert_eq!(
            canonical_parameter_signature("int LOCAL, Vec<LOCAL> LOCAL = make(1, 2), bool LOCAL"),
            Some(canonical)
        );
        let simple = Some(string_tokens(&["int", ",", "bool"]));
        assert_eq!(
            generic_parameter_signature("Widget::run(int LOCAL, bool LOCAL)#cfg"),
            simple
        );
        assert_eq!(
            clang_parameter_signature("Widget::run[type:int (int, bool)]"),
            Some(string_tokens(&["int", ",", "bool"]))
        );
        assert_eq!(parenthesis_groups("outer(inner())"), Some(vec![(5, 13)]));
        assert_eq!(parenthesis_groups("(unterminated"), None);
        assert_eq!(parenthesis_groups(")invalid("), None);
        assert_eq!(clang_parameter_signature("run[type:int (*)(int)]"), None);
    }

    #[test]
    fn quality_exit_helpers_cover_every_precedence_branch() {
        assert_eq!(crap_exit(true, false, 0, false, 0), EXIT_OPERATIONAL);
        assert_eq!(crap_exit(false, false, 1, false, 0), EXIT_OPERATIONAL);
        assert_eq!(crap_exit(false, false, 0, false, 1), EXIT_QUALITY);
        assert_eq!(crap_exit(false, false, 0, false, 0), EXIT_OK);
        assert!(dry_gate_fails(true, true, false));
        assert!(dry_gate_fails(true, false, true));
        assert!(!dry_gate_fails(false, true, true));
        assert_eq!(check_exit(EXIT_OPERATIONAL, true), EXIT_OPERATIONAL);
        assert_eq!(check_exit(EXIT_OK, false), EXIT_QUALITY);
        assert_eq!(check_exit(EXIT_OK, true), EXIT_OK);

        let mut report = MutationReport::new(Vec::new());
        report.summary.compile_error = 1;
        assert_eq!(mutation_exit(&report, false, false), EXIT_OPERATIONAL);
        assert_eq!(mutation_exit(&report, false, true), EXIT_OK);
        report.summary.compile_error = 0;
        report.summary.survived = 1;
        assert_eq!(mutation_exit(&report, false, false), EXIT_QUALITY);
        assert_eq!(mutation_exit(&report, true, false), EXIT_OK);
    }

    #[test]
    fn recovery_rendering_is_stable_for_text_json_and_invalid_formats() {
        let text = render_recovery(Path::new("/fixture"), RecoveryAction::Restored, FormatArg::Text)
            .unwrap_or_else(|error| panic!("text recovery: {error}"));
        assert_eq!(
            text,
            "mutation recovery: restored the original source and removed the pending journal\n"
        );
        let json = render_recovery(Path::new("/fixture"), RecoveryAction::None, FormatArg::Json)
            .unwrap_or_else(|error| panic!("JSON recovery: {error}"));
        let value: serde_json::Value =
            serde_json::from_str(&json).unwrap_or_else(|error| panic!("recovery JSON: {error}"));
        assert_eq!(value["root"], "/fixture");
        assert_eq!(value["recovery"], "none");
        assert!(validate_recovery_format(FormatArg::Sarif).is_err());
    }

    #[test]
    fn clang_routing_predicates_cover_generic_auto_and_native_selection() {
        let source = SourceFile {
            path: PathBuf::from("source.c"),
            relative: "source.c".to_string(),
            language: Language::C,
            generated: false,
            test: false,
        };
        let mut request = AnalysisRequest::new(PathBuf::from("."));
        request.backend = BackendPreference::Generic;
        assert!(!clang_should_be_routed(
            &request,
            std::slice::from_ref(&source),
            true
        ));
        request.backend = BackendPreference::Auto;
        assert!(clang_should_be_routed(
            &request,
            std::slice::from_ref(&source),
            false
        ));
        request.backend = BackendPreference::Native;
        request.languages = BTreeSet::from([Language::C]);
        assert!(clang_database_should_be_inspected(&request, &[]));
        assert!(clang_should_be_routed(&request, &[], true));
        assert!(!clang_should_be_routed(&request, &[], false));
    }

    #[test]
    fn positive_duration_rejects_invalid_values() {
        assert!(positive_duration(0.0).is_err());
        assert!(positive_duration(f64::NAN).is_err());
        assert!(positive_duration(1.0e300).is_err());
        assert!(positive_duration(f64::from_bits(1)).is_err());
        assert!(positive_duration(0.5).is_ok());
    }

    #[test]
    fn report_formats_reject_meaningless_command_combinations() {
        for (arguments, accepted) in [
            ("reporigor|--format|sarif|mutate|.", false),
            ("reporigor|--format|mutation-json|crap|.", false),
            ("reporigor|--format|sarif|check|.", true),
        ] {
            assert_format_validation(arguments, accepted);
        }
    }

    #[test]
    fn native_provider_gate_ignores_unavailable_optional_provider() {
        let resolution = provider_resolution(false);
        assert!(enforce_project_providers(&resolution).is_ok());
    }

    #[test]
    fn native_provider_gate_rejects_unavailable_required_provider() {
        let resolution = provider_resolution(true);
        let error = match enforce_project_providers(&resolution) {
            Ok(()) => panic!("required unavailable provider must fail native mode"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("fixture-provider"));
    }

    #[test]
    fn executable_search_ignores_untrusted_path_entries_and_canonicalizes_matches() {
        let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        let executable = directory.path().join("audit-cargo");
        fs::write(&executable, "fixture").unwrap_or_else(|error| panic!("executable: {error}"));
        let expected = executable
            .canonicalize()
            .unwrap_or_else(|error| panic!("canonical executable: {error}"));

        let search_path = joined_search_path(
            [
                PathBuf::new(),
                PathBuf::from("relative-bin"),
                directory.path().to_path_buf(),
            ],
            "search path",
        );
        assert_eq!(
            find_on_search_path(OsStr::new("audit-cargo"), &search_path),
            Some(expected)
        );

        let untrusted = joined_search_path(
            [PathBuf::new(), PathBuf::from("relative-bin")],
            "untrusted search path",
        );
        assert_eq!(find_on_search_path(OsStr::new("audit-cargo"), &untrusted), None);
    }

    #[cfg(unix)]
    #[test]
    fn executable_anchoring_preserves_rustup_proxy_leaf_name() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        let rustup = directory.path().join("rustup");
        fs::write(&rustup, "fixture").unwrap_or_else(|error| panic!("rustup fixture: {error}"));
        let cargo = directory.path().join("cargo");
        symlink(&rustup, &cargo).unwrap_or_else(|error| panic!("cargo symlink: {error}"));

        let anchored = anchor_executable(&cargo);
        assert_eq!(anchored.file_name(), Some(OsStr::new("cargo")));
        assert!(anchored.is_file());
    }

    #[test]
    fn mutation_projection_source_read_is_bounded_and_contained() {
        let workspace = tempfile::tempdir().unwrap_or_else(|error| panic!("workspace: {error}"));
        let root = workspace.path().join("project");
        fs::create_dir(&root).unwrap_or_else(|error| panic!("project: {error}"));
        assert_python_mutation_failure(&root, MutationSourceFailure::Sparse);
        assert_python_mutation_failure(&root, MutationSourceFailure::Outside);
    }

    #[test]
    fn mutation_projection_rejects_stale_candidate_spans() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("project: {error}"));
        fs::write(root.path().join("source.py"), "false\n").unwrap_or_else(|error| panic!("source: {error}"));
        let stale = mutation_snapshot("source.py", "other", 0, 5);
        assert_mutation_source_failure(root.path(), &stale, 16, "no longer matches its original text");

        let current = mutation_snapshot("source.py", "false", 0, 5);
        let sources = mutation_sources(root.path(), &current, 16)
            .unwrap_or_else(|error| panic!("current mutation source: {error}"));
        assert_eq!(sources.get("source.py").map(String::as_str), Some("false\n"));
    }

    fn mutation_snapshot(file: &str, original: &str, start_byte: usize, end_byte: usize) -> AnalysisSnapshot {
        AnalysisSnapshot {
            mutations: vec![MutationCandidate::new(
                Language::Python,
                file,
                (1, 1),
                original,
                "true",
                start_byte..end_byte,
            )],
            ..AnalysisSnapshot::default()
        }
    }

    #[derive(Clone, Copy)]
    enum MutationSourceFailure {
        Sparse,
        Outside,
    }

    fn assert_python_mutation_failure(root: &Path, failure: MutationSourceFailure) {
        let (file, original, byte_range, message) = match failure {
            MutationSourceFailure::Sparse => {
                let sparse = root.join("sparse.py");
                fs::File::create(&sparse)
                    .and_then(|file| file.set_len(1024 * 1024 * 1024))
                    .unwrap_or_else(|error| panic!("sparse source: {error}"));
                ("sparse.py", "", 0..0, "max_source_bytes (16 bytes)")
            }
            MutationSourceFailure::Outside => {
                let outside = root.parent().unwrap_or(root).join("outside.py");
                fs::write(&outside, "false\n").unwrap_or_else(|error| panic!("outside source: {error}"));
                ("../outside.py", "false", 0..5, "escapes project root")
            }
        };
        let snapshot = mutation_snapshot(file, original, byte_range.start, byte_range.end);
        assert_mutation_source_failure(root, &snapshot, 16, message);
    }

    fn provider_resolution(required_for_native: bool) -> ProviderResolution {
        ProviderResolution {
            context: ProjectContext {
                root: PathBuf::from("."),
                kinds: BTreeSet::from([ProjectKind::Bash]),
                sources: Vec::new(),
                backends: Vec::new(),
                diagnostics: Vec::new(),
            },
            inventory: vec![adapter_project::ProviderStatus {
                id: "fixture-provider".to_string(),
                project: ProjectKind::Bash,
                capabilities: reporigor_core::BackendCapabilities::default(),
                applicable: true,
                available: false,
                required_for_native,
                executable: None,
                version: None,
                fallback: Some("tree-sitter".to_string()),
                reason: Some("fixture is unavailable".to_string()),
                hint: None,
            }],
            provenance: Vec::new(),
        }
    }
}
