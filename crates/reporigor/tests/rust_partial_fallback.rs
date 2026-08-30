use std::error::Error;

pub mod support;
use support::exit_assertion::assert_exit;
use support::fixtures::rust_project;
use support::incomplete_check_assertion::assert_incomplete_json_check;
use support::invocation::run;
use support::json_arguments_with_globals::json_arguments_with_globals;

#[test]
fn auto_rust_falls_back_only_for_native_parse_failures() -> Result<(), Box<dyn Error>> {
    let project = rust_project(
        "rust-partial-fallback",
        "pub fn retained() -> bool { true }\npub fn broken( { false }\n",
    );

    let automatic = run(check_arguments("auto", project.path(), true))?;
    let automatic = assert_incomplete_json_check(&automatic, "automatic Rust partial fallback");
    assert_eq!(automatic["summary"]["files"], 1);
    assert!(automatic["results"]["crap"]["functions"]
        .as_array()
        .is_some_and(|functions| functions.iter().any(|function| function["name"] == "retained")));
    for backend in ["rust-native", "tree-sitter-generic"] {
        assert!(automatic["backends"]
            .as_array()
            .is_some_and(|backends| backends.iter().any(|item| item["id"] == backend)));
    }
    let native_fallbacks = automatic["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|diagnostic| {
            diagnostic["backend"] == "rust-native"
                && diagnostic["fallback_used"] == true
                && diagnostic["location"]["file"] == "src/lib.rs"
        })
        .count();
    assert_eq!(
        native_fallbacks, 1,
        "native fallback diagnostic must not be duplicated"
    );

    let generic = run(check_arguments("generic", project.path(), false))?;
    let generic = assert_incomplete_json_check(&generic, "generic Rust parse-error reference");
    assert_eq!(
        automatic["summary"]["parse_errors"], generic["summary"]["parse_errors"],
        "native placeholder and generic parse errors were both counted"
    );

    let native = run(check_arguments("native", project.path(), true))?;
    assert_exit(&native, 1, "native Rust parse failure");
    assert!(String::from_utf8(native.stderr)?.contains("generic fallback"));
    Ok(())
}

fn check_arguments(backend: &str, root: &std::path::Path, allow_exec: bool) -> Vec<std::ffi::OsString> {
    json_arguments_with_globals(
        backend,
        "rust",
        allow_exec,
        &["--allow-parse-errors"],
        ("check", &["--min-tokens", "1000"]),
        root,
    )
}
