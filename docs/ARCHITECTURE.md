# Architecture

`reporigor` unifies project discovery, analysis scheduling, normalized records,
quality gates, mutation safety, and reporting. It does not force eight
languages into one semantic AST. Each adapter reports only the capabilities and
records it can support.

## End-to-end flow

```text
command line + reporigor.toml
              |
              v
      AnalysisRequest
  root, languages, filters,
  tests, size, backend policy
              |
              v
  project/source discovery -------- reporigor providers
              |                      inventory + preflight
              v
       backend routing
     /        |         \
 Cargo       Clang    Tree-sitter
  Rust      C family    all eight
     \        |         /
              v
       AnalysisSnapshot
 files, functions, token sets,
 mutations, repository semantics,
 backends, diagnostics, reliability
              |
      +-------+---------+
      |       |         |
     CRAP     DRY    mutation
      |       |         |
      +-------+---------+
              |
              v
 integrated deterministic rules
 CRAP, DRY, mutation, KISS, YAGNI,
 dependencies/SOLID, coupling, cohesion
              |
              v
       ReportEnvelope v1
      /       |         \
    text   native JSON   projections
                       SARIF / MTE v2
```

The CLI constructs one `AnalysisRequest`, analyzes the project once per
command, then passes the normalized snapshot to language-neutral analyzers.
`check` reuses that snapshot for the legacy CRAP/DRY/mutation sections and one
canonical `results.rules` stream. There is no second executable, configuration
loader, report envelope, or mutation executor for the structural rules.

## Workspace boundaries

| Crate | Responsibility |
|---|---|
| `reporigor-core` | Shared language/project enums, requests, discovery, backend traits, capabilities, diagnostics, and normalized records. |
| `adapter-tree-sitter` | Deterministic syntax fallback with eight pinned, compiled-in grammars; functions, complexity, tokens, mutations, and parse diagnostics. |
| `adapter-rust` | Cargo-aware active source/module scope, feature/cfg handling, `syn` function/complexity analysis, `rustc_lexer` token normalization, and Rust mutation candidates. |
| `adapter-clang` | Existing compilation-database discovery, safe argv normalization, bounded Clang validation/JSON AST analysis, and native C-family functions/complexity. |
| `adapter-project` | Filesystem-only provider inventory plus explicit bounded preflight for TypeScript, SwiftPM, Python, Bash, and optional ShellCheck. |
| `analysis-crap` | Coverage loading/path normalization, executable-line matching, and the CRAP formula. |
| `analysis-dry` | Language-neutral exact token-region and function-level shingle/Dice clone detection. |
| `analysis-mutate` | Typed operator selection, deterministic seeded ordering, locking, recovery journal, source replacement/restoration, subprocess supervision, and mutation status classification. |
| `analysis-quality` | Deterministic integrated rule evaluation, capability-gated omissions, dependency/coupling algorithms, and native-report baseline comparison. |
| `provider-mutation` | Static optional-engine inventory, bounded version preflight, and normalization of existing ecosystem mutation reports; no external-engine execution. |
| `reporigor-reporting` | Stable report envelope, deterministic human/native JSON rendering, SARIF 2.1.0, and Mutation Testing Elements v2. |
| `reporigor` | Argument/config precedence, routing, analyzer orchestration, quality policy, report selection, and exit codes. |

These boundaries keep risky operations small. Parsers discover candidates;
`analysis-mutate` alone owns source modification. Analyzers do not know how a
language was parsed; reporters do not recalculate results.

## Shared model

The central types in `reporigor-core` are:

- `AnalysisRequest`: canonical root, language and path selection, test policy,
  parse-error policy, size limit, and backend preference.
- `SourceFile`: absolute path, root-relative report path, language, and
  generated/test classification.
- `BackendInfo`: stable ID, version, native/generic marker, and declared
  capabilities.
- `FunctionRecord`: language, repository-relative file/range, stable structural
  symbol, complexity/nesting/statement/parameter counts, normalized tokens,
  resolved references, visibility, package/entry-point data, a structural
  reliability mark, and optional coverage/CRAP values.
- `TokenRecord`: normalized token value, line, and per-file token index.
- `MutationCandidate`: stable run-local ID, stable symbol/operator/fingerprint,
  source location, original text, replacement, and private byte edit span.
- `Diagnostic`: severity, backend, optional location, message, and explicit
  fallback marker.
- `RepositorySemantics`: canonical package/dependency/module/reachability rows,
  identifier/feature/trait/test inventories, and an independent reliability
  flag for every inventory family.
- `AnalysisSnapshot`: merged files, backends, functions, per-file token sets,
  candidates, repository semantics, diagnostics, and parse-error count.
- `RuleResult`: rule and stable violation IDs, repository-relative file,
  stable symbol, measured/allowed JSON values, algorithm, typed comparison,
  pass/fail result, finite excess, and baseline disposition.

Mutation identities are assigned only after all adapters finish. A two-pass
merge fills stable symbols/operators, sorts by structural evidence, preserves
unique adapter fingerprints, deterministically disambiguates collisions, and
then assigns run-local IDs in canonical order. Core-generated fingerprint hashes
exclude line, column, and byte offsets; adapters must provide the same structural
stability when they supply a fingerprint. Fixed operators plus the configured
seed determine execution order. Location fields remain available for applying
an edit but do not define its durable identity.

Capabilities are explicit rather than inferred from a backend name:

```text
syntax, functions, complexity, tokens, mutations,
project-semantics, parse-validation
```

This allows a hybrid result. For example, Clang can own C++ functions and
complexity while Tree-sitter supplies normalized tokens and built-in mutation
sites for the same translation unit.

### Canonical function metric domain

Backend selection must not change which executable bodies count as functions
or which body owns a cyclomatic decision. For Rust and the Clang C family, the
shared metric domain therefore contains only named function and method
declarations owned by a file, module, namespace, or type.

- A named declaration nested inside another function is not a separate
  `FunctionRecord`.
- Rust closures, C++ lambdas, and Objective-C/C block expressions are not
  `FunctionRecord`s. Their generated or source-derived names are not stable
  across Tree-sitter, `syn`, and Clang JSON ASTs.
- Every excluded local function, closure, lambda, or block is still a hard
  complexity boundary. Decisions in its body are not charged to the enclosing
  function.

Generic-only language profiles may report a top-level function expression when
the language gives it a stable source-level name, such as a TypeScript arrow
assigned to a variable. Nested executable bodies remain independent complexity
boundaries. Cross-backend Rust-closure and C++-lambda fixtures enforce identical
function names, counts, and complexity under `generic` and `native` routing.

## Discovery

Source discovery:

- honors Git ignore/exclude files through the `ignore` walker;
- prunes common dependency, build, coverage, virtual-environment, and vendor
  directories;
- sorts paths for deterministic processing;
- recognizes extensionless Bash files by shebang;
- excludes language-recognized tests unless requested;
- applies repeated filters as case-sensitive OR-substring matches to
  root-relative paths;
- returns an empty-source diagnostic rather than panicking. Quality commands
  fail operationally when selection is empty, except for a standalone `crap`
  run with the explicit `--allow-empty` policy.

Project markers include `Cargo.toml`, an existing `compile_commands.json`,
`tsconfig.json`/`package.json`, `Package.swift`, and common Python metadata.
Project providers can refine the source set. A TypeScript preflight, for
example, asks the project-local `tsc --listFilesOnly` for the configured files.

Generic extension discovery classifies `.h` as C. The Clang adapter instead
uses each compilation-database command's `-x` mode, source extension, and driver
flavor to classify actual translation units.

## Backend routing

### `generic`

`generic` performs filesystem/provider discovery without provider subprocesses,
then analyzes every selected source with the pinned Tree-sitter adapter. It is
the reproducible syntax-only baseline and requires no runtime grammar download
or Python runtime.

### `auto`

Without `--allow-project-exec`, `auto` performs filesystem-only provider
discovery, analyzes every selected source with Tree-sitter, and emits a
fallback diagnostic explaining the project-execution trust boundary. With the
trust flag, `auto` explicitly preflights applicable project providers, then
prefers:

1. `adapter-rust` for Rust sources when the root is a Cargo project.
2. `adapter-clang` for C/C++/Objective-C translation units when an existing
   compilation database can be loaded and validated.
3. `adapter-tree-sitter` for remaining files and safe fallbacks.

Native failures that can safely fall back become diagnostics with
`fallback_used: true`. The report therefore shows both the selected backend and
why a less precise path was used.

Oversized input is not a safe fallback condition. Every syntax backend returns
the shared `SourceTooLarge` error when a selected regular file exceeds
`AnalysisRequest.max_source_bytes`. The router propagates that error in
`generic`, `auto`, and `native`, so native Rust or Clang routing cannot turn an
oversized selected source into an empty file result or lose it while merging a
fallback snapshot. The limit is inclusive: exactly `max_source_bytes` bytes is
accepted. The request value itself cannot exceed the immutable 64 MiB per-file
ceiling. Source discovery also enforces immutable limits of 100,000 selected
files and 1 GiB of aggregate selected metadata before parsing. Native Rust
revalidates the filesystem selection before module traversal; Clang accounts
for its deduplicated selected translation units before validating any of them.
Generic syntax input must also be valid UTF-8 and is checked before parsing.
The adapter never normalizes invalid bytes through lossy replacement, so token
values, display columns, and executable mutation spans always describe the same
source byte sequence.

Function, token, and built-in mutation records are derived only from bounded
syntax/source spans: they do not introduce an input-independent record stream.
Tree-sitter and Rust record cardinality is therefore bounded by the selected
source bytes, while Clang additionally caps retained JSON AST output per
translation unit. DRY retains bounded groups/occurrences, and mutation records
are limited to syntax candidate sites. Mutation Testing Elements rendering
rereads each candidate source through the same contained regular-file and
per-file byte bound, then verifies every candidate byte span and original text
before embedding source in the projection. After validation, it collects the
requested byte offsets for each file and maps them to Unicode-scalar
line/column coordinates in one source sweep, rather than rescanning a source
prefix for every mutant.

### `native`

`native` turns unavailable applicable project providers and native Rust/Clang
failures into operational errors. It never silently drops to generic analysis.
Analysis commands also require `--allow-project-exec` before entering this
mode; `providers --preflight` remains the explicit read-only inventory probe.

For TypeScript, Swift, Python, and Bash, the current project providers establish
toolchain availability, source-set/project metadata, versions, and diagnostics;
their syntax-level functions, complexity, tokens, and mutations are still
produced by Tree-sitter. `native` therefore means required project context plus
the best implemented syntax adapter, not a claim that every language already
has a compiler AST integration.

## Native Rust path

The Rust adapter is intentionally exceptional because Rust source ownership is
not reliably described by walking `.rs` files. It uses Cargo metadata and the
selected feature flags to identify active packages/targets, asks Cargo/rustc for
active `cfg` values, and follows modules, `#[path]`, and supported literal
`include!` references.

It then uses:

- `syn` for syntax, declarations, functions, and cyclomatic decisions;
- `rustc_lexer` plus cfg-aware traversal for normalized DRY tokens;
- syntax-aware source spans for built-in mutation candidates.

Cargo commands can evaluate build configuration and write normal Cargo target
artifacts. They are not part of the subprocess-free `generic` path.

## Native Clang path

The Clang adapter consumes, but never generates, `compile_commands.json`.
Discovery checks the root, conventional `build`, `.build`, and `out`
directories, then immediate child directories in deterministic order.

For each selected translation unit it:

1. Parses the database `arguments` array or tokenizes its `command` string
   without invoking a shell.
2. Resolves paths and rejects source entries outside the project root.
3. Removes output/dependency-only flags while preserving compiler semantics.
4. Runs bounded validation and Clang JSON-AST commands.
5. Extracts native C functions, C++ methods, and Objective-C methods with
   cyclomatic decisions.

Tree-sitter still processes native Clang files for tokens and mutation sites.
Its function results are removed before merging, so Clang remains authoritative
for C-family function metrics.

In `auto`, absence or failure of the compilation database is visible and falls
back to generic parsing. In `native`, absence or a selected translation unit
that cannot complete native AST analysis is an operational error.

## Project-provider boundary

`adapter-project` has two deliberately separate APIs:

- `discover`: reads project files and resolves existing executables; it never
  spawns a command.
- `preflight`: runs bounded version/configuration probes and records the exact
  argv in provenance.

TypeScript resolution accepts only a project-local `node_modules/.bin/tsc`; it
does not call `npx` or download a compiler. SwiftPM uses `swift package describe
--type json`. Python prefers a project virtual environment and then PATH. Bash
dialect classification comes from shebangs and `.bats`; ShellCheck is optional.

Expected missing tools, nonzero exits, malformed probe output, and timeouts are
inventory/diagnostic states rather than panics. See [PROVIDERS.md](PROVIDERS.md)
for the exact inventory.

## Optional mutation-provider boundary

`provider-mutation` is a library boundary for reusing established mutation
engines without granting them implicit write access. Its deterministic
inventory contains these stable IDs:

```text
built-in, cargo-mutants, mutmut, stryker, mull, muter
```

Static discovery checks only matching project manifests and existing executable
paths. Explicit preflight runs bounded version probes for available external
providers; the built-in provider is always available and reports the current
`reporigor` version without spawning a command. Discovery and preflight never
install a provider or execute mutations.

Existing results can be normalized into `MutationResult` records from:

- Mutation Testing Elements v2 for every provider, including Stryker;
- Mutation Testing Elements v1 for Mull;
- schema-checked cargo-mutants `outcomes.json`;
- Muter's custom JSON when each basename resolves to exactly one project file.

`mutmut` has no stable detailed JSON contract, so its results require conversion
to Mutation Testing Elements before import. Imported paths, coordinates,
statuses, source spans, and report shapes are validated before normalization.

Effectful external-provider execution is deliberately not exposed: users run an
already-installed engine themselves and import its report, while built-in
mutation remains the CLI execution path. The inventory is exposed by
`reporigor providers`; normalized report import is currently a library API and
does not yet have a report-import CLI option.

## Analysis layers

### CRAP

Adapters provide function ranges and complexity. `analysis-crap` maps a
normalized executable-line coverage report to each inclusive range, excluding
the strict interiors of adapter-recorded nested function/closure ranges from
the outer denominator, and applies:

```text
CRAP = complexity² × (1 - coverage/100)³ + complexity
```

Coverage paths are normalized across absolute/relative and slash conventions.
If nested and outer executable code share a boundary line, line-only coverage
cannot assign that line reliably; the outer function is counted as
coverage-ambiguous and receives no CRAP score. Ambiguous or empty matches remain
explicitly missing rather than being treated as zero coverage. The same rule
applies to sibling function ranges that own one reported executable line, such
as same-line overload definitions.

Coverage files are bounded and read only after non-symlink regular-file checks.
Directory discovery is canonical-contained and bounded by entry, directory,
candidate-file, and aggregate-byte budgets. Each parser also bounds source
paths, records, output files, and executable-line maps. In particular, the LLVM
loader preflights every code-region span plus the whole-report expansion before
iterating any line range, while the Cobertura loader caps sources, classes,
class lines, and the complete source-by-class-line resolution cross-product.
Before its streaming XML parser runs, Cobertura performs a linear markup pass
that bounds element attributes, namespace declarations, nesting, names, values,
and markup size; duplicate attributes and DTD/entity declarations are rejected.
The exact ceilings are listed in [CONFIGURATION.md](CONFIGURATION.md#crap).

### DRY

Adapters normalize comments and literal/identifier classes into token streams.
`analysis-dry` hashes each token once and derives minimum-size windows in O(1)
with a deterministic two-lane rolling fingerprint. It verifies candidates by
exact token comparison, extends matches to maximal sequences, removes overlaps
and contained pairs, applies deterministic occurrence/group limits, and returns
root-relative locations. Immutable total-window, fingerprint-bucket, and exact
candidate-work ceilings fail closed with typed errors instead of returning a
partial result.

For adapter-marked reliable functions, the same analyzer also compares
normalized-token shingle multisets. With `shared` equal to the multiset
intersection size, the similarity is:

```text
Dice(A, B) = 2 × shared / (|A| + |B|)
```

The default eligible function has at least 30 normalized tokens and 5 recursive
statements, uses 4-token shingles, and is a clone at similarity `>= 0.92`.
Accepted pairs are joined into canonical groups; the group reports its minimum
accepted pair similarity. Exact-region groups remain for unmapped or unreliable
regions, so gaining function structure enriches DRY without erasing the safe
fallback.

### Mutation

Adapters enumerate syntax-aware replacements, but do not execute them. The
shared executor either converts candidates to `pending` results (list mode) or
runs optional validation and required tests for one candidate at a time.

Statuses use one vocabulary:

```text
killed, survived, no-coverage, compile-error, runtime-error,
timeout, invalid, ignored, pending
```

The fixed configurable operator vocabulary is `boolean-literal`, `comparison`,
`logical`, and `arithmetic`. A unique structural fingerprint is required before
selection. The configured seed orders `SHA-256(seed, fingerprint)` keys, and
the existing serial executor alone applies `max_mutants`; later candidates are
`ignored`.

Validation and test children receive the active candidate through the
`REPORIGOR_MUTANT_ID` and `REPORIGOR_MUTANT_FINGERPRINT` environment variables;
baseline children receive neither. This lets incremental build systems use a
separate artifact cache for every candidate, avoiding both coarse timestamp
ambiguity and reuse of an earlier mutant's compiled artifact. The active source
replacement also receives a fresh timestamp so an incremental tool cannot
silently reuse the baseline binary; restoration reinstates the exact original
timestamp and other supported metadata.

Mutation quality uses:

```text
score = killed / (killed + survived)
```

Only `killed` and `survived` are scoreable. Every other status in the shared
vocabulary is excluded from the denominator, and no equivalent-mutant status
is inferred. The default minimum is `0.80`; equality passes. No scoreable
results produces an explicit omission. A survivor also produces its own failed
rule row, keyed by the candidate fingerprint.

### Integrated structural rules

`analysis-quality` consumes the adapter snapshot after CRAP, DRY, and optional
mutation execution. It emits one sorted rule stream in this order of concern:

- KISS applies inclusive maxima to cyclomatic complexity (`12`), recursive
  control-flow nesting (`5`), recursive statements (`60`), parameters (`6`),
  and distinct direct production dependencies (`16`). Nested function bodies
  are excluded from the recursive structure of their enclosing function.
- YAGNI applies zero-count defaults to unused unambiguous private functions,
  unused modules, unused production dependencies, unreachable statements,
  unused non-default features, and unreferenced repository-restricted exports.
  Generated, target-gated, framework/reflection managed, externally invoked,
  explicit entry-point, ambiguous, and unrestricted public cases are excluded
  rather than guessed dead.
- Dependency/SOLID rules enforce configured layer direction, forbidden-edge,
  domain-to-infrastructure, and interface-to-implementation predicates; detect
  internal production cycles with Tarjan strongly connected components;
  cap direct production fan-out at `12`; and optionally require an exact
  trait/implementation contract test bearing the configured marker.
- Coupling is informational. `Ca` counts repository packages with a direct
  internal production edge to the package; `Ce` counts distinct direct
  production dependencies, internal and external. Instability is
  `Ce / (Ca + Ce)`, or `0` for an isolated package.
- Module cohesion groups functions by repository-relative file and
  adapter-qualified module/type owner. It is `related function pairs / all
  function pairs`, with a singleton equal to `1`. A pair is related when both
  functions belong to the same exact `(implementation type, trait)` contract,
  or by a uniquely resolved direct call, a shared uniquely resolved local
  callee, or a shared non-ubiquitous non-local reference. The default minimum
  is `0.10`.

Numeric maximums pass at equality; minimums also pass at equality. Boolean
rules pass only when measured and allowed values match. Informational rows
always pass and carry zero excess. Deterministic floating measurements retain
their full finite value for both comparison and serialization, so a real
threshold crossing is never rounded away. The complete field names, defaults, and validation boundaries are in
[CONFIGURATION.md](CONFIGURATION.md#kiss).

Reliability is conservative and granular. An unavailable dependency graph
omits dependency-count, direction, edge, cycle, fan-out, and coupling rules.
Unreliable identifier, module, reachability, feature, trait, or test inventories
omit only the rules that require them. Function structure rules are emitted
only for reliable records; function-level near-clone DRY does the same while
retaining exact-token fallback. Missing coverage omits CRAP, and no scoreable
mutation status omits mutation score. Each explicit omission is serialized
with its rule ID and reason; missing evidence is never a synthetic zero, pass,
failure, or resolved baseline item. Because an integrated result with missing
evidence is incomplete, any nonempty omission list independently makes the
baseline gate false and makes `check` exit 2.

### Mutation safety

The CLI's read-only `crap`, `dry`, `providers`, mutation-list, and non-executing
`check` flows acquire a shared external coordination lock before reading
configuration or source. They reject a pending recovery journal and hold the
lock through analysis, coverage/report source reads, and output generation.
Multiple read-only commands can coexist. They never modify the project, though
acquiring the lock may create owner-only state outside it.

Execute modes acquire the exclusive lock and recover the selected root before
source analysis, then keep that lock through mutation execution. An exclusive
writer waits for transient shared readers for up to three seconds before
failing clearly. One lock covers the entire configured state base, so it also
serializes overlapping roots such as a workspace and one nested package.

Locks and recovery journals live outside the analyzed project in persistent,
owner-only user state. The base is `$XDG_STATE_HOME/reporigor/mutation` (or
`~/.local/state/reporigor/mutation`) on Unix,
`~/Library/Application Support/reporigor/mutation` on macOS, and
`%LOCALAPPDATA%\reporigor\mutation` on Windows. Each canonical project root
uses a SHA-256-keyed child directory. `REPORIGOR_MUTATION_STATE_DIR` can select
an absolute parent; reporigor creates its dedicated `reporigor/mutation`
child beneath it. Project build cleanup therefore cannot unlink an active lock
or journal.

Each edit is journaled before replacement. Source restoration uses an atomic
same-directory rename, preserves the original bytes, permissions, access and
modification timestamps, and bounded extended attributes on supported Unix
platforms, and occurs before the result is returned. Files with multiple hard
links are rejected because atomic replacement would split the linked inode. A
timeout or cooperative cancellation kills the spawned process tree before
restoration.
The CLI maps Unix SIGINT/SIGTERM and Windows Ctrl-C to the shared cancellation
token while validation or tests are supervised; polling is bounded to 25 ms.
The first signal therefore returns a clear operational cancellation error only
after child cleanup and restoration. A repeated signal forces immediate exit,
leaving the journal for the next execute-mode recovery if it interrupted an
active edit. Outside a supervised cancellation window, signals terminate
immediately instead of being swallowed.

A future invocation can recover a mutation left by a process crash, but refuses
to overwrite a file changed independently after the journal was written.
Normal guard restoration performs the same content-hash check. Conflicts leave
both the independent source and the bounded recovery journal untouched.
Recovery never recreates a missing target and mutation targets under
version-control or reporigor control paths are rejected. On Unix, the global
active pointer also records the root device/inode identity: recovery can follow
a renamed checkout with the same identity but rejects an unrelated checkout
recreated at the old path.

The executor library's list operation stops after candidate preflight: it does
not recover a journal, apply candidates, or execute validation/tests. The CLI
wraps that operation in the shared coordination session described above.
Recovery can change source bytes and is therefore restricted to explicit
execution or the library's explicit recovery API.

Child cleanup is execution containment, not a security sandbox. Unix commands
run in a dedicated process group and Windows commands in a kill-on-close Job
Object. A deliberately hostile Unix child can create a new session and escape
its inherited group, so project toolchains and configured test commands must be
trusted.

## Reporting and exit policy

The native `ReportEnvelope` has schema version 1 and contains tool identity,
command, canonical root, a cross-section summary, sorted backend provenance,
sorted diagnostics, and optional CRAP/DRY/mutation sections. Integrated
`check` reports also carry `results.rules`: formulas, `RuleSummary`, canonical
`RuleResult` rows, stable surviving-mutant fingerprints, explicit omitted
checks, and baseline metadata.

Rule comparisons are typed. Inclusive `maximum` and `minimum` comparisons use
`max(measured - allowed, 0)` and `max(allowed - measured, 0)` respectively as
finite `excess`. `maximum-exclusive` fails at equality and uses `f64::EPSILON`
as that equality failure's positive excess; it represents the inclusive DRY
clone threshold. A failed boolean has excess `1`; informational metrics pass
with zero. These serialized comparison and excess values let a later native
report detect genuine worsening without recalculating an older rule.

Baseline mode reads `results.rules` from an ordinary prior native envelope.
Its deterministic `analysis_scope` fingerprint must exactly match the current
selection and normalized configuration, preventing a narrower or otherwise
different run from resolving debt produced by another scope.
Matching failures are `existing` unless current excess increased, in which case
they are `worsened`; unmatched current failures are `new`, prior failures that
now pass are `improved`, and disappeared prior failures are counted as
`resolved` only when their rule was evaluated in the current run. The baseline
classification rejects new and worsened violations. The final integrated gate
also requires an empty omission list, so missing evidence cannot be hidden by a
baseline. `check` never writes the configured prior report, and there is no
second baseline schema.

Constructors sort records and use ordered maps, so equivalent snapshots produce
byte-for-byte deterministic JSON. Native JSON omits mutation wall-clock
durations, raw child-command output, truncation state, and output-derived
detail; those values remain internal to execution and human diagnostics. The
text renderer deliberately emits no ANSI escapes. The other formats are
projections:

- SARIF 2.1.0 contains CRAP/DRY findings and failed integrated rule rows.
- Mutation Testing Elements v2 contains mutation results and required source
  text, using fixed score thresholds 60/80 in the CLI projection.

Exit policy is owned by the CLI rather than individual adapters:

- 0: completed and active gates passed;
- 1: operational failure or disallowed mutation execution error;
- 2: quality finding under an active gate.

For mutation, infrastructure/invalid/timeout/disallowed compile-error takes
precedence over survivor quality failure. With integrated baseline mode
disabled, any failed rule makes `check` a quality failure. With it enabled,
new or worsened failures make `check` a quality failure; existing debt,
improvements, resolutions, and informational rows do not. In both modes, any
nonempty `results.rules.omitted` list forces the baseline gate false and makes
`check` exit 2.

## Determinism and side-effect policy

Normal static analysis has no runtime parser downloads. Files, backends,
diagnostics, functions, repository-semantic inventories, duplicate groups,
mutations, formulas, omissions, and rule results are sorted before reporting.
Toolchain versions and fallbacks are included when known.

Every durable rule violation ID is the 64-character lowercase SHA-256 of
length-delimited `(rule_id, repository-relative path, stable_symbol,
normalized structural evidence)`. Repository separators and redundant `.`
components are normalized. Absolute checkout roots, line/column/byte offsets,
timestamps, command durations, thread scheduling, and enumeration order are
not hash inputs. Clone-group and mutation fingerprints use the same structural
principle, so unrelated line movement and input permutation do not rename an
unchanged finding. Paths, symbols, or normalized structure changing may
intentionally produce a new identity.

Canonical report construction rejects absolute/traversing rule paths,
non-lowercase or non-SHA-256 IDs, duplicate IDs, and non-canonical ordering.
Rule constructors require finite numeric measurements and excess; baseline
loading rejects a prior non-finite excess. Stable seeded mutation selection and
the enforced single worker make `max_mutants` reproducible for a fixed snapshot
and seed.

Side effects are explicit by layer:

| Operation | Possible side effects |
|---|---|
| Read-only CLI analysis (`generic`, or `auto` without the trust flag) | Reads project/source files and may create/restrict the per-user coordination lock outside the project; never changes project files. |
| Provider `discover` | Reads files and executable paths only. |
| Provider `preflight` / `auto --allow-project-exec` | Runs bounded toolchain description/version commands; the external toolchain may create normal caches. |
| Native Rust analysis with the trust flag | Runs Cargo metadata/cfg commands and may create normal Cargo target artifacts or evaluate build configuration. |
| Native Clang analysis with the trust flag | Runs bounded Clang validation/AST commands; never generates a build or compilation database. |
| Mutation list through the CLI | Reads and validates candidate source paths under a shared external lock; may create per-user coordination state but performs no recovery. |
| Mutation run | Temporarily edits selected sources, runs configured commands, journals, and restores. |

No provider discovery or preflight path installs packages, updates dependencies,
downloads grammars, runs `npx` for missing tools, or invokes a build generator.

## Extension points

New syntax/project backends implement the core traits and return declared
capabilities plus normalized records. External mutation engines remain
providers: `provider-mutation` imports their supported report shapes rather than
adding engine-specific fields to the shared status model.

A breaking semantic report change requires a new `schema_version`. Adding
optional fields can remain schema v1 when existing meanings and ordering stay
stable.
