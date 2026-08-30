use std::collections::{BTreeMap, BTreeSet};

use analysis_dry::{Duplicate, CLONE_RULE_ID};
use analysis_mutate::mutation_score;
use reporigor_core::{
    canonicalize_rule_results, rule_result, ArchitectureConfig, DependencyRecord, DependencyScope,
    FunctionRecord, MutationCandidate, MutationResult, MutationStatus, PackageRecord, RepoRigorConfig,
    RepositorySemantics, RuleComparison, RuleResult, SymbolVisibility, TraitImplementationRecord,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    afferent_coupling, count_as_f64, dependency_cycles, efferent_coupling, instability, matches_pattern,
    parse_edge_pattern,
};

macro_rules! push_rule {
    (
        $output:expr,
        $rule_id:expr,
        $file:expr,
        $symbol:expr,
        $measured:expr,
        $allowed:expr,
        $algorithm:expr,
        $comparison:expr,
        $evidence:expr $(,)?
    ) => {
        rule_result!(
            $rule_id,
            $file,
            $symbol,
            $measured,
            $allowed,
            $algorithm,
            $comparison,
            $evidence,
        )
        .map(|result| $output.push(result))
    };
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurvivingMutant {
    pub file: String,
    pub stable_symbol: String,
    pub operator: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OmittedCheck {
    pub rule_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QualityAnalysis {
    pub formulas: BTreeMap<String, String>,
    pub results: Vec<RuleResult>,
    pub surviving_mutants: Vec<SurvivingMutant>,
    pub omitted: Vec<OmittedCheck>,
}

#[derive(Debug, Clone, Copy)]
pub struct QualityInput<'a> {
    pub config: &'a RepoRigorConfig,
    pub functions: &'a [FunctionRecord],
    pub duplicates: &'a [Duplicate],
    pub mutations: &'a [MutationResult],
    pub repository: &'a RepositorySemantics,
}

struct FunctionRuleContext<'a> {
    analysis: &'a mut QualityAnalysis,
    functions: &'a [FunctionRecord],
    config: &'a RepoRigorConfig,
}

struct YagniContext<'a> {
    analysis: &'a mut QualityAnalysis,
    functions: &'a [FunctionRecord],
    repository: &'a RepositorySemantics,
    config: &'a RepoRigorConfig,
}

struct RepositoryRuleContext<'a> {
    analysis: &'a mut QualityAnalysis,
    repository: &'a RepositorySemantics,
    config: &'a RepoRigorConfig,
}

/// Evaluate deterministic quality rules over adapter-produced facts.
///
/// Capability-dependent checks are omitted explicitly when their adapter
/// inventory is not reliable. This prevents absence of project semantics from
/// being misreported as success or as unused code.
///
/// # Errors
///
/// Returns an error when a rule cannot produce a canonical stable identity or
/// when configured dependency predicates are ambiguous or invalid.
pub fn analyze_rules(input: QualityInput<'_>) -> Result<QualityAnalysis, String> {
    let mut analysis = QualityAnalysis {
        formulas: formulas(),
        ..QualityAnalysis::default()
    };
    append_primary_rules(&mut analysis, input)?;
    append_repository_rules(&mut analysis, input)?;
    finalize_analysis(&mut analysis)?;
    Ok(analysis)
}

fn append_primary_rules(analysis: &mut QualityAnalysis, input: QualityInput<'_>) -> Result<(), String> {
    append_function_rules(analysis, input.functions, input.config)?;
    append_dry(analysis, input.duplicates, input.config)?;
    append_repository_structure_rules(analysis, input.repository, input.config)
}

fn append_repository_rules(analysis: &mut QualityAnalysis, input: QualityInput<'_>) -> Result<(), String> {
    append_mutation(analysis, input.mutations, input.config)?;
    append_yagni(analysis, input.functions, input.repository, input.config)?;
    Ok(())
}

fn append_function_rules(
    analysis: &mut QualityAnalysis,
    functions: &[FunctionRecord],
    config: &RepoRigorConfig,
) -> Result<(), String> {
    let mut context = FunctionRuleContext {
        analysis,
        functions,
        config,
    };
    append_crap(&mut context)?;
    append_kiss(&mut context)?;
    append_cohesion(&mut context)
}

fn append_repository_structure_rules(
    analysis: &mut QualityAnalysis,
    repository: &RepositorySemantics,
    config: &RepoRigorConfig,
) -> Result<(), String> {
    let mut context = RepositoryRuleContext {
        analysis,
        repository,
        config,
    };
    append_dependency_count_rules(&mut context)?;
    append_architecture(&mut context)
}

fn finalize_analysis(analysis: &mut QualityAnalysis) -> Result<(), String> {
    let _ = canonicalize_rule_results(&mut analysis.results)?;
    analysis
        .surviving_mutants
        .sort_by(|left, right| left.fingerprint.cmp(&right.fingerprint));
    analysis
        .surviving_mutants
        .dedup_by(|left, right| left.fingerprint == right.fingerprint);
    analysis.omitted.sort();
    analysis.omitted.dedup();
    Ok(())
}

fn formulas() -> BTreeMap<String, String> {
    const ROWS: &str = "crap\tcomplexity^2 * (1 - coverage)^3 + complexity; coverage is the covered executable-line fraction for the function
dry_similarity\tmultiset Sorensen-Dice similarity = 2 * shared normalized-token shingle occurrences / total shingle occurrences; a clone violates when similarity >= the configured threshold
mutation_score\tkilled / scoreable_mutants; scoreable statuses are exactly killed and survived
afferent_coupling\tnumber of repository packages with a direct production dependency on the package
efferent_coupling\tnumber of distinct direct production dependencies of the package
instability\tCe / (Ca + Ce), or 0 when Ca + Ce = 0
module_cohesion\trelated function pairs / all function pairs within an adapter-qualified owner; a pair is related by a shared exact trait-implementation contract, a uniquely resolved direct call, a shared uniquely resolved local callee, or a shared non-local reference; singleton = 1";
    ROWS.lines()
        .filter_map(|row| row.split_once('\t'))
        .map(|(name, formula)| (name.to_owned(), formula.to_owned()))
        .collect()
}

fn append_dry(
    analysis: &mut QualityAnalysis,
    duplicates: &[Duplicate],
    config: &RepoRigorConfig,
) -> Result<(), String> {
    let evaluable = evaluable_duplicates(duplicates, config);
    if evaluable.is_empty() {
        return append_empty_dry_rule(analysis, config);
    }
    append_duplicate_rules(analysis, &evaluable, config)
}

fn evaluable_duplicates<'a>(duplicates: &'a [Duplicate], config: &RepoRigorConfig) -> Vec<&'a Duplicate> {
    duplicates
        .iter()
        .filter(|duplicate| duplicate_has_enough_statements(duplicate, config.dry.min_statements))
        .collect()
}

fn duplicate_has_enough_statements(duplicate: &Duplicate, minimum: usize) -> bool {
    duplicate
        .statement_count
        .is_some_and(|statements| usize::try_from(statements).unwrap_or(usize::MAX) >= minimum)
}

fn append_empty_dry_rule(analysis: &mut QualityAnalysis, config: &RepoRigorConfig) -> Result<(), String> {
    push_rule!(
        &mut analysis.results,
        CLONE_RULE_ID,
        "Cargo.toml",
        "repository clone inventory",
        json!(0.0),
        json!(config.dry.similarity_threshold),
        "normalized-token clone inventory contains no group at or above the configured similarity threshold",
        RuleComparison::MaximumExclusive,
        "empty-clone-inventory-v1",
    )?;
    Ok(())
}

fn append_duplicate_rules(
    analysis: &mut QualityAnalysis,
    duplicates: &[&Duplicate],
    config: &RepoRigorConfig,
) -> Result<(), String> {
    for duplicate in duplicates {
        append_duplicate_rule(analysis, duplicate, config)?;
    }
    Ok(())
}

fn append_duplicate_rule(
    analysis: &mut QualityAnalysis,
    duplicate: &Duplicate,
    config: &RepoRigorConfig,
) -> Result<(), String> {
    let Some(first) = duplicate.locations.first() else {
        return Ok(());
    };
    let symbol = duplicate_identity(duplicate);
    let evidence = duplicate.clone_group_id.clone().unwrap_or_else(|| symbol.clone());
    let result = rule_result!(
        CLONE_RULE_ID,
        &first.file,
        &symbol,
        json!(duplicate.similarity.unwrap_or(1.0)),
        json!(config.dry.similarity_threshold),
        duplicate
            .algorithm
            .as_deref()
            .unwrap_or("normalized-token-exact-v1"),
        RuleComparison::MaximumExclusive,
        &evidence,
    )?;
    analysis.results.push(result);
    Ok(())
}

fn duplicate_identity(duplicate: &Duplicate) -> String {
    let mut identities = duplicate
        .locations
        .iter()
        .map(|location| {
            format!(
                "{}#{}",
                location.file,
                location.stable_symbol.as_deref().unwrap_or("<token-region>")
            )
        })
        .collect::<Vec<_>>();
    identities.sort();
    identities.dedup();
    identities.join("|")
}

fn append_crap(context: &mut FunctionRuleContext<'_>) -> Result<(), String> {
    let scored = append_crap_rules(context)?;
    append_crap_omission(context.analysis, scored, context.functions.len());
    Ok(())
}

fn append_crap_rules(context: &mut FunctionRuleContext<'_>) -> Result<usize, String> {
    let mut scored = 0_usize;
    for function in context.functions {
        let Some(score) = function.crap else {
            continue;
        };
        scored = scored.saturating_add(1);
        push_rule!(
            &mut context.analysis.results,
            "crap.maximum",
            &function.file,
            stable_function_symbol(function),
            json!(score),
            json!(context.config.crap.fail_over),
            "analysis-crap executable-line function coverage and cyclomatic complexity; C^2 * (1 - coverage)^3 + C",
            RuleComparison::Maximum,
            "crap-formula-v1",
        )?;
    }
    Ok(scored)
}

fn append_crap_omission(analysis: &mut QualityAnalysis, scored: usize, total: usize) {
    if scored == 0 {
        omit(
            analysis,
            "crap.maximum",
            "no function-level coverage was supplied; the existing CRAP section reports coverage as missing",
        );
    } else if scored < total {
        omit(
            analysis,
            "crap.maximum",
            "some selected functions lacked function-level coverage, so disappeared CRAP baseline rows cannot be classified as resolved",
        );
    }
}

fn append_kiss(context: &mut FunctionRuleContext<'_>) -> Result<(), String> {
    let reliable = append_function_kiss_rules(context)?;
    append_kiss_metric_omissions(context.analysis, reliable, context.functions.len());
    Ok(())
}

fn append_function_kiss_rules(context: &mut FunctionRuleContext<'_>) -> Result<usize, String> {
    let mut reliable = 0_usize;
    for function in context
        .functions
        .iter()
        .filter(|function| function.structural_metrics_reliable)
    {
        reliable = reliable.saturating_add(1);
        let symbol = stable_function_symbol(function);
        for (rule_id, measured, allowed, algorithm, evidence) in [
            (
                "kiss.cyclomatic-complexity",
                json!(function.complexity),
                json!(context.config.kiss.maximum_cyclomatic_complexity),
                "adapter cyclomatic complexity: one plus language decision points",
                "cyclomatic-complexity-v1",
            ),
            (
                "kiss.nesting-depth",
                json!(function.nesting_depth),
                json!(context.config.kiss.maximum_nesting_depth),
                "maximum recursive AST control-flow nesting depth, excluding nested function boundaries",
                "nesting-depth-v1",
            ),
            (
                "kiss.function-statements",
                json!(function.statement_count),
                json!(context.config.kiss.maximum_function_statements),
                "recursive count of language AST statement nodes, excluding nested function boundaries",
                "recursive-statements-v1",
            ),
            (
                "kiss.parameter-count",
                json!(function.parameter_count),
                json!(context.config.kiss.maximum_parameters),
                "count of declared function or method parameters, including an explicit receiver",
                "parameter-count-v1",
            ),
        ] {
            push_rule!(
                &mut context.analysis.results,
                rule_id,
                &function.file,
                symbol,
                measured,
                allowed,
                algorithm,
                RuleComparison::Maximum,
                evidence,
            )?;
        }
    }
    Ok(reliable)
}

fn append_kiss_metric_omissions(analysis: &mut QualityAnalysis, reliable: usize, total: usize) {
    if reliable == 0 || reliable < total {
        let reason = if reliable == 0 {
            "selected adapters did not provide reliable recursive function metrics"
        } else {
            "some selected functions lacked reliable recursive metrics, so disappeared KISS baseline rows cannot be classified as resolved"
        };
        for rule_id in [
            "kiss.cyclomatic-complexity",
            "kiss.nesting-depth",
            "kiss.function-statements",
            "kiss.parameter-count",
        ] {
            omit(analysis, rule_id, reason);
        }
    }
}

fn append_dependency_count_rules(context: &mut RepositoryRuleContext<'_>) -> Result<(), String> {
    if context.repository.dependency_graph_reliable {
        append_reliable_dependency_counts(context.analysis, context.repository, context.config)?;
    } else {
        omit(
            context.analysis,
            "kiss.module-dependency-count",
            "project adapter did not provide a reliable dependency graph",
        );
    }
    Ok(())
}

fn append_reliable_dependency_counts(
    analysis: &mut QualityAnalysis,
    repository: &RepositorySemantics,
    config: &RepoRigorConfig,
) -> Result<(), String> {
    let outgoing = production_dependencies(repository);
    for package in &repository.packages {
        let count = outgoing.get(&package.name).map_or(0, BTreeSet::len);
        push_rule!(
            &mut analysis.results,
            "kiss.module-dependency-count",
            &manifest_file(&package.root),
            &package.name,
            json!(count),
            json!(config.kiss.maximum_module_dependencies),
            "count of distinct direct non-target-gated production dependencies in adapter project metadata",
            RuleComparison::Maximum,
            "production-dependency-count-v1",
        )?;
    }
    Ok(())
}

fn append_mutation(
    analysis: &mut QualityAnalysis,
    mutations: &[MutationResult],
    config: &RepoRigorConfig,
) -> Result<(), String> {
    if let Some(score) = mutation_score(mutations) {
        append_scoreable_mutation_rules(analysis, mutations, config, score)?;
    } else {
        append_unexecuted_mutation_omissions(analysis);
    }

    if mutation_inventory_incomplete(mutations) {
        append_incomplete_mutation_omissions(analysis);
    }
    append_surviving_mutants(analysis, mutations)
}

fn append_scoreable_mutation_rules(
    analysis: &mut QualityAnalysis,
    mutations: &[MutationResult],
    config: &RepoRigorConfig,
    score: f64,
) -> Result<(), String> {
    push_rule!(
        &mut analysis.results,
        "mutation.score",
        "Cargo.toml",
        "repository mutation set",
        json!(score),
        json!(config.mutation.minimum_score),
        "killed / scoreable_mutants; scoreable statuses are exactly killed and survived; no equivalence inference is made",
        RuleComparison::Minimum,
        &mutation_score_evidence(mutations, config),
    )?;
    if mutations
        .iter()
        .all(|mutation| mutation.status != MutationStatus::Survived)
    {
        append_empty_survivor_rule(analysis)?;
    }
    Ok(())
}

fn append_empty_survivor_rule(analysis: &mut QualityAnalysis) -> Result<(), String> {
    append_empty_boolean_rule(analysis, EmptyBooleanRule::MutationSurvivors)
}

#[derive(Clone, Copy)]
enum EmptyBooleanRule {
    MutationSurvivors,
    LayerEdges,
    PackageCycles,
    ContractImplementations,
}

fn append_empty_boolean_rule(analysis: &mut QualityAnalysis, rule: EmptyBooleanRule) -> Result<(), String> {
    let (rule_id, symbol, value, algorithm, evidence) = empty_boolean_rule(rule);
    push_rule!(
        &mut analysis.results,
        rule_id,
        "Cargo.toml",
        symbol,
        json!(value),
        json!(value),
        algorithm,
        RuleComparison::Boolean,
        evidence,
    )?;
    Ok(())
}

fn empty_boolean_rule(
    rule: EmptyBooleanRule,
) -> (&'static str, &'static str, bool, &'static str, &'static str) {
    match rule {
        EmptyBooleanRule::MutationSurvivors => (
            "mutation.surviving-mutant",
            "repository mutation survivor inventory",
            false,
            "the scoreable mutation inventory contains no surviving mutant",
            "empty-survivor-inventory-v1",
        ),
        EmptyBooleanRule::LayerEdges => (
            "solid.dependency-direction",
            "repository layered dependency inventory",
            true,
            "no internal production edge connects two configured layer patterns",
            "empty-layer-edge-inventory-v1",
        ),
        EmptyBooleanRule::PackageCycles => (
            "solid.package-cycle",
            "repository package dependency graph",
            false,
            "Tarjan strongly connected components found no internal production dependency cycle",
            "acyclic-package-graph-v1",
        ),
        EmptyBooleanRule::ContractImplementations => (
            "solid.subtype-contract-test",
            "configured contract implementation inventory",
            true,
            "no non-target-gated implementation exists for a configured contract trait",
            "empty-contract-implementation-inventory-v1",
        ),
    }
}

fn append_unexecuted_mutation_omissions(analysis: &mut QualityAnalysis) {
    append_mutation_omissions(
        analysis,
        "no killed or surviving mutants were produced; pending, ignored, uncovered, timeout, compile-error, runtime-error, and invalid statuses are non-scoreable",
        "the mutation inventory was not executed to a killed-or-survived outcome, so survivor absence cannot be established",
    );
}

fn mutation_inventory_incomplete(mutations: &[MutationResult]) -> bool {
    mutations
        .iter()
        .any(|mutation| mutation_status_unresolved(mutation.status))
}

fn mutation_status_unresolved(status: MutationStatus) -> bool {
    matches!(
        status,
        MutationStatus::NoCoverage
            | MutationStatus::CompileError
            | MutationStatus::RuntimeError
            | MutationStatus::Timeout
            | MutationStatus::Invalid
            | MutationStatus::Pending
    )
}

fn append_incomplete_mutation_omissions(analysis: &mut QualityAnalysis) {
    append_mutation_omissions(
        analysis,
        "one or more mutants selected for execution had an unresolved non-scoreable status, so the scoreable set is incomplete and disappeared mutation-score baseline rows cannot be classified as resolved",
        "one or more mutants selected for execution had an unresolved non-scoreable status, so disappeared survivor baseline rows cannot be classified as resolved",
    );
}

fn append_mutation_omissions(analysis: &mut QualityAnalysis, score_reason: &str, survivor_reason: &str) {
    for (rule_id, reason) in [
        ("mutation.score", score_reason),
        ("mutation.surviving-mutant", survivor_reason),
    ] {
        omit(analysis, rule_id, reason);
    }
}

fn append_surviving_mutants(
    analysis: &mut QualityAnalysis,
    mutations: &[MutationResult],
) -> Result<(), String> {
    for mutation in mutations
        .iter()
        .filter(|mutation| mutation.status == MutationStatus::Survived)
    {
        append_surviving_mutant(analysis, &mutation.mutation)?;
    }
    Ok(())
}

fn append_surviving_mutant(
    analysis: &mut QualityAnalysis,
    candidate: &MutationCandidate,
) -> Result<(), String> {
    if candidate.fingerprint.is_empty() || candidate.stable_symbol.is_empty() {
        return Err(format!(
            "surviving mutant {} in {} lacks a stable symbol or fingerprint",
            candidate.id, candidate.file
        ));
    }
    analysis.surviving_mutants.push(SurvivingMutant {
        file: candidate.file.clone(),
        stable_symbol: candidate.stable_symbol.clone(),
        operator: candidate.operator.clone(),
        fingerprint: candidate.fingerprint.clone(),
    });
    push_rule!(
        &mut analysis.results,
        "mutation.surviving-mutant",
        &candidate.file,
        &candidate.stable_symbol,
        json!(true),
        json!(false),
        "the selected mutant completed with a successful test command; RepoRigor never classifies it as equivalent automatically",
        RuleComparison::Boolean,
        &candidate.fingerprint,
    )?;
    Ok(())
}

fn append_yagni(
    analysis: &mut QualityAnalysis,
    functions: &[FunctionRecord],
    repository: &RepositorySemantics,
    config: &RepoRigorConfig,
) -> Result<(), String> {
    let mut context = YagniContext {
        analysis,
        functions,
        repository,
        config,
    };
    append_identifier_yagni(&mut context)?;
    append_repository_yagni_inventories(context.analysis, context.repository, context.config)?;
    Ok(())
}

fn append_identifier_yagni(context: &mut YagniContext<'_>) -> Result<(), String> {
    if context.repository.identifier_counts_reliable {
        append_reliable_identifier_yagni(context)?;
    } else {
        append_identifier_yagni_omissions(context.analysis);
    }
    Ok(())
}

fn append_reliable_identifier_yagni(context: &mut YagniContext<'_>) -> Result<(), String> {
    let private = unused_functions(
        context.functions,
        context.repository,
        &context.config.yagni.entry_points,
        SymbolVisibility::Private,
    );
    append_bounded_items(
        &mut context.analysis.results,
        "yagni.unused-private-function",
        &private,
        context.config.yagni.maximum_unused_private_functions,
        "an unambiguous private function has no adapter-resolved production reference in its package and is not an explicit entry point",
    )?;
    let crate_exports = unused_functions(
        context.functions,
        context.repository,
        &context.config.yagni.entry_points,
        SymbolVisibility::Crate,
    );
    append_bounded_items(
        &mut context.analysis.results,
        "yagni.unreferenced-crate-export",
        &crate_exports,
        context.config.yagni.maximum_unreferenced_crate_exports,
        "an unambiguous repository-restricted export has no adapter-resolved repository reference; unrestricted public API is excluded",
    )?;
    append_bounded_items(
        &mut context.analysis.results,
        "yagni.unused-production-dependency",
        &unused_dependencies(context.repository),
        context.config.yagni.maximum_unused_production_dependencies,
        "a non-target-gated production dependency has no resolved production identifier reference or feature activation in its package",
    )
}

fn append_identifier_yagni_omissions(analysis: &mut QualityAnalysis) {
    for rule in [
        "yagni.unused-private-function",
        "yagni.unreferenced-crate-export",
        "yagni.unused-production-dependency",
    ] {
        omit(
            analysis,
            rule,
            "whole-project production references were not reliable for the selected adapters",
        );
    }
}

fn append_repository_yagni_inventories(
    analysis: &mut QualityAnalysis,
    repository: &RepositorySemantics,
    config: &RepoRigorConfig,
) -> Result<(), String> {
    let inventories = [
        OptionalInventory {
            reliable: repository.module_graph_reliable,
            rule_id: "yagni.unused-module",
            findings: unused_modules(repository, &config.yagni.entry_points),
            allowed_count: config.yagni.maximum_unused_modules,
            algorithm: "a production module has zero resolved module references and is not target-gated, generated, framework/reflection managed, externally invoked, or an explicit entry point",
            omission: "module reference inventory was not reliable for the selected adapters",
        },
        OptionalInventory {
            reliable: repository.unreachable_inventory_reliable,
            rule_id: "yagni.unreachable-code",
            findings: unreachable_items(repository),
            allowed_count: config.yagni.maximum_unreachable_statements,
            algorithm: "a recursive AST block contains a statement after an unconditional return, break, or continue in the same block",
            omission: "reachability inventory was not reliable for the selected adapters",
        },
        OptionalInventory {
            reliable: repository.feature_inventory_reliable,
            rule_id: "yagni.unused-feature-flag",
            findings: unused_features(repository),
            allowed_count: config.yagni.maximum_unused_feature_flags,
            algorithm: "a declared non-default feature has no active cfg reference, composition reference, or dependency activation",
            omission: "feature declarations and references were not reliable for the selected adapters",
        },
    ];
    for inventory in &inventories {
        append_optional_inventory(analysis, inventory)?;
    }
    Ok(())
}

fn append_optional_inventory(
    analysis: &mut QualityAnalysis,
    inventory: &OptionalInventory<'_>,
) -> Result<(), String> {
    if inventory.reliable {
        append_bounded_items(
            &mut analysis.results,
            inventory.rule_id,
            &inventory.findings,
            inventory.allowed_count,
            inventory.algorithm,
        )
    } else {
        omit(analysis, inventory.rule_id, inventory.omission);
        Ok(())
    }
}

fn unused_modules(repository: &RepositorySemantics, entry_points: &[String]) -> Vec<ItemFinding> {
    canonical_items(
        repository
            .modules
            .iter()
            .filter(|module| module_is_unused(module, entry_points))
            .map(|module| ItemFinding {
                file: module.file.clone(),
                symbol: module.stable_symbol.clone(),
                evidence: "unreferenced-production-module-v1".to_string(),
            })
            .collect(),
    )
}

fn module_is_unused(module: &reporigor_core::ModuleRecord, entry_points: &[String]) -> bool {
    module.references == 0 && module_is_reportable(module, entry_points)
}

fn module_is_reportable(module: &reporigor_core::ModuleRecord, entry_points: &[String]) -> bool {
    if module.visibility == SymbolVisibility::Public || module_is_managed(module) {
        return false;
    }
    !module_is_entry_point(module, entry_points)
}

fn module_is_managed(module: &reporigor_core::ModuleRecord) -> bool {
    [
        module.target_gated,
        module.generated,
        module.framework_managed,
        module.reflection_reachable,
        module.externally_invoked,
    ]
    .into_iter()
    .any(std::convert::identity)
}

fn module_is_entry_point(module: &reporigor_core::ModuleRecord, entry_points: &[String]) -> bool {
    entry_points
        .iter()
        .any(|entry| entry == &module.stable_symbol || module.file.ends_with(entry))
}

fn unreachable_items(repository: &RepositorySemantics) -> Vec<ItemFinding> {
    canonical_items(
        repository
            .unreachable
            .iter()
            .filter(|record| !record.target_gated)
            .map(|record| ItemFinding {
                file: record.file.clone(),
                symbol: record.stable_symbol.clone(),
                evidence: record.structural_evidence.clone(),
            })
            .collect(),
    )
}

fn unused_features(repository: &RepositorySemantics) -> Vec<ItemFinding> {
    canonical_items(
        repository
            .features
            .iter()
            .filter(|feature| feature_is_unused(repository, feature))
            .map(|feature| ItemFinding {
                file: package_manifest(repository, &feature.package),
                symbol: format!("{}::feature::{}", feature.package, feature.name),
                evidence: format!("unused-feature:{}", feature.name),
            })
            .collect(),
    )
}

fn feature_is_unused(repository: &RepositorySemantics, feature: &reporigor_core::FeatureRecord) -> bool {
    feature.name != "default"
        && feature.references == 0
        && !feature.target_gated
        && !feature_is_composed(repository, feature)
}

fn canonical_items(mut items: Vec<ItemFinding>) -> Vec<ItemFinding> {
    items.sort();
    items.dedup();
    items
}

fn append_architecture(context: &mut RepositoryRuleContext<'_>) -> Result<(), String> {
    append_dependency_architecture(context)?;
    append_contract_architecture(context)
}

fn append_dependency_architecture(context: &mut RepositoryRuleContext<'_>) -> Result<(), String> {
    if context.repository.dependency_graph_reliable {
        dependency_rules(context.analysis, context.repository, &context.config.architecture)?;
    } else {
        append_dependency_architecture_omissions(context.analysis);
    }
    Ok(())
}

fn append_dependency_architecture_omissions(analysis: &mut QualityAnalysis) {
    for rule in "solid.dependency-direction|solid.forbidden-module-edge|solid.package-cycle|solid.domain-to-infrastructure|solid.interface-to-implementation|solid.maximum-module-fan-out|coupling.afferent|coupling.efferent|coupling.instability".split('|') {
        omit(
            analysis,
            rule,
            "project adapter did not provide a reliable production dependency graph",
        );
    }
}

fn append_contract_architecture(context: &mut RepositoryRuleContext<'_>) -> Result<(), String> {
    if contract_inventory_reliable(context.repository) {
        contract_rules(context.analysis, context.repository, &context.config.architecture)?;
    } else if !context.config.architecture.contract_traits.is_empty() {
        omit(
            context.analysis,
            "solid.subtype-contract-test",
            "trait implementation or test reference inventory was not reliable",
        );
    }
    Ok(())
}

fn contract_inventory_reliable(repository: &RepositorySemantics) -> bool {
    repository.trait_inventory_reliable && repository.test_inventory_reliable
}

fn dependency_rules(
    analysis: &mut QualityAnalysis,
    repository: &RepositorySemantics,
    config: &ArchitectureConfig,
) -> Result<(), String> {
    let internal = internal_production_dependencies(repository);
    let direction_evaluated = append_internal_dependency_rules(analysis, repository, config, &internal)?;
    append_empty_dependency_rules(analysis, config, &internal, direction_evaluated)?;
    append_coupling_rules(analysis, repository, config)?;
    append_dependency_cycle_rules(analysis, repository)?;
    Ok(())
}

fn internal_production_dependencies(repository: &RepositorySemantics) -> Vec<&DependencyRecord> {
    production_dependency_records(repository)
        .filter(|edge| edge.internal)
        .collect()
}

fn append_internal_dependency_rules(
    analysis: &mut QualityAnalysis,
    repository: &RepositorySemantics,
    config: &ArchitectureConfig,
    internal: &[&DependencyRecord],
) -> Result<bool, String> {
    let mut direction_evaluated = false;
    for edge in internal {
        let mut context = DependencyEdgeContext {
            analysis,
            repository,
            config,
            edge,
        };
        direction_evaluated |= append_dependency_edge_rules(&mut context)?;
    }
    Ok(direction_evaluated)
}

struct DependencyEdgeContext<'a> {
    analysis: &'a mut QualityAnalysis,
    repository: &'a RepositorySemantics,
    config: &'a ArchitectureConfig,
    edge: &'a DependencyRecord,
}

fn append_dependency_edge_rules(context: &mut DependencyEdgeContext<'_>) -> Result<bool, String> {
    let direction_evaluated = append_direction_rule(context)?;
    append_separation_edge_rules(context)?;
    Ok(direction_evaluated)
}

fn append_separation_edge_rules(context: &mut DependencyEdgeContext<'_>) -> Result<(), String> {
    let checks = [
        (
            "solid.forbidden-module-edge",
            !edge_matches_forbidden(context.config, context.edge)?,
            "an internal production edge must not match any configured source->target wildcard predicate",
        ),
        (
            "solid.domain-to-infrastructure",
            !edge_connects(
                &context.config.domain_modules,
                &context.config.infrastructure_modules,
                context.edge,
            ),
            "a configured domain package must not directly depend on a configured infrastructure package",
        ),
        (
            "solid.interface-to-implementation",
            !edge_connects(
                &context.config.interface_modules,
                &context.config.implementation_modules,
                context.edge,
            ),
            "a configured interface package must not directly depend on a configured implementation package",
        ),
    ];
    for (rule_id, passed, algorithm) in checks {
        append_edge_boolean_rule(
            context.analysis,
            context.repository,
            context.edge,
            rule_id,
            passed,
            algorithm,
        )?;
    }
    Ok(())
}

fn append_direction_rule(context: &mut DependencyEdgeContext<'_>) -> Result<bool, String> {
    let layers = (
        configured_layer(&context.config.layers, &context.edge.package)?,
        configured_layer(&context.config.layers, &context.edge.dependency)?,
    );
    let (Some(source), Some(target)) = layers else {
        return Ok(false);
    };
    append_edge_boolean_rule(
        context.analysis,
        context.repository,
        context.edge,
        "solid.dependency-direction",
        source >= target,
        "an internal production edge must point from an equal-or-higher configured layer to an equal-or-lower layer",
    )?;
    Ok(true)
}

fn append_edge_boolean_rule(
    analysis: &mut QualityAnalysis,
    repository: &RepositorySemantics,
    edge: &DependencyRecord,
    rule_id: &str,
    passed: bool,
    algorithm: &str,
) -> Result<(), String> {
    let file = package_manifest(repository, &edge.package);
    let symbol = format!("{} -> {}", edge.package, edge.dependency);
    push_rule!(
        &mut analysis.results,
        rule_id,
        &file,
        &symbol,
        json!(passed),
        json!(true),
        algorithm,
        RuleComparison::Boolean,
        &format!("{}->{}", edge.package, edge.dependency),
    )?;
    Ok(())
}

fn edge_matches_forbidden(config: &ArchitectureConfig, edge: &DependencyRecord) -> Result<bool, String> {
    for pattern in &config.forbidden_edges {
        let (source, target) = parse_edge_pattern(pattern)?;
        if matches_pattern(source, &edge.package) && matches_pattern(target, &edge.dependency) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn edge_connects(sources: &[String], targets: &[String], edge: &DependencyRecord) -> bool {
    matches_any(sources, &edge.package) && matches_any(targets, &edge.dependency)
}

fn append_empty_dependency_rules(
    analysis: &mut QualityAnalysis,
    config: &ArchitectureConfig,
    internal: &[&DependencyRecord],
    direction_evaluated: bool,
) -> Result<(), String> {
    append_empty_internal_if_needed(analysis, internal)?;
    append_empty_direction_if_needed(analysis, config, direction_evaluated)
}

fn append_empty_internal_if_needed(
    analysis: &mut QualityAnalysis,
    internal: &[&DependencyRecord],
) -> Result<(), String> {
    if internal.is_empty() {
        append_empty_internal_edge_rules(analysis)?;
    }
    Ok(())
}

fn append_empty_direction_if_needed(
    analysis: &mut QualityAnalysis,
    config: &ArchitectureConfig,
    direction_evaluated: bool,
) -> Result<(), String> {
    if !config.layers.is_empty() && !direction_evaluated {
        append_empty_direction_rule(analysis)?;
    }
    Ok(())
}

fn append_empty_internal_edge_rules(analysis: &mut QualityAnalysis) -> Result<(), String> {
    for (rule_id, algorithm) in [
        (
            "solid.forbidden-module-edge",
            "no internal production edge matches a configured forbidden predicate",
        ),
        (
            "solid.domain-to-infrastructure",
            "no internal production edge violates configured domain-to-infrastructure separation",
        ),
        (
            "solid.interface-to-implementation",
            "no internal production edge violates configured interface-to-implementation separation",
        ),
    ] {
        push_rule!(
            &mut analysis.results,
            rule_id,
            "Cargo.toml",
            "repository internal dependency inventory",
            json!(true),
            json!(true),
            algorithm,
            RuleComparison::Boolean,
            "empty-internal-edge-inventory-v1",
        )?;
    }
    Ok(())
}

fn append_empty_direction_rule(analysis: &mut QualityAnalysis) -> Result<(), String> {
    append_empty_boolean_rule(analysis, EmptyBooleanRule::LayerEdges)
}

fn append_coupling_rules(
    analysis: &mut QualityAnalysis,
    repository: &RepositorySemantics,
    config: &ArchitectureConfig,
) -> Result<(), String> {
    let ca = afferent_coupling(&repository.packages, &repository.dependencies);
    let ce = efferent_coupling(&repository.packages, &repository.dependencies);
    for package in &repository.packages {
        let afferent = ca.get(&package.name).copied().unwrap_or_default();
        let efferent = ce.get(&package.name).copied().unwrap_or_default();
        append_package_coupling_rules(analysis, config, package, afferent, efferent)?;
    }
    Ok(())
}

fn append_package_coupling_rules(
    analysis: &mut QualityAnalysis,
    config: &ArchitectureConfig,
    package: &PackageRecord,
    afferent: usize,
    efferent: usize,
) -> Result<(), String> {
    let file = manifest_file(&package.root);
    push_rule!(
        &mut analysis.results,
        "solid.maximum-module-fan-out",
        &file,
        &package.name,
        json!(efferent),
        json!(config.maximum_module_fan_out),
        "count of distinct direct non-target-gated production dependencies",
        RuleComparison::Maximum,
        "package-production-fan-out-v1",
    )?;
    append_informational_coupling(analysis, package, &file, afferent, efferent)
}

fn append_informational_coupling(
    analysis: &mut QualityAnalysis,
    package: &PackageRecord,
    file: &str,
    afferent: usize,
    efferent: usize,
) -> Result<(), String> {
    for (rule, value, algorithm) in [
        (
            "coupling.afferent",
            json!(afferent),
            "number of repository packages with a direct internal production edge to this package",
        ),
        (
            "coupling.efferent",
            json!(efferent),
            "number of distinct direct production dependencies, internal and external",
        ),
        (
            "coupling.instability",
            json!(instability(afferent, efferent)),
            "Ce / (Ca + Ce), or zero when both are zero",
        ),
    ] {
        push_rule!(
            &mut analysis.results,
            rule,
            file,
            &package.name,
            value,
            Value::Null,
            algorithm,
            RuleComparison::Informational,
            rule,
        )?;
    }
    Ok(())
}

fn append_dependency_cycle_rules(
    analysis: &mut QualityAnalysis,
    repository: &RepositorySemantics,
) -> Result<(), String> {
    let cycles = dependency_cycles(&repository.packages, &repository.dependencies);
    if cycles.is_empty() {
        append_acyclic_rule(analysis)?;
    }
    for cycle in cycles {
        append_cycle_rule(analysis, &cycle)?;
    }
    Ok(())
}

fn append_acyclic_rule(analysis: &mut QualityAnalysis) -> Result<(), String> {
    append_empty_boolean_rule(analysis, EmptyBooleanRule::PackageCycles)
}

fn append_cycle_rule(analysis: &mut QualityAnalysis, cycle: &[String]) -> Result<(), String> {
    let symbol = cycle.join(" -> ");
    push_rule!(
        &mut analysis.results,
        "solid.package-cycle",
        "Cargo.toml",
        &symbol,
        json!(true),
        json!(false),
        "Tarjan strongly connected components over internal production dependency edges; singleton self-edges are cycles",
        RuleComparison::Boolean,
        &cycle.join("|"),
    )?;
    Ok(())
}

fn configured_layer(layers: &BTreeMap<String, u32>, package: &str) -> Result<Option<u32>, String> {
    let matches = layers
        .iter()
        .filter(|(pattern, _)| matches_pattern(pattern, package))
        .map(|(pattern, layer)| (pattern.as_str(), *layer))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [(_, layer)] => Ok(Some(*layer)),
        _ => Err(format!(
            "package {package:?} matches multiple architecture.layers patterns: {}",
            matches
                .iter()
                .map(|(pattern, _)| *pattern)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn contract_rules(
    analysis: &mut QualityAnalysis,
    repository: &RepositorySemantics,
    config: &ArchitectureConfig,
) -> Result<(), String> {
    let implementations = configured_contract_implementations(repository, config);
    append_empty_contract_rule(analysis, config, &implementations)?;
    append_contract_implementation_rules(analysis, repository, config, &implementations)
}

fn configured_contract_implementations<'a>(
    repository: &'a RepositorySemantics,
    config: &ArchitectureConfig,
) -> Vec<&'a TraitImplementationRecord> {
    repository
        .trait_implementations
        .iter()
        .filter(|implementation| {
            !implementation.target_gated
                && config
                    .contract_traits
                    .iter()
                    .any(|configured| configured == &implementation.trait_symbol)
        })
        .collect()
}

fn append_empty_contract_rule(
    analysis: &mut QualityAnalysis,
    config: &ArchitectureConfig,
    implementations: &[&TraitImplementationRecord],
) -> Result<(), String> {
    if !config.contract_traits.is_empty() && implementations.is_empty() {
        append_empty_boolean_rule(analysis, EmptyBooleanRule::ContractImplementations)?;
    }
    Ok(())
}

fn append_contract_implementation_rules(
    analysis: &mut QualityAnalysis,
    repository: &RepositorySemantics,
    config: &ArchitectureConfig,
    implementations: &[&TraitImplementationRecord],
) -> Result<(), String> {
    for implementation in implementations {
        append_contract_implementation_rule(analysis, repository, config, implementation)?;
    }
    Ok(())
}

fn append_contract_implementation_rule(
    analysis: &mut QualityAnalysis,
    repository: &RepositorySemantics,
    config: &ArchitectureConfig,
    implementation: &TraitImplementationRecord,
) -> Result<(), String> {
    let present = repository
        .tests
        .iter()
        .any(|test| test_covers_contract(test, config, implementation));
    push_rule!(
        &mut analysis.results,
        "solid.subtype-contract-test",
        &implementation.file,
        &format!(
            "{} for {}",
            implementation.implementation_symbol, implementation.trait_symbol
        ),
        json!(present),
        json!(true),
        "each configured trait implementation requires a non-target-gated test bearing the configured contract marker and referencing both the exact trait and implementation symbols",
        RuleComparison::Boolean,
        &format!(
            "{}|{}|{}",
            implementation.trait_symbol,
            implementation.implementation_symbol,
            config.contract_test_marker
        ),
    )?;
    Ok(())
}

fn test_covers_contract(
    test: &reporigor_core::TestRecord,
    config: &ArchitectureConfig,
    implementation: &TraitImplementationRecord,
) -> bool {
    !test.target_gated
        && test.markers.contains(&config.contract_test_marker)
        && test.referenced_symbols.contains(&implementation.trait_symbol)
        && test
            .referenced_symbols
            .contains(&implementation.implementation_symbol)
}

fn append_cohesion(context: &mut FunctionRuleContext<'_>) -> Result<(), String> {
    let reliable = reliable_function_count(context.functions);
    let modules = cohesion_modules(context.functions);
    if modules.is_empty() {
        append_missing_cohesion_omission(context.analysis);
        return Ok(());
    }
    append_partial_cohesion_omission(context.analysis, reliable, context.functions.len());
    append_module_cohesion_rules(context.analysis, modules, context.config)
}

fn reliable_function_count(functions: &[FunctionRecord]) -> usize {
    functions
        .iter()
        .filter(|function| function.structural_metrics_reliable)
        .count()
}

fn cohesion_modules(functions: &[FunctionRecord]) -> BTreeMap<(String, String), Vec<&FunctionRecord>> {
    let mut modules = BTreeMap::<(String, String), Vec<&FunctionRecord>>::new();
    for function in functions
        .iter()
        .filter(|function| function.structural_metrics_reliable)
    {
        modules
            .entry((function.file.clone(), cohesion_owner(function)))
            .or_default()
            .push(function);
    }
    modules
}

fn append_missing_cohesion_omission(analysis: &mut QualityAnalysis) {
    omit(
        analysis,
        "cohesion.module",
        "selected adapters did not provide reliable function reference sets",
    );
}

fn append_partial_cohesion_omission(analysis: &mut QualityAnalysis, reliable: usize, total: usize) {
    if reliable < total {
        omit(
            analysis,
            "cohesion.module",
            "some selected functions lacked reliable reference sets, so disappeared cohesion baseline rows cannot be classified as resolved",
        );
    }
}

fn append_module_cohesion_rules(
    analysis: &mut QualityAnalysis,
    modules: BTreeMap<(String, String), Vec<&FunctionRecord>>,
    config: &RepoRigorConfig,
) -> Result<(), String> {
    for ((file, owner), mut functions) in modules {
        functions.sort_by(|left, right| stable_function_symbol(left).cmp(stable_function_symbol(right)));
        let cohesion = module_cohesion(&functions);
        push_rule!(
            &mut analysis.results,
            "cohesion.module",
            &file,
            &owner,
            json!(cohesion),
            json!(config.cohesion.minimum),
            "related function pairs / all function pairs within the adapter-qualified module/type owner; the same exact trait-implementation contract, uniquely resolved direct function references, and shared non-local references form relations; singleton modules equal one",
            RuleComparison::Minimum,
            "qualified-owner-function-reference-graph-v1",
        )?;
    }
    Ok(())
}

fn module_cohesion(functions: &[&FunctionRecord]) -> f64 {
    if functions.len() < 2 {
        return 1.0;
    }
    let function_leaves = functions
        .iter()
        .map(|function| simple_name(function))
        .collect::<BTreeSet<_>>();
    let direct_targets = functions
        .iter()
        .map(|function| resolved_function_targets(function, functions))
        .collect::<Vec<_>>();
    let shared_trait_contract = functions
        .first()
        .is_some_and(|function| cohesion_owner(function).contains(" as "));
    let context = CohesionPairContext {
        functions,
        function_leaves: &function_leaves,
        direct_targets: &direct_targets,
        shared_trait_contract,
    };
    let related = related_function_pairs(&context);
    count_as_f64(related) / count_as_f64(pair_count(functions.len()))
}

struct CohesionPairContext<'a, 'b> {
    functions: &'a [&'b FunctionRecord],
    function_leaves: &'a BTreeSet<&'b str>,
    direct_targets: &'a [BTreeSet<usize>],
    shared_trait_contract: bool,
}

fn related_function_pairs(context: &CohesionPairContext<'_, '_>) -> usize {
    let mut related = 0_usize;
    for left in 0..context.functions.len() {
        for right in left + 1..context.functions.len() {
            related = related.saturating_add(usize::from(function_pair_is_related(context, left, right)));
        }
    }
    related
}

fn function_pair_is_related(context: &CohesionPairContext<'_, '_>, left: usize, right: usize) -> bool {
    [
        context.shared_trait_contract,
        functions_are_directly_related(context.direct_targets, left, right),
        functions_share_target(context.direct_targets, left, right),
        functions_share_nonlocal_reference(context.functions, context.function_leaves, left, right),
    ]
    .into_iter()
    .any(std::convert::identity)
}

fn functions_are_directly_related(direct_targets: &[BTreeSet<usize>], left: usize, right: usize) -> bool {
    direct_targets[left].contains(&right) || direct_targets[right].contains(&left)
}

fn functions_share_target(direct_targets: &[BTreeSet<usize>], left: usize, right: usize) -> bool {
    direct_targets[left]
        .intersection(&direct_targets[right])
        .next()
        .is_some()
}

fn functions_share_nonlocal_reference(
    functions: &[&FunctionRecord],
    function_leaves: &BTreeSet<&str>,
    left: usize,
    right: usize,
) -> bool {
    functions[left]
        .references
        .intersection(&functions[right].references)
        .any(|reference| reference_is_nonlocal(reference, function_leaves))
}

fn reference_is_nonlocal(reference: &str, function_leaves: &BTreeSet<&str>) -> bool {
    if reference.starts_with("field::") {
        return true;
    }
    !ubiquitous_reference(reference) && !function_leaves.contains(reference_leaf(reference))
}

fn pair_count(function_count: usize) -> usize {
    function_count.saturating_mul(function_count.saturating_sub(1)) / 2
}

fn cohesion_owner(function: &FunctionRecord) -> String {
    let symbol = without_terminal_signature(without_duplicate_suffix(stable_function_symbol(function)));
    let separator = match (symbol.rfind("::"), symbol.rfind('.')) {
        (Some(colons), Some(dot)) if colons > dot => Some((colons, 2)),
        (Some(_) | None, Some(dot)) => Some((dot, 1)),
        (Some(colons), None) => Some((colons, 2)),
        (None, None) => None,
    };
    separator.map_or_else(
        || function.file.clone(),
        |(offset, _)| symbol[..offset].to_string(),
    )
}

fn resolved_function_targets(function: &FunctionRecord, functions: &[&FunctionRecord]) -> BTreeSet<usize> {
    let mut targets = BTreeSet::new();
    for reference in &function.references {
        if reference.starts_with("field::") {
            continue;
        }
        let reference = reference.strip_prefix("method::").unwrap_or(reference);
        let reference = reference_segments(reference);
        if reference.is_empty() {
            continue;
        }
        let matches = functions
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                reference_path_matches(&reference, &reference_segments(stable_function_symbol(candidate)))
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if matches.len() == 1 {
            targets.insert(matches[0]);
        }
    }
    targets
}

fn reference_segments(value: &str) -> Vec<&str> {
    without_terminal_signature(without_duplicate_suffix(value))
        .trim_end_matches('!')
        .split([':', '.'])
        .map(|part| part.trim_start_matches("r#"))
        .filter(|part| !part.is_empty() && !matches!(*part, "crate" | "self" | "super"))
        .collect()
}

fn reference_path_matches(reference: &[&str], candidate: &[&str]) -> bool {
    !reference.is_empty() && candidate.ends_with(reference)
}

fn ubiquitous_reference(reference: &str) -> bool {
    let reference = reference.strip_prefix("method::").unwrap_or(reference);
    "self|Self|Result|Option|Some|None|Ok|Err|new|default"
        .split('|')
        .any(|ubiquitous| ubiquitous == reference)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ItemFinding {
    file: String,
    symbol: String,
    evidence: String,
}

struct BoundedInventory<'a> {
    rule_id: &'a str,
    findings: &'a [ItemFinding],
    allowed_count: usize,
    algorithm: &'a str,
}

struct OptionalInventory<'a> {
    reliable: bool,
    rule_id: &'a str,
    findings: Vec<ItemFinding>,
    allowed_count: usize,
    algorithm: &'a str,
    omission: &'a str,
}

fn unused_functions(
    functions: &[FunctionRecord],
    repository: &RepositorySemantics,
    entry_points: &[String],
    visibility: SymbolVisibility,
) -> Vec<ItemFinding> {
    let declaration_counts = function_declaration_counts(functions);
    let mut findings = Vec::new();
    for function in functions
        .iter()
        .filter(|function| function_is_yagni_candidate(function, visibility))
    {
        if let Some(finding) =
            unused_function_finding(function, functions, repository, entry_points, &declaration_counts)
        {
            findings.push(finding);
        }
    }
    canonical_items(findings)
}

fn function_declaration_counts(functions: &[FunctionRecord]) -> BTreeMap<(Option<&str>, &str), usize> {
    let mut declaration_counts = BTreeMap::<(Option<&str>, &str), usize>::new();
    for function in functions
        .iter()
        .filter(|function| function.structural_metrics_reliable)
    {
        *declaration_counts
            .entry((function.package.as_deref(), simple_name(function)))
            .or_default() += 1;
    }
    declaration_counts
}

fn function_is_yagni_candidate(function: &FunctionRecord, visibility: SymbolVisibility) -> bool {
    function.structural_metrics_reliable
        && function.production
        && function.visibility == visibility
        && !function.entry_point
}

fn unused_function_finding(
    function: &FunctionRecord,
    functions: &[FunctionRecord],
    repository: &RepositorySemantics,
    entry_points: &[String],
    declaration_counts: &BTreeMap<(Option<&str>, &str), usize>,
) -> Option<ItemFinding> {
    if function_is_explicit_entry(function, entry_points) {
        return None;
    }
    let (package, name, declarations) = unique_function_identity(function, declaration_counts)?;
    let identifier_references = identifier_reference_count(repository, package, name)?;
    if function_is_unreferenced(identifier_references, declarations, function, functions, name) {
        return Some(ItemFinding {
            file: function.file.clone(),
            symbol: stable_function_symbol(function).to_string(),
            evidence: format!("unreferenced-function:{name}"),
        });
    }
    None
}

fn function_is_explicit_entry(function: &FunctionRecord, entry_points: &[String]) -> bool {
    entry_points
        .iter()
        .any(|entry| entry == stable_function_symbol(function) || function.file.ends_with(entry))
}

fn unique_function_identity<'a>(
    function: &'a FunctionRecord,
    declaration_counts: &BTreeMap<(Option<&str>, &str), usize>,
) -> Option<(&'a str, &'a str, usize)> {
    let package = function.package.as_deref()?;
    let name = simple_name(function);
    let declarations = declaration_counts.get(&(Some(package), name)).copied()?;
    if declarations == 1 {
        Some((package, name, declarations))
    } else {
        None
    }
}

fn identifier_reference_count(repository: &RepositorySemantics, package: &str, name: &str) -> Option<usize> {
    repository
        .identifiers
        .iter()
        .find(|record| record.package.as_deref() == Some(package) && record.identifier == name)
        .map(|record| usize::try_from(record.production_references).unwrap_or(usize::MAX))
}

fn function_is_unreferenced(
    identifier_references: usize,
    declarations: usize,
    function: &FunctionRecord,
    functions: &[FunctionRecord],
    name: &str,
) -> bool {
    identifier_references <= declarations && !function_has_repository_reference(function, functions, name)
}

fn function_has_repository_reference(
    function: &FunctionRecord,
    functions: &[FunctionRecord],
    name: &str,
) -> bool {
    functions.iter().any(|other| {
        other_can_reference_function(other, function)
            && other
                .references
                .iter()
                .any(|reference| reference_leaf(reference) == name)
    })
}

fn other_can_reference_function(other: &FunctionRecord, function: &FunctionRecord) -> bool {
    [
        other.structural_metrics_reliable,
        other.production,
        other.package == function.package,
        other.stable_symbol != function.stable_symbol,
    ]
    .into_iter()
    .all(std::convert::identity)
}

fn unused_dependencies(repository: &RepositorySemantics) -> Vec<ItemFinding> {
    let mut findings = Vec::new();
    for edge in production_dependency_records(repository) {
        let source_identifier = if edge.source_identifier.is_empty() {
            &edge.dependency
        } else {
            &edge.source_identifier
        };
        let identifier = source_identifier.replace('-', "_");
        let used_by_source = repository.identifiers.iter().any(|record| {
            record.package.as_deref() == Some(edge.package.as_str())
                && record.identifier == identifier
                && record.production_references > 0
        });
        let used_by_feature = repository.features.iter().any(|feature| {
            feature.package == edge.package
                && feature.enables.iter().any(|enabled| {
                    enabled == source_identifier
                        || enabled == &format!("dep:{source_identifier}")
                        || enabled.starts_with(&format!("{source_identifier}/"))
                        || enabled.starts_with(&format!("{source_identifier}?/"))
                })
        });
        if !used_by_source && !used_by_feature {
            findings.push(ItemFinding {
                file: package_manifest(repository, &edge.package),
                symbol: format!("{}::dependency::{}", edge.package, edge.dependency),
                evidence: format!("{}->{}", edge.package, edge.dependency),
            });
        }
    }
    findings.sort();
    findings.dedup();
    findings
}

fn append_bounded_items(
    output: &mut Vec<RuleResult>,
    rule_id: &str,
    findings: &[ItemFinding],
    allowed_count: usize,
    algorithm: &str,
) -> Result<(), String> {
    let inventory = BoundedInventory {
        rule_id,
        findings,
        allowed_count,
        algorithm,
    };
    if inventory.findings.is_empty() {
        return append_empty_bounded_inventory(output, &inventory);
    }
    append_bounded_findings(output, &inventory)
}

fn append_empty_bounded_inventory(
    output: &mut Vec<RuleResult>,
    inventory: &BoundedInventory<'_>,
) -> Result<(), String> {
    push_rule!(
        output,
        inventory.rule_id,
        "Cargo.toml",
        &format!("repository inventory::{}", inventory.rule_id),
        json!(0),
        json!(inventory.allowed_count),
        inventory.algorithm,
        RuleComparison::Maximum,
        "empty-reliable-inventory-v1",
    )?;
    Ok(())
}

fn append_bounded_findings(
    output: &mut Vec<RuleResult>,
    inventory: &BoundedInventory<'_>,
) -> Result<(), String> {
    for finding in inventory.findings {
        push_rule!(
            output,
            inventory.rule_id,
            &finding.file,
            &finding.symbol,
            json!(inventory.findings.len()),
            json!(inventory.allowed_count),
            inventory.algorithm,
            RuleComparison::Maximum,
            &finding.evidence,
        )?;
    }
    Ok(())
}

fn production_dependencies(repository: &RepositorySemantics) -> BTreeMap<String, BTreeSet<String>> {
    let mut result = BTreeMap::new();
    for edge in production_dependency_records(repository) {
        result
            .entry(edge.package.clone())
            .or_insert_with(BTreeSet::new)
            .insert(edge.dependency.clone());
    }
    result
}

fn production_dependency_records(
    repository: &RepositorySemantics,
) -> impl Iterator<Item = &DependencyRecord> {
    repository
        .dependencies
        .iter()
        .filter(|edge| edge.scope == DependencyScope::Production && !edge.target_gated)
}

fn package_manifest(repository: &RepositorySemantics, package: &str) -> String {
    repository
        .packages
        .iter()
        .find(|record| record.name == package)
        .map_or_else(|| "Cargo.toml".to_string(), |record| manifest_file(&record.root))
}

fn manifest_file(root: &str) -> String {
    match root.trim_end_matches('/') {
        "" | "." => "Cargo.toml".to_string(),
        root if root.ends_with("Cargo.toml") => root.to_string(),
        root => format!("{root}/Cargo.toml"),
    }
}

fn stable_function_symbol(function: &FunctionRecord) -> &str {
    if function.stable_symbol.is_empty() {
        &function.name
    } else {
        &function.stable_symbol
    }
}

fn simple_name(function: &FunctionRecord) -> &str {
    let symbol = without_terminal_signature(without_duplicate_suffix(stable_function_symbol(function)));
    symbol
        .rsplit("::")
        .next()
        .unwrap_or(symbol)
        .rsplit('.')
        .next()
        .unwrap_or(symbol)
        .trim_start_matches("r#")
}

fn without_duplicate_suffix(symbol: &str) -> &str {
    let Some((prefix, suffix)) = symbol.rsplit_once('#') else {
        return symbol;
    };
    if duplicate_suffix_is_valid(prefix, suffix) {
        prefix
    } else {
        symbol
    }
}

fn duplicate_suffix_is_valid(prefix: &str, suffix: &str) -> bool {
    let digest = suffix.split(':').next().unwrap_or(suffix);
    let occurrence = suffix.strip_prefix(digest).unwrap_or_default();
    prefix.ends_with(')')
        && digest.len() == 12
        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        && duplicate_occurrence_is_valid(occurrence)
}

fn duplicate_occurrence_is_valid(occurrence: &str) -> bool {
    if occurrence.is_empty() {
        return true;
    }
    occurrence
        .strip_prefix(':')
        .is_some_and(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
}

fn without_terminal_signature(symbol: &str) -> &str {
    if !symbol.ends_with(')') {
        return symbol;
    }
    terminal_signature_start(symbol).map_or(symbol, |offset| &symbol[..offset])
}

fn terminal_signature_start(symbol: &str) -> Option<usize> {
    let mut depth = 0_usize;
    for (offset, character) in symbol.char_indices().rev() {
        match character {
            ')' => depth = depth.saturating_add(1),
            '(' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn reference_leaf(reference: &str) -> &str {
    reference
        .trim_end_matches('!')
        .rsplit("::")
        .next()
        .unwrap_or(reference)
        .rsplit('.')
        .next()
        .unwrap_or(reference)
        .trim_start_matches("r#")
}

fn feature_is_composed(repository: &RepositorySemantics, target: &reporigor_core::FeatureRecord) -> bool {
    !target.enables.is_empty()
        || repository.features.iter().any(|feature| {
            feature.package == target.package
                && feature.name != target.name
                && feature.enables.contains(&target.name)
        })
}

fn mutation_score_evidence(mutations: &[MutationResult], config: &RepoRigorConfig) -> String {
    let mut operators = config
        .mutation
        .operators
        .iter()
        .map(|operator| operator.as_str())
        .collect::<Vec<_>>();
    operators.sort_unstable();
    let mut fingerprints = mutations
        .iter()
        .filter(|mutation| matches!(mutation.status, MutationStatus::Killed | MutationStatus::Survived))
        .map(|mutation| mutation.mutation.fingerprint.as_str())
        .collect::<Vec<_>>();
    fingerprints.sort_unstable();
    format!(
        "mutation-score-v1;seed={};operators={};scoreable={}",
        config.mutation.seed,
        operators.join(","),
        fingerprints.join(",")
    )
}

fn matches_any(patterns: &[String], value: &str) -> bool {
    patterns.iter().any(|pattern| matches_pattern(pattern, value))
}

fn omit(analysis: &mut QualityAnalysis, rule_id: &str, reason: &str) {
    analysis.omitted.push(OmittedCheck {
        rule_id: rule_id.to_string(),
        reason: reason.to_string(),
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use analysis_dry::Location;
    use reporigor_core::{
        BaselineDisposition, DependencyRecord, FeatureRecord, IdentifierCountRecord, Language, ModuleRecord,
        MutationCandidate, MutationResult, MutationStatus, PackageRecord, RepositorySemantics, RuleOutcome,
        TestRecord, TraitImplementationRecord, UnreachableRecord,
    };

    use super::*;

    fn function(symbol: &str, references: &[&str]) -> FunctionRecord {
        let name = symbol.rsplit("::").next().unwrap_or(symbol);
        let mut function = FunctionRecord::new(Language::Rust, name, "src/lib.rs", 1, 3, 2);
        function.stable_symbol = symbol.to_string();
        function.nesting_depth = 1;
        function.statement_count = 3;
        function.parameter_count = 1;
        function.normalized_tokens = vec!["fn".to_string(), "LOCAL".to_string()];
        function.references = references.iter().map(|value| (*value).to_string()).collect();
        function.visibility = SymbolVisibility::Private;
        function.structural_metrics_reliable = true;
        function.package = Some("fixture".to_string());
        function.coverage = Some(100.0);
        function.crap = Some(2.0);
        function
    }

    fn mutant(id: u64, status: MutationStatus) -> MutationResult {
        MutationResult {
            mutation: MutationCandidate {
                id,
                language: Language::Rust,
                file: "src/lib.rs".to_string(),
                stable_symbol: format!("fixture::function_{id}"),
                operator: "comparison".to_string(),
                fingerprint: format!("{id:064x}"),
                line: 1,
                column: 1,
                original: "==".to_string(),
                replacement: "!=".to_string(),
                start_byte: usize::try_from(id).unwrap_or(0),
                end_byte: usize::try_from(id).unwrap_or(0).saturating_add(2),
            },
            status,
            exit_code: None,
            duration_seconds: 0.0,
            detail: None,
        }
    }

    fn mutation_fixture(statuses: &[MutationStatus]) -> QualityAnalysis {
        let mutations = statuses
            .iter()
            .copied()
            .enumerate()
            .map(|(index, status)| mutant(u64::try_from(index).unwrap_or(0).saturating_add(1), status))
            .collect::<Vec<_>>();
        fixture_analysis(
            &RepoRigorConfig::default(),
            &[],
            &mutations,
            &RepositorySemantics::default(),
        )
    }

    fn fixture_analysis(
        config: &RepoRigorConfig,
        functions: &[FunctionRecord],
        mutations: &[MutationResult],
        repository: &RepositorySemantics,
    ) -> QualityAnalysis {
        analyze_rules(QualityInput {
            config,
            functions,
            duplicates: &[],
            mutations,
            repository,
        })
        .unwrap_or_else(|error| panic!("rules: {error}"))
    }

    fn repository_analysis(config: &RepoRigorConfig, repository: &RepositorySemantics) -> QualityAnalysis {
        fixture_analysis(config, &[], &[], repository)
    }

    fn fixture_analysis_with_duplicates(
        config: &RepoRigorConfig,
        duplicates: &[Duplicate],
    ) -> QualityAnalysis {
        analyze_rules(QualityInput {
            config,
            functions: &[],
            duplicates,
            mutations: &[],
            repository: &RepositorySemantics::default(),
        })
        .unwrap_or_else(|error| panic!("rules: {error}"))
    }

    fn rule<'a>(analysis: &'a QualityAnalysis, rule_id: &str) -> &'a RuleResult {
        analysis
            .results
            .iter()
            .find(|result| result.rule_id == rule_id)
            .unwrap_or_else(|| panic!("missing {rule_id} rule"))
    }

    fn rule_is_omitted(analysis: &QualityAnalysis, rule_id: &str) -> bool {
        analysis
            .omitted
            .iter()
            .any(|omission| omission.rule_id == rule_id)
    }

    fn assert_mutation_score(analysis: &QualityAnalysis, measured: f64, outcome: RuleOutcome) {
        let score = rule(analysis, "mutation.score");
        assert_eq!(score.measured, json!(measured));
        assert_eq!(score.result, outcome);
    }

    fn duplicate_location(
        stable_symbol: Option<&str>,
        lines: std::ops::RangeInclusive<u32>,
        tokens: std::ops::Range<usize>,
    ) -> Location {
        Location {
            file: "src/lib.rs".to_string(),
            start_line: *lines.start(),
            end_line: *lines.end(),
            stable_symbol: stable_symbol.map(str::to_string),
            start_token: tokens.start,
            end_token: tokens.end,
        }
    }

    fn trait_implementation(
        implementation_symbol: &str,
        file: &str,
        target_gated: bool,
    ) -> TraitImplementationRecord {
        TraitImplementationRecord {
            trait_symbol: "Contract".to_string(),
            implementation_symbol: implementation_symbol.to_string(),
            file: file.to_string(),
            package: Some("fixture".to_string()),
            target_gated,
        }
    }

    fn unreachable_record(
        file: &str,
        stable_symbol: &str,
        structural_evidence: &str,
        target_gated: bool,
    ) -> UnreachableRecord {
        UnreachableRecord {
            file: file.to_string(),
            stable_symbol: stable_symbol.to_string(),
            structural_evidence: structural_evidence.to_string(),
            package: Some("domain".to_string()),
            target_gated,
        }
    }

    fn package(name: &str, root: &str) -> PackageRecord {
        PackageRecord {
            name: name.to_string(),
            root: root.to_string(),
        }
    }

    fn dependency(package: &str, dependency: &str) -> DependencyRecord {
        DependencyRecord {
            package: package.to_string(),
            dependency: dependency.to_string(),
            source_identifier: dependency.to_string(),
            scope: DependencyScope::Production,
            internal: true,
            optional: false,
            target_gated: false,
        }
    }

    fn feature(name: &str, enables: &[&str]) -> FeatureRecord {
        FeatureRecord {
            package: "domain".to_string(),
            name: name.to_string(),
            references: 0,
            enables: enables.iter().map(|value| (*value).to_string()).collect(),
            target_gated: false,
        }
    }

    #[derive(Clone, Copy)]
    enum ModuleFixture {
        Generated,
        Public,
        TargetOnly,
        Unused,
    }

    fn module(fixture: ModuleFixture) -> ModuleRecord {
        let (stable_symbol, file, visibility, target_gated, generated) = match fixture {
            ModuleFixture::Generated => (
                "domain::generated",
                "crates/domain/src/generated.rs",
                SymbolVisibility::Private,
                false,
                true,
            ),
            ModuleFixture::Public => (
                "domain::public_module",
                "crates/domain/src/public.rs",
                SymbolVisibility::Public,
                false,
                false,
            ),
            ModuleFixture::TargetOnly => (
                "domain::target_only",
                "crates/domain/src/target.rs",
                SymbolVisibility::Private,
                true,
                false,
            ),
            ModuleFixture::Unused => (
                "domain::unused_module",
                "crates/domain/src/unused.rs",
                SymbolVisibility::Private,
                false,
                false,
            ),
        };
        ModuleRecord {
            stable_symbol: stable_symbol.to_string(),
            file: file.to_string(),
            package: Some("domain".to_string()),
            visibility,
            references: 0,
            target_gated,
            generated,
            ..ModuleRecord::default()
        }
    }

    #[test]
    fn module_fixture_marks_only_target_specific_modules_as_target_gated() {
        assert!(!module(ModuleFixture::Unused).target_gated);
        assert!(module(ModuleFixture::TargetOnly).target_gated);
    }

    #[test]
    fn mutation_denominator_is_only_killed_plus_survived() {
        let analysis = mutation_fixture(&[
            MutationStatus::Killed,
            MutationStatus::Survived,
            MutationStatus::NoCoverage,
            MutationStatus::CompileError,
            MutationStatus::Ignored,
        ]);
        assert_mutation_score(&analysis, 0.5, RuleOutcome::Fail);
        assert_eq!(analysis.surviving_mutants.len(), 1);
        assert_eq!(analysis.surviving_mutants[0].fingerprint, format!("{:064x}", 2));
        assert!(rule_is_omitted(&analysis, "mutation.score"));
    }

    #[test]
    fn deterministic_execution_limit_does_not_make_the_score_incomplete() {
        let analysis = mutation_fixture(&[
            MutationStatus::Killed,
            MutationStatus::Ignored,
            MutationStatus::Ignored,
        ]);
        assert_mutation_score(&analysis, 1.0, RuleOutcome::Pass);
        assert!(analysis.results.iter().any(
            |result| result.rule_id == "mutation.surviving-mutant" && result.result == RuleOutcome::Pass
        ));
        assert!(!analysis
            .omitted
            .iter()
            .any(|omission| omission.rule_id.starts_with("mutation.")));
    }

    #[test]
    fn bounded_inventory_threshold_is_inclusive_and_keeps_item_identity() {
        let findings = vec![ItemFinding {
            file: "src/lib.rs".to_string(),
            symbol: "fixture::unused".to_string(),
            evidence: "unused-fixture".to_string(),
        }];
        let mut identities = Vec::new();
        for (allowed_count, expected) in [(1, RuleOutcome::Pass), (0, RuleOutcome::Fail)] {
            let mut results = Vec::new();
            append_bounded_items(
                &mut results,
                "yagni.unused-private-function",
                &findings,
                allowed_count,
                "fixture inventory",
            )
            .unwrap_or_else(|error| panic!("fixture inventory: {error}"));
            assert_eq!(results[0].result, expected);
            identities.push(results[0].violation_id.clone());
        }
        assert_eq!(identities[0], identities[1]);
    }

    #[test]
    fn dry_threshold_equality_is_a_stable_failure() {
        let config = RepoRigorConfig::default();
        let clone_group_id = reporigor_core::stable_id(
            CLONE_RULE_ID,
            "src/lib.rs",
            "fixture::left|fixture::right",
            "same-normalized-shingles",
        );
        let duplicate = Duplicate {
            token_count: 12,
            locations: vec![
                duplicate_location(Some("fixture::left"), 2..=4, 0..12),
                duplicate_location(Some("fixture::right"), 20..=22, 20..32),
            ],
            clone_group_id: Some(clone_group_id.clone()),
            similarity: Some(config.dry.similarity_threshold),
            statement_count: Some(u32::try_from(config.dry.min_statements).unwrap_or(u32::MAX)),
            algorithm: Some("normalized-token-shingle-dice-v1".to_string()),
        };
        let analysis = fixture_analysis_with_duplicates(&config, std::slice::from_ref(&duplicate));
        let result = rule(&analysis, CLONE_RULE_ID);
        assert_eq!(result.result, RuleOutcome::Fail);
        assert_eq!(result.file, "src/lib.rs");

        let mut shifted = duplicate;
        shifted.locations.reverse();
        for location in &mut shifted.locations {
            location.start_line += 100;
            location.end_line += 100;
        }
        let shifted_analysis = fixture_analysis_with_duplicates(&config, &[shifted]);
        let shifted_result = rule(&shifted_analysis, CLONE_RULE_ID);
        assert_eq!(result.violation_id, shifted_result.violation_id);
    }

    #[test]
    fn dry_requires_reliable_ast_statement_counts() {
        let config = RepoRigorConfig::default();
        let duplicate = Duplicate {
            token_count: config.dry.min_tokens,
            locations: vec![
                duplicate_location(None, 2..=4, 0..config.dry.min_tokens),
                duplicate_location(
                    None,
                    20..=22,
                    config.dry.min_tokens..config.dry.min_tokens.saturating_mul(2),
                ),
            ],
            clone_group_id: Some(format!("{:064x}", 7)),
            similarity: Some(1.0),
            statement_count: None,
            algorithm: Some("normalized-token-exact-v1".to_string()),
        };
        let analysis = fixture_analysis_with_duplicates(&config, &[duplicate]);
        let clone_rows = analysis
            .results
            .iter()
            .filter(|result| result.rule_id == CLONE_RULE_ID)
            .collect::<Vec<_>>();
        assert_eq!(clone_rows.len(), 1);
        assert_eq!(clone_rows[0].stable_symbol, "repository clone inventory");
        assert_eq!(clone_rows[0].result, RuleOutcome::Pass);
    }

    #[test]
    fn evaluated_empty_clone_inventory_resolves_a_prior_group() {
        let config = RepoRigorConfig::default();
        let mut current = fixture_analysis(&config, &[], &[], &RepositorySemantics::default());
        let previous = vec![RuleResult::new(reporigor_core::RuleResultInput {
            rule_id: CLONE_RULE_ID.to_string(),
            file: "src/lib.rs".to_string(),
            stable_symbol: "fixture::left|fixture::right".to_string(),
            measured: json!(0.95),
            allowed: json!(config.dry.similarity_threshold),
            algorithm: "normalized-token-shingle-dice-v1".to_string(),
            comparison: RuleComparison::MaximumExclusive,
            structural_evidence: "prior-clone-group".to_string(),
        })
        .unwrap_or_else(|error| panic!("prior clone: {error}"))];
        let comparison = crate::apply_baseline(&mut current.results, Some(&previous), true)
            .unwrap_or_else(|error| panic!("baseline: {error}"));
        assert_eq!(comparison.resolved, 1);
        assert!(comparison.gate_passed);
    }

    #[test]
    fn cohesion_uses_shared_nonlocal_relations_without_confusing_getters() {
        let first = function("fixture::first", &["second", "Shared"]);
        let second = function("fixture::second", &["Shared"]);
        let third = function("fixture::third", &["Isolated"]);
        assert!((module_cohesion(&[&first, &second, &third]) - (1.0 / 3.0)).abs() < 1.0e-12);

        let getter = function("fixture::Account::balance", &[]);
        let debit = function("fixture::Account::debit", &["field::balance"]);
        let credit = function("fixture::Account::credit", &["field::balance"]);
        assert!((module_cohesion(&[&getter, &debit, &credit]) - (1.0 / 3.0)).abs() < 1.0e-12);
    }

    #[test]
    fn cohesion_relates_callers_that_share_one_local_helper() {
        let first = function("fixture::first", &["helper"]);
        let second = function("fixture::second", &["helper"]);
        let helper = function("fixture::helper", &[]);
        assert!((module_cohesion(&[&first, &second, &helper]) - 1.0).abs() < 1.0e-12);
    }

    #[test]
    fn cohesion_resolves_explicit_method_evidence() {
        let helper = function("fixture::Worker::helper", &[]);
        let caller = function("fixture::Worker::caller", &["method::helper"]);
        assert!((module_cohesion(&[&helper, &caller]) - 1.0).abs() < 1.0e-12);
    }

    #[test]
    fn cohesion_relates_methods_of_the_same_exact_trait_implementation() {
        let first = function("fixture::Worker as fixture::Contract::first", &[]);
        let second = function("fixture::Worker as fixture::Contract::second", &[]);
        assert_eq!(cohesion_owner(&first), "fixture::Worker as fixture::Contract");
        assert!((module_cohesion(&[&first, &second]) - 1.0).abs() < 1.0e-12);

        let inherent_first = function("fixture::Worker::first", &[]);
        let inherent_second = function("fixture::Worker::second", &[]);
        assert!(module_cohesion(&[&inherent_first, &inherent_second]).abs() < 1.0e-12);
    }

    #[test]
    fn cohesion_resolves_qualified_calls_without_guessing_ambiguous_leaves() {
        let left = function("fixture::left::same", &[]);
        let right = function("fixture::right::same", &[]);
        let ambiguous = function("fixture::caller", &["same"]);
        assert!(module_cohesion(&[&left, &right, &ambiguous]).abs() < 1.0e-12);

        let qualified = function("fixture::caller", &["fixture::left::same"]);
        assert!((module_cohesion(&[&left, &right, &qualified]) - (1.0 / 3.0)).abs() < 1.0e-12);
        assert_ne!(cohesion_owner(&left), cohesion_owner(&right));

        let overload_a = function("fixture::owner::same()#aaaaaaaaaaaa", &[]);
        let overload_b = function("fixture::owner::same()#bbbbbbbbbbbb", &[]);
        let overload_caller = function("fixture::owner::caller", &["fixture::owner::same"]);
        assert!(module_cohesion(&[&overload_a, &overload_b, &overload_caller]).abs() < 1.0e-12);
    }

    #[test]
    fn symbol_helpers_preserve_raw_identifiers_and_nested_signatures() {
        let raw = function("fixture::r#abcdefabcdef", &[]);
        assert_eq!(simple_name(&raw), "abcdefabcdef");
        assert_eq!(without_duplicate_suffix(&raw.stable_symbol), raw.stable_symbol);
        assert_eq!(
            without_terminal_signature("fixture::owner::call(fn(i32), (u8, u8))"),
            "fixture::owner::call"
        );
    }

    #[test]
    fn selected_test_functions_receive_kiss_metrics() {
        let config = RepoRigorConfig::default();
        let mut test_function = function("fixture::tests::complex_case", &[]);
        test_function.production = false;
        test_function.complexity = config.kiss.maximum_cyclomatic_complexity + 1;
        let analysis = fixture_analysis(&config, &[test_function], &[], &RepositorySemantics::default());
        let failure = analysis.results.iter().find(|result| {
            result.rule_id == "kiss.cyclomatic-complexity"
                && result.stable_symbol == "fixture::tests::complex_case"
        });
        assert_eq!(failure.map(|result| result.result), Some(RuleOutcome::Fail));
    }

    #[test]
    fn mixed_reliable_functions_mark_kiss_and_cohesion_incomplete() {
        let config = RepoRigorConfig::default();
        let reliable = function("fixture::reliable", &[]);
        let mut unreliable = function("fixture::unreliable", &[]);
        unreliable.structural_metrics_reliable = false;
        let analysis = fixture_analysis(
            &config,
            &[reliable, unreliable],
            &[],
            &RepositorySemantics::default(),
        );
        for rule_id in [
            "kiss.cyclomatic-complexity",
            "kiss.nesting-depth",
            "kiss.function-statements",
            "kiss.parameter-count",
            "cohesion.module",
        ] {
            assert!(analysis
                .omitted
                .iter()
                .any(|omission| omission.rule_id == rule_id));
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn dependency_predicates_and_yagni_are_explicit() {
        let mut config = RepoRigorConfig::default();
        config
            .architecture
            .forbidden_edges
            .push("domain->infra".to_string());
        config.architecture.domain_modules.push("domain".to_string());
        config
            .architecture
            .infrastructure_modules
            .push("infra".to_string());
        config.architecture.interface_modules = vec!["domain".to_string()];
        config.architecture.implementation_modules = vec!["infra".to_string()];
        config.architecture.layers = BTreeMap::from([("domain".to_string(), 0), ("infra".to_string(), 1)]);
        let mut unused_function = function("domain::unused", &[]);
        unused_function.package = Some("domain".to_string());
        let mut crate_export = function("domain::crate_export", &[]);
        crate_export.package = Some("domain".to_string());
        crate_export.visibility = SymbolVisibility::Crate;
        let mut public_api = function("domain::public_api", &[]);
        public_api.package = Some("domain".to_string());
        public_api.visibility = SymbolVisibility::Public;
        let mut entry_point = function("domain::generated_entry", &[]);
        entry_point.package = Some("domain".to_string());
        entry_point.entry_point = true;
        let repository = RepositorySemantics {
            dependency_graph_reliable: true,
            module_graph_reliable: true,
            identifier_counts_reliable: true,
            feature_inventory_reliable: true,
            unreachable_inventory_reliable: true,
            packages: vec![
                package("domain", "crates/domain"),
                package("infra", "crates/infra"),
            ],
            dependencies: vec![dependency("domain", "infra")],
            identifiers: ["unused", "crate_export", "public_api", "generated_entry"]
                .into_iter()
                .map(|identifier| IdentifierCountRecord {
                    identifier: identifier.to_string(),
                    package: Some("domain".to_string()),
                    production_references: 1,
                    test_references: 0,
                })
                .collect(),
            features: vec![
                feature("unused", &[]),
                feature("umbrella", &["unused-child"]),
                feature("unused-child", &[]),
            ],
            modules: [
                ModuleFixture::Unused,
                ModuleFixture::Public,
                ModuleFixture::TargetOnly,
                ModuleFixture::Generated,
            ]
            .map(module)
            .to_vec(),
            unreachable: vec![
                unreachable_record(
                    "crates/domain/src/lib.rs",
                    "domain::run",
                    "after=return|statement=work",
                    false,
                ),
                unreachable_record(
                    "crates/domain/src/target.rs",
                    "domain::target",
                    "after=return|statement=target",
                    true,
                ),
            ],
            ..RepositorySemantics::default()
        };
        let functions = vec![unused_function, crate_export, public_api, entry_point];
        let analysis = fixture_analysis(&config, &functions, &[], &repository);
        assert_rule_result(
            &analysis,
            "yagni.unused-private-function",
            None,
            Some(RuleOutcome::Fail),
            Some(BaselineDisposition::NotApplicable),
        );
        assert_rule_result(&analysis, "yagni.unused-production-dependency", None, None, None);
        assert_rule_result(
            &analysis,
            "yagni.unused-feature-flag",
            Some("domain::feature::unused"),
            None,
            None,
        );
        assert_rule_symbol_absent(
            &analysis,
            "yagni.unused-feature-flag",
            "domain::feature::umbrella",
        );
        assert_rule_symbol_absent(
            &analysis,
            "yagni.unused-feature-flag",
            "domain::feature::unused-child",
        );
        for rule_id in [
            "solid.dependency-direction",
            "solid.forbidden-module-edge",
            "solid.domain-to-infrastructure",
            "solid.interface-to-implementation",
        ] {
            assert_rule_result(&analysis, rule_id, None, Some(RuleOutcome::Fail), None);
        }
        assert_rule_count(&analysis, "yagni.unused-module", 1);
        assert_rule_count(&analysis, "yagni.unreachable-code", 1);
        assert_rule_result(
            &analysis,
            "yagni.unreferenced-crate-export",
            Some("domain::crate_export"),
            None,
            None,
        );
        assert_yagni_symbol_absent(&analysis, "domain::public_api");
        assert_yagni_symbol_absent(&analysis, "domain::generated_entry");
    }

    fn assert_rule_result(
        analysis: &QualityAnalysis,
        rule_id: &str,
        symbol: Option<&str>,
        outcome: Option<RuleOutcome>,
        baseline: Option<BaselineDisposition>,
    ) {
        assert!(analysis.results.iter().any(|result| {
            [
                result.rule_id == rule_id,
                symbol.is_none_or(|symbol| result.stable_symbol == symbol),
                outcome.is_none_or(|outcome| result.result == outcome),
                baseline.is_none_or(|baseline| result.baseline == baseline),
            ]
            .into_iter()
            .all(std::convert::identity)
        }));
    }

    fn assert_rule_symbol_absent(analysis: &QualityAnalysis, rule_id: &str, symbol: &str) {
        assert!(!analysis
            .results
            .iter()
            .any(|result| result.rule_id == rule_id && result.stable_symbol == symbol));
    }

    fn assert_rule_count(analysis: &QualityAnalysis, rule_id: &str, expected: usize) {
        assert_eq!(
            analysis
                .results
                .iter()
                .filter(|result| result.rule_id == rule_id)
                .count(),
            expected
        );
    }

    fn assert_yagni_symbol_absent(analysis: &QualityAnalysis, symbol: &str) {
        assert!(!analysis
            .results
            .iter()
            .any(|result| result.rule_id.starts_with("yagni.") && result.stable_symbol == symbol));
    }

    #[test]
    fn interface_dependency_predicate_requires_both_configured_sides() {
        let config = ArchitectureConfig {
            interface_modules: vec!["api".to_string()],
            implementation_modules: vec!["infra".to_string()],
            ..ArchitectureConfig::default()
        };
        let repository = RepositorySemantics {
            packages: vec![package("domain", "crates/domain")],
            dependencies: vec![dependency("domain", "infra")],
            ..RepositorySemantics::default()
        };
        let mut analysis = QualityAnalysis::default();
        dependency_rules(&mut analysis, &repository, &config)
            .unwrap_or_else(|error| panic!("dependency rules: {error}"));
        let result = rule(&analysis, "solid.interface-to-implementation");
        assert_eq!(result.stable_symbol, "domain -> infra");
        assert_eq!(result.result, RuleOutcome::Pass);
    }

    #[test]
    fn empty_reliable_architecture_inventories_emit_explicit_pass_rows() {
        let mut config = RepoRigorConfig::default();
        config.architecture.layers = BTreeMap::from([("domain".to_string(), 1)]);
        config.architecture.contract_traits = vec!["Contract".to_string()];
        let repository = RepositorySemantics {
            dependency_graph_reliable: true,
            trait_inventory_reliable: true,
            test_inventory_reliable: true,
            ..RepositorySemantics::default()
        };
        let analysis = repository_analysis(&config, &repository);
        for rule_id in [
            "solid.forbidden-module-edge",
            "solid.dependency-direction",
            "solid.subtype-contract-test",
        ] {
            assert_rule_result(&analysis, rule_id, None, Some(RuleOutcome::Pass), None);
        }
    }

    #[test]
    fn subtype_contract_rule_requires_an_exact_marked_test() {
        let mut config = RepoRigorConfig::default();
        config.architecture.contract_traits = vec!["Contract".to_string()];
        let repository = RepositorySemantics {
            trait_inventory_reliable: true,
            test_inventory_reliable: true,
            trait_implementations: vec![
                trait_implementation("fixture::Good", "src/good.rs", false),
                trait_implementation("fixture::Missing", "src/missing.rs", false),
                trait_implementation("fixture::TargetOnly", "src/target.rs", true),
            ],
            tests: vec![TestRecord {
                stable_symbol: "fixture::contract_test".to_string(),
                file: "src/tests.rs".to_string(),
                package: Some("fixture".to_string()),
                referenced_symbols: BTreeSet::from(["Contract".to_string(), "fixture::Good".to_string()]),
                markers: BTreeSet::from(["reporigor_contract".to_string()]),
                target_gated: false,
            }],
            ..RepositorySemantics::default()
        };
        let analysis = repository_analysis(&config, &repository);
        let contracts = analysis
            .results
            .iter()
            .filter(|result| result.rule_id == "solid.subtype-contract-test")
            .collect::<Vec<_>>();
        assert_eq!(contracts.len(), 2);
        for (symbol, outcome) in [
            ("fixture::Good", RuleOutcome::Pass),
            ("fixture::Missing", RuleOutcome::Fail),
        ] {
            assert!(contracts
                .iter()
                .any(|result| { result.stable_symbol.contains(symbol) && result.result == outcome }));
        }
    }
}
