use std::error::Error;

#[cfg(unix)]
use provider_mutation::import_path;
use provider_mutation::{import_json, ImportFormat, MutationProvider, ProviderError};
use reporigor_core::MutationStatus;

#[test]
fn imports_stryker_mte_v2_and_normalizes_every_status() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let statuses = [
        "Killed",
        "Survived",
        "NoCoverage",
        "CompileError",
        "RuntimeError",
        "Timeout",
        "Ignored",
        "Pending",
    ];
    let mutants = statuses
        .iter()
        .enumerate()
        .map(|(index, status)| {
            serde_json::json!({
                "id": format!("stryker-{index}"),
                "mutatorName": "EqualityOperator",
                "description": "replace strict equality",
                "replacement": format!("!={index}"),
                "location": {
                    "start": { "line": 1, "column": 28 },
                    "end": { "line": 1, "column": 31 }
                },
                "status": status,
                "statusReason": if index == 0 { Some("test failed") } else { None },
                "duration": 125.0
            })
        })
        .collect::<Vec<_>>();
    let report = serde_json::json!({
        "schemaVersion": "2.0",
        "thresholds": { "high": 80, "low": 60 },
        "projectRoot": root.path(),
        "files": {
            "src/example.ts": {
                "language": "typescript",
                "source": "export const answer = left === right;\n",
                "mutants": mutants
            }
        },
        "framework": { "name": "StrykerJS", "version": "10.0.0" }
    });

    let imported = import_json(root.path(), MutationProvider::Stryker, &report.to_string())?;
    assert_eq!(imported.format, ImportFormat::MutationTestingElementsV2);
    assert_eq!(imported.framework_name.as_deref(), Some("StrykerJS"));
    assert_eq!(imported.framework_version.as_deref(), Some("10.0.0"));
    assert_eq!(imported.results.len(), 8);
    let expected = [
        MutationStatus::Killed,
        MutationStatus::Survived,
        MutationStatus::NoCoverage,
        MutationStatus::CompileError,
        MutationStatus::RuntimeError,
        MutationStatus::Timeout,
        MutationStatus::Ignored,
        MutationStatus::Pending,
    ];
    for expected_status in expected {
        assert!(imported
            .results
            .iter()
            .any(|mutation| mutation.result.status == expected_status));
    }
    let first = &imported.results[0].result;
    assert_eq!(first.mutation.file, "src/example.ts");
    assert_eq!(first.mutation.original, "===");
    assert_eq!(first.mutation.line, 1);
    assert_eq!(first.mutation.column, 28);
    assert!(first.duration_seconds >= 0.125);
    assert_eq!(first.detail.as_deref(), Some("test failed"));
    Ok(())
}

#[test]
fn stryker_mte_uses_utf16_columns_and_javascript_line_terminators() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let source = "🚀===x\r\n🚀!==y\u{2028}🚀==z\u{2029}🚀!=w";
    let mutants = [
        ("one", 1, 3, 6, "==="),
        ("two", 2, 3, 6, "!=="),
        ("three", 3, 3, 5, "=="),
        ("four", 4, 3, 5, "!="),
    ]
    .into_iter()
    .map(|(id, line, start, end, original)| {
        serde_json::json!({
            "id": id,
            "mutatorName": "EqualityOperator",
            "replacement": if original.starts_with('!') { "==" } else { "!=" },
            "location": {
                "start": { "line": line, "column": start },
                "end": { "line": line, "column": end }
            },
            "status": "Killed"
        })
    })
    .collect::<Vec<_>>();
    let report = serde_json::json!({
        "schemaVersion": "2.0",
        "thresholds": { "high": 80, "low": 60 },
        "files": {
            "src/example.ts": {
                "language": "typescript",
                "source": source,
                "mutants": mutants
            }
        },
        "framework": { "name": "StrykerJS", "version": "10.0.0" }
    });

    let imported = import_json(root.path(), MutationProvider::Stryker, &report.to_string())?;
    let originals = imported
        .results
        .iter()
        .map(|result| result.result.mutation.original.as_str())
        .collect::<Vec<_>>();
    assert_eq!(originals, ["===", "!==", "==", "!="]);
    for (index, mutation) in imported.results.iter().enumerate() {
        assert_eq!(mutation.result.mutation.line, u32::try_from(index)? + 1);
        assert_eq!(mutation.result.mutation.column, 2);
    }
    Ok(())
}

#[test]
fn generic_mte_columns_remain_one_based_unicode_scalars_and_end_exclusive() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let report = serde_json::json!({
        "schemaVersion": "2.0",
        "thresholds": { "high": 80, "low": 60 },
        "files": {
            "src/example.py": {
                "language": "python",
                "source": "🚀===x\n",
                "mutants": [{
                    "id": "one",
                    "mutatorName": "EqualityOperator",
                    "replacement": "!=",
                    "location": {
                        "start": { "line": 1, "column": 2 },
                        "end": { "line": 1, "column": 5 }
                    },
                    "status": "Killed"
                }]
            }
        },
        "framework": { "name": "another-provider", "version": "1.0" }
    });

    let imported = import_json(root.path(), MutationProvider::Mutmut, &report.to_string())?;
    let mutation = &imported.results[0].result.mutation;
    assert_eq!(mutation.original, "===");
    assert_eq!(mutation.start_byte, 4);
    assert_eq!(mutation.end_byte, 7);
    assert_eq!(mutation.column, 2);
    Ok(())
}

#[test]
fn imports_current_cargo_mutants_outcomes_shape() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir_all(root.path().join("src"))?;
    std::fs::write(
        root.path().join("src/lib.rs"),
        "pub fn answer() -> bool {\n    true\n}\n",
    )?;
    let report = serde_json::json!({
        "outcomes": [
            {
                "scenario": "Baseline",
                "summary": "Success",
                "phase_results": []
            },
            {
                "scenario": {
                    "Mutant": {
                        "name": "src/lib.rs:2:5: replace true with false",
                        "package": "demo",
                        "file": "src/lib.rs",
                        "function": null,
                        "span": {
                            "start": { "line": 2, "column": 5 },
                            "end": { "line": 2, "column": 9 }
                        },
                        "replacement": "false",
                        "genre": "FnValue"
                    }
                },
                "summary": "CaughtMutant",
                "log_path": "logs/mutant.log",
                "diff_path": "diff/mutant.diff",
                "phase_results": [
                    { "phase": "Build", "duration": 0.5, "process_status": "Success", "argv": ["cargo", "build"] },
                    { "phase": "Test", "duration": 1.25, "process_status": { "Failure": 101 }, "argv": ["cargo", "test"] }
                ]
            }
        ],
        "total_mutants": 1,
        "caught": 1,
        "missed": 0,
        "timeout": 0,
        "unviable": 0,
        "success": 0,
        "start_time": "2026-08-27T10:00:00Z",
        "end_time": "2026-08-27T10:01:00Z",
        "cargo_mutants_version": "27.1.0"
    });

    let imported = import_json(root.path(), MutationProvider::CargoMutants, &report.to_string())?;
    assert_eq!(imported.format, ImportFormat::CargoMutantsOutcomes);
    assert_eq!(imported.framework_version.as_deref(), Some("27.1.0"));
    assert_eq!(imported.results.len(), 1);
    let result = &imported.results[0].result;
    assert_eq!(result.status, MutationStatus::Killed);
    assert_eq!(result.mutation.original, "true");
    assert_eq!(result.mutation.replacement, "false");
    assert!((result.duration_seconds - 1.75).abs() < f64::EPSILON);
    assert_eq!(result.mutation.file, "src/lib.rs");
    assert_eq!(imported.warnings.len(), 1);
    Ok(())
}

#[test]
fn imports_mull_elements_v1_through_the_common_model() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let report = serde_json::json!({
        "schemaVersion": "1.7",
        "thresholds": { "high": 80, "low": 60 },
        "files": {
            "src/example.cpp": {
                "language": "cpp",
                "source": "bool same(int a, int b) { return a == b; }\n",
                "mutants": [{
                    "id": "mull-1",
                    "mutatorName": "cxx_eq_to_ne",
                    "replacement": "!=",
                    "location": {
                        "start": { "line": 1, "column": 36 },
                        "end": { "line": 1, "column": 38 }
                    },
                    "status": "Killed"
                }]
            }
        },
        "framework": { "name": "Mull", "version": "0.26" }
    });
    let imported = import_json(root.path(), MutationProvider::Mull, &report.to_string())?;
    assert_eq!(imported.format, ImportFormat::MutationTestingElementsV1);
    assert_eq!(imported.results.len(), 1);
    assert_eq!(imported.results[0].result.status, MutationStatus::Killed);
    Ok(())
}

#[test]
fn imports_muter_json_only_when_basename_is_unambiguous() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir_all(root.path().join("Sources/App"))?;
    std::fs::write(
        root.path().join("Sources/App/main.swift"),
        "func run() {\n    block()\n}\n",
    )?;
    let report = serde_json::json!({
        "fileReports": [{
            "fileName": "main.swift",
            "mutationScore": 100,
            "appliedOperators": [{
                "mutationPoint": {
                    "mutationOperatorId": "RemoveSideEffects",
                    "position": { "utf8Offset": 17, "line": 2, "column": 5 }
                },
                "mutationSnapshot": {
                    "before": "block()",
                    "after": "removed line",
                    "description": "removed line"
                },
                "testSuiteOutcome": "runtimeError"
            }]
        }],
        "globalMutationScore": 100,
        "numberOfKilledMutants": 1,
        "totalAppliedMutationOperators": 1
    });

    let imported = import_json(root.path(), MutationProvider::Muter, &report.to_string())?;
    assert_eq!(imported.format, ImportFormat::MuterJson);
    assert_eq!(imported.results.len(), 1);
    let mutation = &imported.results[0].result;
    assert_eq!(mutation.status, MutationStatus::Killed);
    assert_eq!(mutation.mutation.file, "Sources/App/main.swift");
    assert_eq!(mutation.mutation.original, "block()");
    assert_eq!(mutation.mutation.replacement, "removed line");
    assert_eq!(imported.warnings.len(), 1);
    Ok(())
}

#[test]
fn rejects_ambiguous_muter_basenames() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir_all(root.path().join("Sources/One"))?;
    std::fs::create_dir_all(root.path().join("Sources/Two"))?;
    std::fs::write(root.path().join("Sources/One/main.swift"), "let a = 1\n")?;
    std::fs::write(root.path().join("Sources/Two/main.swift"), "let b = 2\n")?;
    let report = serde_json::json!({
        "fileReports": [{
            "fileName": "main.swift",
            "mutationScore": 0,
            "appliedOperators": []
        }]
    });
    let error = import_json(root.path(), MutationProvider::Muter, &report.to_string());
    assert!(matches!(error, Err(ProviderError::InvalidReport { .. })));
    Ok(())
}

#[test]
fn rejects_unknown_status_and_paths_outside_root() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let report = |file: &str, status: &str| {
        serde_json::json!({
            "schemaVersion": "2.0",
            "thresholds": { "high": 80, "low": 60 },
            "files": {
                file: {
                    "language": "python",
                    "source": "value = True\n",
                    "mutants": [{
                        "id": "one",
                        "mutatorName": "BooleanLiteral",
                        "replacement": "False",
                        "location": {
                            "start": { "line": 1, "column": 8 },
                            "end": { "line": 1, "column": 12 }
                        },
                        "status": status
                    }]
                }
            }
        })
    };

    let status_error = import_json(
        root.path(),
        MutationProvider::Mutmut,
        &report("src/example.py", "Maybe").to_string(),
    );
    assert!(matches!(status_error, Err(ProviderError::InvalidReport { .. })));

    let path_error = import_json(
        root.path(),
        MutationProvider::Mutmut,
        &report("../outside.py", "Killed").to_string(),
    );
    assert!(matches!(path_error, Err(ProviderError::InvalidReport { .. })));
    Ok(())
}

#[test]
fn rejects_malformed_mte_versions_and_invalid_thresholds() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    for version in ["2.garbage", "2.", "2.0.0.0", "02.0"] {
        let mut report = minimal_mte_report("src/example.py");
        report["schemaVersion"] = serde_json::json!(version);
        let error = import_json(root.path(), MutationProvider::Mutmut, &report.to_string());
        assert!(
            matches!(error, Err(ProviderError::InvalidReport { ref field, .. }) if field == "schemaVersion"),
            "version {version:?} was not rejected as an invalid schemaVersion: {error:?}"
        );
    }

    for (low, high) in [(81, 80), (0, 101)] {
        let mut report = minimal_mte_report("src/example.py");
        report["thresholds"] = serde_json::json!({ "low": low, "high": high });
        let error = import_json(root.path(), MutationProvider::Mutmut, &report.to_string());
        assert!(
            matches!(error, Err(ProviderError::InvalidReport { ref field, .. }) if field == "thresholds"),
            "thresholds {low}/{high} were not rejected: {error:?}"
        );
    }
    Ok(())
}

#[test]
fn requires_mte_thresholds_language_and_nonempty_mutator_name() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;

    let mut without_thresholds = minimal_mte_report("src/example.py");
    without_thresholds
        .as_object_mut()
        .ok_or("report must be an object")?
        .remove("thresholds");
    assert!(matches!(
        import_json(
            root.path(),
            MutationProvider::Mutmut,
            &without_thresholds.to_string()
        ),
        Err(ProviderError::Json(_))
    ));

    let mut without_language = minimal_mte_report("src/example.py");
    without_language["files"]["src/example.py"]
        .as_object_mut()
        .ok_or("file result must be an object")?
        .remove("language");
    assert!(matches!(
        import_json(
            root.path(),
            MutationProvider::Mutmut,
            &without_language.to_string()
        ),
        Err(ProviderError::Json(_))
    ));

    let mut without_mutator = minimal_mte_report("src/example.py");
    without_mutator["files"]["src/example.py"]["mutants"][0]
        .as_object_mut()
        .ok_or("mutant must be an object")?
        .remove("mutatorName");
    assert!(matches!(
        import_json(
            root.path(),
            MutationProvider::Mutmut,
            &without_mutator.to_string()
        ),
        Err(ProviderError::Json(_))
    ));

    let mut empty_mutator = minimal_mte_report("src/example.py");
    empty_mutator["files"]["src/example.py"]["mutants"][0]["mutatorName"] = serde_json::json!("  ");
    assert!(matches!(
        import_json(root.path(), MutationProvider::Mutmut, &empty_mutator.to_string()),
        Err(ProviderError::InvalidReport { .. })
    ));
    Ok(())
}

#[test]
fn rejects_duplicate_upstream_ids_and_effective_candidates() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;

    let mut duplicate_id = minimal_mte_report("src/example.py");
    let mut second = duplicate_id["files"]["src/example.py"]["mutants"][0].clone();
    second["replacement"] = serde_json::json!("None");
    duplicate_id["files"]["src/example.py"]["mutants"]
        .as_array_mut()
        .ok_or("mutants must be an array")?
        .push(second);
    let error = import_json(root.path(), MutationProvider::Mutmut, &duplicate_id.to_string());
    assert!(matches!(
        error,
        Err(ProviderError::InvalidReport { ref field, .. }) if field == "files.*.mutants[].id"
    ));

    let mut duplicate_candidate = minimal_mte_report("src/example.py");
    let mut second = duplicate_candidate["files"]["src/example.py"]["mutants"][0].clone();
    second["id"] = serde_json::json!("two");
    duplicate_candidate["files"]["src/example.py"]["mutants"]
        .as_array_mut()
        .ok_or("mutants must be an array")?
        .push(second);
    let error = import_json(
        root.path(),
        MutationProvider::Mutmut,
        &duplicate_candidate.to_string(),
    );
    assert!(matches!(
        error,
        Err(ProviderError::InvalidReport { ref message, .. }) if message == "duplicate effective mutation candidate"
    ));
    Ok(())
}

#[test]
fn enforces_report_source_file_and_per_mutant_byte_budgets() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;

    let oversized_report = " ".repeat(32 * 1024 * 1024 + 1);
    let error = import_json(root.path(), MutationProvider::Mutmut, &oversized_report);
    assert!(matches!(
        error,
        Err(ProviderError::InvalidReport { ref field, .. }) if field == "limits.reportBytes"
    ));

    let mut files = serde_json::Map::new();
    for index in 0..=1_024 {
        files.insert(
            format!("src/{index}.py"),
            serde_json::json!({ "language": "python", "source": "", "mutants": [] }),
        );
    }
    let too_many_files = serde_json::json!({
        "schemaVersion": "2.0",
        "thresholds": { "high": 80, "low": 60 },
        "files": files
    });
    let error = import_json(root.path(), MutationProvider::Mutmut, &too_many_files.to_string());
    assert!(matches!(
        error,
        Err(ProviderError::InvalidReport { ref field, .. }) if field == "limits.sourceFiles"
    ));

    let mut oversized_replacement = minimal_mte_report("src/example.py");
    oversized_replacement["files"]["src/example.py"]["mutants"][0]["replacement"] =
        serde_json::json!("x".repeat(1024 * 1024 + 1));
    let error = import_json(
        root.path(),
        MutationProvider::Mutmut,
        &oversized_replacement.to_string(),
    );
    assert!(matches!(
        error,
        Err(ProviderError::InvalidReport { ref field, .. }) if field == "limits.replacementBytesPerMutant"
    ));
    Ok(())
}

#[test]
fn bounded_cargo_source_read_rejects_sparse_oversized_files() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir_all(root.path().join("src"))?;
    let source_path = root.path().join("src/lib.rs");
    let source = std::fs::File::create(&source_path)?;
    source.set_len(16 * 1024 * 1024 + 1)?;
    let report = serde_json::json!({
        "outcomes": [{
            "scenario": { "Mutant": {
                "name": "src/lib.rs:1:1: replace value",
                "file": "src/lib.rs",
                "span": {
                    "start": { "line": 1, "column": 1 },
                    "end": { "line": 1, "column": 1 }
                },
                "replacement": "false"
            }},
            "summary": "CaughtMutant",
            "phase_results": []
        }],
        "end_time": "2026-08-27T10:01:00Z",
        "cargo_mutants_version": "27.1.0"
    });

    let error = import_json(root.path(), MutationProvider::CargoMutants, &report.to_string());
    assert!(matches!(error, Err(ProviderError::InvalidReport { .. })));
    Ok(())
}

#[test]
fn rejects_zero_mte_and_cargo_mutants_coordinates() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let mut mte = minimal_mte_report("src/example.py");
    mte["files"]["src/example.py"]["mutants"][0]["location"]["start"]["line"] = serde_json::json!(0);
    assert!(matches!(
        import_json(root.path(), MutationProvider::Mutmut, &mte.to_string()),
        Err(ProviderError::InvalidReport { .. })
    ));

    std::fs::create_dir_all(root.path().join("src"))?;
    std::fs::write(root.path().join("src/lib.rs"), "true\n")?;
    let cargo = serde_json::json!({
        "outcomes": [{
            "scenario": { "Mutant": {
                "file": "src/lib.rs",
                "span": {
                    "start": { "line": 1, "column": 0 },
                    "end": { "line": 1, "column": 5 }
                },
                "replacement": "false"
            }},
            "summary": "CaughtMutant",
            "phase_results": []
        }],
        "end_time": "2026-08-27T10:01:00Z",
        "cargo_mutants_version": "27.1.0"
    });
    assert!(matches!(
        import_json(root.path(), MutationProvider::CargoMutants, &cargo.to_string()),
        Err(ProviderError::InvalidReport { .. })
    ));
    Ok(())
}

#[test]
fn muter_skips_no_coverage_sentinel_instead_of_inventing_a_mutant() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir_all(root.path().join("Sources/App"))?;
    std::fs::write(root.path().join("Sources/App/main.swift"), "let value = true\n")?;
    let report = serde_json::json!({
        "fileReports": [{
            "fileName": "main.swift",
            "mutationScore": 0,
            "appliedOperators": [{
                "mutationPoint": null,
                "mutationSnapshot": null,
                "testSuiteOutcome": "noCoverage"
            }]
        }]
    });

    let imported = import_json(root.path(), MutationProvider::Muter, &report.to_string())?;
    assert!(imported.results.is_empty());

    let mut malformed = report;
    malformed["fileReports"][0]["appliedOperators"][0]["mutationPoint"] = serde_json::json!({
        "mutationOperatorId": "RemoveSideEffects",
        "position": { "utf8Offset": 0, "line": 0, "column": 0 }
    });
    assert!(matches!(
        import_json(root.path(), MutationProvider::Muter, &malformed.to_string()),
        Err(ProviderError::InvalidReport { .. })
    ));
    Ok(())
}

#[test]
fn muter_uses_validated_utf8_offset_and_exports_scalar_column() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir_all(root.path().join("Sources/App"))?;
    std::fs::write(root.path().join("Sources/App/main.swift"), "let café = true\n")?;
    let report = serde_json::json!({
        "fileReports": [{
            "fileName": "main.swift",
            "mutationScore": 0,
            "appliedOperators": [{
                "mutationPoint": {
                    "mutationOperatorId": "ChangeLogicalConnector",
                    "position": { "utf8Offset": 12, "line": 1, "column": 13 }
                },
                "mutationSnapshot": {
                    "before": "true",
                    "after": "false",
                    "description": "replace true"
                },
                "testSuiteOutcome": "passed"
            }]
        }]
    });

    let imported = import_json(root.path(), MutationProvider::Muter, &report.to_string())?;
    let mutation = &imported.results[0].result.mutation;
    assert_eq!(mutation.original, "true");
    assert_eq!(mutation.start_byte, 12);
    assert_eq!(mutation.end_byte, 16);
    assert_eq!(mutation.line, 1);
    assert_eq!(mutation.column, 12);

    let mut zero_coordinate = report.clone();
    zero_coordinate["fileReports"][0]["appliedOperators"][0]["mutationPoint"]["position"]["line"] =
        serde_json::json!(0);
    assert!(matches!(
        import_json(root.path(), MutationProvider::Muter, &zero_coordinate.to_string()),
        Err(ProviderError::InvalidReport { .. })
    ));

    let mut invalid = report;
    invalid["fileReports"][0]["appliedOperators"][0]["mutationPoint"]["position"]["utf8Offset"] =
        serde_json::json!(8);
    assert!(matches!(
        import_json(root.path(), MutationProvider::Muter, &invalid.to_string()),
        Err(ProviderError::InvalidReport { .. })
    ));
    Ok(())
}

#[test]
fn non_mte_native_json_is_rejected_for_providers_without_stable_importers() {
    let root = std::path::Path::new(".");
    let error = import_json(root, MutationProvider::Muter, "{\"mutants\": []}");
    assert!(matches!(error, Err(ProviderError::UnsupportedReport { .. })));
}

#[cfg(unix)]
#[test]
fn rejects_report_symlinks_and_direct_paths_outside_root() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir()?;
    let external = tempfile::tempdir()?;
    let report = external.path().join("report.json");
    std::fs::write(&report, minimal_mte_report("src/example.py").to_string())?;
    let link = root.path().join("report.json");
    symlink(&report, &link)?;

    let linked = import_path(root.path(), MutationProvider::Mutmut, &link);
    assert!(matches!(linked, Err(ProviderError::InvalidReport { .. })));
    let direct = import_path(root.path(), MutationProvider::Mutmut, &report);
    assert!(matches!(direct, Err(ProviderError::InvalidReport { .. })));
    Ok(())
}

#[cfg(unix)]
#[test]
fn rejects_embedded_report_paths_through_escaping_symlink_directories() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir()?;
    let external = tempfile::tempdir()?;
    std::fs::write(external.path().join("example.py"), "value = True\n")?;
    symlink(external.path(), root.path().join("src"))?;

    let imported = import_json(
        root.path(),
        MutationProvider::Mutmut,
        &minimal_mte_report("src/example.py").to_string(),
    );
    assert!(matches!(imported, Err(ProviderError::InvalidReport { .. })));
    Ok(())
}

#[cfg(unix)]
#[test]
fn cargo_mutants_rejects_source_file_symlinks_that_escape_root() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir()?;
    let external = tempfile::tempdir()?;
    std::fs::create_dir_all(root.path().join("src"))?;
    let outside_source = external.path().join("lib.rs");
    std::fs::write(&outside_source, "pub fn answer() -> bool { true }\n")?;
    symlink(&outside_source, root.path().join("src/lib.rs"))?;
    let report = serde_json::json!({
        "outcomes": [{
            "scenario": { "Mutant": {
                "name": "src/lib.rs:1:26: replace true with false",
                "file": "src/lib.rs",
                "span": {
                    "start": { "line": 1, "column": 26 },
                    "end": { "line": 1, "column": 30 }
                },
                "replacement": "false"
            }},
            "summary": "CaughtMutant",
            "phase_results": []
        }],
        "end_time": "2026-08-27T10:01:00Z",
        "cargo_mutants_version": "27.1.0"
    });

    let imported = import_json(root.path(), MutationProvider::CargoMutants, &report.to_string());
    assert!(matches!(imported, Err(ProviderError::InvalidReport { .. })));
    Ok(())
}

fn minimal_mte_report(file: &str) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": "2.0",
        "thresholds": { "high": 80, "low": 60 },
        "files": {
            file: {
                "language": "python",
                "source": "value = True\n",
                "mutants": [{
                    "id": "one",
                    "mutatorName": "BooleanLiteral",
                    "replacement": "False",
                    "location": {
                        "start": { "line": 1, "column": 9 },
                        "end": { "line": 1, "column": 13 }
                    },
                    "status": "Killed"
                }]
            }
        }
    })
}
