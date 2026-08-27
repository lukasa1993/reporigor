# Provider policy

`reporigor` separates analysis backends from project-provider preflight. That
distinction matters when interpreting `reporigor providers`:

- Rust and Clang are analysis adapters selected by analysis commands. Their
  identities and versions appear in report `backends` and diagnostics.
- TypeScript, SwiftPM, Python, Bash, and ShellCheck are the inventory rows
  returned by `reporigor providers`.
- Tree-sitter is the compiled-in generic syntax backend, not an external
  provider.

## Provider inventory

| ID | Applicable when | Resolution | Required by `native` when applicable | Fallback |
|---|---|---|---|---|
| `bash` | Bash source is discovered | Dialect from shebang/`.bats`; existing `bash` on PATH is used for version metadata when present | Yes, but built-in dialect discovery keeps the row available without an executable | Tree-sitter |
| `python` | Python source or project metadata is discovered | `.venv`/`venv` interpreter, then `python3`/`python` on PATH | Yes | Tree-sitter |
| `shellcheck` | Bash source is discovered | Existing `shellcheck` on PATH | No; always optional | Built-in Bash dialect metadata |
| `swiftpm` | `Package.swift` is present | Existing `swift` on PATH | Yes | Tree-sitter |
| `typescript` | TypeScript source, `tsconfig.json`, or a declared TypeScript dependency is found | Project-local `node_modules/.bin/tsc` only | Yes | Tree-sitter |

The public adapter library also supports explicit executable overrides through
`ProviderOptions`. The current CLI exposes an explicit executable option only
for Cargo (`--cargo`); other provider rows use the resolution above.

## Static discovery and preflight

```bash
reporigor providers .
reporigor --format json providers . --preflight
```

Without `--preflight`, provider discovery reads project files and searches
existing executable paths. It does not spawn commands. Static output does not
contain confirmed tool versions.

`--preflight` runs bounded commands with null stdin, captured output, and a
default 15-second timeout per command:

| Provider | Preflight commands |
|---|---|
| Bash | `bash --version` when an executable exists |
| Python | selected interpreter `--version` |
| ShellCheck | `shellcheck --version` |
| SwiftPM | `swift --version`; `swift package describe --type json` |
| TypeScript | local `tsc --version`; `--showConfig -p tsconfig.json --pretty false`; `--listFilesOnly -p tsconfig.json --pretty false` |

TypeScript preflight uses the resolved compiler configuration and configured
file list to refine the selected TypeScript source set. TypeScript 7 is used
through its CLI because it does not expose the previous stable programmatic
compiler API.

Expected missing executables, probe failures, nonzero exits, timeouts, or
invalid JSON are represented in inventory and diagnostics. One failed optional
provider does not abort the entire provider report.

## Analysis routing

The three policies are:

- `--backend generic`: call static provider discovery but no provider
  preflight, then use the compiled-in Tree-sitter syntax backend.
- `--backend auto` without `--allow-project-exec`: use static discovery and the
  generic syntax backend, with an explicit trust-boundary fallback diagnostic.
- `--backend auto --allow-project-exec`: preflight applicable providers, prefer
  native Rust and Clang analysis where their project prerequisites exist, and
  emit an explicit fallback diagnostic when a safe generic path is used.
- `--backend native --allow-project-exec`: preflight providers and fail when an
  applicable required provider is unavailable; native Rust/Clang failures are
  also fatal. Native analysis without the trust flag is rejected before
  project execution.

Analysis commands perform provider preflight only after this explicit trust
grant. The standalone `providers --preflight` command is itself the explicit
request to run its bounded inventory probes and does not require the flag.

For TypeScript, Swift, Python, and Bash, the current providers contribute
project/source metadata, toolchain versions, availability, and diagnostics.
Syntax functions, complexity, tokens, and built-in mutations for those
languages still come from Tree-sitter. This capability split is visible in the
report's backend declarations.

## Rust and Clang analysis adapters

These adapters are intentionally outside the `providers` inventory:

| Adapter | Prerequisite | Commands/behavior | Automatic generation |
|---|---|---|---|
| Cargo-aware Rust | `Cargo.toml` and usable Cargo | Cargo metadata/active cfg and module-scope resolution; native `syn`/`rustc_lexer` analysis | No dependency installation; Cargo may create normal target artifacts or evaluate build configuration |
| Clang C-family | Existing `compile_commands.json` and usable `clang` | Bounded translation-unit validation and JSON AST extraction with compilation flags | Never configures a build or generates a compilation database |

Clang database discovery checks the project root, `build`, `.build`, and `out`,
then immediate child directories. Generate the database with the project's own
build workflow before invoking `reporigor`; the tool will not run CMake,
Meson, Bear, or another generator.

## Side-effect and network rules

Provider discovery and preflight never:

- install or update packages;
- invoke `npx` to fetch a missing TypeScript compiler;
- download Tree-sitter grammars;
- create a package manifest, build configuration, or compilation database;
- invoke a shell to interpret a compilation-database command string.

Preflight does execute the bounded description/version commands listed above.
It does not invoke an install/update operation, although an external toolchain
can still create its normal caches while loading a project. Native Rust analysis
has the normal Cargo side effects described above. Mutation execution is a
separate explicit workflow documented in
[ARCHITECTURE.md](ARCHITECTURE.md#mutation-safety).

Optional mutation engines use a stricter boundary. `reporigor providers`
includes the built-in engine plus cargo-mutants, mutmut, StrykerJS, Mull, and
Muter. Static discovery never starts any of them. `--preflight` runs only a
timeout- and output-bounded version probe, never a mutation run or installer.
External execution is currently import-only; the built-in engine remains the
deterministic default. See [Mutation providers](MUTATION_PROVIDERS.md).

## Provider JSON

`reporigor --format json providers` emits `ProviderResolution`, not the native
analysis `ReportEnvelope`. Its top-level fields are:

```text
context {
  root, kinds[], sources[], backends[], diagnostics[]
}
inventory[] {
  id, project, capabilities, applicable, available,
  required_for_native, executable?, version?, fallback?, reason?, hint?
}
provenance[] {
  id, backend, executable?, version?, commands[], metadata{}
}
mutation {
  root,
  providers[] {
    id, name, languages[], applicable, available, default,
    execution_enabled, executable?, detection?, version?,
    import_formats[], reason?, hint?
  }
}
```

`commands` is empty after static discovery and records each bounded argv after
preflight. Executable paths, versions, metadata keys, inventory rows, and
diagnostics are emitted in deterministic order for equivalent project state.
