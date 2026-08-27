use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use reporigor_core::{
    read_optional_bounded_utf8_file_within, resolve_optional_regular_file_within, CoreError, Diagnostic,
    Severity, SourceFile, PROJECT_METADATA_MAX_BYTES,
};
use serde::{Deserialize, Serialize};

const SHEBANG_MAX_BYTES: u64 = 4 * 1024;

#[derive(Debug, Default)]
pub(crate) struct ProjectMetadata {
    pub(crate) python: BTreeMap<String, String>,
    pub(crate) swift: BTreeMap<String, String>,
    pub(crate) typescript: BTreeMap<String, String>,
    pub(crate) has_python_manifest: bool,
    pub(crate) has_swift_manifest: bool,
    pub(crate) has_typescript_config: bool,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

impl ProjectMetadata {
    pub(crate) fn discover(root: &Path) -> Self {
        let mut diagnostics = Vec::new();
        let (python, has_python_manifest) = python_metadata(root, &mut diagnostics);
        let (typescript, has_typescript_config) = typescript_metadata(root, &mut diagnostics);
        let (swift, has_swift_manifest) = swift_metadata(root, &mut diagnostics);
        Self {
            python,
            swift,
            typescript,
            has_python_manifest,
            has_swift_manifest,
            has_typescript_config,
            diagnostics,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShellDialect {
    Bash,
    Bats,
    Dash,
    Ksh,
    PosixSh,
    Zsh,
    Unknown,
}

impl ShellDialect {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Bats => "bats",
            Self::Dash => "dash",
            Self::Ksh => "ksh",
            Self::PosixSh => "posix-sh",
            Self::Zsh => "zsh",
            Self::Unknown => "unknown",
        }
    }
}

fn python_metadata(root: &Path, diagnostics: &mut Vec<Diagnostic>) -> (BTreeMap<String, String>, bool) {
    let mut metadata = BTreeMap::new();
    let path = root.join("pyproject.toml");
    let contents = read_metadata(root, &path, "python", diagnostics);
    if let Some(contents) = contents.as_deref() {
        if let Ok(document) = contents.parse::<toml::Value>() {
            metadata.insert("pyproject".to_string(), path.display().to_string());
            for (section, key, output) in [
                ("project", "name", "project_name"),
                ("project", "version", "project_version"),
                ("project", "requires-python", "requires_python"),
                ("build-system", "build-backend", "build_backend"),
            ] {
                if let Some(value) = document
                    .get(section)
                    .and_then(|value| value.get(key))
                    .and_then(toml::Value::as_str)
                {
                    metadata.insert(output.to_string(), value.to_string());
                }
            }
            if !metadata.contains_key("project_name") {
                for (key, output) in [("name", "project_name"), ("version", "project_version")] {
                    if let Some(value) = document
                        .get("tool")
                        .and_then(|value| value.get("poetry"))
                        .and_then(|value| value.get(key))
                        .and_then(toml::Value::as_str)
                    {
                        metadata.insert(output.to_string(), value.to_string());
                    }
                }
            }
        } else {
            metadata.insert("pyproject".to_string(), "invalid".to_string());
            diagnostics.push(metadata_diagnostic(
                "python",
                "pyproject.toml is not valid TOML; package fields were ignored".to_string(),
            ));
        }
    }
    let mut has_manifest = contents.is_some();
    for (name, key) in [
        ("setup.cfg", "setup_config"),
        ("setup.py", "setup_script"),
        ("requirements.txt", "requirements"),
        ("uv.lock", "uv_lock"),
        ("poetry.lock", "poetry_lock"),
    ] {
        let marker = root.join(name);
        match resolve_optional_regular_file_within(root, &marker) {
            Ok(Some(_)) => {
                has_manifest = true;
                metadata.insert(key.to_string(), marker.display().to_string());
            }
            Ok(None) => {}
            Err(error) => diagnostics.push(metadata_read_diagnostic("python", name, &error)),
        }
    }
    (metadata, has_manifest)
}

/// Classify the shell dialects declared by discovered Bash source files.
///
/// This reads only the first line of each source and never invokes a shell.
#[must_use]
pub fn discover_bash_dialects(sources: &[SourceFile]) -> BTreeMap<ShellDialect, usize> {
    let mut result = BTreeMap::new();
    for source in sources {
        if source.language != reporigor_core::Language::Bash {
            continue;
        }
        let dialect = if source
            .path
            .extension()
            .is_some_and(|extension| extension == "bats")
        {
            ShellDialect::Bats
        } else {
            read_shebang(&source.path)
                .as_deref()
                .map_or(ShellDialect::Unknown, dialect_from_shebang)
        };
        *result.entry(dialect).or_default() += 1;
    }
    result
}

fn typescript_metadata(root: &Path, diagnostics: &mut Vec<Diagnostic>) -> (BTreeMap<String, String>, bool) {
    let mut metadata = BTreeMap::new();
    let path = root.join("package.json");
    if let Some(contents) = read_metadata(root, &path, "typescript", diagnostics) {
        match serde_json::from_str::<serde_json::Value>(&contents) {
            Ok(document) => {
                metadata.insert("package_json".to_string(), path.display().to_string());
                for (key, output) in [
                    ("name", "package_name"),
                    ("version", "package_version"),
                    ("packageManager", "package_manager"),
                ] {
                    if let Some(value) = document.get(key).and_then(serde_json::Value::as_str) {
                        metadata.insert(output.to_string(), value.to_string());
                    }
                }
                for section in ["devDependencies", "dependencies", "peerDependencies"] {
                    if let Some(value) = document
                        .get(section)
                        .and_then(|value| value.get("typescript"))
                        .and_then(serde_json::Value::as_str)
                    {
                        metadata.insert("declared_typescript".to_string(), value.to_string());
                        break;
                    }
                }
            }
            Err(error) => {
                metadata.insert("package_json".to_string(), "invalid".to_string());
                diagnostics.push(metadata_diagnostic(
                    "typescript",
                    format!("package.json is not valid JSON; package fields were ignored: {error}"),
                ));
            }
        }
    }

    let config = root.join("tsconfig.json");
    let has_config = match read_metadata(root, &config, "typescript", diagnostics) {
        Some(_) => {
            metadata.insert("tsconfig".to_string(), config.display().to_string());
            true
        }
        None => false,
    };
    (metadata, has_config)
}

fn swift_metadata(root: &Path, diagnostics: &mut Vec<Diagnostic>) -> (BTreeMap<String, String>, bool) {
    let mut metadata = BTreeMap::new();
    let path = root.join("Package.swift");
    let Some(contents) = read_metadata(root, &path, "swiftpm", diagnostics) else {
        return (metadata, false);
    };
    metadata.insert("manifest".to_string(), path.display().to_string());
    if let Some(version) = contents
        .lines()
        .next()
        .and_then(|line| line.trim().strip_prefix("// swift-tools-version:"))
    {
        metadata.insert("swift_tools_version".to_string(), version.trim().to_string());
    }
    (metadata, true)
}

fn read_metadata(
    root: &Path,
    path: &Path,
    provider: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<String> {
    match read_optional_bounded_utf8_file_within(root, path, PROJECT_METADATA_MAX_BYTES) {
        Ok(contents) => contents,
        Err(error) => {
            let name = path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            diagnostics.push(metadata_read_diagnostic(provider, &name, &error));
            None
        }
    }
}

fn metadata_read_diagnostic(provider: &str, name: &str, error: &CoreError) -> Diagnostic {
    metadata_diagnostic(
        provider,
        format!("ignored unsafe or unreadable repository metadata {name}: {error}"),
    )
}

fn metadata_diagnostic(provider: &str, message: String) -> Diagnostic {
    Diagnostic {
        severity: Severity::Warning,
        backend: format!("project-{provider}-discovery"),
        message,
        location: None,
        fallback_used: false,
    }
}

pub(crate) fn dialect_names(dialects: &BTreeMap<ShellDialect, usize>) -> String {
    dialects
        .keys()
        .map(|dialect| dialect.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) fn dialect_from_shebang(shebang: &str) -> ShellDialect {
    match shebang_interpreter(shebang).as_deref() {
        Some("bash") => ShellDialect::Bash,
        Some("bats") => ShellDialect::Bats,
        Some("dash") => ShellDialect::Dash,
        Some("ksh" | "ksh88" | "ksh93") => ShellDialect::Ksh,
        Some("sh") => ShellDialect::PosixSh,
        Some("zsh") => ShellDialect::Zsh,
        _ => ShellDialect::Unknown,
    }
}

fn shebang_interpreter(shebang: &str) -> Option<String> {
    let command = shebang.trim().strip_prefix("#!")?.trim();
    let mut words = command.split_whitespace();
    let first = words.next()?;
    let first_name = executable_name(first);
    if !first_name.eq_ignore_ascii_case("env") {
        return Some(first_name.to_ascii_lowercase());
    }
    for word in words {
        if word.starts_with('-') || word.contains('=') {
            continue;
        }
        return Some(executable_name(word).to_ascii_lowercase());
    }
    None
}

fn executable_name(value: &str) -> &str {
    value.rsplit(['/', '\\']).next().unwrap_or(value)
}

fn read_shebang(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut prefix = Vec::new();
    file.take(SHEBANG_MAX_BYTES).read_to_end(&mut prefix).ok()?;
    let first_line = prefix.split(|byte| *byte == b'\n').next()?;
    let line = std::str::from_utf8(first_line).ok()?;
    line.starts_with("#!").then(|| line.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_shell_shebangs() {
        assert_eq!(dialect_from_shebang("#!/usr/bin/env bash"), ShellDialect::Bash);
        assert_eq!(
            dialect_from_shebang("#!/usr/bin/env -S bash -eu"),
            ShellDialect::Bash
        );
        assert_eq!(dialect_from_shebang("#!/bin/sh"), ShellDialect::PosixSh);
        assert_eq!(dialect_from_shebang("#!/bin/dash"), ShellDialect::Dash);
        assert_eq!(dialect_from_shebang("#!/bin/ksh"), ShellDialect::Ksh);
        assert_eq!(dialect_from_shebang("#!/bin/zsh"), ShellDialect::Zsh);
        assert_eq!(
            dialect_from_shebang("#!/usr/bin/env notbash"),
            ShellDialect::Unknown
        );
        assert_eq!(dialect_from_shebang("not a shebang"), ShellDialect::Unknown);
    }
}
