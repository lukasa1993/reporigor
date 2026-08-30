# Machine-readable report schemas

`reporigor-report-v1.schema.json` describes the lossless native envelope
written by `reporigor --format json`. Its integer `schema_version` is `1`.
Consumers should use this format when they need diagnostics, backend
provenance, or more than one analysis result in a single document.
CRAP sections retain coverage-application counters, and mutation sections
retain built-in run mode, recovery, and baseline provenance when they came
from the shared executor.

`check` reports may also contain `results.rules`, the unified deterministic
rule stream. It carries the formula catalog, canonical core `RuleResult` rows,
their `RuleSummary`, surviving-mutant fingerprints, capability-gated omitted
checks, an `analysis_scope` fingerprint, and optional baseline comparison
metadata. Baselines point to an
ordinary prior native RepoRigor report; they do not introduce another data
format. Rule comparison values are `maximum`, `maximum-exclusive`, `minimum`,
`boolean`, and `informational`. `maximum-exclusive` passes only when the
measured value is strictly below the allowed value, which preserves inclusive
threshold detection for rules such as DRY similarity.

Every rule row contains `rule_id`, stable `violation_id`, repository-relative
`file`, `stable_symbol`, `measured`, `allowed`, `algorithm`, `result`,
`structural_evidence`, `comparison`, `excess`, and `baseline`. Runtime construction additionally
enforces canonical relative paths, globally unique lowercase SHA-256 IDs, and
canonical ordering—constraints that JSON Schema alone cannot fully express.
`RuleSummary` serializes `total`, `passed`, `failed`, and the baseline counts
`baseline_existing`, `baseline_new`, `baseline_worsened`,
`baseline_improved`, and `baseline_resolved`.

Violation IDs hash length-delimited rule ID, normalized repository-relative
path, stable symbol, and normalized structural evidence. Absolute checkout
roots, line/column/byte locations, timestamps, durations, worker scheduling,
and input enumeration order are excluded. Path separators are normalized.
Clone-group IDs and mutation fingerprints follow the same structural identity
policy, while source locations remain report and edit coordinates.

With baseline mode enabled, failed rows are classified as `new`, `worsened`,
or `existing`; a matching prior failure that now passes is `improved`, and a
missing prior failure is counted as `resolved` only if its rule was evaluated
in the current run. With complete evidence, the baseline classification uses
new and worsened counts. Capability-gated omissions are serialized as rule
ID/reason rows and cannot manufacture a pass or resolution; any nonempty
`omitted` list forces `results.rules.baseline.gate_passed` to `false` and makes
`check` exit 2. A prior report is accepted only when its `analysis_scope`
exactly matches the current check selection and normalized configuration.
`check` only reads the configured prior native report and never creates or
rewrites it.

Native DRY clone groups may include `clone_group_id`, Dice `similarity`,
recursive `statement_count`, `algorithm`, and per-location `stable_symbol`.
Native mutation candidates include `stable_symbol`, fixed `operator`, and a
stable structural `fingerprint`. Native JSON deliberately omits mutation
durations, raw command output, output-truncation state, and output-derived
detail because those values are volatile. Executor internals and human-facing
operational diagnostics may still use them.

`mutation-testing-elements-v2.schema.json` describes the Mutation Testing
Elements 2.0-compatible export written by `reporigor --format
mutation-json`. It is a deliberately narrower interchange boundary: it groups
mutants by source file, embeds the source text required by Mutation Testing
Elements consumers, uses one-based locations, and reports durations in
milliseconds. Locations are derived from validated UTF-8 byte spans against
the embedded source; starts are inclusive, ends are exclusive, and columns
count Unicode scalar values rather than UTF-8 bytes, UTF-16 code units, or
display cells.

Each mutation threshold is constrained to the inclusive range 0 through 100.
The additional invariant `low <= high` is enforced by the reporting runtime;
standard Draft 2020-12 JSON Schema cannot compare sibling numeric properties.
`mutatorName` is an open, non-empty provider identifier, so adding a mutator
does not require a structural schema-version change.

Optional Rust values that use `skip_serializing_if` are absent rather than
`null`. The one exception is native mutation `exit_code`, which is always
present and may be `null`. Native mutation candidates include their validated
UTF-8 `start_byte` and `end_byte` edit span so a typed native-report round trip
cannot silently replace it with a default range. Mutation Testing Elements
uses source locations instead; token offsets remain internal.

The native mutation score is `killed / scoreable_mutants * 100`, where
scoreable statuses are exactly `killed` and `survived`. `no-coverage`, timeout,
compile/runtime error, invalid, ignored, and pending mutants are excluded from
the denominator. `scoreable_mutants` is serialized explicitly with the score.
The integrated `mutation.score` rule stores the corresponding fraction in
`[0, 1]` and compares it with `mutation.minimum_score`; the legacy mutation
summary remains a percentage.
This correction and the optional `rules`/stable-identity fields remain schema
version 1: they repair the documented native metric, remove volatile fields
from newly serialized native reports, and add forward-compatible metadata.
The permissive v1 schema still accepts older documents carrying those fields.
New top-level
summary counters and stable mutation/DRY fields are therefore optional in the
schema so previously emitted v1 documents remain structurally acceptable.

Both schemas use JSON Schema Draft 2020-12 and permit unknown object fields so
minor releases can add metadata without breaking forward-compatible readers.
A breaking semantic or structural change requires a new schema version and a
new schema file.
