. "$script_dir/lib/release-signing.sh"
. "$script_dir/lib/release-build.sh"

parse_unsigned_release() {
  rr_require_test "usage: scripts/release-local [--unsigned]" \
    1 "$1" = --unsigned
  signed_release=false
}

parse_release_options() {
  rr_require_args "$#" -le 1 \
    "usage: scripts/release-local [--unsigned]"
  signed_release=true
  if [ "$#" -eq 1 ]; then
    parse_unsigned_release "$1"
  fi
}

verify_release_commands() {
  for release_command in docker rustup shasum tar; do
    rr_require_command "$release_command" \
      "release-local: required command was not found: $release_command"
  done
}

verify_release_platform() {
  release_system=$(uname -s)
  release_machine=$(uname -m)
  rr_require_test "release-local: run this release builder on Apple Silicon macOS" \
    1 "$release_system" = Darwin
  rr_require_test "release-local: run this release builder on Apple Silicon macOS" \
    1 "$release_machine" = arm64
}

verify_release_docker() {
  if ! docker info >/dev/null 2>&1; then
    rr_fail "release-local: Docker is not running"
  fi
}

verify_release_tree_policy() {
  if [ "${REPORIGOR_ALLOW_DIRTY:-}" != 1 ]; then
    release_status=$(git -C "$workspace" status --short)
    rr_require_test "release-local: the Git working tree must be clean" \
      1 -z "$release_status"
  fi
}

verify_release_environment() {
  verify_release_commands
  verify_release_platform
  verify_release_docker
  verify_release_tree_policy
}

read_release_version() {
  version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' "$workspace/Cargo.toml")
  rr_require_test "release-local: could not read the workspace version" \
    1 -n "$version"
}

resolve_release_directory() {
  default_release_dir=$workspace/target/dist/v$version
  release_dir=${REPORIGOR_RELEASE_DIR:-$default_release_dir}
  if [ "${release_dir#/}" = "$release_dir" ]; then
    release_dir=$workspace/$release_dir
  fi
}

initialize_release_directories() {
  rr_require_test "release-local: output already exists: $release_dir" \
    1 ! -e "$release_dir"
  mkdir -p "$workspace/target"
  release_tmp=$(mktemp -d "$workspace/target/reporigor-local-release.XXXXXX")
  asset_dir=$release_tmp/assets
  build_dir=$workspace/target/reporigor-release-build
  cache_dir=$workspace/target/reporigor-release-cache
  mkdir -p "$asset_dir" "$build_dir" "$cache_dir"
  trap cleanup_release_workspace EXIT HUP INT TERM
}

prepare_release_workspace() {
  read_release_version
  resolve_release_directory
  initialize_release_directories
}

cleanup_release_workspace() {
  rr_remove_tree "$release_tmp"
}

build_release_matrix() {
  build_macos aarch64-apple-darwin
  build_macos x86_64-apple-darwin
  build_linux aarch64-unknown-linux-musl linux/arm64 "$alpine_arm64_image"
  build_linux x86_64-unknown-linux-musl linux/amd64 "$alpine_amd64_image"
  build_linux aarch64-unknown-linux-gnu linux/arm64 "$debian_arm64_image"
  build_linux x86_64-unknown-linux-gnu linux/amd64 "$debian_amd64_image"
}

write_release_build_info() {
  commit=$(git -C "$workspace" rev-parse HEAD)
  {
    echo "RepoRigor v$version"
    echo "Git commit: $commit"
    echo "Rust toolchain: $toolchain"
    echo "macOS builder: $(sw_vers -productVersion) $(uname -m)"
    echo "Linux ARM64 musl image: $alpine_arm64_image"
    echo "Linux x86-64 musl image: $alpine_amd64_image"
    echo "Linux ARM64 GNU image: $debian_arm64_image"
    echo "Linux x86-64 GNU image: $debian_amd64_image"
    echo "Apple signing team: $apple_team_name ($apple_team_id)"
    echo "Apple signed and notarized: $signed_release"
  } > "$asset_dir/BUILDINFO.txt"
}

write_release_manifests() {
  cp "$workspace/install.sh" "$asset_dir/install.sh"
  rr_write_sha256_manifest \
    "$asset_dir/install.sh" "$asset_dir/install.sh.sha256" install.sh
  cat "$asset_dir"/*.sha256 | LC_ALL=C sort -k 2 > "$asset_dir/SHA256SUMS"
}

finalize_release_assets() {
  write_release_manifests
  write_release_build_info
  mkdir -p "$(dirname -- "$release_dir")"
  mv "$asset_dir" "$release_dir"
  echo "release-local: RepoRigor v$version assets are ready in $release_dir"
}

initialize_release() {
  rr_set_workspace "$0"
  toolchain=1.95.0
  alpine_arm64_image='rust@sha256:594694ee6b07747b63b5c265be2616b62e814180b66227e2c18c6ee85e4136be'
  alpine_amd64_image='rust@sha256:e98196986adced5602f6e21c54babdbf2a8700400c7a78868324a3630e0c5d15'
  debian_arm64_image='rust@sha256:8e45ae5b178fa788bbbd818b42a1f93a6e2c03e7144badd5e0a37087537177e1'
  debian_amd64_image='rust@sha256:4c2fd73ef19c5ef9d54bee03b06b2839a392604fbfcd578ed948b71b37c1d7fb'
  notary_profile=${REPORIGOR_NOTARY_PROFILE:-reporigor-notary}
  apple_team_id=N43S8JF6JT
  apple_team_name=Picktek
  apple_identity=
  release_cargo_args='--release --locked --package reporigor --bin reporigor'
}

release_local_main() {
  initialize_release
  parse_release_options "$@"
  verify_release_environment
  prepare_release_workspace
  prepare_signing
  build_release_matrix
  finalize_release_assets
}
