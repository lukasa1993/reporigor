use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use analysis_mutate::STATE_DIRECTORY_ENV;
use tempfile::tempdir;

pub mod support;
use support::exit_assertion::assert_exit;
use support::success_assertion::assert_success;

#[test]
fn legacy_launcher_preserves_alias_for_version_output() -> Result<(), Box<dyn std::error::Error>> {
    let output = run_legacy(env!("CARGO_BIN_EXE_dry4python"), None, None, "--version")?;
    assert_success(&output, "legacy version alias");
    assert!(
        String::from_utf8(output.stdout)?.starts_with("dry4python "),
        "the sibling reporigor process must receive the legacy invocation name"
    );
    let invalid = run_legacy(env!("CARGO_BIN_EXE_dry4python"), None, None, "--min-tokens|3")?;
    assert_legacy_error(&invalid, 2, "min-tokens must be at least 4", "legacy parse error");
    Ok(())
}

#[test]
fn isolated_legacy_launcher_explains_its_sibling_requirement() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempdir()?;
    let isolated = directory
        .path()
        .join(format!("dry4python{}", std::env::consts::EXE_SUFFIX));
    fs::copy(env!("CARGO_BIN_EXE_dry4python"), &isolated)?;

    let output = run_legacy(&isolated, None, None, "--version")?;
    assert_legacy_error(
        &output,
        1,
        "install the main reporigor binary beside this alias",
        "isolated legacy alias",
    );
    Ok(())
}

#[test]
fn dry_alias_accepts_legacy_root_json_and_filter_syntax() -> Result<(), Box<dyn std::error::Error>> {
    let (_, stdout) = run_fixture_alias(
        env!("CARGO_BIN_EXE_dry4python"),
        "def alpha(value):\n    return value + 1\n\ndef beta(value):\n    return value + 1\n",
        "sample|--min-tokens|4|--json",
        "legacy DRY alias",
        |_| Ok(()),
    )?;
    assert_legacy_report(&stdout, "dry", "sample.py");
    Ok(())
}

#[test]
fn mutate_alias_list_is_read_only() -> Result<(), Box<dyn std::error::Error>> {
    let source = "def compare(left, right):\n    return left == right\n";
    let (directory, stdout) = run_fixture_alias(
        env!("CARGO_BIN_EXE_mutate4python"),
        source,
        "--list|--json",
        "legacy mutation list alias",
        |_| Ok(()),
    )?;
    let source_path = directory.path().join("sample.py");
    assert!(stdout.contains("\"command\": \"mutate\""), "{stdout}");
    assert_eq!(fs::read_to_string(source_path)?, source);
    Ok(())
}

#[test]
fn crap_alias_reads_existing_coverage_and_preserves_gate_exit() -> Result<(), Box<dyn std::error::Error>> {
    let (directory, stdout) = run_fixture_alias(
        env!("CARGO_BIN_EXE_crap4python"),
        "def covered(value):\n    if value:\n        return 1\n    return 0\n",
        "--coverage|coverage.json|--no-test|--fail-over|100|--json",
        "legacy CRAP alias",
        |root| {
            fs::write(
                root.join("coverage.json"),
                r#"{
  "meta": {"version": "compat-test"},
  "files": {
    "sample.py": {
      "executed_lines": [1, 2, 3, 4],
      "missing_lines": [],
      "excluded_lines": [],
      "summary": {"covered_lines": 4, "num_statements": 4, "percent_covered": 100.0, "percent_covered_display": "100", "missing_lines": 0, "excluded_lines": 0}
    }
  }
}"#,
            )?;
            Ok(())
        },
    )?;
    assert!(directory.path().join("coverage.json").is_file());
    assert_legacy_report(&stdout, "crap", "covered");
    Ok(())
}

#[test]
fn every_crap_binary_rejects_invalid_thresholds_at_parse_time() -> Result<(), Box<dyn std::error::Error>> {
    let binary_directory = Path::new(env!("CARGO_BIN_EXE_reporigor"))
        .parent()
        .unwrap_or_else(|| panic!("test binary has no parent directory"));
    for alias in "crap4bash,crap4c,crap4cpp,crap4objc,crap4python,crap4rust,crap4swift,crap4ts".split(',') {
        let binary = binary_directory.join(format!("{alias}{}", std::env::consts::EXE_SUFFIX));
        for invalid in ["-1", "NaN", "inf"] {
            let arguments = format!("--fail-over|{invalid}");
            let output = run_legacy(&binary, None, None, &arguments)?;
            assert_eq!(
                output.status.code(),
                Some(2),
                "{alias} accepted {invalid:?}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let stderr = String::from_utf8(output.stderr)?;
            assert!(
                stderr.contains("value must be a non-negative finite number"),
                "{alias}: {invalid}: {stderr}"
            );
        }
    }
    Ok(())
}

#[test]
fn no_validate_suppresses_configured_validation_but_absence_preserves_fallback(
) -> Result<(), Box<dyn std::error::Error>> {
    let (directory, state_parent) = legacy_fixture("def compare(left, right):\n    return left == right\n")?;
    let (success_command, failure_command) = shell_exit_commands();
    fs::write(
        directory.path().join("reporigor.toml"),
        format!("[mutation]\nvalidation_command = {failure_command:?}\n"),
    )?;

    let suppressed_arguments = format!(
        "--test-command|{success_command}|--skip-baseline|--max-mutants|1|--no-validate|--allow-survivors|--json"
    );
    let suppressed_stdout = run_mutation_alias(
        directory.path(),
        state_parent.path(),
        &suppressed_arguments,
        "suppressed legacy validation",
    )?;
    assert!(
        suppressed_stdout.contains("\"status\": \"survived\""),
        "{suppressed_stdout}"
    );

    let fallback_arguments = format!(
        "--test-command|{success_command}|--skip-baseline|--max-mutants|1|--allow-compile-errors|--json"
    );
    let configured_stdout = run_mutation_alias(
        directory.path(),
        state_parent.path(),
        &fallback_arguments,
        "configured legacy validation fallback",
    )?;
    assert!(
        configured_stdout.contains("\"status\": \"compile-error\""),
        "{configured_stdout}"
    );
    Ok(())
}

fn legacy_fixture(
    source: &str,
) -> Result<(tempfile::TempDir, tempfile::TempDir), Box<dyn std::error::Error>> {
    let directory = tempdir()?;
    let state_parent = tempdir()?;
    fs::write(directory.path().join("sample.py"), source)?;
    Ok((directory, state_parent))
}

fn assert_legacy_error(output: &Output, exit: i32, needle: &str, context: &str) {
    assert_exit(output, exit, context);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(needle), "{context} was not actionable: {stderr}");
}

fn assert_legacy_report(stdout: &str, command: &str, marker: &str) {
    assert!(
        stdout.contains(&format!("\"command\": \"{command}\"")),
        "{stdout}"
    );
    assert!(stdout.contains(marker), "{stdout}");
}

fn run_mutation_alias(
    root: &Path,
    state: &Path,
    arguments: &str,
    context: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let output = run_legacy(
        env!("CARGO_BIN_EXE_mutate4python"),
        Some(root),
        Some(state),
        arguments,
    )?;
    assert_success(&output, context);
    Ok(String::from_utf8(output.stdout)?)
}

fn run_fixture_alias(
    binary: impl AsRef<std::ffi::OsStr>,
    source: &str,
    arguments: &str,
    context: &str,
    prepare: impl FnOnce(&Path) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(tempfile::TempDir, String), Box<dyn std::error::Error>> {
    let (directory, state) = legacy_fixture(source)?;
    prepare(directory.path())?;
    let output = run_legacy(binary, Some(directory.path()), Some(state.path()), arguments)?;
    assert_success(&output, context);
    Ok((directory, String::from_utf8(output.stdout)?))
}

fn run_legacy(
    binary: impl AsRef<std::ffi::OsStr>,
    root: Option<&Path>,
    state: Option<&Path>,
    arguments: &str,
) -> std::io::Result<Output> {
    let mut command = Command::new(binary);
    if let Some(root) = root {
        command.arg("--root").arg(root);
    }
    command.args(arguments.split('|'));
    if let Some(state) = state {
        command.env(STATE_DIRECTORY_ENV, state);
    }
    command.output()
}

const fn shell_exit_commands() -> (&'static str, &'static str) {
    #[cfg(windows)]
    {
        ("exit /B 0", "exit /B 23")
    }
    #[cfg(not(windows))]
    {
        ("exit 0", "exit 23")
    }
}
