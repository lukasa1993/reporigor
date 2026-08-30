#!/bin/sh
set -eu

repository=https://github.com/lukasa1993/reporigor
install_dir=${REPORIGOR_INSTALL_DIR:-${HOME:?HOME is required}/.local/bin}

if [ "$#" -gt 1 ]; then
  echo "usage: install.sh [version]" >&2
  exit 2
fi
requested_version=${1:-${REPORIGOR_VERSION:-latest}}
if [ "$requested_version" = latest ]; then
  default_release_base=$repository/releases/latest/download
else
  case $requested_version in
    v*)
      release_tag=$requested_version
      ;;
    *)
      release_tag=v$requested_version
      ;;
  esac
  default_release_base=$repository/releases/download/$release_tag
fi
release_base=${REPORIGOR_RELEASE_BASE_URL:-$default_release_base}

case $(uname -s) in
  Darwin)
    platform=apple-darwin
    ;;
  Linux)
    platform=unknown-linux-musl
    ;;
  *)
    echo "install: only macOS and Linux are supported by this installer" >&2
    exit 1
    ;;
esac

case $(uname -m) in
  arm64 | aarch64)
    architecture=aarch64
    ;;
  x86_64 | amd64)
    architecture=x86_64
    ;;
  *)
    echo "install: unsupported CPU architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

target=$architecture-$platform
name=reporigor-$target
archive=$name.tar.gz
checksum=$archive.sha256
install_tmp=$(mktemp -d "${TMPDIR:-/tmp}/reporigor-install.XXXXXX")

cleanup() {
  rm -rf -- "$install_tmp"
}
trap cleanup EXIT HUP INT TERM

fetch() {
  source_url=$1
  destination=$2
  case $source_url in
    file://*)
      cp "${source_url#file://}" "$destination"
      return
      ;;
    https://*)
      ;;
    *)
      echo "install: download URL must use HTTPS" >&2
      exit 1
      ;;
  esac
  if command -v curl >/dev/null 2>&1; then
    curl --fail --location --proto '=https' --silent --show-error \
      --tlsv1.2 --output "$destination" "$source_url"
  elif command -v wget >/dev/null 2>&1; then
    wget --quiet --output-document="$destination" "$source_url"
  else
    echo "install: curl or wget is required" >&2
    exit 1
  fi
}

fetch "$release_base/$archive" "$install_tmp/$archive"
fetch "$release_base/$checksum" "$install_tmp/$checksum"

expected=$(awk 'NR == 1 { print $1 }' "$install_tmp/$checksum")
if ! printf '%s\n' "$expected" | grep -Eq '^[0-9A-Fa-f]{64}$'; then
  echo "install: invalid SHA-256 checksum file" >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$install_tmp/$archive" | awk '{ print $1 }')
else
  actual=$(shasum -a 256 "$install_tmp/$archive" | awk '{ print $1 }')
fi
if [ "$actual" != "$expected" ]; then
  echo "install: archive checksum does not match" >&2
  exit 1
fi

tar -C "$install_tmp" -xzf "$install_tmp/$archive"
stage=$install_tmp/$name
if [ ! -x "$stage/reporigor" ]; then
  echo "install: archive does not contain the expected executable" >&2
  exit 1
fi

mkdir -p "$install_dir"
install -m 0755 "$stage/reporigor" "$install_dir/reporigor"
for alias in \
  crap4bash crap4c crap4cpp crap4objc crap4python crap4rust crap4swift crap4ts \
  dry4bash dry4c dry4cpp dry4objc dry4python dry4rust dry4swift dry4ts \
  mutate4bash mutate4c mutate4cpp mutate4objc mutate4python mutate4rust mutate4swift mutate4ts
do
  ln -sf reporigor "$install_dir/$alias"
done

"$install_dir/reporigor" --version
echo "install: installed RepoRigor and 24 compatibility commands in $install_dir"
case :$PATH: in
  *:$install_dir:*)
    ;;
  *)
    echo "install: add $install_dir to PATH"
    ;;
esac
