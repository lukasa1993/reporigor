use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use analysis_mutate::{MutationMode, STATE_DIRECTORY_ENV};
use reporigor_reporting::{ReportCommand, ReportEnvelope};

const GENERIC_LANGUAGES: [(&str, &str); 8] = [
    ("bash", "bash"),
    ("c", "c"),
    ("cpp", "cpp"),
    ("objective-c", "objective-c"),
    ("python", "python"),
    ("rust", "rust"),
    ("swift", "swift"),
    ("typescript", "typescript"),
];

#[test]
fn every_generic_language_runs_the_complete_read_only_flow() {
    for (language, fixture) in GENERIC_LANGUAGES {
        let root = fixture_path(&format!("fixtures/projects/generic/{fixture}"));

        let crap = run_report(
            "generic",
            language,
            "crap",
            &["--allow-missing-coverage"],
            &root,
            0,
        );
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

        let dry = run_report("generic", language, "dry", &["--min-tokens", "8"], &root, 0);
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

        let mutate = run_report("generic", language, "mutate", &["--list"], &root, 0);
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

        let check = run_report("generic", language, "check", &["--min-tokens", "1000"], &root, 0);
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
    }
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
    assert_exit(&output, 1, "mutation execution without a test command");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--run requires --test-command"),
        "unexpected operational error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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

    let native = tempfile::tempdir().unwrap_or_else(|error| panic!("native tempdir: {error}"));
    fs::create_dir_all(native.path().join("src"))
        .unwrap_or_else(|error| panic!("create native source directory: {error}"));
    write_fixture(
        &native.path().join("Cargo.toml"),
        "[package]\nname = \"oversized-routing\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write_fixture(
        &native.path().join("Cargo.lock"),
        "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 3\n\n[[package]]\nname = \"oversized-routing\"\nversion = \"0.1.0\"\n",
    );
    write_fixture(
        &native.path().join("src/lib.rs"),
        "pub fn oversized_source(value: usize) -> usize { value + 123456789 }\n",
    );
    write_fixture(&native.path().join("reporigor.toml"), "max_source_bytes = 16\n");
    let auto_arguments = vec![
        OsString::from("--backend"),
        OsString::from("auto"),
        OsString::from("--allow-project-exec"),
        OsString::from("--language"),
        OsString::from("rust"),
        OsString::from("dry"),
        OsString::from("--min-tokens"),
        OsString::from("1000"),
        native.path().as_os_str().to_owned(),
    ];
    let auto_output = run_cli(&auto_arguments);
    assert_source_too_large(&auto_output, "auto native oversized source", 16);
}

#[test]
fn native_rust_uses_cargo_project_semantics() {
    let root = fixture_path("fixtures/projects/rust-native");
    let report = run_report("native", "rust", "check", &["--min-tokens", "1000"], &root, 0);

    assert_report_context(&report, ReportCommand::Check, &root, 1);
    assert!(
        report
            .backends
            .iter()
            .any(|backend| backend.id == "rust-native" && backend.native),
        "native Rust backend was not selected: {:?}",
        report.backends
    );
    let Some(crap) = &report.results.crap else {
        panic!("native Rust check omitted CRAP results");
    };
    assert_eq!(crap.summary.functions, 2);
    assert!(
        crap.functions.iter().any(|function| function.name == "classify"),
        "native Rust function extraction lost classify"
    );
    assert!(
        report.summary.mutants > 0,
        "native Rust mutation discovery was empty"
    );
}

#[test]
fn native_clang_uses_the_existing_compilation_database() {
    if !command_available("clang") {
        eprintln!("skipping native Clang CLI test because clang is unavailable");
        return;
    }

    let root = fixture_path("fixtures/projects/clang-native");
    let report = run_report(
        "native",
        "c,cpp,objective-c",
        "check",
        &["--min-tokens", "1000"],
        &root,
        0,
    );

    assert_report_context(&report, ReportCommand::Check, &root, 3);
    assert!(
        report
            .backends
            .iter()
            .any(|backend| backend.id == "clang" && backend.native),
        "native Clang backend was not selected: {:?}",
        report.backends
    );
    assert!(
        report
            .backends
            .iter()
            .any(|backend| backend.id == "tree-sitter-generic" && !backend.native),
        "Clang check did not merge generic tokens/mutations: {:?}",
        report.backends
    );
    let Some(crap) = &report.results.crap else {
        panic!("native Clang check omitted CRAP results");
    };
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
}

#[test]
fn auto_clang_falls_back_only_for_failed_translation_units() {
    if !command_available("clang") {
        eprintln!("skipping partial Clang fallback test because clang is unavailable");
        return;
    }

    let Ok(temp) = tempfile::tempdir() else {
        panic!("failed to create partial Clang fallback fixture");
    };
    let root = temp.path();
    write_fixture(
        &root.join("good.c"),
        "int native_good(int value) {\n  if (value > 0) { return 1; }\n  return 0;\n}\n",
    );
    write_fixture(
        &root.join("failed.c"),
        "#error force native validation failure\nint generic_fallback(int value) {\n  if (value > 0) { return 1; }\n  return 0;\n}\n",
    );
    let database = serde_json::json!([
        {
            "directory": root,
            "file": "good.c",
            "arguments": ["clang", "-c", "good.c"]
        },
        {
            "directory": root,
            "file": "failed.c",
            "arguments": ["clang", "-c", "failed.c"]
        }
    ]);
    write_fixture(&root.join("compile_commands.json"), &database.to_string());

    let auto_arguments = vec![
        OsString::from("--backend"),
        OsString::from("auto"),
        OsString::from("--allow-project-exec"),
        OsString::from("--language"),
        OsString::from("c"),
        OsString::from("--allow-parse-errors"),
        OsString::from("--format"),
        OsString::from("json"),
        OsString::from("check"),
        OsString::from("--min-tokens"),
        OsString::from("1000"),
        root.as_os_str().to_owned(),
    ];
    let output = run_cli(&auto_arguments);
    assert_exit(&output, 0, "partial Clang auto fallback");
    let report: ReportEnvelope = match serde_json::from_slice(&output.stdout) {
        Ok(report) => report,
        Err(error) => panic!(
            "partial Clang fallback emitted invalid JSON: {error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    };
    let Some(crap) = report.results.crap.as_ref() else {
        panic!("partial Clang fallback omitted CRAP results");
    };
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
    assert!(
        report.diagnostics.iter().any(|diagnostic| {
            diagnostic.backend == "clang-router"
                && diagnostic.fallback_used
                && diagnostic.message.contains("failed.c")
        }),
        "failed translation unit did not report explicit generic fallback: {:?}",
        report.diagnostics
    );

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
    if !command_available("clang") {
        eprintln!("skipping empty Clang database routing test because clang is unavailable");
        return;
    }

    let Ok(temp) = tempfile::tempdir() else {
        panic!("failed to create empty Clang database fixture");
    };
    let root = temp.path();
    write_fixture(
        &root.join("generic_only.c"),
        "int generic_only(int value) {\n  if (value > 0) { return 1; }\n  return 0;\n}\n",
    );
    write_fixture(&root.join("compile_commands.json"), "[]");

    let report = run_auto_clang_report(root, &[], "empty Clang database auto fallback");
    assert_report_has_function(&report, "generic_only");
    assert_clang_fallback_for(&report, "generic_only.c");
    assert_native_clang_rejects_zero(root, &[], "empty Clang database native failure");
}

#[test]
fn auto_clang_nonmatching_database_keeps_filtered_generic_source() {
    if !command_available("clang") {
        eprintln!("skipping nonmatching Clang database routing test because clang is unavailable");
        return;
    }

    let Ok(temp) = tempfile::tempdir() else {
        panic!("failed to create nonmatching Clang database fixture");
    };
    let root = temp.path();
    write_fixture(
        &root.join("selected.c"),
        "int selected_metric(int value) {\n  if (value > 0) { return 1; }\n  return 0;\n}\n",
    );
    write_fixture(
        &root.join("database_only.c"),
        "int database_only(int value) { return value + 1; }\n",
    );
    let database = serde_json::json!([{
        "directory": root,
        "file": "database_only.c",
        "arguments": ["clang", "-c", "database_only.c"]
    }]);
    write_fixture(&root.join("compile_commands.json"), &database.to_string());

    let filters = ["selected.c"];
    let report = run_auto_clang_report(root, &filters, "nonmatching Clang database auto fallback");
    assert_report_has_function(&report, "selected_metric");
    assert_eq!(report.summary.files, 1);
    assert_clang_fallback_for(&report, "selected.c");
    assert_native_clang_rejects_zero(root, &filters, "nonmatching Clang database native failure");
}

#[test]
fn auto_clang_partial_database_replaces_only_represented_sources() {
    if !command_available("clang") {
        eprintln!("skipping partial Clang database routing test because clang is unavailable");
        return;
    }

    let Ok(temp) = tempfile::tempdir() else {
        panic!("failed to create partial Clang database fixture");
    };
    let root = temp.path();
    write_fixture(
        &root.join("compiled.c"),
        "int compiled_metric(int value) {\n  if (value > 0) { return 1; }\n  return 0;\n}\n",
    );
    write_fixture(
        &root.join("orphan.c"),
        "int orphan_metric(int value) {\n  if (value > 0) { return 1; }\n  return 0;\n}\n",
    );
    let database = serde_json::json!([{
        "directory": root,
        "file": "compiled.c",
        "arguments": ["clang", "-c", "compiled.c"]
    }]);
    write_fixture(&root.join("compile_commands.json"), &database.to_string());

    let report = run_auto_clang_report(root, &[], "partial Clang database auto fallback");
    assert_report_has_function(&report, "compiled_metric");
    assert_report_has_function(&report, "orphan_metric");
    assert_eq!(report.summary.functions, 2);
    assert_eq!(report.summary.files, 2);
    assert_clang_fallback_for(&report, "orphan.c");
    assert!(
        !report.diagnostics.iter().any(|diagnostic| {
            diagnostic.backend == "clang-router"
                && diagnostic.fallback_used
                && diagnostic.message.contains("compiled.c")
        }),
        "successfully analyzed source was incorrectly reported as a fallback: {:?}",
        report.diagnostics
    );
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
    assert_exit(
        &output,
        expected_exit,
        &format!("{backend} {language} {subcommand}"),
    );
    match serde_json::from_slice(&output.stdout) {
        Ok(report) => report,
        Err(error) => panic!(
            "{backend} {language} {subcommand} emitted invalid report JSON: {error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

fn run_auto_clang_report(root: &Path, filters: &[&str], context: &str) -> ReportEnvelope {
    let arguments = clang_check_arguments("auto", root, filters);
    let output = run_cli(&arguments);
    assert_exit(&output, 0, context);
    match serde_json::from_slice(&output.stdout) {
        Ok(report) => report,
        Err(error) => panic!(
            "{context} emitted invalid report JSON: {error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

fn assert_native_clang_rejects_zero(root: &Path, filters: &[&str], context: &str) {
    let arguments = clang_check_arguments("native", root, filters);
    let output = run_cli(&arguments);
    assert_exit(&output, 1, context);
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("no successfully analyzed selected translation units"),
        "{context} returned an unexpected error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn clang_check_arguments(backend: &str, root: &Path, filters: &[&str]) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("--backend"),
        OsString::from(backend),
        OsString::from("--allow-project-exec"),
        OsString::from("--language"),
        OsString::from("c"),
    ];
    for filter in filters {
        arguments.push(OsString::from("--filter"));
        arguments.push(OsString::from(filter));
    }
    arguments.extend([
        OsString::from("--format"),
        OsString::from("json"),
        OsString::from("check"),
        OsString::from("--min-tokens"),
        OsString::from("1000"),
        root.as_os_str().to_owned(),
    ]);
    arguments
}

fn assert_report_has_function(report: &ReportEnvelope, name: &str) {
    let Some(crap) = report.results.crap.as_ref() else {
        panic!("Clang routing report omitted CRAP results");
    };
    assert!(
        crap.functions.iter().any(|function| function.name == name),
        "Clang routing report lost {name}: {:?}",
        crap.functions
    );
}

fn assert_clang_fallback_for(report: &ReportEnvelope, file: &str) {
    assert!(
        report.diagnostics.iter().any(|diagnostic| {
            diagnostic.backend == "clang-router"
                && diagnostic.fallback_used
                && diagnostic.message.contains(file)
        }),
        "Clang routing report omitted explicit fallback for {file}: {:?}",
        report.diagnostics
    );
}

fn command_arguments(
    backend: &str,
    language: &str,
    subcommand: &str,
    subcommand_arguments: &[&str],
    root: &Path,
) -> Vec<OsString> {
    let mut arguments = vec![OsString::from("--backend"), OsString::from(backend)];
    if backend == "native" {
        arguments.push(OsString::from("--allow-project-exec"));
    }
    arguments.extend([
        OsString::from("--language"),
        OsString::from(language),
        OsString::from("--format"),
        OsString::from("json"),
        OsString::from(subcommand),
    ]);
    arguments.extend(subcommand_arguments.iter().map(OsString::from));
    arguments.push(root.as_os_str().to_owned());
    arguments
}

fn run_cli(arguments: &[OsString]) -> Output {
    let state_parent =
        tempfile::tempdir().unwrap_or_else(|error| panic!("failed to create isolated CLI state: {error}"));
    match Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args(arguments)
        .env(STATE_DIRECTORY_ENV, state_parent.path())
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => panic!("failed to start reporigor: {error}"),
    }
}

fn assert_exit(output: &Output, expected: i32, context: &str) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "{context} returned {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_source_too_large(output: &Output, context: &str, max_source_bytes: u64) {
    assert_exit(output, 1, context);
    assert!(output.stdout.is_empty(), "{context} emitted a partial report");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("source")
            && stderr.contains("is at least")
            && stderr.contains(&format!("max_source_bytes ({max_source_bytes} bytes)")),
        "{context} returned a non-contract error: {stderr}"
    );
}

fn assert_report_context(report: &ReportEnvelope, command: ReportCommand, root: &Path, files: usize) {
    assert_eq!(report.schema_version, 1);
    assert_eq!(report.command, command);
    assert_eq!(report.root, canonical(root));
    assert_eq!(report.summary.files, files);
    assert_eq!(report.summary.parse_errors, 0);
}

fn assert_generic_backend(report: &ReportEnvelope) {
    assert!(
        report
            .backends
            .iter()
            .any(|backend| backend.id == "tree-sitter-generic" && !backend.native),
        "generic backend missing from report: {:?}",
        report.backends
    );
}

fn fixture_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(relative)
}

fn canonical(path: &Path) -> PathBuf {
    match path.canonicalize() {
        Ok(path) => path,
        Err(error) => panic!("failed to canonicalize fixture {}: {error}", path.display()),
    }
}

fn command_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn write_fixture(path: &Path, contents: &str) {
    if let Err(error) = fs::write(path, contents) {
        panic!("failed to write fixture {}: {error}", path.display());
    }
}
