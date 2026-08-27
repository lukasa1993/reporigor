use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use analysis_crap::{CoverageApplication, CrapAnalysis};
use analysis_mutate::{BaselineReport, CommandOutcome, MutationMode, MutationRun, RecoveryAction};
use reporigor_core::{
    BackendCapabilities, BackendInfo, Capability, Diagnostic, FunctionRecord, Language, MutationCandidate,
    MutationResult, MutationStatus, Severity, SourceLocation,
};
use reporigor_reporting::{
    CrapReport, DryReport, Duplicate, DuplicateLocation, MutationReport, MutationThresholds, ReportContext,
    ReportEnvelope, ReportError, REPORT_SCHEMA_VERSION,
};

#[test]
fn native_report_is_versioned_summarized_and_deterministic() -> Result<(), Box<dyn Error>> {
    let mut first_context = context();
    first_context.backends.reverse();
    first_context.diagnostics.reverse();
    let first = ReportEnvelope::check(
        first_context,
        Some(CrapReport::new(vec![low_risk_function(), risky_function()], 30.0)),
        Some(DryReport::new(vec![duplicate()], 4)),
        Some(MutationReport::new(vec![mutation(
            7,
            MutationStatus::Survived,
            None,
        )])),
    );

    let second = ReportEnvelope::check(
        context(),
        Some(CrapReport::new(vec![risky_function(), low_risk_function()], 30.0)),
        Some(DryReport::new(vec![duplicate()], 4)),
        Some(MutationReport::new(vec![mutation(
            7,
            MutationStatus::Survived,
            None,
        )])),
    );

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
fn human_report_contains_all_sections_and_fallback_diagnostics() {
    let report = ReportEnvelope::check(
        context(),
        Some(CrapReport::new(vec![risky_function()], 30.0)),
        Some(DryReport::new(vec![duplicate()], 4)),
        Some(MutationReport::new(vec![mutation(
            1,
            MutationStatus::Killed,
            None,
        )])),
    );

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

    let report = ReportEnvelope::check(
        report_context,
        Some(CrapReport::new(vec![function], 30.0)),
        Some(DryReport::new(vec![duplicate], 4)),
        Some(MutationReport::new(vec![mutant])),
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
    let report = ReportEnvelope::check(
        context(),
        Some(CrapReport::new(vec![low_risk_function(), risky_function()], 30.0)),
        Some(DryReport::new(vec![duplicate_with_path("src/copied file.rs")], 4)),
        None,
    );

    let sarif = report.to_sarif_json()?;
    let value: serde_json::Value = serde_json::from_str(&sarif)?;
    assert_eq!(value["version"], "2.1.0");
    assert_eq!(value["$schema"], "https://json.schemastore.org/sarif-2.1.0.json");
    assert_eq!(value["runs"][0]["tool"]["driver"]["name"], "reporigor");
    assert_eq!(
        value["runs"][0]["tool"]["driver"]["rules"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    assert_eq!(value["runs"][0]["results"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        value["runs"][0]["results"][0]["ruleId"],
        "reporigor/crap-threshold"
    );
    assert_eq!(
        value["runs"][0]["results"][1]["ruleId"],
        "reporigor/duplicate-code"
    );
    assert_eq!(
        value["runs"][0]["results"][1]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
        "src/copied%20file.rs"
    );
    assert_eq!(
        value["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"]["startLine"],
        8
    );
    Ok(())
}

#[test]
fn sarif_rejects_mutation_only_reports() {
    let report = ReportEnvelope::mutate(
        context(),
        MutationReport::new(vec![mutation(1, MutationStatus::Pending, None)]),
    );
    assert!(matches!(
        report.to_sarif_json(),
        Err(ReportError::MissingSection("CRAP or DRY"))
    ));
}

#[test]
fn mutation_elements_projection_uses_v2_statuses_and_locations() -> Result<(), Box<dyn Error>> {
    let report = ReportEnvelope::mutate(
        context(),
        MutationReport::new(vec![
            mutation(2, MutationStatus::Invalid, None),
            mutation(1, MutationStatus::Killed, Some("test failed")),
        ]),
    );
    let sources = BTreeMap::from([(
        "src/lib.rs".to_owned(),
        "fn compare(a: i32, b: i32) { a == b; }\n".to_owned(),
    )]);

    let json = report.to_mutation_elements_json(&sources, MutationThresholds::new(55, 85)?)?;
    let value: serde_json::Value = serde_json::from_str(&json)?;
    assert_eq!(value["schemaVersion"], "2.0");
    assert_eq!(value["thresholds"]["low"], 55);
    assert_eq!(value["thresholds"]["high"], 85);
    assert_eq!(value["framework"]["name"], "reporigor");
    assert_eq!(value["files"]["src/lib.rs"]["language"], "rust");
    assert_eq!(
        value["files"]["src/lib.rs"]["source"],
        "fn compare(a: i32, b: i32) { a == b; }\n"
    );

    let mutants = value["files"]["src/lib.rs"]["mutants"]
        .as_array()
        .ok_or("mutants must be an array")?;
    assert_eq!(mutants.len(), 2);
    assert_eq!(mutants[0]["id"], "1");
    assert_eq!(mutants[0]["status"], "Killed");
    assert_eq!(mutants[0]["statusReason"], "test failed");
    assert_eq!(mutants[0]["duration"], 125.0);
    assert_eq!(mutants[0]["location"]["start"]["line"], 1);
    assert_eq!(mutants[0]["location"]["start"]["column"], 32);
    assert_eq!(mutants[0]["location"]["end"]["column"], 34);
    assert_eq!(mutants[1]["status"], "Ignored");
    assert_eq!(
        mutants[1]["statusReason"],
        "The mutation candidate was rejected before execution."
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
    let report = ReportEnvelope::mutate(context(), MutationReport::new(vec![result]));
    let sources = BTreeMap::from([("src/lib.rs".to_owned(), "😀 ==\n".to_owned())]);

    let value: serde_json::Value =
        serde_json::from_str(&report.to_mutation_elements_json(&sources, MutationThresholds::default())?)?;
    let location = &value["files"]["src/lib.rs"]["mutants"][0]["location"];
    assert_eq!(location["start"]["line"], 1);
    assert_eq!(location["start"]["column"], 3);
    assert_eq!(location["end"]["line"], 1);
    assert_eq!(location["end"]["column"], 5);
    Ok(())
}

#[test]
fn mutation_elements_rejects_stale_spans_and_coordinates() {
    let source = "fn compare(a: i32, b: i32) { a == b; }\n";
    let sources = BTreeMap::from([("src/lib.rs".to_owned(), source.to_owned())]);

    let mut stale = mutation(1, MutationStatus::Pending, None);
    stale.mutation.start_byte = 30;
    stale.mutation.end_byte = 32;
    let report = ReportEnvelope::mutate(context(), MutationReport::new(vec![stale]));
    assert!(matches!(
        report.to_mutation_elements_json(&sources, MutationThresholds::default()),
        Err(ReportError::InvalidMutationSpan { message, .. })
            if message.contains("original text does not match")
    ));

    let mut wrong_coordinate = mutation(2, MutationStatus::Pending, None);
    wrong_coordinate.mutation.line = 0;
    wrong_coordinate.mutation.column = 0;
    let report = ReportEnvelope::mutate(context(), MutationReport::new(vec![wrong_coordinate]));
    assert!(matches!(
        report.to_mutation_elements_json(&sources, MutationThresholds::default()),
        Err(ReportError::InvalidMutationSpan { message, .. })
            if message.contains("recorded start 0:0")
    ));
}

#[test]
fn mutation_elements_requires_source_for_every_mutated_file() {
    let report = ReportEnvelope::mutate(
        context(),
        MutationReport::new(vec![mutation(1, MutationStatus::Pending, None)]),
    );
    let error = report.to_mutation_elements_json(&BTreeMap::new(), MutationThresholds::default());
    assert!(matches!(
        error,
        Err(ReportError::MissingMutationSource(path)) if path == "src/lib.rs"
    ));
}

#[test]
fn mutation_score_uses_mutation_elements_detected_and_valid_states() {
    let report = MutationReport::new(vec![
        mutation(1, MutationStatus::Killed, None),
        mutation(2, MutationStatus::Timeout, None),
        mutation(3, MutationStatus::Survived, None),
        mutation(4, MutationStatus::NoCoverage, None),
        mutation(5, MutationStatus::CompileError, None),
    ]);

    // Detected = killed + timeout = 2. Valid = detected + survived +
    // no-coverage = 4. Compile errors are invalid and excluded.
    assert_eq!(report.summary.mutation_score, Some(50.0));
}

#[test]
fn analyzer_native_results_feed_reporting_sections_directly() -> Result<(), Box<dyn Error>> {
    let mut uncovered = low_risk_function();
    uncovered.name = "uncovered".to_owned();
    uncovered.start_line = 30;
    uncovered.end_line = 34;
    uncovered.coverage = None;
    uncovered.crap = None;
    let coverage = CoverageApplication {
        total_functions: 3,
        matched_functions: 2,
        unmatched_functions: 1,
        empty_ranges: 0,
    };
    let crap = CrapReport::from_analysis(
        CrapAnalysis {
            functions: vec![uncovered, low_risk_function(), risky_function()],
            coverage: Some(coverage),
        },
        30.0,
    );
    assert_eq!(crap.summary.over_limit, 1);
    assert_eq!(crap.coverage, Some(coverage));
    assert_eq!(crap.functions[0].name, "risky");
    assert_eq!(crap.functions[1].name, "safe");
    assert_eq!(crap.functions[2].name, "uncovered");

    let pending = MutationReport::pending(vec![mutation(9, MutationStatus::Killed, None).mutation]);
    assert_eq!(pending.summary.pending, 1);

    let executed = MutationReport::from_run(MutationRun {
        root: PathBuf::from("/workspace/project"),
        mode: MutationMode::Execute,
        recovery: RecoveryAction::Restored,
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
        results: vec![mutation(9, MutationStatus::Killed, None)],
    });
    assert_eq!(executed.summary.killed, 1);
    assert!(matches!(
        executed.run.as_ref(),
        Some(run)
            if run.mode == MutationMode::Execute
                && run.recovery == RecoveryAction::Restored
                && run.baseline.validation.is_some()
    ));
    let envelope = ReportEnvelope::mutate(context(), executed);
    let serialized = envelope.to_pretty_json()?;
    let value: serde_json::Value = serde_json::from_str(&serialized)?;
    assert_eq!(value["results"]["mutate"]["run"]["mode"], "execute");
    assert_eq!(value["results"]["mutate"]["run"]["recovery"], "restored");
    assert_eq!(
        value["results"]["mutate"]["run"]["baseline"]["validation"]["output"],
        "validation passed"
    );
    let round_trip: ReportEnvelope = serde_json::from_str(&serialized)?;
    assert_eq!(round_trip, envelope);
    Ok(())
}

#[test]
fn timeout_is_reported_as_an_operational_mutation_error() {
    let report = ReportEnvelope::mutate(
        context(),
        MutationReport::new(vec![mutation(1, MutationStatus::Timeout, None)]),
    );
    assert_eq!(report.summary.mutation_errors, 1);
}

#[test]
fn shipped_schemas_accept_representative_serializer_output() -> Result<(), Box<dyn Error>> {
    let native_schema = load_shipped_schema("reporigor-report-v1.schema.json")?;
    let mutation_schema = load_shipped_schema("mutation-testing-elements-v2.schema.json")?;
    for (name, schema) in [
        ("reporigor-report-v1.schema.json", &native_schema),
        ("mutation-testing-elements-v2.schema.json", &mutation_schema),
    ] {
        assert_eq!(
            schema.get("$schema").and_then(serde_json::Value::as_str),
            Some("https://json-schema.org/draft/2020-12/schema"),
            "{name} must declare the supported schema dialect"
        );
        ensure_supported_schema(schema, "$").map_err(invalid_schema)?;
    }

    let all_mutations = [
        MutationStatus::Killed,
        MutationStatus::Survived,
        MutationStatus::NoCoverage,
        MutationStatus::CompileError,
        MutationStatus::RuntimeError,
        MutationStatus::Timeout,
        MutationStatus::Invalid,
        MutationStatus::Ignored,
        MutationStatus::Pending,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, status)| mutation(u64::try_from(index + 1).unwrap_or(u64::MAX), status, None))
    .collect::<Vec<_>>();

    let native_reports = [
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
                results: all_mutations.clone(),
            }),
        ),
        ReportEnvelope::check(
            context(),
            Some(CrapReport::new(vec![risky_function()], 30.0)),
            Some(DryReport::new(vec![duplicate()], 4)),
            Some(MutationReport::new(all_mutations.clone())),
        ),
    ];
    for report in native_reports {
        let serialized: serde_json::Value = serde_json::from_str(&report.to_pretty_json()?)?;
        validate_instance(&serialized, &native_schema, &native_schema, "$").map_err(invalid_schema)?;
    }

    let mutation_report = ReportEnvelope::mutate(context(), MutationReport::new(all_mutations));
    let sources = BTreeMap::from([(
        "src/lib.rs".to_owned(),
        "fn compare(a: i32, b: i32) { a == b; }\n".to_owned(),
    )]);
    let serialized: serde_json::Value = serde_json::from_str(
        &mutation_report.to_mutation_elements_json(&sources, MutationThresholds::default())?,
    )?;
    validate_instance(&serialized, &mutation_schema, &mutation_schema, "$").map_err(invalid_schema)?;
    Ok(())
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
        return if schema.is_boolean() {
            Ok(())
        } else {
            Err(format!("{path}: a schema must be an object or boolean"))
        };
    };
    for (keyword, value) in object {
        match keyword.as_str() {
            "$schema" | "$id" | "title" | "description" | "$ref" | "type" | "required" | "const" | "enum"
            | "minimum" | "maximum" | "minLength" | "uniqueItems" => {}
            "properties" | "$defs" => {
                let entries = value
                    .as_object()
                    .ok_or_else(|| format!("{path}/{keyword}: expected an object"))?;
                for (name, child) in entries {
                    ensure_supported_schema(child, &format!("{path}/{keyword}/{name}"))?;
                }
            }
            "items" | "additionalProperties" | "if" | "then" => {
                ensure_supported_schema(value, &format!("{path}/{keyword}"))?;
            }
            "allOf" => {
                let entries = value
                    .as_array()
                    .ok_or_else(|| format!("{path}/allOf: expected an array"))?;
                for (index, child) in entries.iter().enumerate() {
                    ensure_supported_schema(child, &format!("{path}/allOf/{index}"))?;
                }
            }
            unsupported => {
                return Err(format!(
                    "{path}: unsupported JSON Schema keyword {unsupported:?}; extend the contract validator"
                ));
            }
        }
    }
    Ok(())
}

// Keeping the supported keyword evaluation in one routine makes it explicit
// that this focused test validator is not a general-purpose JSON Schema API.
#[allow(clippy::too_many_lines)]
fn validate_instance(
    instance: &serde_json::Value,
    schema: &serde_json::Value,
    root: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    if let Some(accepted) = schema.as_bool() {
        return if accepted {
            Ok(())
        } else {
            Err(format!("{path}: rejected by boolean schema"))
        };
    }
    let object = schema
        .as_object()
        .ok_or_else(|| format!("{path}: schema is not an object"))?;

    if let Some(reference) = object.get("$ref").and_then(serde_json::Value::as_str) {
        let pointer = reference
            .strip_prefix('#')
            .ok_or_else(|| format!("{path}: only local schema references are supported"))?;
        let target = root
            .pointer(pointer)
            .ok_or_else(|| format!("{path}: unresolved schema reference {reference:?}"))?;
        validate_instance(instance, target, root, path)?;
    }

    if let Some(expected) = object.get("type") {
        let accepted = match expected {
            serde_json::Value::String(name) => instance_has_type(instance, name)?,
            serde_json::Value::Array(names) => {
                let mut accepted = false;
                for name in names {
                    let name = name
                        .as_str()
                        .ok_or_else(|| format!("{path}: schema type array contains a non-string"))?;
                    accepted |= instance_has_type(instance, name)?;
                }
                accepted
            }
            _ => return Err(format!("{path}: schema type must be a string or array")),
        };
        if !accepted {
            return Err(format!("{path}: value does not match schema type {expected}"));
        }
    }

    if let Some(expected) = object.get("const") {
        if instance != expected {
            return Err(format!("{path}: expected constant {expected}, found {instance}"));
        }
    }
    if let Some(choices) = object.get("enum") {
        let choices = choices
            .as_array()
            .ok_or_else(|| format!("{path}: schema enum must be an array"))?;
        if !choices.contains(instance) {
            return Err(format!("{path}: {instance} is not one of the allowed values"));
        }
    }

    if let Some(fields) = instance.as_object() {
        if let Some(required) = object.get("required") {
            let required = required
                .as_array()
                .ok_or_else(|| format!("{path}: schema required must be an array"))?;
            for name in required {
                let name = name
                    .as_str()
                    .ok_or_else(|| format!("{path}: required field name is not a string"))?;
                if !fields.contains_key(name) {
                    return Err(format!("{path}: required field {name:?} is absent"));
                }
            }
        }
        let properties = object.get("properties").and_then(serde_json::Value::as_object);
        if let Some(properties) = properties {
            for (name, child_schema) in properties {
                if let Some(value) = fields.get(name) {
                    validate_instance(value, child_schema, root, &format!("{path}/{name}"))?;
                }
            }
        }
        if let Some(additional) = object.get("additionalProperties") {
            for (name, value) in fields {
                if properties.is_some_and(|known| known.contains_key(name)) {
                    continue;
                }
                match additional {
                    serde_json::Value::Bool(true) => {}
                    serde_json::Value::Bool(false) => {
                        return Err(format!("{path}: additional field {name:?} is not allowed"));
                    }
                    child_schema => {
                        validate_instance(value, child_schema, root, &format!("{path}/{name}"))?;
                    }
                }
            }
        }
    }

    if let Some(items) = instance.as_array() {
        if object.get("uniqueItems").and_then(serde_json::Value::as_bool) == Some(true) {
            for (index, value) in items.iter().enumerate() {
                if items[..index].contains(value) {
                    return Err(format!("{path}/{index}: array item is not unique"));
                }
            }
        }
        if let Some(item_schema) = object.get("items") {
            for (index, value) in items.iter().enumerate() {
                validate_instance(value, item_schema, root, &format!("{path}/{index}"))?;
            }
        }
    }

    if let Some(text) = instance.as_str() {
        if let Some(minimum) = object.get("minLength").and_then(serde_json::Value::as_u64) {
            let length = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
            if length < minimum {
                return Err(format!("{path}: string length {length} is below {minimum}"));
            }
        }
    }
    if let Some(number) = instance.as_f64() {
        if let Some(minimum) = object.get("minimum").and_then(serde_json::Value::as_f64) {
            if number < minimum {
                return Err(format!("{path}: number {number} is below {minimum}"));
            }
        }
        if let Some(maximum) = object.get("maximum").and_then(serde_json::Value::as_f64) {
            if number > maximum {
                return Err(format!("{path}: number {number} exceeds {maximum}"));
            }
        }
    }

    if let Some(all_of) = object.get("allOf").and_then(serde_json::Value::as_array) {
        for child_schema in all_of {
            validate_instance(instance, child_schema, root, path)?;
        }
    }
    if let Some(condition) = object.get("if") {
        if validate_instance(instance, condition, root, path).is_ok() {
            if let Some(consequence) = object.get("then") {
                validate_instance(instance, consequence, root, path)?;
            }
        }
    }
    Ok(())
}

fn instance_has_type(instance: &serde_json::Value, name: &str) -> Result<bool, String> {
    match name {
        "null" => Ok(instance.is_null()),
        "boolean" => Ok(instance.is_boolean()),
        "object" => Ok(instance.is_object()),
        "array" => Ok(instance.is_array()),
        "number" => Ok(instance.is_number()),
        "integer" => Ok(instance.as_i64().is_some() || instance.as_u64().is_some()),
        "string" => Ok(instance.is_string()),
        unsupported => Err(format!("unsupported JSON Schema type {unsupported:?}")),
    }
}

fn context() -> ReportContext {
    let generic = BackendInfo {
        id: "generic".to_owned(),
        version: "1.0".to_owned(),
        native: false,
        capabilities: BackendCapabilities::new([Capability::Syntax]),
    };
    let native = BackendInfo {
        id: "cargo".to_owned(),
        version: "1.0".to_owned(),
        native: true,
        capabilities: BackendCapabilities {
            capabilities: BTreeSet::from([Capability::ProjectSemantics]),
        },
    };
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

fn risky_function() -> FunctionRecord {
    FunctionRecord {
        language: Language::Rust,
        name: "risky".to_owned(),
        file: "src/lib.rs".to_owned(),
        start_line: 8,
        end_line: 20,
        complexity: 12,
        coverage: Some(25.0),
        crap: Some(72.75),
    }
}

fn low_risk_function() -> FunctionRecord {
    FunctionRecord {
        language: Language::Rust,
        name: "safe".to_owned(),
        file: "src/lib.rs".to_owned(),
        start_line: 1,
        end_line: 4,
        complexity: 2,
        coverage: Some(100.0),
        crap: Some(2.0),
    }
}

fn duplicate() -> Duplicate {
    duplicate_with_path("src/lib.rs")
}

fn duplicate_with_path(path: &str) -> Duplicate {
    Duplicate {
        token_count: 6,
        locations: vec![
            DuplicateLocation {
                file: path.to_owned(),
                start_line: 10,
                end_line: 12,
                start_token: 20,
                end_token: 26,
            },
            DuplicateLocation {
                file: "src/other.rs".to_owned(),
                start_line: 30,
                end_line: 32,
                start_token: 70,
                end_token: 76,
            },
        ],
    }
}

fn mutation(id: u64, status: MutationStatus, detail: Option<&str>) -> MutationResult {
    MutationResult {
        mutation: MutationCandidate {
            id,
            language: Language::Rust,
            file: "src/lib.rs".to_owned(),
            line: 1,
            column: 32,
            original: "==".to_owned(),
            replacement: "!=".to_owned(),
            start_byte: 31,
            end_byte: 33,
        },
        status,
        exit_code: Some(1),
        duration_seconds: 0.125,
        detail: detail.map(str::to_owned),
    }
}
