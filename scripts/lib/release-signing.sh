collect_signing_identities() {
  identity_file=$release_tmp/developer-identities.txt
  security find-identity -v -p codesigning |
    awk -F '"' -v team="($apple_team_id)" \
      '/Developer ID Application:/ && index($2, team) { print $2 }' > "$identity_file"
}

use_requested_signing_identity() {
  apple_identity=$REPORIGOR_APPLE_SIGNING_IDENTITY
  if ! grep -Fqx "$apple_identity" "$identity_file"; then
    rr_fail "release-local: requested Picktek Developer ID Application identity is unavailable"
  fi
}

use_unique_signing_identity() {
  identity_count=$(wc -l < "$identity_file" | tr -d ' ')
  rr_require_test \
    "release-local: exactly one Picktek ($apple_team_id) Developer ID Application identity is required; found $identity_count
release-local: Xcode automatic signing requires cloud-managed Developer ID certificate access for this team" \
    1 "$identity_count" = 1
  apple_identity=$(sed -n '1p' "$identity_file")
}

select_signing_identity() {
  if [ -n "${REPORIGOR_APPLE_SIGNING_IDENTITY:-}" ]; then
    use_requested_signing_identity
  else
    use_unique_signing_identity
  fi
}

verify_notary_profile() {
  if ! xcrun notarytool history \
    --keychain-profile "$notary_profile" \
    --output-format json >/dev/null 2>&1; then
    rr_fail "release-local: notary profile '$notary_profile' is unavailable or invalid"
  fi
}

prepare_signing() {
  if [ "$signed_release" != true ]; then
    return
  fi
  collect_signing_identities
  select_signing_identity
  verify_notary_profile
}

sign_release_binary() {
  codesign \
    --force \
    --options runtime \
    --sign "$apple_identity" \
    --timestamp \
    "$binary"
  codesign --verify --strict --verbose=2 "$binary"
}

verify_release_signature_team() {
  signed_team=$(codesign --display --verbose=4 "$binary" 2>&1 |
    sed -n 's/^TeamIdentifier=//p')
  rr_require_test \
    "release-local: $target was signed by team '$signed_team', expected $apple_team_name ($apple_team_id)" \
    1 "$signed_team" = "$apple_team_id"
}

submit_release_notarization() {
  notary_zip=$release_tmp/$target-notarization.zip
  notary_result=$asset_dir/reporigor-$target.notarization.json
  ditto -c -k --keepParent "$binary" "$notary_zip"
  xcrun notarytool submit "$notary_zip" \
    --keychain-profile "$notary_profile" \
    --output-format json \
    --timeout 30m \
    --wait > "$notary_result"
}

verify_release_notarization() {
  rr_require_accepted_notarization "$notary_result" \
    "release-local: notarization was not accepted for $target"
}

sign_and_notarize() {
  binary=$1
  target=$2
  if [ "$signed_release" != true ]; then
    return
  fi
  sign_release_binary
  verify_release_signature_team
  submit_release_notarization
  verify_release_notarization
}
