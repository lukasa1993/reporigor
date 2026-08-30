# Local macOS and Linux releases

RepoRigor's public binaries are built, signed, notarized, verified, and
published from the maintainer's Apple Silicon Mac. GitHub Actions has only an
explicitly manual, read-only build-candidate check: version tags do not trigger
it, and it has no signing credentials or release publication permission.

## Consumer install: no build or toolchain

On macOS or Linux, download and run the small release installer:

```sh
curl --proto '=https' --tlsv1.2 -fL \
  https://github.com/lukasa1993/reporigor/releases/latest/download/install.sh \
  -o /tmp/reporigor-install.sh
sh /tmp/reporigor-install.sh
```

The installer detects the OS and CPU, downloads the matching archive and
adjacent checksum over HTTPS, verifies SHA-256 before extraction, and installs
one executable plus all 24 compatibility symlinks under `~/.local/bin`. It uses
the static musl archive on Linux for maximum portability. No Rust, Python,
Node, JVM, or runtime grammar download is needed.

Pin a version or choose another installation directory when reproducibility or
PATH layout requires it:

```sh
REPORIGOR_INSTALL_DIR="$HOME/bin" sh /tmp/reporigor-install.sh 0.1.0
```

## One-time local signing setup

Public macOS command-line tools require a valid `Developer ID Application`
certificate. `Apple Development`, ad-hoc, and Mac App Store distribution
identities are not substitutes for public distribution. Install the Developer
ID certificate and its private key in the login keychain. RepoRigor releases
are locked to the Picktek Apple Developer team (`N43S8JF6JT`); the builder and
publisher both reject a signature from any other team.

Xcode's supported command-line provisioning path is
`xcodebuild -allowProvisioningUpdates`. The Apple account configured in Xcode
must have access to Picktek's cloud-managed Developer ID certificates; without
that team permission Xcode can create development signatures but cannot obtain
the public `Developer ID Application` identity.

Store notarization credentials once in the local Keychain. An App Store Connect
API key is preferred:

```sh
xcrun notarytool store-credentials reporigor-notary \
  --key /secure/path/AuthKey_KEYID.p8 \
  --key-id KEYID \
  --issuer ISSUER_UUID
```

An Apple ID, team ID, and app-specific password also work; omitting `--password`
uses a secure prompt:

```sh
xcrun notarytool store-credentials reporigor-notary \
  --apple-id developer@example.com \
  --team-id N43S8JF6JT
```

Nothing is copied into the repository or GitHub. Set
`REPORIGOR_NOTARY_PROFILE` only if the stored profile has a different name. If
more than one Developer ID identity is installed, set
`REPORIGOR_APPLE_SIGNING_IDENTITY` to the exact identity string.

## Build, sign, notarize, and verify locally

Requirements are Apple Silicon macOS, Xcode command-line tools, Rustup, a
running Docker Desktop, a clean Git worktree, the Developer ID identity, and
the Keychain notarization profile. Then run:

```sh
scripts/release-local
```

The command performs the complete local release build:

1. Cross-builds native Apple Silicon and Intel macOS executables with the pinned
   Rust 1.95.0 toolchain.
2. Signs each Mach-O with a secure timestamp and hardened runtime, verifies the
   signature, submits it to Apple's notary service, and requires `Accepted`.
3. Builds GNU and static musl Linux executables for ARM64 and x86-64 inside four
   architecture-specific Docker images pinned by immutable digest.
4. Packages one multicall executable, 24 compatibility symlinks, schemas,
   configuration, documentation, license, and notices per target.
5. Extracts every archive on the matching native, Rosetta, or Docker runtime
   and runs `--version` through all 25 command names.
6. Writes adjacent checksum files, an aggregate `SHA256SUMS`, notarization JSON,
   and `BUILDINFO.txt` with the exact commit, toolchain, image digests, and
   signing state.

Final assets are written to `target/dist/v<version>/`. The source tree is mounted
read-only in Linux builders, while Cargo registry and build caches stay under
`target/` for faster later releases.

`scripts/release-local --unsigned` exists only to validate builders on a machine
without release credentials. The publisher refuses those artifacts.

## Publish from this Mac

After reviewing all assets, commit and push the exact source revision, then run:

```sh
scripts/publish-local-release
```

The publisher requires a clean checkout whose `HEAD` equals `origin/main`, a
tag exactly matching the Cargo version, all six archives and checksums, signed
Developer ID identities inside both macOS archives, accepted notarization
records, and `BUILDINFO.txt` marked signed. It creates and pushes the annotated
version tag, then creates the GitHub Release and uploads the local assets. It
never invokes GitHub Actions.

## Published targets

| Platform | Rust target | Purpose |
| --- | --- | --- |
| macOS Apple Silicon | `aarch64-apple-darwin` | Native signed/notarized binary |
| macOS Intel | `x86_64-apple-darwin` | Native signed/notarized binary |
| Linux ARM64 static | `aarch64-unknown-linux-musl` | Default ARM64 installer asset |
| Linux x86-64 static | `x86_64-unknown-linux-musl` | Default x86-64 installer asset |
| Linux ARM64 GNU | `aarch64-unknown-linux-gnu` | Native glibc/cargo-binstall asset |
| Linux x86-64 GNU | `x86_64-unknown-linux-gnu` | Native glibc/cargo-binstall asset |

The wider cargo-dist plan still describes Windows and later Homebrew, npm,
shell, and PowerShell channels. They are not part of the first local
macOS/Linux publication.

## Verify a manual download

Keep an archive and its `.sha256` file together. On Linux:

```sh
sha256sum -c reporigor-*.sha256
```

On macOS:

```sh
shasum -a 256 -c reporigor-*.sha256
```

For macOS, an extracted binary must additionally pass:

```sh
codesign --verify --strict --verbose=2 reporigor
codesign --display --verbose=4 reporigor
```

The displayed authority must begin with `Developer ID Application:` and
`TeamIdentifier` must equal `N43S8JF6JT`. An Apple notarization ticket is
recorded by binary hash; bare command-line executables cannot carry a stapled
ticket like an application bundle.
