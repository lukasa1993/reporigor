verify_dogfood_tools() {
  rr_require_command "$cargo_bin" \
    "dogfood: cargo was not found; set CARGO to its executable path"
  rr_require_command jq \
    "dogfood: jq is required to validate the integrated JSON report"
  llvm_cov_version=$("$cargo_bin" llvm-cov --version 2>/dev/null || true)
  rr_require_test \
    "dogfood: cargo-llvm-cov 0.9.0 is required (found: ${llvm_cov_version:-unavailable})" \
    1 "$llvm_cov_version" = "cargo-llvm-cov 0.9.0"
}

build_default_reporigor() {
  if [ -n "$reporigor_bin" ]; then
    return
  fi
  "$cargo_bin" build --quiet --locked --manifest-path "$workspace/Cargo.toml" \
    --package reporigor --bin reporigor
  reporigor_bin=$workspace/target/debug/reporigor
}

prepare_dogfood() {
  rr_require_args "$#" -le 1 \
    "usage: scripts/dogfood [REPORIGOR_BINARY]"
  verify_dogfood_tools
  reporigor_bin=${1:-${REPORIGOR_BIN:-}}
  build_default_reporigor
  rr_require_executable "$reporigor_bin" \
    "dogfood: RepoRigor executable not found at $reporigor_bin"
}

clean_mutation_target() {
  if [ ! -d "$1" ]; then
    return
  fi
  "$cargo_bin" clean --quiet --manifest-path "$workspace/Cargo.toml" \
    --target-dir "$1"
  rr_rmdir_if_empty "$1"
}

clean_mutation_children() {
  for isolated_target in "$mutation_target_dir"/*; do
    clean_mutation_target "$isolated_target"
  done
}

clean_mutation_targets() {
  if [ ! -d "$mutation_target_dir" ]; then
    return
  fi
  clean_mutation_children
  rmdir "$mutation_target_dir"
}

clean_mutation_targets_quietly() {
  clean_mutation_targets >/dev/null 2>&1 || true
}

cleanup() {
  rr_remove_optional_file "$temporary_report"
  rr_remove_optional_file "$temporary_coverage"
  clean_mutation_targets_quietly
  rr_remove_optional_tree "$mutation_state_parent"
}

assert_report() {
  report=$1
  if ! jq -e '
    .schema_version == 1 and
    .tool.name == "reporigor" and
    .command == "check" and
    .summary.files > 0 and
    .summary.rule_results > 0 and
    .summary.rule_failures == 0 and
    .summary.findings == 0 and
    .summary.crap_over_limit == 0 and
    .summary.duplicate_groups == 0 and
    .summary.omitted_checks == 0 and
    .summary.mutation_errors == 0 and
    .summary.parse_errors == 0 and
    .summary.baseline_existing == 0 and
    .summary.baseline_new == 0 and
    .summary.baseline_worsened == 0 and
    .results.crap.summary.missing_coverage == 0 and
    .results.crap.coverage.total_functions == .results.crap.coverage.matched_functions and
    .results.crap.coverage.unmatched_functions == 0 and
    .results.crap.coverage.empty_ranges == 0 and
    .results.crap.coverage.ambiguous_functions == 0 and
    .results.dry.summary.groups == 0 and
    .results.mutate.summary.total > 0 and
    .results.mutate.summary.killed == 6 and
    .results.mutate.summary.survived == 0 and
    .results.mutate.summary.scoreable_mutants == 6 and
    .results.mutate.summary.ignored == (.results.mutate.summary.total - 6) and
    .results.mutate.summary.no_coverage == 0 and
    .results.mutate.summary.compile_error == 0 and
    .results.mutate.summary.runtime_error == 0 and
    .results.mutate.summary.timeout == 0 and
    .results.mutate.summary.invalid == 0 and
    .results.mutate.summary.pending == 0 and
    .results.mutate.summary.mutation_score == 100.0 and
    any(.results.rules.results[]; .rule_id == "crap.maximum") and
    any(.results.rules.results[]; .rule_id == "mutation.score") and
    .results.rules.omitted == [] and
    .results.rules.baseline.enabled == false and
    .results.rules.baseline.gate_passed == true and
    (.results.rules.results as $rules |
      ([$rules[].violation_id] | length) == ([$rules[].violation_id] | unique | length) and
      all($rules[];
        (.violation_id | test("^[0-9a-f]{64}$")) and
        (.file | startswith("/") | not) and
        (.file | test("(^|/)\\.\\.(/|$)") | not) and
        (.file | contains("\\\\") | not)) and
      ([$rules[] | [.rule_id, .file, .stable_symbol, .violation_id]] ==
        ([$rules[] | [.rule_id, .file, .stable_symbol, .violation_id]] | sort)))
  ' "$report" >/dev/null; then
    rr_fail "dogfood: integrated report failed structural self-consistency assertions"
  fi
}

collect_dogfood_coverage() {
  temporary_coverage=$(mktemp "$artifact_dir/reporigor-coverage.XXXXXX")
  coverage_options='--workspace --all-targets --locked --quiet --json --no-default-ignore-filename-regex'
  CARGO_TARGET_DIR=$artifact_dir/coverage-build \
    "$cargo_bin" llvm-cov $coverage_options \
      --output-path "$temporary_coverage" \
      -- --test-threads=1
  mv -f -- "$temporary_coverage" "$artifact_dir/coverage.json"
  temporary_coverage=
}

prepare_dogfood_mutations() {
  clean_mutation_targets
  mutation_state_parent=$(mktemp -d "${TMPDIR:-/tmp}/reporigor-dogfood-state.XXXXXX")
  export REPORIGOR_DOGFOOD_CARGO=$cargo_bin
  export REPORIGOR_DOGFOOD_MUTATION_TARGET=$mutation_target_dir
  export REPORIGOR_DOGFOOD_NESTED_STATE=$mutation_state_parent
  mutation_test_command='CARGO_TARGET_DIR="$REPORIGOR_DOGFOOD_MUTATION_TARGET/${REPORIGOR_MUTANT_ID:-baseline}" REPORIGOR_MUTATION_STATE_DIR="$REPORIGOR_DOGFOOD_NESTED_STATE" "$REPORIGOR_DOGFOOD_CARGO" test --workspace --all-targets --locked --quiet -- --test-threads=1'
}

run_dogfood_check() {
  temporary_report=$(mktemp "$artifact_dir/reporigor-check.XXXXXX")
  check_options='--allow-project-exec --backend auto --language rust,bash --filter crates/ --filter scripts/ --include-tests --format json'
  "$reporigor_bin" $check_options \
    check --coverage "$artifact_dir/coverage.json" --run-mutations \
    --test-command "$mutation_test_command" "$workspace" \
    > "$temporary_report"
  assert_report "$temporary_report"
  mv -f -- "$temporary_report" "$artifact_dir/check.json"
  temporary_report=
}

dogfood_main() {
  rr_set_workspace "$0"
  artifact_dir=${REPORIGOR_DOGFOOD_DIR:-"$workspace/target/dogfood"}
  mkdir -p "$artifact_dir"
  cargo_bin=${CARGO:-cargo}
  temporary_report=
  temporary_coverage=
  mutation_state_parent=
  mutation_target_dir=$artifact_dir/mutation-build
  prepare_dogfood "$@"
  trap cleanup EXIT HUP INT TERM
  collect_dogfood_coverage
  prepare_dogfood_mutations
  run_dogfood_check
  echo "dogfood: the integrated RepoRigor check accepted its Rust and shell sources with every rule evaluated"
  echo "dogfood: reports are in $artifact_dir"
}
