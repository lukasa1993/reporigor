use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

#[test]
fn read_only_quality_commands_reject_an_empty_project() -> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    for arguments in [
        vec!["dry", "--min-tokens", "1000"],
        vec!["mutate", "--list"],
        vec!["check", "--min-tokens", "1000"],
    ] {
        let output = run(generic_arguments(&arguments, project.path()))?;
        assert_no_sources_error(&output, arguments[0]);
    }
    Ok(())
}

#[test]
fn stale_filter_cannot_turn_check_into_a_successful_empty_gate() -> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    fs::write(
        project.path().join("sample.py"),
        "def retained(value):\n    return value + 1\n",
    )?;
    let output = run([
        OsString::from("--backend"),
        OsString::from("generic"),
        OsString::from("--language"),
        OsString::from("python"),
        OsString::from("--filter"),
        OsString::from("stale/path/that/selects/nothing"),
        OsString::from("--format"),
        OsString::from("json"),
        OsString::from("check"),
        OsString::from("--min-tokens"),
        OsString::from("1000"),
        project.path().as_os_str().to_owned(),
    ])?;
    assert_no_sources_error(&output, "stale-filter check");
    Ok(())
}

#[test]
fn crap_retains_its_explicit_historical_empty_opt_in() -> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    let output = run(generic_arguments(
        &["crap", "--allow-empty", "--allow-missing-coverage"],
        project.path(),
    ))?;
    assert!(
        output.status.success(),
        "explicit CRAP empty opt-in failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["summary"]["files"], 0);
    Ok(())
}

fn generic_arguments(arguments: &[&str], root: &Path) -> Vec<OsString> {
    let mut output = vec![
        OsString::from("--backend"),
        OsString::from("generic"),
        OsString::from("--format"),
        OsString::from("json"),
    ];
    output.extend(arguments.iter().map(OsString::from));
    output.push(root.as_os_str().to_owned());
    output
}

fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<Output, std::io::Error> {
    Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args(arguments)
        .output()
}

fn assert_no_sources_error(output: &Output, context: &str) {
    assert_eq!(
        output.status.code(),
        Some(1),
        "{context} returned {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty(), "{context} emitted a successful report");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no source files were selected")
            && stderr.contains("--language")
            && stderr.contains("--filter"),
        "{context} returned a non-actionable error: {stderr}"
    );
}
