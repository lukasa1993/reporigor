use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Output;

use analysis_mutate::MutationMode;
use reporigor_core::{BaselineDisposition, RuleOutcome, RuleResult};
use reporigor_reporting::{ReportCommand, ReportEnvelope};

pub mod support;
use support::canonical_path::canonical;
use support::environment::command_available;
use support::exit_assertion::assert_exit;
use support::fixtures::{rust_project, write_fixture};
use support::generic_backend_assertion::assert_generic_backend as assert_report_uses_generic;
use support::invocation::run_isolated as run_cli;
use support::json_arguments::json_arguments as command_arguments;
use support::json_arguments_with_globals::json_arguments_with_globals;
use support::message_assertion::assert_message_contains_all;
use support::operational_error::operational_error;
use support::operational_error_assertion::assert_operational_error_contains;
use support::output_parser::parse_output as parse_report;
use support::paths::fixture_path;
use support::GENERIC_LANGUAGES;

macro_rules! require_clang {
    ($context:literal) => {
        if !command_available("clang") {
            eprintln!(concat!("skipping ", $context, " because clang is unavailable"));
            return;
        }
    };
}

#[test]
fn every_generic_language_reports_incomplete_integrated_evidence() {
    for language in GENERIC_LANGUAGES.split('|') {
        assert_generic_language_reports(language);
    }
}

fn assert_generic_language_reports(language: &str) {
    let root = fixture_path(&format!("fixtures/projects/generic/{language}"));

    let crap = run_generic_report(language, "crap", &["--allow-missing-coverage"], &root, 0);
    assert_report_context(&crap, ReportCommand::Crap, &root, 2);
    assert_generic_backend(&crap);
    let Some(crap_section) = &crap.results.crap else {
        panic!("{language}: crap report omitted its result section");
    };
    assert!(
        crap_section.summary.functions >= 2,
        "{language}: expected functions in CRAP report, got {crap_section:?}"
    );
    assert!(
        crap_section
            .functions
            .iter()
            .all(|function| function.language.to_string() == language),
        "{language}: CRAP included a function from another language"
    );

    let dry = run_generic_report(language, "dry", &["--min-tokens", "8"], &root, 0);
    assert_report_context(&dry, ReportCommand::Dry, &root, 2);
    assert_generic_backend(&dry);
    let Some(dry_section) = &dry.results.dry else {
        panic!("{language}: dry report omitted its result section");
    };
    assert_eq!(dry_section.summary.min_tokens, 8);
    assert!(
        dry_section.summary.groups > 0,
        "{language}: representative clone pair was not detected"
    );
    assert!(
        dry_section
            .duplicates
            .iter()
            .all(|duplicate| duplicate.locations.len() == 2),
        "{language}: a clone group did not contain a pair of locations"
    );

    let mutate = run_generic_report(language, "mutate", &["--list"], &root, 0);
    assert_report_context(&mutate, ReportCommand::Mutate, &root, 2);
    assert_generic_backend(&mutate);
    let Some(mutation_section) = &mutate.results.mutate else {
        panic!("{language}: mutation report omitted its result section");
    };
    assert!(
        mutation_section.summary.total > 0,
        "{language}: no syntax-aware mutants were inventoried"
    );
    assert_eq!(
        mutation_section.summary.pending, mutation_section.summary.total,
        "{language}: --list executed or discarded a mutation"
    );
    assert!(matches!(
        mutation_section.run.as_ref(),
        Some(run) if run.mode == MutationMode::List
    ));

    let check = run_generic_report(language, "check", &["--min-tokens", "1000"], &root, 2);
    assert_report_context(&check, ReportCommand::Check, &root, 2);
    assert_generic_backend(&check);
    assert!(check.results.crap.is_some(), "{language}: check omitted CRAP");
    assert!(check.results.dry.is_some(), "{language}: check omitted DRY");
    let Some(check_mutation) = check.results.mutate.as_ref() else {
        panic!("{language}: check omitted mutation inventory");
    };
    assert!(matches!(
        check_mutation.run.as_ref(),
        Some(run) if run.mode == MutationMode::List
    ));
    assert!(check.summary.functions >= 2, "{language}: check lost functions");
    assert!(check.summary.mutants > 0, "{language}: check lost mutants");
    assert_eq!(check.summary.duplicate_groups, 0);
    assert_incomplete_integrated_check(&check, language);
}

#[test]
fn quality_and_operational_exit_codes_are_stable() {
    let root = fixture_path("fixtures/projects/generic/python");
    let report = run_report(
        "generic",
        "python",
        "dry",
        &["--min-tokens", "8", "--fail"],
        &root,
        2,
    );
    assert!(report.summary.duplicate_groups > 0);

    let output = run_cli(&command_arguments(
        "generic",
        "python",
        "mutate",
        &["--run"],
        &root,
    ));
    assert_operational_error_contains(
        &output,
        "mutation execution without a test command",
        "--run requires --test-command",
    );
}

#[test]
fn native_report_baseline_is_read_only_and_cannot_hide_omissions() {
    let project = tempfile::tempdir().unwrap_or_else(|error| panic!("baseline tempdir: {error}"));
    let root = project.path();
    let source = root.join("source.py");
    let config = root.join("reporigor.toml");
    let baseline = root.join("reporigor-baseline.json");
    write_fixture(
        &source,
        "def classify(value: int) -> int:\n    if value > 0:\n        return 1\n    return 0\n",
    );
    write_fixture(
        &config,
        "[kiss]\nmaximum_cyclomatic_complexity = 1\n\n[baseline]\nenabled = false\n",
    );

    let arguments = command_arguments("generic", "python", "check", &["--min-tokens", "1000"], root);
    let seed_output = run_cli(&arguments);
    assert_exit(&seed_output, 2, "baseline seed report");
    let seed_report = parse_report(&seed_output, "baseline seed report");
    let seed_rule = complexity_rule(&seed_report);
    assert_eq!(seed_rule.result, RuleOutcome::Fail);
    assert_eq!(seed_rule.baseline, BaselineDisposition::Disabled);
    let violation_id = seed_rule.violation_id.clone();

    if let Err(error) = fs::write(&baseline, &seed_output.stdout) {
        panic!("write native baseline report {}: {error}", baseline.display());
    }
    let baseline_bytes = fs::read(&baseline)
        .unwrap_or_else(|error| panic!("read native baseline report {}: {error}", baseline.display()));
    assert_eq!(baseline_bytes, seed_output.stdout);

    write_baseline_config(&config, false);
    let unchanged_output = run_cli(&arguments);
    assert_exit(&unchanged_output, 2, "unchanged native baseline debt");
    let unchanged_report = parse_report(&unchanged_output, "unchanged native baseline debt");
    let unchanged_rule = complexity_rule(&unchanged_report);
    assert_eq!(unchanged_rule.violation_id, violation_id);
    assert_eq!(unchanged_rule.baseline, BaselineDisposition::Existing);
    assert_failing_baseline(&unchanged_report);
    assert_incomplete_integrated_check(&unchanged_report, "unchanged native baseline debt");
    assert_file_unchanged(&baseline, &baseline_bytes, "unchanged comparison");

    write_baseline_config(&config, true);
    let mismatched_scope = run_cli(&arguments);
    assert_exit(&mismatched_scope, 1, "mismatched baseline analysis scope");
    assert!(
        String::from_utf8_lossy(&mismatched_scope.stderr).contains("different analysis scope"),
        "unexpected scope error: {}",
        String::from_utf8_lossy(&mismatched_scope.stderr)
    );
    assert_file_unchanged(&baseline, &baseline_bytes, "scope mismatch");
    write_baseline_config(&config, false);

    write_fixture(
        &source,
        "def classify(value: int) -> int:\n    if value > 0:\n        if value > 10:\n            return 2\n        return 1\n    return 0\n",
    );
    let worsened_output = run_cli(&arguments);
    assert_exit(&worsened_output, 2, "worsened native baseline debt");
    let worsened_report = parse_report(&worsened_output, "worsened native baseline debt");
    let worsened_rule = complexity_rule(&worsened_report);
    assert_eq!(worsened_rule.violation_id, violation_id);
    assert_eq!(worsened_rule.baseline, BaselineDisposition::Worsened);
    assert!(worsened_rule.excess > unchanged_rule.excess);
    assert_failing_baseline(&worsened_report);
    assert_file_unchanged(&baseline, &baseline_bytes, "worsened comparison");
}

#[test]
fn oversized_sources_fail_closed_in_generic_and_auto_native_routing() {
    let unsafe_config = tempfile::tempdir().unwrap_or_else(|error| panic!("unsafe-config tempdir: {error}"));
    write_fixture(&unsafe_config.path().join("source.py"), "value = True\n");
    write_fixture(
        &unsafe_config.path().join("reporigor.toml"),
        "max_source_bytes = 67108865\n",
    );
    let output = run_cli(&command_arguments(
        "generic",
        "python",
        "dry",
        &["--min-tokens", "1000"],
        unsafe_config.path(),
    ));
    assert_exit(&output, 1, "unsafe configured source limit");
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("immutable 67108864-byte safety limit"),
        "unexpected source-limit error: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let generic = tempfile::tempdir().unwrap_or_else(|error| panic!("generic tempdir: {error}"));
    write_fixture(
        &generic.path().join("large.py"),
        "def oversized_source(value):\n    return value + 123456789\n",
    );
    write_fixture(&generic.path().join("reporigor.toml"), "max_source_bytes = 16\n");
    let generic_output = run_cli(&command_arguments(
        "generic",
        "python",
        "dry",
        &["--min-tokens", "1000"],
        generic.path(),
    ));
    assert_source_too_large(&generic_output, "generic oversized source", 16);

    let native = rust_project(
        "oversized-routing",
        "pub fn oversized_source(value: usize) -> usize { value + 123456789 }\n",
    );
    write_fixture(&native.path().join("reporigor.toml"), "max_source_bytes = 16\n");
    let auto_arguments = json_arguments_with_globals(
        "auto",
        "rust",
        true,
        &[],
        ("dry", &["--min-tokens", "1000"]),
        native.path(),
    );
    let auto_output = run_cli(&auto_arguments);
    assert_source_too_large(&auto_output, "auto native oversized source", 16);
}

#[test]
fn native_rust_uses_cargo_project_semantics() {
    let (_root, report) = native_fixture_report("rust-native", "rust", 7);
    assert_native_rust_report(&report);
}

fn assert_native_rust_report(report: &ReportEnvelope) {
    let crap = native_crap_results(report, "rust-native", "native Rust");
    assert!(crap.summary.functions >= 8);
    assert!(
        crap.functions.iter().any(|function| function.name == "classify"),
        "native Rust function extraction lost classify"
    );
    assert!(
        report.summary.mutants > 0,
        "native Rust mutation discovery was empty"
    );
    assert!(
        report.summary.rule_failures > 0,
        "native Rust check lost additive integrated-rule findings"
    );
    let rules = integrated_rules(report, "native Rust");
    assert_native_rust_failures(&rules.results);
    assert_native_rust_contracts(&rules.results);
    assert_conservative_yagni_exclusions(&rules.results);
}

fn assert_native_rust_failures(results: &[RuleResult]) {
    assert!(rule_failed(
        results,
        "yagni.unused-private-function",
        "reporigor-rust-native-fixture::unused::never_called"
    ));
    assert!(rule_failed(
        results,
        "yagni.unused-module",
        "reporigor-rust-native-fixture::unused"
    ));
    assert!(rule_failed(
        results,
        "yagni.unused-feature-flag",
        "reporigor-rust-native-fixture::feature::unused"
    ));
    assert!(results.iter().any(|result| {
        result.rule_id == "yagni.unreachable-code"
            && result.stable_symbol == "reporigor-rust-native-fixture::unreachable_case"
            && result.result == RuleOutcome::Fail
    }));
}

fn rule_failed(results: &[RuleResult], rule_id: &str, symbol: &str) -> bool {
    results.iter().any(|result| {
        result.rule_id == rule_id && result.stable_symbol == symbol && result.result == RuleOutcome::Fail
    })
}

fn assert_native_rust_contracts(results: &[RuleResult]) {
    for (symbol, outcome) in [
        (
            "Covered for reporigor-rust-native-fixture::Contract",
            RuleOutcome::Pass,
        ),
        (
            "Missing for reporigor-rust-native-fixture::Contract",
            RuleOutcome::Fail,
        ),
    ] {
        assert!(results.iter().any(|result| {
            result.rule_id == "solid.subtype-contract-test"
                && result.stable_symbol.contains(symbol)
                && result.result == outcome
        }));
    }
}

fn assert_conservative_yagni_exclusions(results: &[RuleResult]) {
    for excluded in "public_api,target_only,macros,registry,framework_callback,feature::composed,feature::child,feature::dependency-mode"
        .split(',')
        .map(|suffix| format!("reporigor-rust-native-fixture::{suffix}"))
    {
        assert!(
            !results.iter().any(|result| {
                result.rule_id.starts_with("yagni.")
                    && result.stable_symbol == excluded
                    && result.result == RuleOutcome::Fail
            }),
            "conservative YAGNI exclusion was reported: {excluded}"
        );
    }
    assert!(!results.iter().any(|result| {
        result.rule_id == "yagni.unused-production-dependency" && result.result == RuleOutcome::Fail
    }));
}

#[test]
fn native_clang_uses_the_existing_compilation_database() {
    require_clang!("native Clang CLI test");

    let (_root, report) = native_fixture_report("clang-native", "c,cpp,objective-c", 3);
    let crap = native_crap_results(&report, "clang", "native Clang");
    assert_backend(&report, "tree-sitter-generic", false, "generic Clang merge");
    assert_eq!(crap.summary.functions, 3);
    assert!(
        ["c", "cpp", "objective-c"].iter().all(|language| {
            crap.functions
                .iter()
                .any(|function| function.language.to_string() == *language)
        }),
        "native Clang did not extract all C-family languages: {:?}",
        crap.functions
    );
    assert!(
        report.summary.mutants >= 3,
        "Clang/generic mutation merge was empty"
    );
    let rules = integrated_rules(&report, "native Clang");
    assert!(rules
        .results
        .iter()
        .any(|result| result.rule_id == "kiss.function-statements"));
    assert!(!rules.omitted.iter().any(|omission| {
        matches!(
            omission.rule_id.as_str(),
            "kiss.cyclomatic-complexity"
                | "kiss.nesting-depth"
                | "kiss.function-statements"
                | "kiss.parameter-count"
                | "cohesion.module"
        )
    }));
    assert_incomplete_integrated_check(&report, "native Clang check");
}

#[test]
fn auto_clang_falls_back_only_for_failed_translation_units() {
    require_clang!("partial Clang fallback test");

    let temp = clang_function_fixture(
        "good.c=native_good,failed.c=#error force native validation failure\\ngeneric_fallback",
        "good.c,failed.c",
    );
    let root = temp.path();

    let auto_arguments = json_arguments_with_globals(
        "auto",
        "c",
        true,
        &["--allow-parse-errors"],
        ("check", &["--min-tokens", "1000"]),
        root,
    );
    let output = run_cli(&auto_arguments);
    assert_exit(&output, 2, "partial Clang auto fallback");
    let report: ReportEnvelope = parse_report(&output, "partial Clang fallback");
    let crap = crap_results(&report, "partial Clang fallback");
    assert!(
        ["native_good", "generic_fallback"]
            .iter()
            .all(|name| crap.functions.iter().any(|function| function.name == *name)),
        "auto routing lost a native or fallback function: {:?}",
        crap.functions
    );
    assert_eq!(
        crap.functions
            .iter()
            .find(|function| function.name == "generic_fallback")
            .map(|function| function.complexity),
        Some(2),
        "failed translation unit did not retain generic complexity"
    );
    assert_clang_fallback_for(&report, "failed.c");
    assert_incomplete_integrated_check(&report, "partial Clang auto fallback");

    let native_output = run_cli(&command_arguments(
        "native",
        "c",
        "check",
        &["--min-tokens", "1000"],
        root,
    ));
    assert_exit(&native_output, 1, "partial Clang native failure");
    assert!(
        String::from_utf8_lossy(&native_output.stderr)
            .contains("translation units failed native AST analysis"),
        "native routing did not remain fatal: {}",
        String::from_utf8_lossy(&native_output.stderr)
    );
}

#[test]
fn auto_clang_empty_database_keeps_generic_sources_and_native_rejects_zero() {
    require_clang!("empty Clang database routing test");

    let (temp, report) = run_auto_clang_fixture(
        "generic_only.c=generic_only",
        "",
        &[],
        "empty Clang database auto fallback",
    );
    let root = temp.path();
    assert_report_has_function(&report, "generic_only");
    assert_clang_fallback_for(&report, "generic_only.c");
    assert_native_clang_rejects_zero(root, &[], "empty Clang database native failure");
}

#[test]
fn auto_clang_nonmatching_database_keeps_filtered_generic_source() {
    require_clang!("nonmatching Clang database routing test");

    let (temp, report) = run_auto_clang_fixture(
        "selected.c=selected_metric,database_only.c=database_only",
        "database_only.c",
        &["selected.c"],
        "nonmatching Clang database auto fallback",
    );
    let root = temp.path();

    let filters = ["selected.c"];
    assert_report_has_function(&report, "selected_metric");
    assert_eq!(report.summary.files, 1);
    assert_clang_fallback_for(&report, "selected.c");
    assert_native_clang_rejects_zero(root, &filters, "nonmatching Clang database native failure");
}

#[test]
fn auto_clang_partial_database_replaces_only_represented_sources() {
    require_clang!("partial Clang database routing test");

    let (_temp, report) = run_auto_clang_fixture(
        "compiled.c=compiled_metric,orphan.c=orphan_metric",
        "compiled.c",
        &[],
        "partial Clang database auto fallback",
    );
    assert_report_has_function(&report, "compiled_metric");
    assert_report_has_function(&report, "orphan_metric");
    assert_eq!(report.summary.functions, 2);
    assert_eq!(report.summary.files, 2);
    assert_clang_fallback_for(&report, "orphan.c");
    assert!(
        !has_clang_fallback(&report, "compiled.c"),
        "successfully analyzed source was incorrectly reported as a fallback: {:?}",
        report.diagnostics
    );
}

fn clang_project_fixture(sources: &str, compiled_sources: &str) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap_or_else(|error| panic!("Clang fixture: {error}"));
    for entry in sources.split('\u{1e}') {
        let (file, source) = entry
            .split_once('\u{1f}')
            .unwrap_or_else(|| panic!("invalid encoded Clang fixture entry"));
        write_fixture(&project.path().join(file), source);
    }
    let database = compiled_sources
        .split(',')
        .filter(|file| !file.is_empty())
        .map(|file| {
            serde_json::json!({
                "directory": project.path(),
                "file": file,
                "arguments": ["clang", "-c", file]
            })
        })
        .collect::<Vec<_>>();
    write_fixture(
        &project.path().join("compile_commands.json"),
        &serde_json::Value::Array(database).to_string(),
    );
    project
}

fn clang_function_fixture(functions: &str, compiled_sources: &str) -> tempfile::TempDir {
    let sources = functions
        .split(',')
        .map(|entry| {
            let (file, definition) = entry
                .split_once('=')
                .unwrap_or_else(|| panic!("invalid encoded Clang function fixture"));
            let definition = definition.replace("\\n", "\n");
            let (prefix, name) = definition
                .rsplit_once('\n')
                .map_or(("", definition.as_str()), |parts| parts);
            format!(
                "{file}\u{1f}{prefix}\nint {name}(int value) {{\n  if (value > 0) {{ return 1; }}\n  return 0;\n}}\n"
            )
        })
        .collect::<Vec<_>>()
        .join("\u{1e}");
    clang_project_fixture(&sources, compiled_sources)
}

fn run_report(
    backend: &str,
    language: &str,
    subcommand: &str,
    subcommand_arguments: &[&str],
    root: &Path,
    expected_exit: i32,
) -> ReportEnvelope {
    let output = run_cli(&command_arguments(
        backend,
        language,
        subcommand,
        subcommand_arguments,
        root,
    ));
    let context = format!("{backend} {language} {subcommand}");
    assert_exit(&output, expected_exit, &context);
    parse_report(&output, &context)
}

fn run_generic_report(
    language: &str,
    subcommand: &str,
    arguments: &[&str],
    root: &Path,
    expected_exit: i32,
) -> ReportEnvelope {
    run_report("generic", language, subcommand, arguments, root, expected_exit)
}

fn native_fixture_report(
    fixture: &str,
    language: &str,
    files: usize,
) -> (std::path::PathBuf, ReportEnvelope) {
    let root = fixture_path(&format!("fixtures/projects/{fixture}"));
    let report = run_report("native", language, "check", &["--min-tokens", "1000"], &root, 2);
    assert_report_context(&report, ReportCommand::Check, &root, files);
    (root, report)
}

fn complexity_rule(report: &ReportEnvelope) -> &RuleResult {
    report
        .results
        .rules
        .as_ref()
        .unwrap_or_else(|| panic!("check report omitted integrated rule results"))
        .results
        .iter()
        .find(|result| result.rule_id == "kiss.cyclomatic-complexity")
        .unwrap_or_else(|| panic!("check report omitted KISS cyclomatic-complexity result"))
}

fn integrated_rules<'a>(report: &'a ReportEnvelope, context: &str) -> &'a reporigor_reporting::RuleReport {
    report
        .results
        .rules
        .as_ref()
        .unwrap_or_else(|| panic!("{context} check omitted integrated rules"))
}

fn assert_backend(report: &ReportEnvelope, id: &str, native: bool, context: &str) {
    assert!(
        report
            .backends
            .iter()
            .any(|backend| backend.id == id && backend.native == native),
        "{context} backend was not selected: {:?}",
        report.backends
    );
}

fn assert_failing_baseline(report: &ReportEnvelope) {
    assert_eq!(
        integrated_rules(report, "baseline")
            .baseline
            .as_ref()
            .map(|baseline| (baseline.enabled, baseline.gate_passed)),
        Some((true, false))
    );
}

fn run_auto_clang_report(root: &Path, filters: &[&str], context: &str) -> ReportEnvelope {
    let arguments = clang_check_arguments("auto", root, filters);
    let output = run_cli(&arguments);
    assert_exit(&output, 2, context);
    let report = parse_report(&output, context);
    assert_incomplete_integrated_check(&report, context);
    report
}

fn run_auto_clang_fixture(
    functions: &str,
    compiled_sources: &str,
    filters: &[&str],
    context: &str,
) -> (tempfile::TempDir, ReportEnvelope) {
    let project = clang_function_fixture(functions, compiled_sources);
    let report = run_auto_clang_report(project.path(), filters, context);
    (project, report)
}

fn assert_incomplete_integrated_check(report: &ReportEnvelope, context: &str) {
    let rules = report
        .results
        .rules
        .as_ref()
        .unwrap_or_else(|| panic!("{context}: check omitted integrated rules"));
    assert!(
        !rules.omitted.is_empty(),
        "{context}: incomplete check did not disclose omitted evidence"
    );
    assert_eq!(
        rules.baseline.as_ref().map(|baseline| baseline.gate_passed),
        Some(false),
        "{context}: omitted evidence did not fail the integrated gate"
    );
}

fn assert_native_clang_rejects_zero(root: &Path, filters: &[&str], context: &str) {
    let arguments = clang_check_arguments("native", root, filters);
    let output = run_cli(&arguments);
    let stderr = operational_error(&output, context);
    assert!(
        stderr.contains("no successfully analyzed selected translation units"),
        "{context} returned an unexpected error: {stderr}"
    );
}

fn clang_check_arguments(backend: &str, root: &Path, filters: &[&str]) -> Vec<OsString> {
    let mut global_arguments = Vec::with_capacity(filters.len() * 2);
    for filter in filters {
        global_arguments.extend(["--filter", *filter]);
    }
    json_arguments_with_globals(
        backend,
        "c",
        true,
        &global_arguments,
        ("check", &["--min-tokens", "1000"]),
        root,
    )
}

fn assert_report_has_function(report: &ReportEnvelope, name: &str) {
    let crap = crap_results(report, "Clang routing report");
    assert!(
        crap.functions.iter().any(|function| function.name == name),
        "Clang routing report lost {name}: {:?}",
        crap.functions
    );
}

fn assert_clang_fallback_for(report: &ReportEnvelope, file: &str) {
    assert!(
        has_clang_fallback(report, file),
        "Clang routing report omitted explicit fallback for {file}: {:?}",
        report.diagnostics
    );
}

fn has_clang_fallback(report: &ReportEnvelope, file: &str) -> bool {
    report.diagnostics.iter().any(|diagnostic| {
        diagnostic.backend == "clang-router" && diagnostic.fallback_used && diagnostic.message.contains(file)
    })
}

fn assert_source_too_large(output: &Output, context: &str, max_source_bytes: u64) {
    let stderr = operational_error(output, context);
    let limit = format!("max_source_bytes ({max_source_bytes} bytes)");
    assert_message_contains_all(
        &stderr,
        &format!("{context} returned a non-contract error"),
        &["source", "is at least", &limit],
    );
}

fn assert_file_unchanged(path: &Path, expected: &[u8], context: &str) {
    let actual =
        fs::read(path).unwrap_or_else(|error| panic!("read baseline report {}: {error}", path.display()));
    assert_eq!(actual, expected, "{context} rewrote the native baseline report");
}

fn assert_report_context(report: &ReportEnvelope, command: ReportCommand, root: &Path, files: usize) {
    assert_eq!(report.schema_version, 1);
    assert_eq!(report.command, command);
    assert_eq!(report.root, canonical(root));
    assert_eq!(report.summary.files, files);
    assert_eq!(report.summary.parse_errors, 0);
}

fn assert_generic_backend(report: &ReportEnvelope) {
    assert_report_uses_generic(report, "generic analysis");
}

fn native_crap_results<'a>(
    report: &'a ReportEnvelope,
    backend: &str,
    context: &str,
) -> &'a reporigor_reporting::CrapReport {
    assert_backend(report, backend, true, context);
    crap_results(report, context)
}

fn crap_results<'a>(report: &'a ReportEnvelope, context: &str) -> &'a reporigor_reporting::CrapReport {
    report
        .results
        .crap
        .as_ref()
        .unwrap_or_else(|| panic!("{context} omitted CRAP results"))
}

fn write_baseline_config(path: &Path, include_tests: bool) {
    let scope = if include_tests {
        "include_tests = true\n\n"
    } else {
        ""
    };
    write_fixture(
        path,
        &format!("{scope}[kiss]\nmaximum_cyclomatic_complexity = 1\n\n[baseline]\nenabled = true\n"),
    );
}
