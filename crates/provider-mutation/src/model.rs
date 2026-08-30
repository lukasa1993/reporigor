use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use reporigor_core::{Language, MutationResult};
use serde::{Deserialize, Serialize};

/// Mutation engine selected at the provider boundary.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(usize)]
pub enum MutationProvider {
    BuiltIn,
    CargoMutants,
    Mutmut,
    Stryker,
    Mull,
    Muter,
}

const PROVIDER_NAMES: [&str; MutationProvider::ALL.len()] =
    ["built-in", "cargo-mutants", "mutmut", "stryker", "mull", "muter"];

const PROVIDER_ALIASES: [(&str, MutationProvider); 12] = [
    ("built-in", MutationProvider::BuiltIn),
    ("builtin", MutationProvider::BuiltIn),
    ("reporigor", MutationProvider::BuiltIn),
    ("cargo-mutants", MutationProvider::CargoMutants),
    ("cargomutants", MutationProvider::CargoMutants),
    ("mutmut", MutationProvider::Mutmut),
    ("stryker", MutationProvider::Stryker),
    ("strykerjs", MutationProvider::Stryker),
    ("stryker-js", MutationProvider::Stryker),
    ("mull", MutationProvider::Mull),
    ("mull-runner", MutationProvider::Mull),
    ("muter", MutationProvider::Muter),
];

impl MutationProvider {
    pub const ALL: [Self; 6] = [
        Self::BuiltIn,
        Self::CargoMutants,
        Self::Mutmut,
        Self::Stryker,
        Self::Mull,
        Self::Muter,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        PROVIDER_NAMES[self as usize]
    }

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::BuiltIn => "reporigor built-in",
            Self::CargoMutants => "cargo-mutants",
            Self::Mutmut => "mutmut",
            Self::Stryker => "StrykerJS",
            Self::Mull => "Mull",
            Self::Muter => "Muter",
        }
    }

    #[must_use]
    pub const fn languages(self) -> &'static [Language] {
        match self {
            Self::BuiltIn => &Language::ALL,
            Self::CargoMutants => &[Language::Rust],
            Self::Mutmut => &[Language::Python],
            Self::Stryker => &[Language::TypeScript],
            Self::Mull => &[Language::C, Language::Cpp],
            Self::Muter => &[Language::Swift],
        }
    }
}

impl fmt::Display for MutationProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(formatter)
    }
}

impl FromStr for MutationProvider {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
        PROVIDER_ALIASES
            .iter()
            .find(|(alias, _)| *alias == normalized)
            .map(|(_, provider)| *provider)
            .ok_or_else(|| format!("unsupported mutation provider: {value}"))
    }
}

/// How an executable was resolved. Static discovery never executes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum DetectionSource {
    BuiltIn,
    ExplicitOverride,
    ProjectLocal,
    Path,
}

/// Report format accepted by an optional provider importer.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportFormat {
    MutationTestingElementsV1,
    MutationTestingElementsV2,
    CargoMutantsOutcomes,
    MuterJson,
}

/// Side-effect classification attached to every provider command.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommandEffect {
    ReadOnlyProbe,
    MutationRun,
}

/// A timeout- and output-bounded argv command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub output_limit_bytes: usize,
    pub effect: CommandEffect,
}

impl BoundedCommand {
    #[must_use]
    pub fn display(&self) -> String {
        let mut rendered = self.program.display().to_string();
        for argument in &self.args {
            rendered.push(' ');
            rendered.push_str(&argument.to_string_lossy());
        }
        rendered
    }
}

/// Static and optional-preflight status for one mutation provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct MutationProviderStatus {
    pub id: MutationProvider,
    pub name: String,
    pub languages: Vec<Language>,
    pub applicable: bool,
    pub available: bool,
    pub default: bool,
    pub execution_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detection: Option<DetectionSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub import_formats: Vec<ImportFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Deterministic mutation-provider inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInventory {
    pub root: PathBuf,
    pub providers: Vec<MutationProviderStatus>,
}

impl ProviderInventory {
    #[must_use]
    pub fn status(&self, provider: MutationProvider) -> Option<&MutationProviderStatus> {
        self.providers.iter().find(|status| status.id == provider)
    }
}

/// Explicit executable overrides and probe resource limits.
#[derive(Debug, Clone)]
pub struct MutationProviderOptions {
    pub executables: BTreeMap<MutationProvider, PathBuf>,
    pub probe_timeout: Duration,
    pub output_limit_bytes: usize,
}

impl Default for MutationProviderOptions {
    fn default() -> Self {
        Self {
            executables: BTreeMap::new(),
            probe_timeout: Duration::from_secs(5),
            output_limit_bytes: 256 * 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_names_and_aliases_round_trip() {
        for provider in MutationProvider::ALL {
            assert_eq!(provider.as_str().parse(), Ok(provider));
        }
        assert_eq!("reporigor".parse(), Ok(MutationProvider::BuiltIn));
        assert_eq!("cargo_mutants".parse(), Ok(MutationProvider::CargoMutants));
        assert_eq!("stryker-js".parse(), Ok(MutationProvider::Stryker));
        assert_eq!("mull-runner".parse(), Ok(MutationProvider::Mull));
        assert!(MutationProvider::from_str("unknown").is_err());
    }
}

/// One imported mutation with its provider-native identity retained.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportedMutation {
    pub external_id: String,
    pub result: MutationResult,
}

/// Provider report normalized to the reporigor mutation result model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportedMutationReport {
    pub provider: MutationProvider,
    pub format: ImportFormat,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framework_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framework_version: Option<String>,
    pub results: Vec<ImportedMutation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}
