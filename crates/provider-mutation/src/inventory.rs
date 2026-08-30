use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{
    canonical_root, BoundedCommand, CommandEffect, CommandRunner, DetectionSource, ImportFormat,
    MutationProvider, MutationProviderOptions, MutationProviderStatus, ProviderError, ProviderInventory,
    SystemCommandRunner,
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
    let root = canonical_root(root)?;
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
    for provider_status in &mut inventory.providers {
        preflight_status(provider_status, &inventory.root, options, runner);
    }
    Ok(inventory)
}

fn preflight_status<R: CommandRunner>(
    status: &mut MutationProviderStatus,
    root: &Path,
    options: &MutationProviderOptions,
    runner: &R,
) {
    if !probe_eligible(status) {
        return;
    }
    let Some(program) = status.executable.clone() else {
        return;
    };
    let command = version_command(
        status.id,
        program,
        root.to_path_buf(),
        options.probe_timeout,
        options.output_limit_bytes,
    );
    apply_probe_result(status, runner.run(&command));
}

fn probe_eligible(status: &MutationProviderStatus) -> bool {
    status.id != MutationProvider::BuiltIn && status.applicable && status.available
}

fn apply_probe_result(
    status: &mut MutationProviderStatus,
    result: Result<crate::CommandOutput, ProviderError>,
) {
    match result {
        Ok(output) => apply_probe_output(status, &output),
        Err(error) => mark_probe_failed(status, &error.to_string()),
    }
}

fn apply_probe_output(status: &mut MutationProviderStatus, output: &crate::CommandOutput) {
    if output.output_truncated {
        mark_probe_failed(status, "version probe output exceeded its configured limit");
        return;
    }
    if !output.success() {
        mark_probe_failed(status, &probe_failure_detail(output));
        return;
    }
    match first_nonempty_line(&output.stdout).or_else(|| first_nonempty_line(&output.stderr)) {
        Some(version) => status.version = Some(version),
        None => mark_probe_failed(status, "version probe returned no version text"),
    }
}

fn probe_failure_detail(output: &crate::CommandOutput) -> String {
    first_nonempty_line(&output.stderr)
        .or_else(|| first_nonempty_line(&output.stdout))
        .unwrap_or_else(|| format!("exit status {:?}", output.exit_code))
}

fn status(
    root: &Path,
    provider: MutationProvider,
    options: &MutationProviderOptions,
) -> MutationProviderStatus {
    if provider == MutationProvider::BuiltIn {
        return built_in_status();
    }

    external_status(root, provider, options)
}

fn built_in_status() -> MutationProviderStatus {
    MutationProviderStatus {
        id: MutationProvider::BuiltIn,
        name: MutationProvider::BuiltIn.display_name().to_owned(),
        languages: MutationProvider::BuiltIn.languages().to_vec(),
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
    }
}

fn external_status(
    root: &Path,
    provider: MutationProvider,
    options: &MutationProviderOptions,
) -> MutationProviderStatus {
    let applicable = is_applicable(root, provider);
    let (executable, detection, invalid_override) =
        resolution_fields(resolve_executable(root, provider, options));
    let available = executable
        .as_deref()
        .is_some_and(reporigor_core::is_executable_file)
        && !invalid_override;
    let (reason, hint) = provider_guidance(provider, applicable, available, invalid_override);
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
        import_formats: provider_import_formats(provider),
        reason,
        hint,
    }
}

fn resolution_fields(resolution: Resolution) -> (Option<PathBuf>, Option<DetectionSource>, bool) {
    match resolution {
        Resolution::Found(path, source) => (Some(path), Some(source), false),
        Resolution::InvalidOverride(path) => (Some(path), Some(DetectionSource::ExplicitOverride), true),
        Resolution::Missing => (None, None, false),
    }
}

fn provider_guidance(
    provider: MutationProvider,
    applicable: bool,
    available: bool,
    invalid_override: bool,
) -> (Option<String>, Option<String>) {
    if invalid_override {
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
    }
}

fn provider_import_formats(provider: MutationProvider) -> Vec<ImportFormat> {
    let mut import_formats = vec![ImportFormat::MutationTestingElementsV2];
    if provider == MutationProvider::CargoMutants {
        import_formats.push(ImportFormat::CargoMutantsOutcomes);
    } else if provider == MutationProvider::Mull {
        import_formats.push(ImportFormat::MutationTestingElementsV1);
    } else if provider == MutationProvider::Muter {
        import_formats.push(ImportFormat::MuterJson);
    }
    import_formats
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
        return resolve_override(override_path);
    }
    if let Some(resolution) = resolve_project_executable(root, provider) {
        return resolution;
    }
    resolve_path_executable(root, provider)
}

fn resolve_override(override_path: &Path) -> Resolution {
    match canonical_executable(override_path) {
        Some(canonical) => Resolution::Found(canonical, DetectionSource::ExplicitOverride),
        None => Resolution::InvalidOverride(override_path.to_path_buf()),
    }
}

fn resolve_project_executable(root: &Path, provider: MutationProvider) -> Option<Resolution> {
    for candidate in project_candidates(root, provider) {
        if let Some(canonical) = canonical_executable(&candidate) {
            // Known project-local locations are intentional, but a repository
            // symlink must not turn one into an arbitrary executable outside
            // the inspected project. An explicit override is the opt-in for
            // executables outside the project root.
            if canonical.starts_with(root) {
                return Some(Resolution::Found(canonical, DetectionSource::ProjectLocal));
            }
        }
    }
    None
}

fn resolve_path_executable(root: &Path, provider: MutationProvider) -> Resolution {
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

fn executable_names(provider: MutationProvider) -> impl Iterator<Item = &'static str> {
    let names = match provider {
        MutationProvider::BuiltIn | MutationProvider::Stryker => "",
        MutationProvider::CargoMutants => "cargo-mutants",
        MutationProvider::Mutmut => "mutmut",
        MutationProvider::Mull => {
            "mull-runner-22|mull-runner-21|mull-runner-20|mull-runner-19|mull-runner-18|mull-runner-17|mull-runner-16|mull-runner-15|mull-runner-14|mull-runner-13|mull-runner"
        }
        MutationProvider::Muter => "muter",
    };
    names.split('|').filter(|name| !name.is_empty())
}

fn path_executable(name: &str, root: &Path) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    path_executable_in(name, &path, root)
}

fn path_executable_in(name: &str, path: &std::ffi::OsStr, root: &Path) -> Option<PathBuf> {
    for directory in env::split_paths(path) {
        if let Some(executable) = executable_in_directory(name, &directory, root) {
            return Some(executable);
        }
    }
    None
}

fn executable_in_directory(name: &str, directory: &Path, root: &Path) -> Option<PathBuf> {
    // Empty and relative PATH entries are interpreted relative to the
    // command's cwd. Provider probes run with the project as cwd, so using
    // either would let changing cwd substitute a repository executable.
    if !directory.is_absolute() {
        return None;
    }
    let canonical_directory = fs::canonicalize(directory).ok()?;
    if directory.starts_with(root) || canonical_directory.starts_with(root) {
        return None;
    }
    executable_candidates(name, directory)
        .into_iter()
        .find_map(|candidate| external_executable(&candidate, root))
}

fn executable_candidates(name: &str, directory: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![directory.join(name)];
    if cfg!(windows) {
        candidates
            .extend(["exe", "cmd", "bat"].map(|extension| directory.join(format!("{name}.{extension}"))));
    }
    candidates
}

fn external_executable(candidate: &Path, root: &Path) -> Option<PathBuf> {
    canonical_executable(candidate).filter(|canonical| !canonical.starts_with(root))
}

fn canonical_executable(path: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(path).ok()?;
    reporigor_core::is_executable_file(&canonical).then_some(canonical)
}

fn is_applicable(root: &Path, provider: MutationProvider) -> bool {
    if provider == MutationProvider::BuiltIn {
        return true;
    }
    applicable_manifests(provider).any(|manifest| root.join(manifest).is_file())
}

fn applicable_manifests(provider: MutationProvider) -> impl Iterator<Item = &'static str> {
    const MANIFESTS: [&str; MutationProvider::ALL.len()] = [
        "",
        "Cargo.toml",
        "pyproject.toml|setup.py|setup.cfg",
        "package.json",
        "compile_commands.json|build/compile_commands.json",
        "Package.swift",
    ];
    let manifests = MANIFESTS[provider as usize];
    manifests.split('|').filter(|manifest| !manifest.is_empty())
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

struct ProviderHints {
    install: &'static str,
    applicability: &'static str,
}

const BUILTIN_HINTS: ProviderHints = ProviderHints {
    install: "the built-in provider is always available",
    applicability: "select a supported project root",
};
const CARGO_MUTANTS_HINTS: ProviderHints = ProviderHints {
    install: "install cargo-mutants yourself or set an explicit executable override",
    applicability: "select a root containing Cargo.toml",
};
const MUTMUT_HINTS: ProviderHints = ProviderHints {
    install: "install mutmut in the project virtual environment or PATH; reporigor never runs pip",
    applicability: "select a Python package root",
};
const STRYKER_HINTS: ProviderHints = ProviderHints {
    install: "add StrykerJS to the project dependencies; reporigor never runs npm, npx, or another installer",
    applicability: "select a root containing package.json",
};
const MULL_HINTS: ProviderHints = ProviderHints {
    install: "install Mull yourself or set an explicit mull-runner override",
    applicability: "select a root with an existing compile_commands.json",
};
const MUTER_HINTS: ProviderHints = ProviderHints {
    install: "install/build Muter yourself or set an explicit executable override",
    applicability: "select a root containing Package.swift",
};

const PROVIDER_HINTS: [ProviderHints; MutationProvider::ALL.len()] = [
    BUILTIN_HINTS,
    CARGO_MUTANTS_HINTS,
    MUTMUT_HINTS,
    STRYKER_HINTS,
    MULL_HINTS,
    MUTER_HINTS,
];

const fn install_hint(provider: MutationProvider) -> &'static str {
    PROVIDER_HINTS[provider as usize].install
}

const fn applicability_hint(provider: MutationProvider) -> &'static str {
    PROVIDER_HINTS[provider as usize].applicability
}

#[cfg(test)]
mod tests {
    use super::{env, fs, path_executable_in, Path, PathBuf};
    use crate::test_support::must;

    #[test]
    fn path_search_skips_relative_and_project_local_entries() {
        let [project, external] = [tempfile::tempdir(), tempfile::tempdir()].map(must);
        let project_root = must(fs::canonicalize(project.path()));
        let project_bin = project_root.join("bin");
        let external_bin = external.path().join("bin");
        must(fs::create_dir_all(&project_bin));
        must(fs::create_dir_all(&external_bin));
        write_executable(&project_bin.join("mutmut"));
        write_executable(&external_bin.join("mutmut"));
        let search_path = must(env::join_paths([
            PathBuf::from("relative-bin"),
            project_bin,
            external_bin.clone(),
        ]));

        let resolved = resolve_test_executable(&search_path, &project_root);
        assert_eq!(resolved, must(fs::canonicalize(external_bin.join("mutmut"))));
        assert!(resolved.is_absolute());
    }

    #[cfg(unix)]
    #[test]
    fn path_search_returns_a_canonical_target_not_a_replaceable_symlink() {
        use std::os::unix::fs::symlink;

        let project = must(tempfile::tempdir());
        let external = must(tempfile::tempdir());
        let bin = external.path().join("bin");
        must(fs::create_dir_all(&bin));
        let target = external.path().join("real-mutmut");
        write_executable(&target);
        must(symlink(&target, bin.join("mutmut")));
        let search_path = must(env::join_paths([bin]));

        let project_root = must(fs::canonicalize(project.path()));
        let resolved = resolve_test_executable(&search_path, &project_root);
        assert_eq!(resolved, must(fs::canonicalize(target)));
    }

    fn resolve_test_executable(search_path: &std::ffi::OsStr, project_root: &Path) -> PathBuf {
        must(path_executable_in("mutmut", search_path, project_root))
    }

    fn write_executable(path: &Path) {
        must(fs::write(path, "#!/bin/sh\nexit 0\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = must(fs::metadata(path)).permissions();
            permissions.set_mode(0o755);
            must(fs::set_permissions(path, permissions));
        }
    }
}
