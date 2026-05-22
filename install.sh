#!/usr/bin/env bash
# autocoder bootstrap installer.
#
# Downloads a pre-built autocoder binary, verifies its SHA-256, places it on
# PATH, then execs `autocoder install` which runs the (tested-in-Rust) wizard.
# Operator-facing prompts, useradd / systemctl / apt-get, config generation
# all live in the Rust subcommand. Keep THIS file small enough to read in one
# minute.
set -euo pipefail

OWNER="IndustriousKraken"
REPO="openspec-autocoder"
STEP="init"
trap 'echo "install.sh failed during step: ${STEP}" >&2' ERR

VERSION="${AUTOCODER_VERSION:-}"
USER_INSTALL=0
PASSTHRU=()
while (( $# )); do
  case "$1" in
    --version) VERSION="$2"; shift 2;;
    --user) USER_INSTALL=1; shift;;
    --) shift; PASSTHRU+=("$@"); break;;
    *) PASSTHRU+=("$1"); shift;;
  esac
done

detect_target_triple() {
  local os arch
  os="$(uname -s)"; arch="$(uname -m)"
  [[ "$arch" == "arm64" ]] && arch="aarch64"
  case "${os}/${arch}" in
    Linux/x86_64) echo "x86_64-unknown-linux-gnu";;
    Linux/aarch64) echo "aarch64-unknown-linux-gnu";;
    Darwin/aarch64) echo "aarch64-apple-darwin";;
    *) echo "no pre-built binary for ${os}/${arch}; build from source per README" >&2; exit 1;;
  esac
}

STEP="detect"; TRIPLE="$(detect_target_triple)"

STEP="version"
if [[ -z "$VERSION" ]]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/${OWNER}/${REPO}/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
  [[ -n "$VERSION" ]] || { echo "could not resolve latest release tag" >&2; exit 1; }
fi

STEP="download"
TMP="$(mktemp -d)"
BASENAME="autocoder-${VERSION}-${TRIPLE}"
URL="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/${BASENAME}"
curl -fsSL -o "${TMP}/${BASENAME}" "${URL}"
curl -fsSL -o "${TMP}/${BASENAME}.sha256" "${URL}.sha256"

STEP="verify"
if command -v sha256sum >/dev/null 2>&1; then SUMCHECK="sha256sum -c"; else SUMCHECK="shasum -a 256 -c"; fi
if ! ( cd "${TMP}" && ${SUMCHECK} "${BASENAME}.sha256" ); then
  echo "checksum verification failed; tempdir preserved at ${TMP}" >&2
  exit 1
fi

STEP="install"
SUDO=""; if [[ $EUID -ne 0 ]] && command -v sudo >/dev/null 2>&1; then SUDO="sudo"; fi
if (( USER_INSTALL )) || { [[ $EUID -ne 0 ]] && [[ -z "$SUDO" ]]; }; then
  DEST="${HOME}/.local/bin/autocoder"
  mkdir -p "$(dirname "${DEST}")"
  install -m 755 "${TMP}/${BASENAME}" "${DEST}"
else
  DEST="/usr/local/bin/autocoder"
  ${SUDO} install -m 755 "${TMP}/${BASENAME}" "${DEST}"
fi

STEP="handoff"
exec "${DEST}" install "${PASSTHRU[@]}"
