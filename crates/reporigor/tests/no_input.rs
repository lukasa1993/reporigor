use std::error::Error;
use std::process::Output;

pub mod support;
use support::{
    fixtures::retained_python_project, invocation::run, json_arguments::json_arguments as build_arguments,
    json_arguments_with_globals::json_arguments_with_globals, message_assertion::assert_message_contains_all,
    operational_error::operational_error, output_parser::parse_output, success_assertion::assert_success,
};

#[test]
fn read_only_quality_commands_reject_an_empty_project() -> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    for arguments in [
        vec!["dry", "--min-tokens", "1000"],
        vec!["mutate", "--list"],
        vec!["check", "--min-tokens", "1000"],
    ] {
        let output = run(build_arguments(
            "generic",
            "python",
            arguments[0],
            &arguments[1..],
            project.path(),
        ))?;
        assert_no_sources_error(&output, arguments[0]);
    }
    Ok(())
}

#[test]
fn stale_filter_cannot_turn_check_into_a_successful_empty_gate() -> Result<(), Box<dyn Error>> {
    let project = retained_python_project();
    let output = run(json_arguments_with_globals(
        "generic",
        "python",
        false,
        &["--filter", "stale/path/that/selects/nothing"],
        ("check", &["--min-tokens", "1000"]),
        project.path(),
    ))?;
    assert_no_sources_error(&output, "stale-filter check");
    Ok(())
}

#[test]
fn crap_retains_its_explicit_historical_empty_opt_in() -> Result<(), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    let output = run(build_arguments(
        "generic",
        "python",
        "crap",
        &["--allow-empty", "--allow-missing-coverage"],
        project.path(),
    ))?;
    assert_success(&output, "explicit CRAP empty opt-in");
    let report: serde_json::Value = parse_output(&output, "explicit CRAP empty opt-in");
    assert_eq!(report["summary"]["files"], 0);
    Ok(())
}

fn assert_no_sources_error(output: &Output, context: &str) {
    let stderr = operational_error(output, context);
    assert_message_contains_all(
        &stderr,
        &format!("{context} returned a non-actionable error"),
        &["no source files were selected", "--language", "--filter"],
    );
}
