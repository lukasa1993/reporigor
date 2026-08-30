use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use provider_mutation::{
    discover, discover_with_options, preflight_with_runner, BoundedCommand, CommandEffect, CommandOutput,
    CommandRunner, DetectionSource, MutationProvider, MutationProviderOptions, ProviderError,
    SystemCommandRunner,
};

#[test]
fn static_discovery_finds_project_local_tools_without_running_them() {
    let (project, executable) = executable_project(
        "package.json",
        "{}\n",
        "node_modules/.bin/stryker",
        "#!/bin/sh\nexit 99\n",
    );

    let inventory = must(discover(project.path()));
    let built_in = present(inventory.status(MutationProvider::BuiltIn));
    assert!(built_in.default);
    assert!(built_in.available);
    assert!(built_in.execution_enabled);

    let stryker = present(inventory.status(MutationProvider::Stryker));
    assert!(stryker.applicable);
    assert!(stryker.available);
    assert!(!stryker.execution_enabled);
    let canonical_executable = must(std::fs::canonicalize(&executable));
    assert_eq!(
        stryker.executable.as_deref(),
        Some(canonical_executable.as_path())
    );
    assert_eq!(stryker.detection, Some(DetectionSource::ProjectLocal));
    assert_eq!(stryker.version, None);
    assert!(inventory.root.is_absolute());
    assert_eq!(inventory.root, must(std::fs::canonicalize(project.path())));
}

#[test]
fn invalid_override_is_authoritative_and_never_falls_back() -> Result<(), Box<dyn Error>> {
    let project = project_fixture("Cargo.toml", "[package]\nname='demo'\n");
    let missing = project.path().join("missing-cargo-mutants");
    let options = MutationProviderOptions {
        executables: BTreeMap::from([(MutationProvider::CargoMutants, missing.clone())]),
        ..MutationProviderOptions::default()
    };

    let inventory = discover_with_options(project.path(), &options)?;
    let status = inventory
        .status(MutationProvider::CargoMutants)
        .ok_or("missing cargo-mutants provider")?;
    assert!(status.applicable);
    assert!(!status.available);
    assert_eq!(status.executable.as_deref(), Some(missing.as_path()));
    assert_eq!(status.detection, Some(DetectionSource::ExplicitOverride));
    Ok(())
}

#[test]
fn preflight_runs_only_a_bounded_version_probe() {
    let (project, executable) = executable_project(
        "pyproject.toml",
        "[project]\nname='demo'\n",
        "fake-mutmut",
        "ignored",
    );
    let mut options = options_with_executable(MutationProvider::Mutmut, executable.clone());
    options.probe_timeout = Duration::from_secs(2);
    options.output_limit_bytes = 4096;
    let runner = RecordingRunner::default();

    let inventory = must(preflight_with_runner(project.path(), &options, &runner));
    let status = present(inventory.status(MutationProvider::Mutmut));
    assert_eq!(status.version.as_deref(), Some("mutmut 3.2.1"));
    let commands = must(runner.commands.into_inner());
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].program, must(std::fs::canonicalize(executable)));
    assert_eq!(commands[0].args, ["--version"]);
    assert_eq!(commands[0].effect, CommandEffect::ReadOnlyProbe);
    assert_eq!(commands[0].timeout, Duration::from_secs(2));
    assert_eq!(commands[0].output_limit_bytes, 4096);
}

#[test]
fn cargo_mutants_direct_probe_includes_the_required_subcommand() -> Result<(), Box<dyn Error>> {
    let (project, executable) = cargo_mutants_project();
    let options = options_with_executable(MutationProvider::CargoMutants, executable);
    let runner = RecordingRunner::default();
    let _inventory = preflight_with_runner(project.path(), &options, &runner)?;
    let commands = runner.commands.into_inner()?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].args, ["mutants", "--version"]);
    Ok(())
}

#[test]
fn system_runner_refuses_effectful_provider_commands_before_spawn() {
    let command = BoundedCommand {
        program: "/definitely/not/a/program".into(),
        args: Vec::new(),
        cwd: ".".into(),
        timeout: Duration::from_secs(1),
        output_limit_bytes: 100,
        effect: CommandEffect::MutationRun,
    };
    let error = SystemCommandRunner.run(&command);
    assert!(matches!(error, Err(ProviderError::EffectfulCommand(_))));
}

#[cfg(unix)]
#[test]
fn project_local_symlink_escape_requires_an_explicit_override() {
    use std::os::unix::fs::symlink;

    let project = project_fixture("package.json", "{}\n");
    let external = must(tempfile::tempdir());
    let outside_executable = external.path().join("stryker");
    write_executable(&outside_executable, "#!/bin/sh\nexit 0\n");
    let project_candidate = project.path().join("node_modules/.bin/stryker");
    let parent = present(project_candidate.parent());
    must(std::fs::create_dir_all(parent));
    must(symlink(&outside_executable, &project_candidate));

    let inventory = must(discover(project.path()));
    let status = present(inventory.status(MutationProvider::Stryker));
    assert!(!status.available);
    assert_eq!(status.executable, None);

    let options = MutationProviderOptions {
        executables: BTreeMap::from([(MutationProvider::Stryker, project_candidate.clone())]),
        ..MutationProviderOptions::default()
    };
    let inventory = must(discover_with_options(project.path(), &options));
    let status = present(inventory.status(MutationProvider::Stryker));
    assert!(status.available);
    assert_eq!(status.detection, Some(DetectionSource::ExplicitOverride));
    assert_eq!(
        status.executable.as_deref(),
        Some(must(std::fs::canonicalize(outside_executable)).as_path())
    );
}

#[derive(Debug, Default)]
struct RecordingRunner {
    commands: Mutex<Vec<BoundedCommand>>,
}

impl CommandRunner for RecordingRunner {
    fn run(&self, command: &BoundedCommand) -> Result<CommandOutput, ProviderError> {
        self.commands
            .lock()
            .map_err(|error| ProviderError::CommandOutput {
                program: command.program.clone(),
                message: error.to_string(),
            })?
            .push(command.clone());
        Ok(CommandOutput {
            exit_code: Some(0),
            stdout: "mutmut 3.2.1\n".to_owned(),
            stderr: String::new(),
            output_truncated: false,
        })
    }
}

fn write_executable(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        must(std::fs::create_dir_all(parent));
    }
    must(std::fs::write(path, contents));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        must(std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(0o755),
        ));
    }
}

fn project_fixture(manifest: &str, contents: &str) -> tempfile::TempDir {
    let project = must(tempfile::tempdir());
    must(std::fs::write(project.path().join(manifest), contents));
    project
}

fn executable_project(
    manifest: &str,
    manifest_contents: &str,
    executable_name: &str,
    executable_contents: &str,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let project = project_fixture(manifest, manifest_contents);
    let executable = project.path().join(executable_name);
    write_executable(&executable, executable_contents);
    (project, executable)
}

fn cargo_mutants_project() -> (tempfile::TempDir, std::path::PathBuf) {
    executable_project(
        "Cargo.toml",
        "[package]\nname='demo'\n",
        "cargo-mutants",
        "ignored",
    )
}

fn options_with_executable(
    provider: MutationProvider,
    executable: std::path::PathBuf,
) -> MutationProviderOptions {
    MutationProviderOptions {
        executables: BTreeMap::from([(provider, executable)]),
        ..MutationProviderOptions::default()
    }
}

fn must<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|error| panic!("test operation failed: {error:?}"))
}

fn present<T>(value: Option<T>) -> T {
    value.unwrap_or_else(|| panic!("expected test value"))
}
