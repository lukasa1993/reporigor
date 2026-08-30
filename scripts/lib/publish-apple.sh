verify_notarization_evidence() {
  notary_result=$asset_dir/reporigor-$target.notarization.json
  rr_require_test \
    "publish-local-release: accepted notarization evidence is missing for $target" \
    1 -f "$notary_result"
  rr_require_accepted_notarization "$notary_result" \
    "publish-local-release: accepted notarization evidence is missing for $target"
}

extract_apple_artifact() {
  tar -C "$verify_tmp" -xzf "$asset_dir/reporigor-$target.tar.gz"
  binary=$verify_tmp/reporigor-$target/reporigor
  codesign --verify --strict --verbose=2 "$binary"
}

verify_apple_authority() {
  if ! codesign --display --verbose=4 "$binary" 2>&1 |
    grep -Fq 'Authority=Developer ID Application:'; then
    rr_fail "publish-local-release: $target lacks a Developer ID Application signature"
  fi
}

verify_apple_team() {
  if ! codesign --display --verbose=4 "$binary" 2>&1 |
    grep -Fqx "TeamIdentifier=$apple_team_id"; then
    rr_fail "publish-local-release: $target was not signed by $apple_team_name ($apple_team_id)"
  fi
}

verify_apple_artifact() {
  target=$1
  verify_notarization_evidence
  extract_apple_artifact
  verify_apple_authority
  verify_apple_team
}

verify_apple_artifacts() {
  for apple_target in aarch64-apple-darwin x86_64-apple-darwin; do
    verify_apple_artifact "$apple_target"
  done
}

verify_existing_release_tag() {
  tagged_commit=$(git -C "$workspace" rev-list -n 1 "$tag")
  current_commit=$(git -C "$workspace" rev-parse HEAD)
  rr_require_test \
    "publish-local-release: existing tag $tag points at another commit" \
    1 "$tagged_commit" = "$current_commit"
}

create_release_tag() {
  git -C "$workspace" tag --annotate "$tag" --message "RepoRigor $tag"
}

ensure_release_tag() {
  if git -C "$workspace" rev-parse "$tag" >/dev/null 2>&1; then
    verify_existing_release_tag
  else
    create_release_tag
  fi
}
