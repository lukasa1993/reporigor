use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

macro_rules! serialized_enum {
    ($(#[$enum_attribute:meta])* [$($extra_derive:ident),*], $case:literal, $vis:vis enum $name:ident { $($variants:tt)* }) => {
        $(#[$enum_attribute])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize $(, $extra_derive)*
        )]
        #[serde(rename_all = $case)]
        $vis enum $name {
            $($variants)*
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Language {
    Bash,
    C,
    Cpp,
    ObjectiveC,
    Python,
    Rust,
    Swift,
    #[serde(rename = "typescript", alias = "type-script")]
    TypeScript,
}

impl Language {
    pub const ALL: [Self; 8] = [
        Self::Bash,
        Self::C,
        Self::Cpp,
        Self::ObjectiveC,
        Self::Python,
        Self::Rust,
        Self::Swift,
        Self::TypeScript,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        const NAMES: [&str; 8] = [
            "bash",
            "c",
            "cpp",
            "objective-c",
            "python",
            "rust",
            "swift",
            "typescript",
        ];
        NAMES[self as usize]
    }

    #[must_use]
    pub const fn is_c_family(self) -> bool {
        matches!(self, Self::C | Self::Cpp | Self::ObjectiveC)
    }

    #[must_use]
    pub const fn extensions(self) -> &'static [&'static str] {
        const EXTENSIONS: [&[&str]; 8] = [
            &["sh", "bash", "bats"],
            &["c", "h"],
            &["cpp", "cc", "cxx", "hpp", "hh", "hxx", "h"],
            &["m", "mm", "h"],
            &["py"],
            &["rs"],
            &["swift"],
            &["ts", "tsx", "mts", "cts"],
        ];
        EXTENSIONS[self as usize]
    }

    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        let extension = extension.trim_start_matches('.').to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|language| language.extensions().contains(&extension.as_str()))
    }

    #[must_use]
    pub fn is_test_path(self, relative: &str) -> bool {
        let lower = relative.replace('\\', "/").to_ascii_lowercase();
        let name = lower.rsplit('/').next().unwrap_or(&lower);
        if lower.split('/').any(|part| self.is_test_directory(part)) {
            return true;
        }
        self.test_prefixes().any(|prefix| name.starts_with(prefix))
            || self.test_suffixes().any(|suffix| name.ends_with(suffix))
    }

    fn is_test_directory(self, part: &str) -> bool {
        part == "test"
            || part == "tests"
            || (self as usize == Self::TypeScript as usize && part == "__tests__")
    }

    fn test_prefixes(self) -> impl Iterator<Item = &'static str> {
        const PREFIXES: [&str; 8] = ["", "", "", "", "test_", "", "", ""];
        encoded_values(PREFIXES[self as usize])
    }

    fn test_suffixes(self) -> impl Iterator<Item = &'static str> {
        const SUFFIXES: &str = ".bats
_test.c|_tests.c
_test.cpp|_tests.cpp|_test.cc|_tests.cc
tests.m|tests.mm|test.m|test.mm
_test.py
_test.rs
tests.swift|test.swift
.test.ts|.test.tsx|.spec.ts|.spec.tsx";
        encoded_values(SUFFIXES.lines().nth(self as usize).unwrap_or_default())
    }
}

fn encoded_values(values: &'static str) -> impl Iterator<Item = &'static str> {
    values.split('|').filter(|value| !value.is_empty())
}

impl fmt::Display for Language {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Language {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        const ALIASES: &[(&str, Language)] = &[
            ("bash", Language::Bash),
            ("shell", Language::Bash),
            ("sh", Language::Bash),
            ("c", Language::C),
            ("c++", Language::Cpp),
            ("cpp", Language::Cpp),
            ("cxx", Language::Cpp),
            ("objective-c", Language::ObjectiveC),
            ("objectivec", Language::ObjectiveC),
            ("objc", Language::ObjectiveC),
            ("python", Language::Python),
            ("py", Language::Python),
            ("rust", Language::Rust),
            ("rs", Language::Rust),
            ("swift", Language::Swift),
            ("typescript", Language::TypeScript),
            ("ts", Language::TypeScript),
            ("tsx", Language::TypeScript),
        ];
        let normalized = value.to_ascii_lowercase().replace('_', "-");
        ALIASES
            .iter()
            .find_map(|(alias, language)| (*alias == normalized).then_some(*language))
            .ok_or_else(|| format!("unsupported language: {value}"))
    }
}

serialized_enum!(
    [Default],
    "kebab-case",
    pub enum BackendPreference {
        #[default]
        Auto,
        Native,
        Generic,
    }
);

serialized_enum!(
    [],
    "snake_case",
    pub enum Capability {
        Syntax,
        Functions,
        Complexity,
        Tokens,
        Mutations,
        ProjectSemantics,
        ParseValidation,
    }
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BackendCapabilities {
    pub capabilities: BTreeSet<Capability>,
}

impl BackendCapabilities {
    #[must_use]
    pub fn new(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        let mut result = Self::default();
        for capability in capabilities {
            if !capabilities_contains(&result.capabilities, capability) {
                result.capabilities.insert(capability);
            }
        }
        result
    }

    #[must_use]
    pub fn contains(&self, capability: Capability) -> bool {
        capabilities_contains(&self.capabilities, capability)
    }
}

fn capabilities_contains(capabilities: &BTreeSet<Capability>, capability: Capability) -> bool {
    capabilities.contains(&capability)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    pub id: String,
    pub version: String,
    pub native: bool,
    pub capabilities: BackendCapabilities,
}

impl BackendInfo {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        version: impl Into<String>,
        native: bool,
        capabilities: impl IntoIterator<Item = Capability>,
    ) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            native,
            capabilities: BackendCapabilities::new(capabilities),
        }
    }
}

serialized_enum!(
    [],
    "kebab-case",
    pub enum Severity {
        Info,
        Warning,
        Error,
    }
);

/// A user-visible, half-open span in a source file.
///
/// Lines and columns are 1-based. Columns count Unicode scalar values (`char`s),
/// not UTF-8 bytes or rendered grapheme clusters. The start is inclusive and
/// the end is exclusive; for a one-scalar span at column 4, `end_column` is 5.
/// Backends must keep byte-oriented edit ranges separate from this display
/// location contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLocation {
    pub file: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// Internal half-open source span used to reconcile column-aware coverage
/// regions with syntax-owned function bodies. Lines and columns are 1-based.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
pub struct CoverageSpan {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub backend: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<SourceLocation>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback_used: bool,
}

impl Diagnostic {
    #[must_use]
    pub fn new(severity: Severity, backend: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity,
            backend: backend.into(),
            message: message.into(),
            location: None,
            fallback_used: false,
        }
    }
}

serialized_enum!(
    [],
    "kebab-case",
    pub enum ProjectKind {
        Cargo,
        CompilationDatabase,
        #[serde(rename = "typescript", alias = "type-script")]
        TypeScript,
        SwiftPackage,
        Python,
        Bash,
        Generic,
    }
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: PathBuf,
    pub relative: String,
    pub language: Language,
    #[serde(default)]
    pub generated: bool,
    #[serde(default)]
    pub test: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisRequest {
    pub root: PathBuf,
    #[serde(default)]
    pub languages: BTreeSet<Language>,
    #[serde(default)]
    pub filters: Vec<String>,
    #[serde(default)]
    pub include_tests: bool,
    #[serde(default)]
    pub allow_parse_errors: bool,
    #[serde(default = "default_max_source_bytes")]
    pub max_source_bytes: usize,
    #[serde(default)]
    pub backend: BackendPreference,
}

const fn default_max_source_bytes() -> usize {
    8 * 1024 * 1024
}

impl AnalysisRequest {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            languages: BTreeSet::new(),
            filters: Vec::new(),
            include_tests: false,
            allow_parse_errors: false,
            max_source_bytes: default_max_source_bytes(),
            backend: BackendPreference::Auto,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectContext {
    pub root: PathBuf,
    pub kinds: BTreeSet<ProjectKind>,
    pub sources: Vec<SourceFile>,
    pub backends: Vec<BackendInfo>,
    pub diagnostics: Vec<Diagnostic>,
}

serialized_enum!(
    #[doc = "Visibility reported by a syntax or project adapter."]
    [Default],
    "kebab-case",
    pub enum SymbolVisibility {
        #[default]
        Unknown,
        Private,
        Crate,
        Public,
    }
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionRecord {
    pub language: Language,
    pub name: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub complexity: u32,
    #[serde(default)]
    pub stable_symbol: String,
    #[serde(default)]
    pub nesting_depth: u32,
    #[serde(default)]
    pub statement_count: u32,
    #[serde(default)]
    pub parameter_count: u32,
    #[serde(default, skip_serializing)]
    pub normalized_tokens: Vec<String>,
    #[serde(default, skip_serializing)]
    pub references: BTreeSet<String>,
    /// Precise syntax span used only while assigning region-aware coverage.
    #[serde(default, skip_serializing)]
    pub coverage_span: CoverageSpan,
    /// Inclusive source-line ranges owned by nested executable boundaries.
    /// Line-coverage mapping excludes them from this function's denominator.
    #[serde(default, skip_serializing)]
    pub coverage_excluded_ranges: Vec<(u32, u32)>,
    /// Precise syntax spans owned by nested executable boundaries.
    #[serde(default, skip_serializing)]
    pub coverage_excluded_spans: Vec<CoverageSpan>,
    #[serde(default)]
    pub visibility: SymbolVisibility,
    #[serde(default)]
    pub structural_metrics_reliable: bool,
    /// Whether the declaration belongs to production code and is eligible for
    /// whole-project unused-symbol analysis.
    #[serde(default)]
    pub production: bool,
    #[serde(default)]
    pub entry_point: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crap: Option<f64>,
}

impl FunctionRecord {
    #[must_use]
    pub fn new(
        language: Language,
        name: impl Into<String>,
        file: impl Into<String>,
        start_line: u32,
        end_line: u32,
        complexity: u32,
    ) -> Self {
        let name = name.into();
        Self {
            language,
            stable_symbol: name.clone(),
            name,
            file: file.into(),
            start_line,
            end_line,
            complexity,
            nesting_depth: 0,
            statement_count: 0,
            parameter_count: 0,
            normalized_tokens: Vec::new(),
            references: BTreeSet::new(),
            coverage_span: CoverageSpan::default(),
            coverage_excluded_ranges: Vec::new(),
            coverage_excluded_spans: Vec::new(),
            visibility: SymbolVisibility::default(),
            structural_metrics_reliable: false,
            production: true,
            entry_point: false,
            package: None,
            coverage: None,
            crap: None,
        }
    }
}

/// Canonical declaration ordering shared by syntax adapters.
#[must_use]
pub fn compare_function_records(left: &FunctionRecord, right: &FunctionRecord) -> std::cmp::Ordering {
    (&left.file, &left.stable_symbol, left.start_line).cmp(&(
        &right.file,
        &right.stable_symbol,
        right.start_line,
    ))
}

serialized_enum!(
    [Default],
    "kebab-case",
    pub enum DependencyScope {
        #[default]
        Production,
        Development,
        Build,
    }
);

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PackageRecord {
    pub name: String,
    pub root: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DependencyRecord {
    pub package: String,
    pub dependency: String,
    /// Identifier used by source code, which may differ from the package name
    /// when a dependency is renamed in project metadata.
    #[serde(default)]
    pub source_identifier: String,
    #[serde(default)]
    pub scope: DependencyScope,
    #[serde(default)]
    pub internal: bool,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub target_gated: bool,
}

macro_rules! gated_repository_record {
    ($name:ident, [$($before_package:tt)*], [$($after_package:tt)*]) => {
        #[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[allow(clippy::struct_excessive_bools)]
        pub struct $name {
            $($before_package)*
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub package: Option<String>,
            $($after_package)*
            #[serde(default)]
            pub target_gated: bool,
        }
    };
}

gated_repository_record!(
    ModuleRecord,
    [
        pub stable_symbol: String,
        pub file: String,
    ],
    [
        #[serde(default)]
        pub visibility: SymbolVisibility,
        #[serde(default)]
        pub references: u32,
        #[serde(default)]
        pub generated: bool,
        #[serde(default)]
        pub framework_managed: bool,
        #[serde(default)]
        pub reflection_reachable: bool,
        #[serde(default)]
        pub externally_invoked: bool,
    ]
);

gated_repository_record!(
    UnreachableRecord,
    [
        pub file: String,
        pub stable_symbol: String,
        pub structural_evidence: String,
    ],
    []
);

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct IdentifierCountRecord {
    pub identifier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default)]
    pub production_references: u32,
    #[serde(default)]
    pub test_references: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FeatureRecord {
    pub package: String,
    pub name: String,
    #[serde(default)]
    pub references: u32,
    #[serde(default)]
    pub enables: BTreeSet<String>,
    #[serde(default)]
    pub target_gated: bool,
}

gated_repository_record!(
    TraitImplementationRecord,
    [
        pub trait_symbol: String,
        pub implementation_symbol: String,
        pub file: String,
    ],
    []
);

gated_repository_record!(
    TestRecord,
    [
        pub stable_symbol: String,
        pub file: String,
    ],
    [
        #[serde(default)]
        pub referenced_symbols: BTreeSet<String>,
        #[serde(default)]
        pub markers: BTreeSet<String>,
    ]
);

/// Whole-repository facts emitted by project-aware adapters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct RepositorySemantics {
    #[serde(default)]
    pub dependency_graph_reliable: bool,
    #[serde(default)]
    pub module_graph_reliable: bool,
    #[serde(default)]
    pub identifier_counts_reliable: bool,
    #[serde(default)]
    pub feature_inventory_reliable: bool,
    #[serde(default)]
    pub trait_inventory_reliable: bool,
    #[serde(default)]
    pub test_inventory_reliable: bool,
    #[serde(default)]
    pub unreachable_inventory_reliable: bool,
    #[serde(default)]
    pub packages: Vec<PackageRecord>,
    #[serde(default)]
    pub dependencies: Vec<DependencyRecord>,
    #[serde(default)]
    pub modules: Vec<ModuleRecord>,
    #[serde(default)]
    pub unreachable: Vec<UnreachableRecord>,
    #[serde(default)]
    pub identifiers: Vec<IdentifierCountRecord>,
    #[serde(default)]
    pub features: Vec<FeatureRecord>,
    #[serde(default)]
    pub trait_implementations: Vec<TraitImplementationRecord>,
    #[serde(default)]
    pub tests: Vec<TestRecord>,
}

impl RepositorySemantics {
    pub fn merge(&mut self, mut other: Self) {
        self.dependency_graph_reliable |= other.dependency_graph_reliable;
        self.module_graph_reliable |= other.module_graph_reliable;
        self.identifier_counts_reliable |= other.identifier_counts_reliable;
        self.feature_inventory_reliable |= other.feature_inventory_reliable;
        self.trait_inventory_reliable |= other.trait_inventory_reliable;
        self.test_inventory_reliable |= other.test_inventory_reliable;
        self.unreachable_inventory_reliable |= other.unreachable_inventory_reliable;
        self.packages.append(&mut other.packages);
        self.dependencies.append(&mut other.dependencies);
        self.modules.append(&mut other.modules);
        self.unreachable.append(&mut other.unreachable);
        self.identifiers.append(&mut other.identifiers);
        self.features.append(&mut other.features);
        self.trait_implementations
            .append(&mut other.trait_implementations);
        self.tests.append(&mut other.tests);
        self.canonicalize();
    }

    pub fn canonicalize(&mut self) {
        canonicalize_records(&mut self.packages);
        canonicalize_records(&mut self.dependencies);
        canonicalize_records(&mut self.modules);
        canonicalize_records(&mut self.unreachable);
        canonicalize_records(&mut self.identifiers);
        canonicalize_records(&mut self.features);
        canonicalize_records(&mut self.trait_implementations);
        canonicalize_records(&mut self.tests);
    }
}

fn canonicalize_records<T: Ord>(records: &mut Vec<T>) {
    records.sort();
    records.dedup();
}

serialized_enum!(
    [Default],
    "kebab-case",
    pub enum RuleComparison {
        Maximum,
        MaximumExclusive,
        Minimum,
        Boolean,
        #[default]
        Informational,
    }
);

serialized_enum!(
    [],
    "kebab-case",
    pub enum RuleOutcome {
        Pass,
        Fail,
    }
);

serialized_enum!(
    [Default],
    "kebab-case",
    pub enum BaselineDisposition {
        #[default]
        NotApplicable,
        Disabled,
        Existing,
        New,
        Worsened,
        Improved,
        Resolved,
    }
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleResult {
    pub rule_id: String,
    pub violation_id: String,
    pub file: String,
    pub stable_symbol: String,
    pub measured: serde_json::Value,
    pub allowed: serde_json::Value,
    pub algorithm: String,
    /// Canonical structural evidence used to derive `violation_id`.
    pub structural_evidence: String,
    pub result: RuleOutcome,
    #[serde(default)]
    pub baseline: BaselineDisposition,
    #[serde(default)]
    pub comparison: RuleComparison,
    #[serde(default)]
    pub excess: f64,
}

/// Complete deterministic input for one rule result.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleResultInput {
    pub rule_id: String,
    pub file: String,
    pub stable_symbol: String,
    pub comparison: RuleComparison,
    pub structural_evidence: String,
    pub algorithm: String,
    pub measured: serde_json::Value,
    pub allowed: serde_json::Value,
}

impl RuleResult {
    /// Construct and evaluate one deterministic structural rule result.
    ///
    /// # Errors
    ///
    /// Returns an error when identity fields or the repository-relative path
    /// are invalid, or when the measured/allowed values do not match the
    /// selected comparison.
    pub fn new(input: RuleResultInput) -> Result<Self, String> {
        let input = normalize_rule_input(input)?;
        let result = build_rule_result(input)?;
        let derived_excess = derive_rule_excess(&result)?;
        if derived_excess.to_bits() != result.excess.to_bits() {
            return Err("constructed rule result has inconsistent excess".to_string());
        }
        Ok(result)
    }

    /// Recompute the canonical excess from the serialized measurement,
    /// allowance, and comparison rather than trusting the stored derivative.
    ///
    /// # Errors
    ///
    /// Returns an error when the values do not match the comparison's scalar
    /// contract or produce a non-finite excess.
    pub fn derived_excess(&self) -> Result<f64, String> {
        derive_rule_excess(self)
    }
}

fn derive_rule_excess(result: &RuleResult) -> Result<f64, String> {
    evaluate_rule(&result.measured, &result.allowed, result.comparison).map(|(_, excess)| excess)
}

fn normalize_rule_input(mut input: RuleResultInput) -> Result<RuleResultInput, String> {
    validate_rule_input_identity(&input)?;
    input.file = crate::normalize_repository_path(&input.file)?;
    Ok(input)
}

fn validate_rule_input_identity(input: &RuleResultInput) -> Result<(), String> {
    validate_identity_field("rule_id", &input.rule_id)?;
    validate_identity_field("stable_symbol", &input.stable_symbol)?;
    validate_identity_field("algorithm", &input.algorithm)?;
    if input.structural_evidence.is_empty() {
        return Err("structural evidence must not be empty".to_string());
    }
    Ok(())
}

fn validate_identity_field(name: &str, value: &str) -> Result<(), String> {
    if !canonical_identity_field(value) {
        return Err(format!(
            "{name} must be non-empty and have no surrounding whitespace"
        ));
    }
    Ok(())
}

fn build_rule_result(input: RuleResultInput) -> Result<RuleResult, String> {
    let measured = canonical_rule_value(input.measured)?;
    let allowed = canonical_rule_value(input.allowed)?;
    let (result, excess) = evaluate_rule(&measured, &allowed, input.comparison)?;
    let violation_id = crate::stable_id(
        &input.rule_id,
        &input.file,
        &input.stable_symbol,
        &input.structural_evidence,
    );
    Ok(RuleResult {
        rule_id: input.rule_id,
        violation_id,
        file: input.file,
        stable_symbol: input.stable_symbol,
        measured,
        allowed,
        algorithm: input.algorithm,
        structural_evidence: input.structural_evidence,
        result,
        baseline: BaselineDisposition::NotApplicable,
        comparison: input.comparison,
        excess,
    })
}

fn canonical_rule_value(value: serde_json::Value) -> Result<serde_json::Value, String> {
    if !value.is_number() {
        return Ok(value);
    }
    serde_json::from_str(&value.to_string())
        .map_err(|error| format!("failed to canonicalize numeric rule value: {error}"))
}

fn evaluate_rule(
    measured: &serde_json::Value,
    allowed: &serde_json::Value,
    comparison: RuleComparison,
) -> Result<(RuleOutcome, f64), String> {
    let values = RuleValues { measured, allowed };
    match comparison {
        RuleComparison::Maximum => evaluate_maximum(values),
        RuleComparison::MaximumExclusive => evaluate_maximum_exclusive(values),
        RuleComparison::Minimum => evaluate_minimum(values),
        RuleComparison::Boolean => evaluate_boolean(values),
        RuleComparison::Informational => Ok((RuleOutcome::Pass, 0.0)),
    }
}

#[derive(Clone, Copy)]
struct RuleValues<'a> {
    measured: &'a serde_json::Value,
    allowed: &'a serde_json::Value,
}

impl RuleValues<'_> {
    fn finite_numbers(self) -> Result<(f64, f64), String> {
        Ok((
            finite_rule_number("measured", self.measured)?,
            finite_rule_number("allowed", self.allowed)?,
        ))
    }

    fn booleans(self) -> Result<(bool, bool), String> {
        let measured = self
            .measured
            .as_bool()
            .ok_or_else(|| "measured must be a boolean for a boolean comparison".to_string())?;
        let allowed = self
            .allowed
            .as_bool()
            .ok_or_else(|| "allowed must be a boolean for a boolean comparison".to_string())?;
        Ok((measured, allowed))
    }
}

fn evaluate_maximum(values: RuleValues<'_>) -> Result<(RuleOutcome, f64), String> {
    evaluate_inclusive(values, maximum_excess)
}

const fn maximum_excess(measured: f64, allowed: f64) -> f64 {
    measured - allowed
}

fn evaluate_maximum_exclusive(values: RuleValues<'_>) -> Result<(RuleOutcome, f64), String> {
    let (measured, allowed) = values.finite_numbers()?;
    let passed = measured < allowed;
    let excess = exclusive_excess(measured, allowed, passed);
    Ok((outcome(passed), finite_excess(excess)?))
}

fn exclusive_excess(measured: f64, allowed: f64, passed: bool) -> f64 {
    if passed {
        0.0
    } else {
        // Equality is a real exclusive-threshold violation.
        (measured - allowed).max(f64::EPSILON)
    }
}

fn evaluate_minimum(values: RuleValues<'_>) -> Result<(RuleOutcome, f64), String> {
    evaluate_inclusive(values, minimum_shortfall)
}

const fn minimum_shortfall(measured: f64, allowed: f64) -> f64 {
    allowed - measured
}

fn evaluate_inclusive(
    values: RuleValues<'_>,
    difference: fn(f64, f64) -> f64,
) -> Result<(RuleOutcome, f64), String> {
    let (measured, allowed) = values.finite_numbers()?;
    let excess = finite_excess(difference(measured, allowed).max(0.0))?;
    Ok((outcome(excess == 0.0), excess))
}

fn evaluate_boolean(values: RuleValues<'_>) -> Result<(RuleOutcome, f64), String> {
    values.booleans().map(|(measured, allowed)| {
        let passed = measured == allowed;
        (outcome(passed), boolean_excess(passed))
    })
}

const fn boolean_excess(passed: bool) -> f64 {
    if passed {
        0.0
    } else {
        1.0
    }
}

fn finite_rule_number(name: &str, value: &serde_json::Value) -> Result<f64, String> {
    value
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("{name} must be a finite number"))
}

fn finite_excess(excess: f64) -> Result<f64, String> {
    if excess.is_finite() {
        Ok(excess)
    } else {
        Err("comparison excess must be finite".to_string())
    }
}

const fn outcome(passed: bool) -> RuleOutcome {
    if passed {
        RuleOutcome::Pass
    } else {
        RuleOutcome::Fail
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub baseline_existing: usize,
    pub baseline_new: usize,
    pub baseline_worsened: usize,
    pub baseline_improved: usize,
    pub baseline_resolved: usize,
}

impl RuleSummary {
    #[must_use]
    pub fn from_results(results: &[RuleResult]) -> Self {
        let mut summary = Self {
            total: results.len(),
            ..Self::default()
        };
        for result in results {
            summary.record_outcome(result.result);
            summary.record_baseline(result.baseline);
        }
        summary
    }

    fn record_outcome(&mut self, outcome: RuleOutcome) {
        match outcome {
            RuleOutcome::Pass => self.passed += 1,
            RuleOutcome::Fail => self.failed += 1,
        }
    }

    fn record_baseline(&mut self, baseline: BaselineDisposition) {
        match baseline {
            BaselineDisposition::Existing => self.baseline_existing += 1,
            BaselineDisposition::New => self.baseline_new += 1,
            BaselineDisposition::Worsened => self.baseline_worsened += 1,
            BaselineDisposition::Improved => self.baseline_improved += 1,
            BaselineDisposition::Resolved => self.baseline_resolved += 1,
            BaselineDisposition::NotApplicable | BaselineDisposition::Disabled => {}
        }
    }
}

/// Sort rule results canonically, reject invalid paths or duplicate IDs, and
/// return deterministic summary counts.
///
/// # Errors
///
/// Returns an error when a result path is not canonical and repository-relative
/// or when violation IDs are malformed or duplicated.
pub fn canonicalize_rule_results(results: &mut [RuleResult]) -> Result<RuleSummary, String> {
    results.sort_by(|left, right| rule_result_key(left).cmp(&rule_result_key(right)));
    validate_rule_results(results)?;
    Ok(RuleSummary::from_results(results))
}

/// Validate canonical rule ordering, repository-relative paths, and globally
/// unique lowercase SHA-256 violation IDs.
///
/// # Errors
///
/// Returns a description of the first invalid result.
pub fn validate_rule_results(results: &[RuleResult]) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    let mut previous = None;
    for result in results {
        validate_rule_result(result)?;
        insert_unique_rule_id(&mut ids, result)?;
        let current = rule_result_key(result);
        validate_rule_order(previous.as_ref(), &current)?;
        previous = Some(current);
    }
    Ok(())
}

fn validate_rule_result(result: &RuleResult) -> Result<(), String> {
    validate_result_path(result)?;
    validate_violation_id_format(result)?;
    validate_result_identity(result)?;
    validate_result_stable_id(result)?;
    validate_result_outcome(result)
}

fn validate_result_path(result: &RuleResult) -> Result<(), String> {
    let normalized = crate::normalize_repository_path(&result.file)?;
    if normalized != result.file {
        return Err(format!("rule result path is not canonical: {}", result.file));
    }
    Ok(())
}

fn validate_violation_id_format(result: &RuleResult) -> Result<(), String> {
    if !crate::stable_id::is_lowercase_sha256(&result.violation_id) {
        return Err(format!(
            "rule result violation_id is not lowercase SHA-256: {}",
            result.violation_id
        ));
    }
    Ok(())
}

fn insert_unique_rule_id<'a>(ids: &mut BTreeSet<&'a str>, result: &'a RuleResult) -> Result<(), String> {
    if !ids.insert(result.violation_id.as_str()) {
        return Err(format!(
            "duplicate rule result violation_id: {}",
            result.violation_id
        ));
    }
    Ok(())
}

fn validate_result_identity(result: &RuleResult) -> Result<(), String> {
    let valid = canonical_identity_field(&result.rule_id)
        && canonical_identity_field(&result.stable_symbol)
        && canonical_identity_field(&result.algorithm)
        && !result.structural_evidence.is_empty();
    if !valid {
        return Err(format!(
            "rule result contains a non-canonical identity field: {}",
            result.violation_id
        ));
    }
    Ok(())
}

pub(crate) fn canonical_identity_field(value: &str) -> bool {
    !value.trim().is_empty() && value.trim() == value
}

fn validate_result_stable_id(result: &RuleResult) -> Result<(), String> {
    let expected_id = crate::stable_id(
        &result.rule_id,
        &result.file,
        &result.stable_symbol,
        &result.structural_evidence,
    );
    if result.violation_id != expected_id {
        return Err(format!(
            "rule result violation_id does not match its structural evidence: {}",
            result.violation_id
        ));
    }
    Ok(())
}

fn validate_result_outcome(result: &RuleResult) -> Result<(), String> {
    let (expected_result, expected_excess) =
        evaluate_rule(&result.measured, &result.allowed, result.comparison)?;
    if result.result != expected_result || !rule_excess_matches(result.excess, expected_excess) {
        return Err(format!(
            "rule result outcome or excess is inconsistent with its comparison: {}",
            result.violation_id
        ));
    }
    Ok(())
}

fn validate_rule_order(
    previous: Option<&(&str, &str, &str, &str)>,
    current: &(&str, &str, &str, &str),
) -> Result<(), String> {
    if previous.is_some_and(|previous| previous > current) {
        return Err("rule results are not in canonical order".to_string());
    }
    Ok(())
}

fn rule_excess_matches(stored: f64, expected: f64) -> bool {
    stored.to_bits() == expected.to_bits()
        || (stored.is_finite()
            && expected.is_finite()
            && stored >= 0.0
            && expected >= 0.0
            && stored.to_bits().abs_diff(expected.to_bits()) <= 1)
}

fn rule_result_key(result: &RuleResult) -> (&str, &str, &str, &str) {
    (
        &result.rule_id,
        &result.file,
        &result.stable_symbol,
        &result.violation_id,
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRecord {
    pub value: String,
    pub line: u32,
    pub index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MutationCandidate {
    pub id: u64,
    pub language: Language,
    pub file: String,
    #[serde(default)]
    pub stable_symbol: String,
    #[serde(default)]
    pub operator: String,
    #[serde(default)]
    pub fingerprint: String,
    /// 1-based source line containing the start of the candidate.
    pub line: u32,
    /// 1-based Unicode-scalar column containing the start of the candidate.
    pub column: u32,
    pub original: String,
    pub replacement: String,
    /// Inclusive 0-based UTF-8 byte offset used to apply the edit.
    pub start_byte: usize,
    /// Exclusive 0-based UTF-8 byte offset used to apply the edit.
    pub end_byte: usize,
}

impl MutationCandidate {
    #[must_use]
    pub fn new(
        language: Language,
        file: impl Into<String>,
        position: (u32, u32),
        original: impl Into<String>,
        replacement: impl Into<String>,
        byte_range: std::ops::Range<usize>,
    ) -> Self {
        Self {
            id: 0,
            language,
            file: file.into(),
            stable_symbol: String::new(),
            operator: String::new(),
            fingerprint: String::new(),
            line: position.0,
            column: position.1,
            original: original.into(),
            replacement: replacement.into(),
            start_byte: byte_range.start,
            end_byte: byte_range.end,
        }
    }

    #[must_use]
    pub fn with_identity(
        mut self,
        id: u64,
        stable_symbol: impl Into<String>,
        operator: impl Into<String>,
        fingerprint: impl Into<String>,
    ) -> Self {
        self.id = id;
        self.stable_symbol = stable_symbol.into();
        self.operator = operator.into();
        self.fingerprint = fingerprint.into();
        self
    }
}

#[derive(Deserialize)]
struct MutationCandidateWire {
    id: u64,
    language: Language,
    file: String,
    #[serde(default)]
    stable_symbol: String,
    #[serde(default)]
    operator: String,
    #[serde(default)]
    fingerprint: String,
    line: u32,
    column: u32,
    original: String,
    replacement: String,
    start_byte: usize,
    end_byte: usize,
}

impl<'de> Deserialize<'de> for MutationCandidate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = MutationCandidateWire::deserialize(deserializer)?;
        validate_mutation_wire::<D::Error>(&wire)?;
        Ok(Self {
            id: wire.id,
            language: wire.language,
            file: wire.file,
            stable_symbol: wire.stable_symbol,
            operator: wire.operator,
            fingerprint: wire.fingerprint,
            line: wire.line,
            column: wire.column,
            original: wire.original,
            replacement: wire.replacement,
            start_byte: wire.start_byte,
            end_byte: wire.end_byte,
        })
    }
}

fn validate_mutation_wire<E: serde::de::Error>(wire: &MutationCandidateWire) -> Result<(), E> {
    validate_mutation_position::<E>(wire)?;
    validate_mutation_span::<E>(wire)
}

fn validate_mutation_position<E: serde::de::Error>(wire: &MutationCandidateWire) -> Result<(), E> {
    if wire.line == 0 {
        return Err(E::custom("mutation candidate line must be 1-based"));
    }
    if wire.column == 0 {
        return Err(E::custom("mutation candidate column must be 1-based"));
    }
    Ok(())
}

fn validate_mutation_span<E: serde::de::Error>(wire: &MutationCandidateWire) -> Result<(), E> {
    let span_len = wire
        .end_byte
        .checked_sub(wire.start_byte)
        .ok_or_else(|| E::custom("mutation candidate byte span is reversed"))?;
    if span_len != wire.original.len() {
        return Err(E::custom(
            "mutation candidate byte span length does not match its original text",
        ));
    }
    Ok(())
}

serialized_enum!(
    [],
    "kebab-case",
    pub enum MutationStatus {
        Killed,
        Survived,
        NoCoverage,
        CompileError,
        RuntimeError,
        Timeout,
        Invalid,
        Ignored,
        Pending,
    }
);

impl MutationStatus {
    pub const ALL: [Self; 9] = [
        Self::Killed,
        Self::Survived,
        Self::NoCoverage,
        Self::CompileError,
        Self::RuntimeError,
        Self::Timeout,
        Self::Invalid,
        Self::Ignored,
        Self::Pending,
    ];
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationResult {
    #[serde(flatten)]
    pub mutation: MutationCandidate,
    pub status: MutationStatus,
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing)]
    pub duration_seconds: f64,
    #[serde(default, skip_serializing)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAnalysis {
    pub source: SourceFile,
    pub backend: BackendInfo,
    pub functions: Vec<FunctionRecord>,
    pub tokens: Vec<TokenRecord>,
    pub mutations: Vec<MutationCandidate>,
    pub diagnostics: Vec<Diagnostic>,
    pub parse_errors: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AnalysisSnapshot {
    pub files: Vec<SourceFile>,
    pub backends: Vec<BackendInfo>,
    pub functions: Vec<FunctionRecord>,
    pub tokens: BTreeMap<String, Vec<TokenRecord>>,
    pub mutations: Vec<MutationCandidate>,
    pub diagnostics: Vec<Diagnostic>,
    pub parse_errors: usize,
    #[serde(default)]
    pub repository: RepositorySemantics,
}

impl AnalysisSnapshot {
    pub fn push(&mut self, mut file: FileAnalysis) {
        self.parse_errors += file.parse_errors;
        self.files.push(file.source.clone());
        if !self.backends.iter().any(|backend| backend.id == file.backend.id) {
            self.backends.push(file.backend);
        }
        self.functions.append(&mut file.functions);
        self.tokens.insert(file.source.relative, file.tokens);
        self.mutations.append(&mut file.mutations);
        self.diagnostics.append(&mut file.diagnostics);
    }

    pub fn merge_repository_semantics(&mut self, semantics: RepositorySemantics) {
        self.repository.merge(semantics);
    }

    pub fn merge(&mut self, mut other: Self) {
        self.parse_errors += other.parse_errors;
        self.files.append(&mut other.files);
        self.backends.append(&mut other.backends);
        self.functions.append(&mut other.functions);
        for (file, mut tokens) in other.tokens {
            self.tokens.entry(file).or_default().append(&mut tokens);
        }
        self.mutations.append(&mut other.mutations);
        self.diagnostics.append(&mut other.diagnostics);
        self.repository.merge(other.repository);

        self.files.sort_by(|left, right| {
            (&left.relative, left.language, &left.path).cmp(&(&right.relative, right.language, &right.path))
        });
        self.files
            .dedup_by(|left, right| left.relative == right.relative && left.language == right.language);
        self.backends.sort_by(|left, right| {
            (&left.id, &left.version, left.native).cmp(&(&right.id, &right.version, right.native))
        });
        self.backends.dedup_by(|left, right| {
            left.id == right.id && left.version == right.version && left.native == right.native
        });
        self.functions.sort_by(|left, right| {
            (&left.file, &left.stable_symbol, left.start_line, left.end_line).cmp(&(
                &right.file,
                &right.stable_symbol,
                right.start_line,
                right.end_line,
            ))
        });
        for tokens in self.tokens.values_mut() {
            tokens.sort_by(|left, right| {
                (left.index, left.line, &left.value).cmp(&(right.index, right.line, &right.value))
            });
            tokens.dedup();
        }
        self.mutations.sort_by(|left, right| {
            (&left.file, left.start_byte, left.end_byte, &left.replacement).cmp(&(
                &right.file,
                right.start_byte,
                right.end_byte,
                &right.replacement,
            ))
        });
        self.diagnostics
            .sort_by(|left, right| (&left.backend, &left.message).cmp(&(&right.backend, &right.message)));
    }

    pub fn assign_mutation_ids(&mut self) {
        // First fill adapter-optional structural identity without changing any
        // adapter-provided fingerprint. Source locations are used only to map
        // a candidate to its containing stable symbol and to order otherwise
        // identical occurrences; they are never hashed.
        for mutation in &mut self.mutations {
            fill_mutation_structural_identity(&self.functions, mutation);
        }

        // Canonicalize before occurrence numbering so adapter discovery order
        // and worker scheduling cannot affect duplicate-edit fingerprints.
        self.mutations.sort_by(mutation_structural_location_cmp);
        assign_mutation_fingerprints(&mut self.mutations);

        // Run-local numeric IDs follow stable fingerprints. Lines and byte
        // offsets remain executable edit coordinates but never influence IDs.
        self.mutations.sort_by(|left, right| {
            (
                &left.file,
                &left.stable_symbol,
                &left.fingerprint,
                &left.operator,
                &left.original,
                &left.replacement,
            )
                .cmp(&(
                    &right.file,
                    &right.stable_symbol,
                    &right.fingerprint,
                    &right.operator,
                    &right.original,
                    &right.replacement,
                ))
        });
        for (index, mutation) in self.mutations.iter_mut().enumerate() {
            mutation.id = u64::try_from(index + 1).unwrap_or(u64::MAX);
        }
    }
}

fn fill_mutation_structural_identity(functions: &[FunctionRecord], mutation: &mut MutationCandidate) {
    if let Ok(file) = crate::normalize_repository_path(&mutation.file) {
        mutation.file = file;
    }
    if mutation.stable_symbol.is_empty() {
        mutation.stable_symbol = functions
            .iter()
            .filter(|function| {
                function.file.replace('\\', "/") == mutation.file
                    && function.start_line <= mutation.line
                    && mutation.line <= function.end_line
            })
            .min_by(|left, right| {
                left.end_line
                    .saturating_sub(left.start_line)
                    .cmp(&right.end_line.saturating_sub(right.start_line))
                    .then_with(|| left.stable_symbol.cmp(&right.stable_symbol))
                    .then_with(|| left.name.cmp(&right.name))
            })
            .map_or_else(
                || format!("{}::<module>", mutation.file),
                |function| {
                    if function.stable_symbol.is_empty() {
                        function.name.clone()
                    } else {
                        function.stable_symbol.clone()
                    }
                },
            );
    }
    if mutation.operator.is_empty() {
        mutation.operator = mutation_operator(&mutation.original).to_string();
    }
}

fn assign_mutation_fingerprints(mutations: &mut [MutationCandidate]) {
    let mut ordinals = BTreeMap::<(String, String, String, String, String), usize>::new();
    let mut collisions = BTreeMap::<String, usize>::new();
    let mut used = BTreeSet::new();
    let reserved = mutations
        .iter()
        .filter(|mutation| !mutation.fingerprint.is_empty())
        .map(|mutation| mutation.fingerprint.clone())
        .collect::<BTreeSet<_>>();
    for mutation in mutations {
        let key = (
            mutation.file.clone(),
            mutation.stable_symbol.clone(),
            mutation.operator.clone(),
            mutation.original.clone(),
            mutation.replacement.clone(),
        );
        let ordinal = ordinals.entry(key).or_default();
        let adapter_provided = !mutation.fingerprint.is_empty();
        let proposed = if adapter_provided {
            mutation.fingerprint.clone()
        } else {
            generated_mutation_fingerprint(mutation, *ordinal)
        };
        *ordinal = ordinal.saturating_add(1);
        let conflicts = if adapter_provided {
            !used.insert(proposed.clone())
        } else {
            reserved.contains(&proposed) || !used.insert(proposed.clone())
        };
        if !conflicts {
            mutation.fingerprint = proposed;
            continue;
        }
        mutation.fingerprint =
            disambiguated_fingerprint(mutation, &proposed, &reserved, &mut used, &mut collisions);
    }
}

fn generated_mutation_fingerprint(mutation: &MutationCandidate, ordinal: usize) -> String {
    let evidence = serde_json::to_string(&(
        mutation.operator.as_str(),
        mutation.original.as_str(),
        mutation.replacement.as_str(),
        ordinal,
    ))
    .unwrap_or_default();
    crate::stable_id(
        "mutation.fingerprint",
        &mutation.file,
        &mutation.stable_symbol,
        &evidence,
    )
}

fn disambiguated_fingerprint(
    mutation: &MutationCandidate,
    proposed: &str,
    reserved: &BTreeSet<String>,
    used: &mut BTreeSet<String>,
    collisions: &mut BTreeMap<String, usize>,
) -> String {
    let collision = collisions.entry(proposed.to_string()).or_insert(1);
    loop {
        let evidence = serde_json::to_string(&("provided-fingerprint-collision", proposed, *collision))
            .unwrap_or_default();
        let fingerprint = crate::stable_id(
            "mutation.fingerprint",
            &mutation.file,
            &mutation.stable_symbol,
            &evidence,
        );
        *collision = collision.saturating_add(1);
        if !reserved.contains(&fingerprint) && used.insert(fingerprint.clone()) {
            return fingerprint;
        }
    }
}

fn mutation_structural_location_cmp(
    left: &MutationCandidate,
    right: &MutationCandidate,
) -> std::cmp::Ordering {
    (
        &left.file,
        &left.stable_symbol,
        &left.operator,
        &left.original,
        &left.replacement,
        left.line,
        left.column,
        left.start_byte,
        left.end_byte,
        &left.fingerprint,
    )
        .cmp(&(
            &right.file,
            &right.stable_symbol,
            &right.operator,
            &right.original,
            &right.replacement,
            right.line,
            right.column,
            right.start_byte,
            right.end_byte,
            &right.fingerprint,
        ))
}

fn mutation_operator(original: &str) -> &'static str {
    const FAMILIES: &str = "true,false,True,False,YES,NO:boolean-literal;==,!=,<,<=,>,>=,-eq,-ne,-gt,-ge,-lt,-le:comparison;&&,||,and,or:logical;+,-,*,/,%:arithmetic";
    FAMILIES
        .split(';')
        .find_map(|family| {
            let (operators, name) = family.split_once(':')?;
            operators
                .split(',')
                .any(|operator| operator == original)
                .then_some(name)
        })
        .unwrap_or("other")
}

#[cfg(test)]
mod tests {
    use super::{
        canonicalize_rule_results, validate_rule_results, AnalysisSnapshot, BackendCapabilities,
        BaselineDisposition, Capability, DependencyRecord, Language, MutationCandidate, MutationResult,
        MutationStatus, PackageRecord, ProjectKind, RepositorySemantics, RuleComparison, RuleOutcome,
        RuleResult, RuleSummary, TokenRecord,
    };

    fn snapshot_with_token(parse_errors: usize, value: &str, line: u32, index: usize) -> AnalysisSnapshot {
        AnalysisSnapshot {
            parse_errors,
            tokens: std::collections::BTreeMap::from([(
                "src/lib.rs".to_string(),
                vec![TokenRecord {
                    value: value.to_string(),
                    line,
                    index,
                }],
            )]),
            ..AnalysisSnapshot::default()
        }
    }

    fn assigned_snapshot(mutations: Vec<MutationCandidate>) -> AnalysisSnapshot {
        let mut snapshot = AnalysisSnapshot {
            mutations,
            ..AnalysisSnapshot::default()
        };
        snapshot.assign_mutation_ids();
        snapshot
    }

    fn mutation_fingerprints(snapshot: &AnalysisSnapshot) -> std::collections::BTreeSet<&str> {
        snapshot
            .mutations
            .iter()
            .map(|mutation| mutation.fingerprint.as_str())
            .collect()
    }

    #[test]
    fn typescript_uses_one_stable_machine_name() {
        assert_eq!(
            serde_json::to_string(&Language::TypeScript).unwrap_or_default(),
            r#""typescript""#
        );
        assert_eq!(
            serde_json::to_string(&ProjectKind::TypeScript).unwrap_or_default(),
            r#""typescript""#
        );
        assert_eq!(
            serde_json::from_str::<Language>(r#""type-script""#).unwrap_or(Language::Bash),
            Language::TypeScript
        );
    }

    #[test]
    fn backend_capabilities_deduplicate_and_answer_membership() {
        let capabilities =
            BackendCapabilities::new([Capability::Syntax, Capability::Tokens, Capability::Syntax]);
        assert_eq!(capabilities.capabilities.len(), 2);
        assert!(capabilities.contains(Capability::Syntax));
        assert!(!capabilities.contains(Capability::Mutations));
    }

    #[test]
    fn rule_summary_counts_every_baseline_disposition() {
        let mut summary = RuleSummary::default();
        for disposition in [
            BaselineDisposition::NotApplicable,
            BaselineDisposition::Disabled,
            BaselineDisposition::Existing,
            BaselineDisposition::New,
            BaselineDisposition::Worsened,
            BaselineDisposition::Improved,
            BaselineDisposition::Resolved,
        ] {
            summary.record_baseline(disposition);
        }
        assert_eq!(summary.baseline_existing, 1);
        assert_eq!(summary.baseline_new, 1);
        assert_eq!(summary.baseline_worsened, 1);
        assert_eq!(summary.baseline_improved, 1);
        assert_eq!(summary.baseline_resolved, 1);
    }

    #[test]
    fn analysis_snapshot_merge_combines_and_canonicalizes_token_maps() {
        let mut first = snapshot_with_token(1, "z", 2, 1);
        let second = snapshot_with_token(2, "a", 1, 0);
        first.merge(second);
        assert_eq!(first.parse_errors, 3);
        let tokens = first
            .tokens
            .get("src/lib.rs")
            .map(Vec::as_slice)
            .unwrap_or_default();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].value, "a");
    }

    #[test]
    fn mutation_operator_classification_covers_every_operator_family() {
        for encoded in "true:boolean-literal|==:comparison|&&:logical|+:arithmetic|value:other".split('|') {
            let (original, expected) = encoded
                .split_once(':')
                .unwrap_or_else(|| panic!("invalid operator fixture: {encoded}"));
            assert_eq!(super::mutation_operator(original), expected);
        }
    }

    #[test]
    fn function_record_additions_default_when_reading_an_older_snapshot() {
        let source = r#"{
            "language": "rust",
            "name": "run",
            "file": "src/lib.rs",
            "start_line": 3,
            "end_line": 7,
            "complexity": 2
        }"#;
        let function = serde_json::from_str::<super::FunctionRecord>(source)
            .unwrap_or_else(|error| panic!("function: {error}"));
        assert!(function.stable_symbol.is_empty());
        assert_eq!(function.statement_count, 0);
        assert!(function.normalized_tokens.is_empty());
        assert!(!function.structural_metrics_reliable);
    }

    #[test]
    fn repository_semantics_merge_is_sorted_and_deduplicated() {
        let mut first = RepositorySemantics {
            packages: vec![PackageRecord {
                name: "zeta".to_string(),
                root: "crates/zeta".to_string(),
            }],
            ..RepositorySemantics::default()
        };
        let second = RepositorySemantics {
            dependency_graph_reliable: true,
            packages: vec![
                PackageRecord {
                    name: "alpha".to_string(),
                    root: "crates/alpha".to_string(),
                },
                PackageRecord {
                    name: "zeta".to_string(),
                    root: "crates/zeta".to_string(),
                },
            ],
            dependencies: vec![DependencyRecord {
                package: "zeta".to_string(),
                dependency: "alpha".to_string(),
                internal: true,
                ..DependencyRecord::default()
            }],
            ..RepositorySemantics::default()
        };
        first.merge(second);
        assert!(first.dependency_graph_reliable);
        assert_eq!(
            first
                .packages
                .iter()
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
        assert_eq!(first.dependencies.len(), 1);
    }

    fn mutation_candidate(
        stable_symbol: &str,
        line: u32,
        start_byte: usize,
        fingerprint: &str,
    ) -> MutationCandidate {
        MutationCandidate {
            id: 0,
            language: Language::Rust,
            file: "src/lib.rs".to_string(),
            stable_symbol: stable_symbol.to_string(),
            operator: "comparison".to_string(),
            fingerprint: fingerprint.to_string(),
            line,
            column: 5,
            original: "==".to_string(),
            replacement: "!=".to_string(),
            start_byte,
            end_byte: start_byte + 2,
        }
    }

    fn mutation_candidates(cases: &[(&str, u32, usize, &str)]) -> Vec<MutationCandidate> {
        cases
            .iter()
            .map(|(symbol, line, start, fingerprint)| mutation_candidate(symbol, *line, *start, fingerprint))
            .collect()
    }

    fn moved_mutation_candidates(
        line_offset: u32,
        byte_offset: usize,
        reverse: bool,
    ) -> Vec<MutationCandidate> {
        let mut cases = vec![
            ("crate::same", 10 + line_offset, 100 + byte_offset, ""),
            ("crate::same", 20 + line_offset, 200 + byte_offset, ""),
            ("crate::other", 30 + line_offset, 300 + byte_offset, ""),
        ];
        if reverse {
            cases.reverse();
        }
        mutation_candidates(&cases)
    }

    #[test]
    fn mutation_fingerprints_ignore_input_order_and_unrelated_line_movement() {
        let original = moved_mutation_candidates(0, 0, false);
        let first = assigned_snapshot(original);

        let moved = moved_mutation_candidates(100, 1_000, true);
        let second = assigned_snapshot(moved);

        let identities = |snapshot: &super::AnalysisSnapshot| {
            snapshot
                .mutations
                .iter()
                .map(|mutation| {
                    (
                        mutation.id,
                        mutation.stable_symbol.clone(),
                        mutation.fingerprint.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(identities(&first), identities(&second));
        let unique = mutation_fingerprints(&first);
        assert_eq!(unique.len(), first.mutations.len());
        assert!(first
            .mutations
            .iter()
            .all(|mutation| crate::stable_id::is_lowercase_sha256(&mutation.fingerprint)));
    }

    #[test]
    fn mutation_assignment_preserves_unique_adapter_fingerprints_and_disambiguates_duplicates() {
        let snapshot = assigned_snapshot(mutation_candidates(&[
            ("crate::first", 1, 0, "adapter-kept"),
            ("crate::second", 2, 10, "adapter-kept"),
            ("crate::third", 3, 20, "adapter-other"),
        ]));
        let fingerprints = mutation_fingerprints(&snapshot);
        assert_eq!(fingerprints.len(), 3);
        assert!(fingerprints.contains("adapter-kept"));
        assert!(fingerprints.contains("adapter-other"));
    }

    #[test]
    fn mutation_assignment_uses_stable_symbols_for_duplicate_function_names() {
        let function = |stable_symbol: &str, start_line: u32, end_line: u32| super::FunctionRecord {
            language: Language::Rust,
            name: "same".to_string(),
            file: "src/lib.rs".to_string(),
            start_line,
            end_line,
            complexity: 1,
            stable_symbol: stable_symbol.to_string(),
            nesting_depth: 0,
            statement_count: 1,
            parameter_count: 0,
            normalized_tokens: Vec::new(),
            references: std::collections::BTreeSet::new(),
            coverage_span: super::CoverageSpan::default(),
            coverage_excluded_ranges: Vec::new(),
            coverage_excluded_spans: Vec::new(),
            visibility: super::SymbolVisibility::Private,
            structural_metrics_reliable: true,
            production: true,
            entry_point: false,
            package: Some("fixture".to_string()),
            coverage: None,
            crap: None,
        };
        let mut first = mutation_candidate("", 2, 10, "");
        first.operator.clear();
        let mut second = mutation_candidate("", 12, 100, "");
        second.operator.clear();
        let mut snapshot = super::AnalysisSnapshot {
            functions: vec![
                function("fixture::Left::same", 1, 5),
                function("fixture::Right::same", 10, 15),
            ],
            mutations: vec![second, first],
            ..super::AnalysisSnapshot::default()
        };
        snapshot.assign_mutation_ids();
        let symbols = snapshot
            .mutations
            .iter()
            .map(|mutation| mutation.stable_symbol.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            symbols,
            ["fixture::Left::same", "fixture::Right::same"]
                .into_iter()
                .collect()
        );
        assert!(snapshot
            .mutations
            .iter()
            .all(|mutation| mutation.operator == "comparison"));
    }

    #[derive(Clone, Copy)]
    #[repr(usize)]
    enum RuleCase {
        Maximum,
        Above,
        Exclusive,
        Minimum,
        Boolean,
        Informational,
        First,
        Second,
        Outside,
        Fractional,
    }

    type RuleFixture = (
        &'static str,
        &'static str,
        &'static str,
        serde_json::Value,
        serde_json::Value,
        &'static str,
        RuleComparison,
        &'static str,
    );

    fn rule_fixture(case: RuleCase) -> Result<RuleFixture, String> {
        const FIXTURES: &str = r"kiss.complexity~src/lib.rs~crate::run~12~12~cyclomatic complexity <= configured maximum~maximum~if|match
kiss.complexity~src/lib.rs~crate::run~13~12~cyclomatic complexity <= configured maximum~maximum~if|match
dry.clone~src/lib.rs~crate::left|crate::right~0.92~0.92~normalized-token shingle Dice similarity >= configured threshold~maximum-exclusive~clone-group
cohesion.minimum~src/lib.rs~crate~0.09~0.10~related function pairs / all function pairs~minimum~a-b|a-c|b-c
architecture.no-cycle~Cargo.toml~workspace~false~true~package strongly connected component must be absent~boolean~a->b->a
coupling.afferent~Cargo.toml~crate~3~null~number of internal dependents~informational~a|b|c
kiss.parameters~src\z.rs~crate::z~2~6~parameter count <= configured maximum~maximum~two-parameters
kiss.parameters~src/a.rs~crate::a~1~6~parameter count <= configured maximum~maximum~one-parameter
kiss.parameters~../outside.rs~crate::outside~1~6~parameter count <= configured maximum~maximum~one-parameter
cohesion.module~src/lib.rs~crate::module~0.08888888888888889~0.1~related pairs / all pairs~minimum~qualified-owner-function-reference-graph-v1";
        let encoded = FIXTURES
            .lines()
            .nth(case as usize)
            .ok_or_else(|| "missing encoded rule fixture".to_string())?;
        parse_rule_fixture(encoded)
    }

    fn parse_rule_fixture(encoded: &'static str) -> Result<RuleFixture, String> {
        let fields = encoded.split('~').collect::<Vec<_>>();
        if fields.len() != 8 {
            return Err("invalid encoded rule fixture".to_string());
        }
        build_rule_fixture(&fields)
    }

    fn build_rule_fixture(fields: &[&'static str]) -> Result<RuleFixture, String> {
        let measured =
            serde_json::from_str(fields[3]).map_err(|error| format!("measured fixture: {error}"))?;
        let allowed = serde_json::from_str(fields[4]).map_err(|error| format!("allowed fixture: {error}"))?;
        let comparison = parse_rule_comparison(fields[6])?;
        Ok((
            fields[0], fields[1], fields[2], measured, allowed, fields[5], comparison, fields[7],
        ))
    }

    fn parse_rule_comparison(value: &str) -> Result<RuleComparison, String> {
        const COMPARISONS: [(&str, RuleComparison); 5] = [
            ("maximum", RuleComparison::Maximum),
            ("maximum-exclusive", RuleComparison::MaximumExclusive),
            ("minimum", RuleComparison::Minimum),
            ("boolean", RuleComparison::Boolean),
            ("informational", RuleComparison::Informational),
        ];
        COMPARISONS
            .into_iter()
            .find_map(|(name, comparison)| (name == value).then_some(comparison))
            .ok_or_else(|| format!("invalid fixture comparison: {value}"))
    }

    fn rule_result(case: RuleCase) -> Result<RuleResult, String> {
        let (rule_id, file, symbol, measured, allowed, algorithm, comparison, evidence) = rule_fixture(case)?;
        crate::rule_result!(rule_id, file, symbol, measured, allowed, algorithm, comparison, evidence)
    }

    fn valid_rule_result(case: RuleCase) -> RuleResult {
        rule_result(case).unwrap_or_else(|error| panic!("valid rule fixture: {error}"))
    }

    #[test]
    fn rule_comparisons_include_threshold_boundaries() {
        let maximum = valid_rule_result(RuleCase::Maximum);
        assert_eq!(maximum.result, RuleOutcome::Pass);
        assert!(maximum.excess.abs() < f64::EPSILON);
        let serialized =
            serde_json::to_value(&maximum).unwrap_or_else(|error| panic!("serialize rule result: {error}"));
        assert_eq!(serialized["comparison"], "maximum");
        assert_eq!(serialized["excess"], 0.0);

        let above = valid_rule_result(RuleCase::Above);
        assert_eq!(above.result, RuleOutcome::Fail);
        assert!((above.excess - 1.0).abs() < f64::EPSILON);
        assert_eq!(maximum.violation_id, above.violation_id);

        let exclusive = valid_rule_result(RuleCase::Exclusive);
        assert_eq!(exclusive.result, RuleOutcome::Fail);
        assert!(exclusive.excess > 0.0);

        let minimum = valid_rule_result(RuleCase::Minimum);
        assert_eq!(minimum.result, RuleOutcome::Fail);
        assert!((minimum.excess - 0.01).abs() < f64::EPSILON * 8.0);

        let boolean = valid_rule_result(RuleCase::Boolean);
        assert_eq!(boolean.result, RuleOutcome::Fail);
        assert!((boolean.excess - 1.0).abs() < f64::EPSILON);

        let informational = valid_rule_result(RuleCase::Informational);
        assert_eq!(informational.result, RuleOutcome::Pass);
    }

    #[test]
    fn rule_results_are_canonical_relative_and_unique() {
        let first = valid_rule_result(RuleCase::First);
        assert_eq!(first.file, "src/z.rs");
        let second = valid_rule_result(RuleCase::Second);
        let mut results = vec![first, second];
        let summary =
            canonicalize_rule_results(&mut results).unwrap_or_else(|error| panic!("canonical: {error}"));
        assert_eq!(summary.total, 2);
        assert_eq!(summary.passed, 2);
        assert_eq!(results[0].file, "src/a.rs");
        assert!(validate_rule_results(&results).is_ok());

        let mut duplicate = vec![results[0].clone(), results[0].clone()];
        assert!(canonicalize_rule_results(&mut duplicate).is_err());
        assert!(rule_result(RuleCase::Outside).is_err());
    }

    #[test]
    fn fractional_rule_excess_survives_json_round_trip() {
        let result = valid_rule_result(RuleCase::Fractional);
        let encoded = serde_json::to_string(&result).unwrap_or_else(|error| panic!("serialize: {error}"));
        let decoded: RuleResult =
            serde_json::from_str(&encoded).unwrap_or_else(|error| panic!("deserialize: {error}"));
        assert!(
            validate_rule_results(std::slice::from_ref(&decoded)).is_ok(),
            "round trip changed derived excess: measured={:x} allowed={:x} stored={:x}",
            decoded.measured.as_f64().unwrap_or_default().to_bits(),
            decoded.allowed.as_f64().unwrap_or_default().to_bits(),
            decoded.excess.to_bits()
        );
    }

    #[test]
    fn serialized_report_mutation_preserves_validated_executable_offsets() {
        let result = MutationResult {
            mutation: MutationCandidate::new(Language::Rust, "src/lib.rs", (1, 4), "==", "!=", 3..5)
                .with_identity(1, "crate::run", "comparison", "fixture-fingerprint"),
            status: MutationStatus::Pending,
            exit_code: None,
            duration_seconds: 0.0,
            detail: None,
        };

        let serialized = serde_json::to_string(&result).unwrap_or_default();
        assert!(serialized.contains(r#""start_byte":3"#));
        assert!(serialized.contains(r#""end_byte":5"#));
        let deserialized = serde_json::from_str::<MutationResult>(&serialized);
        assert!(matches!(
            deserialized,
            Ok(result) if result.mutation.start_byte == 3 && result.mutation.end_byte == 5
        ));
    }

    #[test]
    fn executable_candidate_wire_requires_a_consistent_byte_span() {
        let valid = r#"{
            "id": 1,
            "language": "rust",
            "file": "src/lib.rs",
            "line": 1,
            "column": 4,
            "original": "==",
            "replacement": "!=",
            "start_byte": 3,
            "end_byte": 5
        }"#;
        let parsed = serde_json::from_str::<MutationCandidate>(valid);
        assert!(matches!(parsed, Ok(candidate) if candidate.start_byte == 3));

        for invalid in [
            valid.replace(r#""line": 1"#, r#""line": 0"#),
            valid.replace(r#""column": 4"#, r#""column": 0"#),
            valid.replace(r#""end_byte": 5"#, r#""end_byte": 2"#),
            valid.replace(r#""end_byte": 5"#, r#""end_byte": 4"#),
        ] {
            assert!(serde_json::from_str::<MutationCandidate>(&invalid).is_err());
        }

        let missing_span = r#"{
            "id": 1,
            "language": "rust",
            "file": "src/lib.rs",
            "line": 1,
            "column": 4,
            "original": "==",
            "replacement": "!="
        }"#;
        let error = crate::path_io::require_error(
            serde_json::from_str::<MutationCandidate>(missing_span),
            "candidate without a byte span must not deserialize",
        );
        assert!(error.to_string().contains("missing field `start_byte`"));
    }
}
