use reporigor_core::{MutationStatus, RuleOutcome, Severity};

use crate::{ReportCommand, ReportEnvelope};

/// Render a compact terminal report. The output is deliberately free of ANSI
/// escapes so it remains readable in logs and can be snapshot-tested.
#[must_use]
pub fn render_human(report: &ReportEnvelope) -> String {
    let mut lines = vec![
        format!(
            "{} {} report",
            escape_terminal_text(&report.tool.name),
            command_name(report.command)
        ),
        format!(
            "root: {}",
            escape_terminal_text(&report.root.display().to_string())
        ),
        format!(
            "summary: {} files, {} findings, {} parse errors, {} diagnostics",
            report.summary.files,
            report.summary.findings,
            report.summary.parse_errors,
            report.summary.diagnostics
        ),
    ];

    append_backends(report, &mut lines);
    append_crap(report, &mut lines);
    append_dry(report, &mut lines);
    append_mutation(report, &mut lines);
    append_rules(report, &mut lines);
    append_diagnostics(report, &mut lines);

    let mut rendered = lines.join("\n");
    rendered.push('\n');
    rendered
}

fn append_backends(report: &ReportEnvelope, lines: &mut Vec<String>) {
    if report.backends.is_empty() {
        return;
    }
    let backends = join_comma_separated(report.backends.iter().map(|backend| {
        let kind = if backend.native { "native" } else { "generic" };
        format!(
            "{} {} ({kind})",
            escape_terminal_text(&backend.id),
            escape_terminal_text(&backend.version)
        )
    }));
    lines.push(format!("backends: {backends}"));
}

fn append_crap(report: &ReportEnvelope, lines: &mut Vec<String>) {
    let Some(crap) = &report.results.crap else {
        return;
    };
    lines.push(String::new());
    lines.push(format!(
        "CRAP: {} functions, {} over {:.2}, {} missing coverage",
        crap.summary.functions, crap.summary.over_limit, crap.summary.limit, crap.summary.missing_coverage
    ));
    for function in &crap.functions {
        let coverage = format_optional_metric(function.coverage, "%");
        let score = format_optional_metric(function.crap, "");
        lines.push(format!(
            "  {}:{} {} — complexity {}, coverage {}, CRAP {}",
            escape_terminal_text(&function.file),
            function.start_line,
            escape_terminal_text(&function.name),
            function.complexity,
            coverage,
            score
        ));
    }
}

fn format_optional_metric(value: Option<f64>, suffix: &str) -> String {
    value.map_or_else(|| "unknown".to_owned(), |number| format!("{number:.2}{suffix}"))
}

fn append_dry(report: &ReportEnvelope, lines: &mut Vec<String>) {
    let Some(dry) = &report.results.dry else {
        return;
    };
    lines.push(String::new());
    lines.push(format!(
        "DRY: {} duplicate groups (minimum {} tokens)",
        dry.summary.groups, dry.summary.min_tokens
    ));
    for (index, duplicate) in dry.duplicates.iter().enumerate() {
        let locations = join_comma_separated(duplicate.locations.iter().map(|location| {
            format!(
                "{}:{}-{}",
                escape_terminal_text(&location.file),
                location.start_line,
                location.end_line
            )
        }));
        lines.push(format!(
            "  group {}: {} tokens — {}",
            index + 1,
            duplicate.token_count,
            locations
        ));
    }
}

fn join_comma_separated(values: impl Iterator<Item = String>) -> String {
    values.collect::<Vec<_>>().join(", ")
}

fn append_mutation(report: &ReportEnvelope, lines: &mut Vec<String>) {
    let Some(mutation) = &report.results.mutate else {
        return;
    };
    lines.push(String::new());
    let score = mutation
        .summary
        .mutation_score
        .map_or_else(|| "n/a".to_owned(), |value| format!("{value:.2}%"));
    lines.push(format!(
        "Mutation: {} mutants, {} killed, {} survived, {} scoreable, score {}",
        mutation.summary.total,
        mutation.summary.killed,
        mutation.summary.survived,
        mutation.summary.scoreable_mutants,
        score
    ));
    for mutant in &mutation.mutants {
        lines.push(format!(
            "  {}:{}:{} #{} {} -> {} [{}]",
            escape_terminal_text(&mutant.mutation.file),
            mutant.mutation.line,
            mutant.mutation.column,
            mutant.mutation.id,
            escape_terminal_text(&mutant.mutation.original),
            escape_terminal_text(&mutant.mutation.replacement),
            mutation_status(mutant.status)
        ));
    }
}

fn append_rules(report: &ReportEnvelope, lines: &mut Vec<String>) {
    let Some(rules) = &report.results.rules else {
        return;
    };
    lines.push(String::new());
    lines.push(format!(
        "Rules: {} evaluated, {} passed, {} failed, {} omitted, {} surviving mutants",
        rules.summary.total,
        rules.summary.passed,
        rules.summary.failed,
        rules.omitted.len(),
        rules.surviving_mutants.len()
    ));
    append_rule_failures(rules, lines);
    append_baseline(rules, lines);
    append_omitted(rules, lines);
}

fn append_rule_failures(rules: &crate::RuleReport, lines: &mut Vec<String>) {
    for result in rules
        .results
        .iter()
        .filter(|result| result.result == RuleOutcome::Fail)
    {
        lines.push(format!(
            "  {} {} {} — measured {}, allowed {} [{}]",
            escape_terminal_text(&result.rule_id),
            escape_terminal_text(&result.file),
            escape_terminal_text(&result.stable_symbol),
            escape_terminal_text(&result.measured.to_string()),
            escape_terminal_text(&result.allowed.to_string()),
            escape_terminal_text(&result.algorithm)
        ));
    }
}

fn append_baseline(rules: &crate::RuleReport, lines: &mut Vec<String>) {
    if let Some(baseline) = &rules.baseline {
        let path = baseline
            .path
            .as_deref()
            .map_or_else(|| "none".to_owned(), escape_terminal_text);
        lines.push(format!(
            "Baseline: {}, path {}, {} existing, {} new, {} worsened, {} improved, {} resolved, gate {}",
            if baseline.enabled { "enabled" } else { "disabled" },
            path,
            rules.summary.baseline_existing,
            rules.summary.baseline_new,
            rules.summary.baseline_worsened,
            rules.summary.baseline_improved,
            rules.summary.baseline_resolved,
            if baseline.gate_passed { "passed" } else { "failed" }
        ));
    }
}

fn append_omitted(rules: &crate::RuleReport, lines: &mut Vec<String>) {
    if !rules.omitted.is_empty() {
        lines.push("Omitted checks:".to_owned());
        for omitted in &rules.omitted {
            lines.push(format!(
                "  {} — {}",
                escape_terminal_text(&omitted.rule_id),
                escape_terminal_text(&omitted.reason)
            ));
        }
    }
}

fn append_diagnostics(report: &ReportEnvelope, lines: &mut Vec<String>) {
    if report.diagnostics.is_empty() {
        return;
    }
    lines.push(String::new());
    lines.push("Diagnostics:".to_owned());
    for diagnostic in &report.diagnostics {
        let location = diagnostic.location.as_ref().map_or_else(String::new, |location| {
            format!(
                " {}:{}:{}",
                escape_terminal_text(&location.file),
                location.start_line,
                location.start_column
            )
        });
        let fallback = if diagnostic.fallback_used {
            " [fallback]"
        } else {
            ""
        };
        lines.push(format!(
            "  {} {}{}: {}{}",
            severity_name(diagnostic.severity),
            escape_terminal_text(&diagnostic.backend),
            location,
            escape_terminal_text(&diagnostic.message),
            fallback
        ));
    }
}

/// Escape terminal control characters and Unicode bidirectional controls.
///
/// Use this for every repository-, tool-, or user-controlled field written to
/// a human terminal stream. Machine-readable serializers intentionally retain
/// the original value.
#[must_use]
pub fn escape_terminal_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if is_terminal_control(character) || is_bidi_control(character) {
            escaped.extend(character.escape_unicode());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

const fn is_terminal_control(character: char) -> bool {
    matches!(character, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}')
}

const fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn command_name(command: ReportCommand) -> &'static str {
    crate::indexed_name("crap|dry|mutate|check", command as usize, "check")
}

fn severity_name(severity: Severity) -> &'static str {
    crate::indexed_name("info|warning|error", severity as usize, "error")
}

fn mutation_status(status: MutationStatus) -> &'static str {
    crate::indexed_name(
        "killed|survived|no-coverage|compile-error|runtime-error|timeout|invalid|ignored|pending",
        status as usize,
        "pending",
    )
}
