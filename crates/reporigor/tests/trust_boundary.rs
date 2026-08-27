use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

#[cfg(unix)]
use std::path::PathBuf;

#[test]
fn auto_is_filesystem_only_native_requires_consent_and_provider_preflight_remains_explicit(
) -> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    let source = project.path().join("src/app.ts");
    fs::create_dir_all(source.parent().ok_or("source parent")?)?;
    fs::write(
        &source,
        "export function value(input: number) { return input + 1; }\n",
    )?;
    fs::write(project.path().join("tsconfig.json"), "{}\n")?;
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"trust-boundary","devDependencies":{"typescript":"7.0.0"}}"#,
    )?;

    let marker = project.path().join("provider-ran");
    let executable = if cfg!(windows) {
        project.path().join("node_modules/.bin/tsc.cmd")
    } else {
        project.path().join("node_modules/.bin/tsc")
    };
    write_typescript_provider(&executable, &marker, &source)?;

    let automatic = run([
        OsString::from("--format"),
        OsString::from("json"),
        OsString::from("dry"),
        project.path().as_os_str().to_os_string(),
        OsString::from("--min-tokens"),
        OsString::from("1000"),
    ])?;
    assert_success(&automatic, "filesystem-only auto analysis");
    assert!(
        !marker.exists(),
        "default auto analysis executed project-local tsc"
    );
    let report: serde_json::Value = serde_json::from_slice(&automatic.stdout)?;
    assert!(report["diagnostics"].as_array().is_some_and(|diagnostics| {
        diagnostics.iter().any(|diagnostic| {
            diagnostic["fallback_used"] == true
                && diagnostic["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("--allow-project-exec"))
        })
    }));

    let native = run([
        OsString::from("--backend"),
        OsString::from("native"),
        OsString::from("dry"),
        project.path().as_os_str().to_os_string(),
        OsString::from("--min-tokens"),
        OsString::from("1000"),
    ])?;
    assert_eq!(native.status.code(), Some(1));
    assert!(String::from_utf8(native.stderr)?.contains("--allow-project-exec"));
    assert!(
        !marker.exists(),
        "rejected native analysis executed project-local tsc"
    );

    let preflight = run([
        OsString::from("--format"),
        OsString::from("json"),
        OsString::from("providers"),
        project.path().as_os_str().to_os_string(),
        OsString::from("--preflight"),
    ])?;
    assert_success(&preflight, "explicit provider preflight");
    assert!(
        marker.exists(),
        "providers --preflight did not execute the explicit probe"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn auto_does_not_spawn_a_clang_found_on_path_without_consent() -> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    let source = project.path().join("src/main.c");
    fs::create_dir_all(source.parent().ok_or("source parent")?)?;
    fs::write(&source, "int value(int input) { return input + 1; }\n")?;
    let database = serde_json::json!([{
        "directory": project.path(),
        "file": source,
        "arguments": ["clang", "-c", source]
    }]);
    fs::write(
        project.path().join("compile_commands.json"),
        serde_json::to_vec_pretty(&database)?,
    )?;

    let bin = project.path().join("fake-bin");
    fs::create_dir(&bin)?;
    let marker = project.path().join("clang-ran");
    let clang = bin.join("clang");
    write_marker_executable(&clang, &marker)?;
    let search_path = absolute_search_path(&bin)?;

    let mut command = Command::new(env!("CARGO_BIN_EXE_reporigor"));
    command
        .args(["--format", "json", "dry"])
        .arg(project.path())
        .args(["--min-tokens", "1000"])
        .env("PATH", search_path);
    let output = command.output()?;
    assert_success(&output, "filesystem-only auto Clang analysis");
    assert!(!marker.exists(), "default auto analysis executed Clang");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert!(report["diagnostics"].as_array().is_some_and(|diagnostics| {
        diagnostics.iter().any(|diagnostic| {
            diagnostic["fallback_used"] == true
                && diagnostic["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("--allow-project-exec"))
        })
    }));
    Ok(())
}

fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<Output, std::io::Error> {
    Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args(arguments)
        .output()
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_typescript_provider(path: &Path, marker: &Path, source: &Path) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    let contents = format!(
        "#!/bin/sh\n: > {}\ncase \"$1\" in\n  --version) printf '%s\\n' 'Version 7.0.0' ;;\n  --showConfig) printf '%s\\n' '{{\"compilerOptions\":{{}}}}' ;;\n  --listFilesOnly) printf '%s\\n' {} ;;\nesac\n",
        shell_quote(marker),
        shell_quote(source),
    );
    #[cfg(windows)]
    let contents = format!(
        "@echo off\r\ntype nul > \"{}\"\r\nif \"%1\"==\"--version\" echo Version 7.0.0\r\nif \"%1\"==\"--showConfig\" echo {{\"compilerOptions\":{{}}}}\r\nif \"%1\"==\"--listFilesOnly\" echo {}\r\n",
        marker.display(),
        source.display(),
    );
    fs::write(path, contents)?;
    make_executable(path)?;
    Ok(())
}

#[cfg(unix)]
fn write_marker_executable(path: &Path, marker: &Path) -> Result<(), Box<dyn Error>> {
    fs::write(path, format!("#!/bin/sh\n: > {}\nexit 99\n", shell_quote(marker)))?;
    make_executable(path)?;
    Ok(())
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn make_executable(path: &Path) -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
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
