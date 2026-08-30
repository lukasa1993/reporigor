# RepoRigor agent prompt

> RepoRigor: https://github.com/lukasa1993/reporigor
>
> If `reporigor` is missing on macOS/Linux, install the prebuilt binary—no
> toolchain needed: `curl --proto '=https' --tlsv1.2 -fL
> https://github.com/lukasa1993/reporigor/releases/latest/download/install.sh -o
> /tmp/reporigor-install.sh && sh /tmp/reporigor-install.sh`.
>
> Use RepoRigor as the final gate for every code change. In the RepoRigor
> repository, run `scripts/dogfood`. Elsewhere, give a trusted project-aware
> `reporigor check` a real coverage report, enable mutation execution, and supply
> the complete test command; project execution requires `--allow-project-exec`.
> Exit 0 requires every integrated rule to pass and `results.rules.omitted` to be
> empty. Fix every finding, omitted check, parse error, and diagnostic. Never
> weaken thresholds, hide files, or rewrite baselines just to pass. In your
> handoff, report the exact command, backend, and result.
