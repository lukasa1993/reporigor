use std::fs;
use std::process::Command;

use analysis_mutate::STATE_DIRECTORY_ENV;
use tempfile::tempdir;

#[test]
fn legacy_launcher_preserves_alias_for_version_output() -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_dry4python"))
        .arg("--version")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8(output.stdout)?.starts_with("dry4python "),
        "the sibling reporigor process must receive the legacy invocation name"
    );
    Ok(())
}

#[test]
fn isolated_legacy_launcher_explains_its_sibling_requirement() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempdir()?;
    let isolated = directory
        .path()
        .join(format!("dry4python{}", std::env::consts::EXE_SUFFIX));
    fs::copy(env!("CARGO_BIN_EXE_dry4python"), &isolated)?;

    let output = Command::new(isolated).arg("--version").output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)?.contains("install the main reporigor binary beside this alias"),
        "an isolated Cargo --bin alias must fail with an actionable message"
    );
    Ok(())
}

#[test]
fn dry_alias_accepts_legacy_root_json_and_filter_syntax() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempdir()?;
    let state_parent = tempdir()?;
    fs::write(
        directory.path().join("sample.py"),
        "def alpha(value):\n    return value + 1\n\ndef beta(value):\n    return value + 1\n",
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_dry4python"))
        .args(["sample", "--root"])
        .arg(directory.path())
        .args(["--min-tokens", "4", "--json"])
        .env(STATE_DIRECTORY_ENV, state_parent.path())
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("\"command\": \"dry\""), "{stdout}");
    assert!(stdout.contains("sample.py"), "{stdout}");
    Ok(())
}

#[test]
fn mutate_alias_list_is_read_only() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempdir()?;
    let state_parent = tempdir()?;
    let source_path = directory.path().join("sample.py");
    let source = "def compare(left, right):\n    return left == right\n";
    fs::write(&source_path, source)?;
    let output = Command::new(env!("CARGO_BIN_EXE_mutate4python"))
        .args(["--root"])
        .arg(directory.path())
        .args(["--list", "--json"])
        .env(STATE_DIRECTORY_ENV, state_parent.path())
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("\"command\": \"mutate\""), "{stdout}");
    assert_eq!(fs::read_to_string(source_path)?, source);
    Ok(())
}

#[test]
fn crap_alias_reads_existing_coverage_and_preserves_gate_exit() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempdir()?;
    let state_parent = tempdir()?;
    fs::write(
        directory.path().join("sample.py"),
        "def covered(value):\n    if value:\n        return 1\n    return 0\n",
    )?;
    fs::write(
        directory.path().join("coverage.json"),
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
    let output = Command::new(env!("CARGO_BIN_EXE_crap4python"))
        .args(["--root"])
        .arg(directory.path())
        .args([
            "--coverage",
            "coverage.json",
            "--no-test",
            "--fail-over",
            "100",
            "--json",
        ])
        .env(STATE_DIRECTORY_ENV, state_parent.path())
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("\"command\": \"crap\""), "{stdout}");
    assert!(stdout.contains("covered"), "{stdout}");
    Ok(())
}

#[test]
fn legacy_parse_errors_still_exit_two() -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_dry4python"))
        .args(["--min-tokens", "3"])
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr)?.contains("min-tokens must be at least 4"));
    Ok(())
}

#[test]
fn every_crap_binary_rejects_invalid_thresholds_at_parse_time() -> Result<(), Box<dyn std::error::Error>> {
    let binaries = [
        ("crap4bash", env!("CARGO_BIN_EXE_crap4bash")),
        ("crap4c", env!("CARGO_BIN_EXE_crap4c")),
        ("crap4cpp", env!("CARGO_BIN_EXE_crap4cpp")),
        ("crap4objc", env!("CARGO_BIN_EXE_crap4objc")),
        ("crap4python", env!("CARGO_BIN_EXE_crap4python")),
        ("crap4rust", env!("CARGO_BIN_EXE_crap4rust")),
        ("crap4swift", env!("CARGO_BIN_EXE_crap4swift")),
        ("crap4ts", env!("CARGO_BIN_EXE_crap4ts")),
    ];

    for (alias, binary) in binaries {
        for invalid in ["-1", "NaN", "inf"] {
            let output = Command::new(binary).args(["--fail-over", invalid]).output()?;
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
    let directory = tempdir()?;
    let state_parent = tempdir()?;
    fs::write(
        directory.path().join("sample.py"),
        "def compare(left, right):\n    return left == right\n",
    )?;
    let (success_command, failure_command) = shell_exit_commands();
    fs::write(
        directory.path().join("reporigor.toml"),
        format!("[mutation]\nvalidation_command = {failure_command:?}\n"),
    )?;

    let suppressed = Command::new(env!("CARGO_BIN_EXE_mutate4python"))
        .args(["--root"])
        .arg(directory.path())
        .args([
            "--test-command",
            success_command,
            "--skip-baseline",
            "--max-mutants",
            "1",
            "--no-validate",
            "--allow-survivors",
            "--json",
        ])
        .env(STATE_DIRECTORY_ENV, state_parent.path())
        .output()?;
    assert!(
        suppressed.status.success(),
        "{}",
        String::from_utf8_lossy(&suppressed.stderr)
    );
    let suppressed_stdout = String::from_utf8(suppressed.stdout)?;
    assert!(
        suppressed_stdout.contains("\"status\": \"survived\""),
        "{suppressed_stdout}"
    );

    let configured_fallback = Command::new(env!("CARGO_BIN_EXE_mutate4python"))
        .args(["--root"])
        .arg(directory.path())
        .args([
            "--test-command",
            success_command,
            "--skip-baseline",
            "--max-mutants",
            "1",
            "--allow-compile-errors",
            "--json",
        ])
        .env(STATE_DIRECTORY_ENV, state_parent.path())
        .output()?;
    assert!(
        configured_fallback.status.success(),
        "{}",
        String::from_utf8_lossy(&configured_fallback.stderr)
    );
    let configured_stdout = String::from_utf8(configured_fallback.stdout)?;
    assert!(
        configured_stdout.contains("\"status\": \"compile-error\""),
        "{configured_stdout}"
    );
    Ok(())
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
