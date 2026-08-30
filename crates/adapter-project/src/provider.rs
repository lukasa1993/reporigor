use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    time::Duration,
};

use reporigor_core::{
    AnalysisRequest, BackendCapabilities, BackendInfo, BackendPreference, Capability, CoreError, Diagnostic,
    Language, ProjectBackend, ProjectContext, ProjectKind, Severity,
};
use serde::{Deserialize, Serialize};

use crate::command::{CommandRunner, ProviderCommand, ProviderCommandOutput, SystemCommandRunner};
use crate::metadata::{dialect_names, discover_bash_dialects, ProjectMetadata};

const TYPESCRIPT_ID: &str = "typescript";
const SWIFTPM_ID: &str = "swiftpm";
const PYTHON_ID: &str = "python";
const BASH_ID: &str = "bash";
const SHELLCHECK_ID: &str = "shellcheck";

#[derive(Clone, Copy)]
enum TypeScriptQuery {
    Configuration,
    Files,
}

impl TypeScriptQuery {
    const fn command(self) -> (&'static str, &'static str) {
        match self {
            Self::Configuration => ("--showConfig", "configuration"),
            Self::Files => ("--listFilesOnly", "file listing"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderOptions {
    pub typescript_tsc: Option<PathBuf>,
    pub swift: Option<PathBuf>,
    pub python: Option<PathBuf>,
    pub bash: Option<PathBuf>,
    pub shellcheck: Option<PathBuf>,
    pub command_timeout: Duration,
}

impl Default for ProviderOptions {
    fn default() -> Self {
        Self {
            typescript_tsc: None,
            swift: None,
            python: None,
            bash: None,
            shellcheck: None,
            command_timeout: Duration::from_secs(15),
        }
    }
}

/// A stable inventory row suitable for a `reporigor providers` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub id: String,
    pub project: ProjectKind,
    pub capabilities: BackendCapabilities,
    pub applicable: bool,
    pub available: bool,
    #[serde(default = "required_for_native_by_default")]
    pub required_for_native: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

const fn required_for_native_by_default() -> bool {
    true
}

impl ProviderStatus {
    #[must_use]
    pub fn backend_info(&self) -> BackendInfo {
        BackendInfo {
            id: format!("project-{}", self.id),
            version: self
                .version
                .clone()
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
            native: true,
            capabilities: self.capabilities.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProvenance {
    pub id: String,
    pub backend: BackendInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderResolution {
    pub context: ProjectContext,
    pub inventory: Vec<ProviderStatus>,
    pub provenance: Vec<ProviderProvenance>,
}

/// Return a deterministic provider inventory without executing any commands.
#[must_use]
pub fn providers(root: &Path) -> Vec<ProviderStatus> {
    providers_with_options(root, &ProviderOptions::default())
}

/// Inventory variant with explicit executable overrides.
#[must_use]
pub fn providers_with_options(root: &Path, options: &ProviderOptions) -> Vec<ProviderStatus> {
    let request = AnalysisRequest::new(root.to_path_buf());
    match ProjectContext::discover(&request) {
        Ok(mut context) => {
            let metadata = ProjectMetadata::discover(&context.root);
            enrich_project_kinds(&mut context, &metadata);
            build_inventory(&context, &request, options, &metadata)
        }
        Err(error) => unavailable_inventory(&error.to_string()),
    }
}

#[derive(Debug, Clone)]
pub struct ProjectAdapter<R = SystemCommandRunner> {
    options: ProviderOptions,
    runner: R,
}

impl Default for ProjectAdapter<SystemCommandRunner> {
    fn default() -> Self {
        Self {
            options: ProviderOptions::default(),
            runner: SystemCommandRunner,
        }
    }
}

impl ProjectAdapter<SystemCommandRunner> {
    #[must_use]
    pub fn new(options: ProviderOptions) -> Self {
        Self {
            options,
            runner: SystemCommandRunner,
        }
    }
}

impl<R> ProjectAdapter<R> {
    #[must_use]
    pub const fn with_runner(options: ProviderOptions, runner: R) -> Self {
        Self { options, runner }
    }

    #[must_use]
    pub fn options(&self) -> &ProviderOptions {
        &self.options
    }
}

impl<R: CommandRunner> ProjectAdapter<R> {
    /// Discover project providers and metadata without executing subprocesses.
    ///
    /// # Errors
    ///
    /// Returns an error when the project root cannot be read or source
    /// discovery fails.
    pub fn discover(&self, request: &AnalysisRequest) -> Result<ProviderResolution, CoreError> {
        let mut context = ProjectContext::discover(request)?;
        let metadata = ProjectMetadata::discover(&context.root);
        context.diagnostics.extend(metadata.diagnostics.iter().cloned());
        enrich_project_kinds(&mut context, &metadata);
        let inventory = build_inventory(&context, request, &self.options, &metadata);
        let mut resolution = ProviderResolution {
            provenance: build_provenance(&context, &inventory, &metadata),
            inventory,
            context,
        };
        sync_context(&mut resolution, false, request.backend);
        Ok(resolution)
    }

    /// Explicitly execute read-only toolchain probes and project-description
    /// commands. No discovery or trait-based `resolve` call reaches this path.
    ///
    /// # Errors
    ///
    /// Returns an error when initial filesystem discovery fails. Expected
    /// missing, failing, or timed-out providers are recorded in inventory and
    /// diagnostics so one failed optional provider does not abort the report.
    pub fn preflight(&self, request: &AnalysisRequest) -> Result<ProviderResolution, CoreError> {
        let mut resolution = self.discover(request)?;
        self.preflight_typescript(&mut resolution);
        self.preflight_swift(&mut resolution);
        self.preflight_python(&mut resolution);
        self.preflight_bash(&mut resolution);
        self.preflight_shellcheck(&mut resolution);
        sync_context(&mut resolution, true, request.backend);
        Ok(resolution)
    }

    fn preflight_typescript(&self, resolution: &mut ProviderResolution) {
        let Some(executable) = available_provider_executable(resolution, TYPESCRIPT_ID) else {
            return;
        };
        if !self.prepare_typescript(resolution, &executable) {
            return;
        }
        let Some(config) = required_typescript_config(resolution) else {
            return;
        };
        self.resolve_typescript_project(resolution, &executable, &config);
    }

    fn prepare_typescript(&self, resolution: &mut ProviderResolution, executable: &Path) -> bool {
        let Some(version) = self.run_version(
            resolution,
            TYPESCRIPT_ID,
            executable,
            vec![OsString::from("--version")],
        ) else {
            mark_unavailable(
                resolution,
                TYPESCRIPT_ID,
                "the project-local TypeScript compiler failed its version probe",
                "verify the project's local TypeScript installation and executable permissions",
            );
            return false;
        };
        set_version(resolution, TYPESCRIPT_ID, &version);
        record_typescript_integration(resolution, &version);
        true
    }

    fn resolve_typescript_project(
        &self,
        resolution: &mut ProviderResolution,
        executable: &Path,
        config: &Path,
    ) {
        let config_ready = self.preflight_typescript_config(resolution, executable, config);
        let files_ready = self.preflight_typescript_files(resolution, executable, config);
        if !both_ready(config_ready, files_ready) {
            mark_unavailable(
                resolution,
                TYPESCRIPT_ID,
                "the TypeScript compiler could not resolve the project's configuration and source set",
                "inspect the TypeScript preflight diagnostics and repair tsconfig.json",
            );
        }
    }

    fn preflight_swift(&self, resolution: &mut ProviderResolution) {
        let Some(executable) = available_provider_executable(resolution, SWIFTPM_ID) else {
            return;
        };
        let Some(version) = self.preflight_swift_version(resolution, &executable) else {
            return;
        };
        set_version(resolution, SWIFTPM_ID, &version);
        let Some(output) = self.preflight_swift_description(resolution, executable) else {
            return;
        };
        apply_swift_description(resolution, &output.stdout);
    }

    fn preflight_swift_version(
        &self,
        resolution: &mut ProviderResolution,
        executable: &Path,
    ) -> Option<String> {
        let version = self.run_version(
            resolution,
            SWIFTPM_ID,
            executable,
            vec![OsString::from("--version")],
        );
        if version.is_none() {
            mark_unavailable(
                resolution,
                SWIFTPM_ID,
                "the configured Swift toolchain failed its version probe",
                "verify the selected Swift toolchain and executable permissions",
            );
        }
        version
    }

    fn preflight_swift_description(
        &self,
        resolution: &mut ProviderResolution,
        executable: PathBuf,
    ) -> Option<ProviderCommandOutput> {
        let command = ProviderCommand::new(
            executable,
            "package --disable-automatic-resolution --skip-update --disable-netrc describe --type json"
                .split_ascii_whitespace(),
            resolution.context.root.clone(),
            self.options.command_timeout,
        );
        let result = self.run_recorded(resolution, SWIFTPM_ID, &command);
        let output = successful_output(resolution, SWIFTPM_ID, "package description", result);
        if output.is_none() {
            mark_unavailable(
                resolution,
                SWIFTPM_ID,
                "SwiftPM could not describe the package without dependency resolution or network access",
                "restore an up-to-date Package.resolved and populate the local SwiftPM dependency cache, then retry",
            );
        }
        output
    }

    fn preflight_python(&self, resolution: &mut ProviderResolution) {
        let should_probe = status_by_id(&resolution.inventory, PYTHON_ID)
            .is_some_and(|status| status.applicable && status.available);
        if should_probe && !self.preflight_simple_version(resolution, PYTHON_ID) {
            mark_unavailable(
                resolution,
                PYTHON_ID,
                "the configured Python interpreter failed its version probe",
                "verify the selected Python interpreter and executable permissions",
            );
        }
    }

    fn preflight_bash(&self, resolution: &mut ProviderResolution) {
        let _ = self.preflight_simple_version(resolution, BASH_ID);
    }

    fn preflight_shellcheck(&self, resolution: &mut ProviderResolution) {
        let Some(executable) = available_provider_executable(resolution, SHELLCHECK_ID) else {
            return;
        };
        let command = ProviderCommand::new(
            executable,
            ["--version"],
            resolution.context.root.clone(),
            self.options.command_timeout,
        );
        let result = self.run_recorded(resolution, SHELLCHECK_ID, &command);
        let Some(output) = successful_output(resolution, SHELLCHECK_ID, "version probe", result) else {
            mark_unavailable(
                resolution,
                SHELLCHECK_ID,
                "the configured ShellCheck executable failed its version probe",
                "verify the configured ShellCheck executable, or leave the optional provider disabled",
            );
            return;
        };
        record_shellcheck_version(resolution, &output);
    }

    fn preflight_typescript_config(
        &self,
        resolution: &mut ProviderResolution,
        executable: &Path,
        config: &Path,
    ) -> bool {
        let Some(output) =
            self.run_typescript_query(resolution, executable, config, TypeScriptQuery::Configuration)
        else {
            return false;
        };
        match serde_json::from_str::<serde_json::Value>(&output.stdout) {
            Ok(document) => {
                set_metadata(resolution, TYPESCRIPT_ID, "resolved_config", "true");
                if let Some(options) = document
                    .get("compilerOptions")
                    .and_then(serde_json::Value::as_object)
                {
                    set_metadata(
                        resolution,
                        TYPESCRIPT_ID,
                        "compiler_option_count",
                        &options.len().to_string(),
                    );
                }
                true
            }
            Err(error) => {
                push_probe_diagnostic(
                    resolution,
                    TYPESCRIPT_ID,
                    Severity::Warning,
                    format!("tsc --showConfig returned invalid JSON: {error}"),
                );
                false
            }
        }
    }

    fn preflight_typescript_files(
        &self,
        resolution: &mut ProviderResolution,
        executable: &Path,
        config: &Path,
    ) -> bool {
        self.run_typescript_query(resolution, executable, config, TypeScriptQuery::Files)
            .is_some_and(|output| {
                let owned = project_owned_typescript_files(&resolution.context.root, &output.stdout);
                set_metadata(
                    resolution,
                    TYPESCRIPT_ID,
                    "configured_source_count",
                    &owned.len().to_string(),
                );
                if !owned.is_empty() {
                    resolution.context.sources.retain(|source| {
                        source.language != Language::TypeScript
                            || source.path.canonicalize().is_ok_and(|path| owned.contains(&path))
                    });
                }
                true
            })
    }

    fn run_typescript_query(
        &self,
        resolution: &mut ProviderResolution,
        executable: &Path,
        config: &Path,
        query: TypeScriptQuery,
    ) -> Option<ProviderCommandOutput> {
        let (action, phase) = query.command();
        let command = typescript_project_command(
            executable,
            config,
            &resolution.context.root,
            self.options.command_timeout,
            action,
        );
        let result = self.run_recorded(resolution, TYPESCRIPT_ID, &command);
        successful_output(resolution, TYPESCRIPT_ID, phase, result)
    }

    fn preflight_simple_version(&self, resolution: &mut ProviderResolution, id: &str) -> bool {
        let Some(status) = status_by_id(&resolution.inventory, id).cloned() else {
            return false;
        };
        if !provider_can_probe(&status) {
            return false;
        }
        self.run_simple_provider_version(resolution, id, status.executable.as_deref())
    }

    fn run_simple_provider_version(
        &self,
        resolution: &mut ProviderResolution,
        id: &str,
        executable: Option<&Path>,
    ) -> bool {
        let Some(executable) = executable else {
            return id == BASH_ID;
        };
        if let Some(version) = self.run_version(resolution, id, executable, vec![OsString::from("--version")])
        {
            set_version(resolution, id, &version);
            true
        } else {
            false
        }
    }

    fn run_version(
        &self,
        resolution: &mut ProviderResolution,
        id: &str,
        executable: &Path,
        args: Vec<OsString>,
    ) -> Option<String> {
        let command = ProviderCommand::new(
            executable.to_path_buf(),
            args,
            resolution.context.root.clone(),
            self.options.command_timeout,
        );
        let output = self.run_recorded(resolution, id, &command);
        successful_output(resolution, id, "version probe", output)
            .map(|output| compact_version(&output))
            .filter(|version| !version.is_empty())
    }

    fn run_recorded(
        &self,
        resolution: &mut ProviderResolution,
        id: &str,
        command: &ProviderCommand,
    ) -> Result<ProviderCommandOutput, CoreError> {
        if let Some(provenance) = provenance_by_id_mut(&mut resolution.provenance, id) {
            provenance.commands.push(command_display(command));
        }
        self.runner.run(command)
    }
}

fn typescript_project_command(
    executable: &Path,
    config: &Path,
    root: &Path,
    timeout: Duration,
    action: &str,
) -> ProviderCommand {
    ProviderCommand::new(
        executable.to_path_buf(),
        [
            OsString::from(action),
            OsString::from("-p"),
            config.as_os_str().to_os_string(),
            OsString::from("--pretty"),
            OsString::from("false"),
        ],
        root.to_path_buf(),
        timeout,
    )
}

fn available_provider_executable(resolution: &ProviderResolution, id: &str) -> Option<PathBuf> {
    status_by_id(&resolution.inventory, id)
        .filter(|status| status.applicable && status.available)
        .and_then(|status| status.executable.clone())
}

fn provider_can_probe(status: &ProviderStatus) -> bool {
    status.applicable && status.available
}

fn record_typescript_integration(resolution: &mut ProviderResolution, version: &str) {
    set_metadata(resolution, TYPESCRIPT_ID, "integration_mode", "cli");
    if typescript_major(version).is_some_and(|major| major >= 7) {
        resolution.context.diagnostics.push(Diagnostic::new(
            Severity::Info,
            "project-typescript-preflight",
            "TypeScript 7 detected; project resolution uses its CLI because no stable programmatic compiler API is exposed",
        ));
    }
}

fn typescript_major(version: &str) -> Option<u32> {
    version
        .split(|character: char| !character.is_ascii_digit())
        .find(|part| !part.is_empty())
        .and_then(|part| part.parse().ok())
}

fn required_typescript_config(resolution: &mut ProviderResolution) -> Option<PathBuf> {
    let config = typescript_config(resolution);
    if config.is_none() {
        mark_unavailable(
            resolution,
            TYPESCRIPT_ID,
            "native TypeScript project semantics require tsconfig.json",
            "add or select a project tsconfig.json, or use the generic backend",
        );
    }
    config
}

fn both_ready(first: bool, second: bool) -> bool {
    [first, second].into_iter().all(std::convert::identity)
}

fn apply_swift_description(resolution: &mut ProviderResolution, output: &str) {
    match serde_json::from_str::<serde_json::Value>(output) {
        Ok(document) => append_swift_metadata(resolution, &document),
        Err(error) => reject_swift_description(resolution, &error),
    }
}

fn append_swift_metadata(resolution: &mut ProviderResolution, document: &serde_json::Value) {
    for (input, output) in [("name", "package_name"), ("tools_version", "swift_tools_version")] {
        if let Some(value) = document.get(input).and_then(serde_json::Value::as_str) {
            set_metadata(resolution, SWIFTPM_ID, output, value);
        }
    }
    if let Some(targets) = document.get("targets").and_then(serde_json::Value::as_array) {
        set_metadata(resolution, SWIFTPM_ID, "target_count", &targets.len().to_string());
    }
}

fn reject_swift_description(resolution: &mut ProviderResolution, error: &serde_json::Error) {
    push_probe_diagnostic(
        resolution,
        SWIFTPM_ID,
        Severity::Warning,
        format!("swift package describe returned invalid JSON: {error}"),
    );
    mark_unavailable(
        resolution,
        SWIFTPM_ID,
        "SwiftPM returned an invalid package description",
        "verify that the selected Swift toolchain matches the package",
    );
}

fn record_shellcheck_version(resolution: &mut ProviderResolution, output: &ProviderCommandOutput) {
    let version = output
        .stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("version:").map(str::trim))
        .filter(|line| !line.is_empty())
        .map_or_else(|| compact_version(output), str::to_string);
    if !version.is_empty() {
        set_version(resolution, SHELLCHECK_ID, &version);
    }
}

impl<R: CommandRunner> ProjectBackend for ProjectAdapter<R> {
    fn info(&self) -> BackendInfo {
        let capabilities = [Capability::ProjectSemantics, Capability::ParseValidation];
        BackendInfo::new("project-adapters", env!("CARGO_PKG_VERSION"), true, capabilities)
    }

    fn supports(&self, project: ProjectKind) -> bool {
        matches!(
            project,
            ProjectKind::TypeScript
                | ProjectKind::SwiftPackage
                | ProjectKind::Python
                | ProjectKind::Bash
                | ProjectKind::Generic
        )
    }

    fn resolve(&self, request: &AnalysisRequest) -> Result<ProjectContext, CoreError> {
        Ok(self.discover(request)?.context)
    }
}

fn build_inventory(
    context: &ProjectContext,
    request: &AnalysisRequest,
    options: &ProviderOptions,
    metadata: &ProjectMetadata,
) -> Vec<ProviderStatus> {
    let root = &context.root;
    let (has_typescript, has_swift, has_python, has_bash) =
        provider_applicability(context, request, metadata);

    let tsc = resolve_local_tsc(root, options.typescript_tsc.as_deref());
    let swift = resolve_tool(root, options.swift.as_deref(), &[], &["swift"]);
    let python = resolve_tool(
        root,
        options.python.as_deref(),
        &[
            ".venv/bin/python",
            "venv/bin/python",
            ".venv/Scripts/python.exe",
            "venv/Scripts/python.exe",
        ],
        &["python3", "python"],
    );
    let bash = resolve_tool(root, options.bash.as_deref(), &[], &["bash"]);
    let shellcheck = resolve_tool(root, options.shellcheck.as_deref(), &[], &["shellcheck"]);

    let mut statuses = vec![
        inventory_status(ProviderIdentity::Bash, has_bash, bash),
        inventory_status(ProviderIdentity::Python, has_python, python),
        inventory_status(ProviderIdentity::Shellcheck, has_bash, shellcheck),
        inventory_status(ProviderIdentity::SwiftPm, has_swift, swift),
        inventory_status(ProviderIdentity::TypeScript, has_typescript, tsc),
    ];
    statuses.sort_by(|left, right| left.id.cmp(&right.id));
    statuses
}

fn provider_applicability(
    context: &ProjectContext,
    request: &AnalysisRequest,
    metadata: &ProjectMetadata,
) -> (bool, bool, bool, bool) {
    (
        typescript_is_applicable(context, request, metadata),
        project_kind_is_selected(context, request, Language::Swift, ProjectKind::SwiftPackage),
        language_project_is_applicable(context, request, Language::Python, ProjectKind::Python),
        language_project_is_applicable(context, request, Language::Bash, ProjectKind::Bash),
    )
}

fn typescript_is_applicable(
    context: &ProjectContext,
    request: &AnalysisRequest,
    metadata: &ProjectMetadata,
) -> bool {
    let project_detected = metadata.has_typescript_config
        || metadata.typescript.contains_key("declared_typescript")
        || context_has_language(context, Language::TypeScript);
    language_is_selected(request, Language::TypeScript) && project_detected
}

fn project_kind_is_selected(
    context: &ProjectContext,
    request: &AnalysisRequest,
    language: Language,
    kind: ProjectKind,
) -> bool {
    language_is_selected(request, language) && context.kinds.contains(&kind)
}

fn language_project_is_applicable(
    context: &ProjectContext,
    request: &AnalysisRequest,
    language: Language,
    kind: ProjectKind,
) -> bool {
    let project_detected = context.kinds.contains(&kind) || context_has_language(context, language);
    language_is_selected(request, language) && project_detected
}

fn context_has_language(context: &ProjectContext, language: Language) -> bool {
    context.sources.iter().any(|source| source.language == language)
}

fn language_is_selected(request: &AnalysisRequest, language: Language) -> bool {
    request.languages.is_empty() || request.languages.contains(&language)
}

#[derive(Clone, Copy)]
enum ProviderIdentity {
    Bash,
    Python,
    Shellcheck,
    SwiftPm,
    TypeScript,
}

struct ProviderStatusSpec<'a> {
    id: &'a str,
    project: ProjectKind,
    capabilities: Vec<Capability>,
    applicable: bool,
    available: bool,
    required_for_native: bool,
    executable: Option<PathBuf>,
    fallback: Option<&'a str>,
    missing_reason: Option<&'a str>,
    hint: Option<&'a str>,
}

struct ProviderDefinition {
    id: &'static str,
    project: ProjectKind,
    capabilities: &'static [Capability],
    required_for_native: bool,
    fallback: &'static str,
    missing_reason: Option<&'static str>,
    hint: &'static str,
}

impl ProviderDefinition {
    fn parsing_provider(
        id: &'static str,
        project: ProjectKind,
        missing_reason: &'static str,
        hint: &'static str,
    ) -> Self {
        Self {
            id,
            project,
            capabilities: &[Capability::ProjectSemantics, Capability::ParseValidation],
            required_for_native: true,
            fallback: "tree-sitter",
            missing_reason: Some(missing_reason),
            hint,
        }
    }
}

impl ProviderIdentity {
    fn definition(self) -> ProviderDefinition {
        match self {
            Self::Bash => ProviderDefinition {
                id: BASH_ID,
                project: ProjectKind::Bash,
                capabilities: &[Capability::ProjectSemantics],
                required_for_native: true,
                fallback: "tree-sitter",
                missing_reason: None,
                hint: "Bash dialect discovery is built in; a Bash executable is only needed for optional validation",
            },
            Self::Python => ProviderDefinition {
                id: PYTHON_ID,
                project: ProjectKind::Python,
                capabilities: &[Capability::ProjectSemantics],
                required_for_native: required_for_native_by_default(),
                fallback: "tree-sitter",
                missing_reason: Some("no Python interpreter was found in a project virtual environment or PATH"),
                hint: "configure an interpreter path or create/select the project's virtual environment",
            },
            Self::Shellcheck => ProviderDefinition {
                id: SHELLCHECK_ID,
                project: ProjectKind::Bash,
                capabilities: &[],
                required_for_native: false,
                fallback: "built-in-bash-dialect",
                missing_reason: Some("ShellCheck is optional and was not found in PATH"),
                hint: "configure a ShellCheck executable to enable its additional validation",
            },
            Self::SwiftPm => ProviderDefinition::parsing_provider(
                SWIFTPM_ID,
                ProjectKind::SwiftPackage,
                "SwiftPM is unavailable because a Swift executable was not found",
                "configure the Swift executable supplied by the project's selected toolchain",
            ),
            Self::TypeScript => ProviderDefinition::parsing_provider(
                TYPESCRIPT_ID,
                ProjectKind::TypeScript,
                "the project has no local TypeScript compiler executable",
                "restore the project's declared dependencies or configure a local tsc path",
            ),
        }
    }
}

fn inventory_status(
    identity: ProviderIdentity,
    applicable: bool,
    executable: Option<PathBuf>,
) -> ProviderStatus {
    let available = matches!(identity, ProviderIdentity::Bash) || executable.is_some();
    let definition = identity.definition();
    make_status(ProviderStatusSpec {
        id: definition.id,
        project: definition.project,
        capabilities: definition.capabilities.to_vec(),
        applicable,
        available,
        required_for_native: definition.required_for_native,
        executable,
        fallback: Some(definition.fallback),
        missing_reason: definition.missing_reason,
        hint: Some(definition.hint),
    })
}

fn make_status(spec: ProviderStatusSpec<'_>) -> ProviderStatus {
    ProviderStatus {
        id: spec.id.to_string(),
        project: spec.project,
        capabilities: BackendCapabilities::new(spec.capabilities),
        applicable: spec.applicable,
        available: spec.available,
        required_for_native: spec.required_for_native,
        executable: spec.executable,
        version: None,
        fallback: spec.fallback.map(str::to_string),
        reason: (!spec.available).then(|| {
            spec.missing_reason
                .unwrap_or("provider is unavailable")
                .to_string()
        }),
        hint: (!spec.available).then(|| spec.hint.map(str::to_string)).flatten(),
    }
}

fn unavailable_inventory(reason: &str) -> Vec<ProviderStatus> {
    let mut statuses = [
        (BASH_ID, ProjectKind::Bash, vec![Capability::ProjectSemantics]),
        (PYTHON_ID, ProjectKind::Python, vec![Capability::ProjectSemantics]),
        (SHELLCHECK_ID, ProjectKind::Bash, vec![]),
        (
            SWIFTPM_ID,
            ProjectKind::SwiftPackage,
            vec![Capability::ProjectSemantics, Capability::ParseValidation],
        ),
        (
            TYPESCRIPT_ID,
            ProjectKind::TypeScript,
            vec![Capability::ProjectSemantics, Capability::ParseValidation],
        ),
    ]
    .into_iter()
    .map(|(id, project, capabilities)| ProviderStatus {
        id: id.to_string(),
        project,
        capabilities: BackendCapabilities::new(capabilities),
        applicable: false,
        available: false,
        required_for_native: id != SHELLCHECK_ID,
        executable: None,
        version: None,
        fallback: Some("tree-sitter".to_string()),
        reason: Some(reason.to_string()),
        hint: Some("provide an existing readable project directory".to_string()),
    })
    .collect::<Vec<_>>();
    statuses.sort_by(|left, right| left.id.cmp(&right.id));
    statuses
}

fn enrich_project_kinds(context: &mut ProjectContext, metadata: &ProjectMetadata) {
    let has_typescript = metadata.has_typescript_config
        || metadata.typescript.contains_key("declared_typescript")
        || context_has_language(context, Language::TypeScript);
    let has_python = metadata.has_python_manifest || context_has_language(context, Language::Python);
    set_project_kind(context, ProjectKind::TypeScript, has_typescript);
    set_project_kind(context, ProjectKind::Python, has_python);
    set_project_kind(context, ProjectKind::SwiftPackage, metadata.has_swift_manifest);
    if context_has_language(context, Language::Bash) {
        context.kinds.insert(ProjectKind::Bash);
    }
    normalize_generic_project_kind(context);
}

fn set_project_kind(context: &mut ProjectContext, kind: ProjectKind, present: bool) {
    if present {
        context.kinds.insert(kind);
    } else {
        context.kinds.remove(&kind);
    }
}

fn normalize_generic_project_kind(context: &mut ProjectContext) {
    if context.kinds.len() > 1 {
        context.kinds.remove(&ProjectKind::Generic);
    }
    if context.kinds.is_empty() {
        context.kinds.insert(ProjectKind::Generic);
    }
}

fn build_provenance(
    context: &ProjectContext,
    inventory: &[ProviderStatus],
    project_metadata: &ProjectMetadata,
) -> Vec<ProviderProvenance> {
    inventory
        .iter()
        .filter(|status| status.applicable)
        .map(|status| {
            let mut metadata = match status.id.as_str() {
                TYPESCRIPT_ID => project_metadata.typescript.clone(),
                SWIFTPM_ID => project_metadata.swift.clone(),
                PYTHON_ID => project_metadata.python.clone(),
                BASH_ID | SHELLCHECK_ID => {
                    let dialects = discover_bash_dialects(&context.sources);
                    let mut metadata = BTreeMap::new();
                    metadata.insert("dialects".to_string(), dialect_names(&dialects));
                    metadata.insert(
                        "source_count".to_string(),
                        dialects.values().sum::<usize>().to_string(),
                    );
                    metadata
                }
                _ => BTreeMap::new(),
            };
            metadata.insert("discovery_mode".to_string(), "filesystem-only".to_string());
            ProviderProvenance {
                id: status.id.clone(),
                backend: status.backend_info(),
                executable: status.executable.clone(),
                version: status.version.clone(),
                commands: Vec::new(),
                metadata,
            }
        })
        .collect()
}

fn sync_context(resolution: &mut ProviderResolution, preflighted: bool, preference: BackendPreference) {
    resolution.context.backends = synchronized_backends(&resolution.inventory, preflighted);
    resolution.context.diagnostics.retain(retain_project_diagnostic);
    append_unavailable_provider_diagnostics(resolution, preference);
    synchronize_provenance(resolution);
}

fn synchronized_backends(inventory: &[ProviderStatus], preflighted: bool) -> Vec<BackendInfo> {
    if !preflighted {
        return Vec::new();
    }
    inventory
        .iter()
        .filter(|status| provider_is_preflighted(status))
        .map(ProviderStatus::backend_info)
        .collect()
}

fn provider_is_preflighted(status: &ProviderStatus) -> bool {
    status.applicable && status.available && (status.version.is_some() || status.id == BASH_ID)
}

fn retain_project_diagnostic(diagnostic: &Diagnostic) -> bool {
    !diagnostic.backend.starts_with("project-")
        || diagnostic.backend.ends_with("-preflight")
        || diagnostic.backend.ends_with("-discovery")
}

fn append_unavailable_provider_diagnostics(
    resolution: &mut ProviderResolution,
    preference: BackendPreference,
) {
    for status in &resolution.inventory {
        if !provider_requires_diagnostic(status, preference) {
            continue;
        }
        resolution
            .context
            .diagnostics
            .push(unavailable_provider_diagnostic(status, preference));
    }
}

fn provider_requires_diagnostic(status: &ProviderStatus, preference: BackendPreference) -> bool {
    status.applicable && !status.available && !matches!(preference, BackendPreference::Generic)
}

fn unavailable_provider_diagnostic(status: &ProviderStatus, preference: BackendPreference) -> Diagnostic {
    Diagnostic {
        severity: provider_diagnostic_severity(status, preference),
        backend: format!("project-{}", status.id),
        message: status
            .reason
            .clone()
            .unwrap_or_else(|| "provider is unavailable".to_string()),
        location: None,
        fallback_used: provider_uses_fallback(status, preference),
    }
}

fn provider_diagnostic_severity(status: &ProviderStatus, preference: BackendPreference) -> Severity {
    match (status.id.as_str(), preference) {
        (SHELLCHECK_ID, _) => Severity::Info,
        (_, BackendPreference::Native) => Severity::Error,
        _ => Severity::Warning,
    }
}

fn provider_uses_fallback(status: &ProviderStatus, preference: BackendPreference) -> bool {
    matches!(preference, BackendPreference::Auto) && status.required_for_native && status.fallback.is_some()
}

fn synchronize_provenance(resolution: &mut ProviderResolution) {
    for provenance in &mut resolution.provenance {
        if let Some(status) = status_by_id(&resolution.inventory, &provenance.id) {
            provenance.backend = status.backend_info();
            provenance.version = status.version.clone();
        }
    }
}

fn resolve_local_tsc(root: &Path, configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(configured) = configured {
        return existing_path(root, configured, true);
    }
    [
        "node_modules/.bin/tsc",
        "node_modules/.bin/tsc.cmd",
        "node_modules/.bin/tsc.exe",
    ]
    .iter()
    .find_map(|candidate| existing_path(root, Path::new(candidate), true))
}

fn resolve_tool(
    root: &Path,
    configured: Option<&Path>,
    project_candidates: &[&str],
    path_names: &[&str],
) -> Option<PathBuf> {
    if let Some(configured) = configured {
        return existing_path(root, configured, false);
    }
    project_candidates
        .iter()
        .find_map(|candidate| existing_path(root, Path::new(candidate), false))
        .or_else(|| path_names.iter().find_map(|name| find_on_path(name)))
}

fn existing_path(root: &Path, path: &Path, require_local: bool) -> Option<PathBuf> {
    let candidate = resolved_candidate(root, path);
    if !is_executable_file(&candidate) {
        return None;
    }
    let canonical_candidate = candidate.canonicalize().ok()?;
    if !candidate_is_allowed(root, &canonical_candidate, require_local)? {
        return None;
    }
    Some(canonical_candidate)
}

fn resolved_candidate(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn candidate_is_allowed(root: &Path, candidate: &Path, require_local: bool) -> Option<bool> {
    if !require_local {
        return Some(true);
    }
    Some(candidate.starts_with(root.canonicalize().ok()?))
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|path| find_on_search_path(name, &path))
}

fn find_on_search_path(name: &str, search_path: &OsStr) -> Option<PathBuf> {
    for directory in env::split_paths(search_path).filter(|directory| directory.is_absolute()) {
        let candidate = directory.join(name);
        if is_executable_file(&candidate) {
            return candidate.canonicalize().ok();
        }
        #[cfg(windows)]
        for extension in ["exe", "cmd", "bat"] {
            let candidate = directory.join(format!("{name}.{extension}"));
            if is_executable_file(&candidate) {
                return candidate.canonicalize().ok();
            }
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    reporigor_core::is_executable_file(path)
}

fn typescript_config(resolution: &ProviderResolution) -> Option<PathBuf> {
    resolution
        .provenance
        .iter()
        .find(|provenance| provenance.id == TYPESCRIPT_ID)
        .and_then(|provenance| provenance.metadata.get("tsconfig"))
        .map(PathBuf::from)
}

fn project_owned_typescript_files(root: &Path, output: &str) -> BTreeSet<PathBuf> {
    let Ok(root) = root.canonicalize() else {
        return BTreeSet::new();
    };
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let path = PathBuf::from(line);
            let path = if path.is_absolute() { path } else { root.join(path) };
            path.canonicalize().ok()
        })
        .filter(|path| path.starts_with(&root))
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| matches!(extension, "ts" | "tsx" | "mts" | "cts"))
        })
        .collect()
}

fn status_by_id<'a>(inventory: &'a [ProviderStatus], id: &str) -> Option<&'a ProviderStatus> {
    inventory.iter().find(|status| status.id == id)
}

fn status_by_id_mut<'a>(inventory: &'a mut [ProviderStatus], id: &str) -> Option<&'a mut ProviderStatus> {
    inventory.iter_mut().find(|status| status.id == id)
}

fn provenance_by_id_mut<'a>(
    provenance: &'a mut [ProviderProvenance],
    id: &str,
) -> Option<&'a mut ProviderProvenance> {
    provenance.iter_mut().find(|item| item.id == id)
}

fn set_version(resolution: &mut ProviderResolution, id: &str, version: &str) {
    if let Some(status) = status_by_id_mut(&mut resolution.inventory, id) {
        status.version = Some(version.to_string());
    }
    if let Some(provenance) = provenance_by_id_mut(&mut resolution.provenance, id) {
        provenance.version = Some(version.to_string());
    }
}

fn mark_unavailable(resolution: &mut ProviderResolution, id: &str, reason: &str, hint: &str) {
    if let Some(status) = status_by_id_mut(&mut resolution.inventory, id) {
        status.available = false;
        status.reason = Some(reason.to_string());
        status.hint = Some(hint.to_string());
    }
}

fn set_metadata(resolution: &mut ProviderResolution, id: &str, key: &str, value: &str) {
    if let Some(provenance) = provenance_by_id_mut(&mut resolution.provenance, id) {
        provenance.metadata.insert(key.to_string(), value.to_string());
    }
}

fn successful_output(
    resolution: &mut ProviderResolution,
    id: &str,
    operation: &str,
    result: Result<ProviderCommandOutput, CoreError>,
) -> Option<ProviderCommandOutput> {
    match result {
        Ok(output) => accepted_provider_output(resolution, id, operation, output),
        Err(error) => {
            reject_provider_error(resolution, id, operation, &error);
            None
        }
    }
}

fn accepted_provider_output(
    resolution: &mut ProviderResolution,
    id: &str,
    operation: &str,
    output: ProviderCommandOutput,
) -> Option<ProviderCommandOutput> {
    if output.output_truncated {
        reject_truncated_output(resolution, id, operation);
        return None;
    }
    if output.success() {
        Some(output)
    } else {
        reject_failed_output(resolution, id, operation, &output);
        None
    }
}

fn reject_truncated_output(resolution: &mut ProviderResolution, id: &str, operation: &str) {
    push_probe_diagnostic(
        resolution,
        id,
        Severity::Warning,
        format!("{operation} output exceeded the bounded capture size; refusing to use an incomplete result"),
    );
}

fn reject_failed_output(
    resolution: &mut ProviderResolution,
    id: &str,
    operation: &str,
    output: &ProviderCommandOutput,
) {
    let exit_code = output
        .exit_code
        .map_or_else(|| "no exit code".to_string(), |code| code.to_string());
    push_probe_diagnostic(
        resolution,
        id,
        Severity::Warning,
        format!(
            "{operation} exited with {exit_code}{}",
            provider_output_detail(output)
        ),
    );
}

fn provider_output_detail(output: &ProviderCommandOutput) -> String {
    let detail = if output.stderr.trim().is_empty() {
        output.stdout.trim()
    } else {
        output.stderr.trim()
    };
    if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    }
}

fn reject_provider_error(resolution: &mut ProviderResolution, id: &str, operation: &str, error: &CoreError) {
    push_probe_diagnostic(
        resolution,
        id,
        Severity::Warning,
        format!("{operation} failed: {error}"),
    );
}

fn push_probe_diagnostic(resolution: &mut ProviderResolution, id: &str, severity: Severity, message: String) {
    resolution.context.diagnostics.push(Diagnostic {
        severity,
        backend: format!("project-{id}-preflight"),
        message,
        location: None,
        fallback_used: false,
    });
}

fn compact_version(output: &ProviderCommandOutput) -> String {
    output
        .stdout
        .lines()
        .chain(output.stderr.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn command_display(command: &ProviderCommand) -> Vec<String> {
    std::iter::once(command.program.display().to_string())
        .chain(
            command
                .args
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned()),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::TempDir;

    use super::*;
    use crate::test_support::{fixture_executable, make_executable, write_fixtures};

    #[derive(Debug, Default)]
    struct PanicRunner {
        calls: AtomicUsize,
    }

    impl CommandRunner for PanicRunner {
        fn run(&self, _command: &ProviderCommand) -> Result<ProviderCommandOutput, CoreError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            panic!("discovery must not execute commands")
        }
    }

    fn temporary_project(files: &[(&str, &str)]) -> TempDir {
        let project = TempDir::new().unwrap_or_else(|error| panic!("fixture: {error}"));
        write_fixtures(project.path(), files);
        project
    }

    #[derive(Clone, Copy)]
    enum MultiLanguageFixture {
        Inventory,
        LanguageSelection,
    }

    fn multi_language_project(fixture: MultiLanguageFixture) -> TempDir {
        let (pyproject, shell_source) = match fixture {
            MultiLanguageFixture::Inventory => (
                "[project]\nname = \"demo\"\nrequires-python = \">=3.11\"\n",
                "#!/usr/bin/env bash\necho ok\n",
            ),
            MultiLanguageFixture::LanguageSelection => (
                "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
                "#!/bin/sh\necho ok\n",
            ),
        };
        temporary_project(&[
            ("tsconfig.json", "{}"),
            ("pyproject.toml", pyproject),
            ("Package.swift", "// swift-tools-version: 6.0\n"),
            ("tool.sh", shell_source),
            ("app.ts", "export const value = 1;\n"),
            ("app.py", "value = 1\n"),
        ])
    }

    fn discover_without_execution(project: &TempDir) -> (ProjectAdapter<PanicRunner>, ProviderResolution) {
        let adapter = ProjectAdapter::with_runner(ProviderOptions::default(), PanicRunner::default());
        let resolution = adapter
            .discover(&AnalysisRequest::new(project.path().to_path_buf()))
            .unwrap_or_else(|error| panic!("discover: {error}"));
        (adapter, resolution)
    }

    fn assert_no_execution(adapter: &ProjectAdapter<PanicRunner>) {
        assert_eq!(adapter.runner.calls.load(Ordering::SeqCst), 0);
    }

    fn canonical_path(path: &Path) -> PathBuf {
        path.canonicalize()
            .unwrap_or_else(|error| panic!("canonical {}: {error}", path.display()))
    }

    fn joined_search_path(paths: impl IntoIterator<Item = PathBuf>) -> OsString {
        match env::join_paths(paths) {
            Ok(path) => path,
            Err(error) => panic!("search path: {error}"),
        }
    }

    fn diagnostic_by_backend<'a>(resolution: &'a ProviderResolution, backend: &str) -> &'a Diagnostic {
        resolution
            .context
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.backend == backend)
            .unwrap_or_else(|| panic!("{backend} diagnostic"))
    }

    fn typescript_provenance(resolution: &ProviderResolution) -> &ProviderProvenance {
        resolution
            .provenance
            .iter()
            .find(|provenance| provenance.id == TYPESCRIPT_ID)
            .unwrap_or_else(|| panic!("TypeScript provenance"))
    }

    fn assert_typescript_metadata_ignored(
        resolution: &ProviderResolution,
        message_fragment: &str,
        absent_key: &str,
    ) {
        let diagnostic = diagnostic_by_backend(resolution, "project-typescript-discovery");
        assert!(diagnostic.message.contains("package.json"));
        assert!(diagnostic.message.contains(message_fragment));
        assert!(!typescript_provenance(resolution)
            .metadata
            .contains_key(absent_key));
    }

    fn assert_unsafe_metadata_is_inert(project: &TempDir, message_fragment: &str, absent_key: &str) {
        write_fixtures(project.path(), &[("app.ts", "export const value = 1;\n")]);
        let (adapter, resolution) = discover_without_execution(project);
        assert_typescript_metadata_ignored(&resolution, message_fragment, absent_key);
        assert_no_execution(&adapter);
    }

    fn oversized_package_project() -> TempDir {
        let project = temporary_project(&[]);
        let package = project.path().join("package.json");
        let file = fs::File::create(&package).unwrap_or_else(|error| panic!("package: {error}"));
        file.set_len(reporigor_core::PROJECT_METADATA_MAX_BYTES + 1)
            .unwrap_or_else(|error| panic!("sparse length: {error}"));
        project
    }

    #[cfg(unix)]
    fn escaping_package_project() -> (TempDir, TempDir) {
        use std::os::unix::fs::symlink;

        let project = temporary_project(&[]);
        let outside = TempDir::new().unwrap_or_else(|error| panic!("outside: {error}"));
        let target = outside.path().join("package.json");
        fs::write(&target, r#"{"devDependencies":{"typescript":"5.9.0"}}"#)
            .unwrap_or_else(|error| panic!("outside package: {error}"));
        symlink(&target, project.path().join("package.json"))
            .unwrap_or_else(|error| panic!("symlink: {error}"));
        (project, outside)
    }

    #[test]
    fn discovery_is_filesystem_only_and_inventory_is_sorted() {
        let temp = multi_language_project(MultiLanguageFixture::Inventory);
        let (adapter, resolution) = discover_without_execution(&temp);
        assert!(ProjectBackend::supports(&adapter, ProjectKind::Generic));
        let ids = resolution
            .inventory
            .iter()
            .map(|status| status.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["bash", "python", "shellcheck", "swiftpm", "typescript"]);
        let shellcheck =
            status_by_id(&resolution.inventory, SHELLCHECK_ID).unwrap_or_else(|| panic!("ShellCheck status"));
        assert!(!shellcheck.required_for_native);
        assert!(!shellcheck.capabilities.contains(Capability::ParseValidation));
        let python =
            status_by_id(&resolution.inventory, PYTHON_ID).unwrap_or_else(|| panic!("Python status"));
        assert!(!python.capabilities.contains(Capability::ParseValidation));
        let serialized =
            serde_json::to_value(shellcheck).unwrap_or_else(|error| panic!("serialize status: {error}"));
        assert_eq!(
            serialized
                .get("required_for_native")
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(resolution
            .inventory
            .iter()
            .filter(|status| status.id != SHELLCHECK_ID)
            .all(|status| status.required_for_native));
        assert!(resolution.context.kinds.contains(&ProjectKind::Bash));
        assert!(resolution.context.kinds.contains(&ProjectKind::Python));
        assert!(resolution.context.kinds.contains(&ProjectKind::SwiftPackage));
        assert!(resolution.context.kinds.contains(&ProjectKind::TypeScript));
        assert!(resolution.context.backends.is_empty());
        let resolved = ProjectBackend::resolve(&adapter, &AnalysisRequest::new(temp.path().to_path_buf()));
        let resolved = resolved.unwrap_or_else(|error| panic!("trait resolve: {error}"));
        assert!(resolved.backends.is_empty());
        assert_no_execution(&adapter);
    }

    #[test]
    fn plain_package_json_does_not_claim_typescript() {
        let temp = temporary_project(&[
            ("package.json", r#"{"name":"javascript-only"}"#),
            ("index.js", "export const value = 1;\n"),
        ]);
        let (adapter, resolution) = discover_without_execution(&temp);
        assert!(!resolution.context.kinds.contains(&ProjectKind::TypeScript));
        assert!(resolution.context.kinds.contains(&ProjectKind::Generic));
        let status =
            status_by_id(&resolution.inventory, TYPESCRIPT_ID).unwrap_or_else(|| panic!("typescript status"));
        assert!(!status.applicable);
        assert_no_execution(&adapter);
    }

    #[test]
    fn sparse_oversized_metadata_is_ignored_with_a_warning_and_no_execution() {
        let temp = oversized_package_project();
        assert_unsafe_metadata_is_inert(&temp, "maximum", "package_json");
    }

    #[cfg(unix)]
    #[test]
    fn escaping_metadata_symlink_is_ignored_with_a_warning_and_no_execution() {
        let (temp, _outside) = escaping_package_project();
        assert_unsafe_metadata_is_inert(&temp, "escapes project root", "declared_typescript");
    }

    #[test]
    fn language_selection_scopes_manifest_derived_provider_applicability() {
        let temp = multi_language_project(MultiLanguageFixture::LanguageSelection);
        write_fixtures(temp.path(), &[("app.c", "int value(void) { return 1; }\n")]);
        let configured_bash = fixture_executable(temp.path(), "configured-bash");
        let configured_shellcheck = fixture_executable(temp.path(), "configured-shellcheck");

        let adapter = ProjectAdapter::with_runner(
            ProviderOptions {
                bash: Some(configured_bash),
                shellcheck: Some(configured_shellcheck),
                ..ProviderOptions::default()
            },
            PanicRunner::default(),
        );
        let mut c_request = AnalysisRequest::new(temp.path().to_path_buf());
        c_request.languages.insert(Language::C);
        c_request.backend = BackendPreference::Native;
        let c_resolution = adapter
            .preflight(&c_request)
            .unwrap_or_else(|error| panic!("C-only preflight: {error}"));
        for id in [TYPESCRIPT_ID, SWIFTPM_ID, PYTHON_ID, BASH_ID, SHELLCHECK_ID] {
            let status = status_by_id(&c_resolution.inventory, id)
                .unwrap_or_else(|| panic!("missing {id} provider status"));
            assert!(!status.applicable, "{id} must not gate an unselected language");
        }
        assert_eq!(adapter.runner.calls.load(Ordering::SeqCst), 0);

        let mut python_request = AnalysisRequest::new(temp.path().to_path_buf());
        python_request.languages.insert(Language::Python);
        let python_resolution = adapter
            .discover(&python_request)
            .unwrap_or_else(|error| panic!("Python-only discovery: {error}"));
        assert!(status_by_id(&python_resolution.inventory, PYTHON_ID).is_some_and(|status| status.applicable));
        assert!(!status_by_id(&python_resolution.inventory, TYPESCRIPT_ID)
            .is_some_and(|status| status.applicable));
        assert!(
            !status_by_id(&python_resolution.inventory, SWIFTPM_ID).is_some_and(|status| status.applicable)
        );
        assert!(!status_by_id(&python_resolution.inventory, BASH_ID).is_some_and(|status| status.applicable));
        assert!(!status_by_id(&python_resolution.inventory, SHELLCHECK_ID)
            .is_some_and(|status| status.applicable));
    }

    #[test]
    fn provider_diagnostics_respect_backend_preference() {
        let temp = temporary_project(&[("tsconfig.json", "{}"), ("index.ts", "export {};\n")]);
        let adapter = ProjectAdapter::with_runner(ProviderOptions::default(), PanicRunner::default());

        let mut generic_request = AnalysisRequest::new(temp.path().to_path_buf());
        generic_request.backend = BackendPreference::Generic;
        let generic = adapter
            .discover(&generic_request)
            .unwrap_or_else(|error| panic!("generic discovery: {error}"));
        assert!(!generic
            .context
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.backend == "project-typescript"));

        let mut native_request = AnalysisRequest::new(temp.path().to_path_buf());
        native_request.backend = BackendPreference::Native;
        let native = adapter
            .discover(&native_request)
            .unwrap_or_else(|error| panic!("native discovery: {error}"));
        let diagnostic = diagnostic_by_backend(&native, "project-typescript");
        assert_eq!(diagnostic.severity, Severity::Error);
        assert!(!diagnostic.fallback_used);
        assert_eq!(adapter.runner.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn configured_typescript_tool_must_stay_inside_project() {
        let project = temporary_project(&[("tsconfig.json", "{}")]);
        let outside = TempDir::new().unwrap_or_else(|error| panic!("outside: {error}"));
        let outside_tsc = outside.path().join("tsc");
        fs::write(&outside_tsc, "not executable").unwrap_or_else(|error| panic!("tool: {error}"));
        let options = ProviderOptions {
            typescript_tsc: Some(outside_tsc),
            ..ProviderOptions::default()
        };
        let status = providers_with_options(project.path(), &options)
            .into_iter()
            .find(|status| status.id == TYPESCRIPT_ID)
            .unwrap_or_else(|| panic!("typescript status"));
        assert!(!status.available);
        assert!(status.executable.is_none());
    }

    #[test]
    fn executable_search_ignores_relative_path_entries_and_returns_canonical_paths() {
        let trusted = TempDir::new().unwrap_or_else(|error| panic!("trusted directory: {error}"));
        let executable = fixture_executable(trusted.path(), "audit-tool");
        let expected = canonical_path(&executable);

        let trusted_search = joined_search_path([
            PathBuf::new(),
            PathBuf::from("relative-bin"),
            trusted.path().to_path_buf(),
        ]);
        assert_eq!(find_on_search_path("audit-tool", &trusted_search), Some(expected));

        let untrusted_search = joined_search_path([PathBuf::new(), PathBuf::from("relative-bin")]);
        assert_eq!(find_on_search_path("audit-tool", &untrusted_search), None);
    }

    #[cfg(unix)]
    #[test]
    fn executable_resolution_rejects_files_without_execute_permission() {
        use std::os::unix::fs::PermissionsExt;

        let project = temporary_project(&[("configured-python", "fixture")]);
        let executable = project.path().join("configured-python");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o644))
            .unwrap_or_else(|error| panic!("permissions: {error}"));

        assert_eq!(existing_path(project.path(), &executable, false), None);
        let search_path = joined_search_path([project.path().to_path_buf()]);
        assert_eq!(find_on_search_path("configured-python", &search_path), None);

        make_executable(&executable);
        let expected = canonical_path(&executable);
        assert_eq!(
            existing_path(project.path(), &executable, false),
            Some(expected.clone())
        );
        assert_eq!(
            find_on_search_path("configured-python", &search_path),
            Some(expected)
        );
    }
}
