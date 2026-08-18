#!/usr/bin/env bash
# Symora bootstrap installer.
#
# Gets a verified binary onto disk — prebuilt download (SHA-256 checked,
# optionally provenance-verified) or a source build — then optionally
# installs the Claude Code skill by delegating to the binary itself.
# Everything past bootstrap is owned by the binary:
#
#   symora setup            # interactive: skill + dependencies
#   symora setup skill      # skill only (version-aware, backed up)
#   symora setup deps ...   # dependencies only
#   symora self update      # in-place upgrade
#   symora self uninstall   # remove every trace
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/junyeong-ai/symora/main/scripts/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/junyeong-ai/symora/main/scripts/install.sh \
#     | bash -s -- --source --no-skill
#
# The default run asks nothing: prebuilt binary when one is published for
# this platform (source build otherwise), then the Claude Code skill —
# one command, zero prompts. Every decision has an opt-out flag
# (--source, --no-skill, ...), and --interactive restores the guided
# prompts (read from /dev/tty, so it works even under `curl | bash`).
#
# Run with --help for the full flag inventory.

set -Eeuo pipefail
# errexit does not propagate into $(...) on bash < 4.4 — enable where
# possible, and keep every failure point below explicitly guarded so the
# capture-context functions are safe on macOS bash 3.2 regardless.
shopt -s inherit_errexit 2>/dev/null || true

readonly REPO="junyeong-ai/symora"
readonly REPO_URL="https://github.com/junyeong-ai/symora"
readonly BINARY_NAME="symora"
readonly RELEASES_URL="https://github.com/${REPO}/releases"
readonly API_LATEST_URL="https://api.github.com/repos/${REPO}/releases/latest"

INSTALL_DIR="${SYMORA_INSTALL_DIR:-${INSTALL_DIR:-$HOME/.local/bin}}"
VERSION="${SYMORA_VERSION:-}"
VERSION_REQUESTED="$VERSION"
INSTALL_METHOD=""
SKILL_MODE="auto" # auto | yes | no  (auto: install; ask first under --interactive)
INTERACTIVE=false
VERIFY_ATTESTATIONS=false
NO_COLOR="${NO_COLOR:-}"


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
      --version <ver>          Install a specific release (e.g. 0.9.0). Default: latest.
      --install-dir <path>     Where to place the binary. Default: \$HOME/.local/bin.
      --prebuilt               Download the prebuilt binary (no prompt).
      --source                 Build from source (no prompt). Works inside a checkout
                               or anywhere with Rust — outside a checkout the pinned
                               release is built straight from the git tag.
      --skill                  Install the Claude Code skill (this is the default).
      --no-skill               Skip the skill step entirely.
  -i, --interactive            Ask before each decision (method, skill) instead of
                               taking the defaults. Prompts read /dev/tty, so this
                               works even under 'curl | bash'.
      --verify-attestations    Verify GitHub build provenance with 'gh' (must be installed).
      --no-color               Disable ANSI color in output.
  -h, --help                   Show this help.

The default run asks nothing: prebuilt binary when one is published for
this platform, source build otherwise, then the Claude Code skill. The
skill step delegates to 'symora setup skill', which owns version
comparison and updates (rerunning is always safe).

After install, the binary owns its lifecycle:
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
      | bash -s -- --version 0.9.0 --verify-attestations
  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/scripts/install.sh \\
      | bash -s -- --source --no-skill
EOF
}

# ─── argument parsing ───────────────────────────────────────────────────────

parse_args() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --version)              VERSION="${2:?--version requires a value}"; VERSION="${VERSION#v}"; VERSION_REQUESTED="$VERSION"; shift 2 ;;
            --version=*)            VERSION="${1#*=}"; VERSION="${VERSION#v}"; VERSION_REQUESTED="$VERSION"; shift ;;
            --install-dir)          INSTALL_DIR="${2:?--install-dir requires a value}"; shift 2 ;;
            --install-dir=*)        INSTALL_DIR="${1#*=}"; shift ;;
            --prebuilt)             INSTALL_METHOD="prebuilt"; shift ;;
            --source)               INSTALL_METHOD="source"; shift ;;
            --skill)                SKILL_MODE="yes"; shift ;;
            --no-skill)             SKILL_MODE="no"; shift ;;
            -i|--interactive)       INTERACTIVE=true; shift ;;
            --verify-attestations)  VERIFY_ATTESTATIONS=true; shift ;;
            --no-color)             NO_COLOR=1; shift ;;
            -h|--help)              usage; exit 0 ;;
            *)                      log_die "Unknown argument: $1 (use --help)" ;;
        esac
    done

    if [ "$VERSION" = "latest" ]; then
        VERSION=""
        VERSION_REQUESTED=""
    fi

    case "$INSTALL_DIR" in
        -*) log_die "Invalid --install-dir: $INSTALL_DIR (must not start with '-')" ;;
    esac
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

# A controlling terminal we can prompt on — independent of stdin, which
# is the script stream itself under `curl | bash`.
have_tty() { [ -e /dev/tty ] && ( : >/dev/tty ) 2>/dev/null; }

# Prompt on /dev/tty. An explicit empty Enter takes `enter_default`;
# EOF / a closed tty takes `eof_default` — they are different intents
# (a vanished terminal must never be read as consent).
prompt_choice() {
    local prompt="$1" enter_default="$2" eof_default="${3:-$2}" choice=""
    if ! have_tty; then
        printf '%s\n' "$eof_default"
        return 0
    fi
    printf '%s' "$prompt" >/dev/tty
    if IFS= read -r choice </dev/tty; then
        printf '%s\n' "${choice:-$enter_default}"
    else
        printf '\n' >/dev/tty
        printf '%s\n' "$eof_default"
    fi
}

# ─── platform detection ─────────────────────────────────────────────────────

# Target triple for the prebuilt archive; empty when no prebuilt is
# published for this machine (the source path covers it instead).
prebuilt_target() {
    local os arch
    os="$(uname -s | tr '[:upper:]' '[:lower:]')"
    arch="$(uname -m)"

    case "$arch" in
        x86_64|amd64)  arch="x86_64" ;;
        arm64|aarch64) arch="aarch64" ;;
        *) return 0 ;;
    esac

    case "$os" in
        darwin)
            if [ "$arch" = "aarch64" ]; then
                printf '%s\n' "aarch64-apple-darwin"
            fi
            ;;
        linux) printf '%s-unknown-linux-gnu\n' "$arch" ;;
    esac
    # Empty output = no prebuilt for this platform; that is an answer,
    # not an error — a non-zero status here would abort the installer
    # under `set -e` on exactly the platforms the source fallback serves.
    return 0
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

# The latest release version — asked of the API, and of the web redirect only
# where the API could not answer. Both answer the same question and they
# disagree for minutes at a time: the redirect trails the API after a release
# is published, which is exactly when someone installs, and read in that
# window it names the release before. So the API settles it, and the redirect
# answers only when the API cannot: the API's rate limit counts against an
# unauthenticated IP, which a shared runner can exhaust, and the redirect has
# no limit to exhaust.
resolve_latest_version() {
    local effective tag

    tag="$(http_get "${API_LATEST_URL}" 2>/dev/null \
        | sed -nE 's/.*"tag_name": *"v([^"]+)".*/\1/p' \
        | head -n 1)"
    if [ -n "$tag" ]; then
        printf '%s\n' "$tag"
        return 0
    fi

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

    return 1
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
    # Compare digests directly: `-c` would verify whatever filename the
    # downloaded .sha256 happens to name, not necessarily this archive.
    local expected actual
    expected="$(awk 'NR==1 { print tolower($1) }' "$dir/${base}.sha256")"
    [[ "$expected" =~ ^[0-9a-f]{64}$ ]] \
        || log_die "Malformed checksum file for $base"

    if have_cmd sha256sum; then
        actual="$(sha256sum "$archive" | awk '{ print tolower($1) }')"
    elif have_cmd shasum; then
        actual="$(shasum -a 256 "$archive" | awk '{ print tolower($1) }')"
    else
        log_die "Neither sha256sum nor shasum available — cannot verify download"
    fi

    [ "$actual" = "$expected" ] \
        || log_die "Checksum verification failed for $base (expected $expected, got $actual)"
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
    tar -xzf "$archive" -C "$extract_dir" || log_die "Archive extraction failed"
    local bin="$extract_dir/$BINARY_NAME"
    [ -x "$bin" ] || log_die "Archive did not contain executable $BINARY_NAME"
    printf '%s\n' "$bin"
}

# Source build. Inside a checkout it builds the working tree; anywhere
# else it builds the pinned release tag straight from git, so the curl
# one-shot covers machines without a prebuilt (e.g. Intel macs).
build_from_source() {
    have_cmd cargo || log_die "Source build requires Rust + cargo (https://rustup.rs)"

    if [ -n "$PROJECT_ROOT" ] && [ -f "$PROJECT_ROOT/Cargo.toml" ]; then
        if [ -n "$VERSION_REQUESTED" ]; then
            log_warn "--version $VERSION_REQUESTED is ignored inside a checkout — building the working tree"
        fi
        log_info "🔨 building the working tree ($PROJECT_ROOT)"
        ( cd "$PROJECT_ROOT" && cargo build --release ) >&2 \
            || log_die "cargo build failed"
        local built="$PROJECT_ROOT/target/release/$BINARY_NAME"
        [ -x "$built" ] || log_die "Build produced no executable at $built"
        printf '%s\n' "$built"
        return 0
    fi

    [ -n "$VERSION" ] || log_die "Could not resolve a release to build.
Pass --version vX.Y.Z — refusing to build an arbitrary branch."

    local cargo_root="$TMP_ROOT/cargo-root"
    log_info "🔨 building v$VERSION from $REPO_URL (this compiles the release — takes a few minutes)"
    cargo install --locked --git "$REPO_URL" --tag "v$VERSION" \
        --root "$cargo_root" "$BINARY_NAME" >&2 \
        || log_die "cargo install from git failed"
    printf '%s\n' "$cargo_root/bin/$BINARY_NAME"
}

stop_running_daemon() {
    local existing="$INSTALL_DIR/$BINARY_NAME"
    # A daemon is running when one answers, not when a file it left behind
    # exists; `daemon stop` settles that itself and returns at once when
    # nothing is there.
    if [ -x "$existing" ]; then
        log_info "↺ stopping any running daemon before binary swap"
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

    # An installed binary that cannot run is a failed install, not a
    # warning — fail here, before the skill step would mask it.
    "$INSTALL_DIR/$BINARY_NAME" --version >/dev/null 2>&1 \
        || log_die "Installed binary failed to run: $INSTALL_DIR/$BINARY_NAME"

    log_ok "✓ installed $(display_path "$INSTALL_DIR")/$BINARY_NAME"
}

select_install_method() {
    local target="$1"

    if [ -n "$INSTALL_METHOD" ]; then
        printf '%s\n' "$INSTALL_METHOD"
        return 0
    fi

    if ! have_cmd curl; then
        log_warn "curl unavailable — falling back to source build"
        printf 'source\n'
        return 0
    fi

    if [ -z "$target" ]; then
        log_warn "No prebuilt binary is published for this platform — building from source"
        printf 'source\n'
        return 0
    fi

    # Default: no questions. Prebuilt is strictly better when it exists
    # (fast, SHA-256 verified); --source and --interactive stay available
    # for anyone who wants the other path.
    if [ "$INTERACTIVE" = true ] && have_tty; then
        {
            log ""
            log "Installation method:"
            log "  [1] Prebuilt binary  (recommended — fast, SHA-256 verified)"
            log "  [2] Build from source (requires Rust; compiles the release tag)"
            log ""
        }
        local choice
        choice="$(prompt_choice "Choose [1-2] (default: 1): " "1")"
        case "$choice" in
            2)    printf 'source\n' ;;
            1|"") printf 'prebuilt\n' ;;
            *)    log_die "Invalid choice: $choice" ;;
        esac
        return 0
    fi

    printf 'prebuilt\n'
}

# ─── skill ──────────────────────────────────────────────────────────────────

# The binary owns skill installation (version comparison,
# updates) — this step only decides whether to invoke it. `-y` keeps the
# delegated run prompt-free; stdin under `curl | bash` is the script
# stream and must not be consumed by the child.
offer_skill_install() {
    local bin="$INSTALL_DIR/$BINARY_NAME"

    case "$SKILL_MODE" in
        no)
            return 0
            ;;
        auto)
            # Installing the skill is the point of the one-shot: rerunning
            # 'setup skill' is version-aware and backed up, so defaulting
            # to yes is safe. Only --interactive turns this into a question.
            if [ "$INTERACTIVE" = true ] && have_tty; then
                local choice
                choice="$(prompt_choice "Install the Claude Code skill (~/.claude/skills/symora)? [Y/n]: " "y" "n")"
                case "$choice" in
                    [yY]) ;;
                    [nN])
                        log_dim "Skipped — run '${BINARY_NAME} setup skill' anytime"
                        return 0
                        ;;
                    *) log_die "Invalid choice: $choice" ;;
                esac
            else
                log_dim "Installing the Claude Code skill (pass --no-skill to opt out)"
            fi
            ;;
    esac

    log_info "📋 installing Claude Code skill"
    if "$bin" setup skill -y </dev/null >&2; then
        log_ok "✓ skill installed"
    else
        log_warn "Skill installation failed — run '${BINARY_NAME} setup skill' manually"
    fi
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
    target="$(prebuilt_target)"

    INSTALL_METHOD="$(select_install_method "$target")"

    # Resolve the version only for the paths that consume it: the
    # prebuilt archive URL and the remote source build's git tag. A
    # checkout build compiles the working tree and needs no network.
    if [ -z "$VERSION" ] && have_cmd curl; then
        case "$INSTALL_METHOD" in
            prebuilt) VERSION="$(resolve_latest_version || true)" ;;
            source)
                if [ -z "$PROJECT_ROOT" ]; then
                    VERSION="$(resolve_latest_version || true)"
                fi
                ;;
        esac
    fi
    if [ -n "$VERSION" ]; then
        is_valid_version "$VERSION" || log_die "Invalid version: $VERSION"
    fi

    case "$INSTALL_METHOD" in
        prebuilt)
            have_cmd curl || log_die "curl is required for prebuilt downloads.
Rerun with --source (requires Rust): builds the release straight from the git tag."
            [ -n "$target" ] || log_die "No prebuilt binary is published for this platform.
Rerun with --source (requires Rust): builds the release straight from the git tag."
            [ -n "$VERSION" ] || log_die "Could not resolve latest version. Pass --version vX.Y.Z."

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
            if [ "$VERIFY_ATTESTATIONS" = true ]; then
                log_warn "--verify-attestations applies to prebuilt downloads only — ignored for source builds"
            fi
            log_section "Symora · source build"
            log "  prefix: $(display_path "$INSTALL_DIR")"
            log ""
            local bin
            bin="$(build_from_source)"
            install_binary "$bin"
            ;;
        *)
            log_die "Unknown install method: $INSTALL_METHOD"
            ;;
    esac

    offer_skill_install
    print_post_install
}

main "$@"
