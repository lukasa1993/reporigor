prepare_release_smoke() {
  rr_require_args "$#" -eq 2 \
    "usage: scripts/smoke-release-archive <archive> <rust-target>"
  archive=$1
  target=$2
  rr_require_test \
    "smoke-release-archive: archive was not found: $archive" \
    1 -f "$archive"
  initialize_release_smoke
}

initialize_release_smoke() {
  smoke_tmp=$(mktemp -d "${TMPDIR:-/tmp}/reporigor-release-smoke.XXXXXX")
  trap cleanup_release_smoke EXIT HUP INT TERM
}

cleanup_release_smoke() {
  rr_remove_tree "$smoke_tmp"
}

extract_release_smoke() {
  name=reporigor-$target
  tar -C "$smoke_tmp" -xzf "$archive"
  rr_require_executable "$smoke_tmp/$name/reporigor" \
    "smoke-release-archive: archive layout is invalid"
}

run_release_alias() {
  "$smoke_tmp/$name/${1}4${2}" --version >/dev/null
}

smoke_release_analyzer() {
  rr_each_release_language run_release_alias "$1"
}

run_release_smoke() {
  "$smoke_tmp/$name/reporigor" --version >/dev/null
  rr_each_release_analyzer smoke_release_analyzer
}

smoke_release_archive_main() {
  prepare_release_smoke "$@"
  extract_release_smoke
  run_release_smoke
  echo "smoke-release-archive: all 25 commands passed for $target"
}
