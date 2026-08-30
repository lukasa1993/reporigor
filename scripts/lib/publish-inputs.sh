fetch_publish_head() {
  git -C "$workspace" fetch --quiet origin main
  local_head=$(git -C "$workspace" rev-parse HEAD)
  remote_head=$(git -C "$workspace" rev-parse origin/main)
  rr_require_test \
    "publish-local-release: HEAD must exactly match origin/main" \
    1 "$local_head" = "$remote_head"
}

verify_publish_build_info() {
  rr_require_test \
    "publish-local-release: asset directory was not found: $asset_dir" \
    1 -d "$asset_dir"
  rr_require_line "$asset_dir/BUILDINFO.txt" 'Apple signed and notarized: true' \
    "publish-local-release: refusing to publish unsigned macOS artifacts"
  rr_require_line "$asset_dir/BUILDINFO.txt" \
    "Apple signing team: $apple_team_name ($apple_team_id)" \
    "publish-local-release: assets were not signed for $apple_team_name ($apple_team_id)"
  head_commit=$(git -C "$workspace" rev-parse HEAD)
  rr_require_line "$asset_dir/BUILDINFO.txt" "Git commit: $head_commit" \
    "publish-local-release: assets were not built from the current commit"
}

verify_publish_context() {
  rr_require_test \
    "publish-local-release: tag $tag does not match package version v$version" \
    1 "$tag" = "v$version"
  publish_status=$(git -C "$workspace" status --short)
  rr_require_test \
    "publish-local-release: the Git working tree must be clean" \
    1 -z "$publish_status"
  fetch_publish_head
  verify_publish_build_info
}

verify_release_archive() {
  archive=$asset_dir/reporigor-$1.tar.gz
  checksum=$archive.sha256
  rr_require_files "publish-local-release: release asset is missing for $1" \
    "$archive" "$checksum"
  rr_verify_sha256 "$archive" "$checksum" \
    "publish-local-release: checksum mismatch for $1"
}

verify_release_archives() {
  for target in \
    aarch64-apple-darwin \
    x86_64-apple-darwin \
    aarch64-unknown-linux-musl \
    x86_64-unknown-linux-musl \
    aarch64-unknown-linux-gnu \
    x86_64-unknown-linux-gnu
  do
    verify_release_archive "$target"
  done
}

verify_installer_checksum() {
  rr_verify_sha256 "$asset_dir/install.sh" "$asset_dir/install.sh.sha256" \
    "publish-local-release: installer checksum mismatch"
}

verify_aggregate_manifest() {
  manifest=$verify_tmp/SHA256SUMS
  cat "$asset_dir"/*.sha256 | LC_ALL=C sort -k 2 > "$manifest"
  if ! cmp -s "$manifest" "$asset_dir/SHA256SUMS"; then
    rr_fail "publish-local-release: aggregate checksum manifest is stale"
  fi
}

verify_installer_manifest() {
  verify_installer_checksum
  verify_aggregate_manifest
}
