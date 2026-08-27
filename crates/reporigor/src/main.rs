use std::io::{self, Write};
use std::process::ExitCode;

use clap::Parser;
use reporigor::{install_signal_handlers, legacy_entry_from_env, run, Cli};
use reporigor_reporting::escape_terminal_text;

fn main() -> ExitCode {
    if let Err(error) = install_signal_handlers() {
        write_error(&format!(
            "reporigor: cannot install cancellation handler: {error:#}"
        ));
        return ExitCode::FAILURE;
    }
    if let Some(exit) = legacy_entry_from_env() {
        return exit;
    }
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            write_error(&format!("reporigor: {error:#}"));
            ExitCode::FAILURE
        }
    }
}

fn write_error(message: &str) {
    let _result = writeln!(io::stderr().lock(), "{}", escape_terminal_text(message));
}
