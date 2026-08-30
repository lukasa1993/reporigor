rr_workspace_for() {
  CDPATH= cd -- "$(dirname -- "$1")/.." && pwd
}

rr_fail() {
  printf '%s\n' "$1" >&2
  exit "${2:-1}"
}

rr_require_test() {
  rr_test_message=$1
  rr_test_status=$2
  shift 2
  if ! test "$@"; then
    rr_fail "$rr_test_message" "$rr_test_status"
  fi
}

rr_require_args() {
  rr_require_test "$4" 2 "$1" "$2" "$3"
}

rr_require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    rr_fail "$2"
  fi
}

rr_require_executable() {
  rr_require_test "$2" 1 -f "$1"
  rr_require_test "$2" 1 -x "$1"
}

rr_require_line() {
  if ! grep -Fqx "$2" "$1"; then
    rr_fail "$3"
  fi
}

rr_require_files() {
  rr_files_message=$1
  shift
  for rr_required_file in "$@"; do
    rr_require_test "$rr_files_message" 1 -f "$rr_required_file"
  done
}

rr_remove_optional_file() {
  if [ -n "$1" ]; then
    rr_remove_file "$1"
  fi
}

rr_remove_file() {
  if [ -f "$1" ]; then
    rm -f -- "$1"
  fi
}

rr_remove_optional_tree() {
  if [ -n "$1" ]; then
    rr_remove_tree "$1"
  fi
}

rr_remove_tree() {
  if [ -d "$1" ]; then
    rm -rf -- "$1"
  fi
}

rr_rmdir_if_empty() {
  rmdir "$1" 2>/dev/null || true
}

rr_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  else
    shasum -a 256 "$1" | awk '{ print $1 }'
  fi
}

rr_verify_sha256() {
  rr_expected_digest=$(awk 'NR == 1 { print $1 }' "$2")
  rr_actual_digest=$(rr_sha256 "$1")
  rr_require_test "$3" 1 "$rr_actual_digest" = "$rr_expected_digest"
}

rr_require_accepted_notarization() {
  rr_notary_status=$(/usr/bin/plutil -extract status raw -o - "$1")
  rr_require_test "$2" 1 "$rr_notary_status" = Accepted
}

rr_write_sha256_manifest() {
  rr_manifest_digest=$(rr_sha256 "$1")
  printf '%s  %s\n' "$rr_manifest_digest" "$3" > "$2"
}

rr_set_workspace() {
  workspace=$(rr_workspace_for "$1")
}

rr_package_and_smoke_target() {
  "$workspace/scripts/package-release-archive" "$1" "$2" "$3"
  rr_release_archive=$3/reporigor-$2.tar.gz
  "$workspace/scripts/smoke-release-archive" "$rr_release_archive" "$2"
}

rr_each_release_language() {
  for rr_language in bash c cpp objc python rust swift ts; do
    "$1" "$2" "$rr_language"
  done
}

rr_each_release_analyzer() {
  "$1" crap
  "$1" dry
  "$1" mutate
}
