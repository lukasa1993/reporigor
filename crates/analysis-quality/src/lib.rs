//! Deterministic structural quality analysis over adapter-produced facts.
//!
//! This crate is deliberately non-executable. Language and project adapters
//! own syntax, symbols, references, and dependency metadata; this crate only
//! evaluates explicit predicates over the shared core records.

mod baseline;
mod graph;
mod rules;

pub use analysis_dry::{
    analyze_functions as analyze_rule_duplicates, analyze_with_functions as analyze_duplicates, Duplicate,
};
pub use baseline::{apply_baseline, apply_baseline_with_incomplete_rules, BaselineComparison};
pub use graph::{
    afferent_coupling, dependency_cycles, efferent_coupling, instability, matches_pattern, parse_edge_pattern,
};
pub use rules::{analyze_rules, OmittedCheck, QualityAnalysis, QualityInput, SurvivingMutant};

pub(crate) use reporigor_core::count_as_f64;
