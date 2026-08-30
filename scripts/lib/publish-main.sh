. "$script_dir/lib/publish-inputs.sh"
. "$script_dir/lib/publish-apple.sh"

cleanup_publish_verification() {
  rr_remove_tree "$verify_tmp"
}

prepare_publish_release() {
  rr_require_args "$#" -le 2 \
    "usage: scripts/publish-local-release [tag] [asset-dir]"
  rr_set_workspace "$0"
  apple_team_id=N43S8JF6JT
  apple_team_name=Picktek
  version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' "$workspace/Cargo.toml")
  tag=${1:-v$version}
  asset_dir=${2:-$workspace/target/dist/$tag}
}

prepare_publish_verification() {
  verify_tmp=$(mktemp -d "$workspace/target/reporigor-publish-verify.XXXXXX")
  trap cleanup_publish_verification EXIT HUP INT TERM
}

publish_verified_release() {
  ensure_release_tag
  git -C "$workspace" push origin "$tag"
  gh release create "$tag" "$asset_dir"/* \
    --repo lukasa1993/reporigor \
    --verify-tag \
    --generate-notes \
    --title "RepoRigor $tag"
  echo "publish-local-release: https://github.com/lukasa1993/reporigor/releases/tag/$tag"
}

publish_local_release_main() {
  prepare_publish_release "$@"
  verify_publish_context
  verify_release_archives
  prepare_publish_verification
  verify_installer_manifest
  verify_apple_artifacts
  publish_verified_release
}
