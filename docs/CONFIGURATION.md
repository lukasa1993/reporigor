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
unreported_as_zero = false
allow_empty = false

[dry]
min_tokens = 30
min_statements = 5
similarity_threshold = 0.92
shingle_tokens = 4
max_groups = 50
max_occurrences_per_window = 100
max_total_windows = 1000000
max_fingerprint_buckets = 500000
max_candidate_work = 10000000
fail = false

[mutation]
timeout_seconds = 120.0
minimum_score = 0.80
operators = ["boolean-literal", "comparison", "logical", "arithmetic"]
seed = 76412026
workers = 1
# test_command = "cargo test --workspace"
# validation_command = "cargo check --workspace"
# max_mutants = 100

[kiss]
maximum_cyclomatic_complexity = 12
maximum_nesting_depth = 5
maximum_function_statements = 60
maximum_parameters = 6
maximum_module_dependencies = 16

[yagni]
maximum_unused_private_functions = 0
maximum_unused_modules = 0
maximum_unused_production_dependencies = 0
maximum_unreachable_statements = 0
maximum_unused_feature_flags = 0
maximum_unreferenced_crate_exports = 0
entry_points = ["main", "build.rs"]

[architecture]
maximum_module_fan_out = 12
forbidden_edges = []
domain_modules = []
infrastructure_modules = []
interface_modules = []
implementation_modules = []
contract_traits = []
contract_test_marker = "reporigor_contract"

[architecture.layers]

[cohesion]
minimum = 0.10

[baseline]
enabled = false
path = "reporigor-baseline.json"
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
| `unreported_as_zero` | Boolean | `false` | With region-aware coverage, score selected functions absent from the report as 0% instead of omitting them. |
| `allow_empty` | Boolean | `false` | Permit a standalone `crap` run that discovers no functions. |

CRAP uses `C² × (1 - coverage/100)³ + C`. Coverage is executable-line coverage
within the inclusive function line range after subtracting strict interiors of
adapter-owned nested function/closure ranges. If nested and outer executable
code share a boundary line, line-only ownership is ambiguous and RepoRigor
omits that function's coverage and CRAP score rather than guessing. Sibling
function ranges sharing a reported executable line are omitted for the same
reason.
Supported inputs are LCOV, Cobertura XML,
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

LLVM export JSON retains function regions and columns. RepoRigor assigns a
region only when its complete span belongs to exactly one syntax-owned function
and does not cross a nested executable boundary. With
`unreported_as_zero = true`, a selected function that has no assigned region is
scored conservatively as 0% covered. The backward-compatible default is
`false`, which leaves it explicitly unscored; `allow_missing_coverage` still
controls whether the standalone command permits that missing result.

### `[dry]`

| Key | Type | Default | Meaning |
|---|---|---:|---|
| `min_tokens` | Integer, at least 4 | `30` | Smallest normalized token window considered a duplicate. |
| `min_statements` | Positive integer | `5` | Minimum recursive statement count for function-level near-clone comparison. |
| `similarity_threshold` | Finite number in `(0, 1]` | `0.92` | Minimum multiset Sørensen-Dice similarity that is reported as a clone. Equality is a finding. |
| `shingle_tokens` | Integer in `1..=min_tokens` | `4` | Normalized-token shingle width used by function-level similarity. |
| `max_groups` | Positive integer | `50` | Maximum duplicate groups retained after deterministic sorting. |
| `max_occurrences_per_window` | Integer, at least 2 | `100` | Retain the earliest deterministic occurrences for one fingerprint; later occurrences are intentionally omitted. |
| `max_total_windows` | Positive integer, at most `2000000` | `1000000` | Fail before indexing when selected token streams contain more minimum-size windows. |
| `max_fingerprint_buckets` | Positive integer, at most `1000000` | `500000` | Fail when rolling-window indexing would create more distinct fingerprint buckets. |
| `max_candidate_work` | Positive integer, at most `25000000` | `10000000` | Fail when candidate dispatch, exact token comparison, and maximal extension exceed this many work units. |
| `fail` | Boolean | `false` | Make standalone `dry` exit 2 when duplicates exist. |

Reliable functions with at least `min_tokens` normalized tokens and
`min_statements` recursive statements are compared as multisets of
`shingle_tokens`-wide shingles. For shingle multisets `A` and `B`, similarity is
`2 * shared_occurrences / (|A| + |B|)`. Shared occurrences use multiset
intersection, so repeats count only up to the smaller multiplicity. A pair is
accepted when similarity is greater than or equal to `similarity_threshold`;
connected accepted pairs form one canonical clone group whose reported
similarity is the minimum accepted edge in that group. Exact normalized-token
analysis remains the compatibility path for exact regions, including repeated
blocks inside a function. `min_statements` applies only where an adapter owns a
reliable recursive AST statement count; exact compatibility groups still use
`min_tokens` and are gated rather than silently discarded.

With baseline mode disabled, `check` treats every retained duplicate group as
a quality failure. It uses `max_groups` and `max_occurrences_per_window` from
this section, but not `dry.fail`. Repository configuration can lower or raise
the three work budgets only within their compiled ceilings. Budget exhaustion
is an explicit operational error; it never returns a silently partial DRY
result.

### `[mutation]`

| Key | Type | Default | Meaning |
|---|---|---:|---|
| `timeout_seconds` | Positive finite number | `120.0` | Timeout for each validation or test command. |
| `minimum_score` | Finite number in `[0, 1]` | `0.80` | Minimum integrated mutation score. Equality passes. |
| `operators` | Non-empty, duplicate-free array | all four fixed operators | Select from `boolean-literal`, `comparison`, `logical`, and `arithmetic`. |
| `seed` | Unsigned 64-bit integer | `76412026` | Deterministic ordering seed applied to stable mutant fingerprints. |
| `workers` | Integer exactly equal to `1` | `1` | The crash-safe executor is currently serial; any other value is rejected. |
| `test_command` | String or omitted | omitted | Shell command run for the baseline and each executable mutant. |
| `validation_command` | String or omitted | omitted | Optional build/type-check command run before the test command. |
| `max_mutants` | Positive integer or omitted | omitted | Maximum selected candidates executed; remaining candidates are reported as ignored. |

The integrated mutation score is `killed / (killed + survived)`. Exactly
`killed` and `survived` are scoreable. `no-coverage`, `compile-error`,
`runtime-error`, `timeout`, `invalid`, `ignored`, and `pending` are excluded;
RepoRigor never guesses that a mutant is equivalent. When no scoreable status
exists, the score check is explicitly omitted rather than reported as zero or
as a pass. Each surviving mutant is also a separate structural failure with a
stable fingerprint.

The fixed operator set filters candidates before execution. The SHA-256 order
key combines `seed` with each stable structural fingerprint, so input
enumeration order does not affect which candidates reach `max_mutants`.
Candidates beyond that executor-owned cap are `ignored` and therefore
non-scoreable. A deterministic ignored tail does not make an otherwise
scoreable capped run incomplete: the cap, seed, operators, and executed
fingerprints are part of the rule evidence and analysis scope. Commands are
interpreted by the platform shell and run with the project root as their
working directory. `mutate --run` and
`check --run-mutations` require a non-empty test command, either on the CLI or
in this section. Listing mutations never runs the configured commands and
produces only non-scoreable `pending` statuses.

For each active mutant, RepoRigor sets `REPORIGOR_MUTANT_ID` and
`REPORIGOR_MUTANT_FINGERPRINT` in both validation and test commands. Both are
absent from baseline commands. Test tooling may use these values to select a
fresh per-mutant build-cache directory; this prevents one compiled mutant from
being reused while testing the next candidate. Dogfood clears its isolated
cache before and after the run and assigns one cache directory per mutant.

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

### `[kiss]`

| Key | Type | Default | Integrated measurement |
|---|---|---:|---|
| `maximum_cyclomatic_complexity` | Positive integer | `12` | One plus adapter-defined language decision points. |
| `maximum_nesting_depth` | Non-negative integer | `5` | Maximum recursive AST control-flow nesting, excluding nested function boundaries. |
| `maximum_function_statements` | Non-negative integer | `60` | Recursive AST statement-node count, excluding nested function boundaries. |
| `maximum_parameters` | Non-negative integer | `6` | Declared function or method parameters, including an explicit receiver. |
| `maximum_module_dependencies` | Non-negative integer | `16` | Distinct direct, non-target-gated production dependencies for a package. |

These are inclusive maximums: `measured <= configured` passes. Function rules
are emitted only for records whose adapter marked the recursive structural
metrics reliable. The dependency-count rule requires a reliable project
dependency graph.

### `[yagni]`

| Key | Type | Default | Integrated measurement |
|---|---|---:|---|
| `maximum_unused_private_functions` | Non-negative integer | `0` | Unambiguous private functions with no resolved same-package production reference and no adapter entry-point mark. |
| `maximum_unused_modules` | Non-negative integer | `0` | Production modules with zero resolved references after conservative exclusions. |
| `maximum_unused_production_dependencies` | Non-negative integer | `0` | Direct, non-target-gated production dependencies with neither a resolved production identifier reference nor feature activation. |
| `maximum_unreachable_statements` | Non-negative integer | `0` | Statements after an unconditional `return`, `break`, or `continue` in the same recursive AST block. |
| `maximum_unused_feature_flags` | Non-negative integer | `0` | Declared non-default features with no cfg, composition, or dependency-activation reference. |
| `maximum_unreferenced_crate_exports` | Non-negative integer | `0` | Unambiguous repository-restricted exports with no resolved repository reference; unrestricted public API is excluded. |
| `entry_points` | Duplicate-free array of non-empty strings | `["main", "build.rs"]` | Extra stable function/module symbols or path suffixes excluded from unused findings. |

Each count is an inclusive maximum. Generated, target-gated,
framework-managed, reflection-reachable, and externally invoked modules are
excluded. The analysis also relies on adapter entry-point marks for functions;
it does not treat unrestricted public symbols as dead merely because the
selected repository contains no caller.

### `[architecture]` and `[architecture.layers]`

| Key | Type | Default | Meaning |
|---|---|---:|---|
| `maximum_module_fan_out` | Non-negative integer | `12` | Inclusive maximum for distinct direct, non-target-gated production dependencies. |
| `forbidden_edges` | Duplicate-free array of `source->destination` patterns | `[]` | Reject matching internal production edges. Each side is exact or contains one `*` wildcard. |
| `domain_modules` | Duplicate-free pattern array | `[]` | Sources in this set may not directly depend on `infrastructure_modules`. |
| `infrastructure_modules` | Duplicate-free pattern array | `[]` | Destinations used by the domain rule. |
| `interface_modules` | Duplicate-free pattern array | `[]` | Sources in this set may not directly depend on `implementation_modules`. |
| `implementation_modules` | Duplicate-free pattern array | `[]` | Destinations used by the interface rule. |
| `contract_traits` | Duplicate-free exact stable-symbol array | `[]` | Trait implementations that require a contract test. |
| `contract_test_marker` | Non-empty canonical string | `"reporigor_contract"` | Required marker on a qualifying contract test. |
| `architecture.layers` | Package-pattern-to-unsigned-integer map | `{}` | An internal edge passes when `source_layer >= destination_layer`; lower-numbered layers cannot depend on higher-numbered layers. A package matching multiple patterns is rejected as ambiguous. |

Configured layer, edge, domain, and interface predicates use direct internal
production edges and ignore target-gated edges. In addition to those predicates,
the integrated check always detects cycles with Tarjan strongly connected
components (including a self-edge), applies the fan-out maximum, and emits
coupling metrics. A configured trait implementation passes its subtype-contract
rule only when a non-target-gated test has `contract_test_marker` and references
both the exact trait and implementation symbols.

Coupling rows are informational and never fail by themselves. Afferent
coupling `Ca` is the number of repository packages with a direct internal
production dependency on a package. Efferent coupling `Ce` is the number of
distinct direct production dependencies, internal or external. Instability is
`Ce / (Ca + Ce)`, or `0` when both counts are zero.

### `[cohesion]`

| Key | Type | Default | Meaning |
|---|---|---:|---|
| `minimum` | Finite number in `[0, 1]` | `0.10` | Minimum module cohesion; equality passes. |

Functions are grouped by repository-relative file plus the adapter-qualified
module/type owner. A pair is related by a uniquely resolved direct function
reference, a shared uniquely resolved local callee, a shared non-ubiquitous
non-local reference, or membership in the same exact `(implementation type,
trait)` contract. The trait relation applies only within that exact qualified
implementation/trait owner; inherent methods are not related merely because
they share a type. Ambiguous leaf names are not guessed. Cohesion is
`related_pairs / all_function_pairs`; an owner with one function has cohesion
`1.0`. The shared-reference test ignores `self`, `Self`, `Result`, `Option`,
`Some`, `None`, `Ok`, `Err`, `new`, and `default`.

### `[baseline]`

| Key | Type | Default | Meaning |
|---|---|---:|---|
| `enabled` | Boolean | `false` | Compare the integrated rule stream with a prior native report. |
| `path` | Safe repository-relative UTF-8 path | `"reporigor-baseline.json"` | Prior `ReportEnvelope` JSON to read from the analyzed root. Absolute paths and parent traversal are rejected. |

There is no separate baseline schema. When enabled, `check` reads
`results.rules` from the ordinary prior native report and matches rows by
stable `violation_id`. The prior report's `results.rules.analysis_scope`
fingerprint must exactly match the current backend/language/filter/test,
coverage, Cargo-feature, mutation-execution, and normalized configuration
selection; a mismatch is an operational error rather than an unsafe debt
comparison. A current failure is `new` when its ID was absent,
`worsened` when its finite `excess` is greater than the prior excess,
`improved` when its excess is smaller, and otherwise `existing`. A previous
failure that now passes is also `improved`; a previous failed ID that
disappears is counted as `resolved` only when that rule was completely
evaluated in the current run. Capability or partial-inventory omissions block
resolution for their rule. With complete evidence, the baseline classification
fails for `new` or `worsened` violations; existing debt remains visible without
failing it. Independently, any nonempty `results.rules.omitted` list makes
`results.rules.baseline.gate_passed` false and makes `check` exit 2.

`check` is read-only with respect to the configured baseline: it never creates,
updates, or rewrites the prior report. The prior report is bounded by
`max_source_bytes`. Missing, unreadable, malformed, oversized, or non-native
reports, and native reports without integrated rule rows, are operational
errors. Creating or replacing the checked-in baseline is therefore an explicit
user action outside the check.

### Capability-gated omissions

Structural absence is never converted into a zero measurement or a successful
rule. The native report records the rule ID and reason in `results.rules.omitted`:

- CRAP is omitted when no function-level coverage score exists and the explicit
  `unreported_as_zero` policy is disabled.
- Function KISS metrics and cohesion are emitted only for reliable adapter
  structural records. Enhanced function DRY likewise considers only reliable
  records, while exact token DRY remains available as a fallback.
- Package dependency counts, direction/edge/cycle rules, fan-out, and coupling
  require a reliable production dependency graph.
- Unused private functions, repository-restricted exports, and production
  dependencies require reliable whole-project identifier counts. Unused
  modules, unreachable statements, and feature flags each require their own
  reliable inventory.
- Configured subtype-contract checks require both reliable trait-implementation
  and test-reference inventories.
- Mutation score is omitted unless at least one result is `killed` or
  `survived`.

Omitted rules are neither pass nor failure rows. They still make the integrated
result incomplete: one or more entries in `results.rules.omitted` force
`results.rules.baseline.gate_passed` to `false` and make `check` exit 2, whether
baseline mode is enabled or disabled. Baseline resolution also ignores a
previously failed rule when that rule was not evaluated, preventing a weaker
adapter from manufacturing an apparent improvement.

### Integrated rule catalog

| Rule ID | Comparison and evidence |
|---|---|
| `crap.maximum` | Inclusive maximum `crap.fail_over`; measured CRAP is `C² × (1 - coverage_fraction)³ + C`. |
| `dry.clone` | Exclusive maximum against `dry.similarity_threshold`, so similarity equal to the threshold fails. |
| `mutation.score` | Inclusive minimum `mutation.minimum_score`; measured fraction uses only killed and survived results. |
| `mutation.surviving-mutant` | Failed boolean row for each survivor; structural evidence is its stable fingerprint. |
| `kiss.cyclomatic-complexity` | Inclusive maximum per reliable function. |
| `kiss.nesting-depth` | Inclusive maximum per reliable function. |
| `kiss.function-statements` | Inclusive maximum per reliable function. |
| `kiss.parameter-count` | Inclusive maximum per reliable function. |
| `kiss.module-dependency-count` | Inclusive maximum per package. |
| `yagni.unused-private-function` | Inclusive maximum total unused count, emitted at each unambiguous finding. |
| `yagni.unused-module` | Inclusive maximum total unused count. |
| `yagni.unused-production-dependency` | Inclusive maximum total unused count. |
| `yagni.unreachable-code` | Inclusive maximum total unreachable count. |
| `yagni.unused-feature-flag` | Inclusive maximum total unused count. |
| `yagni.unreferenced-crate-export` | Inclusive maximum total unreferenced count. |
| `solid.dependency-direction` | Boolean per configured internal layer edge. |
| `solid.forbidden-module-edge` | Boolean per internal production edge. |
| `solid.domain-to-infrastructure` | Boolean per internal production edge. |
| `solid.interface-to-implementation` | Boolean per internal production edge. |
| `solid.package-cycle` | Failed boolean row per internal production strongly connected component. |
| `solid.maximum-module-fan-out` | Inclusive maximum per package. |
| `solid.subtype-contract-test` | Boolean per configured non-target-gated trait implementation. |
| `coupling.afferent` | Informational `Ca` per package. |
| `coupling.efferent` | Informational `Ce` per package. |
| `coupling.instability` | Informational `Ce / (Ca + Ce)` per package. |
| `cohesion.module` | Inclusive minimum related-pair fraction per module; methods in the same exact implementation/trait contract are related. |

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

`check` analyzes the project once and produces the existing CRAP, DRY, and
mutation sections plus the canonical `results.rules` stream for CRAP, DRY,
mutation quality, KISS, YAGNI, dependency/SOLID predicates, coupling, and
cohesion. Mutation is inventory-only unless `--run-mutations` is present; its
`pending` inventory is non-scoreable, so mutation score is then listed as an
omitted check.

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
| `sarif` | Static-analysis import for CRAP, DRY, and failed integrated rule rows | `crap`, `dry`, `check` |
| `mutation-json` | Mutation Testing Elements v2 | `mutate`, `check` |

`providers --format json` emits provider-resolution JSON rather than the native
analysis report envelope. `providers` rejects SARIF and Mutation Testing
Elements output.

Native analysis JSON has this top-level shape:

```text
schema_version, tool, command, root, summary,
backends[], diagnostics[], results { crap?, dry?, mutate?, rules? }
```

The integrated `rules` section contains the formula catalog, summary counts,
canonical rule rows, surviving-mutant fingerprints, explicit omissions, and
baseline comparison metadata. The native schema is lossless. SARIF projects
failed rule rows but intentionally excludes mutation execution records;
Mutation Testing Elements intentionally excludes CRAP, DRY, and structural rule
details. Invalid command/format combinations are rejected before analysis. All
JSON output is pretty-printed, newline-terminated, and deterministically ordered
for equivalent input. See [the schema guide](../schemas/README.md).

## Exit codes

| Exit | Meaning |
|---:|---|
| `0` | Analysis completed and all active gates passed. |
| `1` | Operational/configuration/backend/parse failure, standalone CRAP missing-data guard, or disallowed mutation execution error. |
| `2` | Quality gate failure. |

For standalone commands, quality exit 2 retains the documented CRAP, DRY, and
survivor policies. For `check`, baseline-disabled mode exits 2 for any failed
integrated rule. With complete evidence, baseline-enabled mode exits 2 for
`new` or `worsened` failures; existing debt, improvements, resolutions, and
informational metrics remain visible without failing that gate. In either
baseline mode, any nonempty `results.rules.omitted` list makes
`results.rules.baseline.gate_passed` false and exits 2. Mutation invalid,
runtime-error, timeout, and disallowed compile-error states take precedence and
produce exit 1.

As with most Clap-based CLIs, invalid command syntax also exits 2. CI that must
distinguish a usage error from a quality failure should validate invocation and
inspect stderr, not the numeric code alone.
