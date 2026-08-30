prepare_release_package() {
  rr_require_args "$#" -eq 3 \
    "usage: scripts/package-release-archive <binary> <rust-target> <output-dir>"
  binary=$1
  target=$2
  output_dir=$3
  rr_require_executable "$binary" \
    "package-release-archive: executable was not found: $binary"
  mkdir -p "$workspace/target" "$output_dir"
  package_tmp=$(mktemp -d "$workspace/target/reporigor-release-package.XXXXXX")
  trap cleanup_release_package EXIT HUP INT TERM
}

cleanup_release_package() {
  rr_remove_tree "$package_tmp"
}

link_release_command() {
  ln -s reporigor "$stage/${1}4${2}"
}

link_release_analyzer() {
  rr_each_release_language link_release_command "$1"
}

stage_release_package() {
  name=reporigor-$target
  stage=$package_tmp/$name
  mkdir -p "$stage"
  cp "$binary" "$stage/reporigor"
  rr_each_release_analyzer link_release_analyzer
}

copy_release_metadata() {
  for metadata_name in README.md AGENT_PROMPT.md LICENSE THIRD_PARTY_NOTICES.md reporigor.example.toml; do
    cp "$workspace/$metadata_name" "$stage/"
  done
  cp -R "$workspace/schemas" "$stage/schemas"
}

create_release_archive() {
  archive=$package_tmp/$name.tar.gz
  if [ "$(uname -s)" = Darwin ]; then
    darwin_tar_options='--no-mac-metadata --no-xattrs'
    env COPYFILE_DISABLE=1 tar $darwin_tar_options \
      -C "$package_tmp" -czf "$archive" "$name"
  else
    tar -C "$package_tmp" -czf "$archive" "$name"
  fi
}

write_release_checksum() {
  checksum=$package_tmp/$name.tar.gz.sha256
  rr_write_sha256_manifest "$archive" "$checksum" "$name.tar.gz"
}

install_release_archive() {
  mv "$archive" "$output_dir/$name.tar.gz"
  mv "$checksum" "$output_dir/$name.tar.gz.sha256"
}

announce_release_archive() {
  echo "package-release-archive: $output_dir/$name.tar.gz"
}

package_release_archive_main() {
  rr_set_workspace "$0"
  prepare_release_package "$@"
  stage_release_package
  copy_release_metadata
  create_release_archive
  write_release_checksum
  install_release_archive
  announce_release_archive
}
