# Mutation provider boundary

The unified tool reuses mature ecosystem mutation engines without making them
trusted, mandatory dependencies. The built-in syntax-aware engine is the only
engine reporigor executes by default. Optional external engines are discovered
and their existing reports can be normalized into the common result model.

## Safety contract

- `reporigor providers` performs filesystem and executable-path inspection
  only. It does not spawn a process.
- `reporigor providers --preflight` may run one direct-argv `--version` probe
  per applicable executable. Every probe has a timeout, a captured-output cap,
  closed stdin, and no shell.
- Discovery and preflight never invoke `cargo install`, `pip`, `pipx`, `npm`,
  `npx`, `brew`, SwiftPM resolution, or any other installer.
- The subprocess boundary rejects commands classified as mutation runs.
- External mutation execution is not exposed by the CLI yet. Tests, build
  scripts, and provider configuration can have arbitrary filesystem or network
  effects even when a provider mutates a scratch copy.
- Existing report import is read-only. Report paths containing `..` or absolute
  paths outside the selected root are rejected.

## Provider matrix

| ID | Language | Discovery | Detailed import | Direct execution |
|---|---|---|---|---|
| `built-in` | all eight supported languages | always available | MTE v2 | enabled; deterministic default |
| `cargo-mutants` | Rust | explicit override or `cargo-mutants` on `PATH` | completed `mutants.out/outcomes.json`; MTE v2 | disabled |
| `mutmut` | Python | `.venv`/`venv` executable, override, or `PATH` | MTE v2 conversion only | disabled |
| `stryker` | TypeScript | project `node_modules/.bin/stryker` or override | native MTE v2 JSON | disabled |
| `mull` | C/C++ | override or `mull-runner[-13..22]` on `PATH` | Elements/MTE 1.x and MTE v2 | disabled |
| `muter` | Swift | `.build/debug/muter`, override, or `PATH` | tolerant native Muter JSON | disabled |

Stryker is intentionally not resolved with `npx`: npm may download a missing
package. A project-local binary or explicit override is required.

## Normalization

The importer retains each provider's string identifier and derives a stable
64-bit internal identifier from provider, file, and external ID. It maps
results to:

```text
killed, survived, no-coverage, compile-error, runtime-error,
timeout, invalid, ignored, pending
```

Mutation Testing Elements durations are milliseconds and are converted to
seconds. Source ranges are validated against the source embedded in the report.
All normalized records retain language, root-relative file, location, original
text, replacement text, status, duration, and provider detail.

Provider-specific rules:

- Stryker's JSON reporter already emits Mutation Testing Elements and uses the
  shared importer.
- Mull's Elements reporter currently emits schema 1.7, so Mull alone may import
  MTE 1.x as well as v2.
- cargo-mutants `outcomes.json` is upstream-documented as changeable and is
  written incrementally. reporigor requires an embedded provider version and
  non-null `end_time`, validates the recognized current shape, skips the
  baseline scenario, and warns about the compatibility boundary.
- mutmut exposes summary JSON but no stable, detailed public JSON report.
  Summary counts cannot reconstruct individual mutations, so convert detailed
  results to MTE v2 first.
- Muter JSON is unversioned and omits full paths. It is accepted only when each
  reported basename resolves to exactly one project file and the reported
  before-text matches source at the reported position.

## Why external execution remains disabled

The engines have materially different effects:

- cargo-mutants normally copies the project, but writes/rotates `mutants.out`,
  runs Cargo metadata/build/tests, and has an unsafe `--in-place` mode.
- mutmut persistently creates a `mutants/` tree and runs tests there.
- Stryker normally uses `.stryker-tmp`, but project configuration can enable
  in-place mutation or dashboard/network reporters.
- Mull mutates an instrumented executable rather than source, but repeatedly
  executes the target program.
- Muter deletes/recreates a sibling `<project>_mutated` directory and performs
  an update check unless explicitly disabled.

The next execution phase must use a disposable checkout or tool-owned snapshot,
provider-specific configuration validation, process-tree cancellation, network
policy, unique artifact directories, and post-run integrity verification. Until
those invariants are implemented and tested, report import gives ecosystem
reuse without risking the user's checkout.

## Primary references

- [Mutation Testing Elements report schema](https://github.com/stryker-mutator/mutation-testing-elements/blob/master/packages/report-schema/src/mutation-testing-report-schema.json)
- [StrykerJS configuration](https://stryker-mutator.io/docs/stryker-js/configuration/)
- [cargo-mutants output directory](https://mutants.rs/mutants-out.html)
- [mutmut repository and CLI](https://github.com/boxed/mutmut)
- [Mull runner](https://mull.readthedocs.io/en/latest/command-line/mull-runner.html)
- [Muter repository](https://github.com/muter-mutation-testing/muter)
