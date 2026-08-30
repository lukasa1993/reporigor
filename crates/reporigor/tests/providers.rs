use std::error::Error;
use std::path::Path;

pub mod support;
use support::fixtures::write_executable_fixture;
use support::invocation::run;
use support::success_assertion::assert_success;

#[test]
fn providers_json_includes_static_mutation_inventory_without_executing_tools() -> Result<(), Box<dyn Error>> {
    let (root, marker) = stryker_inventory_fixture()?;
    let output = run(["--format", "json", "providers"]
        .into_iter()
        .map(std::ffi::OsString::from)
        .chain(std::iter::once(root.path().as_os_str().to_owned())))?;
    assert_success(&output, "providers JSON");
    assert!(!marker.exists(), "static provider discovery executed Stryker");
    assert_provider_inventory(&output.stdout)
}

fn stryker_inventory_fixture() -> Result<(tempfile::TempDir, std::path::PathBuf), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("package.json"), "{}\n")?;
    let marker = root.path().join("probe-ran");
    let executable = if cfg!(windows) {
        root.path().join("node_modules/.bin/stryker.cmd")
    } else {
        root.path().join("node_modules/.bin/stryker")
    };
    write_executable(&executable, &marker)?;
    Ok((root, marker))
}

fn assert_provider_inventory(output: &[u8]) -> Result<(), Box<dyn Error>> {
    let document: serde_json::Value = serde_json::from_slice(output)?;
    let mutation = document["mutation"]["providers"]
        .as_array()
        .ok_or("mutation provider inventory must be an array")?;
    let built_in = provider(mutation, "built-in")?;
    assert_eq!(built_in["default"], true);
    assert_eq!(built_in["execution_enabled"], true);
    let stryker = provider(mutation, "stryker")?;
    assert_eq!(stryker["applicable"], true);
    assert_eq!(stryker["available"], true);
    assert_eq!(stryker["execution_enabled"], false);
    assert_eq!(stryker["detection"], "project-local");
    Ok(())
}

#[test]
fn providers_text_distinguishes_builtin_execution_from_external_import() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let output = run([
        std::ffi::OsString::from("providers"),
        root.path().as_os_str().to_owned(),
    ])?;
    assert_success(&output, "providers text");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("MUTATION_PROVIDER  DEFAULT"));
    assert!(stdout.contains("built-in  true  true  true  execute"));
    assert!(stdout.contains("cargo-mutants  false  false  "));
    assert!(stdout.contains("import-only"));
    Ok(())
}

fn write_executable(path: &Path, marker: &Path) -> Result<(), Box<dyn Error>> {
    let contents = if cfg!(windows) {
        format!("@echo off\r\ntype nul > \"{}\"\r\n", marker.display())
    } else {
        format!("#!/bin/sh\n: > \"{}\"\n", marker.display())
    };
    write_executable_fixture(path, &contents).map_err(Into::into)
}

fn provider<'a>(
    providers: &'a [serde_json::Value],
    id: &str,
) -> Result<&'a serde_json::Value, Box<dyn Error>> {
    providers
        .iter()
        .find(|status| status["id"] == id)
        .ok_or_else(|| format!("mutation provider {id} is missing").into())
}
