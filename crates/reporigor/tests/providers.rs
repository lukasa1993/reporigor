use std::error::Error;
use std::path::Path;
use std::process::Command;

#[test]
fn providers_json_includes_static_mutation_inventory_without_executing_tools() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("package.json"), "{}\n")?;
    let marker = root.path().join("probe-ran");
    let executable = if cfg!(windows) {
        root.path().join("node_modules/.bin/stryker.cmd")
    } else {
        root.path().join("node_modules/.bin/stryker")
    };
    write_executable(&executable, &marker)?;

    let output = Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args(["--format", "json", "providers"])
        .arg(root.path())
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!marker.exists(), "static provider discovery executed Stryker");
    let document: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let mutation = document["mutation"]["providers"]
        .as_array()
        .ok_or("mutation provider inventory must be an array")?;
    let built_in = mutation
        .iter()
        .find(|status| status["id"] == "built-in")
        .ok_or("built-in mutation provider missing")?;
    assert_eq!(built_in["default"], true);
    assert_eq!(built_in["execution_enabled"], true);
    let stryker = mutation
        .iter()
        .find(|status| status["id"] == "stryker")
        .ok_or("Stryker mutation provider missing")?;
    assert_eq!(stryker["applicable"], true);
    assert_eq!(stryker["available"], true);
    assert_eq!(stryker["execution_enabled"], false);
    assert_eq!(stryker["detection"], "project-local");
    Ok(())
}

#[test]
fn providers_text_distinguishes_builtin_execution_from_external_import() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_reporigor"))
        .args(["providers"])
        .arg(root.path())
        .output()?;
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("MUTATION_PROVIDER  DEFAULT"));
    assert!(stdout.contains("built-in  true  true  true  execute"));
    assert!(stdout.contains("cargo-mutants  false  false  "));
    assert!(stdout.contains("import-only"));
    Ok(())
}

fn write_executable(path: &Path, marker: &Path) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = if cfg!(windows) {
        format!("@echo off\r\ntype nul > \"{}\"\r\n", marker.display())
    } else {
        format!("#!/bin/sh\n: > \"{}\"\n", marker.display())
    };
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}
