# RepoRigor

[![CI](https://github.com/lukasa1993/reporigor/actions/workflows/ci.yml/badge.svg)](https://github.com/lukasa1993/reporigor/actions/workflows/ci.yml)

One portable repository-quality CLI for Bash, C, C++, Objective-C, Python,
Rust, Swift, and TypeScript.

RepoRigor also provides migration-compatible `crap4*`, `dry4*`, and
`mutate4*` entry points for all eight languages. See
[`docs/LEGACY_COMMANDS.md`](docs/LEGACY_COMMANDS.md) for flag mappings and the
few intentional compatibility gaps.

## Quick start

```bash
reporigor check .
reporigor crap .
reporigor dry .
reporigor mutate . --list
```

The product combines a shared Rust core and report model with language-aware
project adapters. Generic Tree-sitter parsing is deterministic, compiled into
the binary, and is the subprocess-free default. Pass `--allow-project-exec` to
let `auto` or `native` use existing Cargo, Clang, TypeScript, SwiftPM, Python,
or Bash project toolchains; every selected backend and fallback is reported.

## Install or carry it to another machine

From this checkout, any machine with Rust 1.82 or newer can install the complete
command set with:

```bash
cargo install --locked --path crates/reporigor
```

On macOS or Linux, create a self-contained local archive and checksum with:

```bash
scripts/package-local
```

The command builds only the main multicall executable, adds all 24 compatibility
names as symlinks, includes the schemas/configuration/notices, verifies every
entry point from an extracted copy, and writes the result under `target/dist/`.
Copy that archive to another machine with the same OS and CPU; no Python, Node,
JVM, language grammar download, or Rust toolchain is required there for generic
analysis.

[`dist-workspace.toml`](dist-workspace.toml) is the canonical cross-platform
distribution plan. It is validated with cargo-dist 0.32.0 and describes
checksummed archives for x86-64/ARM64 Linux (GNU and static musl), Intel/Apple
Silicon macOS, and x86-64 Windows, plus shell, PowerShell, Homebrew, and npm
installers. The Cargo package also contains `cargo-binstall` release metadata.
Those online installation channels are configured but are not live until a
maintainer explicitly creates and publishes a release.

## Dogfood gate

RepoRigor checks its own production and test code on every CI run:

```bash
scripts/dogfood
```

This analyzes all Rust files under `crates/` and all shell automation under
`scripts/` through the generic backend. It requires non-empty reports with zero
CRAP-limit, duplicate, mutation, parser, diagnostic, or other findings and
writes the evidence to `target/dogfood/`.

## Agent prompt

Give coding agents the short, copy-ready instructions in
[`AGENT_PROMPT.md`](AGENT_PROMPT.md). It makes RepoRigor the final gate, keeps
native project execution opt-in, and forbids weakening the check merely to make
it pass.

Implementation progress and acceptance gates are tracked in
[`docs/REPORIGOR_TRACKER.md`](docs/REPORIGOR_TRACKER.md).

## Documentation

- [Configuration and CLI reference](docs/CONFIGURATION.md)
- [Migration from the 24 language-specific tools](docs/MIGRATION.md)
- [Legacy executable compatibility](docs/LEGACY_COMMANDS.md)
- [Current implementation architecture](docs/ARCHITECTURE.md)
- [Project-provider policy](docs/PROVIDERS.md)
- [Machine-readable report schemas](schemas/README.md)
- [Portable packaging, installers, and release flow](docs/RELEASING.md)
- [Copy-ready agent quality-gate prompt](AGENT_PROMPT.md)
- [Frozen compatibility baseline](docs/BASELINE.md)

## Status

The unified implementation is under active development. Existing `crap4*`,
`dry4*`, and `mutate4*` repositories remain the compatibility reference until
all migration gates pass.
