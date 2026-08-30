use std::collections::BTreeMap;

use reporigor_core::{FunctionRecord, RuleOutcome, RuleResult};
use serde::Serialize;
use serde_json::Value;

use crate::{pretty_json, ReportEnvelope, ReportError, SARIF_VERSION};

const SARIF_SCHEMA: &str = "https://json.schemastore.org/sarif-2.1.0.json";
const CRAP_RULE_ID: &str = "reporigor/crap-threshold";
const DRY_RULE_ID: &str = "reporigor/duplicate-code";
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Project CRAP, DRY, and file-backed deterministic rule findings into SARIF.
///
/// Mutation results are intentionally omitted because SARIF does not preserve
/// the mutation-testing status model. Use Mutation Testing Elements instead.
///
/// # Errors
///
/// Returns an error when no supported section exists or serialization fails.
pub fn sarif_value(report: &ReportEnvelope) -> Result<Value, ReportError> {
    require_supported_section(report)?;
    let mut rules = Vec::new();
    let mut results = Vec::new();
    append_crap_results(report, &mut rules, &mut results);
    append_dry_results(report, &mut rules, &mut results);
    append_rule_results(report, &mut rules, &mut results);
    Ok(serde_json::to_value(sarif_log(report, rules, results))?)
}

fn require_supported_section(report: &ReportEnvelope) -> Result<(), ReportError> {
    let has_results =
        report.results.crap.is_some() || report.results.dry.is_some() || report.results.rules.is_some();
    has_results
        .then_some(())
        .ok_or(ReportError::MissingSection("CRAP, DRY, or rules"))
}

fn append_crap_results(report: &ReportEnvelope, rules: &mut Vec<SarifRule>, results: &mut Vec<SarifResult>) {
    if let Some(crap) = &report.results.crap {
        rules.push(SarifRule {
            id: CRAP_RULE_ID.to_owned(),
            short_description: SarifMessage {
                text: "Function exceeds the configured CRAP threshold".to_owned(),
            },
        });
        results.extend(
            crap.functions
                .iter()
                .filter(|function| function.crap.is_some_and(|score| score > crap.summary.limit))
                .map(|function| crap_result(function, crap.summary.limit)),
        );
    }
}

fn append_dry_results(report: &ReportEnvelope, rules: &mut Vec<SarifRule>, results: &mut Vec<SarifResult>) {
    if let Some(dry) = &report.results.dry {
        rules.push(SarifRule {
            id: DRY_RULE_ID.to_owned(),
            short_description: SarifMessage {
                text: "Duplicated source token sequence".to_owned(),
            },
        });
        for (index, duplicate) in dry.duplicates.iter().enumerate() {
            results.push(SarifResult {
                rule_id: DRY_RULE_ID.to_owned(),
                level: "warning",
                message: SarifMessage {
                    text: format!(
                        "Duplicate group {} contains {} tokens in {} locations.",
                        index + 1,
                        duplicate.token_count,
                        duplicate.locations.len()
                    ),
                },
                locations: duplicate
                    .locations
                    .iter()
                    .map(|location| sarif_location(&location.file, location.start_line, location.end_line))
                    .collect(),
            });
        }
    }
}

fn append_rule_results(report: &ReportEnvelope, rules: &mut Vec<SarifRule>, results: &mut Vec<SarifResult>) {
    if let Some(rule_report) = &report.results.rules {
        let mut dynamic_rules = BTreeMap::<String, String>::new();
        for result in rule_report
            .results
            .iter()
            .filter(|result| result.result == RuleOutcome::Fail)
            .filter(|result| !result.file.is_empty())
            .filter(|result| !duplicates_legacy_sarif_result(result, report))
        {
            dynamic_rules
                .entry(result.rule_id.clone())
                .or_insert_with(|| result.algorithm.clone());
            results.push(rule_result(result));
        }
        for (id, algorithm) in dynamic_rules {
            if rules.iter().any(|rule| rule.id == id) {
                continue;
            }
            rules.push(SarifRule {
                id,
                short_description: SarifMessage {
                    text: format!("RepoRigor deterministic rule evaluated by {algorithm}"),
                },
            });
        }
    }
}

fn sarif_log(report: &ReportEnvelope, rules: Vec<SarifRule>, results: Vec<SarifResult>) -> SarifLog<'_> {
    SarifLog {
        schema: SARIF_SCHEMA,
        version: SARIF_VERSION,
        runs: vec![SarifRun {
            tool: SarifTool {
                driver: SarifDriver {
                    name: &report.tool.name,
                    version: &report.tool.version,
                    rules,
                },
            },
            results,
        }],
    }
}

/// Serialize the SARIF projection as deterministic pretty JSON.
///
/// # Errors
///
/// Returns the same projection and serialization errors as [`sarif_value`].
pub fn sarif_json(report: &ReportEnvelope) -> Result<String, ReportError> {
    pretty_json(&sarif_value(report)?)
}

fn duplicates_legacy_sarif_result(result: &RuleResult, report: &ReportEnvelope) -> bool {
    (report.results.crap.is_some() && result.rule_id.starts_with("crap."))
        || (report.results.dry.is_some() && result.rule_id.starts_with("dry."))
}

fn crap_result(function: &FunctionRecord, limit: f64) -> SarifResult {
    let score = function.crap.unwrap_or_default();
    SarifResult {
        rule_id: CRAP_RULE_ID.to_owned(),
        level: "warning",
        message: SarifMessage {
            text: format!(
                "Function `{}` has CRAP score {score:.2}, exceeding the configured limit {limit:.2}.",
                function.name
            ),
        },
        locations: vec![sarif_location(
            &function.file,
            function.start_line,
            function.end_line,
        )],
    }
}

fn rule_result(result: &RuleResult) -> SarifResult {
    SarifResult {
        rule_id: result.rule_id.clone(),
        level: "warning",
        message: SarifMessage {
            text: format!(
                "`{}` measured {} with allowed value {} ({}). Violation ID {}.",
                result.stable_symbol, result.measured, result.allowed, result.algorithm, result.violation_id
            ),
        },
        locations: vec![sarif_file_location(&result.file)],
    }
}

fn sarif_location(file: &str, start_line: u32, end_line: u32) -> SarifLocation {
    let start_line = start_line.max(1);
    SarifLocation {
        physical_location: SarifPhysicalLocation {
            artifact_location: SarifArtifactLocation { uri: path_uri(file) },
            region: Some(SarifRegion {
                start_line,
                end_line: end_line.max(start_line),
            }),
        },
    }
}

fn sarif_file_location(file: &str) -> SarifLocation {
    SarifLocation {
        physical_location: SarifPhysicalLocation {
            artifact_location: SarifArtifactLocation { uri: path_uri(file) },
            region: None,
        },
    }
}

fn path_uri(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let mut encoded = String::with_capacity(normalized.len());
    for byte in normalized.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'-' | b'_' | b'~' | b':') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

#[derive(Debug, Serialize)]
struct SarifLog<'a> {
    #[serde(rename = "$schema")]
    schema: &'static str,
    version: &'static str,
    runs: Vec<SarifRun<'a>>,
}

#[derive(Debug, Serialize)]
struct SarifRun<'a> {
    tool: SarifTool<'a>,
    results: Vec<SarifResult>,
}

#[derive(Debug, Serialize)]
struct SarifTool<'a> {
    driver: SarifDriver<'a>,
}

#[derive(Debug, Serialize)]
struct SarifDriver<'a> {
    name: &'a str,
    version: &'a str,
    rules: Vec<SarifRule>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifRule {
    id: String,
    short_description: SarifMessage,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifResult {
    rule_id: String,
    level: &'static str,
    message: SarifMessage,
    locations: Vec<SarifLocation>,
}

#[derive(Debug, Serialize)]
struct SarifMessage {
    text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifLocation {
    physical_location: SarifPhysicalLocation,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifPhysicalLocation {
    artifact_location: SarifArtifactLocation,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<SarifRegion>,
}

#[derive(Debug, Serialize)]
struct SarifArtifactLocation {
    uri: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifRegion {
    start_line: u32,
    end_line: u32,
}
