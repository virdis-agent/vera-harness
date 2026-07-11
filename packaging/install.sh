#!/bin/sh
set -eu

# Public install command:
# curl -fsSL https://raw.githubusercontent.com/virdis-agent/vera-harness/main/packaging/install.sh | sh

VERSION="${VERA_VERSION:-0.1.0-alpha.2}"
REPO="${VERA_REPO:-virdis-agent/vera-harness}"
API_URL="https://api.github.com/repos/${REPO}"
OS="$(uname -s)"
ARCH="$(uname -m)"

if [ "$OS" != "Darwin" ] || [ "$ARCH" != "arm64" ]; then
  echo "vera: this release supports Apple Silicon macOS only" >&2
  exit 1
fi

github_get() {
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    curl --fail --location --retry 3 --silent --show-error \
      -H "Authorization: Bearer ${GITHUB_TOKEN}" "$@"
  else
    curl --fail --location --retry 3 --silent --show-error "$@"
  fi
}

asset_id() {
  target="$1"
  awk -v target="$target" '
    /"id":[[:space:]]*[0-9]+/ {
      id = $0
      sub(/^.*"id":[[:space:]]*/, "", id)
      sub(/[^0-9].*$/, "", id)
    }
    /"name":[[:space:]]*"/ {
      name = $0
      sub(/^.*"name":[[:space:]]*"/, "", name)
      sub(/".*$/, "", name)
      if (name == target) {
        print id
        exit
      }
    }
  '
}

BIN_DIR="${VERA_BIN_DIR:-$HOME/.local/bin}"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/vera-install.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

ARCHIVE="vera-${VERSION}-aarch64-apple-darwin.tar.gz"
RELEASE_JSON="$(github_get "${API_URL}/releases/tags/v${VERSION}")"
ARCHIVE_ID="$(asset_id "$ARCHIVE" <<EOF
${RELEASE_JSON}
EOF
)"
CHECKSUM_ID="$(asset_id SHA256SUMS <<EOF
${RELEASE_JSON}
EOF
)"

if [ -z "$ARCHIVE_ID" ] || [ -z "$CHECKSUM_ID" ]; then
  echo "vera: release v${VERSION} is missing the arm64 archive or SHA256SUMS" >&2
  exit 1
fi

github_get "${API_URL}/releases/assets/${ARCHIVE_ID}" \
  -H 'Accept: application/octet-stream' \
  -o "${TMP_DIR}/${ARCHIVE}"
github_get "${API_URL}/releases/assets/${CHECKSUM_ID}" \
  -H 'Accept: application/octet-stream' \
  -o "${TMP_DIR}/SHA256SUMS"
(cd "$TMP_DIR" && grep "${ARCHIVE}$" SHA256SUMS | shasum -a 256 -c -)

mkdir -p "$BIN_DIR"
tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "$TMP_DIR"
install -m 0755 "${TMP_DIR}/vera" "${BIN_DIR}/vera"
mkdir -p "$HOME/.vera"
chmod 700 "$HOME/.vera"
printf '%s\n' "${VERSION}" > "$HOME/.vera/installer-version"
echo "Installed vera ${VERSION} to ${BIN_DIR}/vera"
