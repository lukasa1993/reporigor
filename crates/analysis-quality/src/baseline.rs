use std::collections::{BTreeMap, BTreeSet};

use reporigor_core::{
    canonicalize_rule_results, validate_rule_results, BaselineDisposition, RuleOutcome, RuleResult,
    RuleSummary,
};
use serde::{Deserialize, Serialize};

/// Result of comparing current rules with a prior native `RepoRigor` report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineComparison {
    pub summary: RuleSummary,
    pub resolved: usize,
    pub gate_passed: bool,
}

/// Apply existing/new/worsened/improved baseline dispositions in place.
///
/// `previous` must be rule rows extracted from an ordinary prior `RepoRigor`
/// report. There is intentionally no second baseline data model or writer.
/// A missing previous violation is counted as resolved only when that rule ID
/// was evaluated in the current run, which prevents capability-gated omissions
/// from being mistaken for improvements.
///
/// # Errors
///
/// Returns an error for non-canonical paths, ordering, duplicate IDs, or
/// non-finite stored excess values.
pub fn apply_baseline(
    current: &mut [RuleResult],
    previous: Option<&[RuleResult]>,
    enabled: bool,
) -> Result<BaselineComparison, String> {
    apply_baseline_with_incomplete_rules(current, previous, enabled, &BTreeSet::new())
}

/// Apply baseline dispositions while preventing incomplete rule inventories
/// from manufacturing resolved findings.
///
/// # Errors
///
/// Returns the same validation errors as [`apply_baseline`].
pub fn apply_baseline_with_incomplete_rules(
    current: &mut [RuleResult],
    previous: Option<&[RuleResult]>,
    enabled: bool,
    incomplete_rules: &BTreeSet<String>,
) -> Result<BaselineComparison, String> {
    let _ = canonicalize_rule_results(current)?;
    if !enabled {
        return Ok(disabled_baseline(current));
    }
    enabled_baseline(current, previous, incomplete_rules)
}

fn enabled_baseline(
    current: &mut [RuleResult],
    previous: Option<&[RuleResult]>,
    incomplete_rules: &BTreeSet<String>,
) -> Result<BaselineComparison, String> {
    let previous = validated_previous(previous)?;
    let previous_by_id: BTreeMap<_, _> = previous
        .iter()
        .map(|result| (result.violation_id.as_str(), result))
        .collect();
    let evaluated_rules: BTreeSet<String> = current.iter().map(|result| result.rule_id.clone()).collect();
    let current_ids: BTreeSet<String> = current.iter().map(|result| result.violation_id.clone()).collect();
    assign_baseline_dispositions(current, &previous_by_id)?;

    let resolved = resolved_violation_count(previous, &evaluated_rules, incomplete_rules, &current_ids);
    let mut summary = RuleSummary::from_results(current);
    summary.baseline_resolved = resolved;
    let gate_passed = summary.baseline_new == 0 && summary.baseline_worsened == 0;
    Ok(BaselineComparison {
        summary,
        resolved,
        gate_passed,
    })
}

fn disabled_baseline(current: &mut [RuleResult]) -> BaselineComparison {
    for result in current.iter_mut() {
        result.baseline = disabled_disposition(result.result);
    }
    BaselineComparison {
        summary: RuleSummary::from_results(current),
        resolved: 0,
        gate_passed: current.iter().all(|result| result.result == RuleOutcome::Pass),
    }
}

fn disabled_disposition(outcome: RuleOutcome) -> BaselineDisposition {
    if outcome == RuleOutcome::Fail {
        BaselineDisposition::Disabled
    } else {
        BaselineDisposition::NotApplicable
    }
}

fn validated_previous(previous: Option<&[RuleResult]>) -> Result<&[RuleResult], String> {
    let previous = previous.ok_or_else(|| {
        "baseline mode is enabled but the prior RepoRigor report has no rule results".to_string()
    })?;
    validate_rule_results(previous)?;
    if previous.iter().any(|result| !result.excess.is_finite()) {
        return Err("baseline report contains a non-finite rule excess".to_string());
    }
    Ok(previous)
}

fn assign_baseline_dispositions(
    current: &mut [RuleResult],
    previous_by_id: &BTreeMap<&str, &RuleResult>,
) -> Result<(), String> {
    for result in current {
        let previous = previous_by_id.get(result.violation_id.as_str()).copied();
        result.baseline = baseline_disposition(result, previous)?;
    }
    Ok(())
}

fn baseline_disposition(
    result: &RuleResult,
    previous: Option<&RuleResult>,
) -> Result<BaselineDisposition, String> {
    let previous_excess = previous.map(RuleResult::derived_excess).transpose()?;
    if result.result == RuleOutcome::Pass {
        return Ok(passing_baseline_disposition(previous));
    }
    failing_baseline_disposition(result.excess, previous, previous_excess)
}

fn passing_baseline_disposition(previous: Option<&RuleResult>) -> BaselineDisposition {
    if previous.is_some_and(|previous| previous.result == RuleOutcome::Fail) {
        BaselineDisposition::Improved
    } else {
        BaselineDisposition::NotApplicable
    }
}

fn failing_baseline_disposition(
    current_excess: f64,
    previous: Option<&RuleResult>,
    previous_excess: Option<f64>,
) -> Result<BaselineDisposition, String> {
    let Some(previous) = previous else {
        return missing_previous_disposition(previous_excess);
    };
    let Some(previous_excess) = previous_excess else {
        return Err("baseline excess state is internally inconsistent".to_string());
    };
    if previous.result == RuleOutcome::Pass {
        return Ok(BaselineDisposition::Worsened);
    }
    Ok(compare_failing_excess(current_excess, previous_excess))
}

fn compare_failing_excess(current_excess: f64, previous_excess: f64) -> BaselineDisposition {
    if current_excess > previous_excess {
        BaselineDisposition::Worsened
    } else if current_excess < previous_excess {
        BaselineDisposition::Improved
    } else {
        BaselineDisposition::Existing
    }
}

fn missing_previous_disposition(previous_excess: Option<f64>) -> Result<BaselineDisposition, String> {
    if previous_excess.is_some() {
        Err("baseline excess state is internally inconsistent".to_string())
    } else {
        Ok(BaselineDisposition::New)
    }
}

fn resolved_violation_count(
    previous: &[RuleResult],
    evaluated_rules: &BTreeSet<String>,
    incomplete_rules: &BTreeSet<String>,
    current_ids: &BTreeSet<String>,
) -> usize {
    previous
        .iter()
        .filter(|result| result.result == RuleOutcome::Fail)
        .filter(|result| evaluated_rules.contains(result.rule_id.as_str()))
        .filter(|result| !incomplete_rules.contains(result.rule_id.as_str()))
        .filter(|result| !current_ids.contains(result.violation_id.as_str()))
        .count()
}

#[cfg(test)]
mod tests {
    use reporigor_core::{rule_result, RuleComparison, RuleResult};
    use serde_json::json;

    use super::*;

    fn maximum(rule: &str, symbol: &str, measured: u64, allowed: u64) -> RuleResult {
        fixture_rule(
            rule,
            symbol,
            json!(measured),
            json!(allowed),
            RuleComparison::Maximum,
            "stable-shape",
        )
    }

    fn fixture_rule(
        rule: &str,
        symbol: &str,
        measured: serde_json::Value,
        allowed: serde_json::Value,
        comparison: RuleComparison,
        evidence: &str,
    ) -> RuleResult {
        rule_result!(
            rule,
            "src/lib.rs",
            symbol,
            measured,
            allowed,
            "fixture comparison v1",
            comparison,
            evidence,
        )
        .unwrap_or_else(|error| panic!("fixture rule: {error}"))
    }

    #[derive(Clone, Copy)]
    enum RulePairFixture {
        DryBoundary,
        MutationScore,
    }

    fn fixture_rule_pair(fixture: RulePairFixture) -> (Vec<RuleResult>, Vec<RuleResult>) {
        let (rule, symbol, allowed, comparison, previous, current) = match fixture {
            RulePairFixture::DryBoundary => (
                "dry.clone",
                "clone-set",
                serde_json::json!(0.92),
                reporigor_core::RuleComparison::MaximumExclusive,
                (serde_json::json!(0.919_999_999_999), "same-clone-set"),
                (serde_json::json!(0.92), "same-clone-set"),
            ),
            RulePairFixture::MutationScore => (
                "mutation.score",
                "repository mutation set",
                serde_json::json!(0.8),
                reporigor_core::RuleComparison::Minimum,
                (
                    serde_json::json!(0.5),
                    "mutation-score-v1;scoreable=killed,survivor",
                ),
                (serde_json::json!(1.0), "mutation-score-v1;scoreable=killed"),
            ),
        };
        let make = |(measured, evidence)| {
            fixture_rule(rule, symbol, measured, allowed.clone(), comparison, evidence)
        };
        (vec![make(previous)], vec![make(current)])
    }

    fn compare_fixture(current: &mut [RuleResult], previous: &[RuleResult]) -> BaselineComparison {
        apply_baseline(current, Some(previous), true).unwrap_or_else(|error| panic!("baseline: {error}"))
    }

    fn compare_incomplete_fixture(
        current: &mut [RuleResult],
        previous: &[RuleResult],
        incomplete_rule: &str,
    ) -> BaselineComparison {
        let incomplete = BTreeSet::from([incomplete_rule.to_string()]);
        apply_baseline_with_incomplete_rules(current, Some(previous), true, &incomplete)
            .unwrap_or_else(|error| panic!("baseline: {error}"))
    }

    #[test]
    fn classifies_existing_new_worsened_improved_and_resolved() {
        let existing = maximum("kiss.statements", "crate::existing", 5, 3);
        let worsened_before = maximum("kiss.statements", "crate::worse", 5, 3);
        let improved_before = maximum("kiss.statements", "crate::better", 5, 3);
        let resolved_before = maximum("kiss.statements", "crate::removed", 5, 3);
        let previous = vec![
            improved_before,
            existing.clone(),
            resolved_before,
            worsened_before,
        ];

        let mut current = vec![
            existing,
            maximum("kiss.statements", "crate::worse", 7, 3),
            // Still over the threshold, but its excess fell from two to one.
            maximum("kiss.statements", "crate::better", 4, 3),
            maximum("kiss.statements", "crate::new", 6, 3),
        ];
        let comparison = compare_fixture(&mut current, &previous);
        let by_symbol: BTreeMap<_, _> = current
            .iter()
            .map(|result| (result.stable_symbol.as_str(), result.baseline))
            .collect();
        assert_eq!(by_symbol["crate::existing"], BaselineDisposition::Existing);
        assert_eq!(by_symbol["crate::worse"], BaselineDisposition::Worsened);
        assert_eq!(by_symbol["crate::better"], BaselineDisposition::Improved);
        assert_eq!(by_symbol["crate::new"], BaselineDisposition::New);
        assert_eq!(comparison.resolved, 1);
        assert!(!comparison.gate_passed);
    }

    #[test]
    fn disabled_baseline_fails_literal_violations_without_rewriting_debt() {
        let mut current = vec![maximum("kiss.parameters", "crate::run", 7, 6)];
        let comparison =
            apply_baseline(&mut current, None, false).unwrap_or_else(|error| panic!("baseline: {error}"));
        assert_eq!(current[0].baseline, BaselineDisposition::Disabled);
        assert!(!comparison.gate_passed);
    }

    #[test]
    fn a_previous_pass_that_crosses_a_tiny_boundary_is_worsened() {
        let (previous, mut current) = fixture_rule_pair(RulePairFixture::DryBoundary);
        let comparison = compare_fixture(&mut current, &previous);
        assert_eq!(current[0].baseline, BaselineDisposition::Worsened);
        assert!(!comparison.gate_passed);
    }

    #[test]
    fn fractional_excess_round_trip_remains_existing_debt() {
        let rule = fixture_rule(
            "cohesion.module",
            "crate::module",
            serde_json::json!(4.0 / 45.0),
            serde_json::json!(0.1),
            reporigor_core::RuleComparison::Minimum,
            "qualified-owner-function-reference-graph-v1",
        );
        let encoded = serde_json::to_string(&rule).unwrap_or_else(|error| panic!("serialize: {error}"));
        let previous: RuleResult =
            serde_json::from_str(&encoded).unwrap_or_else(|error| panic!("deserialize: {error}"));
        let mut current = [rule];
        let comparison = compare_fixture(&mut current, &[previous]);
        assert_eq!(current[0].baseline, BaselineDisposition::Existing);
        assert!(comparison.gate_passed);
    }

    #[test]
    fn incomplete_rule_inventory_does_not_resolve_missing_rows() {
        let previous = vec![maximum("crap.maximum", "crate::missing", 8, 6)];
        let mut current = vec![maximum("crap.maximum", "crate::present", 7, 6)];
        let comparison = compare_incomplete_fixture(&mut current, &previous, "crap.maximum");
        assert_eq!(comparison.resolved, 0);
    }

    #[test]
    fn non_scoreable_mutant_cannot_resolve_a_prior_mutation_score() {
        let (previous, mut current) = fixture_rule_pair(RulePairFixture::MutationScore);
        let comparison = compare_incomplete_fixture(&mut current, &previous, "mutation.score");
        assert_eq!(comparison.resolved, 0);
        assert!(comparison.gate_passed);
    }
}
