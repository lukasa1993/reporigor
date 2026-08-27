# Migration guide

This guide maps the 24 language-specific commands to the unified `reporigor`
workflow. It covers:

```text
crap4{bash,c,cpp,objc,python,rust,swift,ts}
dry4{bash,c,cpp,objc,python,rust,swift,ts}
mutate4{bash,c,cpp,objc,python,rust,swift,ts}
```

Keep the previous tools available while comparing results on your own project.
Their releases remain the rollback path until the compatibility gates in the
[progress tracker](REPORIGOR_TRACKER.md) pass.

## Command mapping

| Previous command family | Unified command |
|---|---|
| `crap4<language>` | `reporigor --language <language> crap` |
| `dry4<language>` | `reporigor --language <language> dry` |
| `mutate4<language> --list` | `reporigor --language <language> mutate --list` |
| `mutate4<language>` execution | `reporigor --language <language> mutate --run` |
| No previous equivalent | `reporigor check` |

## Transitional command shims

Release archives include all 24 previous executable names as multicall shims.
They select a fixed language and translate legacy flags into the unified Rust
engine, so an existing command can be moved to the new installation before its
arguments are rewritten:

```bash
crap4python src --root . --no-test --coverage coverage.json --json
dry4cpp --root . --min-tokens 30 --fail
mutate4swift --root . --list --json
```

The equivalent debug form `reporigor crap4python ...` is also accepted. The
shims retain the previous mutation default (execute unless `--list` is present),
whereas the direct unified command requires `--run`.

These entry points are transitional. They emit the unified report schema, not
the former per-repository JSON layouts, and some unsafe or Rust-specific modes
produce explicit warnings/errors. New automation should prefer direct
`reporigor` commands so language, backend, report format, and mutation
execution are visible. See [LEGACY_COMMANDS.md](LEGACY_COMMANDS.md) for the
verified shim flags, default commands, and intentional gaps.

Use these unified language names:

| Previous suffix | `--language` value |
|---|---|
| `bash` | `bash` |
| `c` | `c` |
| `cpp` | `cpp` |
| `objc` | `objective-c` |
| `python` | `python` |
| `rust` | `rust` |
| `swift` | `swift` |
| `ts` | `typescript` |

The language option is optional when automatic discovery is sufficient. Keep
it during initial parity testing so ambiguous extensions and mixed-language
projects do not broaden the comparison.

## Common argument translation

| Previous argument | Unified argument |
|---|---|
| `--root PATH` | Positional `PATH` after the subcommand |
| Positional path fragments | Repeated `--filter FRAGMENT` |
| `--json` | Global `--format json` |
| `--include-tests` | Global `--include-tests` |
| `--allow-parse-errors` | Global `--allow-parse-errors` |
| `--features a,b` | Global `--features a,b` |
| `--no-default-features` | Global `--no-default-features` |
| `--all-features` | Global `--all-features` |
| Per-tool defaults | Optional shared `reporigor.toml` |

Filters retain the old case-sensitive OR-substring behavior, but the option is
now explicit:

```bash
# Previous
dry4python src/domain src/application --root . --fail

# Unified
reporigor --language python \
  --filter src/domain \
  --filter src/application \
  dry . --fail
```

Global options may appear before or after the subcommand. This guide puts them
before it for readability.

## Migrating CRAP analysis

The important workflow change is that `reporigor crap` consumes coverage but
does not generate it. The previous `crap4*` commands could run a built-in or
user-supplied test command. With the unified CLI, run the project's normal
coverage command first, then analyze the resulting report.

### Python example

```bash
# Previous: the tool ran coverage.py unless --no-test was supplied
crap4python src --root . --fail-over 6 --json

# Unified
coverage run -m pytest
coverage json -o target/coverage/coverage.json
reporigor --language python \
  --filter src \
  --format json \
  crap . \
  --coverage target/coverage/coverage.json \
  --fail-over 6
```

### Rust example

```bash
# Previous
crap4rust --features serde --fail-over 6 --json

# Unified
cargo llvm-cov --workspace --features serde \
  --lcov --output-path target/coverage/lcov.info
reporigor --language rust \
  --features serde \
  --format json \
  crap . \
  --coverage target/coverage/lcov.info \
  --fail-over 6
```

The unified loader accepts LCOV, Cobertura XML, coverage.py JSON, Istanbul JSON,
and LLVM coverage-export JSON. It can also search a supplied directory for a
conventionally named report.

Argument changes:

| Previous CRAP argument | Unified behavior |
|---|---|
| `--coverage PATH` | Same spelling. |
| `--fail-over SCORE` | Same spelling; failure remains strictly `score > limit`. |
| `--allow-missing-coverage` | Same spelling. |
| `--allow-empty` | Same spelling. |
| `--test-command`, `--timeout`, `--no-test` | Removed from CRAP; coverage generation is always a separate step. |

Without `--coverage`, all function scores are unknown and standalone CRAP exits
1 by default. Use `--allow-missing-coverage` only for exploratory complexity
inventory.

## Migrating duplicate-code analysis

The DRY controls map directly:

```bash
# Previous
dry4ts src --root . \
  --min-tokens 30 \
  --max-groups 50 \
  --max-occurrences-per-window 100 \
  --fail --json

# Unified
reporigor --language typescript \
  --filter src \
  --format json \
  dry . \
  --min-tokens 30 \
  --max-groups 50 \
  --max-occurrences-per-window 100 \
  --fail
```

The unified engine validates `min_tokens >= 4`, `max_groups >= 1`, and
`max_occurrences_per_window >= 2`. Some previous Python entry points accepted
nonsensical lower values; those invocations must be corrected.

Unified DRY analysis also has explicit total-window, fingerprint-bucket, and
candidate-work budgets. They can be tuned in `[dry]` only up to immutable
compiled ceilings. Exceeding one is an operational failure rather than a
truncated success. The occurrence cap remains compatibility sampling: it keeps
the earliest occurrences in deterministic file/token order and may omit later
ones.

Native JSON now wraps the DRY section under `results.dry` and always includes
backend and diagnostic provenance. Consumers of the old top-level `duplicates`
array must update their JSON path.

## Migrating mutation testing

Mutation inventory is now the safe default. Execution requires `--run` and an
explicit test command from either the CLI or `reporigor.toml`.

```bash
# Previous inventory
mutate4python src --root . --list --json

# Unified inventory
reporigor --language python \
  --filter src \
  --format json \
  mutate . --list
```

```bash
# Previous execution
mutate4python src --root . \
  --test-command "python -m pytest -q" \
  --validate-command "python -m compileall -q src"

# Unified execution
reporigor --language python \
  --filter src \
  mutate . --run \
  --test-command "python -m pytest -q" \
  --validate-command "python -m compileall -q src"
```

Argument changes:

| Previous mutation argument | Unified behavior |
|---|---|
| `--list` | Same spelling; listing is also the default when `--run` is absent. |
| Normal/default execution | Add `--run`. |
| `--test-command` | Same spelling; required for execution unless configured. |
| `--validate-command` | Same spelling; optional. |
| `--no-validate` | Omit `--validate-command`; there is no separate disable flag. |
| `--timeout`, `--max-mutants`, `--skip-baseline` | Same spelling. |
| `--allow-survivors`, `--allow-compile-errors` | Same spelling. |
| `--manifest` or Rust `--report` | No report-path option; select a stdout format and redirect it. |
| `--json` | Global `--format json`. |

For a persistent native report:

```bash
reporigor --format json mutate . --run \
  --test-command "cargo test --workspace" \
  > target/reporigor/mutation.json
```

For ecosystem interchange, use Mutation Testing Elements v2:

```bash
reporigor --format mutation-json mutate . --run \
  --test-command "cargo test --workspace" \
  > target/reporigor/mutation-elements.json
```

The unified execution mode extends the previous Rust safety work: one global
mutation execution session (safe for overlapping roots), external owner-only
crash-recovery journaling, conflict-aware source restoration, bounded output,
command timeouts, cancellation, and process-tree cleanup. Execute flows acquire
and recover before source analysis and hold the lock through execution. List
mode is project-read-only: the CLI may create owner-only coordination state
outside the checkout and holds a shared lock through analysis/report reads, but
it does not recover a journal or modify project files.

### Rust differential-manifest modes

The current unified command does not expose the Rust-specific `--scan`,
`--update-manifest`, `--since-last-run`, or `--mutate-all` source-manifest
workflow. `reporigor mutate --list` inventories the selected scope, and
`--filter`/`--max-mutants` can narrow it, but they are not semantic equivalents.
Keep `mutate4rust` for automation that depends on embedded differential
manifests until a normalized cache/diff workflow is released.

## Adopting `check`

`check` is the new unified flow. It parses and normalizes a project once, then
emits CRAP, DRY, and mutation sections together:

```bash
reporigor --format json check . \
  --coverage target/coverage/lcov.info \
  --fail-over 6 \
  --min-tokens 30
```

By default, mutation is inventory-only. To execute it:

```bash
reporigor check . \
  --coverage target/coverage/lcov.info \
  --run-mutations \
  --test-command "cargo test --workspace"
```

`check` exits 2 for any CRAP score over the limit, any retained duplicate
group, or disallowed surviving mutant. A mutation infrastructure/error state
takes precedence and exits 1.

## Backend selection during migration

Start with subprocess-free `auto` and retain the `backends` and `diagnostics`
fields in saved reports:

```bash
reporigor --backend auto --format json check . > reporigor-report.json
```

- Without an explicit trust grant, all supported source uses the compiled-in
  Tree-sitter adapter and `auto` records why project execution was skipped.
- Add `--allow-project-exec` when the repository and its local toolchains are
  trusted. Rust Cargo projects can then use the native Rust adapter, and
  C/C++/Objective-C projects can use Clang with an existing compilation
  database.
- Other supported source remains syntax-analyzed by Tree-sitter while
  applicable project providers contribute preflighted metadata.
- Every safe fallback in `auto` is recorded with `fallback_used: true`.

Use `--backend generic` for a subprocess-free syntax baseline. Use
`--backend native --allow-project-exec` only when CI should fail if applicable
project/toolchain prerequisites are missing. Inspect availability with:

```bash
reporigor providers .
reporigor --format json providers . --preflight
```

## JSON and CI migration

The native JSON schema is version 1, but it is a new unified envelope rather
than the old per-tool shape:

```text
schema_version
tool { name, version }
command
root
summary
backends[]
diagnostics[]
results {
  crap? { summary, coverage?, functions[] }
  dry? { summary, duplicates[] }
  mutate? { summary, run?, mutants[] }
}
```

Native mutation records include validated UTF-8 byte spans. Reports produced
by the built-in executor also include run mode, recovery action, and baseline
outcomes; CRAP reports retain coverage matching counters when coverage was
supplied.

Update consumers before replacing a previous JSON command. For code-scanning
systems, prefer `--format sarif` for CRAP/DRY. For mutation dashboards, prefer
`--format mutation-json`.

The stable process exit convention remains:

| Exit | Meaning |
|---:|---|
| `0` | Completed and active gates passed. |
| `1` | Operational/configuration/backend/parse/mutation-execution failure. |
| `2` | Quality gate failed. |

Command-line usage errors also use exit 2, as they did in the previous Rust
Clap and Python argparse entry points.

## Recommended rollout

1. Add `reporigor` without removing the previous command.
2. Pin the same source scope, filters, Cargo features, coverage artifact, and
   thresholds.
3. Save both JSON results in CI without gating on the unified result.
4. Review backend/fallback diagnostics and intentional discovery fixes, such as
   extensionless Bash and `.bats` support.
5. Update JSON consumers to the unified envelope or an interchange format.
6. Enable the unified quality gate.
7. Keep the previous tool available for rollback until representative projects
   pass the compatibility matrix.

See [BASELINE.md](BASELINE.md) for frozen previous behavior and known defects
that the unified interface intentionally does not reproduce.
