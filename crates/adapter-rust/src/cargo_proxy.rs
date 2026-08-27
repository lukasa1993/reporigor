use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::command::{render_stream, run_bounded, CommandLimits};
use crate::CargoOptions;

const REAL_CARGO_ENV: &str = "REPORIGOR_REAL_CARGO";
const CLEANUP_ATTEMPTS: usize = 8;
const PROXY_BUILD_TIMEOUT: Duration = Duration::from_secs(30);
const PROXY_BUILD_OUTPUT_LIMIT: usize = 256 * 1024;
static NEXT_PROXY_ID: AtomicU64 = AtomicU64::new(0);

/// A generated Cargo wrapper whose environment is applied only to explicitly
/// configured child processes. Unlike the legacy guards, this never changes
/// process-global `PATH`, so concurrent analyzers cannot corrupt one another.
#[derive(Debug)]
pub struct CargoProxy {
    directory: PathBuf,
    real_cargo: PathBuf,
}

impl CargoProxy {
    /// Builds a proxy only when non-default Cargo feature options are present.
    ///
    /// # Errors
    ///
    /// Returns an error when Cargo or Rust cannot be located, the temporary
    /// proxy directory cannot be created, or the proxy fails to compile.
    pub fn for_options(options: &CargoOptions) -> io::Result<Option<Self>> {
        let extra = options.feature_args();
        if extra.is_empty() {
            return Ok(None);
        }
        let real_cargo = if options.cargo.is_none() {
            env::var_os(REAL_CARGO_ENV)
                .map(PathBuf::from)
                .filter(|path| path.is_file())
                .map_or_else(|| resolve_program(options.cargo_program()), Ok)?
        } else {
            resolve_program(options.cargo_program())?
        };
        let rustc = find_on_path(OsStr::new("rustc"))?;
        let directory = proxy_directory();
        build_proxy(&directory, &rustc, &extra)?;
        let directory = directory.canonicalize().map_err(|error| {
            remove_proxy_directory(&directory);
            io::Error::new(
                error.kind(),
                format!(
                    "cannot canonicalize Cargo proxy directory {}: {error}",
                    directory.display()
                ),
            )
        })?;
        Ok(Some(Self {
            directory,
            real_cargo,
        }))
    }

    /// Configures a child command and all of its descendants to use the proxy.
    ///
    /// # Errors
    ///
    /// Returns an error when the inherited and proxy paths cannot be joined.
    pub fn apply_to(&self, command: &mut Command) -> io::Result<()> {
        let inherited = env::var_os("PATH");
        let mut paths = vec![self.directory.clone()];
        if let Some(value) = inherited {
            paths.extend(env::split_paths(&value).filter(|path| path.is_absolute()));
        }
        let joined = env::join_paths(paths).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cannot construct Cargo proxy PATH: {error}"),
            )
        })?;
        command.env("PATH", joined);
        command.env(REAL_CARGO_ENV, &self.real_cargo);
        Ok(())
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

impl Drop for CargoProxy {
    fn drop(&mut self) {
        remove_proxy_directory(&self.directory);
    }
}

fn executable_name(name: &str) -> String {
    if cfg!(windows) && !name.to_ascii_lowercase().ends_with(".exe") {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

pub(crate) fn resolve_program(program: &OsStr) -> io::Result<PathBuf> {
    let candidate = PathBuf::from(program);
    if candidate.components().count() > 1 || candidate.is_absolute() {
        if !candidate.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "configured executable path must be absolute, not relative: {}",
                    candidate.display()
                ),
            ));
        }
        return canonical_executable(&candidate);
    }
    find_on_path(program)
}

fn canonical_executable(candidate: &Path) -> io::Result<PathBuf> {
    let canonical = candidate.canonicalize().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot resolve executable {}: {error}", candidate.display()),
        )
    })?;
    if !canonical.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("executable is not a file: {}", canonical.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if canonical
            .metadata()
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot inspect executable {}: {error}", canonical.display()),
                )
            })?
            .permissions()
            .mode()
            & 0o111
            == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("file is not executable: {}", canonical.display()),
            ));
        }
    }
    // rustup selects `cargo` versus `rustc` from the shim's argv[0]. Calling
    // the fully dereferenced `rustup` path would silently change semantics.
    // Keep that well-known shim name while still canonicalizing its parent,
    // validating its final target above, and returning an absolute path.
    if canonical.file_name() == Some(OsStr::new(executable_name("rustup").as_str()))
        && matches!(
            candidate.file_stem().and_then(OsStr::to_str),
            Some("cargo" | "rustc")
        )
    {
        let parent = candidate.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("executable has no parent directory: {}", candidate.display()),
            )
        })?;
        return Ok(parent.canonicalize()?.join(
            candidate
                .file_name()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "executable has no file name"))?,
        ));
    }
    Ok(canonical)
}

fn find_on_search_path(name: &OsStr, path: &OsStr) -> io::Result<PathBuf> {
    let text = name.to_string_lossy();
    let executable = executable_name(&text);
    for directory in env::split_paths(&path) {
        if !directory.is_absolute() {
            continue;
        }
        let candidate = directory.join(&executable);
        if candidate.is_file() {
            return canonical_executable(&candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("cannot find {text} on PATH"),
    ))
}

fn find_on_path(name: &OsStr) -> io::Result<PathBuf> {
    let path =
        env::var_os("PATH").ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PATH is not set"))?;
    find_on_search_path(name, &path)
}

fn proxy_source(extra: &[OsString]) -> String {
    let encoded = extra
        .iter()
        .map(|value| format!("{:?}", value.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"use std::env;
use std::ffi::OsString;
use std::process::{{Command, exit}};

const EXTRA: &[&str] = &[{encoded}];

fn feature_subcommand(name: &str) -> bool {{
    matches!(
        name,
        "rustc" | "metadata" | "test" | "check" | "build" | "clippy" |
        "doc" | "run" | "bench" | "fix" | "llvm-cov"
    )
}}

fn injection_index(args: &[OsString]) -> Option<usize> {{
    for (index, value) in args.iter().enumerate() {{
        let Some(name) = value.to_str() else {{ continue }};
        if feature_subcommand(name) {{ return Some(index + 1); }}
        if name == "nextest" {{
            for (offset, nested) in args[index + 1..].iter().enumerate() {{
                if matches!(nested.to_str(), Some("run" | "list")) {{
                    return Some(index + offset + 2);
                }}
            }}
        }}
    }}
    None
}}

fn main() {{
    let Some(real_cargo) = env::var_os("{REAL_CARGO_ENV}") else {{
        eprintln!("cargo proxy: {REAL_CARGO_ENV} is not set");
        exit(1);
    }};
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let mut command = Command::new(real_cargo);
    if let Some(index) = injection_index(&args) {{
        command.args(&args[..index]);
        command.args(EXTRA);
        command.args(&args[index..]);
    }} else {{
        command.args(&args);
    }}
    match command.status() {{
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(error) => {{
            eprintln!("cargo proxy: cannot execute Cargo: {{error}}");
            exit(1);
        }}
    }}
}}
"#
    )
}

fn proxy_directory() -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_PROXY_ID.fetch_add(1, Ordering::Relaxed);
    env::temp_dir()
        .join("reporigor")
        .join("cargo-proxy")
        .join(format!("{}-{timestamp}-{sequence}", std::process::id()))
}

fn remove_proxy_directory_with<F>(directory: &Path, mut remove: F)
where
    F: FnMut(&Path) -> io::Result<()>,
{
    for attempt in 0..CLEANUP_ATTEMPTS {
        match remove(directory) {
            Ok(()) => return,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) if attempt + 1 == CLEANUP_ATTEMPTS => {
                eprintln!(
                    "warning: cannot remove Cargo proxy directory {} after {} attempts: {error}",
                    directory.display(),
                    CLEANUP_ATTEMPTS
                );
                return;
            }
            Err(_) => {
                let delay_ms = 10_u64 << attempt.min(6);
                thread::sleep(Duration::from_millis(delay_ms));
            }
        }
    }
}

fn remove_proxy_directory(directory: &Path) {
    remove_proxy_directory_with(directory, |path| fs::remove_dir_all(path));
}

fn build_proxy(directory: &Path, rustc: &Path, extra: &[OsString]) -> io::Result<PathBuf> {
    if let Err(error) = fs::create_dir_all(directory) {
        remove_proxy_directory(directory);
        return Err(error);
    }
    let result = (|| {
        let source_path = directory.join("cargo_proxy.rs");
        let executable = directory.join(executable_name("cargo"));
        fs::write(&source_path, proxy_source(extra))?;
        let mut command = Command::new(rustc);
        command
            .arg("--edition=2021")
            .arg(&source_path)
            .arg("-O")
            .arg("-o")
            .arg(&executable)
            .current_dir(directory);
        let limits = CommandLimits {
            timeout: PROXY_BUILD_TIMEOUT,
            stdout_bytes: PROXY_BUILD_OUTPUT_LIMIT,
            stderr_bytes: PROXY_BUILD_OUTPUT_LIMIT,
        };
        let output =
            run_bounded(&mut command, "Cargo feature proxy rustc build", limits).map_err(io::Error::other)?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "rustc failed to build Cargo feature proxy with exit code {:?}: {}",
                output.status.code(),
                render_stream(&output.stderr, limits.stderr_bytes).trim()
            )));
        }
        Ok(executable)
    })();
    if result.is_err() {
        remove_proxy_directory(directory);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap_or_else(|error| panic!("write executable: {error}"));
        let mut permissions = fs::metadata(path)
            .unwrap_or_else(|error| panic!("executable metadata: {error}"))
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)
            .unwrap_or_else(|error| panic!("executable permissions: {error}"));
    }

    #[test]
    fn proxy_directories_are_unique_and_outside_a_project() {
        let first = proxy_directory();
        let second = proxy_directory();
        assert_ne!(first, second);
        assert!(first.starts_with(env::temp_dir().join("reporigor")));
    }

    #[test]
    fn cleanup_retries_transient_failures() {
        let mut attempts = 0;
        remove_proxy_directory_with(Path::new("unused"), |_| {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "locked"))
            } else {
                Ok(())
            }
        });
        assert_eq!(attempts, 3);
    }

    #[test]
    fn generated_proxy_knows_nested_and_direct_cargo_commands() {
        let source = proxy_source(&[OsString::from("--features"), OsString::from("extra")]);
        assert!(source.contains("nextest"));
        assert!(source.contains("llvm-cov"));
        assert!(source.contains("\"--features\", \"extra\""));
    }

    #[cfg(unix)]
    #[test]
    fn executable_lookup_ignores_empty_and_relative_path_entries() {
        let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let absolute_bin = directory.path().join("absolute-bin");
        fs::create_dir(&absolute_bin).unwrap_or_else(|error| panic!("absolute bin: {error}"));
        let cargo = absolute_bin.join("cargo");
        make_executable(&cargo);
        let rustc = absolute_bin.join("rustc");
        make_executable(&rustc);
        let search = env::join_paths([
            PathBuf::new(),
            PathBuf::from("relative-bin"),
            absolute_bin.clone(),
        ])
        .unwrap_or_else(|error| panic!("search path: {error}"));

        let resolved = find_on_search_path(OsStr::new("cargo"), &search)
            .unwrap_or_else(|error| panic!("resolve cargo: {error}"));
        assert_eq!(resolved, cargo.canonicalize().unwrap_or(cargo));
        assert!(resolved.is_absolute());
        let resolved_rustc = find_on_search_path(OsStr::new("rustc"), &search)
            .unwrap_or_else(|error| panic!("resolve rustc: {error}"));
        assert_eq!(resolved_rustc, rustc.canonicalize().unwrap_or(rustc));
        assert!(resolved_rustc.is_absolute());
    }

    #[test]
    fn configured_relative_executable_is_rejected() {
        let error = match resolve_program(OsStr::new("tools/cargo")) {
            Ok(path) => panic!("relative executable unexpectedly resolved to {}", path.display()),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("must be absolute"));
    }

    #[test]
    fn compiled_proxy_is_scoped_to_the_configured_child() {
        let before_path = env::var_os("PATH");
        let before_real_cargo = env::var_os(REAL_CARGO_ENV);
        let proxy = CargoProxy::for_options(&CargoOptions {
            features: vec!["extra".into()],
            ..CargoOptions::default()
        })
        .unwrap_or_else(|error| panic!("build proxy: {error}"))
        .unwrap_or_else(|| panic!("feature options should require a proxy"));
        let mut command = Command::new("cargo");
        command.arg("--version");
        proxy
            .apply_to(&mut command)
            .unwrap_or_else(|error| panic!("configure child: {error}"));
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("execute child: {error}"));
        assert!(output.status.success());
        assert_eq!(env::var_os("PATH"), before_path);
        assert_eq!(env::var_os(REAL_CARGO_ENV), before_real_cargo);
    }
}
