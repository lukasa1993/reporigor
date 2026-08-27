# Configuration reference

`reporigor` accepts one project-root path, optional global analysis settings,
and command-specific quality settings. Configuration is TOML and is shared by
all supported languages.

## Configuration discovery

For a command such as:

```bash
reporigor check path/to/project
```

the positional path is canonicalized and becomes the project root. Unless
`--config` is supplied, `reporigor` checks these files at that root, in order:

1. `reporigor.toml`
2. `.reporigor.toml`

If neither exists, the built-in defaults are used. An explicit path is read
exactly as supplied; a relative `--config` path is therefore relative to the
process working directory, not the analyzed project root.

```bash
reporigor --config config/quality.toml check path/to/project
```

The root must already be an accessible directory. An unreadable configuration,
invalid TOML, unknown key, or value of the wrong type is an operational error.
Configuration and filesystem-only project manifests are limited to 1 MiB,
must be regular UTF-8 files, and are checked before parsing. An automatically
discovered configuration that resolves outside the canonical project root is
an operational error. Unsafe or unreadable optional provider metadata such as
`package.json` or `pyproject.toml` is ignored with a warning and cannot make a
provider applicable. An explicit `--config` path may intentionally live outside
the analyzed root, but the same regular-file and size limits apply.

On Unix, repository-controlled metadata is opened relative to an anchored root
directory descriptor with no-follow component checks and file-identity
verification. Other platforms use canonical containment and file-handle
identity verification. A same-user process concurrently replacing filesystem
namespace components is outside the supported threat model on platforms that
cannot provide the anchored Unix open semantics; the implementation does not
claim race-free containment there.

## Complete TOML example

```toml
backend = "auto"
include_tests = false
allow_parse_errors = false
max_source_bytes = 8388608

[crap]
fail_over = 6.0
allow_missing_coverage = false
allow_empty = false

[dry]
min_tokens = 30
max_groups = 50
max_occurrences_per_window = 100
max_total_windows = 1000000
max_fingerprint_buckets = 500000
max_candidate_work = 10000000
fail = false

[mutation]
timeout_seconds = 120.0
# test_command = "cargo test --workspace"
# validation_command = "cargo check --workspace"
# max_mutants = 100
```

All documented sections and fields are optional. A missing section or field
receives its default; an unknown top-level or section key is rejected so a typo
cannot silently weaken a gate.

### Top-level settings

| Key | Type | Default | Meaning |
|---|---|---:|---|
| `backend` | `"auto"`, `"native"`, or `"generic"` | `"auto"` | Backend routing policy. |
| `include_tests` | Boolean | `false` | Include language-recognized test files and test directories. |
| `allow_parse_errors` | Boolean | `false` | Keep valid syntax subtrees and report parse diagnostics instead of stopping at the first malformed file. |
| `max_source_bytes` | Integer, `1..=67108864` bytes | `8388608` | Fail the command when any selected source is larger than this limit. The 64 MiB ceiling is immutable. |

Backend behavior is described in [ARCHITECTURE.md](ARCHITECTURE.md). In short,
`auto` is filesystem-only and uses the compiled-in Tree-sitter grammars unless
`--allow-project-exec` grants access to existing project toolchains. With that
grant it prefers project-aware analysis and records fallbacks. `native`
requires both the grant and applicable project/toolchain prerequisites;
`generic` never runs project-provider subprocesses.

The source-size policy is fail-closed and backend-independent. A selected file
whose size is greater than `max_source_bytes` produces an operational error in
`generic`, `auto`, and `native`; it is never converted into an empty successful
analysis or silently omitted during fallback. A file exactly at the limit is
accepted. Files excluded by language, filter, or test policy are not selected
and therefore do not trigger this limit.

Repository configuration cannot raise the immutable resource ceilings. At
most 100,000 selected source files and 1 GiB of aggregate selected source
metadata are accepted per analysis. Discovery accounts for these budgets before
syntax parsing or project-source validation begins. Exceeding either aggregate
limit is a typed operational error rather than a partial report.

`dry`, `mutate`, and `check` fail operationally when language/filter/test/ignore
selection yields no source files. Standalone `crap` does the same unless its
explicit `allow_empty` policy is enabled; an empty selection is never silently
reported as a successful quality check.

Selected generic source text must be valid UTF-8. Invalid byte sequences are
rejected before Tree-sitter parsing even when `allow_parse_errors` is enabled;
that option permits recoverable syntax errors, not lossy source decoding.

### `[crap]`

| Key | Type | Default | Meaning |
|---|---|---:|---|
| `fail_over` | Non-negative finite number | `6.0` | Exit 2 when at least one known CRAP score is strictly greater than this value. |
| `allow_missing_coverage` | Boolean | `false` | Permit functions that could not be matched to executable coverage lines. |
| `allow_empty` | Boolean | `false` | Permit a standalone `crap` run that discovers no functions. |

CRAP uses `C² × (1 - coverage/100)³ + C`. Coverage is line coverage within the
inclusive function line range. Supported inputs are LCOV, Cobertura XML,
coverage.py JSON, Istanbul JSON, and LLVM coverage-export JSON. `--coverage`
may name a report file or a directory containing a conventionally named report.

Coverage ingestion is fail-closed under fixed resource ceilings. These limits
are not configurable by a report or repository:

| Resource | Limit |
|---|---:|
| One report file or direct parser input | 64 MiB |
| One report-provided source path | 32 KiB UTF-8 |
| Directory discovery | 100,000 entries and 10,000 directories |
| Conventional reports found during discovery | 4,096 files and 128 MiB aggregate metadata size |
| Normalized report output | 50,000 source paths and 2,000,000 unique executable lines |
| Executable lines for one normalized source path | 500,000 |
| Parsed line/statement/segment candidates | 4,000,000 |
| Cobertura intermediate data | 128 sources, 50,000 classes, and 2,000,000 class-line records |
| Cobertura raw/source-qualified resolution | 1,000,000 candidates |
| Cobertura XML structure | DTDs rejected; 256 attributes per element; 4,096 namespace declarations; depth 1,024; 1 KiB names; 32 KiB attribute/text/entity values; 64 KiB markup |
| LLVM code-region expansion | 100,000 lines per region and 2,000,000 lines per report |

An explicit report may live outside the analyzed project, but it must be a
regular, non-symlink file. Character devices, FIFOs, symlinks, and sparse files
whose metadata exceeds the file ceiling are rejected before reading. Directory
discovery does not follow symlinks, keeps every selected report inside the
canonical requested directory, and accounts for candidate sizes before reading
one. LLVM expansion and Cobertura source/class resolution are fully preflighted
before their generated-line loops begin. Cobertura also receives a bounded
linear markup pass before streaming XML parsing, so attribute/namespace bombs,
deep nesting, duplicate attributes, and DTD/entity declarations fail before
unbounded parser work.

When no coverage path is supplied, function coverage and CRAP scores are
unknown. The standalone `crap` command then exits 1 unless missing coverage is
allowed. `check` reports the missing values but does not apply the standalone
`allow_missing_coverage` or `allow_empty` guards.

### `[dry]`

| Key | Type | Default | Meaning |
|---|---|---:|---|
| `min_tokens` | Integer, at least 4 | `30` | Smallest normalized token window considered a duplicate. |
| `max_groups` | Positive integer | `50` | Maximum duplicate groups retained after deterministic sorting. |
| `max_occurrences_per_window` | Integer, at least 2 | `100` | Retain the earliest deterministic occurrences for one fingerprint; later occurrences are intentionally omitted. |
| `max_total_windows` | Positive integer, at most `2000000` | `1000000` | Fail before indexing when selected token streams contain more minimum-size windows. |
| `max_fingerprint_buckets` | Positive integer, at most `1000000` | `500000` | Fail when rolling-window indexing would create more distinct fingerprint buckets. |
| `max_candidate_work` | Positive integer, at most `25000000` | `10000000` | Fail when candidate dispatch, exact token comparison, and maximal extension exceed this many work units. |
| `fail` | Boolean | `false` | Make standalone `dry` exit 2 when duplicates exist. |

The `check` command always treats any retained duplicate group as a quality
failure. It uses `max_groups` and `max_occurrences_per_window` from this section,
but not `dry.fail`. Repository configuration can lower or raise the three work
budgets only within their compiled ceilings. Budget exhaustion is an explicit
operational error; it never returns a silently partial DRY result.

### `[mutation]`

| Key | Type | Default | Meaning |
|---|---|---:|---|
| `timeout_seconds` | Positive finite number | `120.0` | Timeout for each validation or test command. |
| `test_command` | String or omitted | omitted | Shell command run for the baseline and each executable mutant. |
| `validation_command` | String or omitted | omitted | Optional build/type-check command run before the test command. |
| `max_mutants` | Positive integer or omitted | omitted | Maximum selected candidates executed; remaining candidates are reported as ignored. |

Commands are interpreted by the platform shell and run with the project root as
their working directory. `mutate --run` and `check --run-mutations` require a
non-empty test command, either on the CLI or in this section. Listing mutations
never runs the configured commands.

Execution mode acquires a global external-state lock before source analysis,
recovers the selected root, writes a bounded recovery journal before an edit,
restores each source after the command, and kills the command process tree on
timeout or cancellation. During mutation validation/tests, SIGINT and SIGTERM
on Unix and Ctrl-C on Windows request cooperative cancellation; reporigor
returns operational exit `1` after source restoration. A repeated signal forces
exit and relies on the persistent journal for recovery on the next execution.
Persistent state is outside the project and owner-only where the platform
supports Unix-style modes. Set `REPORIGOR_MUTATION_STATE_DIR` to an absolute
parent when the platform default is unsuitable; reporigor creates a dedicated
`reporigor/mutation` child. Read-only CLI commands, including list mode, may
create that external coordination directory and shared lock but never recover
a journal or modify project files. Multiple readers coexist; execution waits up
to three seconds for transient readers before reporting a lock conflict. See
[ARCHITECTURE.md](ARCHITECTURE.md#mutation-safety).

## Precedence

Values are resolved as follows:

1. Built-in defaults.
2. The discovered or explicit TOML file.
3. Applicable CLI options.

Scalar command options such as `--fail-over`, `--min-tokens`, `--timeout`,
`--test-command`, and `--max-mutants` replace the configured value. Enabling
flags such as `--include-tests`, `--allow-parse-errors`, `--allow-empty`,
`--allow-missing-coverage`, and `--fail` are additive: they can turn a configured
`false` into `true`, but there is currently no CLI flag that turns a configured
`true` back into `false`.

`--backend native` and `--backend generic` override the configured backend. The
CLI default, `auto`, defers to the configured `backend` value; explicitly writing
`--backend auto` has the same behavior.

Language restrictions, path filters, Cargo feature selection, Cargo executable,
and output format are CLI-only. Configuration does not currently contain keys
for them.

## Global CLI options

Global options may be written before or after the subcommand.

| Option | Meaning |
|---|---|
| `--config FILE` | Use one explicit TOML file. |
| `--language LANGS` | Restrict languages; comma-separated values and repeated options are accepted. |
| `--backend auto\|native\|generic` | Select backend policy. |
| `--allow-project-exec` | Permit `auto`/`native` analysis to execute existing project toolchains; never installs one. |
| `--include-tests` | Include test sources. |
| `--allow-parse-errors` | Continue with recoverable syntax subtrees and emit diagnostics. |
| `--filter TEXT` | Keep root-relative source paths containing `TEXT`; repeated filters are case-sensitive OR conditions. |
| `--features FEATURES` | Enable comma-separated Cargo features for native Rust analysis. |
| `--no-default-features` | Disable Cargo default features. May be combined with `--features`. |
| `--all-features` | Enable all Cargo features; conflicts with the other Cargo feature options. |
| `--cargo PATH` | Use an explicit Cargo executable. |
| `--format text\|json\|sarif\|mutation-json` | Select report output. |

Canonical language names are `bash`, `c`, `cpp`, `objective-c`, `python`,
`rust`, `swift`, and `typescript`. The parser also accepts common aliases such
as `sh`, `c++`, `objc`, `py`, `rs`, and `ts`.

Without `--language`, discovery recognizes:

| Language | Extensions or special detection |
|---|---|
| Bash | `.sh`, `.bash`, `.bats`, and extensionless Bash/`sh` shebang files |
| C | `.c`, `.h` |
| C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`, `.hxx` |
| Objective-C | `.m`, `.mm` |
| Python | `.py` |
| Rust | `.rs` |
| Swift | `.swift` |
| TypeScript | `.ts`, `.tsx`, `.mts`, `.cts` |

An ambiguous `.h` is initially classified as C by generic extension discovery.
A Clang compilation database can provide authoritative C-family ownership for
translation units.

## Commands

### `crap`

```text
reporigor [GLOBAL OPTIONS] crap [PATH]
  [--coverage FILE_OR_DIRECTORY]
  [--fail-over SCORE]
  [--allow-missing-coverage]
  [--allow-empty]
```

`PATH` defaults to `.`. Coverage generation is deliberately outside this
command: generate a report with the project's normal test tooling, then pass it
with `--coverage`.

### `dry`

```text
reporigor [GLOBAL OPTIONS] dry [PATH]
  [--min-tokens N]
  [--max-groups N]
  [--max-occurrences-per-window N]
  [--fail]
```

### `mutate`

```text
reporigor [GLOBAL OPTIONS] mutate [PATH]
  [--list | --run]
  [--test-command COMMAND]
  [--validate-command COMMAND]
  [--timeout SECONDS]
  [--max-mutants N]
  [--skip-baseline]
  [--allow-survivors]
  [--allow-compile-errors]
```

Read-only inventory is the default; `--list` makes that intent explicit. Source
is changed only when `--run` is present. `--list` and `--run` are mutually
exclusive.

### `check`

```text
reporigor [GLOBAL OPTIONS] check [PATH]
  [--coverage FILE_OR_DIRECTORY]
  [--fail-over SCORE]
  [--min-tokens N]
  [--run-mutations]
  [--test-command COMMAND]
```

`check` analyzes the project once and produces CRAP, DRY, and mutation sections.
Mutation is inventory-only unless `--run-mutations` is present.

### `providers`

```text
reporigor [--format text|json] providers [PATH] [--preflight]
```

Without `--preflight`, this performs filesystem and executable-path discovery
without spawning provider commands. `--preflight` runs bounded
version/configuration probes. See [PROVIDERS.md](PROVIDERS.md).

## Report formats

| Format | Intended use | Valid commands |
|---|---|---|
| `text` | Terminal and plain CI logs | All commands |
| `json` | Lossless deterministic native schema v1 | All commands |
| `sarif` | Static-analysis import for CRAP and DRY findings | `crap`, `dry`, `check` |
| `mutation-json` | Mutation Testing Elements v2 | `mutate`, `check` |

`providers --format json` emits provider-resolution JSON rather than the native
analysis report envelope. `providers` rejects SARIF and Mutation Testing
Elements output.

Native analysis JSON has this top-level shape:

```text
schema_version, tool, command, root, summary,
backends[], diagnostics[], results { crap?, dry?, mutate? }
```

The native schema is lossless. SARIF intentionally excludes mutation results;
Mutation Testing Elements intentionally excludes CRAP and DRY results. Invalid
command/format combinations are rejected before analysis. All JSON output is
pretty-printed, newline-terminated, and deterministically ordered for equivalent
input.

## Exit codes

| Exit | Meaning |
|---:|---|
| `0` | Analysis completed and all active gates passed. |
| `1` | Operational/configuration/backend/parse failure, standalone CRAP missing-data guard, or disallowed mutation execution error. |
| `2` | Quality gate failure. |

Quality exit 2 is produced by CRAP scores strictly over the limit, DRY findings
when gating is active, or disallowed surviving mutants. Mutation invalid,
runtime-error, timeout, and disallowed compile-error states take precedence and
produce exit 1.

As with most Clap-based CLIs, invalid command syntax also exits 2. CI that must
distinguish a usage error from a quality failure should validate invocation and
inspect stderr, not the numeric code alone.
