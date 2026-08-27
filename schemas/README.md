# Machine-readable report schemas

`reporigor-report-v1.schema.json` describes the lossless native envelope
written by `reporigor --format json`. Its integer `schema_version` is `1`.
Consumers should use this format when they need diagnostics, backend
provenance, or more than one analysis result in a single document.
CRAP sections retain coverage-application counters, and mutation sections
retain built-in run mode, recovery, and baseline provenance when they came
from the shared executor.

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

Both schemas use JSON Schema Draft 2020-12 and permit unknown object fields so
minor releases can add metadata without breaking forward-compatible readers.
A breaking semantic or structural change requires a new schema version and a
new schema file.
