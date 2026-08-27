use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use adapter_project::{
    providers, CommandRunner, ProjectAdapter, ProviderCommand, ProviderCommandOutput, ProviderOptions,
};
use reporigor_core::{AnalysisRequest, Capability, CoreError, Language};
use tempfile::TempDir;

#[derive(Debug, Clone)]
struct RecordingRunner {
    root: PathBuf,
    calls: Arc<Mutex<Vec<ProviderCommand>>>,
}

#[derive(Debug)]
struct FailingTypeScriptRunner;

#[derive(Debug)]
struct FailingSwiftRunner;

#[derive(Debug)]
struct TruncatedTypeScriptRunner;

impl CommandRunner for FailingTypeScriptRunner {
    fn run(&self, command: &ProviderCommand) -> Result<ProviderCommandOutput, CoreError> {
        let version = command.args.first() == Some(&"--version".into());
        Ok(ProviderCommandOutput {
            exit_code: Some(if version { 0 } else { 2 }),
            stdout: if version {
                "Version 7.0.2\n".to_string()
            } else {
                String::new()
            },
            stderr: if version {
                String::new()
            } else {
                "invalid tsconfig fixture".to_string()
            },
            output_truncated: false,
        })
    }
}

impl CommandRunner for FailingSwiftRunner {
    fn run(&self, command: &ProviderCommand) -> Result<ProviderCommandOutput, CoreError> {
        let version = command.args.first() == Some(&"--version".into());
        Ok(ProviderCommandOutput {
            exit_code: Some(i32::from(!version)),
            stdout: if version {
                "Swift version 6.3.3\n".to_string()
            } else {
                String::new()
            },
            stderr: if version {
                String::new()
            } else {
                "Package.resolved is out of date and automatic resolution is disabled".to_string()
            },
            output_truncated: false,
        })
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

impl RecordingRunner {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> Vec<ProviderCommand> {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls lock: {error}"))
            .clone()
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, command: &ProviderCommand) -> Result<ProviderCommandOutput, CoreError> {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls lock: {error}"))
            .push(command.clone());
        let name = command
            .program
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default();
        let args = command
            .args
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        let stdout = match name {
            "tsc" if args.iter().any(|argument| argument == "--showConfig") => {
                r#"{"compilerOptions":{"strict":true},"files":["src/index.ts"],"futureField":true}"#
                    .to_string()
            }
            "tsc" if args.iter().any(|argument| argument == "--listFilesOnly") => format!(
                "/outside/lib.es2025.d.ts\r\n{}\r\n",
                self.root.join("src/index.ts").display()
            ),
            "tsc" => "Version 7.0.2\n".to_string(),
            "swift"
                if args
                    == [
                        "package",
                        "--disable-automatic-resolution",
                        "--skip-update",
                        "--disable-netrc",
                        "describe",
                        "--type",
                        "json",
                    ] =>
            {
                r#"{"name":"Demo","tools_version":"6.0","targets":[{"name":"Demo"},{"name":"DemoTests"}],"new_field":42}"#.to_string()
            }
            "swift" => "Swift version 6.3.3\n".to_string(),
            "python" => "Python 3.14.0\n".to_string(),
            "bash" => "GNU bash, version 5.2.0\n".to_string(),
            "shellcheck" => "ShellCheck - shell script analysis tool\nversion: 0.10.0\n"
                .to_string(),
            _ => String::new(),
        };
        Ok(ProviderCommandOutput {
            exit_code: Some(0),
            stdout,
            stderr: String::new(),
            output_truncated: false,
        })
    }
}

#[test]
fn explicit_preflight_uses_native_cli_contracts_and_records_provenance() {
    let fixture = fixture();
    let root = fixture
        .path()
        .canonicalize()
        .unwrap_or_else(|error| panic!("root: {error}"));
    let runner = RecordingRunner::new(root.clone());
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

    assert_eq!(
        version(&resolution.inventory, "typescript"),
        Some("Version 7.0.2")
    );
    assert_eq!(
        version(&resolution.inventory, "swiftpm"),
        Some("Swift version 6.3.3")
    );
    assert_eq!(version(&resolution.inventory, "python"), Some("Python 3.14.0"));
    assert_eq!(version(&resolution.inventory, "shellcheck"), Some("0.10.0"));
    assert_eq!(resolution.context.backends.len(), 5);
    for id in ["python", "shellcheck"] {
        let status = resolution
            .inventory
            .iter()
            .find(|status| status.id == id)
            .unwrap_or_else(|| panic!("{id} status"));
        assert!(!status.capabilities.contains(Capability::ParseValidation));
    }

    let configured_ts = resolution
        .context
        .sources
        .iter()
        .filter(|source| source.language == Language::TypeScript)
        .map(|source| source.relative.as_str())
        .collect::<Vec<_>>();
    assert_eq!(configured_ts, vec!["src/index.ts"]);

    let typescript = resolution
        .provenance
        .iter()
        .find(|item| item.id == "typescript")
        .unwrap_or_else(|| panic!("typescript provenance"));
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
    let swift = resolution
        .provenance
        .iter()
        .find(|item| item.id == "swiftpm")
        .unwrap_or_else(|| panic!("Swift provenance"));
    assert_eq!(swift.metadata.get("target_count").map(String::as_str), Some("2"));
    let python = resolution
        .provenance
        .iter()
        .find(|item| item.id == "python")
        .unwrap_or_else(|| panic!("Python provenance"));
    assert!(python.metadata.contains_key("setup_script"));
    assert!(!root.join("setup-was-executed").exists());
    assert!(resolution
        .context
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message.contains("TypeScript 7")));

    assert_preflight_commands(&root, &runner.calls());
}

#[test]
fn missing_local_typescript_compiler_never_uses_a_global_candidate() {
    let fixture = TempDir::new().unwrap_or_else(|error| panic!("fixture: {error}"));
    fs::write(fixture.path().join("tsconfig.json"), "{}").unwrap_or_else(|error| panic!("tsconfig: {error}"));
    fs::write(fixture.path().join("index.ts"), "export {};\n")
        .unwrap_or_else(|error| panic!("source: {error}"));
    let runner = RecordingRunner::new(fixture.path().to_path_buf());
    let adapter = ProjectAdapter::with_runner(ProviderOptions::default(), runner.clone());
    let resolution = adapter
        .preflight(&AnalysisRequest::new(fixture.path().to_path_buf()))
        .unwrap_or_else(|error| panic!("preflight: {error}"));
    let status = resolution
        .inventory
        .iter()
        .find(|status| status.id == "typescript")
        .unwrap_or_else(|| panic!("typescript status"));
    assert!(!status.available);
    assert!(status.executable.is_none());
    assert!(status
        .reason
        .as_deref()
        .is_some_and(|reason| reason.contains("local")));
    assert!(runner
        .calls()
        .iter()
        .all(|command| command.program.file_name() != Some(OsStr::new("tsc"))));
}

#[test]
fn failing_typescript_project_resolution_is_not_reported_available() {
    let fixture = TempDir::new().unwrap_or_else(|error| panic!("fixture: {error}"));
    let tsc = fixture.path().join("node_modules/.bin/tsc");
    fs::create_dir_all(tsc.parent().unwrap_or_else(|| panic!("tsc parent")))
        .unwrap_or_else(|error| panic!("node modules: {error}"));
    fs::write(&tsc, "fixture executable").unwrap_or_else(|error| panic!("tsc: {error}"));
    make_executable(&tsc);
    fs::write(fixture.path().join("tsconfig.json"), "invalid")
        .unwrap_or_else(|error| panic!("tsconfig: {error}"));
    fs::write(fixture.path().join("index.ts"), "export {};\n")
        .unwrap_or_else(|error| panic!("source: {error}"));
    let adapter = ProjectAdapter::with_runner(
        ProviderOptions {
            typescript_tsc: Some(tsc),
            ..ProviderOptions::default()
        },
        FailingTypeScriptRunner,
    );
    let resolution = adapter
        .preflight(&AnalysisRequest::new(fixture.path().to_path_buf()))
        .unwrap_or_else(|error| panic!("preflight: {error}"));
    let status = resolution
        .inventory
        .iter()
        .find(|status| status.id == "typescript")
        .unwrap_or_else(|| panic!("typescript status"));
    assert!(!status.available);
    assert!(status
        .reason
        .as_deref()
        .is_some_and(|reason| reason.contains("could not resolve")));
    assert!(!resolution
        .context
        .backends
        .iter()
        .any(|backend| backend.id == "project-typescript"));
    assert!(resolution
        .context
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message.contains("invalid tsconfig fixture")));
}

#[test]
fn truncated_typescript_file_listing_is_rejected_without_pruning_sources() {
    let fixture = TempDir::new().unwrap_or_else(|error| panic!("fixture: {error}"));
    let tsc = fixture.path().join("node_modules/.bin/tsc");
    fs::create_dir_all(tsc.parent().unwrap_or_else(|| panic!("tsc parent")))
        .unwrap_or_else(|error| panic!("node modules: {error}"));
    fs::write(&tsc, "fixture executable").unwrap_or_else(|error| panic!("tsc: {error}"));
    make_executable(&tsc);
    fs::write(fixture.path().join("tsconfig.json"), "{}").unwrap_or_else(|error| panic!("tsconfig: {error}"));
    fs::write(fixture.path().join("index.ts"), "export const first = 1;\n")
        .unwrap_or_else(|error| panic!("index source: {error}"));
    fs::write(fixture.path().join("extra.ts"), "export const second = 2;\n")
        .unwrap_or_else(|error| panic!("extra source: {error}"));
    let adapter = ProjectAdapter::with_runner(
        ProviderOptions {
            typescript_tsc: Some(tsc),
            ..ProviderOptions::default()
        },
        TruncatedTypeScriptRunner,
    );

    let resolution = adapter
        .preflight(&AnalysisRequest::new(fixture.path().to_path_buf()))
        .unwrap_or_else(|error| panic!("preflight: {error}"));

    let status = resolution
        .inventory
        .iter()
        .find(|status| status.id == "typescript")
        .unwrap_or_else(|| panic!("typescript status"));
    assert!(!status.available);
    let sources = resolution
        .context
        .sources
        .iter()
        .filter(|source| source.language == Language::TypeScript)
        .map(|source| source.relative.as_str())
        .collect::<Vec<_>>();
    assert_eq!(sources, vec!["extra.ts", "index.ts"]);
    assert!(resolution.context.diagnostics.iter().any(|diagnostic| {
        diagnostic.backend == "project-typescript-preflight"
            && diagnostic.message.contains("output exceeded")
            && diagnostic.message.contains("incomplete")
    }));
}

#[test]
fn swift_preflight_failure_explains_locked_offline_requirements() {
    let fixture = TempDir::new().unwrap_or_else(|error| panic!("fixture: {error}"));
    let swift = fixture.path().join("tools/swift");
    fs::create_dir_all(swift.parent().unwrap_or_else(|| panic!("Swift parent")))
        .unwrap_or_else(|error| panic!("tools: {error}"));
    fs::write(&swift, "fixture executable").unwrap_or_else(|error| panic!("Swift: {error}"));
    make_executable(&swift);
    fs::write(
        fixture.path().join("Package.swift"),
        "// swift-tools-version: 6.0\n",
    )
    .unwrap_or_else(|error| panic!("manifest: {error}"));
    let adapter = ProjectAdapter::with_runner(
        ProviderOptions {
            swift: Some(swift),
            ..ProviderOptions::default()
        },
        FailingSwiftRunner,
    );
    let resolution = adapter
        .preflight(&AnalysisRequest::new(fixture.path().to_path_buf()))
        .unwrap_or_else(|error| panic!("preflight: {error}"));
    let status = resolution
        .inventory
        .iter()
        .find(|status| status.id == "swiftpm")
        .unwrap_or_else(|| panic!("Swift status"));
    assert!(!status.available);
    assert!(status
        .reason
        .as_deref()
        .is_some_and(|reason| reason.contains("without dependency resolution or network access")));
    assert!(status
        .hint
        .as_deref()
        .is_some_and(|hint| hint.contains("Package.resolved") && hint.contains("cache")));
    assert!(resolution.context.diagnostics.iter().any(|diagnostic| {
        diagnostic.backend == "project-swiftpm-preflight"
            && diagnostic.message.contains("automatic resolution is disabled")
    }));
}

#[test]
fn missing_shellcheck_does_not_disable_builtin_bash_provider() {
    let fixture = TempDir::new().unwrap_or_else(|error| panic!("fixture: {error}"));
    fs::write(fixture.path().join("script.sh"), "#!/bin/sh\necho ok\n")
        .unwrap_or_else(|error| panic!("source: {error}"));
    let bash = fixture.path().join("bash");
    fs::write(&bash, "fixture executable").unwrap_or_else(|error| panic!("bash: {error}"));
    make_executable(&bash);
    let runner = RecordingRunner::new(fixture.path().to_path_buf());
    let adapter = ProjectAdapter::with_runner(
        ProviderOptions {
            bash: Some(bash),
            shellcheck: Some(fixture.path().join("missing-shellcheck")),
            ..ProviderOptions::default()
        },
        runner.clone(),
    );
    let resolution = adapter
        .preflight(&AnalysisRequest::new(fixture.path().to_path_buf()))
        .unwrap_or_else(|error| panic!("preflight: {error}"));
    let bash = resolution
        .inventory
        .iter()
        .find(|status| status.id == "bash")
        .unwrap_or_else(|| panic!("bash status"));
    let shellcheck = resolution
        .inventory
        .iter()
        .find(|status| status.id == "shellcheck")
        .unwrap_or_else(|| panic!("shellcheck status"));
    assert!(bash.available);
    assert!(!shellcheck.available);
    assert!(resolution
        .context
        .backends
        .iter()
        .any(|backend| backend.id == "project-bash"));
    let shellcheck_diagnostic = resolution
        .context
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.backend == "project-shellcheck")
        .unwrap_or_else(|| panic!("ShellCheck diagnostic"));
    assert!(!shellcheck_diagnostic.fallback_used);
    assert!(runner
        .calls()
        .iter()
        .all(|command| command.program.file_name() != Some(OsStr::new("missing-shellcheck"))));
}

#[test]
fn python_inventory_prefers_project_virtual_environment() {
    let fixture = TempDir::new().unwrap_or_else(|error| panic!("fixture: {error}"));
    let interpreter = fixture.path().join(".venv/bin/python");
    fs::create_dir_all(
        interpreter
            .parent()
            .unwrap_or_else(|| panic!("interpreter parent")),
    )
    .unwrap_or_else(|error| panic!("virtual environment: {error}"));
    fs::write(&interpreter, "fixture executable").unwrap_or_else(|error| panic!("interpreter: {error}"));
    make_executable(&interpreter);
    fs::write(fixture.path().join("pyproject.toml"), "[project]\nname='demo'\n")
        .unwrap_or_else(|error| panic!("pyproject: {error}"));
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
        [
            "package",
            "--disable-automatic-resolution",
            "--skip-update",
            "--disable-netrc",
            "describe",
            "--type",
            "json",
        ]
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

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path)
            .unwrap_or_else(|error| panic!("metadata for {}: {error}", path.display()))
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)
            .unwrap_or_else(|error| panic!("permissions for {}: {error}", path.display()));
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn fixture() -> TempDir {
    let temp = TempDir::new().unwrap_or_else(|error| panic!("fixture: {error}"));
    for directory in ["node_modules/.bin", "tools", "src"] {
        fs::create_dir_all(temp.path().join(directory))
            .unwrap_or_else(|error| panic!("create {directory}: {error}"));
    }
    for tool in [
        "node_modules/.bin/tsc",
        "tools/swift",
        "tools/python",
        "tools/bash",
        "tools/shellcheck",
    ] {
        let path = temp.path().join(tool);
        fs::write(&path, "fixture executable").unwrap_or_else(|error| panic!("write {tool}: {error}"));
        make_executable(&path);
    }
    fs::write(
        temp.path().join("package.json"),
        r#"{"name":"demo","devDependencies":{"typescript":"^7.0.2"}}"#,
    )
    .unwrap_or_else(|error| panic!("package: {error}"));
    fs::write(
        temp.path().join("tsconfig.json"),
        r#"{"include":["src/index.ts"]}"#,
    )
    .unwrap_or_else(|error| panic!("tsconfig: {error}"));
    fs::write(temp.path().join("src/index.ts"), "export const value = 1;\n")
        .unwrap_or_else(|error| panic!("included source: {error}"));
    fs::write(temp.path().join("src/ignored.ts"), "export const ignored = 1;\n")
        .unwrap_or_else(|error| panic!("ignored source: {error}"));
    fs::write(temp.path().join("Package.swift"), "// swift-tools-version: 6.0\n")
        .unwrap_or_else(|error| panic!("Swift manifest: {error}"));
    fs::write(
        temp.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\nrequires-python = \">=3.11\"\n",
    )
    .unwrap_or_else(|error| panic!("pyproject: {error}"));
    fs::write(temp.path().join("app.py"), "value = 1\n")
        .unwrap_or_else(|error| panic!("Python source: {error}"));
    fs::write(
        temp.path().join("setup.py"),
        "from pathlib import Path\nPath('setup-was-executed').write_text('bad')\n",
    )
    .unwrap_or_else(|error| panic!("setup.py: {error}"));
    fs::write(
        temp.path().join("tool.sh"),
        "#!/usr/bin/env -S bash -eu\necho ok\n",
    )
    .unwrap_or_else(|error| panic!("Bash source: {error}"));
    temp
}
