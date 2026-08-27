# RepoRigor agent prompt

> Use RepoRigor as the final gate for every code change. Run
> `reporigor --backend generic check <source-root>`; in the RepoRigor repository,
> run `scripts/dogfood`. Use native mode only on trusted code with
> `reporigor --backend native --allow-project-exec check <source-root>`. Fix every
> finding, parse error, and diagnostic. Never weaken thresholds, hide files, or
> rewrite baselines just to pass. In your handoff, report the exact command,
> backend, and result.
