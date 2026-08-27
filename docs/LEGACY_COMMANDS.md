# Legacy command compatibility

`reporigor` ships migration shims for every former language-specific command:

- `crap4{bash,c,cpp,objc,python,rust,swift,ts}`
- `dry4{bash,c,cpp,objc,python,rust,swift,ts}`
- `mutate4{bash,c,cpp,objc,python,rust,swift,ts}`

They are multicall entry points backed by the same Rust implementation. The
invoked filename selects the operation and language; there are no copied Python
packages or per-language analyzer forks. A packaged `reporigor` binary may
also be symlinked to any name above. For debugging, the equivalent form
`reporigor crap4python ...` is accepted too.

Cargo installs use small alias launchers beside the main executable. Install
the package normally so `reporigor` and all aliases are present together; a
single `cargo install --bin crap4python` target is not standalone. Build-only
release archives instead contain direct symlinks on Unix and copies of the
multicall executable on Windows.

## Flag translation

| Legacy shape | Unified equivalent |
| --- | --- |
| `crap4python fragment --root app` | `reporigor --language python --filter fragment crap app` |
| `dry4cpp --min-tokens 40 --fail` | `reporigor --language cpp dry --min-tokens 40 --fail` |
| `mutate4swift --list` | `reporigor --language swift mutate --list` |
| `mutate4rust --max-mutants 10` | `reporigor --language rust mutate --run --max-mutants 10` |
| `--json` | `--format json` |
| positional path fragments | repeated global `--filter` values |
| `--root PATH` | operation path `PATH` |
| `--features`, `--no-default-features`, `--all-features` | the matching unified Cargo options |

The shims preserve the old `0` success, `1` operational failure, and `2`
quality/argument failure convention. They also preserve one important mutation
default: old `mutate4*` commands execute mutants unless `--list` is supplied,
while the new `reporigor mutate` command is deliberately read-only unless
`--run` is explicit.

CRAP shims still run the old coverage command when `--no-test` is absent. The
command has bounded output, a process-tree timeout, and a freshness check before
the unified analyzer reads its report. TypeScript uses `npx --no-install`, so a
compatibility invocation cannot silently download Vitest. C, C++, and
Objective-C still require an explicit `--test-command` unless an existing
report is selected with `--no-test`.

Mutation test-command defaults remain `bats tests`, `python -m pytest -q`,
`cargo test --workspace`, `swift test`, and `npm test`. C-family shims recognize
CTest or a `test` Make target. Validation recognizes existing Ninja, CMake, or
Make builds; Rust uses `cargo check --workspace`, Swift uses `swift build`, and
TypeScript uses only an already-installed `node_modules/.bin/tsc`.

## Intentional gaps

The compatibility layer fails loudly or warns where the old behavior cannot be
represented safely:

- Reports use the unified versioned schema and backend provenance. They do not
  reproduce the 24 subtly different legacy JSON layouts.
- `mutate4* --manifest/--report` is accepted, but the unified report goes to
  standard output. Use `--json > target/mutation/results.json` when a file is
  required.
- The Rust-only `--scan`, `--update-manifest`, `--since-last-run`, and
  `--mutate-all` embedded-manifest protocol is rejected with a migration error.
  The unified engine inventories or runs the mutations selected from the
  current source tree.
- The old implicit Swift CRAP command produces LLVM `.profdata`, not a supported
  coverage interchange report. The shim rejects that default before running
  it. Supply `--test-command` that writes LCOV or LLVM JSON, or use
  `--no-test --coverage REPORT`.
- `--verbose` is accepted with a warning because unified operational
  diagnostics already use standard error. `--fail-on-survivors` is accepted
  with a warning because it is already the default.

These aliases are a migration surface, not separate products. New automation
should use `reporigor` directly so backend choice, report format, and mutation
execution are explicit.
