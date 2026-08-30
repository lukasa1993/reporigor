use std::error::Error;
use std::fs;
use std::process::{Command, Stdio};

pub mod support;
use support::fixtures::{retained_python_project, write_executable_fixture, write_fixture};
use support::invocation::run_at;
use support::success_assertion::assert_success;

#[cfg(unix)]
#[test]
fn provider_text_escapes_tool_control_sequences_but_json_preserves_data() -> Result<(), Box<dyn Error>> {
    let project = hostile_provider_project()?;
    assert_provider_text_escaping(project.path())?;
    assert_provider_json_data(project.path())?;
    Ok(())
}

#[cfg(unix)]
fn hostile_provider_project() -> Result<tempfile::TempDir, Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    write_fixture(
        &project.path().join("pyproject.toml"),
        "[project]\nname = \"terminal-safety\"\nversion = \"0.1.0\"\n",
    );
    write_fixture(&project.path().join("sample.py"), "value = 1\n");
    let executable = project.path().join(".venv/bin/mutmut");
    write_executable_fixture(
        &executable,
        "#!/bin/sh\nprintf 'hostile\\033[31m\\t\\302\\205\\342\\200\\256' >&2\nexit 23\n",
    )?;
    Ok(project)
}

#[cfg(unix)]
fn assert_provider_text_escaping(project: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let text = Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args(["providers", "--preflight"])
        .arg(project)
        .output()?;
    assert_success(&text, "provider text");
    let text = String::from_utf8(text.stdout)?;
    for raw in ['\u{1b}', '\t', '\u{85}', '\u{202e}'] {
        assert!(!text.contains(raw), "provider text emitted raw {raw:?}: {text}");
    }
    for escaped in [r"\u{1b}", r"\u{9}", r"\u{85}", r"\u{202e}"] {
        assert!(text.contains(escaped), "provider text omitted {escaped}: {text}");
    }
    Ok(())
}

#[cfg(unix)]
fn assert_provider_json_data(project: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let json = run_at(&["--format", "json", "providers", "--preflight"], project)?;
    assert_success(&json, "provider JSON");
    let json: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    let reason = json["mutation"]["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|status| status["id"] == "mutmut")
        .and_then(|status| status["reason"].as_str())
        .ok_or("mutmut reason")?;
    for raw in ['\u{1b}', '\t', '\u{85}', '\u{202e}'] {
        assert!(
            reason.contains(raw),
            "provider JSON changed raw {raw:?}: {reason:?}"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn runtime_errors_escape_repo_controlled_filenames_for_unified_and_legacy_entrypoints(
) -> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    let hostile = "forged-line\n\u{1b}[31m\u{202e}.py";
    fs::write(project.path().join(hostile), "def broken(:\n")?;

    let unified = run_at(
        &["--backend", "generic", "--language", "python", "dry"],
        project.path(),
    )?;
    assert_escaped_error(&unified, "unified error");

    let legacy = Command::new(env!("CARGO_BIN_EXE_dry4python"))
        .args(["--root"])
        .arg(project.path())
        .output()?;
    assert_escaped_error(&legacy, "legacy error");
    Ok(())
}

#[test]
fn closed_terminal_streams_do_not_turn_expected_exits_into_panics() -> Result<(), Box<dyn Error>> {
    let project = retained_python_project();

    let mut success = Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args("--backend|generic|--language|python|dry|--min-tokens|1000".split('|'))
        .arg(project.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    drop(success.stdout.take());
    let success = success.wait_with_output()?;
    assert_eq!(
        success.status.code(),
        Some(0),
        "closed stdout changed success exit; stderr: {}",
        String::from_utf8_lossy(&success.stderr)
    );

    let missing = project.path().join("missing-root");
    let mut failure = Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .arg("dry")
        .arg(missing)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    drop(failure.stderr.take());
    let failure = failure.wait()?;
    assert_eq!(failure.code(), Some(1), "closed stderr caused a panic exit");
    Ok(())
}

fn assert_escaped_error(output: &std::process::Output, context: &str) {
    assert_eq!(output.status.code(), Some(1), "{context} returned the wrong exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains('\u{1b}'),
        "{context} emitted raw escape: {stderr:?}"
    );
    assert!(
        !stderr.contains('\u{202e}'),
        "{context} emitted raw bidi control: {stderr:?}"
    );
    assert!(
        !stderr.lines().any(|line| line == "forged-line"),
        "{context} allowed line injection: {stderr:?}"
    );
    assert!(
        stderr.contains(r"forged-line\u{a}\u{1b}[31m\u{202e}.py"),
        "{context} omitted escaped path: {stderr:?}"
    );
}
