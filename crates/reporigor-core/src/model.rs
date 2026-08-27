use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{de::Error as _, Deserialize, Deserializer, Serialize};

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
        match self {
            Self::Bash => "bash",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::ObjectiveC => "objective-c",
            Self::Python => "python",
            Self::Rust => "rust",
            Self::Swift => "swift",
            Self::TypeScript => "typescript",
        }
    }

    #[must_use]
    pub const fn is_c_family(self) -> bool {
        matches!(self, Self::C | Self::Cpp | Self::ObjectiveC)
    }

    #[must_use]
    pub const fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Bash => &["sh", "bash", "bats"],
            Self::C => &["c", "h"],
            Self::Cpp => &["cpp", "cc", "cxx", "hpp", "hh", "hxx", "h"],
            Self::ObjectiveC => &["m", "mm", "h"],
            Self::Python => &["py"],
            Self::Rust => &["rs"],
            Self::Swift => &["swift"],
            Self::TypeScript => &["ts", "tsx", "mts", "cts"],
        }
    }

    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.trim_start_matches('.').to_ascii_lowercase().as_str() {
            "sh" | "bash" | "bats" => Some(Self::Bash),
            "c" | "h" => Some(Self::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(Self::Cpp),
            "m" | "mm" => Some(Self::ObjectiveC),
            "py" => Some(Self::Python),
            "rs" => Some(Self::Rust),
            "swift" => Some(Self::Swift),
            "ts" | "tsx" | "mts" | "cts" => Some(Self::TypeScript),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_test_path(self, relative: &str) -> bool {
        let lower = relative.replace('\\', "/").to_ascii_lowercase();
        let name = lower.rsplit('/').next().unwrap_or(&lower);
        let parts: Vec<_> = lower.split('/').collect();
        let in_test_dir = parts.iter().any(|part| match self {
            Self::TypeScript => matches!(*part, "test" | "tests" | "__tests__"),
            Self::Swift | Self::ObjectiveC => matches!(*part, "test" | "tests"),
            _ => matches!(*part, "test" | "tests"),
        });
        if in_test_dir {
            return true;
        }
        match self {
            Self::Bash => std::path::Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("bats")),
            Self::C => name.ends_with("_test.c") || name.ends_with("_tests.c"),
            Self::Cpp => ["_test.cpp", "_tests.cpp", "_test.cc", "_tests.cc"]
                .iter()
                .any(|suffix| name.ends_with(suffix)),
            Self::ObjectiveC => {
                name.ends_with("tests.m")
                    || name.ends_with("tests.mm")
                    || name.ends_with("test.m")
                    || name.ends_with("test.mm")
            }
            Self::Python => name.starts_with("test_") || name.ends_with("_test.py"),
            Self::Rust => name.ends_with("_test.rs"),
            Self::Swift => name.ends_with("tests.swift") || name.ends_with("test.swift"),
            Self::TypeScript => [".test.ts", ".test.tsx", ".spec.ts", ".spec.tsx"]
                .iter()
                .any(|suffix| name.ends_with(suffix)),
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Language {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().replace('_', "-").as_str() {
            "bash" | "shell" | "sh" => Ok(Self::Bash),
            "c" => Ok(Self::C),
            "c++" | "cpp" | "cxx" => Ok(Self::Cpp),
            "objective-c" | "objectivec" | "objc" => Ok(Self::ObjectiveC),
            "python" | "py" => Ok(Self::Python),
            "rust" | "rs" => Ok(Self::Rust),
            "swift" => Ok(Self::Swift),
            "typescript" | "ts" | "tsx" => Ok(Self::TypeScript),
            _ => Err(format!("unsupported language: {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BackendPreference {
    #[default]
    Auto,
    Native,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Syntax,
    Functions,
    Complexity,
    Tokens,
    Mutations,
    ProjectSemantics,
    ParseValidation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BackendCapabilities {
    pub capabilities: BTreeSet<Capability>,
}

impl BackendCapabilities {
    #[must_use]
    pub fn new(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            capabilities: capabilities.into_iter().collect(),
        }
    }

    #[must_use]
    pub fn contains(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    pub id: String,
    pub version: String,
    pub native: bool,
    pub capabilities: BackendCapabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionRecord {
    pub language: Language,
    pub name: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub complexity: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crap: Option<f64>,
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

#[derive(Deserialize)]
struct MutationCandidateWire {
    id: u64,
    language: Language,
    file: String,
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
        if wire.line == 0 {
            return Err(D::Error::custom("mutation candidate line must be 1-based"));
        }
        if wire.column == 0 {
            return Err(D::Error::custom("mutation candidate column must be 1-based"));
        }
        let span_len = wire
            .end_byte
            .checked_sub(wire.start_byte)
            .ok_or_else(|| D::Error::custom("mutation candidate byte span is reversed"))?;
        if span_len != wire.original.len() {
            return Err(D::Error::custom(
                "mutation candidate byte span length does not match its original text",
            ));
        }
        Ok(Self {
            id: wire.id,
            language: wire.language,
            file: wire.file,
            line: wire.line,
            column: wire.column,
            original: wire.original,
            replacement: wire.replacement,
            start_byte: wire.start_byte,
            end_byte: wire.end_byte,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationResult {
    #[serde(flatten)]
    pub mutation: MutationCandidate,
    pub status: MutationStatus,
    pub exit_code: Option<i32>,
    pub duration_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
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

    pub fn assign_mutation_ids(&mut self) {
        self.mutations.sort_by(|left, right| {
            (&left.file, left.start_byte, left.end_byte, &left.replacement).cmp(&(
                &right.file,
                right.start_byte,
                right.end_byte,
                &right.replacement,
            ))
        });
        for (index, mutation) in self.mutations.iter_mut().enumerate() {
            mutation.id = u64::try_from(index + 1).unwrap_or(u64::MAX);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Language, MutationCandidate, MutationResult, MutationStatus, ProjectKind};

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
    fn serialized_report_mutation_preserves_validated_executable_offsets() {
        let result = MutationResult {
            mutation: MutationCandidate {
                id: 1,
                language: Language::Rust,
                file: "src/lib.rs".to_owned(),
                line: 1,
                column: 4,
                original: "==".to_owned(),
                replacement: "!=".to_owned(),
                start_byte: 3,
                end_byte: 5,
            },
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

        let invalid = valid.replace(r#""end_byte": 5"#, r#""end_byte": 4"#);
        assert!(serde_json::from_str::<MutationCandidate>(&invalid).is_err());

        let missing_span = r#"{
            "id": 1,
            "language": "rust",
            "file": "src/lib.rs",
            "line": 1,
            "column": 4,
            "original": "==",
            "replacement": "!="
        }"#;
        let Err(error) = serde_json::from_str::<MutationCandidate>(missing_span) else {
            panic!("candidate without a byte span must not deserialize");
        };
        assert!(error.to_string().contains("missing field `start_byte`"));
    }
}
