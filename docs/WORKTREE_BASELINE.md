# Source working-tree baseline

Recorded 2026-08-27 before extraction. Original repositories are read-only
inputs to the unified implementation.

The 21 non-Rust repositories were clean. The following Rust repositories had
pre-existing user changes that must not be reset or overwritten:

## `crap4rust`

```text
M .github/workflows/ci.yml
M README.md
M SKILL.md
M src/lib.rs
M src/main.rs
?? bb.edn
```

## `dry4rust`

```text
M .github/workflows/ci.yml
M README.md
M SKILL.md
M src/lib.rs
M src/scoped_lib.rs
?? bb.edn
```

## `mutate4rust`

```text
M .github/workflows/ci.yml
M Cargo.lock
M Cargo.toml
M README.md
M SKILL.md
M src/lib.rs
M src/main.rs
M tests/fixtures/feature-crate/src/lib.rs
?? bb.edn
```

Extraction uses the working-tree versions because they contain the latest
process-safety, active-configuration, differential-scope, and clean-JSON
improvements.
