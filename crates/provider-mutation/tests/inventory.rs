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
fn static_discovery_finds_project_local_tools_without_running_them() -> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    std::fs::write(project.path().join("package.json"), "{}\n")?;
    let executable = project.path().join("node_modules/.bin/stryker");
    write_executable(&executable, "#!/bin/sh\nexit 99\n")?;

    let inventory = discover(project.path())?;
    let built_in = inventory
        .status(MutationProvider::BuiltIn)
        .ok_or("missing built-in provider")?;
    assert!(built_in.default);
    assert!(built_in.available);
    assert!(built_in.execution_enabled);

    let stryker = inventory
        .status(MutationProvider::Stryker)
        .ok_or("missing Stryker provider")?;
    assert!(stryker.applicable);
    assert!(stryker.available);
    assert!(!stryker.execution_enabled);
    let canonical_executable = std::fs::canonicalize(&executable)?;
    assert_eq!(
        stryker.executable.as_deref(),
        Some(canonical_executable.as_path())
    );
    assert_eq!(stryker.detection, Some(DetectionSource::ProjectLocal));
    assert_eq!(stryker.version, None);
    assert!(inventory.root.is_absolute());
    assert_eq!(inventory.root, std::fs::canonicalize(project.path())?);
    Ok(())
}

#[test]
fn invalid_override_is_authoritative_and_never_falls_back() -> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    std::fs::write(project.path().join("Cargo.toml"), "[package]\nname='demo'\n")?;
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
fn preflight_runs_only_a_bounded_version_probe() -> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    std::fs::write(project.path().join("pyproject.toml"), "[project]\nname='demo'\n")?;
    let executable = project.path().join("fake-mutmut");
    write_executable(&executable, "ignored")?;
    let options = MutationProviderOptions {
        executables: BTreeMap::from([(MutationProvider::Mutmut, executable.clone())]),
        probe_timeout: Duration::from_secs(2),
        output_limit_bytes: 4096,
    };
    let runner = RecordingRunner::default();

    let inventory = preflight_with_runner(project.path(), &options, &runner)?;
    let status = inventory
        .status(MutationProvider::Mutmut)
        .ok_or("missing mutmut provider")?;
    assert_eq!(status.version.as_deref(), Some("mutmut 3.2.1"));
    let commands = runner.commands.into_inner()?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].program, std::fs::canonicalize(executable)?);
    assert_eq!(commands[0].args, ["--version"]);
    assert_eq!(commands[0].effect, CommandEffect::ReadOnlyProbe);
    assert_eq!(commands[0].timeout, Duration::from_secs(2));
    assert_eq!(commands[0].output_limit_bytes, 4096);
    Ok(())
}

#[test]
fn cargo_mutants_direct_probe_includes_the_required_subcommand() -> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    std::fs::write(project.path().join("Cargo.toml"), "[package]\nname='demo'\n")?;
    let executable = project.path().join("cargo-mutants");
    write_executable(&executable, "ignored")?;
    let options = MutationProviderOptions {
        executables: BTreeMap::from([(MutationProvider::CargoMutants, executable)]),
        ..MutationProviderOptions::default()
    };
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
fn project_local_symlink_escape_requires_an_explicit_override() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir()?;
    let external = tempfile::tempdir()?;
    std::fs::write(project.path().join("package.json"), "{}\n")?;
    let outside_executable = external.path().join("stryker");
    write_executable(&outside_executable, "#!/bin/sh\nexit 0\n")?;
    let project_candidate = project.path().join("node_modules/.bin/stryker");
    std::fs::create_dir_all(project_candidate.parent().ok_or("missing parent")?)?;
    symlink(&outside_executable, &project_candidate)?;

    let inventory = discover(project.path())?;
    let status = inventory
        .status(MutationProvider::Stryker)
        .ok_or("missing Stryker provider")?;
    assert!(!status.available);
    assert_eq!(status.executable, None);

    let options = MutationProviderOptions {
        executables: BTreeMap::from([(MutationProvider::Stryker, project_candidate.clone())]),
        ..MutationProviderOptions::default()
    };
    let inventory = discover_with_options(project.path(), &options)?;
    let status = inventory
        .status(MutationProvider::Stryker)
        .ok_or("missing Stryker provider")?;
    assert!(status.available);
    assert_eq!(status.detection, Some(DetectionSource::ExplicitOverride));
    assert_eq!(
        status.executable.as_deref(),
        Some(std::fs::canonicalize(outside_executable)?.as_path())
    );
    Ok(())
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

fn write_executable(path: &Path, contents: &str) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
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
