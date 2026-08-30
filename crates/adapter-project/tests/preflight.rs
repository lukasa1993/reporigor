use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use adapter_project::{
    providers, CommandRunner, ProjectAdapter, ProviderCommand, ProviderCommandOutput, ProviderOptions,
    ProviderProvenance, ProviderResolution, ProviderStatus,
};
use reporigor_core::{AnalysisRequest, Capability, CoreError, Language};
use tempfile::TempDir;

mod support;
use support::{fixture_executable, write_fixtures};

fn temporary_fixture(files: &[(&str, &str)]) -> TempDir {
    let fixture = TempDir::new().unwrap_or_else(|error| panic!("fixture: {error}"));
    write_fixtures(fixture.path(), files);
    fixture
}

fn typescript_fixture(config: &str, sources: &[(&str, &str)]) -> (TempDir, PathBuf) {
    let fixture = temporary_fixture(&[("tsconfig.json", config)]);
    write_fixtures(fixture.path(), sources);
    let compiler = fixture_executable(fixture.path(), "node_modules/.bin/tsc");
    (fixture, compiler)
}

fn preflight_with<R: CommandRunner>(
    fixture: &TempDir,
    options: ProviderOptions,
    runner: R,
) -> ProviderResolution {
    ProjectAdapter::with_runner(options, runner)
        .preflight(&AnalysisRequest::new(fixture.path().to_path_buf()))
        .unwrap_or_else(|error| panic!("preflight: {error}"))
}

fn status<'a>(resolution: &'a ProviderResolution, id: &str) -> &'a ProviderStatus {
    resolution
        .inventory
        .iter()
        .find(|status| status.id == id)
        .unwrap_or_else(|| panic!("{id} status"))
}

fn unavailable_status_with_reason<'a>(
    resolution: &'a ProviderResolution,
    id: &str,
    reason_fragment: &str,
) -> &'a ProviderStatus {
    let provider = status(resolution, id);
    assert!(!provider.available);
    assert!(provider
        .reason
        .as_deref()
        .is_some_and(|reason| reason.contains(reason_fragment)));
    provider
}

fn provenance<'a>(resolution: &'a ProviderResolution, id: &str) -> &'a ProviderProvenance {
    resolution
        .provenance
        .iter()
        .find(|item| item.id == id)
        .unwrap_or_else(|| panic!("{id} provenance"))
}

fn sources_for(resolution: &ProviderResolution, language: Language) -> Vec<&str> {
    resolution
        .context
        .sources
        .iter()
        .filter(|source| source.language == language)
        .map(|source| source.relative.as_str())
        .collect()
}

fn assert_diagnostic_contains(resolution: &ProviderResolution, backend: &str, fragments: &[&str]) {
    let matched = resolution.context.diagnostics.iter().any(|diagnostic| {
        diagnostic.backend == backend
            && fragments
                .iter()
                .all(|fragment| diagnostic.message.contains(fragment))
    });
    assert!(matched, "missing {backend} diagnostic containing {fragments:?}");
}

fn diagnostic<'a>(resolution: &'a ProviderResolution, backend: &str) -> &'a reporigor_core::Diagnostic {
    let index = resolution
        .context
        .diagnostics
        .iter()
        .position(|diagnostic| diagnostic.backend == backend)
        .unwrap_or_else(|| panic!("{backend} diagnostic"));
    &resolution.context.diagnostics[index]
}

#[derive(Debug, Clone)]
struct RecordingRunner {
    root: PathBuf,
    calls: Arc<Mutex<Vec<ProviderCommand>>>,
}

#[derive(Debug)]
struct FailingRunner {
    version_text: &'static str,
    failure_text: &'static str,
    failure_code: i32,
}

#[derive(Clone, Copy)]
enum FailureFixture {
    TypeScript,
    Swift,
}

#[derive(Debug)]
struct TruncatedTypeScriptRunner;

fn version_or_failure_output(
    command: &ProviderCommand,
    version_text: &str,
    failure_text: &str,
    failure_code: i32,
) -> ProviderCommandOutput {
    let is_version = command.args.first() == Some(&"--version".into());
    ProviderCommandOutput {
        exit_code: Some(if is_version { 0 } else { failure_code }),
        stdout: if is_version {
            version_text.to_string()
        } else {
            String::new()
        },
        stderr: if is_version {
            String::new()
        } else {
            failure_text.to_string()
        },
        output_truncated: false,
    }
}

impl CommandRunner for FailingRunner {
    fn run(&self, command: &ProviderCommand) -> Result<ProviderCommandOutput, CoreError> {
        Ok(version_or_failure_output(
            command,
            self.version_text,
            self.failure_text,
            self.failure_code,
        ))
    }
}

fn failing_runner(fixture: FailureFixture) -> FailingRunner {
    let (version_text, failure_text, failure_code) = match fixture {
        FailureFixture::TypeScript => ("Version 7.0.2\n", "invalid tsconfig fixture", 2),
        FailureFixture::Swift => (
            "Swift version 6.3.3\n",
            "Package.resolved is out of date and automatic resolution is disabled",
            1,
        ),
    };
    FailingRunner {
        version_text,
        failure_text,
        failure_code,
    }
}

impl CommandRunner for TruncatedTypeScriptRunner {
    fn run(&self, command: &ProviderCommand) -> Result<ProviderCommandOutput, CoreError> {
        let list_files = command.args.first() == Some(&"--listFilesOnly".into());
        let show_config = command.args.first() == Some(&"--showConfig".into());
        Ok(ProviderCommandOutput {
            exit_code: Some(0),
            stdout: if list_files {
                format!("{}\n", command.cwd.join("index.ts").display())
            } else if show_config {
                "{\"compilerOptions\":{}}\n".to_string()
            } else {
                "Version 7.0.2\n".to_string()
            },
            stderr: String::new(),
            output_truncated: list_files,
        })
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, command: &ProviderCommand) -> Result<ProviderCommandOutput, CoreError> {
        record_command(&self.calls, command);
        let name = command_name(command);
        let args = command_arguments(command);
        let stdout = recorded_stdout(&self.root, name, &args);
        Ok(ProviderCommandOutput {
            exit_code: Some(0),
            stdout,
            stderr: String::new(),
            output_truncated: false,
        })
    }
}

fn recording_runner(root: PathBuf) -> RecordingRunner {
    RecordingRunner {
        root,
        calls: Arc::new(Mutex::new(Vec::new())),
    }
}

fn recorded_calls(runner: &RecordingRunner) -> Vec<ProviderCommand> {
    runner
        .calls
        .lock()
        .unwrap_or_else(|error| panic!("calls lock: {error}"))
        .clone()
}

fn assert_program_not_called(runner: &RecordingRunner, name: &str) {
    assert!(recorded_calls(runner)
        .iter()
        .all(|command| command.program.file_name() != Some(OsStr::new(name))));
}

fn record_command(calls: &Mutex<Vec<ProviderCommand>>, command: &ProviderCommand) {
    calls
        .lock()
        .unwrap_or_else(|error| panic!("calls lock: {error}"))
        .push(command.clone());
}

fn command_name(command: &ProviderCommand) -> &str {
    command
        .program
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
}

fn command_arguments(command: &ProviderCommand) -> Vec<String> {
    command
        .args
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
}

fn recorded_stdout(root: &std::path::Path, name: &str, args: &[String]) -> String {
    if name == "tsc" {
        return recorded_typescript_stdout(root, args);
    }
    if name == "swift" {
        return recorded_swift_stdout(args);
    }
    recorded_simple_stdout(name).unwrap_or_default().to_string()
}

fn recorded_typescript_stdout(root: &std::path::Path, args: &[String]) -> String {
    if args.iter().any(|argument| argument == "--showConfig") {
        return r#"{"compilerOptions":{"strict":true},"files":["src/index.ts"],"futureField":true}"#
            .to_string();
    }
    if args.iter().any(|argument| argument == "--listFilesOnly") {
        return format!(
            "/outside/lib.es2025.d.ts\r\n{}\r\n",
            root.join("src/index.ts").display()
        );
    }
    "Version 7.0.2\n".to_string()
}

fn recorded_swift_stdout(args: &[String]) -> String {
    if args == swift_description_arguments() {
        r#"{"name":"Demo","tools_version":"6.0","targets":[{"name":"Demo"},{"name":"DemoTests"}],"new_field":42}"#.to_string()
    } else {
        "Swift version 6.3.3\n".to_string()
    }
}

fn swift_description_arguments() -> Vec<String> {
    "package --disable-automatic-resolution --skip-update --disable-netrc describe --type json"
        .split_ascii_whitespace()
        .map(str::to_string)
        .collect()
}

fn recorded_simple_stdout(name: &str) -> Option<&'static str> {
    let index = ["python", "bash", "shellcheck"]
        .into_iter()
        .position(|candidate| candidate == name)?;
    [
        "Python 3.14.0\n",
        "GNU bash, version 5.2.0\n",
        "ShellCheck - shell script analysis tool\nversion: 0.10.0\n",
    ]
    .get(index)
    .copied()
}

#[test]
fn explicit_preflight_uses_native_cli_contracts_and_records_provenance() {
    let fixture = fixture();
    let root = fixture
        .path()
        .canonicalize()
        .unwrap_or_else(|error| panic!("root: {error}"));
    let runner = recording_runner(root.clone());
    let options = ProviderOptions {
        typescript_tsc: Some(root.join("node_modules/.bin/tsc")),
        swift: Some(root.join("tools/swift")),
        python: Some(root.join("tools/python")),
        bash: Some(root.join("tools/bash")),
        shellcheck: Some(root.join("tools/shellcheck")),
        ..ProviderOptions::default()
    };
    let adapter = ProjectAdapter::with_runner(options, runner.clone());
    let resolution = adapter
        .preflight(&AnalysisRequest::new(root.clone()))
        .unwrap_or_else(|error| panic!("preflight: {error}"));

    for expectation in
        "typescript=Version 7.0.2|swiftpm=Swift version 6.3.3|python=Python 3.14.0|shellcheck=0.10.0"
            .split('|')
    {
        let (provider, expected) = expectation
            .split_once('=')
            .unwrap_or_else(|| panic!("invalid version fixture: {expectation}"));
        assert_eq!(version(&resolution.inventory, provider), Some(expected));
    }
    assert_eq!(resolution.context.backends.len(), 5);
    for id in ["python", "shellcheck"] {
        assert!(!status(&resolution, id)
            .capabilities
            .contains(Capability::ParseValidation));
    }

    assert_eq!(
        sources_for(&resolution, Language::TypeScript),
        vec!["src/index.ts"]
    );

    let typescript = provenance(&resolution, "typescript");
    assert_eq!(
        typescript.metadata.get("integration_mode").map(String::as_str),
        Some("cli")
    );
    assert_eq!(
        typescript
            .metadata
            .get("configured_source_count")
            .map(String::as_str),
        Some("1")
    );
    let swift = provenance(&resolution, "swiftpm");
    assert_eq!(swift.metadata.get("target_count").map(String::as_str), Some("2"));
    let python = provenance(&resolution, "python");
    assert!(python.metadata.contains_key("setup_script"));
    assert!(!root.join("setup-was-executed").exists());
    assert_diagnostic_contains(&resolution, "project-typescript-preflight", &["TypeScript 7"]);

    assert_preflight_commands(&root, &recorded_calls(&runner));
}

#[test]
fn missing_local_typescript_compiler_never_uses_a_global_candidate() {
    let fixture = temporary_fixture(&[("tsconfig.json", "{}"), ("index.ts", "export {};\n")]);
    let runner = recording_runner(fixture.path().to_path_buf());
    let resolution = preflight_with(&fixture, ProviderOptions::default(), runner.clone());
    let typescript = unavailable_status_with_reason(&resolution, "typescript", "local");
    assert!(typescript.executable.is_none());
    assert_program_not_called(&runner, "tsc");
}

#[test]
fn failing_typescript_project_resolution_is_not_reported_available() {
    let (fixture, tsc) = typescript_fixture("invalid", &[("index.ts", "export {};\n")]);
    let resolution = preflight_with(
        &fixture,
        ProviderOptions {
            typescript_tsc: Some(tsc),
            ..ProviderOptions::default()
        },
        failing_runner(FailureFixture::TypeScript),
    );
    unavailable_status_with_reason(&resolution, "typescript", "could not resolve");
    assert!(!resolution
        .context
        .backends
        .iter()
        .any(|backend| backend.id == "project-typescript"));
    assert_diagnostic_contains(
        &resolution,
        "project-typescript-preflight",
        &["invalid tsconfig fixture"],
    );
}

#[test]
fn truncated_typescript_file_listing_is_rejected_without_pruning_sources() {
    let (fixture, tsc) = typescript_fixture(
        "{}",
        &[
            ("index.ts", "export const first = 1;\n"),
            ("extra.ts", "export const second = 2;\n"),
        ],
    );
    let resolution = preflight_with(
        &fixture,
        ProviderOptions {
            typescript_tsc: Some(tsc),
            ..ProviderOptions::default()
        },
        TruncatedTypeScriptRunner,
    );
    assert!(!status(&resolution, "typescript").available);
    assert_eq!(
        sources_for(&resolution, Language::TypeScript),
        vec!["extra.ts", "index.ts"]
    );
    assert_diagnostic_contains(
        &resolution,
        "project-typescript-preflight",
        &["output exceeded", "incomplete"],
    );
}

#[test]
fn swift_preflight_failure_explains_locked_offline_requirements() {
    let fixture = temporary_fixture(&[("Package.swift", "// swift-tools-version: 6.0\n")]);
    let swift = fixture_executable(fixture.path(), "tools/swift");
    let resolution = preflight_with(
        &fixture,
        ProviderOptions {
            swift: Some(swift),
            ..ProviderOptions::default()
        },
        failing_runner(FailureFixture::Swift),
    );
    let swiftpm = unavailable_status_with_reason(
        &resolution,
        "swiftpm",
        "without dependency resolution or network access",
    );
    assert!(swiftpm
        .hint
        .as_deref()
        .is_some_and(|hint| hint.contains("Package.resolved") && hint.contains("cache")));
    assert_diagnostic_contains(
        &resolution,
        "project-swiftpm-preflight",
        &["automatic resolution is disabled"],
    );
}

#[test]
fn missing_shellcheck_does_not_disable_builtin_bash_provider() {
    let fixture = temporary_fixture(&[("script.sh", "#!/bin/sh\necho ok\n")]);
    let bash = fixture_executable(fixture.path(), "bash");
    let runner = recording_runner(fixture.path().to_path_buf());
    let resolution = preflight_with(
        &fixture,
        ProviderOptions {
            bash: Some(bash),
            shellcheck: Some(fixture.path().join("missing-shellcheck")),
            ..ProviderOptions::default()
        },
        runner.clone(),
    );
    let bash = status(&resolution, "bash");
    let shellcheck = status(&resolution, "shellcheck");
    assert!(bash.available);
    assert!(!shellcheck.available);
    assert!(resolution
        .context
        .backends
        .iter()
        .any(|backend| backend.id == "project-bash"));
    let shellcheck_diagnostic = diagnostic(&resolution, "project-shellcheck");
    assert!(!shellcheck_diagnostic.fallback_used);
    assert_program_not_called(&runner, "missing-shellcheck");
}

#[test]
fn python_inventory_prefers_project_virtual_environment() {
    let fixture = temporary_fixture(&[("pyproject.toml", "[project]\nname='demo'\n")]);
    let interpreter = fixture_executable(fixture.path(), ".venv/bin/python");
    let status = providers(fixture.path())
        .into_iter()
        .find(|status| status.id == "python")
        .unwrap_or_else(|| panic!("python status"));
    assert!(status.available);
    let interpreter = interpreter
        .canonicalize()
        .unwrap_or_else(|error| panic!("canonical interpreter: {error}"));
    assert_eq!(
        status
            .executable
            .as_deref()
            .and_then(|path| path.canonicalize().ok())
            .as_deref(),
        Some(interpreter.as_path())
    );
}

fn assert_preflight_commands(root: &PathBuf, calls: &[ProviderCommand]) {
    assert_eq!(calls.len(), 8);
    assert!(calls.iter().all(|command| command.cwd == *root));
    assert!(calls.iter().all(|command| {
        let text = command.program.to_string_lossy().to_ascii_lowercase();
        !text.contains("npx") && !text.contains("npm") && !text.contains("pnpm")
    }));
    let typescript_calls = calls
        .iter()
        .filter(|command| command.program.file_name() == Some(OsStr::new("tsc")))
        .collect::<Vec<_>>();
    assert_eq!(typescript_calls.len(), 3);
    assert!(typescript_calls
        .iter()
        .all(|command| command.program == root.join("node_modules/.bin/tsc")));
    assert!(typescript_calls.iter().any(|command| {
        command.args.first() == Some(&"--showConfig".into()) && command.args.get(1) == Some(&"-p".into())
    }));
    assert!(typescript_calls.iter().any(|command| {
        command.args.first() == Some(&"--listFilesOnly".into()) && command.args.get(1) == Some(&"-p".into())
    }));
    let swift_describe = calls
        .iter()
        .find(|command| {
            command.program.file_name() == Some(OsStr::new("swift"))
                && command.args.first() == Some(&"package".into())
        })
        .unwrap_or_else(|| panic!("Swift package describe command"));
    assert_eq!(
        swift_describe.args,
        swift_description_arguments()
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>()
    );
}

fn version<'a>(inventory: &'a [adapter_project::ProviderStatus], id: &str) -> Option<&'a str> {
    inventory
        .iter()
        .find(|status| status.id == id)
        .and_then(|status| status.version.as_deref())
}

fn fixture() -> TempDir {
    let temp = temporary_fixture(&[
        (
            "package.json",
            r#"{"name":"demo","devDependencies":{"typescript":"^7.0.2"}}"#,
        ),
        ("tsconfig.json", r#"{"include":["src/index.ts"]}"#),
        ("src/index.ts", "export const value = 1;\n"),
        ("src/ignored.ts", "export const ignored = 1;\n"),
        ("Package.swift", "// swift-tools-version: 6.0\n"),
        (
            "pyproject.toml",
            "[project]\nname = \"demo\"\nrequires-python = \">=3.11\"\n",
        ),
        ("app.py", "value = 1\n"),
        (
            "setup.py",
            "from pathlib import Path\nPath('setup-was-executed').write_text('bad')\n",
        ),
        ("tool.sh", "#!/usr/bin/env -S bash -eu\necho ok\n"),
    ]);
    for tool in [
        "node_modules/.bin/tsc",
        "tools/swift",
        "tools/python",
        "tools/bash",
        "tools/shellcheck",
    ] {
        fixture_executable(temp.path(), tool);
    }
    temp
}
