#!/usr/bin/env bash
#
# autocoder installer. Detects OS/arch, optionally installs system packages
# and the Claude CLI, downloads a pre-built autocoder binary from GitHub
# Releases, verifies its SHA-256, runs a short config wizard, and (Linux +
# systemd) generates an autocoder.service unit running under a dedicated
# system user. macOS gets a dev-mode-only flow with user-local paths.
#
# Pipe-friendly:
#   curl -fsSL https://raw.githubusercontent.com/IndustriousKraken/openspec-autocoder/main/install.sh | bash
# Or download first if you want to read it before running:
#   curl -fsSLO https://raw.githubusercontent.com/IndustriousKraken/openspec-autocoder/main/install.sh
#   bash install.sh
#
# Strict mode + a trap that names the step we were on so failures are
# actionable instead of mysterious.

set -euo pipefail

# ---------------------------------------------------------------------------
# Constants — touch these if the upstream URLs ever move.
# ---------------------------------------------------------------------------

readonly SCRIPT_NAME="autocoder install.sh"
readonly SCRIPT_VERSION="0.1.0"
readonly REPO_OWNER="IndustriousKraken"
readonly REPO_NAME="openspec-autocoder"
readonly REPO_URL="https://github.com/${REPO_OWNER}/${REPO_NAME}"
readonly REPO_RAW_URL="https://raw.githubusercontent.com/${REPO_OWNER}/${REPO_NAME}"
readonly CLAUDE_INSTALL_URL="https://claude.ai/install.sh"
readonly RECOMMENDED_TAG_COUNT=5
readonly DEFAULT_INSTALL_PREFIX_SERVER="/usr/local/bin"
readonly DEFAULT_CONFIG_DIR_SERVER="/etc/autocoder"
readonly DEFAULT_STATE_DIR_SERVER="/var/lib/autocoder"
readonly DEFAULT_INSTALL_PREFIX_DEV_USER="${HOME:-/root}/.local/bin"
readonly DEFAULT_INSTALL_PREFIX_DEV_SYSTEM="/usr/local/bin"
readonly DEFAULT_CONFIG_DIR_DEV="${HOME:-/root}/.config/autocoder"

# ---------------------------------------------------------------------------
# Mutable state shared across functions. Plain globals — bash doesn't really
# do anything else and the script is a single file.
# ---------------------------------------------------------------------------

LAST_STEP="startup"
LOG_FILE=""
TARGET_OS=""
TARGET_ARCH=""
TARGET_TRIPLE=""
IS_DEBIAN="false"
HAS_SYSTEMD="false"
INSTALL_MODE=""           # "server" | "dev"
INSTALL_PREFIX=""
CONFIG_DIR=""
STATE_DIR=""
BINARY_OWNER=""           # "root" | "" (current user)
BINARY_GROUP=""
CONFIG_OWNER=""           # "root:autocoder" | "" (current user)
CONFIG_MODE=""            # "640" or "600"
SECRETS_MODE="600"
SELECTED_TAG=""
SELECTED_TAG_SOURCE=""    # "menu" | "manual" | "skip"
TMP_WORK_DIR=""
DOWNLOAD_SKIPPED="false"
HAS_JQ="false"

# ---------------------------------------------------------------------------
# Logging + error trap.
# ---------------------------------------------------------------------------

setup_logging() {
    local ts
    ts="$(date -u +%Y%m%dT%H%M%SZ)"
    LOG_FILE="/tmp/autocoder-install-${ts}.log"
    # Tee both stdout and stderr through `tee -a`. Process substitution
    # keeps the parent shell's stdin attached to whatever was driving the
    # script (a TTY, /dev/tty after re-exec, or the curl pipe).
    exec > >(tee -a "$LOG_FILE")
    exec 2> >(tee -a "$LOG_FILE" >&2)
    echo "[log] writing install log to ${LOG_FILE}"
}

on_error() {
    local rc=$?
    echo
    echo "✗ install failed at step: ${LAST_STEP}"
    if [[ -n "${LOG_FILE}" ]]; then
        echo "  see ${LOG_FILE} for the full transcript"
    fi
    if [[ -n "${TMP_WORK_DIR}" && -d "${TMP_WORK_DIR}" ]]; then
        echo "  download workspace preserved at ${TMP_WORK_DIR}"
    fi
    exit "$rc"
}

trap on_error ERR

step() {
    LAST_STEP="$1"
    echo
    echo "── ${LAST_STEP} ──────────────────────────────────────────────"
}

# ---------------------------------------------------------------------------
# Interactive prompts. Read from /dev/tty when stdin isn't a TTY (the
# curl|bash case) so prompts still work.
# ---------------------------------------------------------------------------

_have_tty() {
    [[ -r /dev/tty ]]
}

prompt() {
    # Usage: prompt "Prompt> " VAR_NAME ["default"]
    local message="$1"
    local var="$2"
    local default="${3:-}"
    local ans=""
    if [[ -t 0 ]]; then
        read -r -p "$message" ans || true
    elif _have_tty; then
        read -r -p "$message" ans < /dev/tty || true
    else
        echo "✗ no TTY available; re-run as: bash install.sh (after downloading)" >&2
        exit 1
    fi
    if [[ -z "$ans" && -n "$default" ]]; then
        ans="$default"
    fi
    printf -v "$var" '%s' "$ans"
}

prompt_secret() {
    # Usage: prompt_secret "Prompt> " VAR_NAME
    local message="$1"
    local var="$2"
    local ans=""
    if [[ -t 0 ]]; then
        read -r -s -p "$message" ans || true
        echo
    elif _have_tty; then
        read -r -s -p "$message" ans < /dev/tty || true
        echo
    else
        echo "✗ no TTY available for secret input" >&2
        exit 1
    fi
    printf -v "$var" '%s' "$ans"
}

confirm() {
    # Usage: confirm "Yes? [Y/n]: " "y"   (default y or n)
    local message="$1"
    local default="${2:-y}"
    local ans=""
    prompt "$message" ans "$default"
    case "${ans,,}" in
        y|yes) return 0 ;;
        n|no)  return 1 ;;
        *)
            # Treat anything else as the default.
            [[ "${default,,}" =~ ^y ]]
            ;;
    esac
}

# ---------------------------------------------------------------------------
# Banner.
# ---------------------------------------------------------------------------

print_banner() {
    cat <<EOF
============================================================
  ${SCRIPT_NAME}  v${SCRIPT_VERSION}
  ${REPO_URL}
------------------------------------------------------------
  This script will:
    • detect your OS + architecture
    • optionally install system packages (git, curl, jq, …)
    • optionally install the Claude CLI (the default executor)
    • download a pre-built autocoder binary and verify SHA-256
    • run a short config wizard
    • (Linux+systemd) generate and enable autocoder.service
============================================================
EOF
}

# ---------------------------------------------------------------------------
# Platform detection.
# ---------------------------------------------------------------------------

detect_os() {
    local kernel
    kernel="$(uname -s)"
    case "$kernel" in
        Linux)   echo "linux" ;;
        Darwin)  echo "darwin" ;;
        *)
            echo "✗ unsupported OS: ${kernel}. autocoder ships binaries for Linux and macOS only." >&2
            exit 1
            ;;
    esac
}

detect_arch() {
    local machine
    machine="$(uname -m)"
    case "$machine" in
        x86_64|amd64)        echo "x86_64" ;;
        aarch64|arm64)       echo "aarch64" ;;
        *)
            echo "✗ unsupported architecture: ${machine}. autocoder ships x86_64 and aarch64 binaries only." >&2
            exit 1
            ;;
    esac
}

detect_target_triple() {
    local os="$1" arch="$2"
    case "${os}-${arch}" in
        linux-x86_64)   echo "x86_64-unknown-linux-gnu" ;;
        linux-aarch64)  echo "aarch64-unknown-linux-gnu" ;;
        darwin-aarch64) echo "aarch64-apple-darwin" ;;
        *)
            echo "✗ no pre-built binary for ${os}/${arch}; build from source per README." >&2
            exit 1
            ;;
    esac
}

detect_debian() {
    [[ -f /etc/debian_version ]]
}

detect_systemd() {
    command -v systemctl >/dev/null 2>&1 && [[ -d /run/systemd/system ]]
}

run_platform_detection() {
    step "platform detection"
    TARGET_OS="$(detect_os)"
    TARGET_ARCH="$(detect_arch)"
    TARGET_TRIPLE="$(detect_target_triple "$TARGET_OS" "$TARGET_ARCH")"
    if [[ "$TARGET_OS" == "linux" ]] && detect_debian; then
        IS_DEBIAN="true"
    fi
    if [[ "$TARGET_OS" == "linux" ]] && detect_systemd; then
        HAS_SYSTEMD="true"
    fi
    echo "  os=${TARGET_OS} arch=${TARGET_ARCH} triple=${TARGET_TRIPLE}"
    echo "  debian=${IS_DEBIAN} systemd=${HAS_SYSTEMD}"
}

# ---------------------------------------------------------------------------
# Install-mode selection.
# ---------------------------------------------------------------------------

select_install_mode() {
    step "install mode"

    if [[ "$TARGET_OS" == "darwin" ]]; then
        echo "  macOS detected — server mode is not supported on this platform yet."
        echo "  Continuing in dev mode (binary in your home dir, no systemd)."
        INSTALL_MODE="dev"
        return
    fi

    local default_mode="2"
    local default_label="dev"
    if [[ "$HAS_SYSTEMD" == "true" ]]; then
        default_mode="1"
        default_label="server"
    fi

    echo "  Install mode:"
    echo "    [1] Server  — systemd, dedicated user, system paths (recommended for production)"
    echo "    [2] Local dev — no systemd, user paths"
    local choice=""
    prompt "  Choose [1/2] (default ${default_mode}=${default_label}): " choice "$default_mode"

    case "$choice" in
        1) INSTALL_MODE="server" ;;
        2) INSTALL_MODE="dev" ;;
        *)
            echo "  unrecognised choice '${choice}'; defaulting to ${default_label}"
            INSTALL_MODE="${default_label}"
            ;;
    esac

    if [[ "$INSTALL_MODE" == "server" && "$HAS_SYSTEMD" != "true" ]]; then
        echo "  ! systemd is not running on this host. Server mode requires systemd."
        echo "  ! Falling back to dev mode."
        INSTALL_MODE="dev"
    fi
}

ensure_root_for_server_mode() {
    [[ "$INSTALL_MODE" == "server" ]] || return 0
    [[ $EUID -eq 0 ]] && return 0

    if ! command -v sudo >/dev/null 2>&1; then
        echo "  ! server mode requires root and sudo is not available."
        echo "  ! Falling back to dev mode."
        INSTALL_MODE="dev"
        return 0
    fi

    if [[ -f "${BASH_SOURCE[0]:-}" && -r "${BASH_SOURCE[0]:-}" ]]; then
        echo "  Server mode requires root. Re-executing under sudo..."
        if confirm "  Re-exec under 'sudo bash ${BASH_SOURCE[0]}'? [Y/n]: " "y"; then
            exec sudo -E bash "${BASH_SOURCE[0]}" "$@"
        else
            echo "  ! operator declined sudo re-exec; falling back to dev mode."
            INSTALL_MODE="dev"
            return 0
        fi
    fi

    # We were curl-piped — no on-disk script to re-exec. Tell the operator
    # how to retry and bail rather than silently dropping to dev mode (that
    # would surprise someone who explicitly asked for server mode).
    echo "  ! server mode requires root, but this script was piped via curl|bash"
    echo "  ! so we can't sudo re-exec the pipe input. Either:"
    echo "  !   1) curl -fsSL ${REPO_URL}/raw/main/install.sh -o install.sh"
    echo "  !      sudo bash install.sh"
    echo "  !   2) re-run as: curl -fsSL ${REPO_URL}/raw/main/install.sh | sudo bash"
    exit 1
}

choose_dev_install_prefix() {
    [[ "$INSTALL_MODE" == "dev" ]] || return 0

    if [[ $EUID -eq 0 ]]; then
        INSTALL_PREFIX="$DEFAULT_INSTALL_PREFIX_DEV_SYSTEM"
        CONFIG_DIR="$DEFAULT_CONFIG_DIR_DEV"
        return 0
    fi

    if command -v sudo >/dev/null 2>&1; then
        echo "  Dev mode: where should the autocoder binary live?"
        echo "    [1] ${DEFAULT_INSTALL_PREFIX_DEV_SYSTEM}/  (needs sudo, picked up by every user — recommended)"
        echo "    [2] ${DEFAULT_INSTALL_PREFIX_DEV_USER}/  (no sudo, current user only)"
        local choice=""
        prompt "  Choose [1/2] (default 1): " choice "1"
        case "$choice" in
            2) INSTALL_PREFIX="$DEFAULT_INSTALL_PREFIX_DEV_USER" ;;
            *) INSTALL_PREFIX="$DEFAULT_INSTALL_PREFIX_DEV_SYSTEM" ;;
        esac
    else
        INSTALL_PREFIX="$DEFAULT_INSTALL_PREFIX_DEV_USER"
    fi
    CONFIG_DIR="$DEFAULT_CONFIG_DIR_DEV"
}

finalise_paths() {
    step "install paths"
    if [[ "$INSTALL_MODE" == "server" ]]; then
        INSTALL_PREFIX="$DEFAULT_INSTALL_PREFIX_SERVER"
        CONFIG_DIR="$DEFAULT_CONFIG_DIR_SERVER"
        STATE_DIR="$DEFAULT_STATE_DIR_SERVER"
        BINARY_OWNER="root"
        BINARY_GROUP="root"
        CONFIG_OWNER="root:autocoder"
        CONFIG_MODE="640"
        SECRETS_MODE="600"
    else
        choose_dev_install_prefix
        STATE_DIR=""
        BINARY_OWNER=""
        BINARY_GROUP=""
        CONFIG_OWNER=""
        CONFIG_MODE="600"
        SECRETS_MODE="600"
    fi
    echo "  mode=${INSTALL_MODE}"
    echo "  binary -> ${INSTALL_PREFIX}/autocoder"
    echo "  config -> ${CONFIG_DIR}/config.yaml"
    if [[ -n "$STATE_DIR" ]]; then
        echo "  state  -> ${STATE_DIR}"
    fi
}

# ---------------------------------------------------------------------------
# Helpers for elevated commands.
# ---------------------------------------------------------------------------

sudo_if_needed() {
    if [[ $EUID -eq 0 ]]; then
        "$@"
    else
        sudo "$@"
    fi
}

# ---------------------------------------------------------------------------
# System dependencies.
# ---------------------------------------------------------------------------

install_system_dependencies() {
    step "system dependencies"

    if [[ "$TARGET_OS" == "darwin" ]]; then
        local missing=()
        for cmd in git curl; do
            command -v "$cmd" >/dev/null 2>&1 || missing+=("$cmd")
        done
        if [[ ${#missing[@]} -eq 0 ]]; then
            echo "  git, curl already present — nothing to do."
        else
            echo "  missing on PATH: ${missing[*]}"
            echo "  Install via Homebrew (https://brew.sh) or Xcode Command Line Tools, then re-run this script."
        fi
        return 0
    fi

    if [[ "$IS_DEBIAN" == "true" ]]; then
        if confirm "  Install system dependencies (git, curl, ca-certificates, jq) via apt? [Y/n]: " "y"; then
            sudo_if_needed apt-get update
            sudo_if_needed apt-get install -y git curl ca-certificates jq
        else
            echo "  Skipped. Make sure git, curl, ca-certificates, jq are installed before continuing."
        fi
    else
        echo "  Non-Debian Linux detected. You'll need git, curl, ca-certificates, jq installed via your distribution's package manager."
    fi
}

# ---------------------------------------------------------------------------
# Claude CLI.
# ---------------------------------------------------------------------------

install_claude_cli() {
    step "claude CLI"

    if command -v claude >/dev/null 2>&1; then
        echo "  claude CLI already present at $(command -v claude) — skipping."
        return 0
    fi

    echo "  The Claude CLI is the default executor backend for autocoder."
    echo "  The official installer is fetched from: ${CLAUDE_INSTALL_URL}"
    if confirm "  Install Claude CLI now? [Y/n]: " "y"; then
        if curl -fsSL "$CLAUDE_INSTALL_URL" | bash; then
            echo "  ✓ claude CLI installed. Now run 'claude auth login' to authenticate before starting autocoder."
        else
            echo "  ! claude installer reported a non-zero exit. You can re-run it manually:"
            echo "  !   curl -fsSL ${CLAUDE_INSTALL_URL} | bash"
        fi
    else
        echo "  Skipped. Remember to install + authenticate claude before autocoder can run."
        echo "  See ${CLAUDE_INSTALL_URL}"
    fi
}

# ---------------------------------------------------------------------------
# Version selection.
# ---------------------------------------------------------------------------

ensure_jq() {
    if command -v jq >/dev/null 2>&1; then
        HAS_JQ="true"
        return 0
    fi
    if [[ "$IS_DEBIAN" == "true" ]]; then
        echo "  jq is not installed; installing via apt..."
        if sudo_if_needed apt-get install -y jq; then
            HAS_JQ="true"
            return 0
        fi
    fi
    if [[ "$TARGET_OS" == "darwin" ]] && command -v brew >/dev/null 2>&1; then
        echo "  jq is not installed; installing via Homebrew..."
        if brew install jq; then
            HAS_JQ="true"
            return 0
        fi
    fi
    HAS_JQ="false"
}

fetch_releases_json() {
    # Echoes the raw JSON array on stdout, or returns non-zero on failure.
    curl -fsSL \
        -H "Accept: application/vnd.github+json" \
        "https://api.github.com/repos/${REPO_OWNER}/${REPO_NAME}/releases"
}

select_release_tag() {
    step "version selection"
    ensure_jq

    if [[ "$HAS_JQ" != "true" ]]; then
        echo "  ! jq is required to parse the GitHub releases list reliably."
        echo "  ! Falling back to manual tag entry."
        local manual_tag=""
        prompt "  Enter the tag to install (e.g. v1.2.3): " manual_tag ""
        if [[ -z "$manual_tag" ]]; then
            echo "  no tag supplied; skipping binary download"
            SELECTED_TAG=""
            SELECTED_TAG_SOURCE="skip"
            DOWNLOAD_SKIPPED="true"
            return 0
        fi
        SELECTED_TAG="$manual_tag"
        SELECTED_TAG_SOURCE="manual"
        return 0
    fi

    local releases_json
    if ! releases_json="$(fetch_releases_json)"; then
        echo "  ! failed to fetch release list from GitHub API."
        echo "  ! You may be rate-limited or offline. Falling back to manual tag entry."
        local manual_tag=""
        prompt "  Enter the tag to install (e.g. v1.2.3): " manual_tag ""
        if [[ -z "$manual_tag" ]]; then
            SELECTED_TAG=""
            SELECTED_TAG_SOURCE="skip"
            DOWNLOAD_SKIPPED="true"
            return 0
        fi
        SELECTED_TAG="$manual_tag"
        SELECTED_TAG_SOURCE="manual"
        return 0
    fi

    # Filter to production tags only, sort by published_at desc.
    local production_lines
    production_lines="$(
        echo "$releases_json" \
        | jq -r '.[]
            | select(.tag_name | test("^v[0-9]+\\.[0-9]+\\.[0-9]+$"))
            | "\(.published_at)\t\(.tag_name)"' \
        | sort -r \
        | head -n "$RECOMMENDED_TAG_COUNT" \
        || true
    )"

    if [[ -z "$production_lines" ]]; then
        echo "  No production releases found on GitHub yet."
        echo "    [m] Enter a tag manually (e.g. v0.1.0, v1.2.3-rc1)"
        echo "    [s] Skip — I'll download the binary myself"
        local choice=""
        prompt "  Choose [m/s] (default m): " choice "m"
        case "$choice" in
            s|S)
                SELECTED_TAG=""
                SELECTED_TAG_SOURCE="skip"
                DOWNLOAD_SKIPPED="true"
                ;;
            *)
                local manual_tag=""
                prompt "  Enter the tag: " manual_tag ""
                if [[ -z "$manual_tag" ]]; then
                    SELECTED_TAG=""
                    SELECTED_TAG_SOURCE="skip"
                    DOWNLOAD_SKIPPED="true"
                else
                    SELECTED_TAG="$manual_tag"
                    SELECTED_TAG_SOURCE="manual"
                fi
                ;;
        esac
        return 0
    fi

    # Build parallel arrays of tag + published date for the menu.
    local -a tags=() dates=()
    while IFS=$'\t' read -r published tag; do
        tags+=("$tag")
        dates+=("${published%%T*}")
    done <<< "$production_lines"

    echo "  Available production releases:"
    local i=1
    for idx in "${!tags[@]}"; do
        local label=""
        if [[ $i -eq 1 ]]; then
            label=" — recommended"
        fi
        printf "    [%d] %s  (released %s)%s\n" "$i" "${tags[$idx]}" "${dates[$idx]}" "$label"
        i=$((i + 1))
    done
    echo "    [m] Enter a specific tag manually (use this for pre-release versions like v1.3.0-rc1)"
    echo "    [s] Skip — I'll download the binary myself"

    local choice=""
    prompt "  Choose [1-${#tags[@]}/m/s] (default 1): " choice "1"
    case "$choice" in
        m|M)
            local manual_tag=""
            prompt "  Enter the tag: " manual_tag ""
            if [[ -z "$manual_tag" ]]; then
                echo "  no tag supplied; skipping binary download"
                SELECTED_TAG=""
                SELECTED_TAG_SOURCE="skip"
                DOWNLOAD_SKIPPED="true"
            elif ! curl -fsI "${REPO_URL}/releases/tag/${manual_tag}" >/dev/null 2>&1; then
                echo "  ! tag '${manual_tag}' could not be verified at ${REPO_URL}/releases/tag/${manual_tag}"
                echo "  ! Proceeding anyway — the download step will print the exact URL it tries."
                SELECTED_TAG="$manual_tag"
                SELECTED_TAG_SOURCE="manual"
            else
                SELECTED_TAG="$manual_tag"
                SELECTED_TAG_SOURCE="manual"
            fi
            ;;
        s|S)
            SELECTED_TAG=""
            SELECTED_TAG_SOURCE="skip"
            DOWNLOAD_SKIPPED="true"
            ;;
        *)
            local n="${choice:-1}"
            if ! [[ "$n" =~ ^[0-9]+$ ]] || (( n < 1 || n > ${#tags[@]} )); then
                echo "  unrecognised choice '${choice}'; defaulting to 1"
                n=1
            fi
            SELECTED_TAG="${tags[$((n - 1))]}"
            SELECTED_TAG_SOURCE="menu"
            ;;
    esac

    if [[ "$DOWNLOAD_SKIPPED" == "true" ]]; then
        echo "  Binary download will be skipped. Place a verified binary at ${INSTALL_PREFIX}/autocoder yourself."
    else
        echo "  selected tag: ${SELECTED_TAG}"
    fi
}

# ---------------------------------------------------------------------------
# Binary download + verification.
# ---------------------------------------------------------------------------

download_and_verify_binary() {
    [[ "$DOWNLOAD_SKIPPED" == "true" ]] && return 0
    step "binary download"

    local asset_name="autocoder-${SELECTED_TAG}-${TARGET_TRIPLE}"
    local asset_url="${REPO_URL}/releases/download/${SELECTED_TAG}/${asset_name}"
    local sha_url="${asset_url}.sha256"

    TMP_WORK_DIR="$(mktemp -d -t autocoder-install.XXXXXX)"
    echo "  workspace: ${TMP_WORK_DIR}"
    echo "  binary URL: ${asset_url}"
    echo "  sha256 URL: ${sha_url}"

    if ! curl -fsSL --output "${TMP_WORK_DIR}/${asset_name}" "$asset_url"; then
        echo "✗ failed to download binary from ${asset_url}"
        echo "  check that the tag '${SELECTED_TAG}' has a release asset for triple '${TARGET_TRIPLE}'."
        exit 1
    fi
    if ! curl -fsSL --output "${TMP_WORK_DIR}/${asset_name}.sha256" "$sha_url"; then
        echo "✗ failed to download checksum from ${sha_url}"
        exit 1
    fi

    step "checksum verification"
    local verify_cmd
    if [[ "$TARGET_OS" == "darwin" ]]; then
        verify_cmd="shasum -a 256 -c"
    else
        verify_cmd="sha256sum -c"
    fi

    if (cd "$TMP_WORK_DIR" && $verify_cmd "${asset_name}.sha256"); then
        echo "  ✓ checksum verified"
    else
        echo "✗ checksum mismatch for ${asset_name}"
        local expected computed
        expected="$(awk '{print $1}' "${TMP_WORK_DIR}/${asset_name}.sha256" 2>/dev/null || true)"
        if [[ "$TARGET_OS" == "darwin" ]]; then
            computed="$(shasum -a 256 "${TMP_WORK_DIR}/${asset_name}" | awk '{print $1}' 2>/dev/null || true)"
        else
            computed="$(sha256sum "${TMP_WORK_DIR}/${asset_name}" | awk '{print $1}' 2>/dev/null || true)"
        fi
        echo "  expected: ${expected}"
        echo "  computed: ${computed}"
        echo "  download URL: ${asset_url}"
        echo "  temp dir preserved at ${TMP_WORK_DIR} for forensics"
        exit 1
    fi

    step "binary install"
    if [[ "$INSTALL_MODE" == "server" ]]; then
        sudo_if_needed install -m 755 -o "$BINARY_OWNER" -g "$BINARY_GROUP" \
            "${TMP_WORK_DIR}/${asset_name}" "${INSTALL_PREFIX}/autocoder"
    else
        if [[ "$INSTALL_PREFIX" == "$DEFAULT_INSTALL_PREFIX_DEV_SYSTEM" ]]; then
            sudo_if_needed install -m 755 "${TMP_WORK_DIR}/${asset_name}" "${INSTALL_PREFIX}/autocoder"
        else
            mkdir -p "$INSTALL_PREFIX"
            install -m 755 "${TMP_WORK_DIR}/${asset_name}" "${INSTALL_PREFIX}/autocoder"
        fi
    fi
    echo "  ✓ installed ${INSTALL_PREFIX}/autocoder"

    # Clean up the temp dir on success — keep it only when verification
    # fails (handled above before exit).
    rm -rf "$TMP_WORK_DIR"
    TMP_WORK_DIR=""
}

# ---------------------------------------------------------------------------
# Server-mode user creation + state dirs.
# ---------------------------------------------------------------------------

server_user_and_state_dirs() {
    [[ "$INSTALL_MODE" == "server" ]] || return 0
    step "system user + state dirs"

    if id autocoder >/dev/null 2>&1; then
        echo "  user 'autocoder' already exists — skipping useradd"
    else
        sudo_if_needed useradd --system --shell /usr/sbin/nologin \
            --home-dir "$STATE_DIR" --create-home autocoder
        echo "  created system user 'autocoder'"
    fi

    sudo_if_needed install -d -o autocoder -g autocoder -m 0750 "$STATE_DIR"
    sudo_if_needed install -d -o root -g autocoder -m 0750 "$CONFIG_DIR"

    # /tmp/workspaces and /tmp/autocoder are managed by the daemon. We only
    # adjust ownership if they already exist from a prior install so the
    # daemon can keep writing them after the re-install. NOT created here.
    local tmp_path
    for tmp_path in /tmp/workspaces /tmp/autocoder; do
        if [[ -d "$tmp_path" ]]; then
            sudo_if_needed chown -R autocoder:autocoder "$tmp_path" || true
        fi
    done
}

ensure_dev_dirs() {
    [[ "$INSTALL_MODE" == "dev" ]] || return 0
    mkdir -p "$CONFIG_DIR"
    chmod 700 "$CONFIG_DIR" || true
}

# ---------------------------------------------------------------------------
# Config wizard.
# ---------------------------------------------------------------------------

write_root_file() {
    # Usage: write_root_file <dest> <mode> <owner-or-empty> < /dev/stdin
    # Reads content from stdin and writes it via sudo when CONFIG_OWNER is set.
    local dest="$1" mode="$2" owner="$3"
    local tmp
    tmp="$(mktemp)"
    cat > "$tmp"
    if [[ -n "$owner" ]]; then
        sudo_if_needed install -m "$mode" -o "${owner%%:*}" -g "${owner##*:}" "$tmp" "$dest"
    else
        install -m "$mode" "$tmp" "$dest"
    fi
    rm -f "$tmp"
}

download_config_example() {
    local tag="${SELECTED_TAG:-main}"
    local url="${REPO_RAW_URL}/${tag}/config.example.yaml"
    local dest="${CONFIG_DIR}/config.example.yaml"
    local tmp
    tmp="$(mktemp)"

    echo "  fetching config.example.yaml from ${url}"
    if ! curl -fsSL --output "$tmp" "$url"; then
        echo "  ! couldn't fetch from tag '${tag}'; falling back to main"
        url="${REPO_RAW_URL}/main/config.example.yaml"
        if ! curl -fsSL --output "$tmp" "$url"; then
            rm -f "$tmp"
            echo "✗ couldn't fetch config.example.yaml from upstream" >&2
            exit 1
        fi
    fi
    write_root_file "$dest" "0644" "$CONFIG_OWNER" < "$tmp"
    rm -f "$tmp"
}

run_config_wizard() {
    step "config wizard"

    ensure_dev_dirs
    local config_path="${CONFIG_DIR}/config.yaml"
    local secrets_path="${CONFIG_DIR}/secrets.env"

    if sudo_if_needed test -f "$config_path" 2>/dev/null; then
        echo "  existing config detected at ${config_path}; skipping wizard."
        echo "  (upgrade path = binary swap only; config + secrets preserved)"
        return 0
    fi

    download_config_example

    # --------- Repository fields ---------
    local repo_url base_branch agent_branch poll_interval
    while true; do
        prompt "  Repository URL (e.g. git@github.com:owner/repo.git): " repo_url ""
        if [[ -n "$repo_url" ]]; then
            break
        fi
        echo "  ! repository URL is required."
    done
    prompt "  Base branch [main]: " base_branch "main"
    prompt "  Agent branch [agent-q]: " agent_branch "agent-q"
    prompt "  Poll interval seconds [300]: " poll_interval "300"

    # --------- GitHub PAT ---------
    local gh_pat
    while true; do
        prompt_secret "  GitHub Personal Access Token (input hidden): " gh_pat
        if [[ -z "$gh_pat" ]]; then
            echo "  ! token cannot be empty"
            continue
        fi
        if [[ "$gh_pat" != ghp_* && "$gh_pat" != github_pat_* ]]; then
            if confirm "  ! token doesn't start with ghp_ or github_pat_. Use it anyway? [y/N]: " "n"; then
                break
            else
                continue
            fi
        fi
        break
    done

    # --------- ChatOps backend ---------
    echo "  ChatOps backend (where the agent escalates blockers):"
    echo "    [1] none (default)"
    echo "    [2] slack"
    echo "    [3] discord"
    echo "    [4] teams"
    echo "    [5] mattermost"
    echo "    [6] matrix"
    local chatops_choice chatops_provider="" chatops_token="" chatops_channel=""
    prompt "  Choose [1-6] (default 1): " chatops_choice "1"
    case "$chatops_choice" in
        2) chatops_provider="slack" ;;
        3) chatops_provider="discord" ;;
        4) chatops_provider="teams" ;;
        5) chatops_provider="mattermost" ;;
        6) chatops_provider="matrix" ;;
        *) chatops_provider="" ;;
    esac
    if [[ -n "$chatops_provider" ]]; then
        prompt_secret "  ${chatops_provider} bot/access token (input hidden): " chatops_token
        prompt "  Default channel id (provider-native): " chatops_channel ""
    fi

    # --------- Reviewer ---------
    echo "  AI code reviewer (optional — reviews each PR for code quality):"
    echo "    [1] none (default)"
    echo "    [2] Anthropic — claude-sonnet-4-6"
    echo "    [3] OpenAI-compatible"
    local reviewer_choice reviewer_provider="" reviewer_model="" reviewer_key=""
    prompt "  Choose [1-3] (default 1): " reviewer_choice "1"
    case "$reviewer_choice" in
        2)
            reviewer_provider="anthropic"
            reviewer_model="claude-sonnet-4-6"
            ;;
        3)
            reviewer_provider="openai_compatible"
            prompt "  Reviewer model name: " reviewer_model "gpt-4o"
            ;;
        *)
            reviewer_provider=""
            ;;
    esac
    if [[ -n "$reviewer_provider" ]]; then
        prompt_secret "  Reviewer API key (input hidden): " reviewer_key
    fi

    # --------- Build secrets.env ---------
    local secrets_tmp
    secrets_tmp="$(mktemp)"
    {
        echo "# autocoder secrets — managed by install.sh. chmod 600."
        echo "GITHUB_TOKEN=${gh_pat}"
        case "$chatops_provider" in
            slack)      echo "SLACK_BOT_TOKEN=${chatops_token}" ;;
            discord)    echo "DISCORD_BOT_TOKEN=${chatops_token}" ;;
            teams)      echo "TEAMS_CLIENT_SECRET=${chatops_token}" ;;
            mattermost) echo "MATTERMOST_TOKEN=${chatops_token}" ;;
            matrix)     echo "MATRIX_ACCESS_TOKEN=${chatops_token}" ;;
        esac
        case "$reviewer_provider" in
            anthropic)         echo "ANTHROPIC_API_KEY=${reviewer_key}" ;;
            openai_compatible) echo "OPENAI_API_KEY=${reviewer_key}" ;;
        esac
    } > "$secrets_tmp"
    write_root_file "$secrets_path" "$SECRETS_MODE" "$CONFIG_OWNER" < "$secrets_tmp"
    rm -f "$secrets_tmp"

    # --------- Build config.yaml ---------
    # Start from the downloaded example. Patch repo URL + branches + poll
    # interval. Append a real reviewer/chatops block at the end when the
    # operator opted in — the example's commented blocks don't conflict.
    local config_tmp example_tmp
    config_tmp="$(mktemp)"
    example_tmp="$(mktemp)"
    if [[ -n "$CONFIG_OWNER" ]]; then
        sudo_if_needed cat "${CONFIG_DIR}/config.example.yaml" > "$example_tmp"
    else
        cat "${CONFIG_DIR}/config.example.yaml" > "$example_tmp"
    fi

    # Patch the first repository block.
    # The example has a fixed shape; we use line-anchored sed to swap the
    # placeholder fields.
    local sed_escaped_url sed_escaped_base sed_escaped_agent sed_escaped_poll
    sed_escaped_url="$(printf '%s' "$repo_url" | sed -e 's/[\/&|]/\\&/g')"
    sed_escaped_base="$(printf '%s' "$base_branch" | sed -e 's/[\/&|]/\\&/g')"
    sed_escaped_agent="$(printf '%s' "$agent_branch" | sed -e 's/[\/&|]/\\&/g')"
    sed_escaped_poll="$(printf '%s' "$poll_interval" | sed -e 's/[\/&|]/\\&/g')"

    sed -e "s|- url: \"git@github.com:your-org/your-repo.git\"|- url: \"${sed_escaped_url}\"|" \
        -e "s|^    base_branch: main\$|    base_branch: ${sed_escaped_base}|" \
        -e "s|^    agent_branch: agent-q\$|    agent_branch: ${sed_escaped_agent}|" \
        -e "s|^    poll_interval_sec: 300\$|    poll_interval_sec: ${sed_escaped_poll}|" \
        "$example_tmp" > "$config_tmp"

    # Append reviewer block when enabled.
    if [[ "$reviewer_provider" == "anthropic" ]]; then
        cat >> "$config_tmp" <<EOF

# Active reviewer block, written by install.sh.
reviewer:
  enabled: true
  provider: anthropic
  model: ${reviewer_model}
  api_key_env: ANTHROPIC_API_KEY
EOF
    elif [[ "$reviewer_provider" == "openai_compatible" ]]; then
        cat >> "$config_tmp" <<EOF

# Active reviewer block, written by install.sh.
reviewer:
  enabled: true
  provider: openai_compatible
  model: ${reviewer_model}
  api_key_env: OPENAI_API_KEY
EOF
    fi

    # Append chatops block when enabled.
    case "$chatops_provider" in
        slack)
            cat >> "$config_tmp" <<EOF

# Active chatops block, written by install.sh.
chatops:
  provider: slack
  default_channel_id: ${chatops_channel}
  slack:
    bot_token_env: SLACK_BOT_TOKEN
EOF
            ;;
        discord)
            cat >> "$config_tmp" <<EOF

# Active chatops block, written by install.sh.
chatops:
  provider: discord
  default_channel_id: ${chatops_channel}
  discord:
    bot_token_env: DISCORD_BOT_TOKEN
EOF
            ;;
        teams)
            cat >> "$config_tmp" <<EOF

# Active chatops block, written by install.sh.
chatops:
  provider: teams
  default_channel_id: ${chatops_channel}
  teams:
    tenant_id: "REPLACE_ME"
    client_id: "REPLACE_ME"
    client_secret_env: TEAMS_CLIENT_SECRET
    team_id: "REPLACE_ME"
EOF
            ;;
        mattermost)
            cat >> "$config_tmp" <<EOF

# Active chatops block, written by install.sh.
chatops:
  provider: mattermost
  default_channel_id: ${chatops_channel}
  mattermost:
    server_url: "https://mattermost.example.com"
    access_token_env: MATTERMOST_TOKEN
EOF
            ;;
        matrix)
            cat >> "$config_tmp" <<EOF

# Active chatops block, written by install.sh.
chatops:
  provider: matrix
  default_channel_id: ${chatops_channel}
  matrix:
    homeserver_url: "https://matrix.example.com"
    access_token_env: MATRIX_ACCESS_TOKEN
EOF
            ;;
    esac

    write_root_file "$config_path" "$CONFIG_MODE" "$CONFIG_OWNER" < "$config_tmp"
    rm -f "$config_tmp" "$example_tmp"

    echo "  ✓ config:  ${config_path}"
    echo "  ✓ secrets: ${secrets_path} (chmod ${SECRETS_MODE})"
}

# ---------------------------------------------------------------------------
# systemd unit (server mode only).
# ---------------------------------------------------------------------------

install_systemd_unit() {
    [[ "$INSTALL_MODE" == "server" ]] || return 0
    step "systemd unit"

    local unit_path="/etc/systemd/system/autocoder.service"
    local unit_tmp
    unit_tmp="$(mktemp)"
    cat > "$unit_tmp" <<EOF
[Unit]
Description=autocoder — autonomous OpenSpec-driven coding agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=autocoder
Group=autocoder
EnvironmentFile=${CONFIG_DIR}/secrets.env
ExecStart=${INSTALL_PREFIX}/autocoder run --config ${CONFIG_DIR}/config.yaml
Restart=on-failure
RestartSec=10
WorkingDirectory=${STATE_DIR}
StandardOutput=journal
StandardError=journal
# Hardening — restrictive but compatible with the daemon's normal operation.
NoNewPrivileges=true
PrivateTmp=false
ProtectSystem=strict
ReadWritePaths=/tmp ${STATE_DIR} ${CONFIG_DIR}
ProtectHome=true

[Install]
WantedBy=multi-user.target
EOF

    if sudo_if_needed test -f "$unit_path"; then
        # Diff existing unit; if different, ask before overwriting.
        local existing
        existing="$(sudo_if_needed cat "$unit_path")"
        if [[ "$existing" == "$(cat "$unit_tmp")" ]]; then
            echo "  systemd unit already up to date at ${unit_path}"
            rm -f "$unit_tmp"
            sudo_if_needed systemctl daemon-reload
            return 0
        fi
        echo "  existing systemd unit at ${unit_path} differs from the generated one."
        if ! confirm "  Overwrite? [y/N]: " "n"; then
            echo "  Skipping unit overwrite. Existing unit left in place."
            rm -f "$unit_tmp"
            return 0
        fi
    fi

    sudo_if_needed install -m 0644 -o root -g root "$unit_tmp" "$unit_path"
    rm -f "$unit_tmp"
    echo "  wrote ${unit_path}"

    sudo_if_needed systemctl daemon-reload
    if confirm "  Enable and start autocoder.service now? [Y/n]: " "y"; then
        sudo_if_needed systemctl enable --now autocoder.service
        echo "  ✓ service enabled and started."
        echo "  follow logs with: journalctl -u autocoder -f"
    else
        echo "  Skipped. Start later with: sudo systemctl enable --now autocoder.service"
    fi
}

# ---------------------------------------------------------------------------
# Post-install summary.
# ---------------------------------------------------------------------------

print_summary() {
    step "summary"
    echo
    echo "✓ autocoder install complete."
    echo
    echo "  binary:  ${INSTALL_PREFIX}/autocoder"
    echo "  config:  ${CONFIG_DIR}/config.yaml"
    echo "  secrets: ${CONFIG_DIR}/secrets.env   (chmod ${SECRETS_MODE} — do not check into git)"
    if [[ "$INSTALL_MODE" == "server" ]]; then
        echo "  state:   ${STATE_DIR}"
        echo
        echo "  service: sudo systemctl status autocoder"
        echo "  logs:    sudo journalctl -u autocoder -f"
    else
        echo
        echo "  run with:  ${INSTALL_PREFIX}/autocoder run --config ${CONFIG_DIR}/config.yaml"
        echo "  (export secrets first:  set -a; source ${CONFIG_DIR}/secrets.env; set +a)"
    fi
    echo
    echo "  Next steps:"
    if ! command -v claude >/dev/null 2>&1; then
        echo "    • install + authenticate claude:  ${CLAUDE_INSTALL_URL}  then  claude auth login"
    else
        echo "    • run 'claude auth login' if you haven't yet (skip if already authenticated)"
    fi
    echo "    • add more repositories: edit ${CONFIG_DIR}/config.yaml then run 'autocoder reload'"
    echo "    • full docs: ${REPO_URL}#readme"
    echo "    • install log: ${LOG_FILE}"
}

# ---------------------------------------------------------------------------
# Main.
# ---------------------------------------------------------------------------

main() {
    setup_logging
    print_banner

    run_platform_detection
    select_install_mode
    ensure_root_for_server_mode "$@"
    finalise_paths

    install_system_dependencies
    install_claude_cli

    select_release_tag
    download_and_verify_binary

    server_user_and_state_dirs
    run_config_wizard
    install_systemd_unit

    print_summary
}

main "$@"
