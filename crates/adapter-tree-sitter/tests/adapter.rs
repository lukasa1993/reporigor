use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use adapter_tree_sitter::TreeSitterBackend;
use reporigor_core::{
    AnalysisRequest, Capability, CoreError, FileAnalysis, FunctionRecord, Language, Severity, SourceFile,
    SymbolVisibility, SyntaxBackend,
};

#[derive(Clone, Copy)]
struct Fixture {
    language: Language,
    filename: &'static str,
}

const FIXTURES: &[Fixture] = &[
    fixture(Language::Bash, "sample.sh"),
    fixture(Language::C, "sample.c"),
    fixture(Language::Cpp, "sample.cpp"),
    fixture(Language::ObjectiveC, "sample.m"),
    fixture(Language::Python, "sample.py"),
    fixture(Language::Rust, "sample.rs"),
    fixture(Language::Swift, "sample.swift"),
    fixture(Language::TypeScript, "sample.ts"),
    fixture(Language::TypeScript, "sample.tsx"),
];

const fn fixture(language: Language, filename: &'static str) -> Fixture {
    Fixture { language, filename }
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_source(fixture: Fixture) -> SourceFile {
    test_source(fixture.language, fixture.filename)
}

fn test_source(language: Language, filename: &str) -> SourceFile {
    source_file(PathBuf::from("tests/fixtures").join(filename), filename, language)
}

fn source_file(path: PathBuf, relative: &str, language: Language) -> SourceFile {
    SourceFile {
        path,
        relative: relative.to_string(),
        language,
        generated: false,
        test: false,
    }
}

fn expected_function_count(filename: &str) -> usize {
    match filename {
        "sample.sh" | "sample.ts" => 2,
        _ => 1,
    }
}

fn analyze(fixture: Fixture) -> FileAnalysis {
    let root = crate_root();
    analyze_requested(
        &root,
        &fixture_source(fixture),
        &AnalysisRequest::new(root.clone()),
    )
}

fn analyze_named(language: Language, filename: &str) -> FileAnalysis {
    let root = crate_root();
    let source = test_source(language, filename);
    analyze_requested(&root, &source, &AnalysisRequest::new(root.clone()))
}

fn analyze_requested(root: &Path, source: &SourceFile, request: &AnalysisRequest) -> FileAnalysis {
    TreeSitterBackend::new()
        .analyze_file(root, source, request)
        .unwrap_or_else(|error| panic!("{} should analyze: {error}", source.relative))
}

fn named_function<'analysis>(analysis: &'analysis FileAnalysis, name: &str) -> &'analysis FunctionRecord {
    analysis
        .functions
        .iter()
        .find(|function| function.name == name)
        .unwrap_or_else(|| panic!("function {name:?} missing"))
}

fn only_function(analysis: &FileAnalysis) -> &FunctionRecord {
    assert_eq!(analysis.functions.len(), 1);
    &analysis.functions[0]
}

fn assert_same_function_structure(left: &FunctionRecord, right: &FunctionRecord) {
    assert_eq!(left.stable_symbol, right.stable_symbol);
    assert_eq!(left.normalized_tokens, right.normalized_tokens);
    assert_eq!(left.references, right.references);
}

fn assert_function_value_absent(functions: &[&FunctionRecord], value: &str, references: bool) {
    for function in functions {
        assert!(!function.normalized_tokens.iter().any(|token| token == value));
        if references {
            assert!(!function.references.contains(value));
        }
    }
}

fn token_values(analysis: &FileAnalysis) -> Vec<&str> {
    analysis.tokens.iter().map(|token| token.value.as_str()).collect()
}

fn mutation_originals(analysis: &FileAnalysis) -> Vec<&str> {
    analysis
        .mutations
        .iter()
        .map(|mutation| mutation.original.as_str())
        .collect()
}

fn equivalent_functions(language: Language, left: &str, right: &str) -> (FunctionRecord, FunctionRecord) {
    let left_analysis = analyze_named(language, left);
    let right_analysis = analyze_named(language, right);
    let left = only_function(&left_analysis).clone();
    let right = only_function(&right_analysis).clone();
    assert_same_function_structure(&left, &right);
    (left, right)
}

fn assert_values_absent(functions: &[&FunctionRecord], encoded: &str, references: bool) {
    for value in encoded.split('|') {
        assert_function_value_absent(functions, value, references);
    }
}

fn assert_values_present(function: &FunctionRecord, encoded: &str) {
    for value in encoded.split('|') {
        assert!(function.references.contains(value));
        assert!(function.normalized_tokens.iter().any(|token| token == value));
    }
}

fn function_symbols(analysis: &FileAnalysis) -> BTreeSet<&str> {
    analysis
        .functions
        .iter()
        .map(|function| function.stable_symbol.as_str())
        .collect()
}

fn function_names(analysis: &FileAnalysis) -> impl Iterator<Item = &str> {
    analysis.functions.iter().map(|function| function.name.as_str())
}

fn assert_symbol_prefixes(symbols: &BTreeSet<&str>, encoded: &str) {
    for prefix in encoded.split('|') {
        assert!(symbols.iter().any(|symbol| symbol.starts_with(prefix)));
    }
}

fn function_visibilities(analysis: &FileAnalysis) -> BTreeMap<&str, SymbolVisibility> {
    analysis
        .functions
        .iter()
        .map(|function| (function.name.as_str(), function.visibility))
        .collect()
}

fn temporary_fixture_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("reporigor-tree-sitter-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap_or_else(|error| panic!("fixture directory: {error}"));
    root
}

fn permissive_request(root: &Path) -> AnalysisRequest {
    let mut request = AnalysisRequest::new(root.to_path_buf());
    request.allow_parse_errors = true;
    request
}

#[derive(Clone, Copy)]
enum PythonFixturePair {
    Locals,
    Scopes,
}

fn equivalent_python_functions(pair: PythonFixturePair) -> (FunctionRecord, FunctionRecord) {
    let (left, right) = match pair {
        PythonFixturePair::Locals => ("python_locals.py", "python_locals_renamed.py"),
        PythonFixturePair::Scopes => ("python_scopes.py", "python_scopes_renamed.py"),
    };
    equivalent_functions(Language::Python, left, right)
}

#[test]
fn declares_every_generic_capability_and_language() {
    let backend = TreeSitterBackend::new();
    for language in Language::ALL {
        assert!(backend.supports(language), "missing support for {language}");
    }

    let info = backend.info();
    assert_eq!(info.id, "tree-sitter-generic");
    assert!(!info.native);
    for capability in TreeSitterBackend::CAPABILITIES {
        assert!(info.capabilities.contains(capability));
    }
    assert!(!info.capabilities.contains(Capability::ProjectSemantics));
}

#[test]
fn adapter_implementation_keeps_every_function_at_crap_safe_complexity() {
    let root = crate_root();
    let source = source_file(PathBuf::from("src/lib.rs"), "src/lib.rs", Language::Rust);
    let analysis = analyze_requested(&root, &source, &AnalysisRequest::new(root.clone()));
    let failures = analysis
        .functions
        .iter()
        .filter(|function| function.complexity > 6)
        .map(|function| format!("{}={}", function.stable_symbol, function.complexity))
        .collect::<Vec<_>>();
    assert!(failures.is_empty(), "CRAP-safe complexity failures: {failures:?}");
}

#[test]
fn pinned_grammars_analyze_all_languages_and_tsx() {
    for fixture in FIXTURES {
        let analysis = analyze(*fixture);
        assert_eq!(analysis.parse_errors, 0, "{} parse errors", fixture.filename);
        assert!(
            analysis.diagnostics.is_empty(),
            "{} diagnostics",
            fixture.filename
        );
        assert_eq!(
            analysis.functions.len(),
            expected_function_count(fixture.filename),
            "{} functions",
            fixture.filename
        );
        assert!(!analysis.tokens.is_empty(), "{} tokens", fixture.filename);
        assert!(!analysis.mutations.is_empty(), "{} mutations", fixture.filename);
        assert!(
            analysis
                .tokens
                .iter()
                .enumerate()
                .all(|(index, token)| token.index == index),
            "{} token indexes",
            fixture.filename
        );
    }
}

#[test]
fn function_names_and_complexity_follow_the_canonical_profiles() {
    let expectations: &[(Fixture, &[&str])] = &[
        (fixture(Language::Bash, "sample.sh"), &["greet", "<script>"]),
        (fixture(Language::C, "sample.c"), &["classify"]),
        (fixture(Language::Cpp, "sample.cpp"), &["compute"]),
        (fixture(Language::ObjectiveC, "sample.m"), &["isValid:"]),
        (fixture(Language::Python, "sample.py"), &["choose"]),
        (fixture(Language::Rust, "sample.rs"), &["Thing::choose"]),
        (fixture(Language::Swift, "sample.swift"), &["Thing.choose"]),
        (
            fixture(Language::TypeScript, "sample.ts"),
            &["Thing.choose", "positive"],
        ),
        (fixture(Language::TypeScript, "sample.tsx"), &["View"]),
    ];

    for (fixture, expected_names) in expectations {
        let analysis = analyze(*fixture);
        let names = function_names(&analysis).collect::<Vec<_>>();
        assert_eq!(
            names.len(),
            expected_names.len(),
            "{}: {names:?}",
            fixture.filename
        );
        for expected in *expected_names {
            assert!(
                names.contains(expected),
                "{} missing {expected:?}; got {names:?}",
                fixture.filename
            );
        }
        assert!(
            analysis.functions.iter().all(|function| function.complexity >= 1),
            "{} has zero complexity",
            fixture.filename
        );
    }

    let python = analyze(fixture(Language::Python, "sample.py"));
    assert_eq!(python.functions[0].complexity, 3);
}

#[test]
fn structural_records_are_line_stable_local_aware_and_overload_safe() {
    let before = analyze_named(Language::Cpp, "structural.cpp");
    let after = analyze_named(Language::Cpp, "structural_shifted.cpp");
    assert_eq!(before.functions.len(), 2);
    assert_eq!(after.functions.len(), 2);

    let mut before = before.functions;
    let mut after = after.functions;
    before.sort_by(|left, right| left.stable_symbol.cmp(&right.stable_symbol));
    after.sort_by(|left, right| left.stable_symbol.cmp(&right.stable_symbol));

    assert_eq!(
        stable_symbols(&before),
        stable_symbols(&after),
        "line movement, local renames, and literal changes are not symbol evidence"
    );
    assert_ne!(before[0].stable_symbol, before[1].stable_symbol);

    for (original, shifted) in before.iter().zip(&after) {
        assert!(original.structural_metrics_reliable);
        assert_eq!(original.parameter_count, 1);
        assert!(original.statement_count >= 2);
        assert_eq!(original.normalized_tokens, shifted.normalized_tokens);
        assert!(original.normalized_tokens.iter().any(|token| token == "LOCAL"));
        assert!(original.normalized_tokens.iter().any(|token| token == "LITERAL"));
        assert!(!original.normalized_tokens.iter().any(|token| {
            matches!(
                token.as_str(),
                "value" | "adjusted" | "amount" | "scaled" | "7" | "3.5"
            )
        }));
    }

    let nested = before
        .iter()
        .find(|function| function.references.contains("helper"))
        .unwrap_or_else(|| panic!("the int overload should retain its non-local helper reference"));
    assert!(nested.nesting_depth >= 2);
    assert!(!nested.references.contains("adjusted"));
}

fn stable_symbols(functions: &[FunctionRecord]) -> Vec<&str> {
    functions
        .iter()
        .map(|function| function.stable_symbol.as_str())
        .collect()
}

#[test]
fn inline_module_owners_keep_duplicate_names_and_raw_identifiers_stable() {
    let before = analyze_named(Language::Rust, "module_owners.rs");
    let changed = analyze_named(Language::Rust, "module_owners_changed.rs");
    let before_symbols = function_symbols(&before);
    let changed_symbols = function_symbols(&changed);
    assert_eq!(before_symbols, changed_symbols);
    assert_symbol_prefixes(&before_symbols, "left::same(|right::same(|raw::r#match(");
    assert!(before_symbols
        .iter()
        .all(|symbol| !symbol.contains('#') || symbol.contains("r#match")));
}

#[test]
fn generic_rust_symbols_include_implemented_trait_ownership() {
    let analysis = analyze_named(Language::Rust, "structural_traits.rs");
    let implementations = analysis
        .functions
        .iter()
        .filter(|function| function.stable_symbol.contains("Value as "))
        .collect::<Vec<_>>();
    assert_eq!(implementations.len(), 2);
    assert!(implementations
        .iter()
        .any(|function| function.stable_symbol.contains("Value as Left::same")));
    assert!(implementations
        .iter()
        .any(|function| function.stable_symbol.contains("Value as Right::same")));
    assert_ne!(implementations[0].stable_symbol, implementations[1].stable_symbol);
}

#[test]
fn python_and_objective_c_method_symbols_include_semantic_owners() {
    for (language, fixture, expected) in [
        (
            Language::Python,
            "structural_owners.py",
            ["Left.same", "Right.same"],
        ),
        (
            Language::ObjectiveC,
            "structural_owners.m",
            ["Left.same:", "Right.same:"],
        ),
    ] {
        let analysis = analyze_named(language, fixture);
        assert_eq!(analysis.functions.len(), 2, "{fixture}");
        for owner in expected {
            assert!(
                analysis
                    .functions
                    .iter()
                    .any(|function| function.stable_symbol.starts_with(owner)),
                "{fixture} missing owner-qualified {owner:?}: {:?}",
                analysis
                    .functions
                    .iter()
                    .map(|function| &function.stable_symbol)
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn objective_c_unary_and_keyword_selectors_are_extracted_exactly() {
    let analysis = analyze_named(Language::ObjectiveC, "objective_c_selectors.m");
    let names = function_names(&analysis).collect::<BTreeSet<_>>();
    assert_eq!(names, BTreeSet::from(["reset", "setValue:forKey:"]));
}

#[test]
fn language_visibility_profiles_distinguish_explicit_and_default_access() {
    let rust = analyze_named(Language::Rust, "visibility.rs");
    let rust_visibility = function_visibilities(&rust);
    assert_eq!(
        rust_visibility.get("crate_visible"),
        Some(&SymbolVisibility::Crate)
    );
    assert_eq!(
        rust_visibility.get("public_visible"),
        Some(&SymbolVisibility::Public)
    );
    assert_eq!(
        rust_visibility.get("private_visible"),
        Some(&SymbolVisibility::Private)
    );

    let c = analyze_named(Language::C, "visibility.c");
    let c_visibility = function_visibilities(&c);
    assert_eq!(
        c_visibility.get("private_function"),
        Some(&SymbolVisibility::Private)
    );
    assert_eq!(
        c_visibility.get("package_function"),
        Some(&SymbolVisibility::Unknown)
    );
}

#[test]
fn non_file_and_missing_sources_fail_before_parsing() {
    let root = temporary_fixture_root("source-kind");
    std::fs::create_dir_all(root.join("directory.py"))
        .unwrap_or_else(|error| panic!("fixture directory: {error}"));
    let backend = TreeSitterBackend::new();
    let request = AnalysisRequest::new(root.clone());
    for path in ["directory.py", "missing.py"] {
        let source = source_file(PathBuf::from(path), path, Language::Python);
        assert!(backend.analyze_file(&root, &source, &request).is_err(), "{path}");
    }
    std::fs::remove_dir_all(root).unwrap_or_else(|error| panic!("fixture cleanup: {error}"));
}

#[test]
fn bash_words_and_plain_c_declarations_normalize_as_literals_and_locals() {
    let bash = analyze_named(Language::Bash, "normalization.sh");
    let tokens = &bash.functions[0].normalized_tokens;
    assert!(
        tokens.iter().any(|token| token == "LOCAL"),
        "Bash local was not normalized: {tokens:?}"
    );
    assert!(tokens.iter().any(|token| token == "LITERAL"));
    assert!(!tokens
        .iter()
        .any(|token| matches!(token.as_str(), "amount" | "42" | "ready")));
    let file_tokens = token_values(&bash);
    assert!(file_tokens.contains(&"LITERAL"));
    assert!(!file_tokens
        .iter()
        .any(|token| matches!(*token, "42" | "ready" | "true" | "false")));

    let cpp = analyze_named(Language::Cpp, "structural.cpp");
    assert!(!cpp
        .functions
        .iter()
        .any(|function| { function.normalized_tokens.iter().any(|token| token == "scratch") }));
}

#[test]
fn generic_references_include_direct_calls_and_shared_nonlocals() {
    let analysis = analyze_named(Language::Python, "cohesion.py");
    let helper = named_function(&analysis, "helper");
    let caller = named_function(&analysis, "caller");
    assert!(helper.references.contains("SHARED"));
    assert!(caller.references.contains("SHARED"));
    assert!(caller.references.contains("helper"));
}

#[test]
fn python_framework_and_nested_bindings_normalize_as_locals() {
    let (first, second) = equivalent_python_functions(PythonFixturePair::Locals);
    assert!(first.normalized_tokens.iter().any(|token| token == "LOCAL"));
    assert_values_absent(
        &[&first, &second],
        "stream_name|error_name|imported_helper|nested_name|selected|item_name",
        false,
    );
}

#[test]
fn python_comprehension_and_lambda_bindings_stay_in_their_executable_scopes() {
    let (first, second) = equivalent_python_functions(PythonFixturePair::Scopes);
    assert!(first.references.contains("item"));
    assert!(first.references.contains("predicate"));
    assert_values_absent(
        &[&first, &second],
        "lambda_value|nested_dependency|guard|fallback|renamed_value|deep_dependency|first_guard|second_guard|alternate_fallback",
        true,
    );
    assert_eq!(first.complexity, second.complexity);
    assert_eq!(first.nesting_depth, second.nesting_depth);
    assert_eq!(first.statement_count, second.statement_count);
    assert_eq!(first.coverage_excluded_ranges, vec![(3, 3)]);
    assert_eq!(second.coverage_excluded_ranges, vec![(3, 3)]);
}

#[test]
fn c_prototype_parameters_do_not_leak_beyond_their_declarators() {
    let (first, second) = equivalent_functions(
        Language::C,
        "c_declarator_scopes.c",
        "c_declarator_scopes_renamed.c",
    );
    assert_values_present(&first, "dependency|parameter|nested");
    assert_values_absent(
        &[&first, &second],
        "prototype|callback|first|second|renamed_prototype|renamed_dependency|renamed_callback|renamed_parameter|runner|renamed_runner|renamed_nested|alpha|beta",
        false,
    );
}

#[test]
fn bash_shebang_and_comments_do_not_create_a_script_function() {
    let analysis = analyze_named(Language::Bash, "functions_only.sh");
    assert_eq!(analysis.functions.len(), 1);
    assert_eq!(analysis.functions[0].name, "choose");
}

#[test]
fn rust_closures_local_functions_and_cpp_lambdas_are_unreported_complexity_boundaries() {
    for fixture in [
        fixture(Language::Cpp, "sample.cpp"),
        fixture(Language::Rust, "sample.rs"),
    ] {
        let analysis = analyze(fixture);
        assert_eq!(analysis.functions.len(), 1, "{}", fixture.filename);
        assert_eq!(
            analysis.functions[0].complexity, 3,
            "{} charged a nested executable body's decisions to its owner",
            fixture.filename
        );
        assert!(
            !analysis.functions[0].name.starts_with('<') && !analysis.functions[0].name.contains("nested"),
            "{} reported an anonymous or local function",
            fixture.filename
        );
    }
}

#[test]
fn normalization_and_mutations_preserve_source_locations() {
    let fixture = fixture(Language::Python, "sample.py");
    let analysis = analyze(fixture);
    let values = token_values(&analysis);
    assert!(values.contains(&"choose"));
    assert!(values.contains(&"LITERAL"));
    assert!(!values.iter().any(|value| matches!(*value, "STR" | "NUM")));
    assert!(!values.iter().any(|value| value.contains("must not become")));

    let originals = mutation_originals(&analysis);
    assert!(originals.contains(&">"));
    assert!(originals.contains(&"and"));
    assert!(originals.contains(&"!="));
    assert!(originals.contains(&"+"));
    assert!(!originals.contains(&"=="));
    assert!(!originals.contains(&"true"));
    assert!(!originals.contains(&"false"));

    let source = std::fs::read(crate_root().join("tests/fixtures/sample.py"))
        .unwrap_or_else(|error| panic!("read fixture: {error}"));
    for mutation in &analysis.mutations {
        assert_eq!(
            &source[mutation.start_byte..mutation.end_byte],
            mutation.original.as_bytes()
        );
        assert!(mutation.line >= 1);
        assert!(mutation.column >= 1);
        assert_eq!(mutation.id, 0, "global aggregation assigns IDs");
    }
}

#[test]
fn filters_comments_literals_markup_and_error_recovery_from_candidates() {
    let rust = analyze(fixture(Language::Rust, "sample.rs"));
    assert!(rust
        .tokens
        .iter()
        .all(|token| !token.value.contains("duplicate marker")));

    let tsx = analyze(fixture(Language::TypeScript, "sample.tsx"));
    let tsx_originals = mutation_originals(&tsx);
    assert_eq!(tsx_originals, ["true", "false"]);

    let root = crate_root();
    let contexts = test_source(Language::TypeScript, "mutation_contexts.ts");
    let clean_contexts = analyze_requested(&root, &contexts, &AnalysisRequest::new(root.clone()));
    assert!(clean_contexts.mutations.is_empty());
    assert!(clean_contexts.tokens.iter().any(|token| token.value == "LITERAL"));

    for context in [
        fixture(Language::Bash, "mutation_contexts.sh"),
        fixture(Language::Cpp, "mutation_contexts.cpp"),
        fixture(Language::Rust, "mutation_contexts.rs"),
    ] {
        let source = fixture_source(context);
        let analysis = analyze_requested(&root, &source, &AnalysisRequest::new(root.clone()));
        assert!(
            analysis.mutations.is_empty(),
            "{} emitted syntax-delimiter mutations: {:?}",
            context.filename,
            analysis.mutations
        );
    }

    let invalid = test_source(Language::Python, "invalid_operator.py");
    let permissive = permissive_request(&root);
    let invalid_analysis = analyze_requested(&root, &invalid, &permissive);
    assert!(invalid_analysis.parse_errors > 0);
    assert!(invalid_analysis.mutations.is_empty());

    let swift = analyze(fixture(Language::Swift, "sample.swift"));
    let swift_originals = mutation_originals(&swift);
    for operator in ["+", "*", ">", "&&", "!=", "||", "=="] {
        assert!(
            swift_originals.contains(&operator),
            "Swift did not expose {operator:?}: {swift_originals:?}"
        );
    }
}

#[test]
fn display_columns_count_unicode_scalars_while_edit_ranges_count_bytes() {
    let root = crate_root();
    let valid = test_source(Language::Python, "unicode.py");
    let analysis = analyze_requested(&root, &valid, &AnalysisRequest::new(root.clone()));
    let plus = analysis
        .mutations
        .iter()
        .find(|mutation| mutation.original == "+")
        .unwrap_or_else(|| panic!("Unicode fixture should expose its binary plus"));
    assert_eq!((plus.line, plus.column), (1, 7));
    assert_eq!((plus.start_byte, plus.end_byte), (8, 9));

    let invalid = test_source(Language::Python, "unicode_invalid.py");
    let permissive = permissive_request(&root);
    let analysis = analyze_requested(&root, &invalid, &permissive);
    let location = analysis
        .diagnostics
        .first()
        .and_then(|diagnostic| diagnostic.location.as_ref())
        .unwrap_or_else(|| panic!("parse diagnostics carry source locations"));
    assert_eq!(
        (
            location.start_line,
            location.start_column,
            location.end_line,
            location.end_column,
        ),
        (1, 11, 1, 13),
        "columns are 1-based Unicode-scalar positions with a half-open end"
    );
    assert!(analysis.mutations.is_empty());
}

#[test]
fn parse_errors_are_strict_by_default_and_structured_when_allowed() {
    let root = crate_root();
    let source = test_source(Language::Python, "invalid.py");
    let backend = TreeSitterBackend::new();
    let strict = backend.analyze_file(&root, &source, &AnalysisRequest::new(root.clone()));
    match strict {
        Err(CoreError::Parse { message, .. }) => {
            assert!(message.contains("line "));
            assert!(message.contains("column "));
        }
        result => panic!("expected strict parse error, got {result:?}"),
    }

    let permissive = permissive_request(&root);
    let analysis = analyze_requested(&root, &source, &permissive);
    assert!(analysis.parse_errors > 0);
    assert_eq!(analysis.parse_errors, analysis.diagnostics.len());
    assert!(analysis.diagnostics.iter().all(|diagnostic| {
        diagnostic.severity == Severity::Error
            && diagnostic.backend == "tree-sitter-generic"
            && diagnostic
                .location
                .as_ref()
                .is_some_and(|location| location.file == "invalid.py" && location.start_line >= 1)
    }));

    let unexpected = test_source(Language::Python, "unexpected.py");
    let deduplicated = analyze_requested(&root, &unexpected, &permissive);
    assert_eq!(deduplicated.parse_errors, 1);
    assert_eq!(deduplicated.diagnostics.len(), 1);

    let recovered = test_source(Language::Python, "partial_invalid.py");
    let recovered = analyze_requested(&root, &recovered, &permissive);
    assert!(recovered.parse_errors > 0);
    assert!(!recovered.functions.is_empty());
    assert!(recovered
        .functions
        .iter()
        .all(|function| !function.structural_metrics_reliable));
}

#[test]
fn rejects_sources_over_the_configured_byte_limit() {
    let fixture = fixture(Language::Python, "sample.py");
    let root = crate_root();
    let source = fixture_source(fixture);
    let path = root.join(&source.path);
    let length = std::fs::metadata(&path)
        .unwrap_or_else(|error| panic!("fixture metadata: {error}"))
        .len();
    let mut request = AnalysisRequest::new(root.clone());
    request.max_source_bytes = usize::try_from(length.saturating_sub(1)).unwrap_or(usize::MAX);

    let result = TreeSitterBackend::new().analyze_file(&root, &source, &request);
    match result {
        Err(CoreError::SourceTooLarge {
            path,
            actual_bytes,
            max_source_bytes,
        }) => {
            assert_eq!(actual_bytes, length);
            assert_eq!(max_source_bytes, length.saturating_sub(1));
            assert!(path.contains(Path::new("sample.py").to_str().unwrap_or_default()));
        }
        result => panic!("expected byte-limit error, got {result:?}"),
    }

    request.max_source_bytes = usize::try_from(length).unwrap_or(usize::MAX);
    let exact = TreeSitterBackend::new().analyze_file(&root, &source, &request);
    assert!(exact.is_ok(), "a source exactly at the limit must be accepted");
}

#[test]
fn rejects_invalid_utf8_before_tree_sitter_or_lossy_normalization() {
    let root = temporary_fixture_root("invalid-utf8");
    let path = root.join("invalid.py");
    std::fs::write(&path, [b'd', b'e', b'f', b' ', 0xff, b'\n'])
        .unwrap_or_else(|error| panic!("fixture source: {error}"));
    let source = source_file(path, "invalid.py", Language::Python);

    for allow_parse_errors in [false, true] {
        let mut request = AnalysisRequest::new(root.clone());
        request.allow_parse_errors = allow_parse_errors;
        let result = TreeSitterBackend::new().analyze_file(&root, &source, &request);
        assert!(matches!(
            result,
            Err(CoreError::InvalidSourceEncoding { valid_up_to: 4, .. })
        ));
    }
    std::fs::remove_dir_all(root).unwrap_or_else(|error| panic!("fixture cleanup: {error}"));
}
