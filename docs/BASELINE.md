# Existing-tool compatibility baseline

This document freezes the behavior that the unified tool must preserve through
compatibility entry points. The unified commands may expose a cleaner schema,
but departures from this baseline must be tested and documented.

## Inventory

The source set contains 24 MIT-licensed repositories owned by `lukasa1993`:

```text
crap4{bash,c,cpp,objc,python,rust,swift,ts}
dry4{bash,c,cpp,objc,python,rust,swift,ts}
mutate4{bash,c,cpp,objc,python,rust,swift,ts}
```

There are three canonical generic Python engines, not 21 distinct algorithms:

- CRAP: `crap4c/src/crap4c/core.py` (the C/C++ declarator fix is the superset).
- DRY: `dry4python/src/dry4python/core.py`.
- Mutation: `mutate4python/src/mutate4python/core.py`.

The three Rust tools provide valuable Cargo-aware source selection, `syn` and
`rustc_lexer` analysis, safe process supervision, and mutation recovery. Their
376-line `cargo_proxy.rs` is byte-for-byte identical.

## Common exit contract

| Exit | Meaning |
|---:|---|
| `0` | Successful analysis and quality gate passed |
| `1` | Configuration, parse, execution, or infrastructure failure |
| `2` | Quality gate failed |

Legacy argument parsers also use exit 2 for usage errors. Compatibility shims
retain that behavior; the unified JSON envelope additionally declares the exit
category.

## Common language profile

| Language | Source extensions | Default test exclusions |
|---|---|---|
| Bash | `.sh`, `.bash` | `test/`, `tests/`, `.bats` |
| C | `.c`, `.h` | test directories, `_test.c`, `_tests.c` |
| C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`, `.hxx`, `.h` | test directories and test suffixes |
| Objective-C | `.m`, `.mm`, `.h` | `Test[s]/` and `Test[s].m/.mm` |
| Python | `.py` | `test/`, `tests/`, `_test.py` |
| Rust | `.rs` | Cargo test/auxiliary targets and test paths |
| Swift | `.swift` | `Test[s]/` and `Test[s].swift` |
| TypeScript | `.ts`, `.tsx` | test directories and test/spec suffixes |

Unified discovery intentionally fixes two legacy gaps: extensionless shebang
Bash files and `.bats` files can be discovered, and `.d.ts` is treated as a
declaration rather than as a test. Ambiguous headers are resolved by project
providers when possible and reported otherwise.

## CRAP contract

Legacy flags shared by the generic tools:

```text
[filters...] --root --coverage --test-command --timeout --no-test
--allow-missing-coverage --allow-empty --allow-parse-errors --include-tests
--json --fail-over --version
```

Rust adds Cargo feature flags and uses LCOV. Generic implementations support
LCOV, Cobertura, coverage.py JSON, Istanbul JSON, and LLVM export JSON.

Formula:

```text
CRAP = complexity^2 * (1 - coverage/100)^3 + complexity
```

Schema-v1 legacy report:

```text
schema_version, tool, version, root
summary { functions, missing_coverage, over_limit, limit }
functions[] { name, file, start_line, end_line, complexity, coverage, crap }
```

Quality failure is strictly `crap > limit`.

## DRY contract

Legacy flags:

```text
[filters...] --root --min-tokens 30 --max-groups 50
--max-occurrences-per-window 100 --include-tests --json --fail --version
```

Schema-v1 report:

```text
schema_version, tool, version, root
summary { groups, min_tokens }
duplicates[] { token_count, locations[] }
```

Generic location records include token offsets; the Rust legacy report omits
them. Compatibility reports preserve the relevant legacy shape.

## Mutation contract

Legacy generic flags:

```text
[filters...] --root --test-command --validate-command --no-validate
--timeout --max-mutants --list --skip-baseline --include-tests
--manifest --json --allow-survivors --allow-compile-errors --version
```

Rust additionally supports Cargo features, differential scans and embedded
source manifests. Mutation status precedence is:

1. infrastructure, invalid, timeout, or disallowed compile error → exit 1;
2. disallowed survivor → exit 2;
3. otherwise → exit 0.

The unified report normalizes status names to the Mutation Testing Elements
vocabulary while compatibility output maps back to each legacy schema.

## Known legacy defects not to preserve in the unified interface

- Python Tree-sitter point property access can segfault on realistic projects.
- Generic C-family parsing ignores build flags, macros, and includes.
- Python CRAP can mix test-command output into JSON stdout.
- Generic mutation has no concurrent-run lock.
- Bash mutation cannot truly disable its built-in syntax validation.
- Parse errors abort entire generic DRY/mutation runs.
- `.h` language ownership is ambiguous by extension alone.
- Objective-C++ `.mm` files use an incomplete generic grammar.

Compatibility shims may emulate output shapes, but never reproduce crashes,
unsafe mutation behavior, or invalid JSON streams.
