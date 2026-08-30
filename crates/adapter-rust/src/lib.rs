//! Cargo-aware native Rust adapter for the unified reporigor analysis model.
//!
//! The primary API resolves and analyzes a project in one operation because a
//! plain [`reporigor_core::SourceFile`] cannot retain Cargo `cfg` variants or
//! the multiple module aliases under which one physical Rust file may appear.

mod cargo_proxy;
mod command;
mod complexity;
mod mutations;
mod output;
mod scope;
mod syntax;
#[cfg(test)]
mod test_support;
mod tokens;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use reporigor_core::{
    discover_sources, AnalysisRequest, AnalysisSnapshot, BackendInfo, Capability, CoreError, Diagnostic,
    DiscoveryOptions, FeatureRecord, FileAnalysis, IdentifierCountRecord, Language, ProjectBackend,
    ProjectContext, ProjectKind, RepositorySemantics, Severity, SourceFile, SourceLocation, SyntaxBackend,
};

pub use cargo_proxy::CargoProxy;

const BACKEND_ID: &str = "rust-native";

fn backend_error(message: impl Into<String>) -> CoreError {
    CoreError::Backend {
        backend: BACKEND_ID.into(),
        message: message.into(),
    }
}

/// Cargo feature and executable selection for native Rust project resolution.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CargoOptions {
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    pub cargo: Option<PathBuf>,
}

impl CargoOptions {
    fn validate(&self) -> Result<(), CoreError> {
        if self.all_features_conflict() {
            return Err(CoreError::Config(
                "Cargo --all-features conflicts with --features and --no-default-features".into(),
            ));
        }
        if self.features.iter().any(|feature| feature.trim().is_empty()) {
            return Err(CoreError::Config("Cargo feature names must not be empty".into()));
        }
        Ok(())
    }

    fn all_features_conflict(&self) -> bool {
        self.all_features && (!self.features.is_empty() || self.no_default_features)
    }

    #[must_use]
    pub fn feature_args(&self) -> Vec<OsString> {
        if self.all_features {
            return vec![OsString::from("--all-features")];
        }
        let mut args = Vec::new();
        if self.no_default_features {
            args.push(OsString::from("--no-default-features"));
        }
        if !self.features.is_empty() {
            args.push(OsString::from("--features"));
            args.push(OsString::from(self.features.join(",")));
        }
        args
    }

    pub(crate) fn cargo_program(&self) -> &OsStr {
        self.cargo
            .as_deref()
            .map_or_else(|| OsStr::new("cargo"), Path::as_os_str)
    }
}

#[derive(Debug, Clone)]
struct CachedScope {
    root: PathBuf,
    include_tests: bool,
    filters: Vec<String>,
    max_source_bytes: usize,
    allow_parse_errors: bool,
    scopes: Vec<scope::ScopedFile>,
}

struct ScopedAnalysis {
    file: FileAnalysis,
    repository: RepositorySemantics,
}

#[derive(Clone, Copy)]
struct FileScopeRequest<'a> {
    root: &'a Path,
    source_path: &'a Path,
    source: &'a SourceFile,
    analysis: &'a AnalysisRequest,
}

/// Native Rust backend backed by Cargo, `syn`, and `rustc_lexer`.
#[derive(Debug)]
pub struct RustAdapter {
    options: CargoOptions,
    cache: Mutex<Option<CachedScope>>,
}

impl Default for RustAdapter {
    fn default() -> Self {
        Self::new(CargoOptions::default())
    }
}

impl RustAdapter {
    #[must_use]
    pub fn new(options: CargoOptions) -> Self {
        Self {
            options,
            cache: Mutex::new(None),
        }
    }

    #[must_use]
    pub fn options(&self) -> &CargoOptions {
        &self.options
    }

    /// Resolves only sources active in the selected Cargo workspace, targets,
    /// features, platform configuration, and test mode.
    ///
    /// # Errors
    ///
    /// Returns a configuration, filesystem, Cargo, or Rust parse error when
    /// the selected workspace cannot be resolved completely.
    pub fn resolve_project(&self, request: &AnalysisRequest) -> Result<ProjectContext, CoreError> {
        self.resolve_scoped(request).and_then(|(context, scopes, _)| {
            self.store_cache(&context.root, request, scopes).map(|()| context)
        })
    }

    /// Resolves and analyzes the Cargo project without losing adapter-private
    /// cfg and module-prefix information between the two phases.
    ///
    /// # Errors
    ///
    /// Returns a configuration, filesystem, Cargo, or Rust parse error when
    /// the selected workspace cannot be resolved and analyzed completely.
    pub fn analyze_project(&self, request: &AnalysisRequest) -> Result<AnalysisSnapshot, CoreError> {
        let (context, scopes, repository) = self.resolve_scoped(request)?;
        self.store_cache(&context.root, request, scopes.clone())?;
        let grouped = group_scopes(scopes);
        let mut snapshot = AnalysisSnapshot::default();
        let mut structural_inventory_complete = true;
        snapshot.merge_repository_semantics(repository);
        for source in &context.sources {
            let analysis = Self::analyze_grouped_source(&context.root, source, &grouped, request)?;
            structural_inventory_complete &= analysis.file.parse_errors == 0;
            snapshot.push(analysis.file);
            snapshot.merge_repository_semantics(analysis.repository);
        }
        canonicalize_rust_repository(&mut snapshot.repository);
        snapshot.repository.test_inventory_reliable &= request.include_tests;
        constrain_inventory_reliability(&mut snapshot.repository, structural_inventory_complete);
        snapshot.assign_mutation_ids();
        Ok(snapshot)
    }

    fn analyze_grouped_source(
        root: &Path,
        source: &SourceFile,
        grouped: &BTreeMap<PathBuf, Vec<scope::ScopedFile>>,
        request: &AnalysisRequest,
    ) -> Result<ScopedAnalysis, CoreError> {
        let canonical = source.path.canonicalize().map_err(|error| CoreError::Read {
            path: source.path.display().to_string(),
            source: error,
        })?;
        let file_scopes = grouped.get(&canonical).ok_or_else(|| {
            backend_error(format!(
                "resolved source has no retained Cargo scope: {}",
                source.path.display()
            ))
        })?;
        Self::analyze_scoped_file(root, source, file_scopes, request)
    }

    fn resolve_scoped(
        &self,
        request: &AnalysisRequest,
    ) -> Result<(ProjectContext, Vec<scope::ScopedFile>, RepositorySemantics), CoreError> {
        self.options.validate()?;
        let root = validated_project_root(request)?;
        let rust_requested = request.languages.is_empty() || request.languages.contains(&Language::Rust);
        let discovery = self.selected_discovery(&root, request, rust_requested)?;
        let scopes = discovery.scopes;
        let sources = unique_sources(&root, &scopes);
        let diagnostics = empty_source_diagnostics(sources.is_empty(), rust_requested);
        Ok((
            ProjectContext {
                root,
                kinds: BTreeSet::from([ProjectKind::Cargo]),
                sources,
                backends: vec![backend_info()],
                diagnostics,
            },
            scopes,
            discovery.repository,
        ))
    }

    fn selected_discovery(
        &self,
        root: &Path,
        request: &AnalysisRequest,
        rust_requested: bool,
    ) -> Result<scope::ScopeDiscovery, CoreError> {
        if !rust_requested {
            return Ok(scope::ScopeDiscovery {
                scopes: Vec::new(),
                repository: RepositorySemantics::default(),
            });
        }
        validate_rust_source_budget(root, request)?;
        self.scope_discovery(root, request)
    }

    fn scope_discovery(
        &self,
        root: &Path,
        request: &AnalysisRequest,
    ) -> Result<scope::ScopeDiscovery, CoreError> {
        scope::discover_with_semantics(
            root,
            request.include_tests,
            &request.filters,
            &self.options,
            request.max_source_bytes,
            request.allow_parse_errors,
        )
        .map_err(backend_error)
    }

    fn store_cache(
        &self,
        root: &Path,
        request: &AnalysisRequest,
        scopes: Vec<scope::ScopedFile>,
    ) -> Result<(), CoreError> {
        let mut cache = self.lock_cache()?;
        *cache = Some(CachedScope {
            root: root.to_path_buf(),
            include_tests: request.include_tests,
            filters: request.filters.clone(),
            max_source_bytes: request.max_source_bytes,
            allow_parse_errors: request.allow_parse_errors,
            scopes,
        });
        Ok(())
    }

    fn lock_cache(&self) -> Result<MutexGuard<'_, Option<CachedScope>>, CoreError> {
        self.cache
            .lock()
            .map_err(|_| backend_error("Rust scope cache lock was poisoned"))
    }

    fn scopes_for_file(
        &self,
        root: &Path,
        source: &SourceFile,
        request: &AnalysisRequest,
    ) -> Result<Vec<scope::ScopedFile>, CoreError> {
        let (root, source_path) = canonical_scope_paths(root, source)?;
        validate_rust_source_budget(&root, request)?;
        if let Some(found) = self.cached_file_scopes(&root, &source_path, request)? {
            return Ok(found);
        }
        self.fresh_file_scopes(FileScopeRequest {
            root: &root,
            source_path: &source_path,
            source,
            analysis: request,
        })
    }

    fn fresh_file_scopes(&self, request: FileScopeRequest<'_>) -> Result<Vec<scope::ScopedFile>, CoreError> {
        let scopes = self.scope_discovery(request.root, request.analysis)?.scopes;
        let found = matching_scopes(&scopes, request.source_path);
        self.store_cache(request.root, request.analysis, scopes)?;
        if found.is_empty() {
            Err(backend_error(format!(
                "{} is not active in the selected Cargo scope",
                request.source.path.display()
            )))
        } else {
            Ok(found)
        }
    }

    fn cached_file_scopes(
        &self,
        root: &Path,
        source_path: &Path,
        request: &AnalysisRequest,
    ) -> Result<Option<Vec<scope::ScopedFile>>, CoreError> {
        let cache = self.lock_cache()?;
        let found = cache
            .as_ref()
            .filter(|cached| cache_matches(cached, root, request))
            .map(|cached| matching_scopes(&cached.scopes, source_path))
            .filter(|found| !found.is_empty());
        Ok(found)
    }

    fn analyze_scoped_file(
        _root: &Path,
        source_file: &SourceFile,
        scopes: &[scope::ScopedFile],
        request: &AnalysisRequest,
    ) -> Result<ScopedAnalysis, CoreError> {
        let source = read_scoped_source(source_file, request.max_source_bytes)?;
        let syntax = match parse_scoped_source(source_file, &source, request.allow_parse_errors)? {
            ParsedSource::Syntax(syntax) => syntax,
            ParsedSource::Fallback(analysis) => return Ok(*analysis),
        };
        let merged_cfg = scope::CfgContext::merged(scopes.iter().map(|scoped| &scoped.cfg));
        let structural = complexity::extract(&syntax, &source, &source_file.relative, scopes);
        Ok(ScopedAnalysis {
            file: FileAnalysis {
                source: source_file.clone(),
                backend: backend_info(),
                functions: structural.functions,
                tokens: tokens::normalize(&syntax, &source, &merged_cfg),
                mutations: mutations::enumerate(&syntax, &source, &source_file.relative, &merged_cfg),
                diagnostics: Vec::new(),
                parse_errors: 0,
            },
            repository: structural.repository,
        })
    }
}

enum ParsedSource {
    Syntax(syn::File),
    Fallback(Box<ScopedAnalysis>),
}

fn validated_project_root(request: &AnalysisRequest) -> Result<PathBuf, CoreError> {
    let root = reporigor_core::canonical_directory(&request.root)?;
    ensure_cargo_manifest(&root)?;
    Ok(root)
}

fn ensure_cargo_manifest(root: &Path) -> Result<(), CoreError> {
    if !root.join("Cargo.toml").is_file() {
        return Err(CoreError::BackendUnavailable {
            backend: BACKEND_ID.into(),
            message: format!("{} does not contain Cargo.toml", root.display()),
        });
    }
    Ok(())
}

fn empty_source_diagnostics(empty: bool, rust_requested: bool) -> Vec<Diagnostic> {
    if !empty || !rust_requested {
        return Vec::new();
    }
    vec![Diagnostic {
        severity: Severity::Warning,
        backend: BACKEND_ID.into(),
        message: "Cargo resolved no active Rust source files".into(),
        location: None,
        fallback_used: false,
    }]
}

fn constrain_inventory_reliability(repository: &mut RepositorySemantics, structural_complete: bool) {
    if repository.module_graph_reliable && structural_complete {
        return;
    }
    repository.module_graph_reliable &= structural_complete;
    repository.identifier_counts_reliable = false;
    repository.feature_inventory_reliable = false;
    repository.trait_inventory_reliable = false;
    repository.test_inventory_reliable = false;
    repository.unreachable_inventory_reliable &= structural_complete;
}

fn canonical_scope_paths(root: &Path, source: &SourceFile) -> Result<(PathBuf, PathBuf), CoreError> {
    let root = canonical_path(root)?;
    let source_path = canonical_path(&source.path)?;
    Ok((root, source_path))
}

fn canonical_path(path: &Path) -> Result<PathBuf, CoreError> {
    path.canonicalize().map_err(|source| CoreError::Read {
        path: path.display().to_string(),
        source,
    })
}

fn cache_matches(cached: &CachedScope, root: &Path, request: &AnalysisRequest) -> bool {
    cached.root == root
        && cached.include_tests == request.include_tests
        && cached.filters == request.filters
        && cached.max_source_bytes == request.max_source_bytes
        && cached.allow_parse_errors == request.allow_parse_errors
}

fn matching_scopes(scopes: &[scope::ScopedFile], source_path: &Path) -> Vec<scope::ScopedFile> {
    scopes
        .iter()
        .filter(|scoped| scoped.path == source_path)
        .cloned()
        .collect()
}

fn read_scoped_source(source: &SourceFile, limit: usize) -> Result<String, CoreError> {
    let bounded = scope::read_source_bounded(&source.path, limit).map_err(|message| CoreError::Backend {
        backend: BACKEND_ID.into(),
        message,
    })?;
    match bounded {
        scope::BoundedSource::Content(source) => Ok(source),
        scope::BoundedSource::TooLarge { actual_bytes } => {
            Err(CoreError::source_too_large(&source.path, actual_bytes, limit))
        }
    }
}

fn parse_scoped_source(
    source_file: &SourceFile,
    source: &str,
    allow_parse_errors: bool,
) -> Result<ParsedSource, CoreError> {
    match syn::parse_file(source) {
        Ok(syntax) => Ok(ParsedSource::Syntax(syntax)),
        Err(error) if allow_parse_errors => Ok(ParsedSource::Fallback(Box::new(parse_fallback(
            source_file,
            source,
            &error,
        )))),
        Err(error) => Err(CoreError::Parse {
            path: source_file.path.display().to_string(),
            message: error.to_string(),
        }),
    }
}

fn parse_fallback(source_file: &SourceFile, source: &str, error: &syn::Error) -> ScopedAnalysis {
    let range = error.span().byte_range();
    ScopedAnalysis {
        file: FileAnalysis {
            source: source_file.clone(),
            backend: backend_info(),
            functions: Vec::new(),
            tokens: Vec::new(),
            mutations: Vec::new(),
            diagnostics: vec![Diagnostic {
                severity: Severity::Error,
                backend: BACKEND_ID.into(),
                message: format!(
                    "native Rust parse failed; generic valid-subtree fallback is required: {error}"
                ),
                location: parse_error_location(source_file, source, range),
                fallback_used: true,
            }],
            parse_errors: 1,
        },
        repository: RepositorySemantics::default(),
    }
}

fn parse_error_location(
    source_file: &SourceFile,
    source: &str,
    range: std::ops::Range<usize>,
) -> Option<SourceLocation> {
    match (
        scalar_position(source, range.start),
        scalar_position(source, range.end),
    ) {
        (Some((start_line, start_column)), Some((end_line, end_column))) => Some(SourceLocation {
            file: source_file.relative.clone(),
            start_line,
            start_column,
            end_line,
            end_column,
        }),
        _ => None,
    }
}

fn validate_rust_source_budget(root: &Path, request: &AnalysisRequest) -> Result<(), CoreError> {
    discover_sources(
        root,
        &DiscoveryOptions {
            languages: BTreeSet::from([Language::Rust]),
            filters: request.filters.clone(),
            include_tests: request.include_tests,
            max_source_bytes: request.max_source_bytes,
        },
    )?;
    Ok(())
}

pub(crate) fn scalar_position(source: &str, byte_offset: usize) -> Option<(u32, u32)> {
    let prefix = source.get(..byte_offset)?;
    let line_start = line_start(prefix)?;
    let line = one_based_u32(prefix.bytes().filter(|byte| *byte == b'\n').count())?;
    let column = one_based_u32(prefix[line_start..].chars().count())?;
    Some((line, column))
}

fn line_start(prefix: &str) -> Option<usize> {
    prefix.rfind('\n').map_or(Some(0), |index| index.checked_add(1))
}

fn one_based_u32(zero_based: usize) -> Option<u32> {
    zero_based.checked_add(1).and_then(|value| value.try_into().ok())
}

type FileAnalysisResult = Result<FileAnalysis, CoreError>;

impl SyntaxBackend for RustAdapter {
    fn analyze_file(
        &self,
        root: &Path,
        source: &SourceFile,
        request: &AnalysisRequest,
    ) -> FileAnalysisResult {
        ensure_rust_source(source)
            .and_then(|()| self.scopes_for_file(root, source, request))
            .and_then(|scopes| Self::analyze_scoped_file(root, source, &scopes, request))
            .map(|analysis| analysis.file)
    }

    fn supports(&self, language: Language) -> bool {
        language == Language::Rust
    }

    fn info(&self) -> BackendInfo {
        backend_info()
    }
}

fn ensure_rust_source(source: &SourceFile) -> Result<(), CoreError> {
    if source.language == Language::Rust {
        return Ok(());
    }
    Err(CoreError::BackendUnavailable {
        backend: BACKEND_ID.into(),
        message: format!("Rust adapter cannot analyze {}", source.language),
    })
}

impl ProjectBackend for RustAdapter {
    fn info(&self) -> BackendInfo {
        backend_info()
    }

    fn supports(&self, project: ProjectKind) -> bool {
        project == ProjectKind::Cargo
    }

    fn resolve(&self, request: &AnalysisRequest) -> Result<ProjectContext, CoreError> {
        self.resolve_project(request)
    }
}

fn backend_info() -> BackendInfo {
    BackendInfo::new(
        BACKEND_ID,
        env!("CARGO_PKG_VERSION"),
        true,
        [
            Capability::Syntax,
            Capability::Functions,
            Capability::Complexity,
            Capability::Tokens,
            Capability::Mutations,
            Capability::ProjectSemantics,
            Capability::ParseValidation,
        ],
    )
}

fn group_scopes(scopes: Vec<scope::ScopedFile>) -> BTreeMap<PathBuf, Vec<scope::ScopedFile>> {
    let mut grouped = BTreeMap::new();
    for scoped in scopes {
        grouped
            .entry(scoped.path.clone())
            .or_insert_with(Vec::new)
            .push(scoped);
    }
    grouped
}

fn canonicalize_rust_repository(repository: &mut RepositorySemantics) {
    let mut identifiers = BTreeMap::<(Option<String>, String), (u32, u32)>::new();
    for record in std::mem::take(&mut repository.identifiers) {
        let counts = identifiers
            .entry((record.package, record.identifier))
            .or_default();
        counts.0 = counts.0.saturating_add(record.production_references);
        counts.1 = counts.1.saturating_add(record.test_references);
    }
    repository.identifiers = identifiers
        .into_iter()
        .map(
            |((package, identifier), (production_references, test_references))| IdentifierCountRecord {
                identifier,
                package,
                production_references,
                test_references,
            },
        )
        .collect();

    let mut features = BTreeMap::<(String, String), FeatureRecord>::new();
    for record in std::mem::take(&mut repository.features) {
        let key = (record.package.clone(), record.name.clone());
        if let Some(existing) = features.get_mut(&key) {
            existing.references = existing.references.saturating_add(record.references);
            existing.enables.extend(record.enables);
            existing.target_gated &= record.target_gated;
        } else {
            features.insert(key, record);
        }
    }
    repository.features = features.into_values().collect();

    populate_module_references(repository);
    enrich_contract_test_references(repository);
    repository.canonicalize();
}

fn populate_module_references(repository: &mut RepositorySemantics) {
    let declarations = module_declarations(repository);
    let identifiers = &repository.identifiers;
    for module in &mut repository.modules {
        populate_module_reference(module, identifiers, &declarations);
    }
}

fn module_declarations(repository: &RepositorySemantics) -> BTreeMap<(String, String), usize> {
    let mut declarations = BTreeMap::<(String, String), usize>::new();
    for module in &repository.modules {
        let Some(package) = module.package.as_ref() else {
            continue;
        };
        let name = module_leaf(&module.stable_symbol);
        if name != *package {
            *declarations.entry((package.clone(), name)).or_default() += 1;
        }
    }
    declarations
}

fn populate_module_reference(
    module: &mut reporigor_core::ModuleRecord,
    identifiers: &[IdentifierCountRecord],
    declarations: &BTreeMap<(String, String), usize>,
) {
    let Some(package) = module.package.as_ref() else {
        module.references = 1;
        return;
    };
    let name = module_leaf(&module.stable_symbol);
    if name == *package {
        module.externally_invoked = true;
        module.references = 1;
        return;
    }
    let key = (package.clone(), name.clone());
    if declarations.get(&key) != Some(&1) {
        module.references = 1;
        return;
    }
    module.references = identifiers
        .iter()
        .find(|record| record.package.as_ref() == Some(package) && record.identifier == name)
        .map_or(1, |record| record.production_references.saturating_sub(1));
}

fn module_leaf(symbol: &str) -> String {
    symbol
        .split("[cfg:")
        .next()
        .unwrap_or(symbol)
        .rsplit("::")
        .next()
        .unwrap_or(symbol)
        .to_string()
}

fn enrich_contract_test_references(repository: &mut RepositorySemantics) {
    let implementations = repository.trait_implementations.clone();
    for test in &mut repository.tests {
        let package = test.package.as_deref();
        let candidates = implementations
            .iter()
            .filter(|implementation| implementation.package.as_deref() == package)
            .collect::<Vec<_>>();
        let raw_references = test.referenced_symbols.clone();
        for raw in raw_references {
            let trait_symbols = best_matching_symbols(
                &raw,
                package,
                &test.stable_symbol,
                candidates
                    .iter()
                    .map(|implementation| implementation.trait_symbol.as_str()),
            );
            if trait_symbols.len() == 1 {
                test.referenced_symbols.insert(trait_symbols[0].to_string());
            }
            let implementation_symbols = best_matching_symbols(
                &raw,
                package,
                &test.stable_symbol,
                candidates
                    .iter()
                    .map(|implementation| implementation.implementation_symbol.as_str()),
            );
            if implementation_symbols.len() == 1 {
                test.referenced_symbols
                    .insert(implementation_symbols[0].to_string());
            }
        }
    }
}

fn best_matching_symbols<'a>(
    reference: &str,
    package: Option<&str>,
    context: &str,
    symbols: impl IntoIterator<Item = &'a str>,
) -> Vec<&'a str> {
    let mut scores = BTreeMap::<&str, u8>::new();
    for symbol in symbols {
        let score = symbol_match_score(reference, symbol, package, context);
        if score > 0 {
            scores
                .entry(symbol)
                .and_modify(|existing| *existing = (*existing).max(score))
                .or_insert(score);
        }
    }
    let maximum = scores.values().copied().max().unwrap_or(0);
    scores
        .into_iter()
        .filter_map(|(symbol, score)| (score == maximum).then_some(symbol))
        .collect()
}

fn symbol_match_score(reference: &str, symbol: &str, package: Option<&str>, context: &str) -> u8 {
    let reference = symbol_segments(reference);
    let mut symbol = symbol_segments(symbol);
    let mut context = symbol_segments(context);
    strip_package_prefixes(&mut symbol, &mut context, package);
    let reference = reference
        .into_iter()
        .skip_while(|part| matches!(part.as_str(), "crate" | "self" | "super"))
        .collect::<Vec<_>>();
    if symbol_match_impossible(&reference, &symbol) {
        return 0;
    }
    let base = base_symbol_match(&reference, &symbol);
    let owner = &symbol[..symbol.len().saturating_sub(1)];
    let lexical = !owner.is_empty() && context.starts_with(owner);
    base.saturating_add(u8::from(lexical).saturating_mul(2))
}

fn strip_package_prefixes(symbol: &mut Vec<String>, context: &mut Vec<String>, package: Option<&str>) {
    let Some(package) = package.map(symbol_segments) else {
        return;
    };
    strip_prefix(symbol, &package);
    strip_prefix(context, &package);
}

fn strip_prefix(parts: &mut Vec<String>, prefix: &[String]) {
    if parts.starts_with(prefix) {
        parts.drain(..prefix.len());
    }
}

fn symbol_match_impossible(reference: &[String], symbol: &[String]) -> bool {
    reference.is_empty() || symbol.is_empty()
}

fn base_symbol_match(reference: &[String], symbol: &[String]) -> u8 {
    if exact_symbol_match(reference, symbol) {
        return 3;
    }
    let leaf = symbol.last().map(String::as_str).unwrap_or_default();
    u8::from(reference.iter().any(|part| part == leaf))
}

fn exact_symbol_match(reference: &[String], symbol: &[String]) -> bool {
    reference == symbol
        || reference.ends_with(symbol)
        || reference.windows(symbol.len()).any(|window| window == symbol)
}

fn symbol_segments(value: &str) -> Vec<String> {
    value
        .split("[cfg:")
        .next()
        .unwrap_or(value)
        .split('<')
        .next()
        .unwrap_or(value)
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn unique_sources(root: &Path, scopes: &[scope::ScopedFile]) -> Vec<SourceFile> {
    let mut paths = BTreeSet::new();
    for scoped in scopes {
        paths.insert(scoped.path.clone());
    }
    paths
        .into_iter()
        .map(|path| {
            let relative = relative(root, &path);
            SourceFile {
                test: Language::Rust.is_test_path(&relative),
                generated: relative
                    .split('/')
                    .any(|part| matches!(part, "generated" | "gen")),
                path,
                relative,
                language: Language::Rust,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::test_support::{aliased_module_project, project, project_with_file, required};

    #[test]
    fn cargo_feature_arguments_follow_cargo_rules() {
        assert_eq!(
            CargoOptions {
                features: vec!["alpha".into(), "beta".into()],
                no_default_features: true,
                ..CargoOptions::default()
            }
            .feature_args(),
            [
                OsString::from("--no-default-features"),
                OsString::from("--features"),
                OsString::from("alpha,beta")
            ]
        );
        assert_eq!(
            CargoOptions {
                all_features: true,
                ..CargoOptions::default()
            }
            .feature_args(),
            [OsString::from("--all-features")]
        );
        let invalid = [
            CargoOptions {
                all_features: true,
                features: vec!["extra".into()],
                ..CargoOptions::default()
            },
            CargoOptions {
                features: vec!["  ".into()],
                ..CargoOptions::default()
            },
        ];
        assert!(invalid.iter().all(|options| options.validate().is_err()));
    }

    #[test]
    fn end_to_end_analysis_retains_aliases_and_normalized_records() {
        let dir = aliased_module_project(
            "rust-adapter-e2e",
            "pub fn choose(a: bool, b: bool) -> bool { if a && b { true } else { false } }\n",
        );

        let adapter = RustAdapter::default();
        let snapshot = adapter
            .analyze_project(&AnalysisRequest::new(dir.path().to_path_buf()))
            .unwrap_or_else(|error| panic!("analysis: {error}"));
        assert_eq!(snapshot.files.len(), 2);
        assert!(snapshot.functions.iter().any(|item| item.name == "alpha::choose"));
        assert!(snapshot.functions.iter().any(|item| item.name == "beta::choose"));
        assert!(snapshot.tokens["src/shared.rs"]
            .iter()
            .any(|token| token.value == "choose"));
        assert!(snapshot.mutations.iter().any(test_support::is_logical_flip));
        assert!(snapshot.mutations.windows(2).all(|pair| pair[0].id < pair[1].id));
        assert!(!snapshot.repository.test_inventory_reliable);

        let source = snapshot
            .files
            .iter()
            .find(|source| source.relative == "src/lib.rs")
            .cloned()
            .unwrap_or_else(|| panic!("root source"));
        let request = AnalysisRequest::new(dir.path().to_path_buf());
        let cached = SyntaxBackend::analyze_file(&adapter, dir.path(), &source, &request)
            .unwrap_or_else(|error| panic!("cached file analysis: {error}"));
        let uncached = SyntaxBackend::analyze_file(&RustAdapter::default(), dir.path(), &source, &request)
            .unwrap_or_else(|error| panic!("uncached file analysis: {error}"));
        assert_eq!(cached.functions, uncached.functions);
        assert_eq!(cached.tokens, uncached.tokens);
        assert_eq!(cached.mutations, uncached.mutations);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn contract_tests_resolve_exact_adapter_symbols_without_covering_missing_implementations() {
        let dir = project_with_file(
            "contract-fixture",
            r#"
#[path = "generated/hidden.rs"]
mod generated_hidden;

pub trait Contract { fn value(&self) -> i32; }
pub struct Covered;
pub struct Missing;
impl Contract for Covered { fn value(&self) -> i32 { 1 } }
impl Contract for Missing { fn value(&self) -> i32 { 2 } }

#[cfg(unix)]
pub fn platform_hook() {}

#[cfg(not(test))]
fn production_only() {}

#[cfg(test)]
fn test_only() {}

#[cfg(test)]
mod tests {
    use super::{Contract, Covered};

    fn helper() {}

    #[test]
    #[reporigor_contract]
    fn covered_contract() {
        helper();
        let covered = Covered;
        let _ = <Covered as Contract>::value(&covered);
    }
}
"#,
            "src/generated/hidden.rs",
            "pub fn generated_function() {}\n",
        );

        let adapter = RustAdapter::default();
        let mut request = AnalysisRequest::new(dir.path().to_path_buf());
        request.include_tests = true;
        let snapshot = adapter
            .analyze_project(&request)
            .unwrap_or_else(|error| panic!("analysis: {error}"));
        assert!(snapshot.repository.test_inventory_reliable);

        let implementations = &snapshot.repository.trait_implementations;
        let covered = required(implementations, "covered implementation", |implementation| {
            implementation.implementation_symbol.ends_with("::Covered")
        });
        let missing = required(implementations, "missing implementation", |implementation| {
            implementation.implementation_symbol.ends_with("::Missing")
        });
        assert_eq!(covered.implementation_symbol, "contract-fixture::Covered");
        assert_eq!(covered.trait_symbol, "contract-fixture::Contract");
        let contract = required(&snapshot.repository.tests, "marked contract test", |test| {
            test.markers.contains("reporigor_contract")
        });

        assert!(
            !contract.target_gated,
            "cfg(test) is a test dimension, not a target gate"
        );
        assert!(contract.referenced_symbols.contains(&covered.trait_symbol));
        assert!(contract
            .referenced_symbols
            .contains(&covered.implementation_symbol));
        assert!(!contract
            .referenced_symbols
            .contains(&missing.implementation_symbol));
        let find_function = |suffix: &str| {
            required(&snapshot.functions, suffix, |function| {
                function.name.ends_with(suffix)
            })
        };
        assert!(find_function("platform_hook").entry_point);
        let helper = find_function("tests::helper");
        assert!(helper.entry_point);
        assert!(helper.structural_metrics_reliable && !helper.production);
        assert!(find_function("production_only").production);
        assert!(!find_function("test_only").production);
        let helper_references = required(
            &snapshot.repository.identifiers,
            "helper identifier inventory",
            |record| record.identifier == "helper",
        );
        assert_eq!(helper_references.production_references, 0);
        assert!(helper_references.test_references > 0);
        assert!(!snapshot
            .functions
            .iter()
            .any(|function| function.name.contains("generated_function")));
        assert!(!snapshot
            .repository
            .modules
            .iter()
            .any(|module| module.stable_symbol.contains("generated_hidden")));
    }

    #[test]
    fn oversized_sparse_source_is_not_read_or_parsed_during_discovery() {
        let dir = project("rust-adapter-large", "pub fn small_prefix() {}\n");
        let source_path = dir.path().join("src/lib.rs");
        let sparse = fs::OpenOptions::new()
            .write(true)
            .open(&source_path)
            .unwrap_or_else(|error| panic!("sparse source: {error}"));
        sparse
            .set_len(16 * 1024 * 1024)
            .unwrap_or_else(|error| panic!("sparse length: {error}"));

        let mut request = AnalysisRequest::new(dir.path().to_path_buf());
        request.max_source_bytes = 64;
        let Err(error) = RustAdapter::default().analyze_project(&request) else {
            panic!("oversized source was unexpectedly analyzed");
        };
        let CoreError::SourceTooLarge {
            actual_bytes,
            max_source_bytes,
            ..
        } = error
        else {
            panic!("unexpected oversized-source error: {error}");
        };
        assert_eq!((actual_bytes, max_source_bytes), (16_777_216, 64));
    }

    #[test]
    fn permissive_parse_failure_marks_file_for_generic_fallback() {
        let malformed = "const EMOJI: &str = \"😀\"; @ pub fn valid() -> bool { true }\n";
        let dir = project("rust-adapter-malformed", malformed);

        let mut request = AnalysisRequest::new(dir.path().to_path_buf());
        request.allow_parse_errors = true;
        let snapshot = RustAdapter::default()
            .analyze_project(&request)
            .unwrap_or_else(|error| panic!("analysis: {error}"));
        let diagnostic = snapshot
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.backend == BACKEND_ID && diagnostic.fallback_used)
            .unwrap_or_else(|| panic!("missing fallback marker: {:?}", snapshot.diagnostics));
        assert!(diagnostic
            .message
            .contains("generic valid-subtree fallback is required"));
        assert_eq!(
            diagnostic
                .location
                .as_ref()
                .map(|location| location.file.as_str()),
            Some("src/lib.rs")
        );
        let Some(error_offset) = malformed.find('@') else {
            panic!("fixture parse-error token must exist");
        };
        let expected_column =
            u32::try_from(malformed[..error_offset].chars().count() + 1).unwrap_or(u32::MAX);
        assert_eq!(
            diagnostic
                .location
                .as_ref()
                .map(|location| (location.start_line, location.start_column)),
            Some((1, expected_column))
        );
        assert_ne!(
            expected_column,
            u32::try_from(error_offset + 1).unwrap_or(u32::MAX)
        );
        assert_eq!(snapshot.parse_errors, 1);
        assert!(snapshot.functions.is_empty());
    }
}
