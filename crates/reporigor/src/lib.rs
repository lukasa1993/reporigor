mod args;
mod compat;
mod signals;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fmt::Write as FmtWrite;
use std::io::{self, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::time::Duration;

use adapter_clang::{AstDumpStatus, ClangAdapter};
use adapter_project::{ProjectAdapter, ProviderResolution};
use adapter_rust::{CargoOptions, RustAdapter};
use adapter_tree_sitter::TreeSitterBackend;
use analysis_crap::{analyze as analyze_crap, analyze_path as analyze_crap_path, CrapAnalysis};
use analysis_dry::{find_duplicates_with_budget, DryWorkBudget};
use analysis_mutate::{
    recover_active, CommandSpec, MutationError, MutationExecutionSession, MutationExecutor, MutationMode,
    MutationOptions, MutationReadSession, RecoveryAction,
};
use anyhow::{anyhow, bail, Context, Result};
use provider_mutation::{ImportFormat, ProviderInventory};
use reporigor_core::{
    checked_duration_from_secs_f64, read_bounded_utf8_file_within, resolve_optional_regular_file_within,
    AnalysisRequest, AnalysisSnapshot, BackendPreference, CoreError, Diagnostic, Language, MutationCandidate,
    ProjectContext, ProjectKind, RepoRigorConfig, Severity, SourceBudget, SyntaxBackend,
};
use reporigor_reporting::{
    escape_terminal_text, CrapReport, DryReport, MutationReport, MutationThresholds, ReportContext,
    ReportEnvelope,
};
use serde::Serialize;

pub use args::{BackendArg, Cli, Command, FormatArg};
pub use compat::entry_from_env as legacy_entry_from_env;
pub use signals::install_signal_handlers;

use signals::{cooperative_cancellation_scope, process_cancellation_token};

const EXIT_OK: u8 = 0;
const EXIT_OPERATIONAL: u8 = 1;
const EXIT_QUALITY: u8 = 2;

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
    let format = cli.format;
    validate_format(&cli.command, format)?;
    let root = command_root(&cli.command);
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve project root {}", root.display()))?;
    if matches!(&cli.command, Command::Mutate(arguments) if arguments.recover) {
        if inherited_read_session.is_some() {
            bail!("mutation recovery cannot run inside a read-only analysis session");
        }
        return run_recovery(&root, format);
    }
    let read_session = if command_requires_read_session(&cli.command) {
        let session = match inherited_read_session {
            Some(session) => {
                if session.root() != root {
                    bail!(
                        "read-only mutation session for {} cannot analyze {}",
                        session.root().display(),
                        root.display()
                    );
                }
                session
            }
            None => MutationReadSession::begin(&root)?,
        };
        refuse_pending_mutation(&session)?;
        Some(session)
    } else {
        if inherited_read_session.is_some() {
            bail!("an executing mutation command cannot reuse a read-only analysis session");
        }
        None
    };
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
    let result = match cli.command {
        Command::Crap(arguments) => run_crap(
            &request,
            &config,
            cargo_options,
            &arguments,
            format,
            allow_project_exec,
        ),
        Command::Dry(arguments) => run_dry(
            &request,
            &config,
            cargo_options,
            &arguments,
            format,
            allow_project_exec,
        ),
        Command::Mutate(arguments) => run_mutate(
            &request,
            &config,
            cargo_options,
            arguments,
            format,
            allow_project_exec,
        ),
        Command::Check(arguments) => run_check(
            &request,
            &config,
            cargo_options,
            arguments,
            format,
            allow_project_exec,
        ),
        Command::Providers(arguments) => run_providers(&request, arguments.preflight, format),
    };
    drop(read_session);
    result
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

#[derive(Serialize)]
struct RecoveryOutput<'a> {
    root: &'a Path,
    recovery: RecoveryAction,
}

fn run_recovery(root: &Path, format: FormatArg) -> Result<u8> {
    if !matches!(format, FormatArg::Text | FormatArg::Json) {
        bail!("mutation recovery supports only --format text or --format json");
    }
    let recovery = recover_active(root)?;
    if format == FormatArg::Json {
        let mut rendered = serde_json::to_string_pretty(&RecoveryOutput { root, recovery })?;
        rendered.push('\n');
        write_stdout(&rendered)?;
    } else {
        let detail = match recovery {
            RecoveryAction::None => "no pending mutation journal",
            RecoveryAction::AlreadyClean => "source was already clean; removed the pending journal",
            RecoveryAction::Restored => "restored the original source and removed the pending journal",
        };
        write_stdout(&format!("mutation recovery: {detail}\n"))?;
    }
    Ok(EXIT_OK)
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
        Command::Crap(arguments) => &arguments.common.path,
        Command::Dry(arguments) => &arguments.common.path,
        Command::Mutate(arguments) => &arguments.common.path,
        Command::Check(arguments) => &arguments.common.path,
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
    let snapshot = analyze_project(request, cargo, allow_project_exec)?;
    let allow_empty = arguments.allow_empty || config.crap.allow_empty;
    if snapshot.files.is_empty() && !allow_empty {
        return no_sources_selected(request);
    }
    let threshold = arguments.fail_over.unwrap_or(config.crap.fail_over);
    let analysis = crap_analysis(request, &snapshot, arguments.coverage.as_deref())?;
    let missing = analysis.missing_coverage();
    let over = analysis.over_threshold(threshold);
    let empty = analysis.functions.is_empty();
    let report = ReportEnvelope::crap(
        ReportContext::from_snapshot(&request.root, &snapshot),
        CrapReport::from_analysis(analysis, threshold),
    );
    emit_report(&report, format, &snapshot, request.max_source_bytes)?;

    if empty && !allow_empty {
        return Ok(EXIT_OPERATIONAL);
    }
    if missing > 0 && !(arguments.allow_missing_coverage || config.crap.allow_missing_coverage) {
        return Ok(EXIT_OPERATIONAL);
    }
    Ok(if over > 0 { EXIT_QUALITY } else { EXIT_OK })
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
    let max_groups = arguments.max_groups.unwrap_or(config.dry.max_groups);
    let max_occurrences = arguments
        .max_occurrences_per_window
        .unwrap_or(config.dry.max_occurrences_per_window);
    let duplicates = find_duplicates_with_budget(
        &snapshot.tokens,
        min_tokens,
        max_groups,
        max_occurrences,
        DryWorkBudget::from(&config.dry),
    )?;
    let findings = !duplicates.is_empty();
    let report = ReportEnvelope::dry(
        ReportContext::from_snapshot(&request.root, &snapshot),
        DryReport::new(duplicates, min_tokens),
    );
    emit_report(&report, format, &snapshot, request.max_source_bytes)?;
    Ok(if findings && (arguments.fail || config.dry.fail) {
        EXIT_QUALITY
    } else {
        EXIT_OK
    })
}

fn run_mutate(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    cargo: CargoOptions,
    arguments: args::MutateArgs,
    format: FormatArg,
    allow_project_exec: bool,
) -> Result<u8> {
    let execute = arguments.run;
    let session = if execute {
        Some(MutationExecutionSession::begin(&request.root)?)
    } else {
        None
    };
    let cancellation = process_cancellation_token();
    let timeout = match arguments.timeout {
        Some(timeout) => timeout,
        None => positive_duration(config.mutation.timeout_seconds)?,
    };
    let test_command = arguments
        .test_command
        .or_else(|| config.mutation.test_command.clone());
    if execute
        && test_command
            .as_ref()
            .is_none_or(|command| command.trim().is_empty())
    {
        bail!("--run requires --test-command or mutation.test_command in reporigor.toml");
    }
    let mut options = MutationOptions {
        mode: if execute {
            MutationMode::Execute
        } else {
            MutationMode::List
        },
        test_command: test_command.map(CommandSpec::shell),
        validation_command: if arguments.no_validate {
            None
        } else {
            arguments
                .validation_command
                .or_else(|| config.mutation.validation_command.clone())
                .map(CommandSpec::shell)
        },
        timeout,
        run_baseline: !arguments.skip_baseline,
        max_mutants: arguments.max_mutants.or(config.mutation.max_mutants),
        max_source_bytes: request.max_source_bytes,
        cancellation: cancellation.clone(),
        ..MutationOptions::default()
    };
    if !execute {
        options.run_baseline = false;
    }
    let snapshot = analyze_project(request, cargo, allow_project_exec)?;
    ensure_sources_selected(request, &snapshot)?;
    let executor = MutationExecutor::new(&request.root, options)?;
    let run = if let Some(session) = &session {
        let _signal_scope = cooperative_cancellation_scope();
        executor.run_in_session(&snapshot.mutations, session)?
    } else {
        executor.run(&snapshot.mutations)?
    };
    if cancellation.is_cancelled() {
        return Err(MutationError::Cancelled.into());
    }
    let report_section = MutationReport::from_run(run);
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

fn run_check(
    request: &AnalysisRequest,
    config: &RepoRigorConfig,
    cargo: CargoOptions,
    arguments: args::CheckArgs,
    format: FormatArg,
    allow_project_exec: bool,
) -> Result<u8> {
    let mutation_session = if arguments.run_mutations {
        Some(MutationExecutionSession::begin(&request.root)?)
    } else {
        None
    };
    let cancellation = process_cancellation_token();
    let mutation_options = if arguments.run_mutations {
        let test_command = arguments
            .test_command
            .or_else(|| config.mutation.test_command.clone())
            .filter(|command| !command.trim().is_empty())
            .ok_or_else(|| anyhow!("--run-mutations requires --test-command or configured command"))?;
        let mut options = MutationOptions::execute(CommandSpec::shell(test_command));
        options.validation_command = config.mutation.validation_command.clone().map(CommandSpec::shell);
        options.timeout = positive_duration(config.mutation.timeout_seconds)?;
        options.max_mutants = config.mutation.max_mutants;
        options.max_source_bytes = request.max_source_bytes;
        options.cancellation = cancellation.clone();
        options
    } else {
        let mut options = MutationOptions::list();
        options.max_source_bytes = request.max_source_bytes;
        options
    };
    let snapshot = analyze_project(request, cargo, allow_project_exec)?;
    ensure_sources_selected(request, &snapshot)?;
    let threshold = arguments.fail_over.unwrap_or(config.crap.fail_over);
    let crap_analysis = crap_analysis(request, &snapshot, arguments.coverage.as_deref())?;
    let crap_over = crap_analysis.over_threshold(threshold);
    let crap = CrapReport::from_analysis(crap_analysis, threshold);

    let min_tokens = arguments.min_tokens.unwrap_or(config.dry.min_tokens);
    let duplicates = find_duplicates_with_budget(
        &snapshot.tokens,
        min_tokens,
        config.dry.max_groups,
        config.dry.max_occurrences_per_window,
        DryWorkBudget::from(&config.dry),
    )?;
    let has_duplicates = !duplicates.is_empty();
    let dry = DryReport::new(duplicates, min_tokens);

    let mutation_executor = MutationExecutor::new(&request.root, mutation_options)?;
    let mutation_run = if let Some(session) = &mutation_session {
        let _signal_scope = cooperative_cancellation_scope();
        mutation_executor.run_in_session(&snapshot.mutations, session)?
    } else {
        mutation_executor.run(&snapshot.mutations)?
    };
    if cancellation.is_cancelled() {
        return Err(MutationError::Cancelled.into());
    }
    let mutation = MutationReport::from_run(mutation_run);
    let mutation_exit = if arguments.run_mutations {
        mutation_exit(&mutation, false, false)
    } else {
        EXIT_OK
    };

    let report = ReportEnvelope::check(
        ReportContext::from_snapshot(&request.root, &snapshot),
        Some(crap),
        Some(dry),
        Some(mutation),
    );
    emit_report(&report, format, &snapshot, request.max_source_bytes)?;
    if mutation_exit == EXIT_OPERATIONAL {
        return Ok(EXIT_OPERATIONAL);
    }
    Ok(
        if crap_over > 0 || has_duplicates || mutation_exit == EXIT_QUALITY {
            EXIT_QUALITY
        } else {
            EXIT_OK
        },
    )
}

fn run_providers(request: &AnalysisRequest, preflight: bool, format: FormatArg) -> Result<u8> {
    if !matches!(format, FormatArg::Text | FormatArg::Json) {
        bail!("providers supports only --format text or --format json");
    }
    let adapter = ProjectAdapter::default();
    let resolution = if preflight {
        adapter.preflight(request)?
    } else {
        adapter.discover(request)?
    };
    let mutation = if preflight {
        provider_mutation::preflight(&request.root)?
    } else {
        provider_mutation::discover(&request.root)?
    };
    if format == FormatArg::Json {
        let mut rendered = serde_json::to_string_pretty(&ProvidersOutput {
            project: &resolution,
            mutation: &mutation,
        })?;
        rendered.push('\n');
        write_stdout(&rendered)?;
    } else {
        write_stdout(&render_provider_tables(&resolution, &mutation)?)?;
    }
    Ok(EXIT_OK)
}

#[derive(Serialize)]
struct ProvidersOutput<'a> {
    #[serde(flatten)]
    project: &'a ProviderResolution,
    mutation: &'a ProviderInventory,
}

#[allow(clippy::too_many_lines)]
fn analyze_project(
    request: &AnalysisRequest,
    cargo: CargoOptions,
    allow_project_exec: bool,
) -> Result<AnalysisSnapshot> {
    let project_adapter = ProjectAdapter::default();
    let provider_resolution = if request.backend == BackendPreference::Generic || !allow_project_exec {
        project_adapter.discover(request)?
    } else {
        project_adapter.preflight(request)?
    };
    if request.backend == BackendPreference::Native {
        enforce_project_providers(&provider_resolution)?;
    }

    let mut snapshot = AnalysisSnapshot::default();
    merge_context_metadata(&mut snapshot, &provider_resolution.context);
    if request.backend == BackendPreference::Auto && !allow_project_exec {
        snapshot.diagnostics.push(fallback_diagnostic(
            "project-router",
            "project execution is disabled by default; using filesystem-only discovery and generic syntax analysis; pass --allow-project-exec to permit existing project toolchains"
                .to_string(),
        ));
    }
    let mut sources = provider_resolution.context.sources.clone();

    let has_rust = sources.iter().any(|source| source.language == Language::Rust);
    let cargo_project = provider_resolution.context.kinds.contains(&ProjectKind::Cargo);
    let mut rust_native = false;
    let mut rust_generic_files = BTreeSet::new();
    if has_rust && request.backend != BackendPreference::Generic && allow_project_exec {
        if cargo_project {
            match RustAdapter::new(cargo).analyze_project(request) {
                Ok(mut rust) => {
                    let fallback_count = rust
                        .diagnostics
                        .iter()
                        .filter(|diagnostic| diagnostic.backend == "rust-native" && diagnostic.fallback_used)
                        .count();
                    if fallback_count > 0 {
                        if request.backend == BackendPreference::Native {
                            bail!(
                                "native Rust analysis requires generic fallback for {fallback_count} selected source file(s)"
                            );
                        }
                        rust_generic_files.extend(
                            rust.diagnostics
                                .iter()
                                .filter(|diagnostic| {
                                    diagnostic.backend == "rust-native" && diagnostic.fallback_used
                                })
                                .filter_map(|diagnostic| {
                                    diagnostic.location.as_ref().map(|location| location.file.clone())
                                }),
                        );
                        if rust_generic_files.len() < fallback_count {
                            rust_generic_files.extend(
                                sources
                                    .iter()
                                    .filter(|source| source.language == Language::Rust)
                                    .map(|source| source.relative.clone()),
                            );
                        }
                        rust.files
                            .retain(|source| !rust_generic_files.contains(&source.relative));
                        rust.functions
                            .retain(|function| !rust_generic_files.contains(&function.file));
                        rust.tokens
                            .retain(|file, _tokens| !rust_generic_files.contains(file));
                        rust.mutations
                            .retain(|mutation| !rust_generic_files.contains(&mutation.file));
                        rust.parse_errors = rust.parse_errors.saturating_sub(fallback_count);
                    }
                    merge_snapshot(&mut snapshot, rust);
                    rust_native = true;
                }
                Err(error @ CoreError::SourceTooLarge { .. }) => return Err(error.into()),
                Err(error) if request.backend == BackendPreference::Native => return Err(error.into()),
                Err(error) => snapshot.diagnostics.push(fallback_diagnostic(
                    "rust-router",
                    format!("native Rust analysis failed; using generic syntax backend: {error}"),
                )),
            }
        } else if request.backend == BackendPreference::Native {
            return Err(CoreError::BackendUnavailable {
                backend: "rust-native".to_string(),
                message: "Rust sources require Cargo.toml in --backend native mode".to_string(),
            }
            .into());
        } else {
            snapshot.diagnostics.push(fallback_diagnostic(
                "rust-router",
                "Cargo.toml is unavailable; using generic Rust syntax analysis".to_string(),
            ));
        }
    }
    if rust_native {
        sources.retain(|source| {
            source.language != Language::Rust || rust_generic_files.contains(&source.relative)
        });
    }

    let c_family = sources.iter().any(|source| source.language.is_c_family());
    let c_family_requested =
        request.languages.is_empty() || request.languages.iter().any(|language| language.is_c_family());
    let inspect_clang_database =
        c_family || (request.backend == BackendPreference::Native && c_family_requested);
    let clang_database =
        if request.backend != BackendPreference::Generic && allow_project_exec && inspect_clang_database {
            ClangAdapter::discover(&request.root)?
        } else {
            None
        };
    let route_clang = c_family
        || (request.backend == BackendPreference::Native && c_family_requested && clang_database.is_some());
    let mut clang_authoritative_files = BTreeSet::new();
    if route_clang && request.backend != BackendPreference::Generic && allow_project_exec {
        match (clang_database, resolve_clang()) {
            (Some(_), Some(compiler)) => match ClangAdapter::new(compiler)
                .analyze_project_with_provenance(request)
            {
                Ok(mut analysis) => {
                    let failed_translation_units = analysis
                        .translation_units
                        .iter()
                        .filter(|unit| {
                            unit.source.is_some() && !matches!(unit.status, AstDumpStatus::Analyzed)
                        })
                        .collect::<Vec<_>>();
                    clang_authoritative_files.extend(
                        analysis
                            .translation_units
                            .iter()
                            .filter(|unit| matches!(unit.status, AstDumpStatus::Analyzed))
                            .filter_map(|unit| unit.source.as_ref())
                            .map(|source| source.relative.clone()),
                    );
                    if request.backend == BackendPreference::Native {
                        if clang_authoritative_files.is_empty() {
                            bail!(
                                "native Clang analysis produced no successfully analyzed selected translation units"
                            );
                        }
                        if !failed_translation_units.is_empty() {
                            bail!(
                                "one or more selected Clang translation units failed native AST analysis"
                            );
                        }
                        sources.retain(|source| !source.language.is_c_family());
                        sources.extend(analysis.project.context.sources.clone());
                    } else {
                        // Auto mode only replaces function metrics for files
                        // that completed native AST extraction. Files absent
                        // from a partial, stale, empty, or filtered database
                        // remain owned by the generic backend.
                        sources.extend(analysis.project.context.sources.clone());
                        let mut failed_files = BTreeSet::new();
                        for unit in failed_translation_units {
                            let Some(source) = unit.source.as_ref() else {
                                continue;
                            };
                            if clang_authoritative_files.contains(&source.relative) {
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
                        for source in sources.iter().filter(|source| source.language.is_c_family()) {
                            if clang_authoritative_files.contains(&source.relative)
                                || failed_files.contains(&source.relative)
                            {
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
                    // Tree-sitter still supplies tokens and generic mutation
                    // sites, so it owns these files in the merged snapshot.
                    analysis.snapshot.files.clear();
                    merge_snapshot(&mut snapshot, analysis.snapshot);
                }
                Err(error @ CoreError::SourceTooLarge { .. }) => return Err(error.into()),
                Err(error) if request.backend == BackendPreference::Native => return Err(error.into()),
                Err(error) => snapshot.diagnostics.push(fallback_diagnostic(
                    "clang-router",
                    format!("Clang project validation failed; using generic syntax backend: {error}"),
                )),
            },
            (Some(_), None) if request.backend == BackendPreference::Native => {
                return Err(CoreError::BackendUnavailable {
                    backend: "clang".to_string(),
                    message: "Clang is not available through a trusted absolute PATH entry".to_string(),
                }
                .into());
            }
            (Some(_), None) => snapshot.diagnostics.push(fallback_diagnostic(
                "clang-router",
                "Clang is not available through a trusted absolute PATH entry; using generic C-family syntax analysis"
                    .to_string(),
            )),
            (None, _) if request.backend == BackendPreference::Native => {
                return Err(CoreError::BackendUnavailable {
                    backend: "clang".to_string(),
                    message: "C-family native analysis requires an existing compile_commands.json"
                        .to_string(),
                }
                .into());
            }
            (None, _) => snapshot.diagnostics.push(fallback_diagnostic(
                "clang-router",
                "no compile_commands.json found; using generic C-family syntax analysis".to_string(),
            )),
        }
    }

    sources.sort_by(|left, right| left.relative.cmp(&right.relative));
    sources.dedup_by(|left, right| left.relative == right.relative && left.language == right.language);
    let generic = TreeSitterBackend::new();
    for source in sources {
        let clang_native = clang_authoritative_files.contains(&source.relative);
        let mut syntax_request = request.clone();
        if clang_native {
            // Clang is authoritative for syntax and function metrics. Generic
            // error recovery here only feeds token and mutation analysis.
            syntax_request.allow_parse_errors = true;
        }
        let mut file = generic.analyze_file(&request.root, &source, &syntax_request)?;
        if clang_native {
            file.functions.clear();
        }
        snapshot.push(file);
    }
    snapshot.assign_mutation_ids();
    Ok(snapshot)
}

fn crap_analysis(
    request: &AnalysisRequest,
    snapshot: &AnalysisSnapshot,
    coverage: Option<&Path>,
) -> Result<CrapAnalysis> {
    match coverage {
        Some(path) => analyze_crap_path(&request.root, snapshot.functions.clone(), path)
            .with_context(|| format!("failed to load coverage from {}", path.display())),
        None => Ok(analyze_crap(&request.root, snapshot.functions.clone(), None)),
    }
}

fn emit_report(
    report: &ReportEnvelope,
    format: FormatArg,
    snapshot: &AnalysisSnapshot,
    max_source_bytes: usize,
) -> Result<()> {
    let mut rendered = match format {
        FormatArg::Text => report.to_human(),
        FormatArg::Json => report.to_pretty_json()?,
        FormatArg::Sarif => report.to_sarif_json()?,
        FormatArg::MutationJson => {
            let sources = mutation_sources(report.root(), snapshot, max_source_bytes)?;
            report.to_mutation_elements_json(&sources, MutationThresholds::new(60, 80)?)?
        }
    };
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    write_stdout(&rendered)
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
    let mut files = BTreeMap::<String, Vec<&MutationCandidate>>::new();
    for mutation in &snapshot.mutations {
        files.entry(mutation.file.clone()).or_default().push(mutation);
    }
    let mut budget = SourceBudget::new(max_source_bytes)?;
    for relative in files.keys() {
        let path = root.join(relative);
        let canonical = resolve_optional_regular_file_within(root, &path)?
            .ok_or_else(|| anyhow!("mutation source {} does not exist", path.display()))?;
        let metadata = canonical.metadata().map_err(|source| CoreError::Read {
            path: path.display().to_string(),
            source,
        })?;
        budget.observe(&path, metadata.len())?;
    }
    let max_source_bytes = u64::try_from(max_source_bytes).unwrap_or(u64::MAX);
    files
        .into_iter()
        .map(|(relative, mutations)| {
            let path = root.join(&relative);
            let source = read_bounded_utf8_file_within(root, &path, max_source_bytes)
                .with_context(|| format!("failed to read mutation source {}", path.display()))?;
            validate_mutation_source(&relative, &source, &mutations)?;
            Ok((relative, source))
        })
        .collect()
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
    if summary.invalid > 0
        || summary.runtime_error > 0
        || summary.timeout > 0
        || (!allow_compile_errors && summary.compile_error > 0)
    {
        EXIT_OPERATIONAL
    } else if !allow_survivors && summary.survived > 0 {
        EXIT_QUALITY
    } else {
        EXIT_OK
    }
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
        let executable = status
            .executable
            .as_deref()
            .map_or_else(|| "-".to_string(), |path| path.display().to_string());
        writeln!(
            rendered,
            "{}  {}  {}  {}  {}  {}  {}",
            escape_terminal_text(status.id.as_str()),
            if status.required_for_native {
                "required"
            } else {
                "optional"
            },
            status.applicable,
            status.available,
            escape_terminal_text(&executable),
            escape_terminal_text(status.version.as_deref().unwrap_or("-")),
            escape_terminal_text(status.fallback.as_deref().unwrap_or("-"))
        )?;
        if let Some(reason) = &status.reason {
            writeln!(rendered, "  reason: {}", escape_terminal_text(reason))?;
        }
        if let Some(hint) = &status.hint {
            writeln!(rendered, "  hint: {}", escape_terminal_text(hint))?;
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
        let imports = status
            .import_formats
            .iter()
            .map(|format| match format {
                ImportFormat::MutationTestingElementsV1 => "mte-v1",
                ImportFormat::MutationTestingElementsV2 => "mte-v2",
                ImportFormat::CargoMutantsOutcomes => "cargo-mutants-outcomes",
                ImportFormat::MuterJson => "muter-json",
            })
            .collect::<Vec<_>>()
            .join(",");
        let executable = status
            .executable
            .as_deref()
            .map_or_else(|| "-".to_owned(), |path| path.display().to_string());
        writeln!(
            rendered,
            "{}  {}  {}  {}  {}  {}  {}  {}",
            escape_terminal_text(status.id.as_str()),
            status.default,
            status.applicable,
            status.available,
            if status.execution_enabled {
                "execute"
            } else {
                "import-only"
            },
            escape_terminal_text(&executable),
            escape_terminal_text(status.version.as_deref().unwrap_or("-")),
            imports,
        )?;
        if let Some(reason) = &status.reason {
            writeln!(rendered, "  reason: {}", escape_terminal_text(reason))?;
        }
        if let Some(hint) = &status.hint {
            writeln!(rendered, "  hint: {}", escape_terminal_text(hint))?;
        }
    }
    Ok(())
}

fn resolve_cargo() -> Option<PathBuf> {
    env::var_os("CARGO")
        .map(PathBuf::from)
        .map(|path| anchor_executable(&path))
        .filter(|path| path.is_file())
        .or_else(|| find_on_path(OsStr::new(if cfg!(windows) { "cargo.exe" } else { "cargo" })))
        .or_else(|| {
            env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .map(|directory| {
                    directory
                        .join("bin")
                        .join(if cfg!(windows) { "cargo.exe" } else { "cargo" })
                })
                .map(|path| anchor_executable(&path))
                .filter(|path| path.is_file())
        })
        .or_else(|| {
            env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(PathBuf::from)
                .map(|directory| {
                    directory
                        .join(".cargo/bin")
                        .join(if cfg!(windows) { "cargo.exe" } else { "cargo" })
                })
                .map(|path| anchor_executable(&path))
                .filter(|path| path.is_file())
        })
}

fn find_on_path(program: &OsStr) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|value| find_on_search_path(program, &value))
}

fn find_on_search_path(program: &OsStr, search_path: &OsStr) -> Option<PathBuf> {
    env::split_paths(search_path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(program))
        .find_map(|candidate| {
            candidate
                .is_file()
                .then(|| candidate.canonicalize().ok())
                .flatten()
        })
}

fn anchor_executable(path: &Path) -> PathBuf {
    let anchored = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    };
    anchored.canonicalize().unwrap_or(anchored)
}

fn resolve_clang() -> Option<PathBuf> {
    find_on_path(OsStr::new(if cfg!(windows) { "clang.exe" } else { "clang" }))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::Parser;

    use super::*;

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
    fn positive_duration_rejects_invalid_values() {
        assert!(positive_duration(0.0).is_err());
        assert!(positive_duration(f64::NAN).is_err());
        assert!(positive_duration(1.0e300).is_err());
        assert!(positive_duration(f64::from_bits(1)).is_err());
        assert!(positive_duration(0.5).is_ok());
    }

    #[test]
    fn report_formats_reject_meaningless_command_combinations() {
        let Ok(mutate) = Cli::try_parse_from(["reporigor", "--format", "sarif", "mutate", "."]) else {
            panic!("fixture CLI must parse");
        };
        assert!(validate_format(&mutate.command, mutate.format).is_err());

        let Ok(crap) = Cli::try_parse_from(["reporigor", "--format", "mutation-json", "crap", "."]) else {
            panic!("fixture CLI must parse");
        };
        assert!(validate_format(&crap.command, crap.format).is_err());

        let Ok(check) = Cli::try_parse_from(["reporigor", "--format", "sarif", "check", "."]) else {
            panic!("fixture CLI must parse");
        };
        assert!(validate_format(&check.command, check.format).is_ok());
    }

    #[test]
    fn native_provider_gate_ignores_unavailable_optional_provider() {
        let resolution = provider_resolution(false);
        assert!(enforce_project_providers(&resolution).is_ok());
    }

    #[test]
    fn native_provider_gate_rejects_unavailable_required_provider() {
        let resolution = provider_resolution(true);
        let Err(error) = enforce_project_providers(&resolution) else {
            panic!("required unavailable provider must fail native mode");
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

        let search_path = env::join_paths([
            PathBuf::new(),
            PathBuf::from("relative-bin"),
            directory.path().to_path_buf(),
        ])
        .unwrap_or_else(|error| panic!("search path: {error}"));
        assert_eq!(
            find_on_search_path(OsStr::new("audit-cargo"), &search_path),
            Some(expected)
        );

        let untrusted = env::join_paths([PathBuf::new(), PathBuf::from("relative-bin")])
            .unwrap_or_else(|error| panic!("untrusted search path: {error}"));
        assert_eq!(find_on_search_path(OsStr::new("audit-cargo"), &untrusted), None);
    }

    #[test]
    fn mutation_projection_source_read_is_bounded_and_contained() {
        let workspace = tempfile::tempdir().unwrap_or_else(|error| panic!("workspace: {error}"));
        let root = workspace.path().join("project");
        fs::create_dir(&root).unwrap_or_else(|error| panic!("project: {error}"));
        let sparse = root.join("sparse.py");
        fs::File::create(&sparse)
            .and_then(|file| file.set_len(1024 * 1024 * 1024))
            .unwrap_or_else(|error| panic!("sparse source: {error}"));
        let snapshot = mutation_snapshot("sparse.py", "", 0, 0);
        let Err(error) = mutation_sources(&root, &snapshot, 16) else {
            panic!("oversized mutation projection source was unexpectedly read");
        };
        assert!(format!("{error:#}").contains("max_source_bytes (16 bytes)"));

        let outside = workspace.path().join("outside.py");
        fs::write(&outside, "false\n").unwrap_or_else(|error| panic!("outside source: {error}"));
        let snapshot = mutation_snapshot("../outside.py", "false", 0, 5);
        let Err(error) = mutation_sources(&root, &snapshot, 16) else {
            panic!("mutation projection source escaped the project root");
        };
        assert!(format!("{error:#}").contains("escapes project root"));
    }

    #[test]
    fn mutation_projection_rejects_stale_candidate_spans() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("project: {error}"));
        fs::write(root.path().join("source.py"), "false\n").unwrap_or_else(|error| panic!("source: {error}"));
        let stale = mutation_snapshot("source.py", "other", 0, 5);
        let Err(error) = mutation_sources(root.path(), &stale, 16) else {
            panic!("stale mutation candidate was unexpectedly projected");
        };
        assert!(error.to_string().contains("no longer matches its original text"));

        let current = mutation_snapshot("source.py", "false", 0, 5);
        let sources = mutation_sources(root.path(), &current, 16)
            .unwrap_or_else(|error| panic!("current mutation source: {error}"));
        assert_eq!(sources.get("source.py").map(String::as_str), Some("false\n"));
    }

    fn mutation_snapshot(file: &str, original: &str, start_byte: usize, end_byte: usize) -> AnalysisSnapshot {
        AnalysisSnapshot {
            mutations: vec![MutationCandidate {
                id: 1,
                language: Language::Python,
                file: file.to_string(),
                line: 1,
                column: 1,
                original: original.to_string(),
                replacement: "true".to_string(),
                start_byte,
                end_byte,
            }],
            ..AnalysisSnapshot::default()
        }
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
