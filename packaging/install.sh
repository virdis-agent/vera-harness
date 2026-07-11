#!/bin/sh
set -eu

# Public install command:
# curl -fsSL https://raw.githubusercontent.com/virdis-agent/vera-harness/main/packaging/install.sh | sh

VERSION="${VERA_VERSION:-0.1.0-alpha.1}"
REPO="${VERA_REPO:-virdis-agent/vera-harness}"
BASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}"
OS="$(uname -s)"
ARCH="$(uname -m)"

if [ "$OS" != "Darwin" ] || [ "$ARCH" != "arm64" ]; then
  echo "vera: this release supports Apple Silicon macOS only" >&2
  exit 1
fi

BIN_DIR="${VERA_BIN_DIR:-$HOME/.local/bin}"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/vera-install.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

ARCHIVE="vera-${VERSION}-aarch64-apple-darwin.tar.gz"
curl --fail --location --retry 3 --silent --show-error "${BASE_URL}/${ARCHIVE}" -o "${TMP_DIR}/${ARCHIVE}"
curl --fail --location --retry 3 --silent --show-error "${BASE_URL}/SHA256SUMS" -o "${TMP_DIR}/SHA256SUMS"
(cd "$TMP_DIR" && grep "${ARCHIVE}$" SHA256SUMS | shasum -a 256 -c -)

mkdir -p "$BIN_DIR"
tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "$TMP_DIR"
install -m 0755 "${TMP_DIR}/vera" "${BIN_DIR}/vera"
mkdir -p "$HOME/.vera"
chmod 700 "$HOME/.vera"
printf '%s\n' "${VERSION}" > "$HOME/.vera/installer-version"
echo "Installed vera ${VERSION} to ${BIN_DIR}/vera"
