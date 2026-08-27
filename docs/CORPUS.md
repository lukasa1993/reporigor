# Pinned corpus regression harness

The corpus harness is deliberately opt-in. Ordinary `cargo build`, `cargo
test`, and CI fixture jobs never contact a network, clone a repository, or run
the large real-world corpus. The lock file is parsed and validated locally, but
only an explicit `populate` operation performs Git network I/O.

The source of truth is [`../corpus/corpus.lock.toml`](../corpus/corpus.lock.toml).
Every entry contains a full commit ID, license, CI tier, permitted backend
modes, path filters, wall-clock limit, and report-size limit. The lock covers
all eight language grammars. `crap4rust` supplies the Rust corpus at a commit
already present in the original repository inventory; this avoids inventing an
unverified revision.

The following check is network-free and does not require corpus checkouts:

```sh
scripts/corpus-harness validate
scripts/corpus-harness validate --native --require-all
```

It validates the lock and binds every baseline record to a unique declared
`name + backend`, language, and revision. It also checks lowercase SHA-256
format. `--require-all` requires a baseline for every selected generic mode;
adding `--native` requires every selected declared native mode as well.

## Populate and verify

From the workspace root:

```sh
scripts/corpus-harness populate --require-all
scripts/corpus-harness verify --require-all
```

`populate` initializes each missing directory, fetches exactly the locked
commit at depth one, and checks it out detached. It never updates or deletes an
existing checkout. If a checkout is at the wrong revision, has tracked or
untracked changes, or points at a different origin, verification fails with the
exact path. Move or repair that checkout yourself, then verify again.

Use `--name NAME` (repeatable) to populate or verify a subset, and
`--checkout-root PATH` or `REPORIGOR_CORPUS_ROOT` to keep checkouts elsewhere.
Use `--tier pull-request` for the smaller fast gate or `--tier scheduled` for
the larger periodic set. Name and tier selectors are combined, so a mismatched
selection is rejected instead of becoming an accidental no-op.
For example:

```sh
scripts/corpus-harness populate --name crap4rust
scripts/corpus-harness verify --name crap4rust
scripts/corpus-harness run --tier pull-request --require-all
```

## Run and compare

Build the unified CLI and compare every present generic corpus against the
committed baseline:

```sh
scripts/corpus-harness run --require-all
```

Native modes are a separate explicit gate because they depend on installed
toolchains and project metadata. Only entries declaring `native` in the lock
are attempted:

```sh
scripts/corpus-harness run --native --require-all
```

The CLI's project-execution trust grant is added to argv only for a locked
`native` run. Generic corpus runs never receive `--allow-project-exec`, so they
remain filesystem-only even when native runs are selected in the same harness
invocation. Selecting `--native` is therefore the explicit authorization point
for existing Cargo, Clang, SwiftPM, Python, or Bash project tooling.

Each `reporigor` subprocess receives a per-entry wall-clock timeout, a maximum
captured-output size, language selection, deterministic path filters, permissive
parse-error reporting, and a high DRY window. The harness writes its own config
under `target/corpus-harness/`, outside every checkout, retaining two
occurrences per fingerprint and using the immutable 25,000,000-unit candidate
work ceiling. This keeps highly repetitive real projects bounded without
weakening the analyzer's fail-closed budget.

A timeout, signal, operational exit, oversized report, or invalid schema is a
harness failure. Exit 2 is accepted only when stdout is a valid quality-gate
report. The checkout commit, origin, and clean status are verified again after
every analysis so a provider cannot silently turn a read-only regression run
into a source change. Each subprocess starts in operating-system containment—a
process group on Unix and a Job Object on Windows. Timeout/error cleanup
terminates the whole descendant tree, and successful direct-child exit also
cleans up any compiler or build-script descendants that retained output pipes.
This containment is for reliable cleanup, not a sandbox; corpus project
toolchains must still be trusted.

Raw stdout/stderr, path/version-normalized JSON, and the current summary are
written below `target/corpus-harness/`. The normalized SHA-256 covers the full
report after replacing the checkout root and tool/backend versions; the
committed [`../corpus/baseline.toml`](../corpus/baseline.toml) also records the
exit code and stable summary counts.

## Review and update

Updating expected results is intentionally distinct from running them:

```sh
scripts/corpus-harness run --name crap4rust
# Inspect target/corpus-harness/crap4rust.generic.normalized.json and the diff.
scripts/corpus-harness update --name crap4rust
```

Add `--native` to update native records too. `update` merges only the selected,
present records; it does not erase baselines for absent corpora. Review changes
to `corpus/baseline.toml` like source changes: count shifts can be legitimate,
but unexpected parse-error or mutant changes require investigation.

To change a corpus revision, first update its full commit in the lock, create a
fresh detached checkout through the explicit populate flow, run both applicable
modes, inspect normalized artifacts, and then update the baseline. Never use a
branch or tag in the lock.

The ordinary CI workflow runs only `validate`, so pull requests never fetch the
corpus. A separate read-only corpus workflow performs explicit population and
generic comparison on its schedule or by manual dispatch; manual dispatch can
also opt into declared native modes. Neither workflow receives repository write
permission.
