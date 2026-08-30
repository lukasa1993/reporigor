use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::Duration,
};

use anyhow::{anyhow, bail, ensure, Context, Result};
use reporigor_process_tree::{configure_piped_command, CleanupPolicy, ProcessTree, WaitReason};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const LOCK_SCHEMA_VERSION: u32 = 1;
const BASELINE_SCHEMA_VERSION: u32 = 1;
const REPORT_SCHEMA_VERSION: u64 = 1;
const LANGUAGES: &str = "bash|c|cpp|objective-c|python|rust|swift|typescript";

fn supported_languages() -> impl Iterator<Item = &'static str> {
    LANGUAGES.split('|')
}

#[derive(Debug, Deserialize)]
struct CorpusLock {
    schema_version: u32,
    corpus: Vec<CorpusEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct CorpusEntry {
    language: String,
    name: String,
    repository: String,
    revision: String,
    license: String,
    tier: String,
    modes: Vec<String>,
    #[serde(default)]
    filters: Vec<String>,
    timeout_seconds: u64,
    max_output_bytes: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct Baseline {
    schema_version: u32,
    #[serde(default, rename = "result")]
    results: Vec<RegressionRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
struct RegressionRecord {
    #[serde(rename = "name")]
    name: String,
    language: String,
    revision: String,
    backend: String,
    exit_code: i32,
    files: u64,
    functions: u64,
    duplicate_groups: u64,
    mutants: u64,
    parse_errors: u64,
    diagnostics: u64,
    sha256: String,
}

#[derive(Debug, Clone)]
struct Options {
    operation: Operation,
    checkout_root: PathBuf,
    reporigor: Option<PathBuf>,
    include_native: bool,
    require_all: bool,
    names: BTreeSet<String>,
    tier: Option<Tier>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Validate,
    Verify,
    Run,
    Update,
    Populate,
}

const OPERATIONS: [(&str, Operation); 5] = [
    ("validate", Operation::Validate),
    ("verify", Operation::Verify),
    ("run", Operation::Run),
    ("update", Operation::Update),
    ("populate", Operation::Populate),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    PullRequest,
    Scheduled,
}

impl Tier {
    const fn as_str(self) -> &'static str {
        match self {
            Self::PullRequest => "pull-request",
            Self::Scheduled => "scheduled",
        }
    }
}

#[derive(Debug)]
struct Verification<'a> {
    present: Vec<(&'a CorpusEntry, PathBuf)>,
    missing: Vec<&'a CorpusEntry>,
}

fn main() {
    if let Err(error) = try_main() {
        eprintln!("corpus-harness: {error:#}");
        std::process::exit(1);
    }
}

fn try_main() -> Result<()> {
    workspace_root().and_then(|workspace| run_from_workspace(&workspace, env::args_os().skip(1)))
}

fn run_from_workspace(workspace: &Path, arguments: impl Iterator<Item = OsString>) -> Result<()> {
    let options = parse_options_from(workspace, arguments)?;
    let (lock, baseline_path, baseline) = load_harness_state(workspace, &options)?;

    dispatch_operation(workspace, &lock, &baseline_path, baseline, &options)
}

fn dispatch_operation(
    workspace: &Path,
    lock: &CorpusLock,
    baseline_path: &Path,
    baseline: Baseline,
    options: &Options,
) -> Result<()> {
    match options.operation {
        Operation::Validate => validate_operation(lock, &baseline, options),
        Operation::Verify => verify_operation(lock, options, false),
        Operation::Populate => verify_operation(lock, options, true),
        Operation::Run | Operation::Update => {
            execute_operation(workspace, lock, baseline_path, baseline, options)
        }
    }
}

fn load_harness_state(workspace: &Path, options: &Options) -> Result<(CorpusLock, PathBuf, Baseline)> {
    let lock_path = workspace.join("corpus/corpus.lock.toml");
    let lock = load_lock(&lock_path)?;
    validate_lock(&lock)?;
    validate_selection(&lock, options)?;
    let baseline_path = workspace.join("corpus/baseline.toml");
    let baseline = load_baseline(&baseline_path)?;
    validate_baseline(&lock, &baseline)?;
    Ok((lock, baseline_path, baseline))
}

fn validate_selection(lock: &CorpusLock, options: &Options) -> Result<()> {
    validate_selected_names(lock, &options.names)?;
    ensure_selection(lock, options)
}

fn validate_operation(lock: &CorpusLock, baseline: &Baseline, options: &Options) -> Result<()> {
    if options.require_all {
        validate_baseline_completeness(lock, baseline, options)?;
    }
    let selected = lock
        .corpus
        .iter()
        .filter(|entry| is_selected(entry, options))
        .count();
    println!(
        "validated {selected} selected lock entries and {} baseline records",
        baseline.results.len()
    );
    Ok(())
}

fn verify_operation(lock: &CorpusLock, options: &Options, populate_first: bool) -> Result<()> {
    if populate_first {
        populate(lock, options)?;
    }
    prepare_verification(lock, options)?;
    Ok(())
}

fn prepare_verification<'a>(lock: &'a CorpusLock, options: &Options) -> Result<Verification<'a>> {
    let verification = verify_checkouts(lock, options)?;
    print_verification(&verification);
    if options.require_all && !verification.missing.is_empty() {
        bail!(
            "{} selected corpus checkout(s) are missing under {}; run `scripts/corpus-harness populate` explicitly",
            verification.missing.len(),
            options.checkout_root.display()
        );
    }
    Ok(verification)
}

fn execute_operation(
    workspace: &Path,
    lock: &CorpusLock,
    baseline_path: &Path,
    mut baseline: Baseline,
    options: &Options,
) -> Result<()> {
    let verification = execution_verification(lock, &baseline, options)?;
    let current = run_selected_corpora(workspace, &verification, options)?;
    finish_execution(
        &CompletionContext {
            lock,
            baseline_path,
            options,
        },
        &mut baseline,
        current,
    )
}

fn execution_verification<'a>(
    lock: &'a CorpusLock,
    baseline: &Baseline,
    options: &Options,
) -> Result<Verification<'a>> {
    let verification = prepare_verification(lock, options)?;
    ensure!(
        !verification.present.is_empty(),
        "no selected pinned checkouts are present under {}; populate one or pass --checkout-root",
        options.checkout_root.display()
    );
    if options.operation == Operation::Run && options.require_all {
        validate_baseline_completeness(lock, baseline, options)?;
    }
    Ok(verification)
}

fn run_selected_corpora(
    workspace: &Path,
    verification: &Verification<'_>,
    options: &Options,
) -> Result<Vec<RegressionRecord>> {
    let reporigor = resolve_reporigor(options)?;
    let artifact_root = workspace.join("target/corpus-harness");
    fs::create_dir_all(&artifact_root)
        .with_context(|| format!("failed to create {}", artifact_root.display()))?;
    let current = run_corpora(&verification.present, options, &reporigor, &artifact_root)?;
    write_current(&artifact_root.join("current.toml"), &current)?;
    Ok(current)
}

struct CompletionContext<'a> {
    lock: &'a CorpusLock,
    baseline_path: &'a Path,
    options: &'a Options,
}

fn finish_execution(
    context: &CompletionContext<'_>,
    baseline: &mut Baseline,
    current: Vec<RegressionRecord>,
) -> Result<()> {
    if context.options.operation == Operation::Update {
        return context.update_baseline(baseline, current);
    }
    compare_baseline(baseline, &current)?;
    println!("corpus regression baseline matches");
    Ok(())
}

impl CompletionContext<'_> {
    fn update_baseline(&self, baseline: &mut Baseline, current: Vec<RegressionRecord>) -> Result<()> {
        merge_baseline(baseline, current);
        validate_baseline(self.lock, baseline)?;
        if self.options.require_all {
            validate_baseline_completeness(self.lock, baseline, self.options)?;
        }
        write_baseline(self.baseline_path, baseline)?;
        println!("updated {}", self.baseline_path.display());
        Ok(())
    }
}

fn workspace_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .context("failed to resolve workspace root")
}

fn parse_options_from(workspace: &Path, mut arguments: impl Iterator<Item = OsString>) -> Result<Options> {
    let operation_argument = arguments.next();
    let operation = parse_operation(operation_argument.as_ref())?;
    let default_checkout = env::var_os("REPORIGOR_CORPUS_ROOT")
        .map_or_else(|| workspace.join("corpus/checkouts"), PathBuf::from);
    let mut options = Options {
        operation,
        checkout_root: default_checkout,
        reporigor: env::var_os("REPORIGOR_BIN").map(PathBuf::from),
        include_native: false,
        require_all: false,
        names: BTreeSet::new(),
        tier: None,
    };
    while let Some(argument) = arguments.next() {
        apply_option(&argument, &mut arguments, &mut options)?;
    }
    Ok(options)
}

fn parse_operation(argument: Option<&OsString>) -> Result<Operation> {
    match argument.and_then(|value| value.to_str()) {
        Some("help" | "--help" | "-h") => {
            print_usage();
            std::process::exit(0);
        }
        Some(name) => {
            operation_from_name(name).ok_or_else(|| anyhow!("unknown operation {name:?}\n\n{}", usage()))
        }
        None => Err(anyhow!("an operation is required\n\n{}", usage())),
    }
}

fn operation_from_name(name: &str) -> Option<Operation> {
    named_value(name, &OPERATIONS)
}

fn apply_option(
    argument: &OsString,
    arguments: &mut impl Iterator<Item = OsString>,
    options: &mut Options,
) -> Result<()> {
    let option = argument
        .to_str()
        .ok_or_else(|| anyhow!("arguments must be valid UTF-8 except for filesystem paths"))?;
    apply_utf8_option(option, arguments, options)
}

fn apply_utf8_option(
    option: &str,
    arguments: &mut impl Iterator<Item = OsString>,
    options: &mut Options,
) -> Result<()> {
    if let Some(value) = value_option(option) {
        return apply_value_option(options, value, arguments);
    }
    match option {
        "--native" => options.include_native = true,
        "--require-all" => options.require_all = true,
        other => bail!("unknown option {other:?}\n\n{}", usage()),
    }
    Ok(())
}

fn apply_value_option(
    options: &mut Options,
    option: ValueOption,
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<()> {
    match option {
        ValueOption::CheckoutRoot => {
            set_path_option(options, "--checkout-root", arguments, |target, path| {
                target.checkout_root = path;
            })
        }
        ValueOption::Reporigor => set_path_option(options, "--reporigor", arguments, |target, path| {
            target.reporigor = Some(path);
        }),
        ValueOption::Name => insert_name(arguments, options),
        ValueOption::Tier => set_tier(options, arguments),
    }
}

#[derive(Clone, Copy)]
enum ValueOption {
    CheckoutRoot,
    Reporigor,
    Name,
    Tier,
}

fn value_option(option: &str) -> Option<ValueOption> {
    named_value(
        option,
        &[
            ("--checkout-root", ValueOption::CheckoutRoot),
            ("--reporigor", ValueOption::Reporigor),
            ("--name", ValueOption::Name),
            ("--tier", ValueOption::Tier),
        ],
    )
}

fn named_value<T: Copy>(name: &str, values: &[(&str, T)]) -> Option<T> {
    values
        .iter()
        .find_map(|(candidate, value)| (*candidate == name).then_some(*value))
}

fn set_path_option(
    options: &mut Options,
    name: &str,
    arguments: &mut impl Iterator<Item = OsString>,
    assign: impl FnOnce(&mut Options, PathBuf),
) -> Result<()> {
    let path = PathBuf::from(next_value(arguments, name)?);
    assign(options, path);
    Ok(())
}

fn insert_name(arguments: &mut impl Iterator<Item = OsString>, options: &mut Options) -> Result<()> {
    let name = next_utf8("--name", arguments)?;
    options.names.insert(name);
    Ok(())
}

fn set_tier(options: &mut Options, arguments: &mut impl Iterator<Item = OsString>) -> Result<()> {
    ensure!(options.tier.is_none(), "--tier may be supplied only once");
    let tier = next_utf8("--tier", arguments)?;
    options.tier = Some(parse_tier(&tier)?);
    Ok(())
}

fn next_utf8(option: &str, arguments: &mut impl Iterator<Item = OsString>) -> Result<String> {
    next_value(arguments, option)?
        .into_string()
        .map_err(|_| anyhow!("{option} must be valid UTF-8"))
}

fn next_value(arguments: &mut impl Iterator<Item = OsString>, option: &str) -> Result<OsString> {
    arguments
        .next()
        .ok_or_else(|| anyhow!("{option} requires a value"))
}

fn parse_tier(value: &str) -> Result<Tier> {
    match value {
        "pull-request" => Ok(Tier::PullRequest),
        "scheduled" => Ok(Tier::Scheduled),
        _ => bail!("invalid --tier {value:?}; expected pull-request or scheduled"),
    }
}

fn print_usage() {
    println!("{}", usage());
}

fn usage() -> &'static str {
    "Usage: corpus-harness <validate|verify|run|update|populate> [OPTIONS]\n\
     \n\
     Options:\n\
       --checkout-root PATH  Checkout directory (default: corpus/checkouts)\n\
       --reporigor PATH     reporigor executable for run/update\n\
       --name NAME           Select one corpus; repeat to select more\n\
       --tier TIER           Select pull-request or scheduled entries\n\
       --native              Also run lockfile-declared native modes\n\
       --require-all         Require every selected checkout/baseline mode"
}

fn load_lock(path: &Path) -> Result<CorpusLock> {
    load_toml(path, "corpus lock")
}

fn load_toml<T: DeserializeOwned>(path: &Path, description: &str) -> Result<T> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read {description} {}", path.display()))?;
    toml::from_str(&source).with_context(|| format!("failed to parse {description} {}", path.display()))
}

fn validate_schema(actual: u32, expected: u32, description: &str) -> Result<()> {
    ensure!(
        actual == expected,
        "unsupported {description} schema {actual}; expected {expected}"
    );
    Ok(())
}

fn validate_lock(lock: &CorpusLock) -> Result<()> {
    validate_schema(lock.schema_version, LOCK_SCHEMA_VERSION, "corpus lock")?;
    ensure!(!lock.corpus.is_empty(), "corpus lock contains no entries");
    let mut names = BTreeSet::new();
    let mut languages = BTreeSet::new();
    for entry in &lock.corpus {
        ensure!(
            names.insert(entry.name.as_str()),
            "duplicate corpus name {}",
            entry.name
        );
        ensure!(
            supported_languages().any(|language| language == entry.language.as_str()),
            "unsupported language {}",
            entry.language
        );
        languages.insert(entry.language.as_str());
        ensure!(
            is_commit_revision(&entry.revision),
            "{} revision must be a full 40-character hexadecimal commit",
            entry.name
        );
        ensure!(
            entry.repository.starts_with("https://")
                && Path::new(&entry.repository)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("git")),
            "{} repository must be an explicit HTTPS Git URL ending in .git",
            entry.name
        );
        ensure!(
            !entry.license.trim().is_empty(),
            "{} has an empty license",
            entry.name
        );
        ensure!(
            matches!(entry.tier.as_str(), "pull-request" | "scheduled"),
            "{} has invalid tier",
            entry.name
        );
        ensure!(
            entry.modes.iter().any(|mode| mode == "generic"),
            "{} must declare generic mode",
            entry.name
        );
        ensure!(
            entry
                .modes
                .iter()
                .all(|mode| mode == "generic" || mode == "native"),
            "{} declares an unknown backend mode",
            entry.name
        );
        ensure!(
            entry.timeout_seconds > 0 && entry.timeout_seconds <= 900,
            "{} timeout is outside 1..=900 seconds",
            entry.name
        );
        ensure!(
            entry.max_output_bytes > 0,
            "{} output bound must be positive",
            entry.name
        );
        ensure!(
            entry.filters.iter().all(|filter| !filter.is_empty()),
            "{} has an empty path filter",
            entry.name
        );
    }
    for language in supported_languages() {
        ensure!(
            languages.contains(language),
            "corpus lock has no {language} entry"
        );
    }
    Ok(())
}

fn is_commit_revision(revision: &str) -> bool {
    revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_selected_names(lock: &CorpusLock, selected: &BTreeSet<String>) -> Result<()> {
    for name in selected {
        ensure!(
            lock.corpus.iter().any(|entry| entry.name == *name),
            "unknown corpus name {name:?}"
        );
    }
    Ok(())
}

fn ensure_selection(lock: &CorpusLock, options: &Options) -> Result<()> {
    ensure!(
        lock.corpus.iter().any(|entry| is_selected(entry, options)),
        "the requested name/tier selection matches no corpus entries"
    );
    Ok(())
}

fn is_selected(entry: &CorpusEntry, options: &Options) -> bool {
    (options.names.is_empty() || options.names.contains(&entry.name))
        && options.tier.is_none_or(|tier| entry.tier == tier.as_str())
}

fn selected_checkouts<'a>(lock: &'a CorpusLock, options: &Options) -> Vec<(&'a CorpusEntry, PathBuf)> {
    let mut checkouts = Vec::new();
    for entry in lock.corpus.iter().filter(|entry| is_selected(entry, options)) {
        checkouts.push((entry, options.checkout_root.join(&entry.name)));
    }
    checkouts
}

fn verify_checkouts<'a>(lock: &'a CorpusLock, options: &Options) -> Result<Verification<'a>> {
    let mut present = Vec::new();
    let mut missing = Vec::with_capacity(lock.corpus.len());
    for (entry, checkout) in selected_checkouts(lock, options) {
        if !checkout.exists() {
            missing.push(entry);
            continue;
        }
        verify_checkout(entry, &checkout)?;
        present.push((entry, checkout));
    }
    Ok(Verification { present, missing })
}

fn verify_checkout(entry: &CorpusEntry, checkout: &Path) -> Result<()> {
    ensure!(checkout.is_dir(), "{} is not a directory", checkout.display());
    let revision = git_stdout(checkout, &["rev-parse", "--verify", "HEAD"])?;
    ensure!(
        revision.trim() == entry.revision,
        "{} is at {}, expected {}",
        checkout.display(),
        revision.trim(),
        entry.revision
    );
    let status = git_stdout(checkout, &["status", "--porcelain=v1", "--untracked-files=all"])?;
    if !status.trim().is_empty() {
        bail!("{} is not clean:\n{status}", checkout.display());
    }
    let remote = git_stdout(checkout, &["remote", "get-url", "origin"])?;
    ensure!(
        normalize_repository(remote.trim()) == normalize_repository(&entry.repository),
        "{} origin is {:?}, expected {:?}",
        checkout.display(),
        remote.trim(),
        entry.repository
    );
    Ok(())
}

fn normalize_repository(repository: &str) -> String {
    repository
        .trim()
        .trim_end_matches(".git")
        .replace("git@github.com:", "https://github.com/")
        .to_ascii_lowercase()
}

fn print_verification(verification: &Verification<'_>) {
    for (entry, checkout) in &verification.present {
        println!("verified {:<18} {}", entry.name, checkout.display());
    }
    for entry in &verification.missing {
        println!("missing  {:<18} {}", entry.name, entry.revision);
    }
}

fn populate(lock: &CorpusLock, options: &Options) -> Result<()> {
    fs::create_dir_all(&options.checkout_root)
        .with_context(|| format!("failed to create {}", options.checkout_root.display()))?;
    for (entry, checkout) in selected_checkouts(lock, options) {
        populate_checkout(entry, &checkout)?;
    }
    Ok(())
}

fn populate_checkout(entry: &CorpusEntry, checkout: &Path) -> Result<()> {
    if checkout.exists() {
        println!("kept     {:<18} already exists; no fetch attempted", entry.name);
        return Ok(());
    }
    create_pinned_checkout(entry, checkout)?;
    println!("populated {:<18} {}", entry.name, entry.revision);
    Ok(())
}

fn create_pinned_checkout(entry: &CorpusEntry, checkout: &Path) -> Result<()> {
    fs::create_dir(checkout).with_context(|| format!("failed to create {}", checkout.display()))?;
    run_git(checkout, &["init", "--quiet"])?;
    run_git(checkout, &["remote", "add", "origin", &entry.repository])?;
    run_git(
        checkout,
        &["fetch", "--quiet", "--depth=1", "origin", &entry.revision],
    )?;
    run_git(checkout, &["checkout", "--quiet", "--detach", &entry.revision])
}

fn run_git(root: &Path, arguments: &[&str]) -> Result<()> {
    git_stdout(root, arguments).map(drop)
}

fn git_stdout(root: &Path, arguments: &[&str]) -> Result<String> {
    let output = git_command(root, arguments)
        .output()
        .with_context(|| format!("failed to start git in {}", root.display()))?;
    ensure!(
        output.status.success(),
        "git {} failed in {}: {}",
        arguments.join(" "),
        root.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).context("git output was not UTF-8")
}

fn git_command(root: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(arguments).stdin(Stdio::null());
    command
}

fn resolve_reporigor(options: &Options) -> Result<PathBuf> {
    match options.reporigor.as_deref() {
        Some(path) => resolve_executable(path),
        None => resolve_adjacent_reporigor(),
    }
}

fn resolve_executable(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_file(),
        "reporigor executable not found at {}",
        path.display()
    );
    path.canonicalize()
        .context("failed to resolve reporigor executable")
}

fn resolve_adjacent_reporigor() -> Result<PathBuf> {
    let current = env::current_exe().context("failed to locate corpus-harness executable")?;
    let candidate = current
        .parent()
        .ok_or_else(|| anyhow!("corpus-harness executable has no parent directory"))?
        .join(format!("reporigor{}", env::consts::EXE_SUFFIX));
    ensure!(
        candidate.is_file(),
        "reporigor executable not found at {}; build it first or pass --reporigor",
        candidate.display()
    );
    resolve_executable(&candidate)
}

fn load_baseline(path: &Path) -> Result<Baseline> {
    let baseline: Baseline = load_toml(path, "corpus baseline")?;
    validate_schema(
        baseline.schema_version,
        BASELINE_SCHEMA_VERSION,
        "corpus baseline",
    )?;
    Ok(baseline)
}

fn validate_baseline(lock: &CorpusLock, baseline: &Baseline) -> Result<()> {
    validate_schema(
        baseline.schema_version,
        BASELINE_SCHEMA_VERSION,
        "corpus baseline",
    )?;
    let entries = lock
        .corpus
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut keys = BTreeSet::new();
    for record in &baseline.results {
        ensure!(
            keys.insert((record.name.as_str(), record.backend.as_str())),
            "duplicate baseline record for {} {}",
            record.name,
            record.backend
        );
        let entry = entries
            .get(record.name.as_str())
            .ok_or_else(|| anyhow!("baseline references unknown corpus {}", record.name))?;
        ensure!(
            record.language == entry.language,
            "{} {} baseline language is {}, expected {}",
            record.name,
            record.backend,
            record.language,
            entry.language
        );
        ensure!(
            record.revision == entry.revision,
            "{} {} baseline revision is {}, expected {}",
            record.name,
            record.backend,
            record.revision,
            entry.revision
        );
        ensure!(
            entry.modes.contains(&record.backend),
            "{} baseline backend {} is not declared by the lock",
            record.name,
            record.backend
        );
        ensure!(
            matches!(record.exit_code, 0 | 2),
            "{} {} baseline has operational exit code {}",
            record.name,
            record.backend,
            record.exit_code
        );
        ensure!(
            record.sha256.len() == 64
                && record
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "{} {} baseline SHA-256 must be 64 lowercase hexadecimal characters",
            record.name,
            record.backend
        );
    }
    Ok(())
}

fn validate_baseline_completeness(lock: &CorpusLock, baseline: &Baseline, options: &Options) -> Result<()> {
    let actual = baseline
        .results
        .iter()
        .map(|record| (record.name.as_str(), record.backend.as_str()))
        .collect::<BTreeSet<_>>();
    let mut missing = Vec::with_capacity(lock.corpus.len());
    for entry in lock.corpus.iter().filter(|entry| is_selected(entry, options)) {
        for mode in &entry.modes {
            if mode == "native" && !options.include_native {
                continue;
            }
            if !actual.contains(&(entry.name.as_str(), mode.as_str())) {
                missing.push(format!("{} {mode}", entry.name));
            }
        }
    }
    ensure!(
        missing.is_empty(),
        "baseline is incomplete for the selected modes: {}",
        missing.join(", ")
    );
    Ok(())
}

fn run_corpora(
    present: &[(&CorpusEntry, PathBuf)],
    options: &Options,
    reporigor: &Path,
    artifact_root: &Path,
) -> Result<Vec<RegressionRecord>> {
    let config = write_corpus_config(artifact_root)?;
    let context = CorpusRunContext {
        options,
        reporigor,
        artifact_root,
    };
    let mut records = collect_records(present, &context, &config)?;
    records.sort_by(|left, right| (&left.name, &left.backend).cmp(&(&right.name, &right.backend)));
    Ok(records)
}

struct CorpusRunContext<'a> {
    options: &'a Options,
    reporigor: &'a Path,
    artifact_root: &'a Path,
}

fn collect_records(
    present: &[(&CorpusEntry, PathBuf)],
    context: &CorpusRunContext<'_>,
    config: &Path,
) -> Result<Vec<RegressionRecord>> {
    let mut records = Vec::new();
    for (entry, checkout) in present {
        for mode in &entry.modes {
            if mode == "native" && !context.options.include_native {
                continue;
            }
            println!("running  {:<18} {mode}", entry.name);
            records.push(run_one(&RunRequest {
                entry,
                checkout,
                mode,
                reporigor: context.reporigor,
                artifact_root: context.artifact_root,
                config,
            })?);
        }
    }
    Ok(records)
}

fn write_corpus_config(artifact_root: &Path) -> Result<PathBuf> {
    let config = artifact_root.join("corpus-config.toml");
    fs::write(
        &config,
        "[dry]\nmax_occurrences_per_window = 2\nmax_candidate_work = 25000000\n",
    )
    .with_context(|| format!("failed to write {}", config.display()))?;
    Ok(config)
}

fn reporigor_arguments(entry: &CorpusEntry, checkout: &Path, mode: &str, config: &Path) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("--config"),
        config.as_os_str().to_owned(),
        OsString::from("--backend"),
        OsString::from(mode),
    ];
    if mode == "native" {
        arguments.push(OsString::from("--allow-project-exec"));
    }
    push_argument_value(&mut arguments, "--language", &entry.language);
    arguments.push(OsString::from("--allow-parse-errors"));
    push_argument_value(&mut arguments, "--format", "json");
    for filter in &entry.filters {
        arguments.push(OsString::from("--filter"));
        arguments.push(OsString::from(filter));
    }
    arguments.extend([
        OsString::from("check"),
        OsString::from("--min-tokens"),
        OsString::from("1000"),
        checkout.as_os_str().to_owned(),
    ]);
    arguments
}

fn push_argument_value(arguments: &mut Vec<OsString>, option: &str, value: impl AsRef<std::ffi::OsStr>) {
    arguments.push(OsString::from(option));
    arguments.push(value.as_ref().to_owned());
}

struct RunRequest<'a> {
    entry: &'a CorpusEntry,
    checkout: &'a Path,
    mode: &'a str,
    reporigor: &'a Path,
    artifact_root: &'a Path,
    config: &'a Path,
}

fn run_one(request: &RunRequest<'_>) -> Result<RegressionRecord> {
    let output_stem = format!("{}.{}", request.entry.name, request.mode);
    let captured = execute_reporigor(request)?;
    write_captured_run(request.artifact_root, &output_stem, &captured)?;
    validate_captured_run(request, &captured)?;
    record_captured_run(request, &output_stem, &captured)
}

fn write_captured_run(artifact_root: &Path, output_stem: &str, captured: &CapturedRun) -> Result<()> {
    let stdout_path = artifact_root.join(format!("{output_stem}.stdout.json"));
    let stderr_path = artifact_root.join(format!("{output_stem}.stderr.txt"));
    write_artifact(&stdout_path, &captured.stdout)?;
    write_artifact(&stderr_path, &captured.stderr)
}

fn write_artifact(path: &Path, contents: &[u8]) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn record_captured_run(
    request: &RunRequest<'_>,
    output_stem: &str,
    captured: &CapturedRun,
) -> Result<RegressionRecord> {
    let mut report = read_report(
        &captured.stdout,
        &captured.stderr,
        captured.status,
        request.entry,
        request.mode,
    )?;
    normalize_report(&mut report, request.checkout);
    let normalized = serde_json::to_vec_pretty(&report).context("failed to normalize report JSON")?;
    fs::write(
        request
            .artifact_root
            .join(format!("{output_stem}.normalized.json")),
        &normalized,
    )
    .context("failed to write normalized report artifact")?;
    regression_record(request.entry, request.mode, captured.status, &report, &normalized)
}

struct CapturedRun {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: ExitStatus,
    reason: WaitReason,
    tree_confirmed_gone: bool,
}

type ReaderHandle = thread::JoinHandle<std::io::Result<Vec<u8>>>;

struct CaptureReaders {
    stdout: ReaderHandle,
    stderr: ReaderHandle,
}

fn execute_reporigor(request: &RunRequest<'_>) -> Result<CapturedRun> {
    let (mut process, readers) = start_reporigor(request)?;
    let timeout = Duration::from_secs(request.entry.timeout_seconds);
    let outcome = process
        .wait_bounded(timeout, CleanupPolicy::default())
        .context("failed while waiting for or cleaning the reporigor process tree")?;
    finish_capture(
        readers,
        outcome.status,
        outcome.reason,
        outcome.termination.tree_confirmed_gone,
    )
}

fn start_reporigor(request: &RunRequest<'_>) -> Result<(ProcessTree, CaptureReaders)> {
    let mut command = Command::new(request.reporigor);
    command.args(reporigor_arguments(
        request.entry,
        request.checkout,
        request.mode,
        request.config,
    ));
    configure_reporigor_capture(&mut command);

    let mut process = ProcessTree::spawn(&mut command).with_context(|| {
        format!(
            "failed to start reporigor for {} {}",
            request.entry.name, request.mode
        )
    })?;
    let stdout = process
        .take_stdout()
        .ok_or_else(|| anyhow!("reporigor stdout pipe was unavailable"))?;
    let stderr = process
        .take_stderr()
        .ok_or_else(|| anyhow!("reporigor stderr pipe was unavailable"))?;
    let stdout_limit = request.entry.max_output_bytes;
    let stdout_reader = thread::spawn(move || read_capped(stdout, stdout_limit));
    let stderr_reader = thread::spawn(move || read_capped(stderr, 1024 * 1024));
    Ok((
        process,
        CaptureReaders {
            stdout: stdout_reader,
            stderr: stderr_reader,
        },
    ))
}

fn configure_reporigor_capture(command: &mut Command) {
    configure_piped_command(command);
}

fn finish_capture(
    readers: CaptureReaders,
    status: ExitStatus,
    reason: WaitReason,
    tree_confirmed_gone: bool,
) -> Result<CapturedRun> {
    let stdout = join_reader(readers.stdout, "stdout")?;
    let stderr = join_reader(readers.stderr, "stderr")?;
    Ok(CapturedRun {
        stdout,
        stderr,
        status,
        reason,
        tree_confirmed_gone,
    })
}

fn validate_captured_run(request: &RunRequest<'_>, captured: &CapturedRun) -> Result<()> {
    validate_cleanup(request, captured)?;
    verify_checkout(request.entry, request.checkout).with_context(|| {
        format!(
            "{} {} changed its pinned checkout",
            request.entry.name, request.mode
        )
    })?;
    validate_run_bounds(request, captured)
}

fn validate_cleanup(request: &RunRequest<'_>, captured: &CapturedRun) -> Result<()> {
    ensure!(
        captured.tree_confirmed_gone,
        "{} {} process-tree cleanup was not confirmed",
        request.entry.name,
        request.mode
    );
    Ok(())
}

fn validate_run_bounds(request: &RunRequest<'_>, captured: &CapturedRun) -> Result<()> {
    if captured.reason == WaitReason::TimedOut {
        bail!(
            "{} {} exceeded its {} second bound",
            request.entry.name,
            request.mode,
            request.entry.timeout_seconds
        );
    }
    ensure!(
        u64::try_from(captured.stdout.len()).unwrap_or(u64::MAX) <= request.entry.max_output_bytes,
        "{} produced more than its {} byte output bound",
        request.entry.name,
        request.entry.max_output_bytes
    );
    Ok(())
}

fn read_report(
    stdout: &[u8],
    stderr: &[u8],
    status: ExitStatus,
    entry: &CorpusEntry,
    mode: &str,
) -> Result<Value> {
    let code = status
        .code()
        .ok_or_else(|| anyhow!("{} {mode} terminated by a signal", entry.name))?;
    ensure!(
        code == 0 || code == 2,
        "{} {mode} failed with exit {code}: {}",
        entry.name,
        String::from_utf8_lossy(stderr)
    );
    let report: Value = serde_json::from_slice(stdout).with_context(|| {
        format!(
            "{} {mode} did not emit valid JSON; stderr: {}",
            entry.name,
            String::from_utf8_lossy(stderr)
        )
    })?;
    ensure!(
        report.pointer("/schema_version").and_then(Value::as_u64) == Some(REPORT_SCHEMA_VERSION),
        "{} {mode} emitted an unsupported report schema",
        entry.name
    );
    Ok(report)
}

fn read_capped(mut input: impl Read, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_reader(reader: thread::JoinHandle<std::io::Result<Vec<u8>>>, stream: &str) -> Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| anyhow!("{stream} capture thread panicked"))?
        .with_context(|| format!("failed to capture reporigor {stream}"))
}

fn normalize_report(report: &mut Value, checkout: &Path) {
    if let Some(root) = report.get_mut("root") {
        *root = Value::String("$CORPUS".to_string());
    }
    normalize_version(report.pointer_mut("/tool/version"));
    if let Some(backends) = report.get_mut("backends").and_then(Value::as_array_mut) {
        for backend in backends {
            normalize_version(backend.get_mut("version"));
        }
    }
    normalize_strings(report, &checkout.to_string_lossy());
}

fn normalize_version(version: Option<&mut Value>) {
    if let Some(value) = version {
        *value = Value::String("$VERSION".to_string());
    }
}

fn normalize_strings(value: &mut Value, checkout: &str) {
    match value {
        Value::String(text) => normalize_string(text, checkout),
        Value::Array(items) => normalize_values(items, checkout),
        Value::Object(fields) => normalize_values(fields.values_mut(), checkout),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn normalize_string(text: &mut String, checkout: &str) {
    if text.contains(checkout) {
        *text = text.replace(checkout, "$CORPUS");
    }
}

fn normalize_values<'a>(values: impl IntoIterator<Item = &'a mut Value>, checkout: &str) {
    for value in values {
        normalize_strings(value, checkout);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct ReportCounts {
    files: u64,
    functions: u64,
    duplicate_groups: u64,
    mutants: u64,
    parse_errors: u64,
    diagnostics: u64,
}

fn regression_record(
    entry: &CorpusEntry,
    mode: &str,
    status: ExitStatus,
    report: &Value,
    normalized: &[u8],
) -> Result<RegressionRecord> {
    let summary = report
        .get("summary")
        .ok_or_else(|| anyhow!("{} {mode} report has no summary", entry.name))?;
    let counts: ReportCounts = serde_json::from_value(summary.clone())
        .with_context(|| format!("{} {mode} report has invalid summary counts", entry.name))?;
    Ok(RegressionRecord {
        name: entry.name.clone(),
        language: entry.language.clone(),
        revision: entry.revision.clone(),
        backend: mode.to_string(),
        exit_code: status.code().ok_or_else(|| anyhow!("missing exit code"))?,
        files: counts.files,
        functions: counts.functions,
        duplicate_groups: counts.duplicate_groups,
        mutants: counts.mutants,
        parse_errors: counts.parse_errors,
        diagnostics: counts.diagnostics,
        sha256: format!("{:x}", Sha256::digest(normalized)),
    })
}

fn write_current(path: &Path, records: &[RegressionRecord]) -> Result<()> {
    let current = Baseline {
        schema_version: BASELINE_SCHEMA_VERSION,
        results: records.to_vec(),
    };
    write_toml(path, &current, "current corpus results")
}

fn merge_baseline(baseline: &mut Baseline, current: Vec<RegressionRecord>) {
    let mut records = baseline
        .results
        .drain(..)
        .map(|record| ((record.name.clone(), record.backend.clone()), record))
        .collect::<BTreeMap<_, _>>();
    for record in current {
        records.insert((record.name.clone(), record.backend.clone()), record);
    }
    baseline.results = records.into_values().collect();
}

fn write_baseline(path: &Path, baseline: &Baseline) -> Result<()> {
    write_toml(path, baseline, "corpus baseline")
}

fn write_toml(path: &Path, value: &(impl Serialize + ?Sized), description: &str) -> Result<()> {
    let rendered =
        toml::to_string_pretty(value).with_context(|| format!("failed to serialize {description}"))?;
    fs::write(path, rendered).with_context(|| format!("failed to write {}", path.display()))
}

fn compare_baseline(baseline: &Baseline, current: &[RegressionRecord]) -> Result<()> {
    let mut expected = BTreeMap::new();
    for record in &baseline.results {
        expected.insert((record.name.as_str(), record.backend.as_str()), record);
    }
    let mut differences = Vec::new();
    for actual in current {
        let key = (actual.name.as_str(), actual.backend.as_str());
        match expected.get(&key) {
            Some(expected) if *expected == actual => {}
            Some(expected) => differences.push(format!(
                "{} {} changed\n  expected: {expected:?}\n  actual:   {actual:?}",
                actual.name, actual.backend
            )),
            None => differences.push(format!(
                "{} {} has no baseline record (sha256 {})",
                actual.name, actual.backend, actual.sha256
            )),
        }
    }
    ensure!(
        differences.is_empty(),
        "corpus regression mismatch:\n{}\nreview target/corpus-harness/*.normalized.json, then run `scripts/corpus-harness update` deliberately",
        differences.join("\n")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{tempdir, TempDir};

    #[test]
    fn committed_lock_is_local_valid_and_covers_every_language() {
        let (_workspace, lock) = committed_lock();
        validate_lock(&lock).unwrap_or_else(|error| panic!("invalid lock: {error:#}"));

        let languages = lock
            .corpus
            .iter()
            .map(|entry| entry.language.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(languages, super::supported_languages().collect::<BTreeSet<_>>());
    }

    #[test]
    fn normalization_removes_checkout_and_build_versions() {
        let mut report = serde_json::json!({
            "root": "/private/corpus/project",
            "tool": { "version": "9.9.9" },
            "backends": [{ "id": "generic", "version": "1.2.3" }],
            "diagnostics": [{ "message": "read /private/corpus/project/src/lib.rs" }]
        });
        normalize_report(&mut report, Path::new("/private/corpus/project"));

        assert_eq!(report["root"], "$CORPUS");
        assert_eq!(report["tool"]["version"], "$VERSION");
        assert_eq!(report["backends"][0]["version"], "$VERSION");
        assert_eq!(report["diagnostics"][0]["message"], "read $CORPUS/src/lib.rs");

        let mut primitives = serde_json::json!([null, true, 1, "unchanged"]);
        normalize_strings(&mut primitives, "/private/corpus/project");
        assert_eq!(primitives, serde_json::json!([null, true, 1, "unchanged"]));
    }

    #[test]
    fn operation_and_option_parsing_cover_supported_inputs() {
        for (name, expected) in OPERATIONS {
            assert_eq!(operation_from_name(name), Some(expected));
            assert_eq!(parse_operation(Some(&OsString::from(name))).ok(), Some(expected));
        }
        assert_eq!(operation_from_name("unknown"), None);
        assert_result_error_contains(
            parse_operation(Some(&OsString::from("unknown"))),
            "unknown operation",
        );
        assert_result_error_contains(parse_operation(None), "operation is required");
    }

    #[test]
    fn value_options_and_switches_parse_together() {
        let workspace = Path::new("/workspace");
        let arguments = strings(&"run --checkout-root /checkouts --reporigor /bin/reporigor --name one --name two --tier scheduled --native --require-all".split_ascii_whitespace().collect::<Vec<_>>());
        let options = parse_options_from(workspace, arguments.into_iter())
            .unwrap_or_else(|error| panic!("parse options: {error:#}"));
        assert_eq!(options.operation, Operation::Run);
        assert_eq!(options.checkout_root, Path::new("/checkouts"));
        assert_eq!(options.reporigor.as_deref(), Some(Path::new("/bin/reporigor")));
        assert_eq!(
            options.names,
            BTreeSet::from(["one".to_string(), "two".to_string()])
        );
        assert_eq!(options.tier, Some(Tier::Scheduled));
        assert!(options.include_native && options.require_all);
    }

    #[test]
    fn option_parser_rejects_invalid_combinations() {
        assert_parse_error(&["verify", "--unknown"], "unknown option");
        assert_parse_error(&["verify", "--checkout-root"], "requires a value");
        assert_parse_error(
            &["verify", "--tier", "scheduled", "--tier", "pull-request"],
            "only once",
        );
        assert_parse_error(&["verify", "--tier", "nightly"], "invalid --tier");
    }

    #[cfg(unix)]
    #[test]
    fn option_parser_rejects_non_utf8_names_and_flags() {
        use std::os::unix::ffi::OsStringExt;

        let invalid = OsString::from_vec(vec![0xff]);
        let name_arguments = vec![
            OsString::from("verify"),
            OsString::from("--name"),
            invalid.clone(),
        ];
        assert_result_error_contains(
            parse_options_from(Path::new("/workspace"), name_arguments.into_iter()),
            "--name must be valid UTF-8",
        );
        let flag_arguments = vec![OsString::from("verify"), invalid];
        assert_result_error_contains(
            parse_options_from(Path::new("/workspace"), flag_arguments.into_iter()),
            "arguments must be valid UTF-8",
        );
    }

    #[test]
    fn committed_validate_workflow_uses_the_checked_in_state() {
        let workspace = workspace_root().unwrap_or_else(|error| panic!("workspace: {error:#}"));
        run_from_workspace(&workspace, strings(&["validate", "--require-all"]).into_iter())
            .unwrap_or_else(|error| panic!("validate workflow: {error:#}"));
    }

    #[test]
    fn committed_baseline_is_parseable_without_running_or_fetching() {
        let (workspace, lock) = committed_lock();
        let baseline = load_baseline(&workspace.join("corpus/baseline.toml"))
            .unwrap_or_else(|error| panic!("baseline: {error:#}"));
        validate_baseline(&lock, &baseline).unwrap_or_else(|error| panic!("invalid baseline: {error:#}"));
    }

    #[test]
    fn baseline_records_are_bound_to_unique_declared_lock_modes() {
        let lock = sample_lock();
        let valid = sample_baseline(true);
        validate_baseline(&lock, &valid).unwrap_or_else(|error| panic!("valid baseline: {error:#}"));

        let mut duplicate = valid.clone();
        duplicate.results.push(duplicate.results[0].clone());
        assert_error_contains(validate_baseline(&lock, &duplicate), "duplicate baseline record");

        for (fault, expected) in [
            (RecordFault::Language, "baseline language"),
            (RecordFault::Revision, "baseline revision"),
            (RecordFault::Backend, "not declared by the lock"),
            (RecordFault::Digest, "lowercase hexadecimal"),
        ] {
            assert_record_fault_rejected(&lock, &valid, fault, expected);
        }
    }

    #[test]
    fn require_all_checks_every_selected_execution_mode() {
        let lock = sample_lock();
        let generic_only = sample_baseline(false);
        let generic_options = sample_options(false);
        assert_baseline_complete(&lock, &generic_only, &generic_options, "generic");

        let native_options = sample_options(true);
        assert_error_contains(
            validate_baseline_completeness(&lock, &generic_only, &native_options),
            "sample native",
        );

        let complete = sample_baseline(true);
        assert_baseline_complete(&lock, &complete, &native_options, "native");
    }

    #[test]
    fn tier_selection_is_strict_and_composes_with_names() {
        assert_eq!(
            parse_tier("pull-request").unwrap_or_else(|error| panic!("tier: {error:#}")),
            Tier::PullRequest
        );
        assert!(parse_tier("nightly").is_err());

        let lock = sample_lock();
        let mut options = sample_options(false);
        options.tier = Some(Tier::Scheduled);
        assert_error_contains(ensure_selection(&lock, &options), "matches no corpus entries");
    }

    #[test]
    fn project_execution_permission_is_native_only() {
        let mut lock = sample_lock();
        let entry = lock.corpus.remove(0);
        let checkout = Path::new("corpus/sample");
        let config = Path::new("/tmp/corpus-config.toml");
        let generic = reporigor_arguments(&entry, checkout, "generic", config);
        let native = reporigor_arguments(&entry, checkout, "native", config);

        assert!(!contains_argument(&generic, "--allow-project-exec"));
        assert_eq!(
            native
                .iter()
                .filter(|argument| argument.to_str() == Some("--allow-project-exec"))
                .count(),
            1
        );
    }

    #[test]
    fn local_checkout_population_and_verification_are_deterministic() {
        let fixture = populated_fixture();
        populate_checked(&fixture);

        let verification = checked(
            verify_checkouts(&fixture.lock, &fixture.options),
            "verify checkout",
        );
        assert_eq!(verification.present.len(), 1);
        assert!(verification.missing.is_empty());
        print_verification(&verification);
        checked(
            verify_operation(&fixture.lock, &fixture.options, false),
            "verify operation",
        );
    }

    #[test]
    fn verification_reports_missing_and_dirty_checkouts() {
        let fixture = local_fixture();
        let missing = checked(
            verify_checkouts(&fixture.lock, &fixture.options),
            "missing checkout",
        );
        assert!(missing.present.is_empty());
        assert_eq!(missing.missing.len(), 1);
        print_verification(&missing);

        populate_checked(&fixture);
        let checkout = fixture.checkout();
        write_checked(&checkout.join("untracked"), "dirty", "write dirty marker");
        assert_result_error_contains(verify_checkout(fixture.entry(), &checkout), "is not clean");
        checked(fs::remove_file(checkout.join("untracked")), "remove dirty marker");
        assert_result_error_contains(
            run_git(&checkout, &["not-a-git-command"]),
            "git not-a-git-command failed",
        );
    }

    #[test]
    fn operation_dispatch_covers_local_verify_and_populate() {
        let fixture = local_fixture();
        let baseline_path = fixture.temp.path().join("baseline.toml");
        dispatch_checked(&fixture, &baseline_path, Operation::Populate);
        dispatch_checked(&fixture, &baseline_path, Operation::Verify);
    }

    #[cfg(unix)]
    #[test]
    fn fake_reporigor_drives_update_run_and_artifact_workflows() {
        let (fixture, executable) = fake_fixture();
        let baseline_path = fixture.temp.path().join("baseline.toml");
        let mut options = fixture.options.clone();
        options.operation = Operation::Update;
        options.include_native = true;
        options.reporigor = Some(executable.clone());
        checked(
            execute_operation(
                fixture.temp.path(),
                &fixture.lock,
                &baseline_path,
                empty_baseline(),
                &options,
            ),
            "update execution",
        );

        let baseline = checked(load_baseline(&baseline_path), "load updated baseline");
        assert_eq!(baseline.results.len(), 2);
        options.operation = Operation::Run;
        options.require_all = true;
        checked(
            execute_operation(
                fixture.temp.path(),
                &fixture.lock,
                &baseline_path,
                baseline,
                &options,
            ),
            "run execution",
        );
        assert!(fixture
            .temp
            .path()
            .join("target/corpus-harness/current.toml")
            .is_file());
        assert_eq!(
            resolve_reporigor(&options).ok().as_deref(),
            executable.canonicalize().ok().as_deref()
        );
    }

    #[cfg(unix)]
    #[test]
    fn corpus_runner_skips_native_mode_when_not_requested() {
        let (fixture, executable) = fake_fixture();
        let artifact_root = fixture.temp.path().join("artifacts");
        checked(fs::create_dir(&artifact_root), "create artifacts");
        let verification = checked(verify_checkouts(&fixture.lock, &fixture.options), "verification");
        let records = checked(
            run_corpora(
                &verification.present,
                &fixture.options,
                &executable,
                &artifact_root,
            ),
            "run generic corpus",
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].backend, "generic");
    }

    #[test]
    fn baseline_comparison_describes_changed_and_missing_records() {
        let actual = sample_record("generic");
        let baseline = Baseline {
            schema_version: BASELINE_SCHEMA_VERSION,
            results: vec![actual.clone()],
        };
        compare_baseline(&baseline, std::slice::from_ref(&actual))
            .unwrap_or_else(|error| panic!("matching baseline: {error:#}"));
        let mut changed = actual.clone();
        changed.files += 1;
        assert_result_error_contains(compare_baseline(&baseline, &[changed]), "changed");
        assert_result_error_contains(
            compare_baseline(&empty_baseline(), &[actual]),
            "no baseline record",
        );
    }

    #[test]
    fn run_validation_rejects_timeouts_output_overflow_and_cleanup_failure() {
        let fixture = local_fixture();
        let mut captured = captured_run(WaitReason::TimedOut, true, Vec::new());
        assert_result_error_contains(validate_run_bounds(&fixture.request(), &captured), "exceeded");
        captured.reason = WaitReason::Exited;
        captured.stdout = vec![0; 4097];
        assert_result_error_contains(validate_run_bounds(&fixture.request(), &captured), "output bound");
        captured.tree_confirmed_gone = false;
        assert_result_error_contains(
            validate_cleanup(&fixture.request(), &captured),
            "cleanup was not confirmed",
        );
    }

    #[test]
    fn executable_resolution_rejects_missing_paths() {
        assert_result_error_contains(
            resolve_executable(Path::new("/definitely/missing/reporigor")),
            "not found",
        );
        let _ = resolve_adjacent_reporigor();
    }

    #[cfg(unix)]
    #[test]
    fn timeout_termination_kills_descendant_process_group() {
        use std::io::{BufRead, BufReader};
        use std::time::Instant;

        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sleep 30 & descendant=$!; printf '%s\\n' \"$descendant\"; wait \"$descendant\"");
        configure_piped_command(&mut command);
        command.stderr(Stdio::null());
        let mut process =
            ProcessTree::spawn(&mut command).unwrap_or_else(|error| panic!("spawn process group: {error}"));
        let stdout = process
            .take_stdout()
            .unwrap_or_else(|| panic!("missing subprocess stdout"));
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .unwrap_or_else(|error| panic!("read descendant PID: {error}"));
        let descendant = line
            .trim()
            .parse::<u32>()
            .unwrap_or_else(|error| panic!("parse descendant PID: {error}"));

        let outcome = process
            .wait_bounded(Duration::from_millis(25), CleanupPolicy::default())
            .unwrap_or_else(|error| panic!("wait for process group: {error}"));
        assert_eq!(outcome.reason, WaitReason::TimedOut);
        assert!(outcome.termination.tree_confirmed_gone);

        let deadline = Instant::now() + Duration::from_secs(2);
        while process_is_running(descendant) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !process_is_running(descendant),
            "descendant {descendant} survived process-group termination"
        );
    }

    struct LocalFixture {
        temp: TempDir,
        lock: CorpusLock,
        options: Options,
        reporigor: PathBuf,
        config: PathBuf,
    }

    impl LocalFixture {
        fn entry(&self) -> &CorpusEntry {
            &self.lock.corpus[0]
        }

        fn checkout(&self) -> PathBuf {
            self.options.checkout_root.join(&self.entry().name)
        }

        fn request(&self) -> RunRequest<'_> {
            RunRequest {
                entry: self.entry(),
                checkout: &self.options.checkout_root,
                mode: "generic",
                reporigor: &self.reporigor,
                artifact_root: self.temp.path(),
                config: &self.config,
            }
        }
    }

    fn local_fixture() -> LocalFixture {
        let temp = tempdir().unwrap_or_else(|error| panic!("temporary fixture: {error}"));
        let source = temp.path().join("origin");
        fs::create_dir(&source).unwrap_or_else(|error| panic!("create origin: {error}"));
        initialize_repository(&source);
        let revision = git_stdout(&source, &["rev-parse", "--verify", "HEAD"])
            .unwrap_or_else(|error| panic!("read fixture revision: {error:#}"));
        let entry = local_entry(&source, revision.trim());
        let options = local_options(temp.path().join("checkouts"));
        LocalFixture {
            reporigor: temp.path().join("reporigor"),
            config: temp.path().join("config.toml"),
            temp,
            lock: CorpusLock {
                schema_version: LOCK_SCHEMA_VERSION,
                corpus: vec![entry],
            },
            options,
        }
    }

    fn populated_fixture() -> LocalFixture {
        let fixture = local_fixture();
        populate_checked(&fixture);
        fixture
    }

    fn populate_checked(fixture: &LocalFixture) {
        checked(populate(&fixture.lock, &fixture.options), "populate fixture");
    }

    fn dispatch_checked(fixture: &LocalFixture, baseline_path: &Path, operation: Operation) {
        let mut options = fixture.options.clone();
        options.operation = operation;
        checked(
            dispatch_operation(
                fixture.temp.path(),
                &fixture.lock,
                baseline_path,
                empty_baseline(),
                &options,
            ),
            "dispatch operation",
        );
    }

    fn initialize_repository(source: &Path) {
        run_test_git_line(source, "init --quiet");
        run_test_git_line(source, "config user.email corpus@example.invalid");
        run_test_git(source, &["config", "user.name", "Corpus Harness"]);
        write_checked(
            &source.join("fixture.txt"),
            "deterministic fixture\n",
            "write fixture",
        );
        run_test_git_line(source, "add fixture.txt");
        run_test_git_line(source, "commit --quiet -m fixture");
    }

    fn run_test_git_line(root: &Path, command: &str) {
        run_test_git(root, &command.split_ascii_whitespace().collect::<Vec<_>>());
    }

    fn run_test_git(root: &Path, arguments: &[&str]) {
        run_git(root, arguments).unwrap_or_else(|error| panic!("git {}: {error:#}", arguments.join(" ")));
    }

    fn local_entry(source: &Path, revision: &str) -> CorpusEntry {
        fixture_entry(source.to_string_lossy().into_owned(), revision.to_string())
    }

    fn fixture_entry(repository: String, revision: String) -> CorpusEntry {
        CorpusEntry {
            language: "rust".to_string(),
            name: "sample".to_string(),
            repository,
            revision,
            license: "MIT".to_string(),
            tier: "pull-request".to_string(),
            modes: vec!["generic".to_string(), "native".to_string()],
            filters: Vec::new(),
            timeout_seconds: 5,
            max_output_bytes: 4096,
        }
    }

    fn local_options(checkout_root: PathBuf) -> Options {
        Options {
            operation: Operation::Verify,
            checkout_root,
            reporigor: None,
            include_native: false,
            require_all: true,
            names: BTreeSet::new(),
            tier: None,
        }
    }

    #[cfg(unix)]
    fn write_fake_reporigor(root: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let executable = root.join("fake-reporigor");
        let report = r#"{"schema_version":1,"root":"/variable/root","tool":{"version":"9.9.9"},"backends":[{"version":"1.2.3"}],"summary":{"files":1,"functions":2,"duplicate_groups":0,"mutants":3,"parse_errors":0,"diagnostics":0}}"#;
        fs::write(&executable, format!("#!/bin/sh\nprintf '%s\\n' '{report}'\n"))
            .unwrap_or_else(|error| panic!("write fake reporigor: {error}"));
        let executable_permissions = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&executable, executable_permissions)
            .unwrap_or_else(|error| panic!("make fake reporigor executable: {error}"));
        executable
    }

    #[cfg(unix)]
    fn fake_fixture() -> (LocalFixture, PathBuf) {
        let fixture = populated_fixture();
        let executable = write_fake_reporigor(fixture.temp.path());
        (fixture, executable)
    }

    fn captured_run(reason: WaitReason, tree_confirmed_gone: bool, stdout: Vec<u8>) -> CapturedRun {
        let status = git_command(Path::new("."), &["--version"])
            .status()
            .unwrap_or_else(|error| panic!("obtain successful status: {error}"));
        CapturedRun {
            stdout,
            stderr: Vec::new(),
            status,
            reason,
            tree_confirmed_gone,
        }
    }

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn assert_parse_error(arguments: &[&str], expected: &str) {
        assert_result_error_contains(
            parse_options_from(Path::new("/workspace"), strings(arguments).into_iter()),
            expected,
        );
    }

    fn empty_baseline() -> Baseline {
        Baseline {
            schema_version: BASELINE_SCHEMA_VERSION,
            results: Vec::new(),
        }
    }

    fn sample_lock() -> CorpusLock {
        let mut entry = fixture_entry(
            "https://github.com/example/sample.git".to_string(),
            "1".repeat(40),
        );
        entry.filters.push("src/".to_string());
        entry.timeout_seconds = 30;
        entry.max_output_bytes = 1024;
        CorpusLock {
            schema_version: LOCK_SCHEMA_VERSION,
            corpus: vec![entry],
        }
    }

    fn committed_lock() -> (PathBuf, CorpusLock) {
        let workspace = workspace_root().unwrap_or_else(|error| panic!("workspace: {error:#}"));
        let lock = load_lock(&workspace.join("corpus/corpus.lock.toml"))
            .unwrap_or_else(|error| panic!("lock: {error:#}"));
        (workspace, lock)
    }

    #[derive(Clone, Copy)]
    enum RecordFault {
        Language,
        Revision,
        Backend,
        Digest,
    }

    fn assert_record_fault_rejected(
        lock: &CorpusLock,
        baseline: &Baseline,
        fault: RecordFault,
        expected: &str,
    ) {
        let mut changed = baseline.clone();
        let record = &mut changed.results[0];
        match fault {
            RecordFault::Language => record.language = String::from("python"),
            RecordFault::Revision => record.revision = "0".repeat(40),
            RecordFault::Backend => {
                record.backend.clear();
                record.backend.push_str("invented");
            }
            RecordFault::Digest => record.sha256 = ["A"; 64].concat(),
        }
        assert_error_contains(validate_baseline(lock, &changed), expected);
    }

    fn assert_baseline_complete(lock: &CorpusLock, baseline: &Baseline, options: &Options, label: &str) {
        validate_baseline_completeness(lock, baseline, options)
            .unwrap_or_else(|error| panic!("{label} completeness: {error:#}"));
    }

    fn sample_baseline(include_native: bool) -> Baseline {
        let mut results = vec![sample_record("generic")];
        if include_native {
            results.push(sample_record("native"));
        }
        Baseline {
            schema_version: BASELINE_SCHEMA_VERSION,
            results,
        }
    }

    fn sample_record(backend: &str) -> RegressionRecord {
        RegressionRecord {
            name: "sample".to_string(),
            language: "rust".to_string(),
            revision: "1".repeat(40),
            backend: backend.to_string(),
            exit_code: 0,
            files: 1,
            functions: 1,
            duplicate_groups: 0,
            mutants: 1,
            parse_errors: 0,
            diagnostics: 0,
            sha256: "a".repeat(64),
        }
    }

    fn sample_options(include_native: bool) -> Options {
        Options {
            operation: Operation::Validate,
            checkout_root: PathBuf::from("unused"),
            reporigor: None,
            include_native,
            require_all: true,
            names: BTreeSet::new(),
            tier: None,
        }
    }

    fn contains_argument(arguments: &[OsString], expected: &str) -> bool {
        arguments
            .iter()
            .any(|argument| argument.to_str() == Some(expected))
    }

    #[cfg(unix)]
    fn process_is_running(process: u32) -> bool {
        let output = Command::new("ps")
            .args(["-o", "stat=", "-p", &process.to_string()])
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("inspect descendant process: {error}"));
        output.status.success()
            && !String::from_utf8_lossy(&output.stdout)
                .trim_start()
                .starts_with('Z')
    }

    fn assert_error_contains<T>(result: Result<T>, expected: &str) {
        assert_result_error_contains(result, expected);
    }

    fn assert_result_error_contains<T>(result: Result<T>, expected: &str) {
        let error = result
            .err()
            .unwrap_or_else(|| panic!("expected error containing {expected:?}"));
        assert!(
            error.to_string().contains(expected),
            "expected {expected:?}, got {error:#}"
        );
    }

    fn checked<T, E: std::fmt::Display>(result: std::result::Result<T, E>, context: &str) -> T {
        result.unwrap_or_else(|error| panic!("{context}: {error}"))
    }

    fn write_checked(path: &Path, contents: &str, context: &str) {
        checked(fs::write(path, contents), context);
    }
}
