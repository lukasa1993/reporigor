use std::path::{Path, PathBuf};

use adapter_tree_sitter::TreeSitterBackend;
use reporigor_core::{
    AnalysisRequest, Capability, CoreError, FileAnalysis, Language, Severity, SourceFile, SyntaxBackend,
};

#[derive(Clone, Copy)]
struct Fixture {
    language: Language,
    filename: &'static str,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        language: Language::Bash,
        filename: "sample.sh",
    },
    Fixture {
        language: Language::C,
        filename: "sample.c",
    },
    Fixture {
        language: Language::Cpp,
        filename: "sample.cpp",
    },
    Fixture {
        language: Language::ObjectiveC,
        filename: "sample.m",
    },
    Fixture {
        language: Language::Python,
        filename: "sample.py",
    },
    Fixture {
        language: Language::Rust,
        filename: "sample.rs",
    },
    Fixture {
        language: Language::Swift,
        filename: "sample.swift",
    },
    Fixture {
        language: Language::TypeScript,
        filename: "sample.ts",
    },
    Fixture {
        language: Language::TypeScript,
        filename: "sample.tsx",
    },
];

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_source(fixture: Fixture) -> SourceFile {
    SourceFile {
        path: PathBuf::from("tests/fixtures").join(fixture.filename),
        relative: fixture.filename.to_string(),
        language: fixture.language,
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
    TreeSitterBackend::new()
        .analyze_file(
            &root,
            &fixture_source(fixture),
            &AnalysisRequest::new(root.clone()),
        )
        .unwrap_or_else(|error| panic!("{} should analyze: {error}", fixture.filename))
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
    for capability in [
        Capability::Syntax,
        Capability::Functions,
        Capability::Complexity,
        Capability::Tokens,
        Capability::Mutations,
        Capability::ParseValidation,
    ] {
        assert!(info.capabilities.contains(capability));
    }
    assert!(!info.capabilities.contains(Capability::ProjectSemantics));
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
        (
            Fixture {
                language: Language::Bash,
                filename: "sample.sh",
            },
            &["greet", "<script>"],
        ),
        (
            Fixture {
                language: Language::C,
                filename: "sample.c",
            },
            &["classify"],
        ),
        (
            Fixture {
                language: Language::Cpp,
                filename: "sample.cpp",
            },
            &["compute"],
        ),
        (
            Fixture {
                language: Language::ObjectiveC,
                filename: "sample.m",
            },
            &["isValid:"],
        ),
        (
            Fixture {
                language: Language::Python,
                filename: "sample.py",
            },
            &["choose"],
        ),
        (
            Fixture {
                language: Language::Rust,
                filename: "sample.rs",
            },
            &["Thing::choose"],
        ),
        (
            Fixture {
                language: Language::Swift,
                filename: "sample.swift",
            },
            &["Thing.choose"],
        ),
        (
            Fixture {
                language: Language::TypeScript,
                filename: "sample.ts",
            },
            &["Thing.choose", "positive"],
        ),
        (
            Fixture {
                language: Language::TypeScript,
                filename: "sample.tsx",
            },
            &["View"],
        ),
    ];

    for (fixture, expected_names) in expectations {
        let analysis = analyze(*fixture);
        let names = analysis
            .functions
            .iter()
            .map(|function| function.name.as_str())
            .collect::<Vec<_>>();
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

    let python = analyze(Fixture {
        language: Language::Python,
        filename: "sample.py",
    });
    assert_eq!(python.functions[0].complexity, 3);
}

#[test]
fn rust_closures_local_functions_and_cpp_lambdas_are_unreported_complexity_boundaries() {
    for fixture in [
        Fixture {
            language: Language::Cpp,
            filename: "sample.cpp",
        },
        Fixture {
            language: Language::Rust,
            filename: "sample.rs",
        },
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
    let fixture = Fixture {
        language: Language::Python,
        filename: "sample.py",
    };
    let analysis = analyze(fixture);
    let values = analysis
        .tokens
        .iter()
        .map(|token| token.value.as_str())
        .collect::<Vec<_>>();
    assert!(values.contains(&"ID"));
    assert!(values.contains(&"STR"));
    assert!(values.contains(&"NUM"));
    assert!(!values.iter().any(|value| value.contains("must not become")));

    let originals = analysis
        .mutations
        .iter()
        .map(|mutation| mutation.original.as_str())
        .collect::<Vec<_>>();
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
    let rust = analyze(Fixture {
        language: Language::Rust,
        filename: "sample.rs",
    });
    assert!(rust
        .tokens
        .iter()
        .all(|token| !token.value.contains("duplicate marker")));

    let tsx = analyze(Fixture {
        language: Language::TypeScript,
        filename: "sample.tsx",
    });
    let tsx_originals = tsx
        .mutations
        .iter()
        .map(|mutation| mutation.original.as_str())
        .collect::<Vec<_>>();
    assert_eq!(tsx_originals, ["true", "false"]);

    let root = crate_root();
    let contexts = SourceFile {
        path: PathBuf::from("tests/fixtures/mutation_contexts.ts"),
        relative: "mutation_contexts.ts".to_string(),
        language: Language::TypeScript,
        generated: false,
        test: false,
    };
    let clean_contexts = TreeSitterBackend::new()
        .analyze_file(&root, &contexts, &AnalysisRequest::new(root.clone()))
        .unwrap_or_else(|error| panic!("analyze literal contexts: {error}"));
    assert!(clean_contexts.mutations.is_empty());
    assert!(clean_contexts.tokens.iter().any(|token| token.value == "STR"));

    for context in [
        Fixture {
            language: Language::Bash,
            filename: "mutation_contexts.sh",
        },
        Fixture {
            language: Language::Cpp,
            filename: "mutation_contexts.cpp",
        },
        Fixture {
            language: Language::Rust,
            filename: "mutation_contexts.rs",
        },
    ] {
        let source = fixture_source(context);
        let analysis = TreeSitterBackend::new()
            .analyze_file(&root, &source, &AnalysisRequest::new(root.clone()))
            .unwrap_or_else(|error| panic!("analyze {}: {error}", context.filename));
        assert!(
            analysis.mutations.is_empty(),
            "{} emitted syntax-delimiter mutations: {:?}",
            context.filename,
            analysis.mutations
        );
    }

    let invalid = SourceFile {
        path: PathBuf::from("tests/fixtures/invalid_operator.py"),
        relative: "invalid_operator.py".to_string(),
        language: Language::Python,
        generated: false,
        test: false,
    };
    let mut permissive = AnalysisRequest::new(root.clone());
    permissive.allow_parse_errors = true;
    let invalid_analysis = TreeSitterBackend::new()
        .analyze_file(&root, &invalid, &permissive)
        .unwrap_or_else(|error| panic!("analyze recovery tree: {error}"));
    assert!(invalid_analysis.parse_errors > 0);
    assert!(invalid_analysis.mutations.is_empty());

    let swift = analyze(Fixture {
        language: Language::Swift,
        filename: "sample.swift",
    });
    let swift_originals = swift
        .mutations
        .iter()
        .map(|mutation| mutation.original.as_str())
        .collect::<Vec<_>>();
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
    let backend = TreeSitterBackend::new();
    let valid = SourceFile {
        path: PathBuf::from("tests/fixtures/unicode.py"),
        relative: "unicode.py".to_string(),
        language: Language::Python,
        generated: false,
        test: false,
    };
    let analysis = backend
        .analyze_file(&root, &valid, &AnalysisRequest::new(root.clone()))
        .unwrap_or_else(|error| panic!("analyze Unicode fixture: {error}"));
    let plus = analysis
        .mutations
        .iter()
        .find(|mutation| mutation.original == "+")
        .unwrap_or_else(|| panic!("Unicode fixture should expose its binary plus"));
    assert_eq!((plus.line, plus.column), (1, 7));
    assert_eq!((plus.start_byte, plus.end_byte), (8, 9));

    let invalid = SourceFile {
        path: PathBuf::from("tests/fixtures/unicode_invalid.py"),
        relative: "unicode_invalid.py".to_string(),
        language: Language::Python,
        generated: false,
        test: false,
    };
    let mut permissive = AnalysisRequest::new(root.clone());
    permissive.allow_parse_errors = true;
    let analysis = backend
        .analyze_file(&root, &invalid, &permissive)
        .unwrap_or_else(|error| panic!("analyze invalid Unicode fixture: {error}"));
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
    let source = SourceFile {
        path: PathBuf::from("tests/fixtures/invalid.py"),
        relative: "invalid.py".to_string(),
        language: Language::Python,
        generated: false,
        test: false,
    };
    let backend = TreeSitterBackend::new();
    let strict = backend.analyze_file(&root, &source, &AnalysisRequest::new(root.clone()));
    match strict {
        Err(CoreError::Parse { message, .. }) => {
            assert!(message.contains("line "));
            assert!(message.contains("column "));
        }
        result => panic!("expected strict parse error, got {result:?}"),
    }

    let mut permissive = AnalysisRequest::new(root.clone());
    permissive.allow_parse_errors = true;
    let analysis = backend
        .analyze_file(&root, &source, &permissive)
        .unwrap_or_else(|error| panic!("permissive parse should return diagnostics: {error}"));
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

    let unexpected = SourceFile {
        path: PathBuf::from("tests/fixtures/unexpected.py"),
        relative: "unexpected.py".to_string(),
        language: Language::Python,
        generated: false,
        test: false,
    };
    let deduplicated = backend
        .analyze_file(&root, &unexpected, &permissive)
        .unwrap_or_else(|error| panic!("analyze unexpected token: {error}"));
    assert_eq!(deduplicated.parse_errors, 1);
    assert_eq!(deduplicated.diagnostics.len(), 1);
}

#[test]
fn rejects_sources_over_the_configured_byte_limit() {
    let fixture = Fixture {
        language: Language::Python,
        filename: "sample.py",
    };
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
    let root = std::env::temp_dir().join(format!(
        "reporigor-tree-sitter-invalid-utf8-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap_or_else(|error| panic!("fixture directory: {error}"));
    let path = root.join("invalid.py");
    std::fs::write(&path, [b'd', b'e', b'f', b' ', 0xff, b'\n'])
        .unwrap_or_else(|error| panic!("fixture source: {error}"));
    let source = SourceFile {
        path,
        relative: "invalid.py".to_string(),
        language: Language::Python,
        generated: false,
        test: false,
    };

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
