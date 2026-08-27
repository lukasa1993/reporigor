# Portable packaging and release readiness

## Immediate local package

On macOS or Linux, build and smoke-test a portable archive for the current
machine without publishing anything:

```sh
scripts/package-local
```

The script writes `reporigor-<rust-target>.tar.gz` and its adjacent SHA-256
file under `target/dist/`. The archive has the same top-level layout expected by
`cargo-binstall`, contains one multicall executable plus 24 compatibility
symlinks, includes the copy-ready agent prompt, and is extracted and
smoke-tested before the command succeeds.

Any machine with Rust 1.82 or newer can instead build and install directly:

```sh
cargo install --locked --path crates/reporigor
```

## Cross-platform distribution plan

[`../dist-workspace.toml`](../dist-workspace.toml) pins cargo-dist 0.32.0 and is
the canonical machine-readable distribution plan. A validated plan includes
shell and PowerShell installers, a Homebrew formula, an npm binary wrapper,
SHA-256 checksums, source archives, and the following executable archives. The
online channels remain inactive until publication is separately approved.

The `Release artifacts (build only)` GitHub Actions workflow builds release
candidates without publishing them. It runs when a `v*` tag is pushed or when a
maintainer starts it with `workflow_dispatch`. A tag run proceeds only when the
tag is exactly `v` followed by the `reporigor` package version.

Before building, the workflow checks formatting, compilation, Clippy, tests,
documentation, dependency advisories, license allowlists, dependency bans, and
dependency sources. The workflow has read-only repository permissions. It does
not create a GitHub Release, push tags, or publish crates.

## Artifacts

Each run produces one archive and one adjacent `.sha256` checksum file for each
supported target:

| Runner | Rust target | Archive |
| --- | --- | --- |
| Linux | `x86_64-unknown-linux-gnu` | `.tar.gz` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | `.tar.gz` |
| Linux static | `x86_64-unknown-linux-musl` | `.tar.gz` |
| Linux ARM64 static | `aarch64-unknown-linux-musl` | `.tar.gz` |
| macOS | `x86_64-apple-darwin` | `.tar.gz` |
| macOS | `aarch64-apple-darwin` | `.tar.gz` |
| Windows | `x86_64-pc-windows-msvc` | `.zip` |

Archives contain the `reporigor` executable, all 24 `crap4*`, `dry4*`, and
`mutate4*` compatibility command names, the example configuration, the
machine-readable report schemas, `README.md`, `AGENT_PROMPT.md`, `LICENSE`, and
`THIRD_PARTY_NOTICES.md`. Unix archives represent the compatibility commands as
symlinks to the multicall executable; Windows archives contain `.exe` copies.
GitHub retains these workflow artifacts for 14 days. Archive names are stable
per Rust target; a future GitHub Release tag supplies the versioned URL.

The `reporigor` Cargo manifest contains matching `cargo-binstall` metadata, so a
published release can be installed without compiling. cargo-dist's generated
installers also handle PATH placement and select an archive for the host OS and
architecture.

## Verify a download

Keep the archive and its `.sha256` file in the same directory. On Linux, run:

```sh
sha256sum -c reporigor-*.sha256
```

On macOS, run:

```sh
shasum -a 256 -c reporigor-*.sha256
```

On Windows PowerShell, compare the first value in the checksum file with:

```powershell
(Get-FileHash .\reporigor-*.zip -Algorithm SHA256).Hash.ToLowerInvariant()
```

## Promotion is a separate decision

Downloading and verifying these artifacts is the end of the automated flow.
Publishing to crates.io, creating a GitHub Release, attaching assets to a
release, signing artifacts, or distributing packages requires an explicit,
separately reviewed maintainer action. Do not treat a successful artifact build
as a publication approval.
