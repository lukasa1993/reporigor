# Unified RepoRigor — End-to-End Plan and Progress Tracker

> Persistent source of truth for consolidating the `crap4*`, `dry4*`, and
> `mutate4*` projects into one production-quality tool.

## Current status

| Field | Value |
|---|---|
| Last updated | 2026-08-27 |
| Overall state | Dedicated RepoRigor package implemented, self-hosting, locally packaged, and fully verified; exhaustive parity, signing, and publication remain |
| Active phase | Phase 7 — portable local package complete; multi-platform publication and compatibility completion remain |
| Target product | One `reporigor` CLI with `crap`, `dry`, `mutate`, and `check` commands |
| Existing scope | 24 repositories: 3 analyzers × Bash, C, C++, Objective-C, Python, Rust, Swift, and TypeScript |
| Canonical implementation | Local `reporigor/` Rust workspace and `https://github.com/lukasa1993/reporigor` |
| Publication state | Public GitHub source repository created and `main` pushed; no tag, release, crate, installer, or package publication |

Progress rules:

- Mark an item complete only after its acceptance check passes.
- Record important architectural choices in the decision log.
- Append a dated entry to the work log after every material work session.
- Never silently change an existing CLI or output contract; record and test it.
- Preserve unrelated and pre-existing working-tree changes.

## End goal

Users install one tool and run the same workflow in every supported language:

```bash
reporigor check .
reporigor crap .
reporigor dry .
reporigor mutate .
```

The unified tool must:

- discover projects and languages automatically;
- select the best available language/project backend;
- calculate CRAP, duplication, and mutation results through one interface;
- expose which backend ran and any fallback or parse limitations;
- emit consistent terminal, JSON, and CI-friendly reports;
- retain compatibility commands for the existing `crap4*`, `dry4*`, and
  `mutate4*` tools during migration;
- run deterministically in CI without surprise network downloads;
- never crash on malformed or unsupported source code.

## Architecture

Unification happens at the workflow, configuration, scheduling, data model, and
reporting layers. Language parsing and project semantics remain replaceable
adapters.

```text
                              reporigor CLI
                                   |
                 discovery, config, cache, process control
                                   |
               +-------------------+-------------------+
               |                   |                   |
             CRAP                  DRY               mutation
               |                   |                   |
               +---------- normalized model -----------+
                                   |
                        language/project adapters
          +-----------+-----------+----------+----------+-----------+
          |           |           |          |          |           |
        Cargo       Clang      TypeScript  SwiftPM    Python      generic
       + syn       tooling      compiler  SwiftSyntax   AST      Tree-sitter
          |           |           |          |          |           |
       Rust       C/C++/ObjC      TS        Swift      Python   Bash/fallback
```

The tool will support explicit backend selection:

```bash
reporigor --backend auto check .
reporigor --backend native --allow-project-exec check .
reporigor --backend generic check .
```

`auto` may fall back only when it reports the fallback. `native` must fail
clearly when its required project information or toolchain is unavailable.

## What we reuse

| Area | Component | Intended use |
|---|---|---|
| Generic parsing | [Tree-sitter language pack](https://github.com/xberg-io/tree-sitter-language-pack) | Rust library behind an internal adapter; pin versions and required grammars |
| Rust projects | Cargo metadata, `syn`, and the existing Rust repositories | Reuse Cargo feature/target/module discovery and process supervision |
| C-family projects | [Clang tooling and compilation database](https://clang.llvm.org/docs/JSONCompilationDatabase.html) | Native parsing using real compiler flags, includes, macros, and targets |
| TypeScript projects | [TypeScript compiler API](https://github.com/microsoft/TypeScript/wiki/Using-the-Compiler-API) | Isolated provider using `tsconfig.json`; keep unstable API behind the adapter |
| Swift projects | [SwiftSyntax](https://github.com/swiftlang/swift-syntax) and SwiftPM | Toolchain-matched syntax and build configuration provider |
| Bash validation | [ShellCheck](https://github.com/koalaman/shellcheck) | Optional dialect/source-awareness provider; do not embed GPL code |
| Duplication oracle | [PMD CPD](https://pmd.github.io/pmd/pmd_userdocs_cpd.html) | Differential benchmark for clone detection; not a required Java runtime |
| Complexity oracle | [Lizard](https://github.com/terryyin/lizard) | Differential benchmark across supported languages |
| Rust mutation | [cargo-mutants](https://github.com/sourcefrog/cargo-mutants) | Optional native mutation provider |
| Python mutation | [mutmut](https://github.com/boxed/mutmut) | Optional native mutation provider |
| TypeScript mutation | [StrykerJS](https://stryker-mutator.io/docs/stryker-js/introduction/) | Optional native mutation provider |
| C/C++ mutation | [Mull](https://github.com/mull-project/mull) | Optional LLVM-based mutation provider |
| Swift mutation | [Muter](https://github.com/muter-mutation-testing/muter) | Optional native mutation provider |
| Static results | [SARIF 2.1.0](https://www.oasis-open.org/standard/sarif-v2-1-0/) | CI/editor output for CRAP and duplication findings |
| Mutation results | [Mutation Testing Elements](https://github.com/stryker-mutator/mutation-testing-elements) | Interchange output for mutation results |

External mutation tools are providers, not mandatory dependencies. The unified
tool keeps a built-in mutation engine for deterministic basic operation and for
languages without a suitable maintained native engine.

## What we own

- One Rust CLI, configuration format, and stable versioned JSON envelope.
- Project discovery and backend capability negotiation.
- A shared source/file/function/location model.
- Shared CRAP calculation and coverage normalization.
- A small generic token-based duplication engine.
- A built-in syntax mutation engine and mutation execution contract.
- Provider lifecycle: discovery, subprocess execution, timeouts, cancellation,
  caching, diagnostics, and result normalization.
- Compatibility shims for existing command names and flags.
- Corpus, regression, compatibility, and packaging test infrastructure.

We do **not** attempt to invent one universal semantic AST. Each adapter maps
the information it can prove into the shared model and declares its
capabilities.

## Implemented repository layout

```text
reporigor/
  Cargo.toml
  crates/
    reporigor/
    reporigor-core/
    analysis-crap/
    analysis-dry/
    analysis-mutate/
    adapter-tree-sitter/
    adapter-rust/
    adapter-clang/
    adapter-project/
    provider-mutation/
    reporting/
    corpus-harness/
  fixtures/
  corpus/
  schemas/
  docs/
```

The workspace is a dedicated Git repository at `reporigor/`, published at
`https://github.com/lukasa1993/reporigor`. The 24 original repositories remain
separate, untouched compatibility inputs. No release, package publication, or
retirement action is implied by publishing the unified source repository.

## Shared contracts

Every backend returns capabilities and normalized records, conceptually:

```text
BackendCapabilities
  syntax | functions | complexity | tokens | mutations | project_semantics

FunctionRecord
  language | file | name | start | end | complexity | coverage

TokenRecord
  language | file | normalized_kind | text_hash | start | end

MutationRecord
  id | language | file | location | operator | replacement | status | duration

Diagnostic
  severity | backend | file | location | message | fallback_used
```

The schema must be versioned from the first release. Unknown fields should be
forward-compatible, while breaking semantic changes require a new schema
version.

## Delivery roadmap

### Research — completed

- [x] Inventory all 24 repositories.
- [x] Compare duplication across the Python and Rust implementations.
- [x] Run existing package tests and CLI smoke tests.
- [x] Test representative real-world language corpora.
- [x] Investigate generic parser limits and native project APIs.
- [x] Compare maintained complexity, duplication, and mutation tools.
- [x] Select a native Rust core with language-aware adapters.

Research result: the existing small tests pass, but they are insufficient. The
Python tools can segfault on realistic repositories through the current
Tree-sitter point-access pattern. A temporary in-memory workaround removed the
crash and exposed substantial C-family parse gaps: approximately 29.5% of the C
files, 35% of Objective-C files, and 95% of the sampled macro-heavy C++ files
reported parse errors. Python had no errors; TypeScript, Bash, and Swift were
roughly 0–5%. This is why Clang is an early required adapter rather than a late
optimization.

### Phase 0 — baseline, safety, and contracts

- [x] Choose the canonical repository name, GitHub location, license, and
  initial supported platforms.
- [x] Record the current state and uncommitted changes of all source repositories.
- [x] Snapshot every existing CLI: commands, flags, exit codes, text output, and
  JSON output.
- [ ] Add exhaustive old-binary golden output fixtures for all 24 existing tools.
  Unified wrapper/exit fixtures exist, but the original binaries are not yet
  differentially executed in CI.
- [x] Create a repeatable corpus harness with pinned repository revisions.
- [x] Add bounded subprocess regressions for malformed and Unicode input across
  all eight grammars, covering the historical Python binding crash class without
  loading the unsafe Python binding.
- [x] Define JSON schema v1, diagnostic rules, capability declarations, and
  backend fallback policy.
- [x] Define compatibility and deprecation policy.
- [x] Decide whether the new repository starts from one existing repository or a
  clean repository without losing history attribution.

Acceptance gate:

- Existing behavior is machine-recorded and reproducible.
- The crash and corpus limitations are represented by automated tests.
- Schema, fallback behavior, platforms, and repository destination are decided.
- No existing user changes have been overwritten.

### Phase 1 — unified core and CLI skeleton

- [x] Create the Cargo workspace and crate boundaries.
- [x] Implement strict configuration loading and project/language discovery.
- [x] Implement backend traits and capability negotiation.
- [x] Implement versioned diagnostics and JSON envelope.
- [x] Implement functional `crap`, `dry`, `mutate`, `check`, and `providers`
  commands.
- [x] Add bounded subprocess, process-tree timeout, mutation lock/recovery, and
  adapter-scope cache primitives.
- [x] Establish unit, integration, schema, malformed-input, end-to-end, MSRV,
  cross-platform, and dependency-policy CI jobs.

Acceptance gate:

- One binary discovers all target project types and selects a declared backend.
- All commands produce valid schema-v1 reports for selected inputs; an empty
  source selection fails closed except for explicit standalone CRAP opt-in.
- Invalid configuration and unavailable backends fail without panics.

### Phase 2 — shared analyzers and generic backend

- [x] Port the shared CRAP formula and coverage normalization.
- [x] Port one canonical DRY token/clone implementation.
- [x] Port one canonical built-in mutation implementation.
- [x] Implement the Rust Tree-sitter adapter using pinned grammars.
- [x] Avoid runtime grammar downloads in normal CI/offline operation.
- [x] Implement transparent parse-error accounting and strict/permissive policy.
- [ ] Complete exhaustive differential CRAP/DRY output testing against every
  original binary. Algorithm and compatibility fixtures exist, but this gate is
  not yet exhaustive.

Acceptance gate:

- Python, TypeScript, Bash, and basic Swift fixtures run end to end.
- No Python runtime or Python Tree-sitter binding is required.
- Repeated corpus runs are deterministic and do not crash.

### Phase 3 — Rust adapter

- [x] Extract and share Cargo metadata, feature, target, workspace, and module
  discovery from the Rust repositories.
- [x] Consolidate the duplicated Cargo proxy and scope code using per-child
  environment configuration outside the analyzed project.
- [x] Connect `syn`-based Rust analysis to the shared model.
- [x] Preserve current Rust feature selection and document unsupported legacy
  differential-manifest modes.
- [ ] Execute and compare optional `cargo-mutants` with the built-in provider.
  Safe discovery, bounded preflight, and completed-report import are implemented;
  external execution is intentionally disabled pending isolation.

Acceptance gate:

- The unified commands match or intentionally improve all three Rust tools on
  their fixtures and real Cargo workspaces.
- Cargo features, targets, `cfg`, workspaces, cancellation, and timeouts are
  covered by tests.

### Phase 4 — Clang adapter for C, C++, and Objective-C

- [x] Discover and validate `compile_commands.json`.
- [x] Add explicit generation guidance for CMake and other build systems.
- [x] Extract functions, locations, and complexity through bounded Clang JSON
  AST analysis using sanitized translation-unit flags; Tree-sitter supplies
  normalized tokens and built-in mutation sites.
- [ ] Complete corpus coverage for headers, generated files, duplicate
  translation units, macro-heavy C++, and Objective-C++.
- [x] Define clear behavior when a compilation database is unavailable or an
  individual translation unit fails.
- [x] Add optional Mull discovery/preflight and Elements/MTE report import.
  Direct Mull execution remains disabled.

Acceptance gate:

- The pinned C/C++/Objective-C corpora parse with project compiler settings.
- Macro-heavy C++ no longer depends on permissive Tree-sitter error recovery.
- Generic fallback is visible in output and never silently treated as native.

### Phase 5 — remaining project-aware adapters

- [ ] TypeScript: project-local TypeScript 7 CLI discovery and bounded preflight
  are implemented; full references/module-resolution semantics remain future
  native-adapter work.
- [ ] Swift: SwiftPM inventory/preflight is implemented; SwiftSyntax analysis is
  not yet integrated.
- [ ] Python: virtual-environment/project discovery is implemented; syntax still
  uses deterministic Tree-sitter rather than the standard AST.
- [x] Bash: model dialect/shebang and `.bats`; optionally invoke
  ShellCheck without making it mandatory.
- [x] Declare and test per-language capability differences.

Acceptance gate:

- Every supported language has an end-to-end fixture and pinned corpus run.
- Project-aware mode respects each ecosystem's project configuration.
- Missing optional toolchains produce actionable diagnostics.

### Phase 6 — mutation providers and execution workflow

- [x] Finalize provider inventory/import protocol and Mutation Testing Elements
  mapping.
- [ ] Integrate cargo-mutants, mutmut, StrykerJS, Mull, and Muter execution.
  All five have safe discovery/preflight and supported detailed-report import;
  direct execution is deliberately disabled.
- [ ] Complete cross-ecosystem test-command discovery. Explicit CLI/config
  commands and compatibility defaults are implemented.
- [x] Enforce locking, journaled restoration, bounded process trees, and
  compile/test result classification.
- [ ] Add mutation selection, diff-only mode, sharding, and resumable cache.
- [ ] Compare provider results with the built-in mutation engine.

Acceptance gate:

- Mutation never leaves the user's checkout modified.
- Killed, survived, no-coverage, compile-error, runtime-error, timeout, ignored,
  and pending states normalize correctly.
- Interrupted and resumed runs are safe and deterministic.

### Phase 7 — compatibility, packaging, and release

- [x] Implement all 24 legacy command aliases as thin argv-compatible shims.
- [ ] Complete an exhaustive old-versus-new compatibility matrix. Real wrapper
  binary, flag, exit, and read-only mutation tests exist; full legacy golden
  output comparison remains.
- [x] Provide a self-contained local archive builder with one multicall binary,
  24 compatibility aliases, schemas, configuration, notices, checksum, and
  extracted-copy smoke tests.
- [x] Configure and validate cargo-dist plans for x86-64/ARM64 Linux GNU and
  musl, Intel/ARM macOS, and x86-64 Windows.
- [ ] Build signed binaries for the supported platforms.
- [x] Verify local Cargo installation and configure cargo-binstall release
  metadata. Publication remains pending.
- [ ] Publish GitHub Releases and the Cargo/cargo-binstall installation paths.
- [x] Configure shell, PowerShell, Homebrew, and npm installer generation.
  The channels remain deliberately unpublished.
- [ ] Decide whether a PyPI download shim is useful; Python is not required to
  run RepoRigor.
- [ ] Generate shell completions, man pages, configuration reference, and
  migration guide. Configuration and migration guides are complete; completions
  and man pages remain.
- [ ] Produce an SBOM, dependency/license audit, checksums, and provenance.
  Dependency policy and checksummed build-only archives are implemented; SBOM,
  signing, and provenance attestations remain.

Acceptance gate:

- A clean machine can install and run the tool through every supported channel.
- Legacy documented workflows either pass or emit a documented migration error.
- Release artifacts are reproducible, signed, checksummed, and smoke-tested.

### Phase 8 — migration and retirement

- [ ] Release a preview and collect real-project results.
- [ ] Resolve correctness, performance, and compatibility regressions.
- [ ] Publish a stable release with a deprecation timeline.
- [ ] Update all 24 repository READMEs to point to the unified tool.
- [ ] Keep old releases accessible and repositories readable.
- [ ] Archive old repositories only after the compatibility window closes.

Acceptance gate:

- The definition of done below is satisfied.
- There are no unresolved critical compatibility or data-loss defects.
- Users have a documented migration path and rollback option.

## Test matrix

Every language/backend must cover these levels:

| Level | Purpose |
|---|---|
| Unit | Algorithms, schema, configuration, path/location math |
| Adapter fixture | Language constructs and project configuration |
| Golden compatibility | Old versus unified CLI/output behavior |
| Corpus | Real-world repositories at pinned revisions |
| Malformed input | Parse errors, invalid encodings, incomplete source, missing tools |
| Process safety | Timeout, cancellation, signals, child cleanup, checkout restoration |
| Determinism | Same input/config/toolchain produces equivalent normalized output |
| Packaging | Fresh-machine install and smoke test for each release channel |

Required language cases:

- Bash: dialects, shebangs, sourced files, functions, pipelines, substitutions.
- C: headers, macros, generated configuration, multiple translation units.
- C++: templates, concepts, macros, modules where supported, compile flags.
- Objective-C: interfaces, implementations, categories, blocks, mixed C/C++.
- Python: decorators, async, comprehensions, pattern matching, incomplete source.
- Rust: workspaces, features, targets, `cfg`, modules, proc-macro consumers.
- Swift: SwiftPM targets, conditional compilation, extensions, async/closures.
- TypeScript: project references, JSX/TSX, decorators, modules, declaration files.

## Definition of done

The consolidation is complete only when:

- one documented installation provides all three analyzers;
- all eight languages pass their fixture and pinned-corpus gates;
- no supported input can crash or corrupt the process;
- project-aware and fallback behavior is explicit in every result;
- CRAP, duplication, and mutation have stable versioned machine output;
- SARIF and Mutation Testing Elements exports validate;
- mutation execution cannot leave source or manifests modified;
- compatibility tests cover every existing documented command;
- performance and memory budgets are measured and enforced in CI;
- release artifacts pass license, security, provenance, and clean-install checks;
- old repositories have a migration path and are retired only after parity.

## Known risks and controls

| Risk | Control |
|---|---|
| Parser/binding native crash | Use Rust parser API, subprocess crash tests, fuzz malformed input |
| Rapid grammar/package releases | Pin parser and grammar revisions; deliberate update job |
| C-family flags unavailable | Prefer compilation database; explicit generic fallback diagnostic |
| TypeScript compiler API instability | Versioned sidecar adapter and compatibility tests |
| SwiftSyntax/toolchain coupling | Detect toolchain version and maintain tested compatibility matrix |
| External mutation tools missing | Built-in baseline provider and actionable installation diagnostics |
| Different tools disagree semantically | Capability declarations, normalized schema, differential corpora |
| Licensing conflict | Keep incompatible tools external; complete audit before release |
| Legacy behavior traps | Golden CLI/output snapshots and staged deprecation |
| Existing uncommitted work | Inventory before edits; never reset or overwrite user changes |

## Decision log

| Date | Decision | Reason |
|---|---|---|
| 2026-08-27 | Build one native Rust core and CLI | Removes Python binding/runtime failure mode and shares robust process infrastructure |
| 2026-08-27 | Unify workflow, not all language semantics | Cargo, Clang, TypeScript, SwiftPM, Python, and Bash have different project models |
| 2026-08-27 | Use adapters with explicit capabilities | Allows native correctness plus deterministic generic fallback |
| 2026-08-27 | Make Clang an early required adapter | Generic parsing performed poorly on realistic C-family corpora |
| 2026-08-27 | Keep mature mutation engines optional | Reuses ecosystem expertise without imposing every runtime on every user |
| 2026-08-27 | Preserve legacy commands during migration | Enables measured parity and reversible adoption |
| 2026-08-27 | Do not archive existing repositories before parity | Existing users need a stable migration and rollback path |
| 2026-08-27 | Start a clean `reporigor` workspace and retain all original repositories | Avoids overwriting dirty Rust worktrees while preserving their history and rollback path |
| 2026-08-27 | Use MIT, Rust 1.82 MSRV, and seven initial release targets | Matches the source projects and provides x86-64/ARM64 Linux GNU/musl, macOS Intel/ARM, and Windows coverage |
| 2026-08-27 | Compile all generic grammars into the Rust binary | Removes the Python binding/runtime crash class and all runtime grammar downloads |
| 2026-08-27 | Treat project tool execution as an explicit trust boundary | A repository can control local compilers, manifests, build scripts, and compilation flags; ordinary auto analysis must stay subprocess-free unless the user opts in |
| 2026-08-27 | Keep external mutation engines import-only for the preview | Their filesystem/network effects differ too much to execute safely before disposable-checkout isolation exists |
| 2026-08-27 | Keep publication separate from build verification | Local artifacts and packaging can be proven without creating a remote, tag, release, or package publication |
| 2026-08-27 | Name the dedicated product and package `RepoRigor` / `reporigor` | The name describes repository-wide quality enforcement without implying a generic “clean code” doctrine; exact-name checks found no crates.io, npm, PyPI, or GitHub repository collision on the decision date |
| 2026-08-27 | Ship one multicall executable and retain the 24 old names as aliases | One engine avoids per-language implementation drift while the aliases preserve reversible migration paths |
| 2026-08-27 | Use cargo-dist 0.32.0 as the canonical portable release plan | It describes checksummed archives for seven targets and shell, PowerShell, Homebrew, and npm installers while the audited workflow remains build-only until publication is approved |
| 2026-08-27 | Rebaseline corpus hashes only after proving the rename was the sole normalized-report delta | All 13 generic/native reports reproduced their prior hashes when only `tool.name` was substituted back; analyzer counts and behavior did not change |
| 2026-08-27 | Exclude nested local declarations and anonymous Rust/C-family bodies from function metrics | Tree-sitter, `syn`, and Clang cannot give closures/lambdas stable equivalent identities; treating them as complexity boundaries preserves backend-independent counts and attribution |
| 2026-08-27 | Enforce immutable coverage-ingestion budgets | Coverage artifacts are untrusted inputs; preflighted file, discovery, map, Cobertura cross-product, and LLVM expansion ceilings prevent sparse-file, device, allocation, and iteration denial of service |
| 2026-08-27 | Coordinate analysis with shared project locks and mutation with an exclusive lock | Read-only commands may coexist, while mutation execution waits for readers and recovery state cannot race analysis |
| 2026-08-27 | Preserve source metadata and reject multiply linked mutation targets | Byte restoration alone is insufficient; modes, timestamps, bounded extended attributes, checkout identity, and hard-link safety are part of the no-damage contract |
| 2026-08-27 | Replace `quick-xml` with bounded `xml-rs` coverage ingestion | The fixed `quick-xml` line requires a newer compiler than the Rust 1.82 contract; the replacement plus linear preflight removes the advisories without weakening MSRV |
| 2026-08-27 | Fail closed when discovery selects no source files | A successful empty report hides configuration and path mistakes; only standalone CRAP may opt in when coverage-only reporting is intentional |

The exact-name availability check used the public searches for
[crates.io](https://crates.io/search?q=reporigor),
[npm](https://www.npmjs.com/search?q=reporigor),
[PyPI](https://pypi.org/search/?q=reporigor), and
[GitHub repositories](https://github.com/search?q=reporigor&type=repositories).
It records package/repository availability on the decision date, not trademark
clearance or a permanent reservation; publication must repeat the checks.

## Open decisions

- [x] Canonical repository name `reporigor`, published at
  `https://github.com/lukasa1993/reporigor`.
- [x] MIT license for the unified repository, with dependency policy and third
  party notices checked in CI.
- [x] Initial targets: x86-64/ARM64 Linux GNU and musl, macOS x86-64/ARM64,
  and Windows x86-64 MSVC.
- [x] Minimum supported Rust version 1.82.0; pinned development/release toolchain
  1.95.0.
- [x] Configure an npm binary installer alongside native installers.
- [ ] Whether a PyPI shim is required for the first stable release.
- [x] Initial coverage inputs: LCOV, Cobertura XML, coverage.py JSON, Istanbul
  JSON, and LLVM coverage-export JSON. Coverage generation remains project-owned.
- [ ] End-to-end wall-clock and peak-memory budgets for check and mutation
  workflows. Parser, source, coverage, DRY, report, and process limits are
  already enforced.
- [ ] Compatibility window before old repositories are archived.

## Work log

### 2026-08-27 — research and architecture agreement

- Pulled and inspected the 24 language/analyzer repositories.
- Verified existing unit and CLI smoke tests, then tested representative
  real-world corpora.
- Found that repeated Tree-sitter Python `Point.row`/`Point.column` access can
  trigger a native crash that the small repository tests do not expose.
- Confirmed that generic syntax parsing is viable for several languages but not
  sufficient for compiler-configured C-family projects.
- Reviewed native project APIs and mature duplication, complexity, and mutation
  tools.
- Agreed on one Rust CLI/core with project-aware adapters and normalized output.
- Created this persistent end-to-end tracker.

### 2026-08-27 — unified implementation and release-candidate hardening

- Created the new local `reporigor` Cargo workspace without modifying any of
  the 24 original repositories.
- Implemented one CLI and report model for CRAP, DRY, mutation inventory/run,
  combined checks, provider inventory, JSON, SARIF, and Mutation Testing
  Elements v2.
- Compiled pinned Tree-sitter grammars for all eight languages; added Cargo-aware
  Rust, compilation-database-aware Clang, and project metadata/preflight
  adapters.
- Ported coverage normalization, deterministic clone analysis, mutation
  candidate generation, safe execution, journal recovery, and process-tree
  supervision into Rust crates.
- Added all 24 legacy command names with real-binary compatibility tests and
  documented intentional gaps.
- Added optional cargo-mutants, mutmut, StrykerJS, Mull, and Muter discovery,
  bounded version preflight, and supported detailed-report import without
  automatic installation or execution.
- Added fixtures and end-to-end flows for every language, native Rust/Clang
  fixtures, malformed-input subprocess tests, pinned real-world corpus tooling,
  schemas, migration/configuration/provider documentation, cross-platform CI,
  MSRV checks, dependency policy, and build-only release archives.
- Ran full current-toolchain and Rust 1.82 test suites, Clippy with warnings
  denied, rustdoc with warnings denied, schema validation, Cargo packaging, and
  clean-install smoke checks; final gates are rerun after every audit fix.
- A hostile-input audit found and drove fixes for mutation recovery/state
  placement, descendant processes, duration overflow, Clang plugin/config
  injection, provider path substitution, partial native fallback, and unsafe
  default project-tool execution.
- Added shared analysis/exclusive mutation coordination, checkout identity
  checks, hard-link rejection, metadata restoration, bounded xattr handling,
  fallible terminal output, control-character escaping, and per-file Rust
  fallback when native analysis is incomplete.
- Hardened coverage ingestion with bounded regular-file reads, contained
  directory discovery, bounded parser/output maps, and complete preflight of
  LLVM region expansion and Cobertura source/class-line resolution; malicious
  sparse-file, device, symlink, `u32::MAX`, and cross-product regressions now
  fail before expensive reads or generated-line loops.
- Replaced the vulnerable `quick-xml` dependency with Rust-1.82-compatible
  `xml-rs` plus linear Cobertura markup preflight, duplicate-expanded-attribute
  detection, and explicit depth, namespace, attribute, entity, and text bounds.
- Defined canonical function semantics and added generic/native parity tests:
  Rust local functions/closures and C++ lambdas are not report rows, and their
  decisions never inflate the enclosing function's complexity.
- Optimized Mutation Testing Elements import to index positions once per file,
  memoized coverage-path resolution, and bounded Muter basename matching and
  DRY candidate work.
- Pinned nine real repositories covering all eight languages, with 13 generic
  and native baseline records. Both required corpus runs and checkout revision
  verification pass.
- Verified the settled dependency graph with current Rust and Rust 1.82 full
  test suites, warnings-as-errors Clippy, warnings-as-errors rustdoc, all four
  `cargo-deny` policy categories, release builds, all 13 package tarballs, and a
  clean install exposing all 25 executable entry points.
- Cross-checked the platform-neutral and Windows-specific Rust crates for
  `x86_64-pc-windows-gnu`. A full local Windows cross-build remains an
  environment-only gap because the host has no MinGW C compiler/sysroot for
  bundled Tree-sitter grammars; native Windows MSVC CI is configured.
- No remote repository, tag, release, package publication, signing, or archival
  action was performed.

### 2026-08-27 — RepoRigor identity, portability, and self-hosting

- Selected `RepoRigor` as the product name and exact `reporigor` identifier
  after checking crates.io, npm, PyPI, and GitHub repository names for an exact
  collision. Renamed the repository, user-facing executable, public report
  identity, core/reporting/process crates, configuration files, cache/state
  paths, schemas, documentation, and environment variables consistently.
- Added a pinned cargo-dist 0.32.0 plan for seven Rust targets, SHA-256
  checksums, source archives, shell and PowerShell installers, Homebrew, npm,
  and cargo-binstall-compatible release URLs. The existing audited workflow
  builds candidates only and has no publication permission.
- Added `scripts/package-local`, which builds one release multicall binary,
  creates all 24 legacy aliases, includes the schemas/configuration/license/
  notices, verifies the checksum, extracts the archive, and smoke-tests every
  entry point. The local ARM64 macOS result is
  `target/dist/reporigor-aarch64-apple-darwin.tar.gz`.
- Added `scripts/dogfood` to CI. RepoRigor analyzed 101 Rust files with 1,430
  functions and 3 shell files with 8 functions; both reports had zero CRAP
  violations, duplicate groups, mutation errors, findings, parse errors, or
  diagnostics.
- Added a short copy-ready agent prompt that requires RepoRigor as the final
  code-change gate, keeps native project execution trust-gated, forbids
  weakening checks to manufacture a pass, and requires an exact-command/result
  handoff. Portable archives include the prompt.
- Proved the product rename was the only normalized corpus change: substituting
  the former tool name into each of all 13 generic/native reports reproduced
  every frozen SHA-256 exactly. Deliberately updated the identity hashes, then
  reran all nine pinned repositories and obtained a matching baseline.
- Passed formatting, all-target/all-feature compilation, warnings-as-errors
  Clippy, the complete current-toolchain test suite, warnings-as-errors rustdoc,
  the complete Rust 1.82 MSRV test suite, and advisories/bans/licenses/sources
  policy checks.
- Packaged and independently verified all 13 workspace crates. A fresh source
  install created exactly 25 executable entry points and every `--version`
  smoke test passed. The local archive checksum and all 25 extracted entry
  points also passed.
- Parsed every GitHub Actions workflow, validated the cargo-dist plan, and
  configured build-only jobs for all seven release targets. Only the local
  ARM64 macOS archive was compiled on this host; the other platform jobs remain
  CI verification work.
- Re-audited all 24 original repositories: 21 remain clean, and the exact 6/6/9
  pre-existing changes in `crap4rust`, `dry4rust`, and `mutate4rust` remain.
  Every pinned corpus checkout is clean.
- No remote repository, tag, release, package publication, signing, or archival
  action was performed.

### 2026-08-27 — public repository and agent handoff prompt

- Added `AGENT_PROMPT.md`, a short copy-ready quality-gate prompt for coding
  agents, linked it from the README, and included it in local, cargo-dist, and
  build-only release archives.
- Reran RepoRigor dogfood with zero findings, rebuilt and checksum-verified the
  local ARM64 macOS archive, confirmed the prompt is present after extraction,
  revalidated all seven cargo-dist archive plans, and parsed every workflow.
- Audited all 204 staged files for whitespace errors, generated/build content,
  corpus checkouts, and common credential patterns before publication.
- Created the public GitHub repository `lukasa1993/reporigor` and pushed the
  initial `main` branch. No tag, release, crate, installer, or package was
  published.
- Used the first clean GitHub Actions run as a portability audit and fixed four
  issues it exposed: macOS rustup shims resolving through `rustup-init`, Linux
  container zombies confusing cancellation assertions, Windows same-process
  file-lock semantics, and legacy CLI tests sharing user-level state.
- Added focused regressions, repeated the formerly racy legacy and cancellation
  tests five times, and passed formatting, strict Clippy, the complete current
  and Rust 1.82 suites, Windows-target compilation, and RepoRigor dogfood with
  zero findings before the corrective push.

## Next action

Use the dedicated RepoRigor package for local trials, then finish exhaustive
old-versus-new output parity and the remaining C-family/project-semantics cases.
External mutation providers require disposable-checkout isolation before direct
execution. Run the seven target builds in CI before publication; signing,
SBOM/provenance, publication, and old-repository retirement remain separate
explicit decisions.
