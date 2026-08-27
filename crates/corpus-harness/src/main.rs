use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};
use reporigor_process_tree::{CleanupPolicy, ProcessTree, WaitReason};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const LOCK_SCHEMA_VERSION: u32 = 1;
const BASELINE_SCHEMA_VERSION: u32 = 1;
const REPORT_SCHEMA_VERSION: u64 = 1;
const LANGUAGES: [&str; 8] = [
    "bash",
    "c",
    "cpp",
    "objective-c",
    "python",
    "rust",
    "swift",
    "typescript",
];

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
struct RegressionRecord {
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

#[derive(Debug)]
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
    let workspace = workspace_root()?;
    let options = parse_options(&workspace)?;
    let lock_path = workspace.join("corpus/corpus.lock.toml");
    let lock = load_lock(&lock_path)?;
    validate_lock(&lock)?;
    validate_selected_names(&lock, &options.names)?;
    ensure_selection(&lock, &options)?;
    let baseline_path = workspace.join("corpus/baseline.toml");
    let mut baseline = load_baseline(&baseline_path)?;
    validate_baseline(&lock, &baseline)?;

    if options.operation == Operation::Validate {
        if options.require_all {
            validate_baseline_completeness(&lock, &baseline, &options)?;
        }
        let selected = lock
            .corpus
            .iter()
            .filter(|entry| is_selected(entry, &options))
            .count();
        println!(
            "validated {selected} selected lock entries and {} baseline records",
            baseline.results.len()
        );
        return Ok(());
    }

    if options.operation == Operation::Populate {
        populate(&lock, &options)?;
    }

    let verification = verify_checkouts(&lock, &options)?;
    print_verification(&verification);
    if options.require_all && !verification.missing.is_empty() {
        bail!(
            "{} selected corpus checkout(s) are missing under {}; run `scripts/corpus-harness populate` explicitly",
            verification.missing.len(),
            options.checkout_root.display()
        );
    }
    if options.operation == Operation::Verify || options.operation == Operation::Populate {
        return Ok(());
    }
    ensure!(
        !verification.present.is_empty(),
        "no selected pinned checkouts are present under {}; populate one or pass --checkout-root",
        options.checkout_root.display()
    );

    if options.operation == Operation::Run && options.require_all {
        validate_baseline_completeness(&lock, &baseline, &options)?;
    }

    let reporigor = resolve_reporigor(&options)?;
    let artifact_root = workspace.join("target/corpus-harness");
    fs::create_dir_all(&artifact_root)
        .with_context(|| format!("failed to create {}", artifact_root.display()))?;
    let current = run_corpora(&verification.present, &options, &reporigor, &artifact_root)?;
    write_current(&artifact_root.join("current.toml"), &current)?;

    if options.operation == Operation::Update {
        merge_baseline(&mut baseline, current);
        validate_baseline(&lock, &baseline)?;
        if options.require_all {
            validate_baseline_completeness(&lock, &baseline, &options)?;
        }
        write_baseline(&baseline_path, &baseline)?;
        println!("updated {}", baseline_path.display());
        return Ok(());
    }

    compare_baseline(&baseline, &current)?;
    println!("corpus regression baseline matches");
    Ok(())
}

fn workspace_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .context("failed to resolve workspace root")
}

fn parse_options(workspace: &Path) -> Result<Options> {
    let mut arguments = env::args_os().skip(1);
    let operation = match arguments.next().as_deref().and_then(|value| value.to_str()) {
        Some("validate") => Operation::Validate,
        Some("verify") => Operation::Verify,
        Some("run") => Operation::Run,
        Some("update") => Operation::Update,
        Some("populate") => Operation::Populate,
        Some("help" | "--help" | "-h") => {
            print_usage();
            std::process::exit(0);
        }
        Some(other) => bail!("unknown operation {other:?}\n\n{}", usage()),
        None => bail!("an operation is required\n\n{}", usage()),
    };

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
        match argument.to_str() {
            Some("--checkout-root") => {
                options.checkout_root = PathBuf::from(next_value(&mut arguments, "--checkout-root")?);
            }
            Some("--reporigor") => {
                options.reporigor = Some(PathBuf::from(next_value(&mut arguments, "--reporigor")?));
            }
            Some("--name") => {
                let name = next_value(&mut arguments, "--name")?
                    .into_string()
                    .map_err(|_| anyhow!("--name must be valid UTF-8"))?;
                options.names.insert(name);
            }
            Some("--tier") => {
                ensure!(options.tier.is_none(), "--tier may be supplied only once");
                let tier = next_value(&mut arguments, "--tier")?
                    .into_string()
                    .map_err(|_| anyhow!("--tier must be valid UTF-8"))?;
                options.tier = Some(parse_tier(&tier)?);
            }
            Some("--native") => options.include_native = true,
            Some("--require-all") => options.require_all = true,
            Some(other) => bail!("unknown option {other:?}\n\n{}", usage()),
            None => bail!("arguments must be valid UTF-8 except for filesystem paths"),
        }
    }
    Ok(options)
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
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read corpus lock {}", path.display()))?;
    toml::from_str(&source).with_context(|| format!("failed to parse corpus lock {}", path.display()))
}

fn validate_lock(lock: &CorpusLock) -> Result<()> {
    ensure!(
        lock.schema_version == LOCK_SCHEMA_VERSION,
        "unsupported corpus lock schema {}; expected {LOCK_SCHEMA_VERSION}",
        lock.schema_version
    );
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
            LANGUAGES.contains(&entry.language.as_str()),
            "unsupported language {}",
            entry.language
        );
        languages.insert(entry.language.as_str());
        ensure!(
            entry.revision.len() == 40 && entry.revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
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
    for language in LANGUAGES {
        ensure!(
            languages.contains(language),
            "corpus lock has no {language} entry"
        );
    }
    Ok(())
}

fn validate_selected_names(lock: &CorpusLock, selected: &BTreeSet<String>) -> Result<()> {
    let known = lock
        .corpus
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<BTreeSet<_>>();
    for name in selected {
        ensure!(known.contains(name.as_str()), "unknown corpus name {name:?}");
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

fn verify_checkouts<'a>(lock: &'a CorpusLock, options: &Options) -> Result<Verification<'a>> {
    let mut present = Vec::new();
    let mut missing = Vec::new();
    for entry in lock.corpus.iter().filter(|entry| is_selected(entry, options)) {
        let checkout = options.checkout_root.join(&entry.name);
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
    ensure!(
        status.trim().is_empty(),
        "{} is not clean:\n{status}",
        checkout.display()
    );
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
    for entry in lock.corpus.iter().filter(|entry| is_selected(entry, options)) {
        let checkout = options.checkout_root.join(&entry.name);
        if checkout.exists() {
            println!("kept     {:<18} already exists; no fetch attempted", entry.name);
            continue;
        }
        fs::create_dir(&checkout).with_context(|| format!("failed to create {}", checkout.display()))?;
        run_git(&checkout, &["init", "--quiet"])?;
        run_git(&checkout, &["remote", "add", "origin", &entry.repository])?;
        run_git(
            &checkout,
            &["fetch", "--quiet", "--depth=1", "origin", &entry.revision],
        )?;
        run_git(&checkout, &["checkout", "--quiet", "--detach", &entry.revision])?;
        println!("populated {:<18} {}", entry.name, entry.revision);
    }
    Ok(())
}

fn run_git(root: &Path, arguments: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("failed to start git in {}", root.display()))?;
    ensure!(
        status.success(),
        "git {} failed with {status}",
        arguments.join(" ")
    );
    Ok(())
}

fn git_stdout(root: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .stdin(Stdio::null())
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

fn resolve_reporigor(options: &Options) -> Result<PathBuf> {
    if let Some(path) = &options.reporigor {
        ensure!(
            path.is_file(),
            "reporigor executable not found at {}",
            path.display()
        );
        return path
            .canonicalize()
            .context("failed to resolve reporigor executable");
    }
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
    candidate
        .canonicalize()
        .context("failed to resolve reporigor executable")
}

fn load_baseline(path: &Path) -> Result<Baseline> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read corpus baseline {}", path.display()))?;
    let baseline: Baseline = toml::from_str(&source)
        .with_context(|| format!("failed to parse corpus baseline {}", path.display()))?;
    ensure!(
        baseline.schema_version == BASELINE_SCHEMA_VERSION,
        "unsupported corpus baseline schema {}",
        baseline.schema_version
    );
    Ok(baseline)
}

fn validate_baseline(lock: &CorpusLock, baseline: &Baseline) -> Result<()> {
    ensure!(
        baseline.schema_version == BASELINE_SCHEMA_VERSION,
        "unsupported corpus baseline schema {}",
        baseline.schema_version
    );
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
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
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
    let mut missing = Vec::new();
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
    let config = artifact_root.join("corpus-config.toml");
    fs::write(
        &config,
        "[dry]\nmax_occurrences_per_window = 2\nmax_candidate_work = 25000000\n",
    )
    .with_context(|| format!("failed to write {}", config.display()))?;
    let mut records = Vec::new();
    for (entry, checkout) in present {
        for mode in &entry.modes {
            if mode == "native" && !options.include_native {
                continue;
            }
            println!("running  {:<18} {mode}", entry.name);
            records.push(run_one(entry, checkout, mode, reporigor, artifact_root, &config)?);
        }
    }
    records.sort_by(|left, right| (&left.name, &left.backend).cmp(&(&right.name, &right.backend)));
    Ok(records)
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
    arguments.extend([
        OsString::from("--language"),
        OsString::from(&entry.language),
        OsString::from("--allow-parse-errors"),
        OsString::from("--format"),
        OsString::from("json"),
    ]);
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

fn run_one(
    entry: &CorpusEntry,
    checkout: &Path,
    mode: &str,
    reporigor: &Path,
    artifact_root: &Path,
    config: &Path,
) -> Result<RegressionRecord> {
    let output_stem = format!("{}.{}", entry.name, mode);
    let stdout_path = artifact_root.join(format!("{output_stem}.stdout.json"));
    let stderr_path = artifact_root.join(format!("{output_stem}.stderr.txt"));

    let mut command = Command::new(reporigor);
    command
        .args(reporigor_arguments(entry, checkout, mode, config))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut process = ProcessTree::spawn(&mut command)
        .with_context(|| format!("failed to start reporigor for {} {mode}", entry.name))?;
    let stdout = process
        .take_stdout()
        .ok_or_else(|| anyhow!("reporigor stdout pipe was unavailable"))?;
    let stderr = process
        .take_stderr()
        .ok_or_else(|| anyhow!("reporigor stderr pipe was unavailable"))?;
    let stdout_limit = entry.max_output_bytes;
    let stdout_reader = thread::spawn(move || read_capped(stdout, stdout_limit));
    let stderr_reader = thread::spawn(move || read_capped(stderr, 1024 * 1024));
    let timeout = Duration::from_secs(entry.timeout_seconds);
    let outcome = process
        .wait_bounded(timeout, CleanupPolicy::default())
        .context("failed while waiting for or cleaning the reporigor process tree")?;
    ensure!(
        outcome.termination.tree_confirmed_gone,
        "{} {mode} process-tree cleanup was not confirmed",
        entry.name
    );
    let stdout = join_reader(stdout_reader, "stdout")?;
    let stderr = join_reader(stderr_reader, "stderr")?;
    fs::write(&stdout_path, &stdout).with_context(|| format!("failed to write {}", stdout_path.display()))?;
    fs::write(&stderr_path, &stderr).with_context(|| format!("failed to write {}", stderr_path.display()))?;
    verify_checkout(entry, checkout)
        .with_context(|| format!("{} {mode} changed its pinned checkout", entry.name))?;
    if outcome.reason == WaitReason::TimedOut {
        bail!(
            "{} {mode} exceeded its {} second bound",
            entry.name,
            entry.timeout_seconds
        );
    }
    ensure!(
        u64::try_from(stdout.len()).unwrap_or(u64::MAX) <= entry.max_output_bytes,
        "{} produced more than its {} byte output bound",
        entry.name,
        entry.max_output_bytes
    );
    let mut report = read_report(&stdout, &stderr, outcome.status, entry, mode)?;
    normalize_report(&mut report, checkout);
    let normalized = serde_json::to_vec_pretty(&report).context("failed to normalize report JSON")?;
    fs::write(
        artifact_root.join(format!("{output_stem}.normalized.json")),
        &normalized,
    )
    .context("failed to write normalized report artifact")?;
    regression_record(entry, mode, outcome.status, &report, &normalized)
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
    if let Some(version) = report.pointer_mut("/tool/version") {
        *version = Value::String("$VERSION".to_string());
    }
    if let Some(backends) = report.get_mut("backends").and_then(Value::as_array_mut) {
        for backend in backends {
            if let Some(version) = backend.get_mut("version") {
                *version = Value::String("$VERSION".to_string());
            }
        }
    }
    normalize_strings(report, &checkout.to_string_lossy());
}

fn normalize_strings(value: &mut Value, checkout: &str) {
    match value {
        Value::String(text) => {
            if text.contains(checkout) {
                *text = text.replace(checkout, "$CORPUS");
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_strings(item, checkout);
            }
        }
        Value::Object(fields) => {
            for field in fields.values_mut() {
                normalize_strings(field, checkout);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
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
    let count = |field: &str| -> Result<u64> {
        summary
            .get(field)
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("{} {mode} summary has no integer {field}", entry.name))
    };
    Ok(RegressionRecord {
        name: entry.name.clone(),
        language: entry.language.clone(),
        revision: entry.revision.clone(),
        backend: mode.to_string(),
        exit_code: status.code().ok_or_else(|| anyhow!("missing exit code"))?,
        files: count("files")?,
        functions: count("functions")?,
        duplicate_groups: count("duplicate_groups")?,
        mutants: count("mutants")?,
        parse_errors: count("parse_errors")?,
        diagnostics: count("diagnostics")?,
        sha256: format!("{:x}", Sha256::digest(normalized)),
    })
}

fn write_current(path: &Path, records: &[RegressionRecord]) -> Result<()> {
    let current = Baseline {
        schema_version: BASELINE_SCHEMA_VERSION,
        results: records.to_vec(),
    };
    let rendered = toml::to_string_pretty(&current).context("failed to serialize current corpus results")?;
    fs::write(path, rendered).with_context(|| format!("failed to write {}", path.display()))
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
    let rendered = toml::to_string_pretty(baseline).context("failed to serialize corpus baseline")?;
    fs::write(path, rendered).with_context(|| format!("failed to write {}", path.display()))
}

fn compare_baseline(baseline: &Baseline, current: &[RegressionRecord]) -> Result<()> {
    let expected = baseline
        .results
        .iter()
        .map(|record| ((record.name.as_str(), record.backend.as_str()), record))
        .collect::<BTreeMap<_, _>>();
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

    #[test]
    fn committed_lock_is_local_valid_and_covers_every_language() {
        let workspace = workspace_root().unwrap_or_else(|error| panic!("workspace: {error:#}"));
        let lock = load_lock(&workspace.join("corpus/corpus.lock.toml"))
            .unwrap_or_else(|error| panic!("lock: {error:#}"));
        validate_lock(&lock).unwrap_or_else(|error| panic!("invalid lock: {error:#}"));

        let languages = lock
            .corpus
            .iter()
            .map(|entry| entry.language.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(languages, BTreeSet::from(LANGUAGES));
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
    }

    #[test]
    fn committed_baseline_is_parseable_without_running_or_fetching() {
        let workspace = workspace_root().unwrap_or_else(|error| panic!("workspace: {error:#}"));
        let lock = load_lock(&workspace.join("corpus/corpus.lock.toml"))
            .unwrap_or_else(|error| panic!("lock: {error:#}"));
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

        let mut language = valid.clone();
        language.results[0].language = "python".to_string();
        assert_error_contains(validate_baseline(&lock, &language), "baseline language");

        let mut revision = valid.clone();
        revision.results[0].revision = "0".repeat(40);
        assert_error_contains(validate_baseline(&lock, &revision), "baseline revision");

        let mut backend = valid.clone();
        backend.results[0].backend = "invented".to_string();
        assert_error_contains(validate_baseline(&lock, &backend), "not declared by the lock");

        let mut digest = valid;
        digest.results[0].sha256 = "A".repeat(64);
        assert_error_contains(validate_baseline(&lock, &digest), "lowercase hexadecimal");
    }

    #[test]
    fn require_all_checks_every_selected_execution_mode() {
        let lock = sample_lock();
        let generic_only = sample_baseline(false);
        let generic_options = sample_options(false);
        validate_baseline_completeness(&lock, &generic_only, &generic_options)
            .unwrap_or_else(|error| panic!("generic completeness: {error:#}"));

        let native_options = sample_options(true);
        assert_error_contains(
            validate_baseline_completeness(&lock, &generic_only, &native_options),
            "sample native",
        );

        let complete = sample_baseline(true);
        validate_baseline_completeness(&lock, &complete, &native_options)
            .unwrap_or_else(|error| panic!("native completeness: {error:#}"));
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

    #[cfg(unix)]
    #[test]
    fn timeout_termination_kills_descendant_process_group() {
        use std::io::{BufRead, BufReader};
        use std::time::Instant;

        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "sleep 30 & descendant=$!; printf '%s\\n' \"$descendant\"; wait \"$descendant\"",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
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

    fn sample_lock() -> CorpusLock {
        CorpusLock {
            schema_version: LOCK_SCHEMA_VERSION,
            corpus: vec![CorpusEntry {
                language: "rust".to_string(),
                name: "sample".to_string(),
                repository: "https://github.com/example/sample.git".to_string(),
                revision: "1".repeat(40),
                license: "MIT".to_string(),
                tier: "pull-request".to_string(),
                modes: vec!["generic".to_string(), "native".to_string()],
                filters: vec!["src/".to_string()],
                timeout_seconds: 30,
                max_output_bytes: 1024,
            }],
        }
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

    fn assert_error_contains(result: Result<()>, expected: &str) {
        let error = result
            .err()
            .unwrap_or_else(|| panic!("expected error containing {expected:?}"));
        assert!(
            error.to_string().contains(expected),
            "expected {expected:?}, got {error:#}"
        );
    }
}
