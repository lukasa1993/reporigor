use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use analysis_crap::{CoverageApplication, CrapAnalysis};
use analysis_mutate::{BaselineReport, CommandOutcome, MutationMode, MutationRun, RecoveryAction};
use analysis_quality::{BaselineComparison, OmittedCheck, QualityAnalysis, SurvivingMutant};
use reporigor_core::{
    rule_result, BackendCapabilities, BackendInfo, BaselineDisposition, Capability, Diagnostic,
    FunctionRecord, Language, MutationCandidate, MutationResult, MutationStatus, RuleComparison, RuleResult,
    RuleSummary, Severity, SourceLocation,
};
use reporigor_reporting::{
    CrapReport, DryReport, Duplicate, DuplicateLocation, MutationReport, MutationThresholds, ReportContext,
    ReportEnvelope, ReportError, RuleReport, REPORT_SCHEMA_VERSION,
};
use serde_json::json;

#[test]
fn native_report_is_versioned_summarized_and_deterministic() -> Result<(), Box<dyn Error>> {
    let mut first_context = context();
    first_context.backends.reverse();
    first_context.diagnostics.reverse();
    let first = deterministic_check_report(first_context, vec![low_risk_function(), risky_function()]);
    let second = deterministic_check_report(context(), vec![risky_function(), low_risk_function()]);

    assert_eq!(first.schema_version, REPORT_SCHEMA_VERSION);
    assert_eq!(first.summary.files, 2);
    assert_eq!(first.summary.functions, 2);
    assert_eq!(first.summary.crap_over_limit, 1);
    assert_eq!(first.summary.duplicate_groups, 1);
    assert_eq!(first.summary.mutants, 1);
    assert_eq!(first.summary.survived, 1);
    assert_eq!(first.summary.findings, 3);
    assert_eq!(first.to_pretty_json()?, second.to_pretty_json()?);
    assert!(first.to_pretty_json()?.ends_with('\n'));

    let round_trip: ReportEnvelope = serde_json::from_str(&first.to_pretty_json()?)?;
    assert_eq!(round_trip.to_pretty_json()?, first.to_pretty_json()?);
    Ok(())
}

#[test]
fn unified_rules_are_canonical_and_baseline_counts_do_not_double_count_legacy_findings(
) -> Result<(), Box<dyn Error>> {
    let mut first_analysis = quality_analysis_fixture();
    first_analysis.results.reverse();
    first_analysis.surviving_mutants.reverse();
    first_analysis.omitted.reverse();
    let first_rules = baseline_rule_report(first_analysis)?;
    let second_rules = baseline_rule_report(quality_analysis_fixture())?;

    let build =
        |rules| representative_check_report(vec![mutation(7, MutationStatus::Survived, None)], Some(rules));
    let first = build(first_rules);
    let second = build(second_rules);

    assert_eq!(first.to_pretty_json()?, second.to_pretty_json()?);
    assert_eq!(first.summary.rule_results, 5);
    assert_eq!(first.summary.rule_failures, 4);
    assert_eq!(first.summary.omitted_checks, 1);
    assert_eq!(first.summary.surviving_mutants, 1);
    assert_eq!(first.summary.baseline_existing, 2);
    assert_eq!(first.summary.baseline_new, 1);
    assert_eq!(first.summary.baseline_worsened, 1);
    assert_eq!(first.summary.baseline_improved, 1);
    assert_eq!(first.summary.baseline_resolved, 2);
    // Legacy CRAP, DRY, and surviving-mutant findings contribute three;
    // only the additional KISS failure is added from the unified rule stream.
    assert_eq!(first.summary.findings, 4);

    let rules = first.results.rules.as_ref().ok_or("rules section")?;
    let ids: BTreeSet<_> = rules
        .results
        .iter()
        .map(|result| result.violation_id.as_str())
        .collect();
    assert_eq!(ids.len(), rules.results.len());
    assert!(rules.results.iter().all(|result| {
        !Path::new(&result.file).is_absolute() && !result.file.split('/').any(|part| part == "..")
    }));
    assert_eq!(rules.surviving_mutants[0].fingerprint, "survivor-fingerprint-001");
    assert!(first.to_human().contains(
        "Baseline: enabled, path artifacts/prior-report.json, 2 existing, 1 new, 1 worsened, 1 improved, 2 resolved, gate failed"
    ));
    assert!(first.to_human().contains("Omitted checks:"));
    Ok(())
}

fn baseline_rule_report(analysis: QualityAnalysis) -> Result<RuleReport, ReportError> {
    RuleReport::with_baseline(
        analysis,
        true,
        Some("artifacts/prior-report.json".to_owned()),
        "a".repeat(64),
        &BaselineComparison {
            summary: RuleSummary::default(),
            resolved: 2,
            gate_passed: false,
        },
    )
}

#[test]
fn rule_report_rejects_duplicate_ids_and_non_relative_paths() {
    let mut duplicate_ids = quality_analysis_fixture();
    duplicate_ids.results = vec![duplicate_ids.results[0].clone(); 2];
    assert!(matches!(
        RuleReport::new(duplicate_ids),
        Err(ReportError::InvalidRules(message)) if message.contains("duplicate")
    ));

    let mut absolute = quality_analysis_fixture();
    absolute.results[0].file = "/absolute/src/lib.rs".to_owned();
    assert!(matches!(
        RuleReport::new(absolute),
        Err(ReportError::InvalidRules(message)) if message.contains("relative")
    ));
}

#[test]
fn sarif_projects_failed_generic_rules_with_dynamic_rule_ids() -> Result<(), Box<dyn Error>> {
    let report = rules_only_report()?;

    let value: serde_json::Value = serde_json::from_str(&report.to_sarif_json()?)?;
    let rule_ids: BTreeSet<_> = value["runs"][0]["tool"]["driver"]["rules"]
        .as_array()
        .ok_or("SARIF rules")?
        .iter()
        .filter_map(|rule| rule["id"].as_str())
        .collect();
    assert!(rule_ids.contains("kiss.cyclomatic-complexity"));
    assert!(rule_ids.contains("dry.similarity"));
    let results = value["runs"][0]["results"].as_array().ok_or("SARIF results")?;
    assert_eq!(results.len(), 4);
    assert!(results.iter().all(|result| {
        result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"]
            .as_str()
            .is_some_and(|path| path.starts_with("src/"))
    }));
    assert!(results
        .iter()
        .all(|result| result["locations"][0]["physicalLocation"].get("region").is_none()));
    Ok(())
}

#[test]
fn sarif_deduplicates_integrated_crap_and_dry_rules_against_legacy_sections() -> Result<(), Box<dyn Error>> {
    let report = integrated_rule_report()?;
    let value: serde_json::Value = serde_json::from_str(&report.to_sarif_json()?)?;
    let result_ids: Vec<_> = value["runs"][0]["results"]
        .as_array()
        .ok_or("SARIF results")?
        .iter()
        .filter_map(|result| result["ruleId"].as_str())
        .collect();

    assert_eq!(
        result_ids
            .iter()
            .filter(|rule| **rule == "reporigor/crap-threshold")
            .count(),
        1
    );
    assert_eq!(
        result_ids
            .iter()
            .filter(|rule| **rule == "reporigor/duplicate-code")
            .count(),
        1
    );
    assert!(!result_ids.contains(&"crap.maximum"));
    assert!(!result_ids.contains(&"dry.similarity"));
    assert!(result_ids.contains(&"kiss.cyclomatic-complexity"));
    assert!(result_ids.contains(&"mutation.surviving-mutant"));
    Ok(())
}

#[test]
fn human_report_contains_all_sections_and_fallback_diagnostics() {
    let report = representative_check_report(vec![mutation(1, MutationStatus::Killed, None)], None);

    let human = report.to_human();
    assert!(human.starts_with("reporigor check report\n"));
    assert!(human.contains("CRAP: 1 functions, 1 over 30.00"));
    assert!(human.contains("DRY: 1 duplicate groups"));
    assert!(human.contains("Mutation: 1 mutants, 1 killed"));
    assert!(human.contains("warning generic src/lib.rs:2:1: recovered syntax [fallback]"));
    assert!(human.ends_with('\n'));
}

#[test]
fn human_report_escapes_repo_controlled_terminal_and_bidi_characters() {
    let poison = "\u{1b}[31m\nforged-line\t\u{85}\u{202e}";
    let mut report_context = context();
    report_context.root = PathBuf::from(format!("/workspace/{poison}"));
    report_context.backends[0].id = format!("backend-{poison}");
    report_context.backends[0].version = poison.to_owned();
    report_context.diagnostics = vec![Diagnostic {
        severity: Severity::Error,
        backend: format!("diagnostic-{poison}"),
        message: format!("message-{poison}"),
        location: Some(SourceLocation {
            file: format!("src/diagnostic-{poison}.rs"),
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 2,
        }),
        fallback_used: false,
    }];

    let mut function = risky_function();
    function.file = format!("src/function-{poison}.rs");
    function.name = format!("function-{poison}");
    let mut duplicate = duplicate();
    duplicate.locations[0].file = format!("src/duplicate-{poison}.rs");
    let mut mutant = mutation(1, MutationStatus::Pending, None);
    mutant.mutation.file = format!("src/mutation-{poison}.rs");
    mutant.mutation.original = format!("original-{poison}");
    mutant.mutation.replacement = format!("replacement-{poison}");

    let report = report_fixture(
        report_context,
        ReportSections {
            crap: Some(CrapReport::new(vec![function], 30.0)),
            dry: Some(DryReport::new(vec![duplicate], 4)),
            mutate: Some(MutationReport::new(vec![mutant])),
            rules: None,
        },
    );
    let human = report.to_human();

    assert!(!human.contains('\u{1b}'));
    assert!(!human.contains('\t'));
    assert!(!human.contains('\u{85}'));
    assert!(!human.contains('\u{202e}'));
    assert!(!human.lines().any(|line| line.starts_with("forged-line")));
    assert!(human.contains(r"\u{1b}[31m\u{a}forged-line\u{9}\u{85}\u{202e}"));
}

#[test]
fn sarif_contains_crap_and_duplicate_findings() -> Result<(), Box<dyn Error>> {
    let report = report_fixture(
        context(),
        ReportSections {
            crap: Some(CrapReport::new(vec![low_risk_function(), risky_function()], 30.0)),
            dry: Some(DryReport::new(vec![duplicate_with_path("src/copied file.rs")], 4)),
            mutate: None,
            rules: None,
        },
    );

    let sarif = report.to_sarif_json()?;
    let value: serde_json::Value = serde_json::from_str(&sarif)?;
    let duplicate_location = sarif_physical_location(&value, 1);
    let projected_contract = [
        value["version"].clone(),
        value["$schema"].clone(),
        value["runs"][0]["tool"]["driver"]["name"].clone(),
        value["runs"][0]["results"][0]["ruleId"].clone(),
        value["runs"][0]["results"][1]["ruleId"].clone(),
        duplicate_location["artifactLocation"]["uri"].clone(),
    ];
    assert_eq!(
        json!(projected_contract),
        json!([
            "2.1.0",
            "https://json.schemastore.org/sarif-2.1.0.json",
            "reporigor",
            "reporigor/crap-threshold",
            "reporigor/duplicate-code",
            "src/copied%20file.rs",
        ])
    );
    let collection_lengths = (
        value["runs"][0]["tool"]["driver"]["rules"]
            .as_array()
            .map(Vec::len),
        value["runs"][0]["results"].as_array().map(Vec::len),
    );
    assert_eq!(collection_lengths, (Some(2), Some(2)));
    assert_eq!(sarif_physical_location(&value, 0)["region"]["startLine"], 8);
    Ok(())
}

fn sarif_physical_location(report: &serde_json::Value, result_index: usize) -> &serde_json::Value {
    &report["runs"][0]["results"][result_index]["locations"][0]["physicalLocation"]
}

#[test]
fn sarif_rejects_mutation_only_reports() {
    let report = mutation_envelope(vec![mutation(1, MutationStatus::Pending, None)]);
    assert!(matches!(
        report.to_sarif_json(),
        Err(ReportError::MissingSection("CRAP, DRY, or rules"))
    ));
}

#[test]
fn mutation_elements_projection_uses_v2_statuses_and_locations() -> Result<(), Box<dyn Error>> {
    let report = mutation_envelope(vec![
        mutation(2, MutationStatus::Invalid, None),
        mutation(1, MutationStatus::Killed, Some("test failed")),
    ]);
    let sources = mutation_sources();

    let json = report.to_mutation_elements_json(&sources, MutationThresholds::new(55, 85)?)?;
    let value: serde_json::Value = serde_json::from_str(&json)?;
    assert_eq!(
        [
            value["schemaVersion"].clone(),
            value["thresholds"]["low"].clone(),
            value["thresholds"]["high"].clone(),
            value["framework"]["name"].clone(),
            value["files"]["src/lib.rs"]["language"].clone(),
        ],
        [
            json!("2.0"),
            json!(55),
            json!(85),
            json!("reporigor"),
            json!("rust")
        ]
    );
    assert_eq!(
        value["files"]["src/lib.rs"]["source"],
        "fn compare(a: i32, b: i32) { a == b; }\n"
    );

    let mutants = value["files"]["src/lib.rs"]["mutants"]
        .as_array()
        .ok_or("mutants must be an array")?;
    assert_eq!(mutants.len(), 2);
    assert_eq!(
        (
            string_field(&mutants[0], "id"),
            string_field(&mutants[0], "status"),
            string_field(&mutants[0], "statusReason"),
            mutants[0]["duration"].as_f64(),
            string_field(&mutants[1], "status"),
            string_field(&mutants[1], "statusReason"),
            mutation_location_coordinates(&mutants[0]["location"]),
        ),
        (
            Some("1"),
            Some("Killed"),
            Some("test failed"),
            Some(125.0),
            Some("Ignored"),
            Some("The mutation candidate was rejected before execution."),
            (Some(1), Some(32), Some(1), Some(34)),
        )
    );
    Ok(())
}

#[test]
fn mutation_elements_derives_scalar_locations_from_validated_byte_spans() -> Result<(), Box<dyn Error>> {
    let mut result = mutation(1, MutationStatus::Pending, None);
    result.mutation.line = 1;
    result.mutation.column = 3;
    result.mutation.start_byte = 5;
    result.mutation.end_byte = 7;
    let report = mutation_envelope(vec![result]);
    let sources = BTreeMap::from([("src/lib.rs".to_owned(), "😀 ==\n".to_owned())]);

    let value: serde_json::Value =
        serde_json::from_str(&report.to_mutation_elements_json(&sources, MutationThresholds::default())?)?;
    let location = &value["files"]["src/lib.rs"]["mutants"][0]["location"];
    assert_mutation_location(location, (Some(1), Some(3), Some(1), Some(5)));
    Ok(())
}

fn mutation_location_coordinates(
    location: &serde_json::Value,
) -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
    let start = point_coordinates(location, "start");
    let end = point_coordinates(location, "end");
    (start.0, start.1, end.0, end.1)
}

fn point_coordinates(location: &serde_json::Value, point: &str) -> (Option<u64>, Option<u64>) {
    (
        location[point]["line"].as_u64(),
        location[point]["column"].as_u64(),
    )
}

fn string_field<'a>(value: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    value[field].as_str()
}

fn assert_mutation_location(
    location: &serde_json::Value,
    expected: (Option<u64>, Option<u64>, Option<u64>, Option<u64>),
) {
    assert_eq!(mutation_location_coordinates(location), expected);
}

#[test]
fn mutation_elements_rejects_stale_spans_and_coordinates() {
    let source = "fn compare(a: i32, b: i32) { a == b; }\n";
    let sources = BTreeMap::from([("src/lib.rs".to_owned(), source.to_owned())]);

    let mut stale = mutation(1, MutationStatus::Pending, None);
    stale.mutation.start_byte = 30;
    stale.mutation.end_byte = 32;
    assert_invalid_mutation_span(stale, &sources, "original text does not match");

    let mut wrong_coordinate = mutation(2, MutationStatus::Pending, None);
    wrong_coordinate.mutation.line = 0;
    wrong_coordinate.mutation.column = 0;
    assert_invalid_mutation_span(wrong_coordinate, &sources, "recorded start 0:0");
}

fn assert_invalid_mutation_span(
    mutant: MutationResult,
    sources: &BTreeMap<String, String>,
    expected_message: &str,
) {
    let report = mutation_envelope(vec![mutant]);
    assert!(matches!(
        report.to_mutation_elements_json(sources, MutationThresholds::default()),
        Err(ReportError::InvalidMutationSpan { message, .. }) if message.contains(expected_message)
    ));
}

#[test]
fn mutation_elements_requires_source_for_every_mutated_file() {
    let report = mutation_envelope(vec![mutation(1, MutationStatus::Pending, None)]);
    let error = report.to_mutation_elements_json(&BTreeMap::new(), MutationThresholds::default());
    assert!(matches!(
        error,
        Err(ReportError::MissingMutationSource(path)) if path == "src/lib.rs"
    ));
}

#[test]
fn mutation_score_uses_exactly_killed_and_survived_as_scoreable_states() {
    let report = MutationReport::new(vec![
        mutation(1, MutationStatus::Killed, None),
        mutation(2, MutationStatus::Timeout, None),
        mutation(3, MutationStatus::Survived, None),
        mutation(4, MutationStatus::Survived, None),
        mutation(5, MutationStatus::NoCoverage, None),
        mutation(6, MutationStatus::CompileError, None),
        mutation(7, MutationStatus::RuntimeError, None),
        mutation(8, MutationStatus::Invalid, None),
        mutation(9, MutationStatus::Ignored, None),
        mutation(10, MutationStatus::Pending, None),
    ]);

    assert_eq!(report.summary.total, 10);
    assert_eq!(report.summary.killed, 1);
    assert_eq!(report.summary.survived, 2);
    assert_eq!(report.summary.scoreable_mutants, 3);
    assert_eq!(report.summary.no_coverage, 1);
    assert_eq!(report.summary.compile_error, 1);
    assert_eq!(report.summary.runtime_error, 1);
    assert_eq!(report.summary.timeout, 1);
    assert_eq!(report.summary.invalid, 1);
    assert_eq!(report.summary.ignored, 1);
    assert_eq!(report.summary.pending, 1);
    assert!(report
        .summary
        .mutation_score
        .is_some_and(|score| (score - 100.0 / 3.0).abs() < 1.0e-12));
}

#[test]
fn analyzer_native_results_feed_reporting_sections_directly() -> Result<(), Box<dyn Error>> {
    let (crap, coverage) = analyzed_crap_report();
    assert_eq!(crap.summary.over_limit, 1);
    assert_eq!(crap.coverage, Some(coverage));
    assert_eq!(crap.functions[0].name, "risky");
    assert_eq!(crap.functions[1].name, "safe");
    assert_eq!(crap.functions[2].name, "uncovered");

    let pending = MutationReport::pending(vec![mutation(9, MutationStatus::Killed, None).mutation]);
    assert_eq!(pending.summary.pending, 1);

    let executed = executed_mutation_report(0.25, "validation passed", RecoveryAction::Restored);
    assert_executed_provenance(&executed);
    assert_stable_execution_projection(executed)?;
    Ok(())
}

fn analyzed_crap_report() -> (CrapReport, CoverageApplication) {
    let mut uncovered = low_risk_function();
    "uncovered".clone_into(&mut uncovered.name);
    uncovered.start_line = 30;
    uncovered.end_line = 34;
    uncovered.coverage = None;
    uncovered.crap = None;
    let coverage = CoverageApplication {
        total_functions: 3,
        matched_functions: 2,
        unmatched_functions: 1,
        empty_ranges: 0,
        ambiguous_functions: 0,
    };
    let report = CrapReport::from_analysis(
        CrapAnalysis {
            functions: vec![uncovered, low_risk_function(), risky_function()],
            coverage: Some(coverage),
        },
        30.0,
    );
    (report, coverage)
}

fn executed_mutation_report(duration_seconds: f64, output: &str, recovery: RecoveryAction) -> MutationReport {
    let mut result = mutation(9, MutationStatus::Killed, Some(output));
    result.duration_seconds = duration_seconds;
    MutationReport::from_run(MutationRun {
        root: PathBuf::from("/workspace/project"),
        mode: MutationMode::Execute,
        recovery,
        baseline: BaselineReport {
            validation: Some(CommandOutcome {
                exit_code: Some(0),
                timed_out: false,
                duration_seconds,
                output: output.to_owned(),
                output_truncated: output.len() > 8,
            }),
            test: None,
        },
        results: vec![result],
    })
}

fn assert_executed_provenance(executed: &MutationReport) {
    assert_eq!(executed.summary.killed, 1);
    assert!(matches!(
        executed.run.as_ref(),
        Some(run)
            if run.mode == MutationMode::Execute
                && run.recovery == RecoveryAction::Restored
                && run.baseline.validation.is_some()
    ));
}

fn assert_stable_execution_projection(executed: MutationReport) -> Result<(), Box<dyn Error>> {
    let envelope = ReportEnvelope::mutate(context(), executed);
    let serialized = envelope.to_pretty_json()?;
    let value: serde_json::Value = serde_json::from_str(&serialized)?;
    assert_eq!(value["results"]["mutate"]["run"]["mode"], "execute");
    assert_eq!(value["results"]["mutate"]["run"]["recovery"], "restored");
    let baseline = &value["results"]["mutate"]["run"]["baseline"]["validation"];
    assert!(baseline.get("duration_seconds").is_none());
    assert!(baseline.get("output").is_none());
    assert!(baseline.get("output_truncated").is_none());
    let mutant = &value["results"]["mutate"]["mutants"][0];
    assert!(mutant.get("duration_seconds").is_none());
    assert!(mutant.get("detail").is_none());
    let round_trip: ReportEnvelope = serde_json::from_str(&serialized)?;
    assert_eq!(round_trip.command, envelope.command);
    assert_eq!(round_trip.summary, envelope.summary);
    Ok(())
}

#[test]
fn executed_native_report_omits_runtime_volatility() -> Result<(), Box<dyn Error>> {
    let fast = render_executed_mutation(0.125, "fast")?;
    let slow = render_executed_mutation(19.75, "slow timestamp /tmp/run")?;
    assert_eq!(fast, slow);
    Ok(())
}

fn render_executed_mutation(duration_seconds: f64, output: &str) -> Result<String, ReportError> {
    let report = executed_mutation_report(duration_seconds, output, RecoveryAction::Restored);
    ReportEnvelope::mutate(context(), report).to_pretty_json()
}

#[test]
fn timeout_is_reported_as_an_operational_mutation_error() {
    let report = mutation_envelope(vec![mutation(1, MutationStatus::Timeout, None)]);
    assert_eq!(report.summary.mutation_errors, 1);
}

#[test]
fn shipped_schemas_accept_representative_serializer_output() -> Result<(), Box<dyn Error>> {
    validate_shipped_schema_outputs()
}

fn validate_shipped_schema_outputs() -> Result<(), Box<dyn Error>> {
    let schemas = ShippedSchemas::load()?;
    schemas.validate_dialects()?;
    schemas.validate_reports()
}

struct ShippedSchemas {
    native: serde_json::Value,
    mutation: serde_json::Value,
}

impl ShippedSchemas {
    fn load() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            native: load_shipped_schema("reporigor-report-v1.schema.json")?,
            mutation: load_shipped_schema("mutation-testing-elements-v2.schema.json")?,
        })
    }

    fn validate_dialects(&self) -> Result<(), Box<dyn Error>> {
        validate_schema_dialects([
            ("reporigor-report-v1.schema.json", &self.native),
            ("mutation-testing-elements-v2.schema.json", &self.mutation),
        ])
    }

    fn validate_reports(&self) -> Result<(), Box<dyn Error>> {
        let all_mutations = all_mutation_results();
        validate_native_reports(representative_native_reports(&all_mutations)?, &self.native)?;
        validate_mutation_elements_report(all_mutations, &self.mutation)
    }
}

fn validate_schema_dialects<const N: usize>(
    schemas: [(&str, &serde_json::Value); N],
) -> Result<(), Box<dyn Error>> {
    for (name, schema) in schemas {
        assert_eq!(
            schema.get("$schema").and_then(serde_json::Value::as_str),
            Some("https://json-schema.org/draft/2020-12/schema"),
            "{name} must declare the supported schema dialect"
        );
        ensure_supported_schema(schema, "$").map_err(invalid_schema)?;
    }
    Ok(())
}

fn all_mutation_results() -> Vec<MutationResult> {
    MutationStatus::ALL
        .into_iter()
        .enumerate()
        .map(|(index, status)| mutation(u64::try_from(index + 1).unwrap_or(u64::MAX), status, None))
        .collect()
}

fn representative_native_reports(
    all_mutations: &[MutationResult],
) -> Result<Vec<ReportEnvelope>, ReportError> {
    Ok(vec![
        ReportEnvelope::crap(
            context(),
            CrapReport::from_analysis(
                CrapAnalysis {
                    functions: vec![low_risk_function(), risky_function()],
                    coverage: Some(CoverageApplication {
                        total_functions: 2,
                        matched_functions: 2,
                        unmatched_functions: 0,
                        empty_ranges: 0,
                        ambiguous_functions: 0,
                    }),
                },
                30.0,
            ),
        ),
        ReportEnvelope::dry(context(), DryReport::new(vec![duplicate()], 4)),
        ReportEnvelope::mutate(
            context(),
            MutationReport::from_run(MutationRun {
                root: PathBuf::from("/workspace/project"),
                mode: MutationMode::Execute,
                recovery: RecoveryAction::AlreadyClean,
                baseline: BaselineReport {
                    validation: Some(CommandOutcome {
                        exit_code: Some(0),
                        timed_out: false,
                        duration_seconds: 0.25,
                        output: "validation passed".to_owned(),
                        output_truncated: false,
                    }),
                    test: None,
                },
                results: all_mutations.to_vec(),
            }),
        ),
        representative_check_report(
            all_mutations.to_vec(),
            Some(RuleReport::new(quality_analysis_fixture())?),
        ),
    ])
}

fn validate_native_reports(
    reports: Vec<ReportEnvelope>,
    native_schema: &serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    for report in reports {
        let serialized: serde_json::Value = serde_json::from_str(&report.to_pretty_json()?)?;
        validate_instance(&serialized, native_schema, native_schema, "$").map_err(invalid_schema)?;
    }
    Ok(())
}

fn validate_mutation_elements_report(
    all_mutations: Vec<MutationResult>,
    mutation_schema: &serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    let mutation_report = mutation_envelope(all_mutations);
    let sources = mutation_sources();
    let serialized: serde_json::Value = serde_json::from_str(
        &mutation_report.to_mutation_elements_json(&sources, MutationThresholds::default())?,
    )?;
    validate_instance(&serialized, mutation_schema, mutation_schema, "$").map_err(invalid_schema)?;
    Ok(())
}

#[test]
fn v1_schema_keeps_new_identity_and_summary_fields_optional_for_prior_documents() -> Result<(), Box<dyn Error>>
{
    validate_prior_document_compatibility()
}

fn validate_prior_document_compatibility() -> Result<(), Box<dyn Error>> {
    let schema = load_shipped_schema("reporigor-report-v1.schema.json")?;
    let mut prior = serde_json::to_value(prior_compatible_report())?;
    remove_new_optional_fields(&mut prior)?;
    validate_instance(&prior, &schema, &schema, "$").map_err(invalid_schema)?;
    Ok(())
}

fn remove_new_optional_fields(prior: &mut serde_json::Value) -> Result<(), Box<dyn Error>> {
    remove_new_summary_fields(prior)?;
    remove_new_mutation_fields(prior)?;
    remove_new_duplicate_fields(prior)
}

fn prior_compatible_report() -> ReportEnvelope {
    representative_check_report(vec![mutation(1, MutationStatus::Survived, None)], None)
}

fn remove_new_summary_fields(prior: &mut serde_json::Value) -> Result<(), Box<dyn Error>> {
    let summary = prior["summary"].as_object_mut().ok_or("report summary object")?;
    for field in encoded_names(
        "rule_results|rule_failures|omitted_checks|surviving_mutants|baseline_existing|baseline_new|baseline_worsened|baseline_improved|baseline_resolved",
    ) {
        summary.remove(field);
    }
    Ok(())
}

fn remove_new_mutation_fields(prior: &mut serde_json::Value) -> Result<(), Box<dyn Error>> {
    prior["results"]["mutate"]["summary"]
        .as_object_mut()
        .ok_or("mutation summary object")?
        .remove("scoreable_mutants");
    mutate_json_objects(&mut prior["results"]["mutate"]["mutants"], "mutant", |mutant| {
        mutant.remove("stable_symbol");
        mutant.remove("operator");
        mutant.remove("fingerprint");
        Ok(())
    })?;
    Ok(())
}

fn remove_new_duplicate_fields(prior: &mut serde_json::Value) -> Result<(), Box<dyn Error>> {
    mutate_json_objects(
        &mut prior["results"]["dry"]["duplicates"],
        "duplicate",
        |duplicate| {
            duplicate.remove("clone_group_id");
            duplicate.remove("similarity");
            duplicate.remove("statement_count");
            duplicate.remove("algorithm");
            remove_new_location_fields(duplicate)
        },
    )?;
    Ok(())
}

fn mutate_json_objects(
    value: &mut serde_json::Value,
    label: &str,
    mut action: impl FnMut(&mut serde_json::Map<String, serde_json::Value>) -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    let entries = value.as_array_mut().ok_or_else(|| format!("{label} array"))?;
    for entry in entries {
        let object = entry.as_object_mut().ok_or_else(|| format!("{label} object"))?;
        action(object)?;
    }
    Ok(())
}

fn encoded_names(names: &str) -> impl Iterator<Item = &str> {
    names.split('|')
}

fn remove_new_location_fields(
    duplicate: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<(), Box<dyn Error>> {
    for location in duplicate["locations"]
        .as_array_mut()
        .ok_or("duplicate locations")?
    {
        location
            .as_object_mut()
            .ok_or("duplicate location object")?
            .remove("stable_symbol");
    }
    Ok(())
}

#[test]
fn v1_schema_rejects_a_non_hex_violation_id() -> Result<(), Box<dyn Error>> {
    let (mut serialized, schema) = rule_document_and_schema()?;
    serialized["results"]["rules"]["results"][0]["violation_id"] = serde_json::Value::String("g".repeat(64));
    let error = rejected_validation_error(&serialized, &schema)?;
    assert!(error.contains("lowercase SHA-256"), "{error}");
    Ok(())
}

fn rule_document_and_schema() -> Result<(serde_json::Value, serde_json::Value), Box<dyn Error>> {
    let schema = load_shipped_schema("reporigor-report-v1.schema.json")?;
    let report = rules_only_report()?;
    let serialized = serde_json::from_str(&report.to_pretty_json()?)?;
    Ok((serialized, schema))
}

fn rejected_validation_error(
    document: &serde_json::Value,
    schema: &serde_json::Value,
) -> Result<String, Box<dyn Error>> {
    match validate_instance(document, schema, schema, "$") {
        Ok(()) => Err("schema accepted a non-hex violation ID".into()),
        Err(error) => Ok(error),
    }
}

fn load_shipped_schema(name: &str) -> Result<serde_json::Value, Box<dyn Error>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas")
        .join(name);
    let source = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&source)?)
}

fn invalid_schema(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

fn ensure_supported_schema(schema: &serde_json::Value, path: &str) -> Result<(), String> {
    let Some(object) = schema.as_object() else {
        return schema
            .is_boolean()
            .then_some(())
            .ok_or_else(|| format!("{path}: a schema must be an object or boolean"));
    };
    for (keyword, value) in object {
        ensure_supported_keyword(keyword, value, path)?;
    }
    Ok(())
}

fn ensure_supported_keyword(keyword: &str, value: &serde_json::Value, path: &str) -> Result<(), String> {
    const PASSIVE: &str =
        "$schema|$id|title|description|$ref|type|required|const|enum|minimum|maximum|minLength|uniqueItems";
    if encoded_names(PASSIVE).any(|passive| passive == keyword) {
        return Ok(());
    }
    ensure_structural_keyword(SchemaKeyword {
        name: keyword,
        value,
        path,
    })
}

#[derive(Clone, Copy)]
struct SchemaKeyword<'a> {
    name: &'a str,
    value: &'a serde_json::Value,
    path: &'a str,
}

fn ensure_structural_keyword(keyword: SchemaKeyword<'_>) -> Result<(), String> {
    if keyword.name == "pattern" {
        return ensure_supported_pattern(keyword.value, keyword.path);
    }
    if ["properties", "$defs"].contains(&keyword.name) {
        return ensure_supported_map(keyword.name, keyword.value, keyword.path);
    }
    if ["items", "additionalProperties", "if", "then"].contains(&keyword.name) {
        return ensure_supported_schema(keyword.value, &format!("{}/{}", keyword.path, keyword.name));
    }
    ensure_supported_all_of(keyword)
}

fn ensure_supported_pattern(value: &serde_json::Value, path: &str) -> Result<(), String> {
    (value.as_str() == Some("^[0-9a-f]{64}$"))
        .then_some(())
        .ok_or_else(|| format!("{path}/pattern: unsupported focused pattern {value}"))
}

fn ensure_supported_map(keyword: &str, value: &serde_json::Value, path: &str) -> Result<(), String> {
    let entries = value
        .as_object()
        .ok_or_else(|| format!("{path}/{keyword}: expected an object"))?;
    for (name, child) in entries {
        ensure_supported_schema(child, &format!("{path}/{keyword}/{name}"))?;
    }
    Ok(())
}

fn ensure_supported_all_of(keyword: SchemaKeyword<'_>) -> Result<(), String> {
    if keyword.name != "allOf" {
        return Err(format!(
            "{}: unsupported JSON Schema keyword {:?}; extend the contract validator",
            keyword.path, keyword.name
        ));
    }
    let entries = keyword
        .value
        .as_array()
        .ok_or_else(|| format!("{}/allOf: expected an array", keyword.path))?;
    for (index, child) in entries.iter().enumerate() {
        ensure_supported_schema(child, &format!("{}/allOf/{index}", keyword.path))?;
    }
    Ok(())
}

fn validate_instance(
    instance: &serde_json::Value,
    schema: &serde_json::Value,
    root: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    FocusedSchemaValidator { root }.validate(instance, schema, path)
}

fn instance_has_type(instance: &serde_json::Value, name: &str) -> Result<bool, String> {
    type JsonTypePredicate = fn(&serde_json::Value) -> bool;
    const TYPES: &[(&str, JsonTypePredicate)] = &[
        ("null", serde_json::Value::is_null),
        ("boolean", serde_json::Value::is_boolean),
        ("object", serde_json::Value::is_object),
        ("array", serde_json::Value::is_array),
        ("number", serde_json::Value::is_number),
        ("integer", is_json_integer),
        ("string", serde_json::Value::is_string),
    ];
    TYPES
        .iter()
        .find(|(supported, _)| *supported == name)
        .map(|(_, predicate)| predicate(instance))
        .ok_or_else(|| format!("unsupported JSON Schema type {name:?}"))
}

fn is_json_integer(value: &serde_json::Value) -> bool {
    value.as_i64().is_some() || value.as_u64().is_some()
}

struct FocusedSchemaValidator<'a> {
    root: &'a serde_json::Value,
}

#[derive(Clone, Copy)]
struct SchemaValidation<'a> {
    instance: &'a serde_json::Value,
    schema: &'a serde_json::Map<String, serde_json::Value>,
    path: &'a str,
}

#[derive(Clone, Copy)]
struct ObjectValidation<'a> {
    node: SchemaValidation<'a>,
    fields: &'a serde_json::Map<String, serde_json::Value>,
    properties: Option<&'a serde_json::Map<String, serde_json::Value>>,
}

#[derive(Clone, Copy)]
struct ArrayValidation<'a> {
    node: SchemaValidation<'a>,
    items: &'a [serde_json::Value],
}

#[derive(Clone, Copy)]
struct StringValidation<'a> {
    text: &'a str,
    node: SchemaValidation<'a>,
}

#[derive(Clone, Copy)]
struct NumberValidation<'a> {
    number: f64,
    node: SchemaValidation<'a>,
}

#[derive(Clone, Copy)]
enum BoundDirection {
    Minimum,
    Maximum,
}

impl BoundDirection {
    fn rejects<T: PartialOrd>(self, actual: &T, bound: &T) -> bool {
        match self {
            Self::Minimum => actual < bound,
            Self::Maximum => actual > bound,
        }
    }

    fn failure<T: std::fmt::Display>(self, path: &str, subject: &str, actual: &T, bound: &T) -> String {
        match self {
            Self::Minimum => format!("{path}: {subject} {actual} is below {bound}"),
            Self::Maximum => format!("{path}: {subject} {actual} exceeds {bound}"),
        }
    }
}

impl FocusedSchemaValidator<'_> {
    fn validate(
        &self,
        instance: &serde_json::Value,
        schema: &serde_json::Value,
        path: &str,
    ) -> Result<(), String> {
        if let Some(accepted) = schema.as_bool() {
            return accepted
                .then_some(())
                .ok_or_else(|| format!("{path}: rejected by boolean schema"));
        }
        let object = schema
            .as_object()
            .ok_or_else(|| format!("{path}: schema is not an object"))?;
        let validation = SchemaValidation {
            instance,
            schema: object,
            path,
        };
        self.validate_identity_constraints(validation)?;
        self.validate_value_constraints(validation)?;
        self.validate_composition(validation)
    }

    fn validate_identity_constraints(&self, validation: SchemaValidation<'_>) -> Result<(), String> {
        self.validate_reference(validation)?;
        Self::validate_optional_constraint(validation, "type", Self::validate_type_value)?;
        Self::validate_optional_constraint(validation, "const", Self::validate_const_value)?;
        Self::validate_optional_constraint(validation, "enum", Self::validate_enum_value)
    }

    fn validate_value_constraints(&self, validation: SchemaValidation<'_>) -> Result<(), String> {
        self.validate_object(validation)?;
        self.validate_array(validation)?;
        Self::validate_string(validation)?;
        Self::validate_number(validation)
    }

    fn validate_reference(&self, validation: SchemaValidation<'_>) -> Result<(), String> {
        let Some(reference) = validation.schema.get("$ref").and_then(serde_json::Value::as_str) else {
            return Ok(());
        };
        let pointer = reference
            .strip_prefix('#')
            .ok_or_else(|| format!("{}: only local schema references are supported", validation.path))?;
        let target = self
            .root
            .pointer(pointer)
            .ok_or_else(|| format!("{}: unresolved schema reference {reference:?}", validation.path))?;
        self.validate(validation.instance, target, validation.path)
    }

    fn validate_type_value(
        validation: SchemaValidation<'_>,
        expected: &serde_json::Value,
    ) -> Result<(), String> {
        if Self::matches_expected_type(validation.instance, expected, validation.path)? {
            return Ok(());
        }
        Err(format!(
            "{}: value does not match schema type {expected}",
            validation.path
        ))
    }

    fn matches_expected_type(
        instance: &serde_json::Value,
        expected: &serde_json::Value,
        path: &str,
    ) -> Result<bool, String> {
        if let Some(name) = expected.as_str() {
            return instance_has_type(instance, name);
        }
        let names = expected
            .as_array()
            .ok_or_else(|| format!("{path}: schema type must be a string or array"))?;
        Self::matches_type_array(instance, names, path)
    }

    fn matches_type_array(
        instance: &serde_json::Value,
        names: &[serde_json::Value],
        path: &str,
    ) -> Result<bool, String> {
        let mut accepted = false;
        for name in names {
            let name = name
                .as_str()
                .ok_or_else(|| format!("{path}: schema type array contains a non-string"))?;
            accepted |= instance_has_type(instance, name)?;
        }
        Ok(accepted)
    }

    fn validate_const_value(
        validation: SchemaValidation<'_>,
        expected: &serde_json::Value,
    ) -> Result<(), String> {
        Self::require_constraint(
            validation.instance == expected,
            format!(
                "{}: expected constant {expected}, found {}",
                validation.path, validation.instance
            ),
        )
    }

    fn validate_enum_value(
        validation: SchemaValidation<'_>,
        choices: &serde_json::Value,
    ) -> Result<(), String> {
        let choices = choices
            .as_array()
            .ok_or_else(|| format!("{}: schema enum must be an array", validation.path))?;
        Self::require_constraint(
            choices.contains(validation.instance),
            format!(
                "{}: {} is not one of the allowed values",
                validation.path, validation.instance
            ),
        )
    }

    fn require_constraint(accepted: bool, message: String) -> Result<(), String> {
        accepted.then_some(()).ok_or(message)
    }

    fn validate_optional_constraint<F>(
        validation: SchemaValidation<'_>,
        keyword: &str,
        validate: F,
    ) -> Result<(), String>
    where
        F: FnOnce(SchemaValidation<'_>, &serde_json::Value) -> Result<(), String>,
    {
        validation
            .schema
            .get(keyword)
            .map_or(Ok(()), |value| validate(validation, value))
    }

    fn validate_object(&self, validation: SchemaValidation<'_>) -> Result<(), String> {
        let Some(fields) = validation.instance.as_object() else {
            return Ok(());
        };
        let properties = validation
            .schema
            .get("properties")
            .and_then(serde_json::Value::as_object);
        let object = ObjectValidation {
            node: validation,
            fields,
            properties,
        };
        Self::validate_required(object)?;
        self.validate_properties(object)?;
        self.validate_additional(object)
    }

    fn validate_required(validation: ObjectValidation<'_>) -> Result<(), String> {
        for name in required_names(validation.node.schema, validation.node.path)? {
            Self::validate_required_name(validation.fields, name, validation.node.path)?;
        }
        Ok(())
    }

    fn validate_required_name(
        fields: &serde_json::Map<String, serde_json::Value>,
        name: &str,
        path: &str,
    ) -> Result<(), String> {
        fields
            .contains_key(name)
            .then_some(())
            .ok_or_else(|| format!("{path}: required field {name:?} is absent"))
    }

    fn validate_properties(&self, validation: ObjectValidation<'_>) -> Result<(), String> {
        let Some(properties) = validation.properties else {
            return Ok(());
        };
        for (name, child_schema) in properties {
            if let Some(value) = validation.fields.get(name) {
                self.validate(value, child_schema, &format!("{}/{name}", validation.node.path))?;
            }
        }
        Ok(())
    }

    fn validate_additional(&self, validation: ObjectValidation<'_>) -> Result<(), String> {
        let Some(additional) = validation.node.schema.get("additionalProperties") else {
            return Ok(());
        };
        for (name, value) in validation.fields {
            if validation
                .properties
                .is_some_and(|known| known.contains_key(name))
            {
                continue;
            }
            self.validate_additional_value(value, additional, validation.node.path, name)?;
        }
        Ok(())
    }

    fn validate_additional_value(
        &self,
        value: &serde_json::Value,
        schema: &serde_json::Value,
        path: &str,
        name: &str,
    ) -> Result<(), String> {
        if schema == &serde_json::Value::Bool(true) {
            return Ok(());
        }
        if schema == &serde_json::Value::Bool(false) {
            return Err(format!("{path}: additional field {name:?} is not allowed"));
        }
        self.validate(value, schema, &format!("{path}/{name}"))
    }

    fn validate_array(&self, validation: SchemaValidation<'_>) -> Result<(), String> {
        let Some(items) = validation.instance.as_array() else {
            return Ok(());
        };
        let array = ArrayValidation {
            node: validation,
            items,
        };
        Self::validate_unique_items(array)?;
        self.validate_items(array)
    }

    fn validate_unique_items(validation: ArrayValidation<'_>) -> Result<(), String> {
        if validation
            .node
            .schema
            .get("uniqueItems")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Ok(());
        }
        for (index, value) in validation.items.iter().enumerate() {
            if validation.items[..index].contains(value) {
                return Err(format!(
                    "{}/{index}: array item is not unique",
                    validation.node.path
                ));
            }
        }
        Ok(())
    }

    fn validate_items(&self, validation: ArrayValidation<'_>) -> Result<(), String> {
        let Some(item_schema) = validation.node.schema.get("items") else {
            return Ok(());
        };
        for (index, value) in validation.items.iter().enumerate() {
            self.validate(value, item_schema, &format!("{}/{index}", validation.node.path))?;
        }
        Ok(())
    }

    fn validate_string(validation: SchemaValidation<'_>) -> Result<(), String> {
        if !validation.instance.is_string() {
            return Ok(());
        }
        let string = StringValidation {
            text: validation.instance.as_str().unwrap_or_default(),
            node: validation,
        };
        Self::validate_string_length(string)?;
        Self::validate_string_pattern(string)
    }

    fn validate_string_length(validation: StringValidation<'_>) -> Result<(), String> {
        let length = u64::try_from(validation.text.chars().count()).unwrap_or(u64::MAX);
        Self::validate_bound(
            validation.node,
            &length,
            "minLength",
            serde_json::Value::as_u64,
            BoundDirection::Minimum,
            "string length",
        )
    }

    fn validate_string_pattern(validation: StringValidation<'_>) -> Result<(), String> {
        let checks_sha256 = validation
            .node
            .schema
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            == Some("^[0-9a-f]{64}$");
        if !checks_sha256 || is_lowercase_sha256(validation.text) {
            return Ok(());
        }
        Err(format!(
            "{}: value is not a lowercase SHA-256 identifier",
            validation.node.path
        ))
    }

    fn validate_number(validation: SchemaValidation<'_>) -> Result<(), String> {
        let Some(number) = validation.instance.as_f64() else {
            return Ok(());
        };
        Self::validate_numeric_bounds(NumberValidation {
            number,
            node: validation,
        })
    }

    fn validate_numeric_bounds(validation: NumberValidation<'_>) -> Result<(), String> {
        Self::validate_bound(
            validation.node,
            &validation.number,
            "minimum",
            serde_json::Value::as_f64,
            BoundDirection::Minimum,
            "number",
        )?;
        Self::validate_bound(
            validation.node,
            &validation.number,
            "maximum",
            serde_json::Value::as_f64,
            BoundDirection::Maximum,
            "number",
        )
    }

    fn validate_bound<T: PartialOrd + std::fmt::Display>(
        validation: SchemaValidation<'_>,
        actual: &T,
        keyword: &str,
        extract: fn(&serde_json::Value) -> Option<T>,
        direction: BoundDirection,
        subject: &str,
    ) -> Result<(), String> {
        let Some(bound) = validation.schema.get(keyword).and_then(extract) else {
            return Ok(());
        };
        if direction.rejects(actual, &bound) {
            return Err(direction.failure(validation.path, subject, actual, &bound));
        }
        Ok(())
    }

    fn validate_composition(&self, validation: SchemaValidation<'_>) -> Result<(), String> {
        self.validate_all_of(validation)?;
        self.validate_conditional(validation)
    }

    fn validate_all_of(&self, validation: SchemaValidation<'_>) -> Result<(), String> {
        let Some(all_of) = validation
            .schema
            .get("allOf")
            .and_then(serde_json::Value::as_array)
        else {
            return Ok(());
        };
        for child_schema in all_of {
            self.validate(validation.instance, child_schema, validation.path)?;
        }
        Ok(())
    }

    fn validate_conditional(&self, validation: SchemaValidation<'_>) -> Result<(), String> {
        let Some(condition) = validation.schema.get("if") else {
            return Ok(());
        };
        if self
            .validate(validation.instance, condition, validation.path)
            .is_err()
        {
            return Ok(());
        }
        let Some(consequence) = validation.schema.get("then") else {
            return Ok(());
        };
        self.validate(validation.instance, consequence, validation.path)
    }
}

fn required_names<'a>(
    schema: &'a serde_json::Map<String, serde_json::Value>,
    path: &str,
) -> Result<Vec<&'a str>, String> {
    let Some(required) = schema.get("required") else {
        return Ok(Vec::new());
    };
    required
        .as_array()
        .ok_or_else(|| format!("{path}: schema required must be an array"))?
        .iter()
        .map(|name| {
            name.as_str()
                .ok_or_else(|| format!("{path}: required field name is not a string"))
        })
        .collect()
}

fn is_lowercase_sha256(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn quality_analysis_fixture() -> QualityAnalysis {
    let crap = quality_rule(
        "crap.maximum|src/lib.rs|crate::risky()|40|30|crap-formula-v1",
        RuleComparison::Maximum,
        BaselineDisposition::Existing,
    );
    let dry = quality_rule(
        "dry.similarity;src/copied.rs;crate::left()|crate::right();0.92;0.92;normalized-token-shingle-dice-v1",
        RuleComparison::MaximumExclusive,
        BaselineDisposition::Existing,
    );
    let mutation = quality_rule(
        "mutation.surviving-mutant|src/lib.rs|crate::sample()|1|0|mutation-fingerprint-v1",
        RuleComparison::Maximum,
        BaselineDisposition::Worsened,
    );
    let kiss = quality_rule(
        "kiss.cyclomatic-complexity|src/complex.rs|crate::complex()|11|10|cyclomatic-complexity-v1",
        RuleComparison::Maximum,
        BaselineDisposition::New,
    );
    let passing = quality_rule(
        "kiss.parameter-count|src/lib.rs|crate::small()|1|4|parameter-count-v1",
        RuleComparison::Maximum,
        BaselineDisposition::Improved,
    );

    QualityAnalysis {
        formulas: BTreeMap::from([
            (
                "mutation_score".to_owned(),
                "killed / scoreable_mutants; scoreable statuses are exactly killed and survived".to_owned(),
            ),
            (
                "crap".to_owned(),
                "complexity^2 * (1 - coverage)^3 + complexity".to_owned(),
            ),
        ]),
        results: vec![mutation, passing, dry, kiss, crap],
        surviving_mutants: vec![surviving_mutant_fixture()],
        omitted: vec![OmittedCheck {
            rule_id: "yagni.unused-module".to_owned(),
            reason: "whole-project module inventory is unavailable".to_owned(),
        }],
    }
}

fn quality_rule(
    specification: &str,
    comparison: RuleComparison,
    baseline: BaselineDisposition,
) -> RuleResult {
    let delimiter = if specification.contains(';') { ';' } else { '|' };
    let fields = specification.split(delimiter).collect::<Vec<_>>();
    assert_eq!(fields.len(), 6, "invalid quality rule fixture");
    let measured = fields[3]
        .parse::<f64>()
        .unwrap_or_else(|error| panic!("measured fixture: {error}"));
    let allowed = fields[4]
        .parse::<f64>()
        .unwrap_or_else(|error| panic!("allowed fixture: {error}"));
    let mut result = rule_fixture(
        fields[0],
        fields[1],
        fields[2],
        (measured, allowed),
        comparison,
        fields[5],
    );
    result.baseline = baseline;
    result
}

fn surviving_mutant_fixture() -> SurvivingMutant {
    SurvivingMutant {
        file: "src/lib.rs".to_owned(),
        stable_symbol: "crate::sample()".to_owned(),
        operator: "comparison".to_owned(),
        fingerprint: "survivor-fingerprint-001".to_owned(),
    }
}

fn rule_fixture(
    rule_id: &str,
    file: &str,
    stable_symbol: &str,
    values: (f64, f64),
    comparison: RuleComparison,
    algorithm: &str,
) -> RuleResult {
    let (measured, allowed) = values;
    rule_result!(
        rule_id,
        file,
        stable_symbol,
        json!(measured),
        json!(allowed),
        algorithm,
        comparison,
        &format!("{rule_id}|{stable_symbol}|{algorithm}"),
    )
    .unwrap_or_else(|error| panic!("rule fixture failed: {error}"))
}

fn deterministic_check_report(context: ReportContext, functions: Vec<FunctionRecord>) -> ReportEnvelope {
    check_fixture(
        context,
        functions,
        vec![mutation(7, MutationStatus::Survived, None)],
        None,
    )
}

fn representative_check_report(mutants: Vec<MutationResult>, rules: Option<RuleReport>) -> ReportEnvelope {
    check_fixture(context(), vec![risky_function()], mutants, rules)
}

fn rules_only_report() -> Result<ReportEnvelope, ReportError> {
    rule_report_fixture(RuleFixtureMode::Only)
}

fn integrated_rule_report() -> Result<ReportEnvelope, ReportError> {
    rule_report_fixture(RuleFixtureMode::Integrated)
}

#[derive(Clone, Copy)]
enum RuleFixtureMode {
    Only,
    Integrated,
}

fn rule_report_fixture(mode: RuleFixtureMode) -> Result<ReportEnvelope, ReportError> {
    let rules = RuleReport::new(quality_analysis_fixture())?;
    let sections = match mode {
        RuleFixtureMode::Only => ReportSections {
            crap: None,
            dry: None,
            mutate: None,
            rules: Some(rules),
        },
        RuleFixtureMode::Integrated => ReportSections {
            crap: Some(CrapReport::new(vec![risky_function()], 30.0)),
            dry: Some(DryReport::new(vec![duplicate()], 4)),
            mutate: None,
            rules: Some(rules),
        },
    };
    Ok(report_fixture(context(), sections))
}

fn check_fixture(
    context: ReportContext,
    functions: Vec<FunctionRecord>,
    mutants: Vec<MutationResult>,
    rules: Option<RuleReport>,
) -> ReportEnvelope {
    report_fixture(
        context,
        ReportSections {
            crap: Some(CrapReport::new(functions, 30.0)),
            dry: Some(DryReport::new(vec![duplicate()], 4)),
            mutate: Some(MutationReport::new(mutants)),
            rules,
        },
    )
}

struct ReportSections {
    crap: Option<CrapReport>,
    dry: Option<DryReport>,
    mutate: Option<MutationReport>,
    rules: Option<RuleReport>,
}

fn report_fixture(context: ReportContext, sections: ReportSections) -> ReportEnvelope {
    ReportEnvelope::check(
        context,
        sections.crap,
        sections.dry,
        sections.mutate,
        sections.rules,
    )
}

fn context() -> ReportContext {
    let generic = backend_fixture("generic", false, Capability::Syntax);
    let native = backend_fixture("cargo", true, Capability::ProjectSemantics);
    let mut context = ReportContext::new("/workspace/project");
    context.files = 2;
    context.parse_errors = 1;
    context.backends = vec![generic, native];
    context.diagnostics = vec![
        Diagnostic {
            severity: Severity::Warning,
            backend: "generic".to_owned(),
            message: "recovered syntax".to_owned(),
            location: Some(SourceLocation {
                file: "src/lib.rs".to_owned(),
                start_line: 2,
                start_column: 1,
                end_line: 2,
                end_column: 2,
            }),
            fallback_used: true,
        },
        Diagnostic {
            severity: Severity::Info,
            backend: "cargo".to_owned(),
            message: "workspace selected".to_owned(),
            location: None,
            fallback_used: false,
        },
    ];
    context
}

fn backend_fixture(id: &str, native: bool, capability: Capability) -> BackendInfo {
    BackendInfo {
        id: id.to_owned(),
        version: "1.0".to_owned(),
        native,
        capabilities: BackendCapabilities::new([capability]),
    }
}

fn risky_function() -> FunctionRecord {
    function_fixture("risky", (8, 20), 12, 25.0, 72.75)
}

fn low_risk_function() -> FunctionRecord {
    function_fixture("safe", (1, 4), 2, 100.0, 2.0)
}

fn function_fixture(
    name: &str,
    lines: (u32, u32),
    complexity: u32,
    coverage: f64,
    crap: f64,
) -> FunctionRecord {
    let mut function = FunctionRecord::new(Language::Rust, name, "src/lib.rs", lines.0, lines.1, complexity);
    function.coverage = Some(coverage);
    function.crap = Some(crap);
    function
}

fn duplicate() -> Duplicate {
    duplicate_with_path("src/lib.rs")
}

fn duplicate_with_path(path: &str) -> Duplicate {
    Duplicate {
        token_count: 6,
        locations: vec![
            duplicate_location(path, (10, 12), "crate::first", (20, 26)),
            duplicate_location("src/other.rs", (30, 32), "crate::second", (70, 76)),
        ],
        clone_group_id: Some("clone-group-fixture".to_owned()),
        similarity: Some(1.0),
        statement_count: Some(4),
        algorithm: Some("normalized-token-exact-v1".to_owned()),
    }
}

fn duplicate_location(
    file: &str,
    lines: (u32, u32),
    stable_symbol: &str,
    tokens: (usize, usize),
) -> DuplicateLocation {
    DuplicateLocation {
        file: file.to_owned(),
        start_line: lines.0,
        end_line: lines.1,
        stable_symbol: Some(stable_symbol.to_owned()),
        start_token: tokens.0,
        end_token: tokens.1,
    }
}

fn mutation(id: u64, status: MutationStatus, detail: Option<&str>) -> MutationResult {
    let (file, stable_symbol, operator) = ("src/lib.rs", "crate::sample", "comparison");
    MutationResult {
        mutation: MutationCandidate::new(Language::Rust, file, (1, 32), "==", "!=", 31..33).with_identity(
            id,
            stable_symbol,
            operator,
            format!("fingerprint-{id}"),
        ),
        status,
        exit_code: Some(1),
        duration_seconds: 0.125,
        detail: detail.map(str::to_owned),
    }
}

fn mutation_sources() -> BTreeMap<String, String> {
    BTreeMap::from([(
        "src/lib.rs".to_owned(),
        "fn compare(a: i32, b: i32) { a == b; }\n".to_owned(),
    )])
}

fn mutation_envelope(mutants: Vec<MutationResult>) -> ReportEnvelope {
    ReportEnvelope::mutate(context(), MutationReport::new(mutants))
}
