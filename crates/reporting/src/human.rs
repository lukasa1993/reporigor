use reporigor_core::{MutationStatus, Severity};

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
    append_diagnostics(report, &mut lines);

    let mut rendered = lines.join("\n");
    rendered.push('\n');
    rendered
}

fn append_backends(report: &ReportEnvelope, lines: &mut Vec<String>) {
    if report.backends.is_empty() {
        return;
    }
    let backends = report
        .backends
        .iter()
        .map(|backend| {
            let kind = if backend.native { "native" } else { "generic" };
            format!(
                "{} {} ({kind})",
                escape_terminal_text(&backend.id),
                escape_terminal_text(&backend.version)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
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
        let coverage = function
            .coverage
            .map_or_else(|| "unknown".to_owned(), |value| format!("{value:.2}%"));
        let score = function
            .crap
            .map_or_else(|| "unknown".to_owned(), |value| format!("{value:.2}"));
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
        let locations = duplicate
            .locations
            .iter()
            .map(|location| {
                format!(
                    "{}:{}-{}",
                    escape_terminal_text(&location.file),
                    location.start_line,
                    location.end_line
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!(
            "  group {}: {} tokens — {}",
            index + 1,
            duplicate.token_count,
            locations
        ));
    }
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
        "Mutation: {} mutants, {} killed, {} survived, score {}",
        mutation.summary.total, mutation.summary.killed, mutation.summary.survived, score
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

const fn command_name(command: ReportCommand) -> &'static str {
    match command {
        ReportCommand::Crap => "crap",
        ReportCommand::Dry => "dry",
        ReportCommand::Mutate => "mutate",
        ReportCommand::Check => "check",
    }
}

const fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "info",
        Severity::Warning => "warning",
        Severity::Error => "error",
    }
}

const fn mutation_status(status: MutationStatus) -> &'static str {
    match status {
        MutationStatus::Killed => "killed",
        MutationStatus::Survived => "survived",
        MutationStatus::NoCoverage => "no-coverage",
        MutationStatus::CompileError => "compile-error",
        MutationStatus::RuntimeError => "runtime-error",
        MutationStatus::Timeout => "timeout",
        MutationStatus::Invalid => "invalid",
        MutationStatus::Ignored => "ignored",
        MutationStatus::Pending => "pending",
    }
}
