use std::{
    env,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{self, Command},
};

use reporigor::write_terminal_error;

fn main() {
    let mut arguments = env::args_os();
    let invoked_as = arguments
        .next()
        .unwrap_or_else(|| OsString::from("reporigor-legacy"));
    let alias = Path::new(&invoked_as)
        .file_name()
        .map_or_else(|| invoked_as.clone(), OsStr::to_os_string);
    let mut reporigor = env::current_exe().unwrap_or_else(|error| {
        launch_error(Path::new("reporigor"), &error);
    });
    reporigor.set_file_name(reporigor_file_name());

    launch(&reporigor, &alias, arguments);
}

#[cfg(unix)]
fn launch(reporigor: &Path, alias: &OsStr, arguments: impl Iterator<Item = OsString>) -> ! {
    use std::os::unix::process::CommandExt;

    let error = Command::new(reporigor).arg0(alias).args(arguments).exec();
    launch_error(reporigor, &error)
}

#[cfg(not(unix))]
fn launch(reporigor: &Path, alias: &OsStr, arguments: impl Iterator<Item = OsString>) -> ! {
    // `std::process::Command` has no portable argv[0] override. The multicall
    // entry point therefore also accepts the legacy name as its first argument.
    let status = Command::new(reporigor)
        .arg(alias)
        .args(arguments)
        .status()
        .unwrap_or_else(|error| launch_error(reporigor, &error));
    match status.code() {
        Some(code) => process::exit(code),
        None => {
            write_launcher_error(&format!(
                "reporigor legacy launcher: {} terminated without an exit code",
                reporigor.display()
            ));
            process::exit(1);
        }
    }
}

fn reporigor_file_name() -> PathBuf {
    let mut name = PathBuf::from("reporigor");
    if cfg!(windows) {
        name.set_extension("exe");
    }
    name
}

fn launch_error(reporigor: &Path, error: &std::io::Error) -> ! {
    write_launcher_error(&format!(
        "reporigor legacy launcher: failed to execute sibling {}: {error}; install the main reporigor binary beside this alias",
        reporigor.display()
    ));
    process::exit(1)
}

fn write_launcher_error(message: &str) {
    write_terminal_error(message);
}
