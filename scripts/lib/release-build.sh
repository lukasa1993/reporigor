build_macos_binary() {
  rustup target add --toolchain "$toolchain" "$target"
  CARGO_TARGET_DIR=$build_dir/macos \
    rustup run "$toolchain" cargo build $release_cargo_args --target "$target"
  binary=$build_dir/macos/$target/release/reporigor
}

verify_signed_macos_binary() {
  if [ "$signed_release" = true ]; then
    codesign --verify --strict --verbose=2 "$binary"
  fi
}

build_macos() {
  target=$1
  build_macos_binary
  sign_and_notarize "$binary" "$target"
  rr_package_and_smoke_target "$binary" "$target" "$asset_dir"
  verify_signed_macos_binary
}

run_release_container() {
  docker run --rm \
    --platform "$platform" \
    --volume "$workspace:/work:ro" \
    "$@"
}

prepare_linux_build_directories() {
  mkdir -p "$target_build_dir" "$target_cache_dir"
}

build_linux_binary() {
  run_release_container \
    --volume "$target_build_dir:/target" \
    --volume "$target_cache_dir:/usr/local/cargo/registry" \
    --workdir /work \
    --env CARGO_TARGET_DIR=/target \
    "$image" \
    sh -eu -c '
      target=$1
      host=$(rustc -vV | sed -n "s/^host: //p")
      if [ "$host" != "$target" ]; then
        echo "container host $host does not match release target $target" >&2
        exit 1
      fi
      cargo build $2 --target "$target"
    ' sh "$target" "$release_cargo_args"
  binary=$target_build_dir/$target/release/reporigor
}

smoke_linux_release() {
  run_linux_smoke_container \
    scripts/smoke-release-archive "/dist/reporigor-$target.tar.gz" "$target"
}

run_linux_smoke_container() {
  run_release_container --volume "$asset_dir:/dist:ro" --workdir /work "$image" "$@"
}

build_linux() {
  target=$1
  platform=$2
  image=$3
  target_build_dir=$build_dir/$target
  target_cache_dir=$cache_dir/$target
  prepare_linux_build_directories
  build_linux_binary
  "$workspace/scripts/package-release-archive" "$binary" "$target" "$asset_dir"
  smoke_linux_release
}
