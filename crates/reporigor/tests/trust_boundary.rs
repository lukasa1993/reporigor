use std::{
    error::Error,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub mod support;
use support::fixtures::{temporary_project_file, write_executable_fixture, write_fixture};
use support::invocation::run;
use support::json_arguments_with_globals::json_arguments_with_globals;
use support::operational_error::operational_error;
use support::success_assertion::assert_success;

#[test]
fn auto_is_filesystem_only_native_requires_consent_and_provider_preflight_remains_explicit(
) -> Result<(), Box<dyn Error>> {
    let (project, marker) = typescript_fixture()?;
    assert_auto_filesystem_only(project.path(), &marker)?;
    assert_native_requires_consent(project.path(), &marker)?;
    assert_explicit_preflight(project.path(), &marker)?;
    Ok(())
}

fn typescript_fixture() -> Result<(tempfile::TempDir, PathBuf), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    let source = project.path().join("src/app.ts");
    write_fixture(
        &source,
        "export function value(input: number) { return input + 1; }\n",
    );
    write_fixture(&project.path().join("tsconfig.json"), "{}\n");
    write_fixture(
        &project.path().join("package.json"),
        r#"{"name":"trust-boundary","devDependencies":{"typescript":"7.0.0"}}"#,
    );

    let marker = project.path().join("provider-ran");
    let executable = if cfg!(windows) {
        project.path().join("node_modules/.bin/tsc.cmd")
    } else {
        project.path().join("node_modules/.bin/tsc")
    };
    write_typescript_provider(&executable, &marker, &source)?;
    Ok((project, marker))
}

fn assert_auto_filesystem_only(project: &Path, marker: &Path) -> Result<(), Box<dyn Error>> {
    let automatic = run_typescript_dry(project, "auto")?;
    assert_no_provider_run(&automatic, marker, "filesystem-only auto analysis")?;
    Ok(())
}

fn assert_native_requires_consent(project: &Path, marker: &Path) -> Result<(), Box<dyn Error>> {
    let native = run_typescript_dry(project, "native")?;
    let stderr = operational_error(&native, "native analysis without consent");
    assert!(stderr.contains("--allow-project-exec"));
    assert!(
        !marker.exists(),
        "rejected native analysis executed project-local tsc"
    );
    Ok(())
}

fn run_typescript_dry(project: &Path, backend: &str) -> Result<std::process::Output, Box<dyn Error>> {
    Ok(run(json_arguments_with_globals(
        backend,
        "typescript",
        false,
        &[],
        ("dry", &["--min-tokens", "1000"]),
        project,
    ))?)
}

fn assert_no_provider_run(
    output: &std::process::Output,
    marker: &Path,
    context: &str,
) -> Result<(), Box<dyn Error>> {
    assert_success(output, context);
    assert!(!marker.exists(), "{context} executed a project-local provider");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_consent_diagnostic(&report);
    Ok(())
}

fn assert_explicit_preflight(project: &Path, marker: &Path) -> Result<(), Box<dyn Error>> {
    let preflight = run([
        OsString::from("--format"),
        OsString::from("json"),
        OsString::from("providers"),
        project.as_os_str().to_os_string(),
        OsString::from("--preflight"),
    ])?;
    assert_success(&preflight, "explicit provider preflight");
    assert!(
        marker.exists(),
        "providers --preflight did not execute the explicit probe"
    );
    Ok(())
}

fn assert_consent_diagnostic(report: &serde_json::Value) {
    assert!(report["diagnostics"].as_array().is_some_and(|diagnostics| {
        diagnostics.iter().any(|diagnostic| {
            diagnostic["fallback_used"] == true
                && diagnostic["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("--allow-project-exec"))
        })
    }));
}

#[cfg(unix)]
#[test]
fn auto_does_not_spawn_a_clang_found_on_path_without_consent() -> Result<(), Box<dyn Error>> {
    let (project, marker, search_path) = clang_trust_fixture()?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporigor"));
    command
        .args(["--format", "json", "dry"])
        .arg(project.path())
        .args(["--min-tokens", "1000"])
        .env("PATH", search_path);
    let output = command.output()?;
    assert_no_provider_run(&output, &marker, "filesystem-only auto Clang analysis")?;
    Ok(())
}

#[cfg(unix)]
fn clang_trust_fixture() -> Result<(tempfile::TempDir, PathBuf, OsString), Box<dyn Error>> {
    let project = temporary_project_file("src/main.c", "int value(int input) { return input + 1; }\n");
    let source = project.path().join("src/main.c");
    let database = serde_json::json!([{
        "directory": project.path(),
        "file": source,
        "arguments": ["clang", "-c", source]
    }]);
    write_fixture(
        &project.path().join("compile_commands.json"),
        &serde_json::to_string_pretty(&database)?,
    );

    let bin = project.path().join("fake-bin");
    fs::create_dir(&bin)?;
    let marker = project.path().join("clang-ran");
    let clang = bin.join("clang");
    write_marker_executable(&clang, &marker)?;
    let search_path = absolute_search_path(&bin)?;
    Ok((project, marker, search_path))
}

fn write_typescript_provider(path: &Path, marker: &Path, source: &Path) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let probes = [
        ("--version", "Version 7.0.0".to_owned()),
        ("--showConfig", r#"{"compilerOptions":{}}"#.to_owned()),
        ("--listFilesOnly", source.to_string_lossy().into_owned()),
    ];
    let contents = provider_script(&provider_path(marker), &probes);
    write_executable(path, &contents)
}

fn provider_script(marker: &str, probes: &[(&str, String)]) -> String {
    if cfg!(windows) {
        let commands = probes
            .iter()
            .map(|(flag, value)| format!("if \"%1\"==\"{flag}\" echo {value}"))
            .collect::<Vec<_>>()
            .join("\r\n");
        format!("@echo off\r\ntype nul > \"{marker}\"\r\n{commands}\r\n")
    } else {
        let cases = probes
            .iter()
            .map(|(flag, value)| {
                let value = shell_quote_text(value);
                format!("  {flag}) printf '%s\\n' {value} ;;")
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("#!/bin/sh\n: > {marker}\ncase \"$1\" in\n{cases}\nesac\n")
    }
}

fn provider_path(path: &Path) -> String {
    if cfg!(windows) {
        path.display().to_string()
    } else {
        shell_quote(path)
    }
}

#[cfg(unix)]
fn write_marker_executable(path: &Path, marker: &Path) -> Result<(), Box<dyn Error>> {
    let contents = format!("#!/bin/sh\n: > {}\nexit 99\n", shell_quote(marker));
    write_executable(path, &contents)
}

fn write_executable(path: &Path, contents: &str) -> Result<(), Box<dyn Error>> {
    write_executable_fixture(path, contents).map_err(Into::into)
}

fn shell_quote(path: &Path) -> String {
    shell_quote_text(&path.to_string_lossy())
}

fn shell_quote_text(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(unix)]
fn absolute_search_path(first: &Path) -> Result<OsString, Box<dyn Error>> {
    let remaining = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<PathBuf>>())
        .filter(|path| path.is_absolute());
    Ok(std::env::join_paths(
        std::iter::once(first.to_path_buf()).chain(remaining),
    )?)
}
