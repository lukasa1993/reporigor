select_rustup_cargo() {
  if command -v rustup >/dev/null 2>&1; then
    harness_cargo_mode=rustup
    harness_rustup_bin=$(command -v rustup)
    harness_toolchain=$("$harness_rustup_bin" show active-toolchain | awk '{print $1}')
  fi
}

select_path_cargo() {
  if command -v cargo >/dev/null 2>&1; then
    harness_cargo_mode=direct
    harness_cargo_bin=cargo
  fi
}

select_configured_cargo() {
  if [ -n "${CARGO:-}" ]; then
    harness_cargo_mode=direct
    harness_cargo_bin=$CARGO
  fi
}

select_harness_cargo() {
  harness_cargo_mode=
  select_rustup_cargo
  select_path_cargo
  select_configured_cargo
  rr_require_test \
    "corpus-harness: cargo was not found; set CARGO to its executable path" \
    1 -n "$harness_cargo_mode"
}

run_harness_cargo() {
  if [ "$harness_cargo_mode" = rustup ]; then
    "$harness_rustup_bin" run "$harness_toolchain" cargo "$@"
  else
    "$harness_cargo_bin" "$@"
  fi
}

classify_harness_operation() {
  harness_action=$(awk -v operation="$harness_operation" 'BEGIN {
    if (operation ~ /^(validate|verify|populate)$/) print "direct"
    if (operation ~ /^(run|update)$/) print "build"
    if (operation ~ /^(help|-h|--help)$/) print "help"
  }')
  if [ -z "$harness_action" ]; then
    rr_fail "corpus-harness: unknown operation: $harness_operation
usage: scripts/corpus-harness <validate|verify|populate|run|update> [options]" 2
  fi
}

select_harness_build() {
  if [ "$harness_action" = build ]; then
    harness_build_first=true
  fi
}

normalize_harness_help() {
  if [ "$harness_action" = help ]; then
    harness_operation=help
  fi
}

build_harness_tools() {
  if [ "$harness_build_first" = true ]; then
    run_harness_cargo build --quiet --locked --manifest-path "$workspace/Cargo.toml" \
      -p reporigor --bin reporigor -p corpus-harness
  fi
}

dispatch_harness() {
  if [ "$#" -eq 0 ]; then
    set -- help
  fi
  harness_operation=$1
  shift
  harness_build_first=false
  classify_harness_operation
  select_harness_build
  normalize_harness_help
  build_harness_tools
  run_harness_cargo run --quiet --locked --manifest-path "$workspace/Cargo.toml" \
    -p corpus-harness -- "$harness_operation" "$@"
}

corpus_harness_main() {
  rr_set_workspace "$0"
  select_harness_cargo
  dispatch_harness "$@"
}
