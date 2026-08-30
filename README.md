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

`reporigor check` is the single integrated gate. One merged adapter snapshot feeds
CRAP, exact and near-clone DRY, mutation quality, KISS, YAGNI, dependency/SOLID,
coupling, and cohesion checks, and all results share the native report and exit
policy. A check whose selected adapter cannot establish a required structural
fact records an explicit omission instead of treating missing evidence as a
pass. A nonempty omission list makes the integrated check exit 2, including
when baseline mode is enabled. With complete evidence, optional baseline mode
reads an earlier native RepoRigor JSON report and gates new or worsened
violations; `check` never creates or rewrites that report.

## Install on macOS or Linux without building

Download the release installer, inspect it if desired, then run it:

```bash
curl --proto '=https' --tlsv1.2 -fL \
  https://github.com/lukasa1993/reporigor/releases/latest/download/install.sh \
  -o /tmp/reporigor-install.sh
sh /tmp/reporigor-install.sh
```

It selects Apple Silicon/Intel macOS or ARM64/x86-64 static Linux, verifies the
adjacent SHA-256 checksum, and installs `reporigor` plus all 24 compatibility
commands under `~/.local/bin`. The destination machine needs no Rust, Python,
Node, JVM, or grammar download. Pass a version such as `0.1.0` to the installer
to pin a release, or set `REPORIGOR_INSTALL_DIR` to choose another directory.

## Build or carry it from a checkout

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

[`dist-workspace.toml`](dist-workspace.toml) remains the canonical wider
cross-platform distribution plan. Public macOS/Linux archives are built,
Developer ID signed/notarized where applicable, smoke-tested, and published
from the maintainer Mac—not CI/CD. The exact local process and pinned Linux
builder image digests are documented in [`docs/RELEASING.md`](docs/RELEASING.md).
Windows, Homebrew, npm, and ecosystem-package publication remain later channels.

## Dogfood gate

RepoRigor checks its own production and test code on every CI run:

```bash
scripts/dogfood
```

This runs one normal, project-aware `reporigor check` over all Rust files under
`crates/` and shell automation under `scripts/`. It supplies measured workspace
coverage and executes a fixed six-mutant sample that covers all four mutation
operators. Each mutant uses an isolated Cargo artifact directory so no compiled
mutant can leak into another result. Baseline mode is disabled for this
self-check: every integrated rule must pass, every sampled mutant must be
killed, and `results.rules.omitted` must be empty. Evidence is written to
`target/dogfood/check.json`.

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
