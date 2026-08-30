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
type MetadataMap = BTreeMap<String, String>;
type MetadataDiscovery = (MetadataMap, bool);

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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
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
    pub fn as_str(self) -> &'static str {
        "bash|bats|dash|ksh|posix-sh|zsh|unknown"
            .split('|')
            .nth(self as usize)
            .unwrap_or("unknown")
    }
}

fn python_metadata(root: &Path, diagnostics: &mut Vec<Diagnostic>) -> MetadataDiscovery {
    let (mut metadata, mut has_manifest) = load_manifest_metadata(
        root,
        "pyproject.toml",
        "python",
        diagnostics,
        append_pyproject_metadata,
    );
    append_python_markers(root, &mut metadata, diagnostics, &mut has_manifest);
    (metadata, has_manifest)
}

fn append_pyproject_metadata(
    metadata: &mut MetadataMap,
    path: &Path,
    contents: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Ok(document) = contents.parse::<toml::Value>() else {
        append_invalid_pyproject(metadata, diagnostics);
        return;
    };
    append_valid_pyproject(metadata, path, &document);
}

fn append_valid_pyproject(metadata: &mut BTreeMap<String, String>, path: &Path, document: &toml::Value) {
    metadata.insert("pyproject".to_string(), path.display().to_string());
    append_toml_fields(
        metadata,
        document,
        &[
            ("project", "name", "project_name"),
            ("project", "version", "project_version"),
            ("project", "requires-python", "requires_python"),
            ("build-system", "build-backend", "build_backend"),
        ],
    );
    if !metadata.contains_key("project_name") {
        append_poetry_fields(metadata, document);
    }
}

fn append_toml_fields(
    metadata: &mut BTreeMap<String, String>,
    document: &toml::Value,
    fields: &[(&str, &str, &str)],
) {
    for &(section, key, output) in fields {
        if let Some(value) = document
            .get(section)
            .and_then(|value| value.get(key))
            .and_then(toml::Value::as_str)
        {
            metadata.insert(output.to_string(), value.to_string());
        }
    }
}

fn append_poetry_fields(metadata: &mut BTreeMap<String, String>, document: &toml::Value) {
    let Some(poetry) = document.get("tool").and_then(|value| value.get("poetry")) else {
        return;
    };
    [("name", "project_name"), ("version", "project_version")]
        .into_iter()
        .filter_map(|(key, output)| {
            poetry
                .get(key)
                .and_then(toml::Value::as_str)
                .map(|value| (output, value))
        })
        .for_each(|(output, value)| {
            metadata.insert(output.to_string(), value.to_string());
        });
}

fn append_invalid_manifest(
    metadata: &mut MetadataMap,
    diagnostics: &mut Vec<Diagnostic>,
    metadata_key: &str,
    backend: &str,
    message: String,
) {
    metadata.insert(metadata_key.to_string(), "invalid".to_string());
    diagnostics.push(metadata_diagnostic(backend, message));
}

fn append_invalid_pyproject(metadata: &mut BTreeMap<String, String>, diagnostics: &mut Vec<Diagnostic>) {
    append_invalid_manifest(
        metadata,
        diagnostics,
        "pyproject",
        "python",
        "pyproject.toml is not valid TOML; package fields were ignored".to_string(),
    );
}

fn append_python_markers(
    root: &Path,
    metadata: &mut BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
    has_manifest: &mut bool,
) {
    for (name, key) in encoded_pairs(
        "setup.cfg:setup_config|setup.py:setup_script|requirements.txt:requirements|uv.lock:uv_lock|poetry.lock:poetry_lock",
    ) {
        let marker = root.join(name);
        match resolve_optional_regular_file_within(root, &marker) {
            Ok(Some(_)) => {
                *has_manifest = true;
                metadata.insert(key.to_string(), marker.display().to_string());
            }
            Ok(None) => {}
            Err(error) => diagnostics.push(metadata_read_diagnostic("python", name, &error)),
        }
    }
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

fn typescript_metadata(root: &Path, diagnostics: &mut Vec<Diagnostic>) -> MetadataDiscovery {
    let (mut metadata, _) = load_manifest_metadata(
        root,
        "package.json",
        "typescript",
        diagnostics,
        append_package_json_metadata,
    );
    let has_config = append_typescript_config(root, &mut metadata, diagnostics);
    (metadata, has_config)
}

fn append_package_json_metadata(
    metadata: &mut MetadataMap,
    path: &Path,
    contents: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match serde_json::from_str::<serde_json::Value>(contents) {
        Ok(document) => append_valid_package_json(metadata, path, &document),
        Err(error) => append_invalid_package_json(metadata, diagnostics, &error),
    }
}

fn append_valid_package_json(
    metadata: &mut BTreeMap<String, String>,
    path: &Path,
    document: &serde_json::Value,
) {
    metadata.insert("package_json".to_string(), path.display().to_string());
    for (key, output) in
        encoded_pairs("name:package_name|version:package_version|packageManager:package_manager")
    {
        if let Some(value) = document.get(key).and_then(serde_json::Value::as_str) {
            metadata.insert(output.to_string(), value.to_string());
        }
    }
    append_declared_typescript(metadata, document);
}

fn append_declared_typescript(metadata: &mut BTreeMap<String, String>, document: &serde_json::Value) {
    for section in ["devDependencies", "dependencies", "peerDependencies"] {
        let declared = document
            .get(section)
            .and_then(|value| value.get("typescript"))
            .and_then(serde_json::Value::as_str);
        if let Some(value) = declared {
            metadata.insert("declared_typescript".to_string(), value.to_string());
            break;
        }
    }
}

fn append_invalid_package_json(
    metadata: &mut BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
    error: &serde_json::Error,
) {
    append_invalid_manifest(
        metadata,
        diagnostics,
        "package_json",
        "typescript",
        format!("package.json is not valid JSON; package fields were ignored: {error}"),
    );
}

fn encoded_pairs(specification: &str) -> impl Iterator<Item = (&str, &str)> {
    specification.split('|').map(|pair| {
        pair.split_once(':')
            .unwrap_or_else(|| panic!("invalid internal metadata mapping: {pair}"))
    })
}

fn append_typescript_config(
    root: &Path,
    metadata: &mut BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
) -> bool {
    let config = root.join("tsconfig.json");
    match read_metadata(root, &config, "typescript", diagnostics) {
        Some(_) => {
            metadata.insert("tsconfig".to_string(), config.display().to_string());
            true
        }
        None => false,
    }
}

fn swift_metadata(root: &Path, diagnostics: &mut Vec<Diagnostic>) -> MetadataDiscovery {
    let (mut metadata, path, contents) = metadata_document(root, "Package.swift", "swiftpm", diagnostics);
    let Some(contents) = contents else {
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

fn metadata_document(
    root: &Path,
    name: &str,
    provider: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> (BTreeMap<String, String>, std::path::PathBuf, Option<String>) {
    let path = root.join(name);
    let contents = read_metadata(root, &path, provider, diagnostics);
    (BTreeMap::new(), path, contents)
}

fn load_manifest_metadata(
    root: &Path,
    name: &str,
    provider: &str,
    diagnostics: &mut Vec<Diagnostic>,
    append: impl FnOnce(&mut BTreeMap<String, String>, &Path, &str, &mut Vec<Diagnostic>),
) -> (BTreeMap<String, String>, bool) {
    let (mut metadata, path, contents) = metadata_document(root, name, provider, diagnostics);
    let found = contents.is_some();
    if let Some(contents) = contents {
        append(&mut metadata, &path, &contents, diagnostics);
    }
    (metadata, found)
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
    shebang_interpreter(shebang)
        .as_deref()
        .and_then(known_shell_dialect)
        .unwrap_or(ShellDialect::Unknown)
}

fn known_shell_dialect(interpreter: &str) -> Option<ShellDialect> {
    [
        ("bash", ShellDialect::Bash),
        ("bats", ShellDialect::Bats),
        ("dash", ShellDialect::Dash),
        ("ksh", ShellDialect::Ksh),
        ("ksh88", ShellDialect::Ksh),
        ("ksh93", ShellDialect::Ksh),
        ("sh", ShellDialect::PosixSh),
        ("zsh", ShellDialect::Zsh),
    ]
    .into_iter()
    .find_map(|(name, dialect)| (name == interpreter).then_some(dialect))
}

fn shebang_interpreter(shebang: &str) -> Option<String> {
    let command = shebang.trim().strip_prefix("#!")?.trim();
    let mut words = command.split_whitespace();
    let first = words.next()?;
    let first_name = executable_name(first);
    if !first_name.eq_ignore_ascii_case("env") {
        return Some(first_name.to_ascii_lowercase());
    }
    env_interpreter(words)
}

fn env_interpreter<'a>(words: impl Iterator<Item = &'a str>) -> Option<String> {
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
