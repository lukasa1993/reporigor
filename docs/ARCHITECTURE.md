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
 mutations, backends, diagnostics
              |
      +-------+--------+
      |       |        |
     CRAP     DRY    mutation
      |       |        |
      +-------+--------+
              v
       ReportEnvelope v1
      /       |         \
    text   native JSON   projections
                       SARIF / MTE v2
```

The CLI constructs one `AnalysisRequest`, analyzes the project once per
command, then passes the normalized snapshot to language-neutral analyzers.
`check` reuses that snapshot for all three result sections.

## Workspace boundaries

| Crate | Responsibility |
|---|---|
| `reporigor-core` | Shared language/project enums, requests, discovery, backend traits, capabilities, diagnostics, and normalized records. |
| `adapter-tree-sitter` | Deterministic syntax fallback with eight pinned, compiled-in grammars; functions, complexity, tokens, mutations, and parse diagnostics. |
| `adapter-rust` | Cargo-aware active source/module scope, feature/cfg handling, `syn` function/complexity analysis, `rustc_lexer` token normalization, and Rust mutation candidates. |
| `adapter-clang` | Existing compilation-database discovery, safe argv normalization, bounded Clang validation/JSON AST analysis, and native C-family functions/complexity. |
| `adapter-project` | Filesystem-only provider inventory plus explicit bounded preflight for TypeScript, SwiftPM, Python, Bash, and optional ShellCheck. |
| `analysis-crap` | Coverage loading/path normalization, executable-line matching, and the CRAP formula. |
| `analysis-dry` | Language-neutral normalized-token clone detection. |
| `analysis-mutate` | Locking, recovery journal, source replacement/restoration, subprocess supervision, and mutation status classification. |
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
- `FunctionRecord`: language, qualified name where available, file/range,
  complexity, and optional coverage/CRAP values.
- `TokenRecord`: normalized token value, line, and per-file token index.
- `MutationCandidate`: stable run-local ID, source location, original text,
  replacement, and private byte edit span.
- `Diagnostic`: severity, backend, optional location, message, and explicit
  fallback marker.
- `AnalysisSnapshot`: merged files, backends, functions, per-file token sets,
  candidates, diagnostics, and parse-error count.

Mutation IDs are assigned only after all adapters finish. Candidates are sorted
by file and exact edit span, making IDs deterministic for equivalent input and
toolchain state.

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
normalized executable-line coverage report to each inclusive range and applies:

```text
CRAP = complexity² × (1 - coverage/100)³ + complexity
```

Coverage paths are normalized across absolute/relative and slash conventions.
Ambiguous or empty matches remain explicitly missing rather than being treated
as zero coverage.

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

### Mutation

Adapters enumerate syntax-aware replacements, but do not execute them. The
shared executor either converts candidates to `pending` results (list mode) or
runs optional validation and required tests for one candidate at a time.

Statuses use one vocabulary:

```text
killed, survived, no-coverage, compile-error, runtime-error,
timeout, invalid, ignored, pending
```

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
sorted diagnostics, and optional CRAP/DRY/mutation sections.

Constructors sort records and use ordered maps, so equivalent snapshots produce
byte-for-byte deterministic JSON. The text renderer deliberately emits no ANSI
escapes. The other formats are projections:

- SARIF 2.1.0 contains CRAP-threshold and duplicate-code rules/findings.
- Mutation Testing Elements v2 contains mutation results and required source
  text, using fixed score thresholds 60/80 in the CLI projection.

Exit policy is owned by the CLI rather than individual adapters:

- 0: completed and active gates passed;
- 1: operational failure or disallowed mutation execution error;
- 2: quality finding under an active gate.

For mutation, infrastructure/invalid/timeout/disallowed compile-error takes
precedence over survivor quality failure.

## Determinism and side-effect policy

Normal static analysis has no runtime parser downloads. Files, backends,
diagnostics, functions, duplicate groups, and mutations are sorted before
reporting. Toolchain versions and fallbacks are included when known.

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
