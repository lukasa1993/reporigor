use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use proc_macro2::TokenStream;
use quote::ToTokens;
use serde::Deserialize;
use syn::parse::{ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, Lit, LitStr, Meta, Token};
use walkdir::{DirEntry, WalkDir};

use reporigor_core::{
    DependencyRecord, DependencyScope, FeatureRecord, ModuleRecord, PackageRecord, RepositorySemantics,
    SymbolVisibility,
};

use crate::{
    cargo_proxy::resolve_program,
    command::{run_bounded, CommandLimits},
    output::render_stream,
    CargoOptions,
};

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

    #[cfg(test)]
    pub(crate) fn with_synthetic_names(include_tests: bool, names: &[&str]) -> Self {
        Self::with_synthetic_values(include_tests, names, false)
    }

    #[cfg(test)]
    pub(crate) fn with_synthetic_features(include_tests: bool, features: &[&str]) -> Self {
        Self::with_synthetic_values(include_tests, features, true)
    }

    #[cfg(test)]
    fn with_synthetic_values(include_tests: bool, values: &[&str], features: bool) -> Self {
        let mut context = SingleCfgContext::synthetic(include_tests);
        let selected = if features {
            &mut context.features
        } else {
            &mut context.names
        };
        selected.extend(values.iter().map(|value| (*value).to_string()));
        Self {
            variants: vec![context],
        }
    }

    pub(crate) fn attrs_active(&self, attrs: &[Attribute]) -> bool {
        self.variants.iter().any(|context| context.attrs_active(attrs))
    }

    pub(crate) fn attrs_active_in_production(&self, attrs: &[Attribute]) -> bool {
        self.attrs_active_where(attrs, false)
    }

    pub(crate) fn attrs_active_in_tests(&self, attrs: &[Attribute]) -> bool {
        self.attrs_active_where(attrs, true)
    }

    fn attrs_active_where(&self, attrs: &[Attribute], include_tests: bool) -> bool {
        self.variants
            .iter()
            .any(|context| context.include_tests == include_tests && context.attrs_active(attrs))
    }

    pub(crate) fn has_production_variant(&self) -> bool {
        self.variants.iter().any(|context| !context.include_tests)
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
    pub package: String,
    pub cfg: CfgContext,
    pub target_gated: bool,
    pub cfg_evidence: String,
    pub visibility: SymbolVisibility,
    pub framework_managed: bool,
    pub reflection_reachable: bool,
}

#[derive(Debug)]
pub(crate) struct ScopeDiscovery {
    pub scopes: Vec<ScopedFile>,
    pub repository: RepositorySemantics,
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
    package: String,
    cfg: SingleCfgContext,
    target_gated: bool,
    cfg_evidence: String,
    visibility: SymbolVisibility,
    framework_managed: bool,
    reflection_reachable: bool,
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
    #[serde(default)]
    deps: Vec<ResolveDependency>,
}

#[derive(Deserialize)]
struct ResolveDependency {
    name: String,
    pkg: String,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    manifest_path: PathBuf,
    targets: Vec<Target>,
    dependencies: Vec<MetadataDependency>,
    features: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
struct MetadataDependency {
    name: String,
    rename: Option<String>,
    kind: Option<String>,
    optional: bool,
    target: Option<String>,
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

pub(crate) fn parse_meta_list_for_analysis(tokens: TokenStream) -> Option<Vec<Meta>> {
    parse_meta_list(tokens)
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

pub(crate) fn cfg_evidence(attributes: &[Attribute]) -> String {
    attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("cfg") || attribute.path().is_ident("cfg_attr"))
        .map(|attribute| attribute.to_token_stream().to_string().replace(' ', ""))
        .collect::<Vec<_>>()
        .join("|")
}

pub(crate) fn combine_cfg_evidence(inherited: &str, attributes: &[Attribute]) -> String {
    let own = cfg_evidence(attributes);
    [inherited, own.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("|")
}

fn cfg_predicate_has_target_dimension(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => !path.is_ident("test"),
        Meta::NameValue(_) => true,
        Meta::List(list) => parse_meta_list(list.tokens.clone())
            .is_none_or(|items| items.iter().any(cfg_predicate_has_target_dimension)),
    }
}

pub(crate) fn attributes_target_gated(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        let Meta::List(list) = &attribute.meta else {
            return false;
        };
        let items = parse_meta_list(list.tokens.clone());
        if attribute.path().is_ident("cfg") {
            return items.is_none_or(|items| items.iter().any(cfg_predicate_has_target_dimension));
        }
        if attribute.path().is_ident("cfg_attr") {
            return items.is_none_or(|items| {
                items.first().is_some_and(cfg_predicate_has_target_dimension)
                    || items.iter().skip(1).any(|item| {
                        matches!(item, Meta::List(nested) if nested.path.is_ident("cfg")
                        && parse_meta_list(nested.tokens.clone()).is_none_or(|predicates| {
                            predicates.iter().any(cfg_predicate_has_target_dimension)
                        }))
                    })
            });
        }
        false
    })
}

fn target_gated(attributes: &[Attribute]) -> bool {
    attributes_target_gated(attributes)
}

pub(crate) fn module_visibility(value: &syn::Visibility) -> SymbolVisibility {
    match value {
        syn::Visibility::Public(_) => SymbolVisibility::Public,
        syn::Visibility::Inherited => SymbolVisibility::Private,
        syn::Visibility::Restricted(_) => SymbolVisibility::Crate,
    }
}

pub(crate) fn module_reflection_reachable(attributes: &[Attribute]) -> bool {
    attribute_markers(attributes).any(|marker| reflection_marker(&marker))
}

pub(crate) fn module_framework_managed(attributes: &[Attribute]) -> bool {
    attribute_markers(attributes).any(|marker| framework_marker(&marker))
}

fn reflection_marker(marker: &str) -> bool {
    marker_set_contains(
        marker,
        "ctor|inventory|linkme|reflect|reflection|register|typetag|used",
    )
}

fn framework_marker(marker: &str) -> bool {
    !marker_set_contains(marker, "allow|cfg|cfg_attr|deny|deprecated|doc|forbid|path|warn")
}

fn marker_set_contains(marker: &str, encoded: &str) -> bool {
    encoded.split('|').any(|candidate| marker == candidate)
}

fn attribute_markers(attributes: &[Attribute]) -> impl Iterator<Item = String> + '_ {
    attributes.iter().map(|attribute| {
        attribute
            .path()
            .segments
            .last()
            .map_or_else(String::new, |segment| segment.ident.to_string())
    })
}

#[derive(Default)]
struct RegistrationVisitor {
    reachable: bool,
}

impl<'ast> Visit<'ast> for RegistrationVisitor {
    fn visit_attribute(&mut self, attribute: &'ast Attribute) {
        let attributes = std::slice::from_ref(attribute);
        self.reachable |= module_reflection_reachable(attributes) || module_framework_managed(attributes);
        visit::visit_attribute(self, attribute);
    }

    fn visit_macro(&mut self, item: &'ast syn::Macro) {
        // An arbitrary macro invocation can expand to registration or exported
        // items. The adapter cannot prove the module unused without expansion.
        self.reachable = true;
        visit::visit_macro(self, item);
    }
}

pub(crate) fn items_reflection_reachable(items: &[syn::Item]) -> bool {
    let mut visitor = RegistrationVisitor::default();
    for item in items {
        visitor.visit_item(item);
    }
    visitor.reachable
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
            Meta::Path(path) => self.eval_path(path),
            Meta::NameValue(value) => self.eval_name_value(value),
            Meta::List(list) => self.eval_list(list),
        }
    }

    fn eval_path(&self, path: &syn::Path) -> bool {
        if path.is_ident("test") {
            return self.include_tests;
        }
        path.get_ident()
            .is_some_and(|name| self.names.contains(&name.to_string()))
    }

    fn eval_name_value(&self, value: &syn::MetaNameValue) -> bool {
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

    fn eval_list(&self, list: &syn::MetaList) -> bool {
        let predicate = list.path.get_ident().map(ToString::to_string);
        let items = parse_meta_list(list.tokens.clone());
        match predicate.as_deref() {
            Some("all") => items.is_some_and(|items| items.iter().all(|item| self.eval(item))),
            Some("any") => items.is_some_and(|items| items.iter().any(|item| self.eval(item))),
            Some("not") => items.is_some_and(|items| items.len() == 1 && !self.eval(&items[0])),
            _ => false,
        }
    }

    fn meta_attribute_active(&self, meta: &Meta) -> bool {
        match meta {
            Meta::Path(path) => !path.is_ident("test") || self.include_tests,
            Meta::List(list) => self.list_attribute_active(list),
            Meta::NameValue(_) => true,
        }
    }

    fn list_attribute_active(&self, list: &syn::MetaList) -> bool {
        if list.path.is_ident("cfg") {
            return self.cfg_attribute_active(list);
        }
        if !list.path.is_ident("cfg_attr") {
            return true;
        }
        self.cfg_attr_attribute_active(list)
    }

    fn cfg_attribute_active(&self, list: &syn::MetaList) -> bool {
        syn::parse2::<Meta>(list.tokens.clone())
            .ok()
            .is_none_or(|predicate| self.eval(&predicate))
    }

    fn cfg_attr_attribute_active(&self, list: &syn::MetaList) -> bool {
        let Some((predicate, nested)) = cfg_attr_parts(list) else {
            return true;
        };
        !self.eval(&predicate)
            || nested
                .iter()
                .all(|attribute| self.meta_attribute_active(attribute))
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

fn cfg_attr_parts(list: &syn::MetaList) -> Option<(Meta, Vec<Meta>)> {
    parse_meta_list(list.tokens.clone()).and_then(|items| {
        items
            .split_first()
            .map(|(predicate, nested)| (predicate.clone(), nested.to_vec()))
    })
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
    std::env::var_os("RUSTC").map_or_else(
        || discover_rustc_program(options),
        |configured| resolved_program(&configured, "cannot resolve configured RUSTC"),
    )
}

fn discover_rustc_program(options: &CargoOptions) -> Result<OsString, String> {
    let cargo = resolved_cargo(options, "for rustc discovery")?;
    let executable = if cfg!(windows) { "rustc.exe" } else { "rustc" };
    if let Some(candidate) = adjacent_rustc(&cargo, executable) {
        return resolved_program(candidate.as_os_str(), "cannot resolve rustc beside Cargo");
    }
    resolved_program(
        OsStr::new("rustc"),
        "cannot resolve rustc on absolute PATH entries",
    )
}

fn resolved_cargo(options: &CargoOptions, purpose: &str) -> Result<PathBuf, String> {
    resolve_program(options.cargo_program())
        .map_err(|error| format!("cannot resolve Cargo executable {purpose}: {error}"))
}

fn resolved_program(program: &OsStr, context: &str) -> Result<OsString, String> {
    resolve_program(program)
        .map(PathBuf::into_os_string)
        .map_err(|error| format!("{context}: {error}"))
}

fn adjacent_rustc(cargo: &Path, executable: &str) -> Option<PathBuf> {
    cargo
        .parent()
        .map(|parent| parent.join(executable))
        .filter(|candidate| candidate.is_file())
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
        RustcCfgRequest {
            root,
            package,
            target,
            include_tests,
            features,
        },
        &program,
        cfg_limits(),
    )
}

#[derive(Clone, Copy)]
struct RustcCfgRequest<'a> {
    root: &'a Path,
    package: &'a Package,
    target: &'a Target,
    include_tests: bool,
    features: &'a [String],
}

fn rustc_cfg_with_limits(
    request: RustcCfgRequest<'_>,
    program: &OsStr,
    limits: CommandLimits,
) -> Result<SingleCfgContext, String> {
    let mut command = rustc_cfg_command(request.root, program);
    let action = format!(
        "rustc cfg discovery for {} target {}",
        request.package.manifest_path.display(),
        request.target.name
    );
    let output = run_bounded(&mut command, &action, limits)?;
    validate_rustc_cfg_output(&output, request.package, request.target, limits)?;
    let text = String::from_utf8(output.stdout.bytes)
        .map_err(|error| format!("rustc cfg output is invalid UTF-8: {error}"))?;
    let mut context = parse_cfg_output(&text, request.include_tests);
    context.features.extend(request.features.iter().cloned());
    Ok(context)
}

fn rustc_cfg_command(root: &Path, program: &OsStr) -> Command {
    let mut command = Command::new(program);
    command.args(["--print", "cfg"]);
    if let Some(cargo_target) = std::env::var_os("CARGO_BUILD_TARGET") {
        command.arg("--target").arg(cargo_target);
    }
    command
        .current_dir(root)
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    command
}

fn validate_rustc_cfg_output(
    output: &crate::command::BoundedOutput,
    package: &Package,
    target: &Target,
    limits: CommandLimits,
) -> Result<(), String> {
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
    Ok(())
}

fn cargo_metadata(root: &Path, options: &CargoOptions) -> Result<Metadata, String> {
    cargo_metadata_with_limits(root, options, metadata_limits())
}

fn cargo_metadata_with_limits(
    root: &Path,
    options: &CargoOptions,
    limits: CommandLimits,
) -> Result<Metadata, String> {
    let cargo = resolved_cargo(options, "on absolute PATH entries")?;
    let mut command = cargo_metadata_command(root, options, &cargo);
    let output = run_bounded(&mut command, "Cargo metadata discovery", limits)?;
    validate_metadata_output(&output, limits)?;
    serde_json::from_slice(&output.stdout.bytes)
        .map_err(|error| format!("cannot decode Cargo metadata JSON: {error}"))
}

fn cargo_metadata_command(root: &Path, options: &CargoOptions, cargo: &Path) -> Command {
    let mut command = Command::new(cargo);
    command
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .args(options.feature_args())
        .current_dir(root)
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TERM_COLOR", "never");
    command
}

fn validate_metadata_output(
    output: &crate::command::BoundedOutput,
    limits: CommandLimits,
) -> Result<(), String> {
    if !output.status.success() {
        let detail = metadata_error_detail(output, limits);
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
    Ok(())
}

fn metadata_error_detail(output: &crate::command::BoundedOutput, limits: CommandLimits) -> String {
    let stderr = render_stream(&output.stderr, limits.stderr_bytes);
    if stderr.trim().is_empty() {
        render_stream(&output.stdout, limits.stdout_bytes)
    } else {
        stderr
    }
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
    let metadata = source_metadata(path)?;
    let limit = u64::try_from(max_source_bytes).unwrap_or(u64::MAX);
    if metadata.len() > limit {
        return Ok(BoundedSource::TooLarge {
            actual_bytes: metadata.len(),
        });
    }
    let initial_capacity = usize::try_from(metadata.len())
        .unwrap_or(max_source_bytes)
        .min(max_source_bytes);
    let bytes = read_limited_source(path, initial_capacity, limit)?;
    if bytes.len() > max_source_bytes {
        return Ok(BoundedSource::TooLarge {
            actual_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        });
    }
    String::from_utf8(bytes)
        .map(BoundedSource::Content)
        .map_err(|error| format!("Rust source {} is invalid UTF-8: {error}", path.display()))
}

fn source_metadata(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("cannot inspect Rust source {}: {error}", path.display()))?;
    if metadata.is_file() {
        Ok(metadata)
    } else {
        Err(format!("Rust source is not a regular file: {}", path.display()))
    }
}

fn read_limited_source(path: &Path, capacity: usize, limit: u64) -> Result<Vec<u8>, String> {
    let file = fs::File::open(path)
        .map_err(|error| format!("cannot read Rust source {}: {error}", path.display()))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read Rust source {}: {error}", path.display()))?;
    Ok(bytes)
}

fn resolve_module(
    module: &syn::ItemMod,
    module_dir: &Path,
    context: &SingleCfgContext,
) -> Result<ResolvedModule, String> {
    if let Some(relative) = context.path_override(&module.attrs) {
        return resolve_overridden_module(module, module_dir, &relative);
    }
    resolve_conventional_module(module, module_dir)
}

fn resolve_overridden_module(
    module: &syn::ItemMod,
    module_dir: &Path,
    relative: &Path,
) -> Result<ResolvedModule, String> {
    let path = module_dir.join(relative);
    if !path.is_file() {
        return Err(format!(
            "active #[path] module {} does not exist: {}",
            module.ident,
            path.display()
        ));
    }
    let descendant_dir = path.parent().unwrap_or(module_dir).to_path_buf();
    Ok(ResolvedModule { path, descendant_dir })
}

fn resolve_conventional_module(module: &syn::ItemMod, module_dir: &Path) -> Result<ResolvedModule, String> {
    let direct = module_dir.join(format!("{}.rs", module.ident));
    let nested = module_dir.join(module.ident.to_string()).join("mod.rs");
    if direct.is_file() {
        return resolve_direct_module(module, module_dir, direct, &nested);
    }
    if nested.is_file() {
        return Ok(ResolvedModule {
            path: nested,
            descendant_dir: module_dir.join(module.ident.to_string()),
        });
    }
    Err(format!(
        "active module {} cannot be resolved below {}",
        module.ident,
        module_dir.display()
    ))
}

fn resolve_direct_module(
    module: &syn::ItemMod,
    module_dir: &Path,
    direct: PathBuf,
    nested: &Path,
) -> Result<ResolvedModule, String> {
    if nested.is_file() {
        return Err(format!(
            "module {} is ambiguous: both {} and {} exist",
            module.ident,
            direct.display(),
            nested.display()
        ));
    }
    Ok(ResolvedModule {
        path: direct,
        descendant_dir: module_dir.join(module.ident.to_string()),
    })
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
    package: &'a str,
    max_source_bytes: usize,
    allow_parse_errors: bool,
    context: &'a SingleCfgContext,
    visited: &'a mut HashSet<(PathBuf, String)>,
    output: &'a mut Vec<TargetScopedFile>,
}

#[derive(Clone, Copy)]
struct ModuleContext<'a> {
    prefix: &'a str,
    target_gated: bool,
    cfg_evidence: &'a str,
    framework_managed: bool,
    reflection_reachable: bool,
}

#[derive(Clone, Copy)]
struct FileContext<'a> {
    module: ModuleContext<'a>,
    visibility: SymbolVisibility,
}

fn walk_items(
    state: &mut WalkState<'_>,
    items: &[syn::Item],
    module_dir: &Path,
    source_dir: &Path,
    context: ModuleContext<'_>,
) -> Result<(), String> {
    for item in items {
        if !state.context.attrs_active(item_attrs(item)) {
            continue;
        }
        walk_item(state, item, module_dir, source_dir, context)?;
    }
    Ok(())
}

fn walk_item(
    state: &mut WalkState<'_>,
    item: &syn::Item,
    module_dir: &Path,
    source_dir: &Path,
    context: ModuleContext<'_>,
) -> Result<(), String> {
    match item {
        syn::Item::Mod(module) => walk_module(state, module, module_dir, source_dir, context),
        syn::Item::Macro(item_macro) => walk_include(state, item_macro, source_dir, context),
        _ => Ok(()),
    }
}

fn walk_module(
    state: &mut WalkState<'_>,
    module: &syn::ItemMod,
    module_dir: &Path,
    source_dir: &Path,
    context: ModuleContext<'_>,
) -> Result<(), String> {
    let next_prefix = prefix(context.prefix, &module.ident.to_string());
    let nested_cfg_evidence = combine_cfg_evidence(context.cfg_evidence, &module.attrs);
    let nested_context = nested_module_context(context, module, &next_prefix, &nested_cfg_evidence);
    if let Some((_, nested)) = &module.content {
        let nested_dir = module_dir.join(module.ident.to_string());
        return walk_items(state, nested, &nested_dir, source_dir, nested_context);
    }
    let resolved = resolve_module(module, module_dir, state.context)?;
    visit_file(
        state,
        &resolved.path,
        &resolved.descendant_dir,
        FileContext {
            module: nested_context,
            visibility: module_visibility(&module.vis),
        },
    )
}

fn nested_module_context<'a>(
    context: ModuleContext<'a>,
    module: &syn::ItemMod,
    next_prefix: &'a str,
    cfg_evidence: &'a str,
) -> ModuleContext<'a> {
    ModuleContext {
        prefix: next_prefix,
        target_gated: context.target_gated || target_gated(&module.attrs),
        cfg_evidence,
        framework_managed: context.framework_managed || module_framework_managed(&module.attrs),
        reflection_reachable: context.reflection_reachable || module_reflection_reachable(&module.attrs),
    }
}

fn walk_include(
    state: &mut WalkState<'_>,
    item: &syn::ItemMacro,
    source_dir: &Path,
    context: ModuleContext<'_>,
) -> Result<(), String> {
    let Some(path) = static_include_path(item, source_dir) else {
        return Ok(());
    };
    if !path.is_file() {
        return Ok(());
    }
    let include_dir = path.parent().unwrap_or(source_dir).to_path_buf();
    visit_file(
        state,
        &path,
        &include_dir,
        FileContext {
            module: context,
            visibility: SymbolVisibility::Crate,
        },
    )
}

fn visit_file(
    state: &mut WalkState<'_>,
    path: &Path,
    module_dir: &Path,
    context: FileContext<'_>,
) -> Result<(), String> {
    let Some(canonical) = prepare_visit_path(state, path, context.module.prefix)? else {
        return Ok(());
    };
    match read_source_bounded(&canonical, state.max_source_bytes)? {
        BoundedSource::Content(source) => visit_source(state, &canonical, module_dir, context, &source),
        BoundedSource::TooLarge { .. } => {
            state.output.push(base_scoped_file(state, canonical, context));
            Ok(())
        }
    }
}

fn prepare_visit_path(
    state: &mut WalkState<'_>,
    path: &Path,
    module_prefix: &str,
) -> Result<Option<PathBuf>, String> {
    let canonical = canonical_within_root(state.root, path, "Rust source")?;
    if is_generated_path(state.root, &canonical) {
        return Ok(None);
    }
    let key = (canonical.clone(), module_prefix.to_string());
    Ok(state.visited.insert(key).then_some(canonical))
}

fn visit_source(
    state: &mut WalkState<'_>,
    canonical: &Path,
    module_dir: &Path,
    context: FileContext<'_>,
    source: &str,
) -> Result<(), String> {
    if has_generated_marker(source) {
        return Ok(());
    }
    let Some(syntax) = parse_visited_source(canonical, source, state.allow_parse_errors)? else {
        state
            .output
            .push(base_scoped_file(state, canonical.to_path_buf(), context));
        return Ok(());
    };
    visit_syntax(state, canonical, module_dir, context, &syntax)
}

fn parse_visited_source(
    path: &Path,
    source: &str,
    allow_parse_errors: bool,
) -> Result<Option<syn::File>, String> {
    match syn::parse_file(source) {
        Ok(syntax) => Ok(Some(syntax)),
        Err(_) if allow_parse_errors => Ok(None),
        Err(error) => Err(format!("Rust parse error in {}: {error}", path.display())),
    }
}

fn visit_syntax(
    state: &mut WalkState<'_>,
    canonical: &Path,
    module_dir: &Path,
    context: FileContext<'_>,
    syntax: &syn::File,
) -> Result<(), String> {
    if !state.context.attrs_active(&syntax.attrs) {
        return Ok(());
    }
    let reflection = syntax_reflection(context, syntax);
    let record = enriched_scoped_file(
        state,
        canonical.to_path_buf(),
        context,
        &syntax.attrs,
        reflection.syntax,
    );
    state.output.push(record);
    let source_dir = canonical.parent().unwrap_or(module_dir);
    let nested_cfg_evidence = combine_cfg_evidence(context.module.cfg_evidence, &syntax.attrs);
    walk_items(
        state,
        &syntax.items,
        module_dir,
        source_dir,
        ModuleContext {
            prefix: context.module.prefix,
            target_gated: context.module.target_gated || target_gated(&syntax.attrs),
            cfg_evidence: &nested_cfg_evidence,
            framework_managed: context.module.framework_managed || module_framework_managed(&syntax.attrs),
            reflection_reachable: reflection.attributed,
        },
    )
}

struct SyntaxReflection {
    attributed: bool,
    syntax: bool,
}

fn syntax_reflection(context: FileContext<'_>, syntax: &syn::File) -> SyntaxReflection {
    let attributed = context.module.reflection_reachable || module_reflection_reachable(&syntax.attrs);
    SyntaxReflection {
        attributed,
        syntax: attributed || items_reflection_reachable(&syntax.items),
    }
}

fn base_scoped_file(state: &WalkState<'_>, path: PathBuf, context: FileContext<'_>) -> TargetScopedFile {
    TargetScopedFile {
        path,
        module_prefix: context.module.prefix.to_string(),
        package: state.package.to_string(),
        cfg: state.context.clone(),
        target_gated: context.module.target_gated,
        cfg_evidence: context.module.cfg_evidence.to_string(),
        visibility: context.visibility,
        framework_managed: context.module.framework_managed,
        reflection_reachable: context.module.reflection_reachable,
    }
}

fn enriched_scoped_file(
    state: &WalkState<'_>,
    path: PathBuf,
    context: FileContext<'_>,
    attrs: &[Attribute],
    reflection_reachable: bool,
) -> TargetScopedFile {
    let mut scoped = base_scoped_file(state, path, context);
    scoped.target_gated |= target_gated(attrs);
    scoped.cfg_evidence = combine_cfg_evidence(context.module.cfg_evidence, attrs);
    scoped.framework_managed |= module_framework_managed(attrs);
    scoped.reflection_reachable = reflection_reachable;
    scoped
}

fn ignored(entry: &DirEntry) -> bool {
    entry.file_name().to_str().is_some_and(|name| {
        ".git|target|vendor|node_modules|.venv|venv|build|dist"
            .split('|')
            .any(|ignored| ignored == name)
    })
}

fn is_generated_path(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).unwrap_or(path).components().any(|part| {
        matches!(
            part.as_os_str().to_str(),
            Some("generated" | "gen" | "DerivedSources")
        )
    })
}

fn has_generated_marker(source: &str) -> bool {
    let header = source.lines().take(8).collect::<Vec<_>>().join("\n");
    let header = header.to_ascii_lowercase();
    header.contains("@generated")
        || header.contains("code generated")
        || header.contains("automatically generated")
        || (header.contains("generated") && header.contains("do not edit"))
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
        .filter(|path| !is_generated_path(root, path))
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
            package: String::new(),
            cfg: context.clone(),
            target_gated: false,
            cfg_evidence: String::new(),
            visibility: SymbolVisibility::Unknown,
            framework_managed: false,
            reflection_reachable: false,
        })
        .collect();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    files
}

struct MergedScope {
    contexts: Vec<SingleCfgContext>,
    all_target_gated: bool,
    cfg_evidence: BTreeSet<String>,
    visibility: SymbolVisibility,
    framework_managed: bool,
    reflection_reachable: bool,
}

fn merge_context(merged: &mut HashMap<(PathBuf, String, String), MergedScope>, file: TargetScopedFile) {
    let key = (file.path, file.module_prefix, file.package);
    let entry = merged.entry(key).or_insert_with(|| MergedScope {
        contexts: Vec::new(),
        all_target_gated: file.target_gated,
        cfg_evidence: BTreeSet::new(),
        visibility: file.visibility,
        framework_managed: file.framework_managed,
        reflection_reachable: file.reflection_reachable,
    });
    entry.all_target_gated &= file.target_gated;
    entry.visibility = entry.visibility.max(file.visibility);
    entry.framework_managed |= file.framework_managed;
    entry.reflection_reachable |= file.reflection_reachable;
    if !file.cfg_evidence.is_empty() {
        entry.cfg_evidence.insert(file.cfg_evidence);
    }
    if !entry.contexts.contains(&file.cfg) {
        entry.contexts.push(file.cfg);
    }
}

fn dependency_scope(kind: Option<&str>) -> DependencyScope {
    match kind {
        Some("dev") => DependencyScope::Development,
        Some("build") => DependencyScope::Build,
        _ => DependencyScope::Production,
    }
}

fn crate_identifier(value: &str) -> String {
    value.replace('-', "_")
}

fn dependency_source_identifier(
    metadata: &Metadata,
    package: &Package,
    dependency: &MetadataDependency,
) -> String {
    let configured = dependency.rename.as_deref().map(crate_identifier);
    let resolved = metadata
        .resolve
        .as_ref()
        .and_then(|resolve| resolve.nodes.iter().find(|node| node.id == package.id))
        .into_iter()
        .flat_map(|node| &node.deps)
        .filter(|edge| {
            metadata
                .packages
                .iter()
                .find(|candidate| candidate.id == edge.pkg)
                .is_some_and(|candidate| candidate.name == dependency.name)
        })
        .find(|edge| configured.as_ref().is_none_or(|expected| edge.name == *expected))
        .map(|edge| edge.name.clone());
    if let Some(resolved) = resolved {
        return resolved;
    }
    if let Some(configured) = configured {
        return configured;
    }
    metadata
        .packages
        .iter()
        .find(|candidate| candidate.name == dependency.name)
        .and_then(|candidate| {
            candidate.targets.iter().find(|target| {
                target.kind.iter().any(|kind| {
                    "lib|rlib|dylib|cdylib|staticlib|proc-macro"
                        .split('|')
                        .any(|library_kind| library_kind == kind)
                })
            })
        })
        .map_or_else(
            || crate_identifier(&dependency.name),
            |target| crate_identifier(&target.name),
        )
}

fn metadata_semantics(root: &Path, metadata: &Metadata) -> RepositorySemantics {
    let workspace = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let packages = metadata
        .packages
        .iter()
        .filter(|package| workspace.contains(package.id.as_str()))
        .collect::<Vec<_>>();
    let internal = packages
        .iter()
        .map(|package| package.name.as_str())
        .collect::<HashSet<_>>();
    let mut repository = RepositorySemantics {
        dependency_graph_reliable: true,
        feature_inventory_reliable: true,
        ..RepositorySemantics::default()
    };
    for package in packages {
        let package_root = package
            .manifest_path
            .parent()
            .unwrap_or(package.manifest_path.as_path());
        repository.packages.push(PackageRecord {
            name: package.name.clone(),
            root: crate::relative(root, package_root),
        });
        for dependency in &package.dependencies {
            repository.dependencies.push(DependencyRecord {
                package: package.name.clone(),
                dependency: dependency.name.clone(),
                source_identifier: dependency_source_identifier(metadata, package, dependency),
                scope: dependency_scope(dependency.kind.as_deref()),
                internal: internal.contains(dependency.name.as_str()),
                optional: dependency.optional,
                target_gated: dependency.target.is_some(),
            });
        }
        for (name, enables) in &package.features {
            repository.features.push(FeatureRecord {
                package: package.name.clone(),
                name: name.clone(),
                references: 0,
                enables: enables.iter().cloned().collect(),
                target_gated: false,
            });
        }
    }
    repository.canonicalize();
    repository
}

pub(crate) fn discover_with_semantics(
    root: &Path,
    include_tests: bool,
    filters: &[String],
    options: &CargoOptions,
    max_source_bytes: usize,
    allow_parse_errors: bool,
) -> Result<ScopeDiscovery, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve Rust project root {}: {error}", root.display()))?;
    let root = root.as_path();
    if !root.join("Cargo.toml").is_file() {
        return Ok(ScopeDiscovery {
            scopes: fallback_files(root, include_tests),
            repository: RepositorySemantics::default(),
        });
    }
    let metadata = cargo_metadata(root, options)?;
    let mut repository = metadata_semantics(root, &metadata);
    let resolved_features = resolved_feature_map(&metadata);
    let workspace: HashSet<_> = metadata.workspace_members.iter().cloned().collect();
    let inputs = DiscoveryInputs {
        root,
        include_tests,
        options,
        max_source_bytes,
        allow_parse_errors,
        resolved_features: &resolved_features,
    };
    let merged = collect_workspace_scopes(inputs, metadata.packages, &workspace)?;
    let mut output = scopes_from_merged(root, merged);
    let complete_scope_count = output.len();
    retain_scope_filters(root, &mut output, filters);
    let module_inventory_complete = output.len() == complete_scope_count;
    sort_scopes(&mut output);
    append_module_records(root, &output, &mut repository);
    repository.module_graph_reliable = module_inventory_complete;
    repository.canonicalize();
    Ok(ScopeDiscovery {
        scopes: output,
        repository,
    })
}

#[derive(Clone, Copy)]
struct DiscoveryInputs<'a> {
    root: &'a Path,
    include_tests: bool,
    options: &'a CargoOptions,
    max_source_bytes: usize,
    allow_parse_errors: bool,
    resolved_features: &'a HashMap<String, Vec<String>>,
}

fn resolved_feature_map(metadata: &Metadata) -> HashMap<String, Vec<String>> {
    metadata
        .resolve
        .as_ref()
        .map(|resolve| {
            resolve
                .nodes
                .iter()
                .map(|node| (node.id.clone(), node.features.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn collect_workspace_scopes(
    inputs: DiscoveryInputs<'_>,
    packages: Vec<Package>,
    workspace: &HashSet<String>,
) -> Result<HashMap<(PathBuf, String, String), MergedScope>, String> {
    let mut merged = HashMap::new();
    for package in packages
        .into_iter()
        .filter(|package| workspace.contains(&package.id))
    {
        collect_package_scopes(inputs, &package, &mut merged)?;
    }
    Ok(merged)
}

fn collect_package_scopes(
    inputs: DiscoveryInputs<'_>,
    package: &Package,
    merged: &mut HashMap<(PathBuf, String, String), MergedScope>,
) -> Result<(), String> {
    canonical_within_root(inputs.root, &package.manifest_path, "Cargo workspace manifest")?;
    let mut collector = PackageScopeCollector {
        inputs,
        package,
        merged,
    };
    for target in package
        .targets
        .iter()
        .filter(|target| target_in_scope(&target.kind, inputs.include_tests))
    {
        collector.collect_target(target)?;
    }
    Ok(())
}

struct PackageScopeCollector<'a, 'output> {
    inputs: DiscoveryInputs<'a>,
    package: &'a Package,
    merged: &'output mut HashMap<(PathBuf, String, String), MergedScope>,
}

impl PackageScopeCollector<'_, '_> {
    fn collect_target(&mut self, target: &Target) -> Result<(), String> {
        canonical_within_root(self.inputs.root, &target.src_path, "Cargo target source")?;
        let features = self
            .inputs
            .resolved_features
            .get(&self.package.id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let production = rustc_cfg(
            self.inputs.root,
            self.package,
            target,
            false,
            self.inputs.options,
            features,
        )?;
        for context in target_contexts(production, self.inputs.include_tests) {
            collect_context_scopes(self.inputs, self.package, target, &context, self.merged)?;
        }
        Ok(())
    }
}

fn target_contexts(production: SingleCfgContext, include_tests: bool) -> Vec<SingleCfgContext> {
    let mut contexts = vec![production.clone()];
    if include_tests {
        let mut test = production;
        test.include_tests = true;
        contexts.push(test);
    }
    contexts
}

fn collect_context_scopes(
    inputs: DiscoveryInputs<'_>,
    package: &Package,
    target: &Target,
    context: &SingleCfgContext,
    merged: &mut HashMap<(PathBuf, String, String), MergedScope>,
) -> Result<(), String> {
    let mut visited = HashSet::new();
    let mut target_files = Vec::new();
    let module_dir = root_module_dir(&target.src_path);
    let mut state = WalkState {
        root: inputs.root,
        package: &package.name,
        max_source_bytes: inputs.max_source_bytes,
        allow_parse_errors: inputs.allow_parse_errors,
        context,
        visited: &mut visited,
        output: &mut target_files,
    };
    visit_file(
        &mut state,
        &target.src_path,
        &module_dir,
        FileContext {
            module: ModuleContext {
                prefix: "",
                target_gated: false,
                cfg_evidence: "",
                framework_managed: false,
                reflection_reachable: false,
            },
            visibility: SymbolVisibility::Public,
        },
    )?;
    for file in target_files {
        merge_context(merged, file);
    }
    Ok(())
}

fn scopes_from_merged(
    root: &Path,
    merged: HashMap<(PathBuf, String, String), MergedScope>,
) -> Vec<ScopedFile> {
    merged
        .into_iter()
        .map(|((path, module_prefix, package), merged)| ScopedFile {
            path,
            module_prefix,
            package,
            cfg: CfgContext {
                variants: merged.contexts,
            },
            target_gated: merged.all_target_gated,
            cfg_evidence: merged.cfg_evidence.into_iter().collect::<Vec<_>>().join("|"),
            visibility: merged.visibility,
            framework_managed: merged.framework_managed,
            reflection_reachable: merged.reflection_reachable,
        })
        .filter(|file| !is_generated_path(root, &file.path))
        .collect()
}

fn retain_scope_filters(root: &Path, output: &mut Vec<ScopedFile>, filters: &[String]) {
    if !filters.is_empty() {
        output.retain(|file| {
            let relative = file
                .path
                .strip_prefix(root)
                .unwrap_or(&file.path)
                .to_string_lossy();
            filters.iter().any(|filter| relative.contains(filter))
        });
    }
}

fn sort_scopes(output: &mut [ScopedFile]) {
    output.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.package.cmp(&right.package))
            .then_with(|| left.module_prefix.cmp(&right.module_prefix))
    });
}

fn append_module_records(root: &Path, output: &[ScopedFile], repository: &mut RepositorySemantics) {
    for scope in output {
        repository.modules.push(ModuleRecord {
            stable_symbol: if scope.module_prefix.is_empty() {
                scope.package.clone()
            } else {
                format!("{}::{}", scope.package, scope.module_prefix)
            },
            file: crate::relative(root, &scope.path),
            package: Some(scope.package.clone()),
            visibility: scope.visibility,
            references: 0,
            target_gated: scope.target_gated,
            generated: false,
            framework_managed: scope.framework_managed,
            reflection_reachable: scope.reflection_reachable,
            externally_invoked: scope.module_prefix.is_empty(),
        });
    }
}

#[cfg(test)]
fn discover(
    root: &Path,
    include_tests: bool,
    filters: &[String],
    options: &CargoOptions,
) -> Result<Vec<ScopedFile>, String> {
    discover_with_semantics(root, include_tests, filters, options, 8 * 1024 * 1024, false)
        .map(|discovery| discovery.scopes)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    #[cfg(unix)]
    use std::thread;

    use super::*;
    use crate::test_support::{
        aliased_module_project, create_directory, executable, read_file, temporary, write_basic_project,
        write_file, write_lock, write_manifest,
    };

    const TEST_SOURCE_LIMIT: usize = 8 * 1024 * 1024;

    fn options() -> CargoOptions {
        CargoOptions::default()
    }

    fn discovered(root: &Path, options: &CargoOptions) -> Vec<ScopedFile> {
        discover(root, false, &[], options).unwrap_or_else(|error| panic!("discover: {error}"))
    }

    fn metadata_package(id: &str, name: &str, target: &str) -> Package {
        let root = PathBuf::from(format!("/workspace/{name}"));
        Package {
            id: id.into(),
            name: name.into(),
            manifest_path: root.join("Cargo.toml"),
            targets: vec![Target {
                name: target.into(),
                kind: vec!["lib".into()],
                src_path: root.join("src/lib.rs"),
            }],
            dependencies: Vec::new(),
            features: BTreeMap::new(),
        }
    }

    fn metadata_dependency(name: &str, rename: Option<&str>) -> MetadataDependency {
        MetadataDependency {
            name: name.into(),
            rename: rename.map(str::to_string),
            kind: None,
            optional: false,
            target: None,
        }
    }

    fn resolve_dependency(name: &str, package: &str) -> ResolveDependency {
        ResolveDependency {
            name: name.into(),
            pkg: package.into(),
        }
    }

    #[cfg(unix)]
    fn fake_cargo_options(root: &Path, script: &str) -> CargoOptions {
        CargoOptions {
            cargo: Some(executable(root, "fake-cargo", script)),
            ..CargoOptions::default()
        }
    }

    fn test_limits(timeout: Duration, bytes: usize) -> CommandLimits {
        CommandLimits {
            timeout,
            stdout_bytes: bytes,
            stderr_bytes: bytes,
        }
    }

    fn expect_error<T>(result: Result<T, String>, unexpected: &str) -> String {
        match result {
            Ok(_) => panic!("{unexpected}"),
            Err(error) => error,
        }
    }

    fn expect_success<T>(result: Result<T, String>, label: &str) -> T {
        result.unwrap_or_else(|error| panic!("{label}: {error}"))
    }

    fn assert_error_contains(error: &str, expected: &[&str]) {
        for fragment in expected {
            assert!(error.contains(fragment), "{error}");
        }
    }

    fn successful_recording<T>(
        result: Result<T, String>,
        label: &str,
        path: impl AsRef<Path>,
        recording_label: &str,
    ) -> (T, String) {
        (expect_success(result, label), read_file(path, recording_label))
    }

    #[cfg(unix)]
    fn inspect_failed_cargo(
        script: &str,
        timeout: Duration,
        unexpected: &str,
        inspect: impl FnOnce(&Path, &str),
    ) {
        let directory = temporary();
        let options = fake_cargo_options(directory.path(), script);
        let error = expect_error(
            cargo_metadata_with_limits(directory.path(), &options, test_limits(timeout, 128)),
            unexpected,
        );
        inspect(directory.path(), &error);
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
    fn cfg_attr_evaluation_handles_nested_and_empty_forms() {
        let context = SingleCfgContext::synthetic(false);
        let inactive_nested: Attribute = syn::parse_quote!(#[cfg_attr(all(), cfg(any()))]);
        let inactive_predicate: Attribute = syn::parse_quote!(#[cfg_attr(any(), cfg(any()))]);
        let empty: Attribute = syn::parse_quote!(#[cfg_attr()]);

        assert!(!context.meta_attribute_active(&inactive_nested.meta));
        assert!(context.meta_attribute_active(&inactive_predicate.meta));
        assert!(context.meta_attribute_active(&empty.meta));
    }

    #[test]
    fn conventional_modules_distinguish_direct_nested_ambiguous_and_missing_paths() {
        let directory = temporary();
        let module: syn::ItemMod = syn::parse_quote!(
            mod child;
        );

        let direct_dir = directory.path().join("direct");
        create_directory(&direct_dir, "direct directory");
        write_file(&direct_dir.join("child.rs"), "", "direct module");
        assert!(resolve_conventional_module(&module, &direct_dir)
            .unwrap_or_else(|error| panic!("direct resolution: {error}"))
            .path
            .ends_with("child.rs"));

        let nested_dir = directory.path().join("nested");
        create_directory(&nested_dir.join("child"), "nested directory");
        write_file(&nested_dir.join("child/mod.rs"), "", "nested module");
        assert!(resolve_conventional_module(&module, &nested_dir)
            .unwrap_or_else(|error| panic!("nested resolution: {error}"))
            .path
            .ends_with("child/mod.rs"));

        let ambiguous_dir = directory.path().join("ambiguous");
        create_directory(&ambiguous_dir.join("child"), "ambiguous directory");
        write_file(&ambiguous_dir.join("child.rs"), "", "ambiguous direct module");
        write_file(&ambiguous_dir.join("child/mod.rs"), "", "ambiguous nested module");
        assert!(resolve_conventional_module(&module, &ambiguous_dir).is_err());

        let missing_dir = directory.path().join("missing");
        create_directory(&missing_dir, "missing directory");
        assert!(resolve_conventional_module(&module, &missing_dir).is_err());
    }

    #[test]
    fn module_attributes_conservatively_mark_framework_and_reflection_reachability() {
        let cfg: Attribute = syn::parse_quote!(#[cfg(unix)]);
        let macro_use: Attribute = syn::parse_quote!(#[macro_use]);
        let inventory: Attribute = syn::parse_quote!(#[inventory]);
        assert!(!module_framework_managed(&[cfg]));
        assert!(module_framework_managed(&[macro_use]));
        assert!(module_framework_managed(std::slice::from_ref(&inventory)));
        assert!(module_reflection_reachable(&[inventory]));
    }

    #[test]
    fn module_contents_conservatively_preserve_framework_and_macro_modules() {
        let framework: syn::Item = syn::parse_quote!(
            #[actix_web::get("/health")]
            fn health() {}
        );
        let macro_invocation: syn::Item = syn::parse_quote!(register_handler!(health););
        let inert: syn::Item = syn::parse_quote!(
            #[allow(dead_code)]
            fn helper() {}
        );
        assert!(items_reflection_reachable(&[framework]));
        assert!(items_reflection_reachable(&[macro_invocation]));
        assert!(!items_reflection_reachable(&[inert]));
    }

    #[test]
    fn plain_external_module_is_not_framework_or_reflection_managed() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/projects/rust-native")
            .canonicalize()
            .unwrap_or_else(|error| panic!("fixture root: {error}"));
        let discovery = discover_with_semantics(
            &root,
            true,
            &[],
            &CargoOptions::default(),
            TEST_SOURCE_LIMIT,
            false,
        )
        .unwrap_or_else(|error| panic!("discover fixture: {error}"));
        let unused = discovery
            .scopes
            .iter()
            .find(|scope| scope.module_prefix == "unused")
            .unwrap_or_else(|| panic!("unused module scope missing"));
        assert!(!unused.framework_managed);
        assert!(!unused.reflection_reachable);
    }

    #[test]
    fn cargo_resolve_extern_names_override_package_names() {
        let mut app = metadata_package("app 0.1.0", "app", "app");
        app.dependencies = vec![
            metadata_dependency("xml-rs", None),
            metadata_dependency("actual-kit", Some("alias-kit")),
        ];
        let metadata = Metadata {
            packages: vec![
                app,
                metadata_package("xml-rs 0.8.0", "xml-rs", "xml"),
                metadata_package("actual-kit 1.0.0", "actual-kit", "actual_kit"),
            ],
            workspace_members: vec!["app 0.1.0".into()],
            resolve: Some(Resolve {
                nodes: vec![ResolveNode {
                    id: "app 0.1.0".into(),
                    features: Vec::new(),
                    deps: vec![
                        resolve_dependency("xml", "xml-rs 0.8.0"),
                        resolve_dependency("alias_kit", "actual-kit 1.0.0"),
                    ],
                }],
            }),
        };
        let repository = metadata_semantics(Path::new("/workspace/app"), &metadata);
        let names = repository
            .dependencies
            .iter()
            .map(|dependency| {
                (
                    dependency.dependency.as_str(),
                    dependency.source_identifier.as_str(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(names.get("xml-rs"), Some(&"xml"));
        assert_eq!(names.get("actual-kit"), Some(&"alias_kit"));
    }

    #[test]
    fn path_override_and_static_include_preserve_descendant_scopes() {
        let cases = [
            (
                "rust-adapter-path-fixture",
                "#[path=\"foo.rs\"] mod bar;\n",
                [
                    ("foo.rs", "mod baz;\n"),
                    ("baz.rs", "pub fn value() -> bool { true }\n"),
                ],
                "baz.rs",
                None,
            ),
            (
                "rust-adapter-include-fixture",
                "mod outer;\n",
                [
                    ("outer.rs", "include!(\"shared.rs\",);\n"),
                    ("shared.rs", "pub fn shared() {}\n"),
                ],
                "shared.rs",
                Some("outer"),
            ),
        ];
        for (package, root_source, nested_sources, expected_file, expected_prefix) in cases {
            let directory = temporary();
            write_basic_project(directory.path(), package, root_source);
            for (path, source) in nested_sources {
                write_file(&directory.path().join("src").join(path), source, "nested source");
            }

            let files = discovered(directory.path(), &options());
            assert!(files.iter().any(|file| {
                file.path.ends_with(expected_file)
                    && expected_prefix.is_none_or(|prefix| file.module_prefix == prefix)
            }));
        }
    }

    #[test]
    fn same_file_under_two_module_names_keeps_two_scopes() {
        let dir = aliased_module_project("rust-adapter-alias-fixture", "pub fn work() {}\n");

        let files = discovered(dir.path(), &options());
        let prefixes: HashSet<_> = files
            .iter()
            .filter(|file| file.path.ends_with("shared.rs"))
            .map(|file| file.module_prefix.as_str())
            .collect();
        assert_eq!(prefixes, HashSet::from(["alpha", "beta"]));
    }

    #[test]
    fn explicit_feature_changes_active_source_graph() {
        let dir = temporary();
        create_directory(&dir.path().join("src"), "create source");
        write_manifest(
            dir.path(),
            "rust-adapter-feature-fixture",
            "[features]\nextra=[]\n",
        );
        write_lock(dir.path(), "rust-adapter-feature-fixture");
        write_file(
            &dir.path().join("src/lib.rs"),
            "#[cfg(feature=\"extra\")] mod extra;\npub fn base() {}\n",
            "root module",
        );
        write_file(
            &dir.path().join("src/extra.rs"),
            "pub fn enabled() {}\n",
            "feature module",
        );

        let defaults = discovered(dir.path(), &options());
        assert!(!defaults.iter().any(|file| file.path.ends_with("extra.rs")));
        let enabled = discovered(
            dir.path(),
            &CargoOptions {
                features: vec!["extra".into()],
                ..CargoOptions::default()
            },
        );
        assert!(enabled.iter().any(|file| file.path.ends_with("extra.rs")));
    }

    #[test]
    fn path_override_and_static_include_outside_root_are_rejected() {
        for (package, source) in [
            (
                "rust-adapter-outside-path",
                "#[path=\"../../outside.rs\"] mod outside;\n",
            ),
            (
                "rust-adapter-outside-include",
                "include!(\"../../outside.rs\");\n",
            ),
        ] {
            let directory = temporary();
            let project = directory.path().join("project");
            write_basic_project(&project, package, source);
            write_file(
                &directory.path().join("outside.rs"),
                "pub fn secret() {}\n",
                "outside source",
            );

            let error = expect_error(
                discover(&project, false, &[], &options()),
                &format!("outside source was unexpectedly accepted for {package}"),
            );
            assert_error_contains(&error, &["Rust source outside project root", "outside.rs"]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn fake_cargo_receives_literal_safe_bounded_arguments() {
        let directory = temporary();
        let arguments = directory.path().join("arguments");
        let marker = directory.path().join("injected");
        let mut options = fake_cargo_options(
            directory.path(),
            concat!(
                "#!/bin/sh\n",
                "printf '%s\\n' \"$@\" > arguments\n",
                "printf '%s\\n' '{\"packages\":[],\"workspace_members\":[],\"resolve\":{\"nodes\":[]}}'\n",
            ),
        );
        let feature = format!("alpha;touch {}", marker.display());
        options.features = vec![feature.clone()];
        let (_, actual) = successful_recording(
            cargo_metadata_with_limits(
                directory.path(),
                &options,
                test_limits(Duration::from_secs(2), 4096),
            ),
            "metadata",
            arguments,
            "recorded arguments",
        );
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
        inspect_failed_cargo(
            "#!/bin/sh\ni=0; while [ \"$i\" -lt 100 ]; do printf '0123456789'; i=$((i + 1)); done\n",
            Duration::from_secs(10),
            "oversized metadata was unexpectedly accepted",
            |_, error| assert_error_contains(error, &["128-byte stdout limit"]),
        );
    }

    #[cfg(unix)]
    #[test]
    fn fake_cargo_timeout_kills_descendants() {
        inspect_failed_cargo(
            "#!/bin/sh\n(sleep 0.2; printf leaked > leaked) &\nsleep 5\n",
            Duration::from_millis(20),
            "timed-out metadata command unexpectedly succeeded",
            |root, error| {
                assert_error_contains(error, &["timed out after", "process tree was terminated"]);
                thread::sleep(Duration::from_millis(350));
                assert!(!root.join("leaked").exists());
            },
        );
    }

    #[cfg(unix)]
    #[test]
    fn fake_rustc_cfg_preserves_resolved_features_and_argv() {
        let directory = temporary();
        let arguments = directory.path().join("rustc-arguments");
        let rustc = executable(
            directory.path(),
            "fake-rustc",
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > rustc-arguments\nprintf 'unix\\ntarget_os=\"fake\"\\n'\n",
        );
        let package = Package {
            id: "fixture".into(),
            name: "fixture".into(),
            manifest_path: directory.path().join("Cargo.toml"),
            targets: Vec::new(),
            dependencies: Vec::new(),
            features: BTreeMap::new(),
        };
        let target = Target {
            name: "fixture".into(),
            kind: vec!["lib".into()],
            src_path: directory.path().join("src/lib.rs"),
        };
        let (context, actual) = successful_recording(
            rustc_cfg_with_limits(
                RustcCfgRequest {
                    root: directory.path(),
                    package: &package,
                    target: &target,
                    include_tests: false,
                    features: &["alpha".into()],
                },
                rustc.as_os_str(),
                test_limits(Duration::from_secs(2), 4096),
            ),
            "cfg",
            arguments,
            "rustc arguments",
        );
        assert_eq!(actual, "--print\ncfg\n");
        let feature: Attribute = syn::parse_quote!(#[cfg(feature = "alpha")]);
        let unix: Attribute = syn::parse_quote!(#[cfg(unix)]);
        assert!(context.attrs_active(&[feature, unix]));
    }
}
