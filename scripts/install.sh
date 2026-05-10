#!/usr/bin/env bash
# Symora bootstrap installer.
#
# Downloads the official prebuilt binary, verifies the SHA-256, and places
# it on disk. Everything else (skill install, language-server deps,
# updates, removal) is owned by the binary itself:
#
#   symora setup            # interactive: skill + dependencies
#   symora setup skill      # skill only
#   symora setup deps ...   # dependencies only
#   symora self update      # in-place upgrade
#   symora self uninstall   # remove every trace
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/junyeong-ai/symora/main/scripts/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/junyeong-ai/symora/main/scripts/install.sh \
#     | bash -s -- --version 0.7.0 --verify-attestations
#
# Run with --help for the full flag inventory.

set -Eeuo pipefail

readonly REPO="junyeong-ai/symora"
readonly BINARY_NAME="symora"
readonly RELEASES_URL="https://github.com/${REPO}/releases"
readonly API_LATEST_URL="https://api.github.com/repos/${REPO}/releases/latest"

INSTALL_DIR="${SYMORA_INSTALL_DIR:-${INSTALL_DIR:-$HOME/.local/bin}}"
VERSION="${SYMORA_VERSION:-}"
INSTALL_METHOD=""
VERIFY_ATTESTATIONS=false
NO_COLOR="${NO_COLOR:-}"

DAEMON_DIR="$HOME/.symora"

SCRIPT_PATH="${BASH_SOURCE[0]:-$0}"
ORIGINAL_DIR="$(pwd)"
SCRIPT_DIR=""
PROJECT_ROOT=""
TMP_ROOT=""

# ─── logging ────────────────────────────────────────────────────────────────

if [ -t 2 ] && [ -z "${NO_COLOR}" ]; then
    readonly C_RED=$'\033[0;31m'
    readonly C_GREEN=$'\033[0;32m'
    readonly C_YELLOW=$'\033[1;33m'
    readonly C_BLUE=$'\033[0;34m'
    readonly C_BOLD=$'\033[1m'
    readonly C_DIM=$'\033[2m'
    readonly C_OFF=$'\033[0m'
else
    readonly C_RED="" C_GREEN="" C_YELLOW="" C_BLUE="" C_BOLD="" C_DIM="" C_OFF=""
fi

log()       { printf '%s\n' "$*" >&2; }
log_info()  { printf '%s%s%s\n' "$C_BLUE" "$*" "$C_OFF" >&2; }
log_ok()    { printf '%s%s%s\n' "$C_GREEN" "$*" "$C_OFF" >&2; }
log_warn()  { printf '%s%s%s\n' "$C_YELLOW" "$*" "$C_OFF" >&2; }
log_err()   { printf '%s%s%s\n' "$C_RED" "$*" "$C_OFF" >&2; }
log_dim()   { printf '%s%s%s\n' "$C_DIM" "$*" "$C_OFF" >&2; }
log_die()   { log_err "$*"; exit 1; }

log_section() {
    local title="$1"
    local bar="━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    printf '\n%s%s%s\n' "$C_BOLD" "$bar" "$C_OFF" >&2
    printf '%s  %s%s\n' "$C_BOLD" "$title" "$C_OFF" >&2
    printf '%s%s%s\n\n' "$C_BOLD" "$bar" "$C_OFF" >&2
}

# ─── traps ──────────────────────────────────────────────────────────────────

cleanup() {
    local code=$?
    if [ -n "$TMP_ROOT" ] && [ -d "$TMP_ROOT" ]; then
        rm -rf "$TMP_ROOT"
    fi
    exit "$code"
}

on_error() {
    log_err "✗ install.sh failed at line $2 (exit $1)"
}

trap cleanup EXIT
trap 'on_error $? $LINENO' ERR

# ─── help ───────────────────────────────────────────────────────────────────

usage() {
    cat <<EOF
Symora bootstrap installer

Usage:
  install.sh [OPTIONS]

Options:
      --version <ver>          Install a specific release (e.g. 0.7.0). Default: latest.
      --install-dir <path>     Where to place the binary. Default: \$HOME/.local/bin.
      --prebuilt               Force download of a prebuilt binary.
      --source                 Force a build from source (requires a checkout + Rust).
      --verify-attestations    Verify GitHub build provenance with 'gh' (must be installed).
      --no-color               Disable ANSI color in output.
  -h, --help                   Show this help.

After install, run:
  symora setup                 Interactive: install Claude Code skill and language servers
  symora self update           Upgrade in place
  symora self uninstall        Remove the binary, skill, config, and daemon data

Environment overrides (flags win):
  SYMORA_VERSION         Same as --version.
  SYMORA_INSTALL_DIR     Same as --install-dir.
  NO_COLOR               Disable ANSI color.

Examples:
  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/scripts/install.sh | bash
  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/scripts/install.sh \\
      | bash -s -- --version 0.7.0 --verify-attestations
EOF
}

# ─── argument parsing ───────────────────────────────────────────────────────

parse_args() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --version)              VERSION="${2:?--version requires a value}"; VERSION="${VERSION#v}"; shift 2 ;;
            --version=*)            VERSION="${1#*=}"; VERSION="${VERSION#v}"; shift ;;
            --install-dir)          INSTALL_DIR="${2:?--install-dir requires a value}"; shift 2 ;;
            --install-dir=*)        INSTALL_DIR="${1#*=}"; shift ;;
            --prebuilt)             INSTALL_METHOD="prebuilt"; shift ;;
            --source)               INSTALL_METHOD="source"; shift ;;
            --verify-attestations)  VERIFY_ATTESTATIONS=true; shift ;;
            --no-color)             NO_COLOR=1; shift ;;
            -h|--help)              usage; exit 0 ;;
            *)                      log_die "Unknown argument: $1 (use --help)" ;;
        esac
    done

    if [ "$VERSION" = "latest" ]; then
        VERSION=""
    fi
}

# ─── path resolution ────────────────────────────────────────────────────────

resolve_paths() {
    if SCRIPT_DIR="$(cd "$(dirname "$SCRIPT_PATH")" 2>/dev/null && pwd -P)"; then
        :
    else
        SCRIPT_DIR="$ORIGINAL_DIR"
    fi
    if [ -f "$SCRIPT_DIR/../Cargo.toml" ]; then
        PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
    fi
}

setup_tmp() {
    TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/symora-install.XXXXXX")"
}

# ─── helpers ────────────────────────────────────────────────────────────────

display_path() {
    local path="$1"
    if [ "$path" = "$HOME" ]; then
        printf '%s\n' "\$HOME"
    elif [[ "$path" == "$HOME/"* ]]; then
        printf '%s\n' "\$HOME/${path#"$HOME"/}"
    else
        printf '%s\n' "$path"
    fi
}

have_cmd() { command -v "$1" >/dev/null 2>&1; }

# ─── platform detection ─────────────────────────────────────────────────────

detect_target() {
    local os arch
    os="$(uname -s | tr '[:upper:]' '[:lower:]')"
    arch="$(uname -m)"

    case "$arch" in
        x86_64|amd64)         arch="x86_64" ;;
        arm64|aarch64)        arch="aarch64" ;;
        *) log_die "Unsupported CPU architecture: $arch" ;;
    esac

    case "$os" in
        darwin)
            if [ "$arch" != "aarch64" ]; then
                log_die "Prebuilt binaries are published for Apple Silicon only.
Intel Macs: rerun with --source inside a symora checkout, or build with 'cargo install --path .'"
            fi
            printf '%s\n' "aarch64-apple-darwin"
            ;;
        linux)
            printf '%s-unknown-linux-gnu\n' "$arch"
            ;;
        *) log_die "Unsupported OS: $os" ;;
    esac
}

# ─── network ────────────────────────────────────────────────────────────────

http_get() {
    curl --fail --silent --show-error --location \
         --retry 3 --retry-delay 2 --retry-connrefused \
         "$@"
}

is_valid_version() {
    [[ "$1" =~ ^[0-9]+(\.[0-9]+){0,2}([-+][0-9A-Za-z.+-]*)?$ ]]
}

resolve_latest_version() {
    local effective tag

    effective="$(curl --fail --silent --location --head \
                      --output /dev/null \
                      --write-out '%{url_effective}' \
                      "${RELEASES_URL}/latest" 2>/dev/null || true)"
    case "$effective" in
        */releases/tag/v*)
            tag="${effective##*/releases/tag/v}"
            tag="${tag%%[/?#]*}"
            if [ -n "$tag" ]; then
                printf '%s\n' "$tag"
                return 0
            fi
            ;;
    esac

    http_get "${API_LATEST_URL}" 2>/dev/null \
        | sed -nE 's/.*"tag_name": *"v([^"]+)".*/\1/p' \
        | head -n 1
}

# ─── binary install ─────────────────────────────────────────────────────────

download_archive() {
    local version="$1" target="$2"
    local archive url out

    archive="${BINARY_NAME}-v${version}-${target}.tar.gz"
    url="${RELEASES_URL}/download/v${version}/${archive}"
    out="$TMP_ROOT/$archive"

    log_info "↓ downloading ${archive}"
    http_get -o "$out" "$url" || log_die "Download failed: $url"

    log_info "↓ downloading ${archive}.sha256"
    http_get -o "${out}.sha256" "${url}.sha256" \
        || log_die "Checksum download failed (refusing to install without verification)"

    printf '%s\n' "$out"
}

verify_checksum() {
    local archive="$1"
    local dir base

    dir="$(dirname "$archive")"
    base="$(basename "$archive")"

    log_info "🔐 verifying SHA-256"
    if have_cmd sha256sum; then
        ( cd "$dir" && sha256sum -c "${base}.sha256" >/dev/null ) \
            || log_die "Checksum verification failed for $base"
    elif have_cmd shasum; then
        ( cd "$dir" && shasum -a 256 -c "${base}.sha256" >/dev/null ) \
            || log_die "Checksum verification failed for $base"
    else
        log_die "Neither sha256sum nor shasum available — cannot verify download"
    fi
    log_ok "  checksum OK"
}

verify_attestation() {
    local archive="$1"
    if ! have_cmd gh; then
        log_die "--verify-attestations requires the 'gh' CLI (https://cli.github.com)"
    fi
    log_info "🔐 verifying GitHub build provenance"
    gh attestation verify "$archive" --repo "$REPO" >&2 \
        || log_die "Attestation verification failed for $(basename "$archive")"
    log_ok "  attestation OK"
}

extract_archive() {
    local archive="$1"
    local extract_dir="$TMP_ROOT/extract"
    mkdir -p "$extract_dir"
    log_info "📦 extracting"
    tar -xzf "$archive" -C "$extract_dir"
    local bin="$extract_dir/$BINARY_NAME"
    [ -x "$bin" ] || log_die "Archive did not contain executable $BINARY_NAME"
    printf '%s\n' "$bin"
}

build_from_source() {
    if [ -z "$PROJECT_ROOT" ] || [ ! -f "$PROJECT_ROOT/Cargo.toml" ]; then
        log_die "Source build requires running this script from inside a symora checkout."
    fi
    have_cmd cargo || log_die "Source build requires Rust + cargo (https://rustup.rs)"

    log_info "🔨 building from source ($PROJECT_ROOT)"
    ( cd "$PROJECT_ROOT" && cargo build --release ) >&2
    printf '%s\n' "$PROJECT_ROOT/target/release/$BINARY_NAME"
}

stop_running_daemon() {
    local existing="$INSTALL_DIR/$BINARY_NAME"
    if [ -x "$existing" ] && [ -f "$DAEMON_DIR/daemon.pid" ]; then
        log_info "↺ stopping running daemon before binary swap"
        "$existing" daemon stop >/dev/null 2>&1 || true
    fi
}

install_binary() {
    local src="$1"
    stop_running_daemon
    mkdir -p "$INSTALL_DIR"
    install -m 0755 "$src" "$INSTALL_DIR/$BINARY_NAME"

    if [[ "$(uname -s)" == "Darwin" ]]; then
        codesign --force --deep --sign - "$INSTALL_DIR/$BINARY_NAME" 2>/dev/null || true
    fi

    log_ok "✓ installed $(display_path "$INSTALL_DIR")/$BINARY_NAME"
}

select_install_method() {
    local method="$INSTALL_METHOD"
    if [ -n "$method" ]; then
        printf '%s\n' "$method"
        return 0
    fi

    if ! have_cmd curl; then
        if [ -n "$PROJECT_ROOT" ]; then
            log_warn "curl unavailable — falling back to source build"
            printf 'source\n'
        else
            log_die "curl is required for prebuilt downloads (or run from a checkout with --source)"
        fi
        return 0
    fi

    printf 'prebuilt\n'
}

# ─── post-install summary ───────────────────────────────────────────────────

print_post_install() {
    log_section "Installation Complete"

    case ":$PATH:" in
        *":$INSTALL_DIR:"*)
            log_ok "✓ $(display_path "$INSTALL_DIR") is on \$PATH" ;;
        *)
            log_warn "$(display_path "$INSTALL_DIR") is not on \$PATH"
            log ""
            log "  Append this line to your shell profile (~/.zshrc, ~/.bashrc):"
            log_dim "    export PATH=\"$(display_path "$INSTALL_DIR"):\$PATH\""
            ;;
    esac

    log ""
    log "Installed version:"
    "$INSTALL_DIR/$BINARY_NAME" --version >&2 || true

    log ""
    log "Next:"
    log "  ${BINARY_NAME} setup            # install Claude Code skill + LSP servers"
    log "  ${BINARY_NAME} init             # initialize this project"
    log "  ${BINARY_NAME} self update      # upgrade in place"
    log "  ${BINARY_NAME} self uninstall   # remove every trace"
    log ""
}

# ─── main ───────────────────────────────────────────────────────────────────

main() {
    parse_args "$@"
    resolve_paths
    setup_tmp

    local target
    target="$(detect_target)"
    INSTALL_METHOD="$(select_install_method)"

    case "$INSTALL_METHOD" in
        prebuilt)
            if [ -z "$VERSION" ]; then
                VERSION="$(resolve_latest_version || true)"
            fi
            [ -n "$VERSION" ] || log_die "Could not resolve latest version. Pass --version vX.Y.Z."
            is_valid_version "$VERSION" || log_die "Invalid version: $VERSION"

            log_section "Symora · v$VERSION · $target"
            log "  prefix: $(display_path "$INSTALL_DIR")"
            log ""

            local archive bin
            archive="$(download_archive "$VERSION" "$target")"
            verify_checksum "$archive"
            if [ "$VERIFY_ATTESTATIONS" = true ]; then
                verify_attestation "$archive"
            fi
            bin="$(extract_archive "$archive")"
            install_binary "$bin"
            ;;
        source)
            log_section "Symora · source build"
            log "  prefix: $(display_path "$INSTALL_DIR")"
            log ""
            local bin
            bin="$(build_from_source)"
            install_binary "$bin"
            ;;
    esac

    print_post_install
}

main "$@"
