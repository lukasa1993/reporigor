# Third-party notices

`reporigor` statically links Tree-sitter and pinned grammar crates for Bash,
C, C++, Objective-C, Python, Rust, Swift, and TypeScript. These crates declare
the MIT license. Their exact versions are recorded in `Cargo.lock` and the
workspace manifest.

The project also uses dependencies under permissive MIT, Apache-2.0,
BSD-family, ISC, Unicode-3.0, Unlicense, or Zlib terms. Release CI must generate
and verify a complete machine-readable license inventory before publishing.

External programs such as Clang, Swift, ShellCheck, cargo-mutants, Stryker,
Mull, Muter, and mutmut are optional providers and are not redistributed unless
a future release explicitly states otherwise. Explicit project preflight/native
analysis invokes applicable tools as separate processes. External mutation
engines are currently discovery/preflight/import-only; only the built-in
mutation executor runs configured validation and test commands.
