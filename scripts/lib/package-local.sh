prepare_local_toolchain() {
  cargo_bin=${CARGO:-cargo}
  rustc_bin=${RUSTC:-rustc}
  rr_require_command "$cargo_bin" \
    "package-local: cargo was not found; set CARGO to its executable"
  rr_require_command "$rustc_bin" \
    "package-local: rustc was not found; set RUSTC to its executable"
}

build_local_reporigor() {
  "$cargo_bin" build --release --locked --manifest-path "$workspace/Cargo.toml" \
    --package reporigor --bin reporigor
  binary=$workspace/target/release/reporigor
  rr_require_executable "$binary" \
    "package-local: release binary was not created at $binary"
}

read_local_target() {
  host_target=$("$rustc_bin" -vV | sed -n 's/^host: //p')
  rr_require_test "package-local: rustc did not report a host target" \
    1 -n "$host_target"
}

read_local_version() {
  set -- $("$binary" --version)
  version=${2:-}
  rr_require_test "package-local: could not read the RepoRigor version" \
    1 -n "$version"
}

package_local_target() {
  dist_dir=${REPORIGOR_DIST_DIR:-"$workspace/target/dist"}
  mkdir -p "$dist_dir"
  rr_package_and_smoke_target "$binary" "$host_target" "$dist_dir"
}

announce_local_package() {
  name=reporigor-$host_target
  echo "package-local: built RepoRigor $version for $host_target"
  echo "package-local: $dist_dir/$name.tar.gz"
  echo "package-local: $dist_dir/$name.tar.gz.sha256"
}

package_local_main() {
  rr_set_workspace "$0"
  prepare_local_toolchain
  build_local_reporigor
  read_local_target
  read_local_version
  package_local_target
  announce_local_package
}
