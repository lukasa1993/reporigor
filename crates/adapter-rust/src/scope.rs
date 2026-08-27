use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use proc_macro2::TokenStream;
use serde::Deserialize;
use syn::parse::{ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::{Attribute, Expr, Lit, LitStr, Meta, Token};
use walkdir::{DirEntry, WalkDir};

use crate::cargo_proxy::resolve_program;
use crate::command::{render_stream, run_bounded, CommandLimits};
use crate::CargoOptions;

const CARGO_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const METADATA_STDOUT_LIMIT: usize = 64 * 1024 * 1024;
const CFG_STDOUT_LIMIT: usize = 2 * 1024 * 1024;
const CARGO_STDERR_LIMIT: usize = 1024 * 1024;
const CARGO_SAFETY_NOTE: &str =
    "reporigor runs Cargo with --locked --offline to prevent network access and lockfile changes; ensure Cargo.lock exists and dependencies are cached";

const fn metadata_limits() -> CommandLimits {
    CommandLimits {
        timeout: CARGO_COMMAND_TIMEOUT,
        stdout_bytes: METADATA_STDOUT_LIMIT,
        stderr_bytes: CARGO_STDERR_LIMIT,
    }
}

const fn cfg_limits() -> CommandLimits {
    CommandLimits {
        timeout: CARGO_COMMAND_TIMEOUT,
        stdout_bytes: CFG_STDOUT_LIMIT,
        stderr_bytes: CARGO_STDERR_LIMIT,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SingleCfgContext {
    names: HashSet<String>,
    values: HashMap<String, HashSet<String>>,
    features: HashSet<String>,
    include_tests: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CfgContext {
    variants: Vec<SingleCfgContext>,
}

impl CfgContext {
    pub(crate) fn synthetic(include_tests: bool) -> Self {
        Self {
            variants: vec![SingleCfgContext::synthetic(include_tests)],
        }
    }

    pub(crate) fn attrs_active(&self, attrs: &[Attribute]) -> bool {
        self.variants.iter().any(|context| context.attrs_active(attrs))
    }

    pub(crate) fn merged<'a>(contexts: impl IntoIterator<Item = &'a Self>) -> Self {
        let mut variants = Vec::new();
        for context in contexts {
            for variant in &context.variants {
                if !variants.contains(variant) {
                    variants.push(variant.clone());
                }
            }
        }
        Self { variants }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ScopedFile {
    pub path: PathBuf,
    pub module_prefix: String,
    pub cfg: CfgContext,
}

#[derive(Debug)]
pub(crate) enum BoundedSource {
    Content(String),
    TooLarge { actual_bytes: u64 },
}

#[derive(Clone)]
struct TargetScopedFile {
    path: PathBuf,
    module_prefix: String,
    cfg: SingleCfgContext,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
    resolve: Option<Resolve>,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<ResolveNode>,
}

#[derive(Deserialize)]
struct ResolveNode {
    id: String,
    features: Vec<String>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    manifest_path: PathBuf,
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct Target {
    name: String,
    kind: Vec<String>,
    src_path: PathBuf,
}

struct ResolvedModule {
    path: PathBuf,
    descendant_dir: PathBuf,
}

fn parse_meta_list(tokens: TokenStream) -> Option<Vec<Meta>> {
    Punctuated::<Meta, Token![,]>::parse_terminated
        .parse2(tokens)
        .ok()
        .map(|items| items.into_iter().collect())
}

fn literal_string(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Lit(value) => match &value.lit {
            Lit::Str(value) => Some(value.value()),
            _ => None,
        },
        _ => None,
    }
}

impl SingleCfgContext {
    fn synthetic(include_tests: bool) -> Self {
        Self {
            names: HashSet::new(),
            values: HashMap::new(),
            features: HashSet::new(),
            include_tests,
        }
    }

    fn eval(&self, meta: &Meta) -> bool {
        match meta {
            Meta::Path(path) => {
                if path.is_ident("test") {
                    return self.include_tests;
                }
                path.get_ident()
                    .is_some_and(|name| self.names.contains(&name.to_string()))
            }
            Meta::NameValue(value) => {
                let Some(key) = value.path.get_ident().map(ToString::to_string) else {
                    return false;
                };
                let Some(value) = literal_string(&value.value) else {
                    return false;
                };
                if key == "feature" {
                    self.features.contains(&value)
                } else {
                    self.values
                        .get(&key)
                        .is_some_and(|values| values.contains(&value))
                }
            }
            Meta::List(list) if list.path.is_ident("all") => parse_meta_list(list.tokens.clone())
                .is_some_and(|items| items.iter().all(|item| self.eval(item))),
            Meta::List(list) if list.path.is_ident("any") => parse_meta_list(list.tokens.clone())
                .is_some_and(|items| items.iter().any(|item| self.eval(item))),
            Meta::List(list) if list.path.is_ident("not") => parse_meta_list(list.tokens.clone())
                .is_some_and(|items| items.len() == 1 && !self.eval(&items[0])),
            Meta::List(_) => false,
        }
    }

    fn meta_attribute_active(&self, meta: &Meta) -> bool {
        match meta {
            Meta::Path(path) if path.is_ident("test") => self.include_tests,
            Meta::List(list) if list.path.is_ident("cfg") => syn::parse2::<Meta>(list.tokens.clone())
                .ok()
                .is_none_or(|predicate| self.eval(&predicate)),
            Meta::List(list) if list.path.is_ident("cfg_attr") => {
                let Some(items) = parse_meta_list(list.tokens.clone()) else {
                    return true;
                };
                let Some((predicate, nested)) = items.split_first() else {
                    return true;
                };
                !self.eval(predicate)
                    || nested
                        .iter()
                        .all(|attribute| self.meta_attribute_active(attribute))
            }
            _ => true,
        }
    }

    fn attrs_active(&self, attrs: &[Attribute]) -> bool {
        attrs
            .iter()
            .all(|attribute| self.meta_attribute_active(&attribute.meta))
    }

    fn path_override(&self, attrs: &[Attribute]) -> Option<PathBuf> {
        fn from_meta(context: &SingleCfgContext, meta: &Meta) -> Option<PathBuf> {
            match meta {
                Meta::NameValue(value) if value.path.is_ident("path") => {
                    literal_string(&value.value).map(PathBuf::from)
                }
                Meta::List(list) if list.path.is_ident("cfg_attr") => {
                    let items = parse_meta_list(list.tokens.clone())?;
                    let (predicate, nested) = items.split_first()?;
                    if context.eval(predicate) {
                        nested.iter().find_map(|meta| from_meta(context, meta))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }

        attrs
            .iter()
            .find_map(|attribute| from_meta(self, &attribute.meta))
    }
}

fn parse_cfg_output(text: &str, include_tests: bool) -> SingleCfgContext {
    let mut context = SingleCfgContext::synthetic(include_tests);
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim_matches('"').to_string();
            if key == "feature" {
                context.features.insert(value.clone());
            }
            context.values.entry(key.to_string()).or_default().insert(value);
        } else {
            context.names.insert(line.to_string());
        }
    }
    context
}

fn rustc_program(options: &CargoOptions) -> Result<OsString, String> {
    if let Some(configured) = std::env::var_os("RUSTC") {
        return resolve_program(&configured)
            .map(PathBuf::into_os_string)
            .map_err(|error| format!("cannot resolve configured RUSTC: {error}"));
    }
    let cargo = resolve_program(options.cargo_program())
        .map_err(|error| format!("cannot resolve Cargo executable for rustc discovery: {error}"))?;
    let executable = if cfg!(windows) { "rustc.exe" } else { "rustc" };
    if let Some(candidate) = cargo.parent().map(|parent| parent.join(executable)) {
        if candidate.is_file() {
            return resolve_program(candidate.as_os_str())
                .map(PathBuf::into_os_string)
                .map_err(|error| format!("cannot resolve rustc beside Cargo: {error}"));
        }
    }
    resolve_program(OsStr::new("rustc"))
        .map(PathBuf::into_os_string)
        .map_err(|error| format!("cannot resolve rustc on absolute PATH entries: {error}"))
}

fn rustc_cfg(
    root: &Path,
    package: &Package,
    target: &Target,
    include_tests: bool,
    options: &CargoOptions,
    features: &[String],
) -> Result<SingleCfgContext, String> {
    let program = rustc_program(options)?;
    rustc_cfg_with_limits(
        root,
        package,
        target,
        include_tests,
        features,
        &program,
        cfg_limits(),
    )
}

fn rustc_cfg_with_limits(
    root: &Path,
    package: &Package,
    target: &Target,
    include_tests: bool,
    features: &[String],
    program: &OsStr,
    limits: CommandLimits,
) -> Result<SingleCfgContext, String> {
    let mut command = Command::new(program);
    command.args(["--print", "cfg"]);
    if let Some(cargo_target) = std::env::var_os("CARGO_BUILD_TARGET") {
        command.arg("--target").arg(cargo_target);
    }
    command
        .current_dir(root)
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    let action = format!(
        "rustc cfg discovery for {} target {}",
        package.manifest_path.display(),
        target.name
    );
    let output = run_bounded(&mut command, &action, limits)?;
    if !output.status.success() {
        return Err(format!(
            "rustc cfg discovery failed for {} target {} with exit code {:?}: {}",
            package.manifest_path.display(),
            target.name,
            output.status.code(),
            render_stream(&output.stderr, limits.stderr_bytes).trim()
        ));
    }
    if output.stdout.truncated {
        return Err(format!(
            "rustc cfg discovery for {} target {} exceeded the {}-byte stdout limit",
            package.manifest_path.display(),
            target.name,
            limits.stdout_bytes
        ));
    }
    let text = String::from_utf8(output.stdout.bytes)
        .map_err(|error| format!("rustc cfg output is invalid UTF-8: {error}"))?;
    let mut context = parse_cfg_output(&text, include_tests);
    context.features.extend(features.iter().cloned());
    Ok(context)
}

fn cargo_metadata(root: &Path, options: &CargoOptions) -> Result<Metadata, String> {
    cargo_metadata_with_limits(root, options, metadata_limits())
}

fn cargo_metadata_with_limits(
    root: &Path,
    options: &CargoOptions,
    limits: CommandLimits,
) -> Result<Metadata, String> {
    let cargo = resolve_program(options.cargo_program())
        .map_err(|error| format!("cannot resolve Cargo executable on absolute PATH entries: {error}"))?;
    let mut command = Command::new(&cargo);
    command
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .args(options.feature_args())
        .current_dir(root)
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TERM_COLOR", "never");
    let output = run_bounded(&mut command, "Cargo metadata discovery", limits)?;
    if !output.status.success() {
        let stderr = render_stream(&output.stderr, limits.stderr_bytes);
        let detail = if stderr.trim().is_empty() {
            render_stream(&output.stdout, limits.stdout_bytes)
        } else {
            stderr
        };
        return Err(format!(
            "Cargo metadata discovery failed with exit code {:?}: {}; {CARGO_SAFETY_NOTE}",
            output.status.code(),
            detail.trim()
        ));
    }
    if output.stdout.truncated {
        return Err(format!(
            "Cargo metadata discovery exceeded the {}-byte stdout limit; narrow the selected workspace",
            limits.stdout_bytes
        ));
    }
    serde_json::from_slice(&output.stdout.bytes)
        .map_err(|error| format!("cannot decode Cargo metadata JSON: {error}"))
}

fn target_in_scope(kind: &[String], include_tests: bool) -> bool {
    if kind
        .iter()
        .any(|value| value == "custom-build" || value == "example" || value == "bench")
    {
        return false;
    }
    include_tests || !kind.iter().any(|value| value == "test")
}

fn prefix(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_string()
    } else {
        format!("{parent}::{child}")
    }
}

fn root_module_dir(path: &Path) -> PathBuf {
    path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf()
}

fn canonical_within_root(root: &Path, path: &Path, description: &str) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("cannot resolve {description} {}: {error}", path.display()))?;
    if !canonical.starts_with(root) {
        return Err(format!(
            "refusing {description} outside project root: {} resolves to {}, but the project root is {}",
            path.display(),
            canonical.display(),
            root.display()
        ));
    }
    Ok(canonical)
}

pub(crate) fn read_source_bounded(path: &Path, max_source_bytes: usize) -> Result<BoundedSource, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("cannot inspect Rust source {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("Rust source is not a regular file: {}", path.display()));
    }
    let limit = u64::try_from(max_source_bytes).unwrap_or(u64::MAX);
    if metadata.len() > limit {
        return Ok(BoundedSource::TooLarge {
            actual_bytes: metadata.len(),
        });
    }
    let initial_capacity = usize::try_from(metadata.len())
        .unwrap_or(max_source_bytes)
        .min(max_source_bytes);
    let file = fs::File::open(path)
        .map_err(|error| format!("cannot read Rust source {}: {error}", path.display()))?;
    let mut bytes = Vec::with_capacity(initial_capacity);
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read Rust source {}: {error}", path.display()))?;
    if bytes.len() > max_source_bytes {
        return Ok(BoundedSource::TooLarge {
            actual_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        });
    }
    String::from_utf8(bytes)
        .map(BoundedSource::Content)
        .map_err(|error| format!("Rust source {} is invalid UTF-8: {error}", path.display()))
}

fn resolve_module(
    module: &syn::ItemMod,
    module_dir: &Path,
    context: &SingleCfgContext,
) -> Result<ResolvedModule, String> {
    if let Some(relative) = context.path_override(&module.attrs) {
        let path = module_dir.join(relative);
        if !path.is_file() {
            return Err(format!(
                "active #[path] module {} does not exist: {}",
                module.ident,
                path.display()
            ));
        }
        let descendant_dir = path.parent().unwrap_or(module_dir).to_path_buf();
        return Ok(ResolvedModule { path, descendant_dir });
    }
    let direct = module_dir.join(format!("{}.rs", module.ident));
    let nested = module_dir.join(module.ident.to_string()).join("mod.rs");
    match (direct.is_file(), nested.is_file()) {
        (true, false) => Ok(ResolvedModule {
            path: direct,
            descendant_dir: module_dir.join(module.ident.to_string()),
        }),
        (false, true) => Ok(ResolvedModule {
            path: nested,
            descendant_dir: module_dir.join(module.ident.to_string()),
        }),
        (true, true) => Err(format!(
            "module {} is ambiguous: both {} and {} exist",
            module.ident,
            direct.display(),
            nested.display()
        )),
        (false, false) => Err(format!(
            "active module {} cannot be resolved below {}",
            module.ident,
            module_dir.display()
        )),
    }
}

fn include_literal(tokens: TokenStream) -> Option<LitStr> {
    let parser = |input: ParseStream<'_>| {
        let literal: LitStr = input.parse()?;
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
        if !input.is_empty() {
            return Err(input.error("include! expects one string literal"));
        }
        Ok(literal)
    };
    parser.parse2(tokens).ok()
}

fn static_include_path(item: &syn::ItemMacro, source_dir: &Path) -> Option<PathBuf> {
    if !item.mac.path.is_ident("include") {
        return None;
    }
    let literal = include_literal(item.mac.tokens.clone())?;
    let path = PathBuf::from(literal.value());
    if path.extension().and_then(|value| value.to_str()) != Some("rs") {
        return None;
    }
    Some(if path.is_absolute() {
        path
    } else {
        source_dir.join(path)
    })
}

fn item_attrs(item: &syn::Item) -> &[Attribute] {
    crate::syntax::item_attrs(item)
}

struct WalkState<'a> {
    root: &'a Path,
    max_source_bytes: usize,
    allow_parse_errors: bool,
    context: &'a SingleCfgContext,
    visited: &'a mut HashSet<(PathBuf, String)>,
    output: &'a mut Vec<TargetScopedFile>,
}

fn walk_items(
    state: &mut WalkState<'_>,
    items: &[syn::Item],
    module_dir: &Path,
    source_dir: &Path,
    module_prefix: &str,
) -> Result<(), String> {
    for item in items {
        if !state.context.attrs_active(item_attrs(item)) {
            continue;
        }
        match item {
            syn::Item::Mod(module) => {
                let next_prefix = prefix(module_prefix, &module.ident.to_string());
                if let Some((_, nested)) = &module.content {
                    let nested_dir = module_dir.join(module.ident.to_string());
                    walk_items(state, nested, &nested_dir, source_dir, &next_prefix)?;
                } else {
                    let resolved = resolve_module(module, module_dir, state.context)?;
                    visit_file(state, &resolved.path, &resolved.descendant_dir, &next_prefix)?;
                }
            }
            syn::Item::Macro(item_macro) => {
                if let Some(path) = static_include_path(item_macro, source_dir) {
                    if path.is_file() {
                        let include_dir = path.parent().unwrap_or(source_dir).to_path_buf();
                        visit_file(state, &path, &include_dir, module_prefix)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn visit_file(
    state: &mut WalkState<'_>,
    path: &Path,
    module_dir: &Path,
    module_prefix: &str,
) -> Result<(), String> {
    let canonical = canonical_within_root(state.root, path, "Rust source")?;
    let key = (canonical.clone(), module_prefix.to_string());
    if !state.visited.insert(key) {
        return Ok(());
    }
    let source = match read_source_bounded(&canonical, state.max_source_bytes)? {
        BoundedSource::Content(source) => source,
        BoundedSource::TooLarge { .. } => {
            state.output.push(TargetScopedFile {
                path: canonical,
                module_prefix: module_prefix.to_string(),
                cfg: state.context.clone(),
            });
            return Ok(());
        }
    };
    let syntax = match syn::parse_file(&source) {
        Ok(syntax) => syntax,
        Err(_) if state.allow_parse_errors => {
            state.output.push(TargetScopedFile {
                path: canonical,
                module_prefix: module_prefix.to_string(),
                cfg: state.context.clone(),
            });
            return Ok(());
        }
        Err(error) => {
            return Err(format!("Rust parse error in {}: {error}", canonical.display()));
        }
    };
    if !state.context.attrs_active(&syntax.attrs) {
        return Ok(());
    }
    state.output.push(TargetScopedFile {
        path: canonical.clone(),
        module_prefix: module_prefix.to_string(),
        cfg: state.context.clone(),
    });
    let source_dir = canonical.parent().unwrap_or(module_dir);
    walk_items(state, &syntax.items, module_dir, source_dir, module_prefix)
}

fn ignored(entry: &DirEntry) -> bool {
    matches!(
        entry.file_name().to_str(),
        Some(".git" | "target" | "vendor" | "node_modules" | ".venv" | "venv" | "build" | "dist")
    )
}

fn fallback_files(root: &Path, include_tests: bool) -> Vec<ScopedFile> {
    let context = CfgContext::synthetic(include_tests);
    let mut files: Vec<_> = WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !ignored(entry))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("rs"))
        .filter(|path| {
            let relative = path.strip_prefix(root).unwrap_or(path);
            !relative
                .components()
                .any(|part| matches!(part.as_os_str().to_str(), Some("examples" | "benches" | "fuzz")))
        })
        .filter(|path| path.file_name().and_then(|value| value.to_str()) != Some("build.rs"))
        .filter(|path| {
            include_tests
                || !path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|name| name.ends_with("_test.rs"))
        })
        .filter(|path| {
            include_tests
                || !path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .components()
                    .any(|part| part.as_os_str() == "tests")
        })
        .map(|path| ScopedFile {
            path,
            module_prefix: String::new(),
            cfg: context.clone(),
        })
        .collect();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    files
}

fn merge_context(merged: &mut HashMap<(PathBuf, String), Vec<SingleCfgContext>>, file: TargetScopedFile) {
    let key = (file.path, file.module_prefix);
    let contexts = merged.entry(key).or_default();
    if !contexts.contains(&file.cfg) {
        contexts.push(file.cfg);
    }
}

pub(crate) fn discover(
    root: &Path,
    include_tests: bool,
    filters: &[String],
    options: &CargoOptions,
    max_source_bytes: usize,
    allow_parse_errors: bool,
) -> Result<Vec<ScopedFile>, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve Rust project root {}: {error}", root.display()))?;
    let root = root.as_path();
    if !root.join("Cargo.toml").is_file() {
        return Ok(fallback_files(root, include_tests));
    }
    let metadata = cargo_metadata(root, options)?;
    let resolved_features: HashMap<_, _> = metadata
        .resolve
        .as_ref()
        .map(|resolve| {
            resolve
                .nodes
                .iter()
                .map(|node| (node.id.as_str(), node.features.as_slice()))
                .collect()
        })
        .unwrap_or_default();
    let workspace: HashSet<_> = metadata.workspace_members.into_iter().collect();
    let mut merged = HashMap::<(PathBuf, String), Vec<SingleCfgContext>>::new();
    for package in metadata
        .packages
        .into_iter()
        .filter(|package| workspace.contains(&package.id))
    {
        canonical_within_root(root, &package.manifest_path, "Cargo workspace manifest")?;
        for target in package
            .targets
            .iter()
            .filter(|target| target_in_scope(&target.kind, include_tests))
        {
            canonical_within_root(root, &target.src_path, "Cargo target source")?;
            let features = resolved_features
                .get(package.id.as_str())
                .copied()
                .unwrap_or_default();
            let context = rustc_cfg(root, &package, target, include_tests, options, features)?;
            let mut visited = HashSet::new();
            let mut target_files = Vec::new();
            let module_dir = root_module_dir(&target.src_path);
            let mut state = WalkState {
                root,
                max_source_bytes,
                allow_parse_errors,
                context: &context,
                visited: &mut visited,
                output: &mut target_files,
            };
            visit_file(&mut state, &target.src_path, &module_dir, "")?;
            for file in target_files {
                merge_context(&mut merged, file);
            }
        }
    }
    let mut output: Vec<_> = merged
        .into_iter()
        .map(|((path, module_prefix), variants)| ScopedFile {
            path,
            module_prefix,
            cfg: CfgContext { variants },
        })
        .filter(|file| {
            if filters.is_empty() {
                return true;
            }
            let relative = file
                .path
                .strip_prefix(root)
                .unwrap_or(&file.path)
                .to_string_lossy();
            filters.iter().any(|filter| relative.contains(filter))
        })
        .collect();
    output.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.module_prefix.cmp(&right.module_prefix))
    });
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::thread;

    use tempfile::tempdir;

    use super::*;

    const TEST_SOURCE_LIMIT: usize = 8 * 1024 * 1024;

    fn options() -> CargoOptions {
        CargoOptions::default()
    }

    fn write_lock(root: &Path, package: &str) {
        fs::write(
            root.join("Cargo.lock"),
            format!(
                "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 3\n\n[[package]]\nname = \"{package}\"\nversion = \"0.1.0\"\n"
            ),
        )
        .unwrap_or_else(|error| panic!("lockfile: {error}"));
    }

    fn write_basic_project(root: &Path, package: &str, source: &str) {
        fs::create_dir_all(root.join("src"))
            .unwrap_or_else(|error| panic!("create source directory: {error}"));
        fs::write(
            root.join("Cargo.toml"),
            format!("[package]\nname='{package}'\nversion='0.1.0'\nedition='2021'\n"),
        )
        .unwrap_or_else(|error| panic!("manifest: {error}"));
        write_lock(root, package);
        fs::write(root.join("src/lib.rs"), source).unwrap_or_else(|error| panic!("root source: {error}"));
    }

    #[cfg(unix)]
    fn fake_program(root: &Path, name: &str, source: &str) -> PathBuf {
        let program = root.join(name);
        fs::write(&program, source).unwrap_or_else(|error| panic!("fake program: {error}"));
        let mut permissions = fs::metadata(&program)
            .unwrap_or_else(|error| panic!("fake program metadata: {error}"))
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&program, permissions)
            .unwrap_or_else(|error| panic!("fake program permissions: {error}"));
        program
    }

    #[test]
    fn merged_context_accepts_either_target_cfg() {
        let mut unix = SingleCfgContext::synthetic(false);
        unix.names.insert("unix".into());
        let mut windows = SingleCfgContext::synthetic(false);
        windows.names.insert("windows".into());
        let context = CfgContext {
            variants: vec![unix, windows],
        };
        let unix_attr: Attribute = syn::parse_quote!(#[cfg(unix)]);
        let windows_attr: Attribute = syn::parse_quote!(#[cfg(windows)]);
        assert!(context.attrs_active(&[unix_attr]));
        assert!(context.attrs_active(&[windows_attr]));
    }

    #[test]
    fn path_override_and_descendant_are_discovered() {
        let dir = tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        fs::create_dir_all(dir.path().join("src")).unwrap_or_else(|error| panic!("create source: {error}"));
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname='rust-adapter-path-fixture'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap_or_else(|error| panic!("manifest: {error}"));
        write_lock(dir.path(), "rust-adapter-path-fixture");
        fs::write(dir.path().join("src/lib.rs"), "#[path=\"foo.rs\"] mod bar;\n")
            .unwrap_or_else(|error| panic!("root module: {error}"));
        fs::write(dir.path().join("src/foo.rs"), "mod baz;\n")
            .unwrap_or_else(|error| panic!("override module: {error}"));
        fs::write(dir.path().join("src/baz.rs"), "pub fn value() -> bool { true }\n")
            .unwrap_or_else(|error| panic!("descendant module: {error}"));

        let files = discover(dir.path(), false, &[], &options(), TEST_SOURCE_LIMIT, false)
            .unwrap_or_else(|error| panic!("discover: {error}"));
        assert!(files.iter().any(|file| file.path.ends_with("baz.rs")));
    }

    #[test]
    fn same_file_under_two_module_names_keeps_two_scopes() {
        let dir = tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        fs::create_dir_all(dir.path().join("src")).unwrap_or_else(|error| panic!("create source: {error}"));
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname='rust-adapter-alias-fixture'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap_or_else(|error| panic!("manifest: {error}"));
        write_lock(dir.path(), "rust-adapter-alias-fixture");
        fs::write(
            dir.path().join("src/lib.rs"),
            "#[path=\"shared.rs\"] mod alpha;\n#[path=\"shared.rs\"] mod beta;\n",
        )
        .unwrap_or_else(|error| panic!("root module: {error}"));
        fs::write(dir.path().join("src/shared.rs"), "pub fn work() {}\n")
            .unwrap_or_else(|error| panic!("shared module: {error}"));

        let files = discover(dir.path(), false, &[], &options(), TEST_SOURCE_LIMIT, false)
            .unwrap_or_else(|error| panic!("discover: {error}"));
        let prefixes: HashSet<_> = files
            .iter()
            .filter(|file| file.path.ends_with("shared.rs"))
            .map(|file| file.module_prefix.as_str())
            .collect();
        assert_eq!(prefixes, HashSet::from(["alpha", "beta"]));
    }

    #[test]
    fn static_include_with_trailing_comma_keeps_module_prefix() {
        let dir = tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        fs::create_dir_all(dir.path().join("src")).unwrap_or_else(|error| panic!("create source: {error}"));
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname='rust-adapter-include-fixture'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap_or_else(|error| panic!("manifest: {error}"));
        write_lock(dir.path(), "rust-adapter-include-fixture");
        fs::write(dir.path().join("src/lib.rs"), "mod outer;\n")
            .unwrap_or_else(|error| panic!("root module: {error}"));
        fs::write(dir.path().join("src/outer.rs"), "include!(\"shared.rs\",);\n")
            .unwrap_or_else(|error| panic!("outer module: {error}"));
        fs::write(dir.path().join("src/shared.rs"), "pub fn shared() {}\n")
            .unwrap_or_else(|error| panic!("included source: {error}"));

        let files = discover(dir.path(), false, &[], &options(), TEST_SOURCE_LIMIT, false)
            .unwrap_or_else(|error| panic!("discover: {error}"));
        assert!(files
            .iter()
            .any(|file| file.path.ends_with("shared.rs") && file.module_prefix == "outer"));
    }

    #[test]
    fn explicit_feature_changes_active_source_graph() {
        let dir = tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        fs::create_dir_all(dir.path().join("src")).unwrap_or_else(|error| panic!("create source: {error}"));
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname='rust-adapter-feature-fixture'\nversion='0.1.0'\nedition='2021'\n[features]\nextra=[]\n",
        )
        .unwrap_or_else(|error| panic!("manifest: {error}"));
        write_lock(dir.path(), "rust-adapter-feature-fixture");
        fs::write(
            dir.path().join("src/lib.rs"),
            "#[cfg(feature=\"extra\")] mod extra;\npub fn base() {}\n",
        )
        .unwrap_or_else(|error| panic!("root module: {error}"));
        fs::write(dir.path().join("src/extra.rs"), "pub fn enabled() {}\n")
            .unwrap_or_else(|error| panic!("feature module: {error}"));

        let defaults = discover(dir.path(), false, &[], &options(), TEST_SOURCE_LIMIT, false)
            .unwrap_or_else(|error| panic!("default discover: {error}"));
        assert!(!defaults.iter().any(|file| file.path.ends_with("extra.rs")));
        let enabled = discover(
            dir.path(),
            false,
            &[],
            &CargoOptions {
                features: vec!["extra".into()],
                ..CargoOptions::default()
            },
            TEST_SOURCE_LIMIT,
            false,
        )
        .unwrap_or_else(|error| panic!("feature discover: {error}"));
        assert!(enabled.iter().any(|file| file.path.ends_with("extra.rs")));
    }

    #[test]
    fn path_override_outside_root_is_rejected() {
        let directory = tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        let project = directory.path().join("project");
        write_basic_project(
            &project,
            "rust-adapter-outside-path",
            "#[path=\"../../outside.rs\"] mod outside;\n",
        );
        fs::write(directory.path().join("outside.rs"), "pub fn secret() {}\n")
            .unwrap_or_else(|error| panic!("outside source: {error}"));

        let Err(error) = discover(&project, false, &[], &options(), TEST_SOURCE_LIMIT, false) else {
            panic!("outside #[path] was unexpectedly accepted");
        };
        assert!(error.contains("Rust source outside project root"), "{error}");
        assert!(error.contains("outside.rs"), "{error}");
    }

    #[test]
    fn static_include_outside_root_is_rejected() {
        let directory = tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        let project = directory.path().join("project");
        write_basic_project(
            &project,
            "rust-adapter-outside-include",
            "include!(\"../../outside.rs\");\n",
        );
        fs::write(directory.path().join("outside.rs"), "pub fn secret() {}\n")
            .unwrap_or_else(|error| panic!("outside source: {error}"));

        let Err(error) = discover(&project, false, &[], &options(), TEST_SOURCE_LIMIT, false) else {
            panic!("outside include! was unexpectedly accepted");
        };
        assert!(error.contains("Rust source outside project root"), "{error}");
        assert!(error.contains("outside.rs"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn fake_cargo_receives_literal_safe_bounded_arguments() {
        let directory = tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        let arguments = directory.path().join("arguments");
        let marker = directory.path().join("injected");
        let cargo = fake_program(
            directory.path(),
            "fake-cargo",
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > arguments\nprintf '%s\\n' '{\"packages\":[],\"workspace_members\":[],\"resolve\":{\"nodes\":[]}}'\n",
        );
        let feature = format!("alpha;touch {}", marker.display());
        let options = CargoOptions {
            features: vec![feature.clone()],
            cargo: Some(cargo),
            ..CargoOptions::default()
        };
        cargo_metadata_with_limits(
            directory.path(),
            &options,
            CommandLimits {
                timeout: Duration::from_secs(2),
                stdout_bytes: 4096,
                stderr_bytes: 4096,
            },
        )
        .unwrap_or_else(|error| panic!("metadata: {error}"));

        let actual =
            fs::read_to_string(arguments).unwrap_or_else(|error| panic!("recorded arguments: {error}"));
        let expected = [
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--offline",
            "--features",
            &feature,
        ]
        .join("\n");
        assert_eq!(actual.trim_end(), expected);
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn fake_cargo_output_limit_is_actionable() {
        let directory = tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        let cargo = fake_program(
            directory.path(),
            "fake-cargo",
            "#!/bin/sh\ni=0; while [ \"$i\" -lt 100 ]; do printf '0123456789'; i=$((i + 1)); done\n",
        );
        let options = CargoOptions {
            cargo: Some(cargo),
            ..CargoOptions::default()
        };
        let result = cargo_metadata_with_limits(
            directory.path(),
            &options,
            CommandLimits {
                timeout: Duration::from_secs(10),
                stdout_bytes: 128,
                stderr_bytes: 128,
            },
        );
        let Err(error) = result else {
            panic!("oversized metadata was unexpectedly accepted");
        };
        assert!(error.contains("128-byte stdout limit"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn fake_cargo_timeout_kills_descendants() {
        let directory = tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        let marker = directory.path().join("leaked");
        let cargo = fake_program(
            directory.path(),
            "fake-cargo",
            "#!/bin/sh\n(sleep 0.2; printf leaked > leaked) &\nsleep 5\n",
        );
        let options = CargoOptions {
            cargo: Some(cargo),
            ..CargoOptions::default()
        };
        let result = cargo_metadata_with_limits(
            directory.path(),
            &options,
            CommandLimits {
                timeout: Duration::from_millis(20),
                stdout_bytes: 128,
                stderr_bytes: 128,
            },
        );
        let Err(error) = result else {
            panic!("timed-out metadata command unexpectedly succeeded");
        };
        assert!(error.contains("timed out after"), "{error}");
        assert!(error.contains("process tree was terminated"), "{error}");
        thread::sleep(Duration::from_millis(350));
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn fake_rustc_cfg_preserves_resolved_features_and_argv() {
        let directory = tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        let arguments = directory.path().join("rustc-arguments");
        let rustc = fake_program(
            directory.path(),
            "fake-rustc",
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > rustc-arguments\nprintf 'unix\\ntarget_os=\"fake\"\\n'\n",
        );
        let package = Package {
            id: "fixture".into(),
            manifest_path: directory.path().join("Cargo.toml"),
            targets: Vec::new(),
        };
        let target = Target {
            name: "fixture".into(),
            kind: vec!["lib".into()],
            src_path: directory.path().join("src/lib.rs"),
        };
        let context = rustc_cfg_with_limits(
            directory.path(),
            &package,
            &target,
            false,
            &["alpha".into()],
            rustc.as_os_str(),
            CommandLimits {
                timeout: Duration::from_secs(2),
                stdout_bytes: 4096,
                stderr_bytes: 4096,
            },
        )
        .unwrap_or_else(|error| panic!("cfg: {error}"));

        let actual = fs::read_to_string(arguments).unwrap_or_else(|error| panic!("rustc arguments: {error}"));
        assert_eq!(actual, "--print\ncfg\n");
        let feature: Attribute = syn::parse_quote!(#[cfg(feature = "alpha")]);
        let unix: Attribute = syn::parse_quote!(#[cfg(unix)]);
        assert!(context.attrs_active(&[feature, unix]));
    }
}
