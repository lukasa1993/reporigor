use std::error::Error;
use std::path::Path;

#[cfg(unix)]
use provider_mutation::import_path;
use provider_mutation::{import_json, ImportFormat, ImportedMutationReport, MutationProvider, ProviderError};
use reporigor_core::{MutationCandidate, MutationStatus};

type TestResult = Result<(), Box<dyn Error>>;

#[derive(Clone, Copy)]
enum MteFixture<'a> {
    Minimal(&'a str),
    GenericUnicode,
    Mull,
}

struct MteSpec<'a> {
    schema: &'a str,
    file: &'a str,
    language: &'a str,
    source: &'a str,
    id: &'a str,
    mutator: &'a str,
    replacement: &'a str,
    columns: (u32, u32),
    framework: Option<(&'a str, &'a str)>,
}

impl<'a> MteSpec<'a> {
    const fn minimal(file: &'a str) -> Self {
        Self {
            schema: "2.0",
            file,
            language: "python",
            source: "value = True\n",
            id: "one",
            mutator: "BooleanLiteral",
            replacement: "False",
            columns: (9, 13),
            framework: None,
        }
    }

    fn set_mutation(&mut self, encoded: &'a str) {
        let fields = fixture_fields(encoded, 7, "mutation");
        self.source = fields[0];
        self.mutator = fields[1];
        self.replacement = fields[2];
        self.columns = (
            fields[3]
                .parse()
                .unwrap_or_else(|error| panic!("start column: {error}")),
            fields[4]
                .parse()
                .unwrap_or_else(|error| panic!("end column: {error}")),
        );
        self.framework = Some((fields[5], fields[6]));
    }

    fn for_fixture(fixture: MteFixture<'a>) -> Self {
        let file = match fixture {
            MteFixture::Minimal(file) => file,
            MteFixture::GenericUnicode => "src/example.py",
            MteFixture::Mull => "src/example.cpp",
        };
        let mut spec = Self::minimal(file);
        match fixture {
            MteFixture::Minimal(_) => {}
            MteFixture::GenericUnicode => {
                spec.set_mutation("🚀===x\n~EqualityOperator~!=~2~5~another-provider~1.0");
            }
            MteFixture::Mull => {
                spec.schema = "1.7";
                spec.language = "cpp";
                spec.id = "mull-1";
                spec.set_mutation(
                    "bool same(int a, int b) { return a == b; }\n~cxx_eq_to_ne~!=~36~38~Mull~0.26",
                );
            }
        }
        spec
    }
}

fn fixture_fields<'a>(encoded: &'a str, expected: usize, label: &str) -> Vec<&'a str> {
    let fields = encoded.split('~').collect::<Vec<_>>();
    assert_eq!(fields.len(), expected, "{label} fixture field count");
    fields
}

fn with_framework(mut report: serde_json::Value, framework: Option<(&str, &str)>) -> serde_json::Value {
    if let Some((name, version)) = framework {
        report["framework"] = serde_json::json!({ "name": name, "version": version });
    }
    report
}

fn single_mutant_mte(fixture: MteFixture<'_>) -> serde_json::Value {
    let spec = MteSpec::for_fixture(fixture);
    let report = serde_json::json!({
        "schemaVersion": spec.schema,
        "thresholds": { "high": 80, "low": 60 },
        "files": {
            spec.file: {
                "language": spec.language,
                "source": spec.source,
                "mutants": [{
                    "id": spec.id,
                    "mutatorName": spec.mutator,
                    "replacement": spec.replacement,
                    "location": {
                        "start": { "line": 1, "column": spec.columns.0 },
                        "end": { "line": 1, "column": spec.columns.1 }
                    },
                    "status": "Killed"
                }]
            }
        }
    });
    with_framework(report, spec.framework)
}

fn import_value(
    root: &Path,
    provider: MutationProvider,
    report: &serde_json::Value,
) -> Result<ImportedMutationReport, ProviderError> {
    import_json(root, provider, &report.to_string())
}

fn import_error(root: &Path, provider: MutationProvider, report: &serde_json::Value) -> ProviderError {
    match import_value(root, provider, report) {
        Ok(_) => panic!("invalid fixture unexpectedly imported for {provider}"),
        Err(error) => error,
    }
}

fn assert_invalid_report(root: &Path, provider: MutationProvider, report: &serde_json::Value) {
    assert!(matches!(
        import_error(root, provider, report),
        ProviderError::InvalidReport { .. }
    ));
}

fn assert_invalid_field(
    root: &Path,
    provider: MutationProvider,
    report: &serde_json::Value,
    expected_field: &str,
) {
    let error = import_error(root, provider, report);
    assert!(
        matches!(error, ProviderError::InvalidReport { ref field, .. } if field == expected_field),
        "expected invalid field {expected_field:?}, found {error:?}"
    );
}

fn assert_json_error(root: &Path, report: &serde_json::Value) {
    assert!(matches!(
        import_error(root, MutationProvider::Mutmut, report),
        ProviderError::Json(_)
    ));
}

fn write_fixture(root: &Path, relative: &str, contents: &str) -> Result<(), std::io::Error> {
    let path = root.join(relative);
    match path.parent() {
        Some(parent) => std::fs::create_dir_all(parent)?,
        None => return Err(std::io::Error::other("fixture path has no parent")),
    }
    std::fs::write(path, contents)
}

fn fixture_root(relative: &str, contents: &str) -> Result<tempfile::TempDir, std::io::Error> {
    let root = tempfile::tempdir()?;
    write_fixture(root.path(), relative, contents)?;
    Ok(root)
}

fn write_muter_source(root: &Path, contents: &str) -> Result<(), std::io::Error> {
    write_fixture(root, "Sources/App/main.swift", contents)
}

fn assert_candidate_coordinates(
    candidate: &MutationCandidate,
    original: &str,
    span: std::ops::Range<usize>,
    line: u32,
    column: u32,
) {
    assert_eq!(candidate.original, original);
    assert_eq!(candidate.start_byte, span.start);
    assert_eq!(candidate.end_byte, span.end);
    assert_eq!((candidate.line, candidate.column), (line, column));
}

fn first_candidate(
    root: &Path,
    provider: MutationProvider,
    report: &serde_json::Value,
) -> Result<MutationCandidate, ProviderError> {
    Ok(import_value(root, provider, report)?.results[0]
        .result
        .mutation
        .clone())
}

fn assert_report_shape(report: &ImportedMutationReport, format: ImportFormat, result_count: usize) {
    assert_eq!(report.format, format);
    assert_eq!(report.results.len(), result_count);
}

fn assert_warning_count(report: &ImportedMutationReport, expected: usize) {
    assert_eq!(report.warnings.len(), expected);
}

fn report_location(start: (u32, u32), end: (u32, u32)) -> serde_json::Value {
    serde_json::json!({
        "start": { "line": start.0, "column": start.1 },
        "end": { "line": end.0, "column": end.1 }
    })
}

fn empty_muter_report() -> serde_json::Value {
    serde_json::json!({
        "fileReports": [{
            "fileName": "main.swift",
            "mutationScore": 0,
            "appliedOperators": []
        }]
    })
}

fn remove_report_field(report: &mut serde_json::Value, object_pointer: &str, field: &str) -> TestResult {
    report
        .pointer_mut(object_pointer)
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| format!("missing object at {object_pointer}").into())
        .map(|object| {
            object.remove(field);
        })
}

fn set_muter_position(report: &mut serde_json::Value, field: &str, value: u32) {
    report["fileReports"][0]["appliedOperators"][0]["mutationPoint"]["position"][field] =
        serde_json::json!(value);
}

fn set_report_value(report: &mut serde_json::Value, pointer: &str, value: serde_json::Value) -> TestResult {
    let slot = report
        .pointer_mut(pointer)
        .ok_or_else(|| format!("missing report value at {pointer}"))?;
    *slot = value;
    Ok(())
}

fn muter_fixture(
    root: &Path,
    source: &str,
    fixture: MuterFixture,
) -> Result<serde_json::Value, std::io::Error> {
    write_muter_source(root, source)?;
    Ok(muter_report(fixture))
}

fn isolated_muter_fixture(
    source: &str,
    fixture: MuterFixture,
) -> Result<(tempfile::TempDir, serde_json::Value), std::io::Error> {
    let root = tempfile::tempdir()?;
    let report = muter_fixture(root.path(), source, fixture)?;
    Ok((root, report))
}

#[cfg(unix)]
fn escaping_symlink_fixture(
    external_name: &str,
    root_link: &str,
    contents: &str,
    link_directory: bool,
) -> Result<(tempfile::TempDir, tempfile::TempDir), std::io::Error> {
    use std::os::unix::fs::symlink;

    let (root, external) = temporary_directory_pair()?;
    let external_file = external.path().join(external_name);
    write_fixture(external.path(), external_name, contents)?;
    let link = root.path().join(root_link);
    create_parent_directory(&link)?;
    symlink(symlink_target(&external, &external_file, link_directory), link)?;
    Ok((root, external))
}

#[cfg(unix)]
fn temporary_directory_pair() -> Result<(tempfile::TempDir, tempfile::TempDir), std::io::Error> {
    Ok((tempfile::tempdir()?, tempfile::tempdir()?))
}

#[cfg(unix)]
fn create_parent_directory(path: &Path) -> std::io::Result<()> {
    path.parent().map_or(Ok(()), std::fs::create_dir_all)
}

#[cfg(unix)]
fn symlink_target<'a>(
    external: &'a tempfile::TempDir,
    external_file: &'a Path,
    link_directory: bool,
) -> &'a Path {
    if link_directory {
        external.path()
    } else {
        external_file
    }
}

fn stryker_mutant(
    id: impl serde::Serialize,
    replacement: impl serde::Serialize,
    line: u32,
    start: u32,
    end: u32,
    status: impl serde::Serialize,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "mutatorName": "EqualityOperator",
        "replacement": replacement,
        "location": {
            "start": { "line": line, "column": start },
            "end": { "line": line, "column": end }
        },
        "status": status
    })
}

fn mte_report(
    file: &str,
    language: &str,
    source: &str,
    mutants: &[serde_json::Value],
    framework: Option<(&str, &str)>,
) -> serde_json::Value {
    let files = serde_json::json!({
        file: {
            "language": language,
            "source": source,
            "mutants": mutants
        }
    });
    with_framework(mte_files_report(&files), framework)
}

fn mte_files_report(files: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": "2.0",
        "thresholds": { "high": 80, "low": 60 },
        "files": files
    })
}

fn stryker_report(source: &str, mutants: &[serde_json::Value]) -> serde_json::Value {
    mte_report(
        "src/example.ts",
        "typescript",
        source,
        mutants,
        Some(("StrykerJS", "10.0.0")),
    )
}

fn append_modified_mutant(
    report: &mut serde_json::Value,
    field: &str,
    value: serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    let mutants = &mut report["files"]["src/example.py"]["mutants"];
    let mut duplicate = mutants[0].clone();
    duplicate[field] = value;
    let items = mutants
        .as_array_mut()
        .ok_or_else(|| -> Box<dyn Error> { "mutants must be an array".into() })?;
    items.push(duplicate);
    Ok(())
}

#[derive(Clone, Copy)]
enum CargoFixture {
    OversizedSource,
    ZeroCoordinate,
    EscapingSource,
}

fn cargo_mutant_report(fixture: CargoFixture) -> serde_json::Value {
    let (start, end, name) = match fixture {
        CargoFixture::OversizedSource => ((1, 1), (1, 1), Some("src/lib.rs:1:1: replace value")),
        CargoFixture::ZeroCoordinate => ((1, 0), (1, 5), None),
        CargoFixture::EscapingSource => ((1, 26), (1, 30), Some("src/lib.rs:1:26: replace true with false")),
    };
    let mut mutant = serde_json::json!({
        "file": "src/lib.rs",
        "span": report_location(start, end),
        "replacement": "false"
    });
    if let Some(name) = name {
        mutant["name"] = serde_json::json!(name);
    }
    serde_json::json!({
        "outcomes": [{
            "scenario": { "Mutant": mutant },
            "summary": "CaughtMutant",
            "phase_results": []
        }],
        "end_time": "2026-08-27T10:01:00Z",
        "cargo_mutants_version": "27.1.0"
    })
}

#[derive(Clone, Copy)]
enum MuterFixture {
    RemoveSideEffects,
    NoCoverage,
    Utf8Offset,
}

fn muter_point(operator: &str, offset: u64, line: u64, column: u64) -> serde_json::Value {
    serde_json::json!({
        "mutationOperatorId": operator,
        "position": { "utf8Offset": offset, "line": line, "column": column }
    })
}

fn muter_report(fixture: MuterFixture) -> serde_json::Value {
    let encoded = match fixture {
        MuterFixture::RemoveSideEffects => {
            "100~RemoveSideEffects~17~2~5~block()~removed line~removed line~runtimeError"
        }
        MuterFixture::NoCoverage => "0~~~~~~~~noCoverage",
        MuterFixture::Utf8Offset => "0~ChangeLogicalConnector~12~1~13~true~false~replace true~passed",
    };
    let fields = fixture_fields(encoded, 9, "Muter");
    let fixture_number = |index: usize| {
        fields[index]
            .parse::<u64>()
            .unwrap_or_else(|error| panic!("Muter fixture number: {error}"))
    };
    let mutation_score = fixture_number(0);
    let details = (!fields[1].is_empty()).then(|| {
        (
            fields[1],
            fixture_number(2),
            fixture_number(3),
            fixture_number(4),
            fields[5],
            fields[6],
            fields[7],
        )
    });
    let outcome = fields[8];
    let (mutation_point, mutation_snapshot) = details.map_or(
        (serde_json::Value::Null, serde_json::Value::Null),
        |(operator, offset, line, column, before, after, description)| {
            (
                muter_point(operator, offset, line, column),
                serde_json::json!({ "before": before, "after": after, "description": description }),
            )
        },
    );
    let operator = serde_json::json!({
        "mutationPoint": mutation_point,
        "mutationSnapshot": mutation_snapshot,
        "testSuiteOutcome": outcome
    });
    serde_json::json!({
        "fileReports": [{
            "fileName": "main.swift",
            "mutationScore": mutation_score,
            "appliedOperators": [operator]
        }]
    })
}

#[test]
fn imports_stryker_mte_v2_and_normalizes_every_status() -> TestResult {
    let root = tempfile::tempdir()?;
    let statuses = "Killed|Survived|NoCoverage|CompileError|RuntimeError|Timeout|Ignored|Pending";
    let mutants = statuses
        .split('|')
        .enumerate()
        .map(|(index, status)| {
            let mut mutant = stryker_mutant(
                format!("stryker-{index}"),
                format!("!={index}"),
                1,
                28,
                31,
                status,
            );
            mutant["description"] = serde_json::json!("replace strict equality");
            mutant["statusReason"] = serde_json::json!((index == 0).then_some("test failed"));
            mutant["duration"] = serde_json::json!(125.0);
            mutant
        })
        .collect::<Vec<_>>();
    let mut report = stryker_report("export const answer = left === right;\n", &mutants);
    report["projectRoot"] = serde_json::json!(root.path());

    let imported = import_json(root.path(), MutationProvider::Stryker, &report.to_string())?;
    assert_report_shape(&imported, ImportFormat::MutationTestingElementsV2, 8);
    assert_eq!(imported.framework_name.as_deref(), Some("StrykerJS"));
    assert_eq!(imported.framework_version.as_deref(), Some("10.0.0"));
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
fn stryker_mte_uses_utf16_columns_and_javascript_line_terminators() -> TestResult {
    let root = tempfile::tempdir()?;
    let source = "🚀===x\r\n🚀!==y\u{2028}🚀==z\u{2029}🚀!=w";
    let mutants = "one~1~3~6~===|two~2~3~6~!==|three~3~3~5~==|four~4~3~5~!="
        .split('|')
        .map(|encoded| {
            let fields = encoded.split('~').collect::<Vec<_>>();
            let coordinate = |index: usize| {
                fields[index]
                    .parse::<u32>()
                    .unwrap_or_else(|error| panic!("Stryker coordinate: {error}"))
            };
            let (id, line, start, end, original) =
                (fields[0], coordinate(1), coordinate(2), coordinate(3), fields[4]);
            let replacement = if original.starts_with('!') { "==" } else { "!=" };
            stryker_mutant(id, replacement, line, start, end, "Killed")
        })
        .collect::<Vec<_>>();
    let report = stryker_report(source, &mutants);

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
fn generic_mte_columns_remain_one_based_unicode_scalars_and_end_exclusive() -> TestResult {
    let root = tempfile::tempdir()?;
    let report = single_mutant_mte(MteFixture::GenericUnicode);

    let mutation = first_candidate(root.path(), MutationProvider::Mutmut, &report)?;
    assert_candidate_coordinates(&mutation, "===", 4..7, 1, 2);
    Ok(())
}

#[test]
fn imports_current_cargo_mutants_outcomes_shape() -> TestResult {
    let root = tempfile::tempdir()?;
    write_fixture(
        root.path(),
        "src/lib.rs",
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
                        "span": report_location((2, 5), (2, 9)),
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
    assert_report_shape(&imported, ImportFormat::CargoMutantsOutcomes, 1);
    assert_eq!(imported.framework_version.as_deref(), Some("27.1.0"));
    let result = &imported.results[0].result;
    assert_eq!(result.status, MutationStatus::Killed);
    assert_eq!(result.mutation.original, "true");
    assert_eq!(result.mutation.replacement, "false");
    assert!((result.duration_seconds - 1.75).abs() < f64::EPSILON);
    assert_eq!(result.mutation.file, "src/lib.rs");
    assert_warning_count(&imported, 1);
    Ok(())
}

#[test]
fn imports_mull_elements_v1_through_the_common_model() -> TestResult {
    let root = tempfile::tempdir()?;
    let report = single_mutant_mte(MteFixture::Mull);
    let imported = import_value(root.path(), MutationProvider::Mull, &report)?;
    assert_eq!(imported.format, ImportFormat::MutationTestingElementsV1);
    assert_eq!(imported.results.len(), 1);
    assert_eq!(imported.results[0].result.status, MutationStatus::Killed);
    Ok(())
}

#[test]
fn imports_muter_json_only_when_basename_is_unambiguous() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut report = muter_fixture(
        root.path(),
        "func run() {\n    block()\n}\n",
        MuterFixture::RemoveSideEffects,
    )?;
    report["globalMutationScore"] = serde_json::json!(100);
    report["numberOfKilledMutants"] = serde_json::json!(1);
    report["totalAppliedMutationOperators"] = serde_json::json!(1);

    let imported = import_json(root.path(), MutationProvider::Muter, &report.to_string())?;
    assert_eq!(imported.format, ImportFormat::MuterJson);
    assert_eq!(imported.results.len(), 1);
    let mutation = &imported.results[0].result;
    assert_eq!(mutation.status, MutationStatus::Killed);
    assert_eq!(mutation.mutation.file, "Sources/App/main.swift");
    assert_eq!(mutation.mutation.original, "block()");
    assert_eq!(mutation.mutation.replacement, "removed line");
    assert_warning_count(&imported, 1);
    Ok(())
}

#[test]
fn rejects_ambiguous_muter_basenames() -> TestResult {
    let root = fixture_root("Sources/One/main.swift", "let a = 1\n")?;
    write_fixture(root.path(), "Sources/Two/main.swift", "let b = 2\n")?;
    let report = empty_muter_report();
    assert_invalid_report(root.path(), MutationProvider::Muter, &report);
    Ok(())
}

#[test]
fn rejects_unknown_status_and_paths_outside_root() -> TestResult {
    let root = tempfile::tempdir()?;
    let report = |file: &str, status: &str| {
        let mutant = serde_json::json!({
            "id": "one",
            "mutatorName": "BooleanLiteral",
            "replacement": "False",
            "location": report_location((1, 8), (1, 12)),
            "status": status
        });
        mte_report(file, "python", "value = True\n", &[mutant], None)
    };

    assert_invalid_report(
        root.path(),
        MutationProvider::Mutmut,
        &report("src/example.py", "Maybe"),
    );
    assert_invalid_report(
        root.path(),
        MutationProvider::Mutmut,
        &report("../outside.py", "Killed"),
    );
    Ok(())
}

#[test]
fn rejects_malformed_mte_versions_and_invalid_thresholds() -> TestResult {
    let root = tempfile::tempdir()?;
    for version in ["2.garbage", "2.", "2.0.0.0", "02.0"] {
        let mut report = minimal_mte_report("src/example.py");
        report["schemaVersion"] = serde_json::json!(version);
        assert_invalid_field(root.path(), MutationProvider::Mutmut, &report, "schemaVersion");
    }

    for (low, high) in [(81, 80), (0, 101)] {
        let mut report = minimal_mte_report("src/example.py");
        report["thresholds"] = serde_json::json!({ "low": low, "high": high });
        assert_invalid_field(root.path(), MutationProvider::Mutmut, &report, "thresholds");
    }
    Ok(())
}

#[test]
fn requires_mte_thresholds_language_and_nonempty_mutator_name() -> TestResult {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("MTE fixture: {error}"));
    assert_mte_requires_thresholds(root.path())?;
    assert_mte_requires_language(root.path())?;
    assert_mte_requires_mutator(root.path())?;
    assert_mte_requires_nonempty_mutator(root.path())?;
    Ok(())
}

fn assert_mte_requires_thresholds(root: &Path) -> TestResult {
    let mut without_thresholds = minimal_mte_report("src/example.py");
    without_thresholds
        .as_object_mut()
        .ok_or("report must be an object")?
        .remove("thresholds");
    assert_json_error(root, &without_thresholds);
    Ok(())
}

fn assert_mte_requires_language(root: &Path) -> TestResult {
    let mut without_language = minimal_mte_report("src/example.py");
    remove_report_field(&mut without_language, "/files/src~1example.py", "language")?;
    assert_json_error(root, &without_language);
    Ok(())
}

fn assert_mte_requires_mutator(root: &Path) -> TestResult {
    let mut without_mutator = minimal_mte_report("src/example.py");
    remove_report_field(
        &mut without_mutator,
        "/files/src~1example.py/mutants/0",
        "mutatorName",
    )?;
    assert_json_error(root, &without_mutator);
    Ok(())
}

fn assert_mte_requires_nonempty_mutator(root: &Path) -> TestResult {
    let mut empty_mutator = minimal_mte_report("src/example.py");
    set_report_value(
        &mut empty_mutator,
        "/files/src~1example.py/mutants/0/mutatorName",
        serde_json::json!("  "),
    )?;
    assert_invalid_report(root, MutationProvider::Mutmut, &empty_mutator);
    Ok(())
}

#[test]
fn rejects_duplicate_upstream_ids_and_effective_candidates() -> TestResult {
    let root = tempfile::tempdir()?;

    let mut duplicate_id = minimal_mte_report("src/example.py");
    append_modified_mutant(&mut duplicate_id, "replacement", serde_json::json!("None"))?;
    assert_invalid_field(
        root.path(),
        MutationProvider::Mutmut,
        &duplicate_id,
        "files.*.mutants[].id",
    );

    let mut duplicate_candidate = minimal_mte_report("src/example.py");
    append_modified_mutant(&mut duplicate_candidate, "id", serde_json::json!("two"))?;
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
fn enforces_report_source_file_and_per_mutant_byte_budgets() -> TestResult {
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
    let too_many_files = mte_files_report(&serde_json::Value::Object(files));
    assert_invalid_field(
        root.path(),
        MutationProvider::Mutmut,
        &too_many_files,
        "limits.sourceFiles",
    );

    let mut oversized_replacement = minimal_mte_report("src/example.py");
    set_report_value(
        &mut oversized_replacement,
        "/files/src~1example.py/mutants/0/replacement",
        serde_json::json!("x".repeat(1024 * 1024 + 1)),
    )?;
    assert_invalid_field(
        root.path(),
        MutationProvider::Mutmut,
        &oversized_replacement,
        "limits.replacementBytesPerMutant",
    );
    Ok(())
}

#[test]
fn bounded_cargo_source_read_rejects_sparse_oversized_files() -> TestResult {
    let root = fixture_root("src/lib.rs", "")?;
    let source_path = root.path().join("src/lib.rs");
    let source = std::fs::OpenOptions::new().write(true).open(&source_path)?;
    source.set_len(16 * 1024 * 1024 + 1)?;
    let report = cargo_mutant_report(CargoFixture::OversizedSource);

    assert_invalid_report(root.path(), MutationProvider::CargoMutants, &report);
    Ok(())
}

#[test]
fn rejects_zero_mte_and_cargo_mutants_coordinates() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut mte = minimal_mte_report("src/example.py");
    set_report_value(
        &mut mte,
        "/files/src~1example.py/mutants/0/location/start/line",
        serde_json::json!(0),
    )?;
    assert_invalid_report(root.path(), MutationProvider::Mutmut, &mte);

    write_fixture(root.path(), "src/lib.rs", "true\n")?;
    let cargo = cargo_mutant_report(CargoFixture::ZeroCoordinate);
    assert_invalid_report(root.path(), MutationProvider::CargoMutants, &cargo);
    Ok(())
}

#[test]
fn muter_skips_no_coverage_sentinel_instead_of_inventing_a_mutant() -> TestResult {
    let (root, report) = isolated_muter_fixture("let value = true\n", MuterFixture::NoCoverage)?;

    let imported = import_json(root.path(), MutationProvider::Muter, &report.to_string())?;
    assert!(imported.results.is_empty());

    let mut malformed = report;
    malformed["fileReports"][0]["appliedOperators"][0]["mutationPoint"] =
        muter_point("RemoveSideEffects", 0, 0, 0);
    assert_invalid_report(root.path(), MutationProvider::Muter, &malformed);
    Ok(())
}

#[test]
fn muter_uses_validated_utf8_offset_and_exports_scalar_column() -> TestResult {
    let (root, report) = isolated_muter_fixture("let café = true\n", MuterFixture::Utf8Offset)?;

    let mutation = first_candidate(root.path(), MutationProvider::Muter, &report)?;
    assert_candidate_coordinates(&mutation, "true", 12..16, 1, 12);

    let mut zero_coordinate = report.clone();
    set_muter_position(&mut zero_coordinate, "line", 0);
    assert_invalid_report(root.path(), MutationProvider::Muter, &zero_coordinate);

    let mut invalid = report;
    set_muter_position(&mut invalid, "utf8Offset", 8);
    assert_invalid_report(root.path(), MutationProvider::Muter, &invalid);
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
fn rejects_report_symlinks_and_direct_paths_outside_root() -> TestResult {
    let contents = minimal_mte_report("src/example.py").to_string();
    let (root, external) = escaping_symlink_fixture("report.json", "report.json", &contents, false)?;
    let report = external.path().join("report.json");
    let link = root.path().join("report.json");

    let linked = import_path(root.path(), MutationProvider::Mutmut, &link);
    assert!(matches!(linked, Err(ProviderError::InvalidReport { .. })));
    let direct = import_path(root.path(), MutationProvider::Mutmut, &report);
    assert!(matches!(direct, Err(ProviderError::InvalidReport { .. })));
    Ok(())
}

#[cfg(unix)]
#[test]
fn rejects_embedded_report_paths_through_escaping_symlink_directories() -> TestResult {
    let (root, _external) = escaping_symlink_fixture("example.py", "src", "value = True\n", true)?;

    assert_invalid_report(
        root.path(),
        MutationProvider::Mutmut,
        &minimal_mte_report("src/example.py"),
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn cargo_mutants_rejects_source_file_symlinks_that_escape_root() -> TestResult {
    let (root, _external) = escaping_symlink_fixture(
        "lib.rs",
        "src/lib.rs",
        "pub fn answer() -> bool { true }\n",
        false,
    )?;
    let report = cargo_mutant_report(CargoFixture::EscapingSource);

    assert_invalid_report(root.path(), MutationProvider::CargoMutants, &report);
    Ok(())
}

fn minimal_mte_report(file: &str) -> serde_json::Value {
    single_mutant_mte(MteFixture::Minimal(file))
}
