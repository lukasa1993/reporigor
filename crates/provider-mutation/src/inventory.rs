use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{
    BoundedCommand, CommandEffect, CommandRunner, DetectionSource, ImportFormat, MutationProvider,
    MutationProviderOptions, MutationProviderStatus, ProviderError, ProviderInventory, SystemCommandRunner,
};

/// Discover mutation providers without executing subprocesses.
///
/// # Errors
///
/// Returns an error when `root` is not a directory.
pub fn discover(root: &Path) -> Result<ProviderInventory, ProviderError> {
    discover_with_options(root, &MutationProviderOptions::default())
}

/// Discover mutation providers using explicit executable overrides.
///
/// Overrides are authoritative: an invalid override is reported as
/// unavailable and does not silently fall back to `PATH`.
///
/// # Errors
///
/// Returns an error when `root` is not a directory.
pub fn discover_with_options(
    root: &Path,
    options: &MutationProviderOptions,
) -> Result<ProviderInventory, ProviderError> {
    if !root.is_dir() {
        return Err(ProviderError::InvalidRoot(root.to_path_buf()));
    }
    let root = fs::canonicalize(root).map_err(|source| ProviderError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    let mut providers = Vec::with_capacity(MutationProvider::ALL.len());
    for provider in MutationProvider::ALL {
        providers.push(status(&root, provider, options));
    }
    Ok(ProviderInventory { root, providers })
}

/// Run bounded, read-only version probes after static discovery.
///
/// # Errors
///
/// Returns an error only when the root is invalid. Individual optional probe
/// failures are represented on their provider status.
pub fn preflight(root: &Path) -> Result<ProviderInventory, ProviderError> {
    preflight_with_options(root, &MutationProviderOptions::default())
}

/// Preflight variant using explicit options and the system command runner.
///
/// # Errors
///
/// Returns an error only when the root is invalid.
pub fn preflight_with_options(
    root: &Path,
    options: &MutationProviderOptions,
) -> Result<ProviderInventory, ProviderError> {
    preflight_with_runner(root, options, &SystemCommandRunner)
}

/// Preflight variant with an injected runner for deterministic tests.
///
/// # Errors
///
/// Returns an error only when the root is invalid.
pub fn preflight_with_runner<R: CommandRunner>(
    root: &Path,
    options: &MutationProviderOptions,
    runner: &R,
) -> Result<ProviderInventory, ProviderError> {
    let mut inventory = discover_with_options(root, options)?;
    for status in &mut inventory.providers {
        if status.id == MutationProvider::BuiltIn || !status.applicable || !status.available {
            continue;
        }
        let Some(program) = status.executable.clone() else {
            continue;
        };
        let command = version_command(
            status.id,
            program,
            inventory.root.clone(),
            options.probe_timeout,
            options.output_limit_bytes,
        );
        match runner.run(&command) {
            Ok(output) if output.success() && !output.output_truncated => {
                let version =
                    first_nonempty_line(&output.stdout).or_else(|| first_nonempty_line(&output.stderr));
                if let Some(version) = version {
                    status.version = Some(version);
                } else {
                    mark_probe_failed(status, "version probe returned no version text");
                }
            }
            Ok(output) if output.output_truncated => {
                mark_probe_failed(status, "version probe output exceeded its configured limit");
            }
            Ok(output) => {
                let detail = first_nonempty_line(&output.stderr)
                    .or_else(|| first_nonempty_line(&output.stdout))
                    .unwrap_or_else(|| format!("exit status {:?}", output.exit_code));
                mark_probe_failed(status, &detail);
            }
            Err(error) => mark_probe_failed(status, &error.to_string()),
        }
    }
    Ok(inventory)
}

fn status(
    root: &Path,
    provider: MutationProvider,
    options: &MutationProviderOptions,
) -> MutationProviderStatus {
    if provider == MutationProvider::BuiltIn {
        return MutationProviderStatus {
            id: provider,
            name: provider.display_name().to_owned(),
            languages: provider.languages().to_vec(),
            applicable: true,
            available: true,
            default: true,
            execution_enabled: true,
            executable: None,
            detection: Some(DetectionSource::BuiltIn),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            import_formats: vec![ImportFormat::MutationTestingElementsV2],
            reason: None,
            hint: None,
        };
    }

    let applicable = is_applicable(root, provider);
    let resolved = resolve_executable(root, provider, options);
    let (executable, detection, invalid_override) = match resolved {
        Resolution::Found(path, source) => (Some(path), Some(source), false),
        Resolution::InvalidOverride(path) => (Some(path), Some(DetectionSource::ExplicitOverride), true),
        Resolution::Missing => (None, None, false),
    };
    let available = executable.as_deref().is_some_and(is_executable) && !invalid_override;
    let (reason, hint) = if invalid_override {
        (
            Some("the explicit executable override is not an executable file".to_owned()),
            Some("fix or remove the provider override; reporigor never installs providers".to_owned()),
        )
    } else if !available {
        (
            Some(format!("{} executable was not found", provider.display_name())),
            Some(install_hint(provider).to_owned()),
        )
    } else if !applicable {
        (
            Some("the provider is installed but no matching project manifest was found".to_owned()),
            Some(applicability_hint(provider).to_owned()),
        )
    } else {
        (
            Some("report import is enabled; direct external execution remains opt-in and disabled until checkout isolation is guaranteed".to_owned()),
            Some("run the provider yourself and import its report, or use the deterministic built-in engine".to_owned()),
        )
    };
    let mut import_formats = vec![ImportFormat::MutationTestingElementsV2];
    if provider == MutationProvider::CargoMutants {
        import_formats.push(ImportFormat::CargoMutantsOutcomes);
    } else if provider == MutationProvider::Mull {
        import_formats.push(ImportFormat::MutationTestingElementsV1);
    } else if provider == MutationProvider::Muter {
        import_formats.push(ImportFormat::MuterJson);
    }
    MutationProviderStatus {
        id: provider,
        name: provider.display_name().to_owned(),
        languages: provider.languages().to_vec(),
        applicable,
        available,
        default: false,
        execution_enabled: false,
        executable,
        detection,
        version: None,
        import_formats,
        reason,
        hint,
    }
}

enum Resolution {
    Found(PathBuf, DetectionSource),
    InvalidOverride(PathBuf),
    Missing,
}

fn resolve_executable(
    root: &Path,
    provider: MutationProvider,
    options: &MutationProviderOptions,
) -> Resolution {
    if let Some(override_path) = options.executables.get(&provider) {
        return if let Some(canonical) = canonical_executable(override_path) {
            Resolution::Found(canonical, DetectionSource::ExplicitOverride)
        } else {
            Resolution::InvalidOverride(override_path.clone())
        };
    }
    for candidate in project_candidates(root, provider) {
        if let Some(canonical) = canonical_executable(&candidate) {
            // Known project-local locations are intentional, but a repository
            // symlink must not turn one into an arbitrary executable outside
            // the inspected project. An explicit override is the opt-in for
            // executables outside the project root.
            if canonical.starts_with(root) {
                return Resolution::Found(canonical, DetectionSource::ProjectLocal);
            }
        }
    }
    if provider != MutationProvider::Stryker {
        for name in executable_names(provider) {
            if let Some(path) = path_executable(name, root) {
                return Resolution::Found(path, DetectionSource::Path);
            }
        }
    }
    Resolution::Missing
}

fn project_candidates(root: &Path, provider: MutationProvider) -> Vec<PathBuf> {
    let mut candidates = match provider {
        MutationProvider::BuiltIn | MutationProvider::CargoMutants | MutationProvider::Mull => Vec::new(),
        MutationProvider::Mutmut => vec![
            root.join(".venv/bin/mutmut"),
            root.join("venv/bin/mutmut"),
            root.join(".venv/Scripts/mutmut.exe"),
            root.join("venv/Scripts/mutmut.exe"),
        ],
        MutationProvider::Stryker => vec![
            root.join("node_modules/.bin/stryker"),
            root.join("node_modules/.bin/stryker.cmd"),
        ],
        MutationProvider::Muter => vec![root.join(".build/debug/muter")],
    };
    if cfg!(windows) {
        let windows = candidates
            .iter()
            .filter(|path| path.extension().is_none())
            .map(|path| path.with_extension("exe"))
            .collect::<Vec<_>>();
        candidates.extend(windows);
    }
    candidates
}

const fn executable_names(provider: MutationProvider) -> &'static [&'static str] {
    match provider {
        MutationProvider::BuiltIn | MutationProvider::Stryker => &[],
        MutationProvider::CargoMutants => &["cargo-mutants"],
        MutationProvider::Mutmut => &["mutmut"],
        MutationProvider::Mull => &[
            "mull-runner-22",
            "mull-runner-21",
            "mull-runner-20",
            "mull-runner-19",
            "mull-runner-18",
            "mull-runner-17",
            "mull-runner-16",
            "mull-runner-15",
            "mull-runner-14",
            "mull-runner-13",
            "mull-runner",
        ],
        MutationProvider::Muter => &["muter"],
    }
}

fn path_executable(name: &str, root: &Path) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    path_executable_in(name, &path, root)
}

fn path_executable_in(name: &str, path: &std::ffi::OsStr, root: &Path) -> Option<PathBuf> {
    for directory in env::split_paths(path) {
        // Empty and relative PATH entries are interpreted relative to the
        // command's cwd. Provider probes run with the project as cwd, so using
        // either would let changing cwd substitute a repository executable.
        if !directory.is_absolute() {
            continue;
        }
        let Ok(canonical_directory) = fs::canonicalize(&directory) else {
            continue;
        };
        if directory.starts_with(root) || canonical_directory.starts_with(root) {
            continue;
        }
        let candidate = directory.join(name);
        if let Some(canonical) = canonical_executable(&candidate) {
            if !canonical.starts_with(root) {
                return Some(canonical);
            }
        }
        if cfg!(windows) {
            for extension in ["exe", "cmd", "bat"] {
                let candidate = directory.join(format!("{name}.{extension}"));
                if let Some(canonical) = canonical_executable(&candidate) {
                    if !canonical.starts_with(root) {
                        return Some(canonical);
                    }
                }
            }
        }
    }
    None
}

fn canonical_executable(path: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(path).ok()?;
    is_executable(&canonical).then_some(canonical)
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn is_applicable(root: &Path, provider: MutationProvider) -> bool {
    match provider {
        MutationProvider::BuiltIn => true,
        MutationProvider::CargoMutants => root.join("Cargo.toml").is_file(),
        MutationProvider::Mutmut => ["pyproject.toml", "setup.py", "setup.cfg"]
            .iter()
            .any(|name| root.join(name).is_file()),
        MutationProvider::Stryker => root.join("package.json").is_file(),
        MutationProvider::Mull => {
            root.join("compile_commands.json").is_file() || root.join("build/compile_commands.json").is_file()
        }
        MutationProvider::Muter => root.join("Package.swift").is_file(),
    }
}

fn version_command(
    provider: MutationProvider,
    program: PathBuf,
    cwd: PathBuf,
    timeout: std::time::Duration,
    output_limit_bytes: usize,
) -> BoundedCommand {
    let args = match provider {
        MutationProvider::BuiltIn => Vec::new(),
        MutationProvider::CargoMutants => {
            vec![OsString::from("mutants"), OsString::from("--version")]
        }
        MutationProvider::Mutmut
        | MutationProvider::Stryker
        | MutationProvider::Mull
        | MutationProvider::Muter => vec![OsString::from("--version")],
    };
    BoundedCommand {
        program,
        args,
        cwd,
        timeout,
        output_limit_bytes,
        effect: CommandEffect::ReadOnlyProbe,
    }
}

fn first_nonempty_line(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(240).collect())
}

fn mark_probe_failed(status: &mut MutationProviderStatus, message: &str) {
    status.available = false;
    status.version = None;
    status.reason = Some(format!("read-only version probe failed: {message}"));
    status.hint = Some("verify the detected executable or use an explicit override".to_owned());
}

const fn install_hint(provider: MutationProvider) -> &'static str {
    match provider {
        MutationProvider::BuiltIn => "the built-in provider is always available",
        MutationProvider::CargoMutants => {
            "install cargo-mutants yourself or set an explicit executable override"
        }
        MutationProvider::Mutmut => {
            "install mutmut in the project virtual environment or PATH; reporigor never runs pip"
        }
        MutationProvider::Stryker => {
            "add StrykerJS to the project dependencies; reporigor never runs npm, npx, or another installer"
        }
        MutationProvider::Mull => "install Mull yourself or set an explicit mull-runner override",
        MutationProvider::Muter => "install/build Muter yourself or set an explicit executable override",
    }
}

const fn applicability_hint(provider: MutationProvider) -> &'static str {
    match provider {
        MutationProvider::BuiltIn => "select a supported project root",
        MutationProvider::CargoMutants => "select a root containing Cargo.toml",
        MutationProvider::Mutmut => "select a Python package root",
        MutationProvider::Stryker => "select a root containing package.json",
        MutationProvider::Mull => "select a root with an existing compile_commands.json",
        MutationProvider::Muter => "select a root containing Package.swift",
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;

    #[test]
    fn path_search_skips_relative_and_project_local_entries() -> Result<(), Box<dyn Error>> {
        let project = tempfile::tempdir()?;
        let external = tempfile::tempdir()?;
        let project_root = fs::canonicalize(project.path())?;
        let project_bin = project_root.join("bin");
        let external_bin = external.path().join("bin");
        fs::create_dir_all(&project_bin)?;
        fs::create_dir_all(&external_bin)?;
        write_executable(&project_bin.join("mutmut"))?;
        write_executable(&external_bin.join("mutmut"))?;
        let search_path =
            env::join_paths([PathBuf::from("relative-bin"), project_bin, external_bin.clone()])?;

        let resolved = path_executable_in("mutmut", &search_path, &project_root)
            .ok_or("external executable was not resolved")?;
        assert_eq!(resolved, fs::canonicalize(external_bin.join("mutmut"))?);
        assert!(resolved.is_absolute());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn path_search_returns_a_canonical_target_not_a_replaceable_symlink() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir()?;
        let external = tempfile::tempdir()?;
        let bin = external.path().join("bin");
        fs::create_dir_all(&bin)?;
        let target = external.path().join("real-mutmut");
        write_executable(&target)?;
        symlink(&target, bin.join("mutmut"))?;
        let search_path = env::join_paths([bin])?;

        let resolved = path_executable_in("mutmut", &search_path, &fs::canonicalize(project.path())?)
            .ok_or("symlinked executable was not resolved")?;
        assert_eq!(resolved, fs::canonicalize(target)?);
        Ok(())
    }

    fn write_executable(path: &Path) -> Result<(), Box<dyn Error>> {
        fs::write(path, "#!/bin/sh\nexit 0\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path)?.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions)?;
        }
        Ok(())
    }
}
