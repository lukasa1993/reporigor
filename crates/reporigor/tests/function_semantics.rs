use std::collections::BTreeMap;
use std::path::Path;

use reporigor_reporting::ReportEnvelope;

pub mod support;
use support::environment::command_available;
use support::fixtures::{rust_project, temporary_project_file, write_fixture};
use support::invocation::run_isolated;
use support::json_arguments::json_arguments;
use support::output_parser::parse_output;
use support::success_assertion::assert_success;

#[test]
fn generic_and_native_rust_share_nested_function_semantics() {
    let project = rust_project(
        "function-semantics",
        r"pub fn outer(flag: bool) -> bool {
    fn local(value: bool) -> bool {
        if value { loop { break; } }
        value
    }
    let deferred = |value: bool| {
        if value { while value { break; } }
        value
    };
    if flag { deferred(flag) } else { false }
}

pub fn sibling() -> bool { true }

pub fn semantic_decisions(value: Option<i32>, fallback: Option<i32>) -> Option<i32> {
    let value = value?;
    let Some(fallback) = fallback else { return None; };
    match value {
        0 => Some(0),
        1 if fallback > 0 => Some(1),
        _ => Some(fallback),
    }
}
",
    );

    assert_rust_complexities(project.path());
}

fn assert_rust_complexities(root: &Path) {
    assert_backend_complexities(
        "rust",
        root,
        &[("outer", 2), ("semantic_decisions", 6), ("sibling", 1)],
    );
}

#[test]
fn generic_and_native_clang_share_cpp_lambda_semantics() {
    if !command_available("clang") {
        eprintln!("skipping C++ function-semantics test because clang is unavailable");
        return;
    }

    let project = temporary_project_file(
        "src/sample.cpp",
        r"int outer(int value) {
  auto deferred = [](int inner) {
    if (inner > 0) { while (inner > 1) { --inner; } }
    return inner;
  };
  if (value > 0) { return deferred(value); }
  return 0;
}

int sibling() { return 1; }
",
    );
    let database = serde_json::json!([{
        "directory": project.path(),
        "file": "src/sample.cpp",
        "arguments": ["clang++", "-std=c++17", "-c", "src/sample.cpp"]
    }]);
    write_fixture(
        &project.path().join("compile_commands.json"),
        &database.to_string(),
    );

    assert_backend_complexities("cpp", project.path(), &[("outer", 2), ("sibling", 1)]);
}

fn assert_backend_complexities(language: &str, root: &Path, expected: &[(&str, u32)]) {
    let expected = expected
        .iter()
        .map(|(name, complexity)| ((*name).to_owned(), *complexity))
        .collect::<BTreeMap<_, _>>();
    for backend in ["generic", "native"] {
        let report = run_crap(backend, language, root);
        assert_eq!(function_complexities(&report), expected);
    }
}

fn run_crap(backend: &str, language: &str, root: &Path) -> ReportEnvelope {
    let arguments = json_arguments(backend, language, "crap", &["--allow-missing-coverage"], root);
    let output = run_isolated(&arguments);
    let context = format!("{backend} {language}");
    assert_success(&output, &context);
    parse_output(&output, &context)
}

fn function_complexities(report: &ReportEnvelope) -> BTreeMap<String, u32> {
    report
        .results
        .crap
        .as_ref()
        .unwrap_or_else(|| panic!("CRAP section is missing"))
        .functions
        .iter()
        .map(|function| (function.name.clone(), function.complexity))
        .collect()
}
