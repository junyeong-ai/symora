#!/usr/bin/env bash
set -e

BINARY_NAME="symora"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
REPO="junyeong-ai/symora"
SKILL_NAME="symora"
PROJECT_SKILL_DIR=".claude/skills/$SKILL_NAME"
USER_SKILL_DIR="$HOME/.claude/skills/$SKILL_NAME"

# ============================================================================
# Color Output
# ============================================================================

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

info() { echo -e "${BLUE}$1${NC}" >&2; }
success() { echo -e "${GREEN}$1${NC}" >&2; }
warn() { echo -e "${YELLOW}$1${NC}" >&2; }
error() { echo -e "${RED}$1${NC}" >&2; }

# ============================================================================
# Platform Detection
# ============================================================================

detect_platform() {
    local os=$(uname -s | tr '[:upper:]' '[:lower:]')
    local arch=$(uname -m)

    case "$os" in
        linux) os="unknown-linux-gnu" ;;
        darwin) os="apple-darwin" ;;
        *) error "Unsupported OS: $os"; exit 1 ;;
    esac

    case "$arch" in
        x86_64) arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) error "Unsupported architecture: $arch"; exit 1 ;;
    esac

    echo "${arch}-${os}"
}

get_os() {
    case "$(uname -s)" in
        Darwin) echo "macos" ;;
        Linux) echo "linux" ;;
        *) echo "unknown" ;;
    esac
}

# ============================================================================
# Binary Installation
# ============================================================================

get_latest_version() {
    curl -sf "https://api.github.com/repos/$REPO/releases/latest" \
        | grep '"tag_name"' \
        | sed -E 's/.*"v([^"]+)".*/\1/' \
        || echo ""
}

download_binary() {
    local version="$1"
    local target="$2"
    local archive="symora-v${version}-${target}.tar.gz"
    local url="https://github.com/$REPO/releases/download/v${version}/${archive}"
    local checksum_url="${url}.sha256"

    info "Downloading $archive..."
    if ! curl -fLO "$url" 2>/dev/null; then
        error "Download failed"
        return 1
    fi

    info "Verifying checksum..."
    if curl -fLO "$checksum_url" 2>/dev/null; then
        if command -v sha256sum >/dev/null; then
            sha256sum -c "${archive}.sha256" 2>/dev/null || return 1
        elif command -v shasum >/dev/null; then
            shasum -a 256 -c "${archive}.sha256" 2>/dev/null || return 1
        else
            warn "No checksum tool found, skipping verification"
        fi
    fi

    info "Extracting..."
    tar -xzf "$archive" 2>/dev/null
    rm -f "$archive" "${archive}.sha256"

    echo "$BINARY_NAME"
}

build_from_source() {
    info "Building from source..."
    cargo build --release >&2
    echo "target/release/$BINARY_NAME"
}

install_binary() {
    local binary_path="$1"

    mkdir -p "$INSTALL_DIR"
    cp "$binary_path" "$INSTALL_DIR/$BINARY_NAME"
    chmod +x "$INSTALL_DIR/$BINARY_NAME"

    if [[ "$OSTYPE" == "darwin"* ]]; then
        codesign --force --deep --sign - "$INSTALL_DIR/$BINARY_NAME" 2>/dev/null || true
    fi

    success "Installed to $INSTALL_DIR/$BINARY_NAME"
}

# ============================================================================
# Skill Installation
# ============================================================================

get_skill_version() {
    local skill_md="$1"
    [ -f "$skill_md" ] && grep "^version:" "$skill_md" 2>/dev/null | sed 's/version: *//' || echo "unknown"
}

check_skill_exists() {
    [ -d "$USER_SKILL_DIR" ] && [ -f "$USER_SKILL_DIR/SKILL.md" ]
}

compare_versions() {
    local ver1="$1"
    local ver2="$2"

    if [ "$ver1" = "$ver2" ]; then
        echo "equal"
    elif [ "$ver1" = "unknown" ] || [ "$ver2" = "unknown" ]; then
        echo "unknown"
    else
        if [ "$(printf '%s\n' "$ver1" "$ver2" | sort -V | head -n1)" = "$ver1" ]; then
            [ "$ver1" != "$ver2" ] && echo "older" || echo "equal"
        else
            echo "newer"
        fi
    fi
}

backup_skill() {
    local timestamp=$(date +%Y%m%d_%H%M%S)
    local backup_dir="$USER_SKILL_DIR.backup_$timestamp"

    info "Creating backup: $backup_dir"
    cp -r "$USER_SKILL_DIR" "$backup_dir"
    success "Backup created"
}

install_skill() {
    info "Installing skill to $USER_SKILL_DIR"
    mkdir -p "$(dirname "$USER_SKILL_DIR")"
    cp -r "$PROJECT_SKILL_DIR" "$USER_SKILL_DIR"
    success "Skill installed"
}

prompt_skill_installation() {
    [ ! -d "$PROJECT_SKILL_DIR" ] && return 0

    local project_version=$(get_skill_version "$PROJECT_SKILL_DIR/SKILL.md")

    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "Claude Code Skill Installation"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo ""
    echo "Skill: $SKILL_NAME (v$project_version)"
    echo ""

    if check_skill_exists; then
        local existing_version=$(get_skill_version "$USER_SKILL_DIR/SKILL.md")
        local comparison=$(compare_versions "$existing_version" "$project_version")

        echo "Status: Already installed (v$existing_version)"
        echo ""

        case "$comparison" in
            equal)
                success "Latest version installed"
                echo ""
                read -p "Reinstall? [y/N]: " choice
                [[ "$choice" =~ ^[yY]$ ]] && { backup_skill; rm -rf "$USER_SKILL_DIR"; install_skill; } || echo "Skipped"
                ;;
            older)
                warn "New version available: v$project_version"
                echo ""
                read -p "Update? [Y/n]: " choice
                [[ ! "$choice" =~ ^[nN]$ ]] && { backup_skill; rm -rf "$USER_SKILL_DIR"; install_skill; success "Updated to v$project_version"; } || echo "Keeping current version"
                ;;
            newer)
                warn "Installed version (v$existing_version) > project version (v$project_version)"
                echo ""
                read -p "Downgrade? [y/N]: " choice
                [[ "$choice" =~ ^[yY]$ ]] && { backup_skill; rm -rf "$USER_SKILL_DIR"; install_skill; } || echo "Keeping current version"
                ;;
            *)
                warn "Version comparison failed"
                echo ""
                read -p "Reinstall? [y/N]: " choice
                [[ "$choice" =~ ^[yY]$ ]] && { backup_skill; rm -rf "$USER_SKILL_DIR"; install_skill; } || echo "Skipped"
                ;;
        esac
    else
        echo "Installation options:"
        echo ""
        echo "  [1] User-level install (RECOMMENDED)"
        echo "      → ~/.claude/skills/ (available in all projects)"
        echo ""
        echo "  [2] Project-level only"
        echo "      → Works only in this project directory"
        echo ""
        echo "  [3] Skip"
        echo ""

        read -p "Choose [1-3] (default: 1): " choice
        case "$choice" in
            2)
                echo ""
                success "Using project-level skill"
                echo "   Location: $(pwd)/$PROJECT_SKILL_DIR"
                ;;
            3)
                echo ""
                echo "Skipped"
                ;;
            1|"")
                echo ""
                install_skill
                echo ""
                success "Skill installed successfully!"
                echo ""
                echo "Claude Code can now use symora for:"
                echo "  • Finding symbol definitions and references"
                echo "  • Analyzing call hierarchies (who calls what)"
                echo "  • Refactoring code (rename, edit symbols)"
                echo "  • Searching code with AST patterns"
                ;;
            *)
                echo ""
                error "Invalid choice. Skipped."
                ;;
        esac
    fi

    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
}

# ============================================================================
# Dependency Installation
# ============================================================================

check_command() {
    command -v "$1" >/dev/null 2>&1
}

install_ripgrep() {
    local os=$(get_os)

    if check_command rg; then
        success "ripgrep already installed: $(rg --version | head -1)"
        return 0
    fi

    info "Installing ripgrep..."
    case "$os" in
        macos) brew install ripgrep ;;
        linux)
            if check_command apt; then
                sudo apt install -y ripgrep
            elif check_command dnf; then
                sudo dnf install -y ripgrep
            else
                cargo install ripgrep
            fi
            ;;
    esac
    success "ripgrep installed"
}

install_lsp_core() {
    local os=$(get_os)

    echo ""
    info "Installing Core LSP servers (Rust, TypeScript, Python, Go)..."
    echo ""

    # Rust
    if ! check_command rust-analyzer; then
        info "Installing rust-analyzer..."
        rustup component add rust-analyzer 2>/dev/null || warn "rust-analyzer: rustup not found"
    else
        success "rust-analyzer: $(rust-analyzer --version 2>/dev/null | head -1 || echo 'installed')"
    fi

    # TypeScript/JavaScript
    if ! check_command typescript-language-server; then
        info "Installing typescript-language-server..."
        npm install -g typescript typescript-language-server 2>/dev/null || warn "typescript-language-server: npm not found"
    else
        success "typescript-language-server: $(typescript-language-server --version 2>/dev/null || echo 'installed')"
    fi

    # Python
    if ! check_command pyright; then
        info "Installing pyright..."
        npm install -g pyright 2>/dev/null || warn "pyright: npm not found"
    else
        success "pyright: $(pyright --version 2>/dev/null || echo 'installed')"
    fi

    # Go
    if ! check_command gopls; then
        info "Installing gopls..."
        go install golang.org/x/tools/gopls@latest 2>/dev/null || warn "gopls: go not found"
    else
        success "gopls: $(gopls version 2>/dev/null | head -1 || echo 'installed')"
    fi
}

install_lsp_jvm() {
    local os=$(get_os)

    echo ""
    info "Installing JVM LSP servers (Java, Kotlin)..."
    echo ""

    # Java
    if ! check_command jdtls; then
        info "Installing jdtls..."
        case "$os" in
            macos) brew install jdtls 2>/dev/null || warn "jdtls: brew install failed" ;;
            *) warn "jdtls: Manual install required from https://download.eclipse.org/jdtls/snapshots/" ;;
        esac
    else
        success "jdtls: installed"
    fi

    # Kotlin
    if ! check_command kotlin-lsp; then
        info "Installing kotlin-lsp (JetBrains)..."
        case "$os" in
            macos) brew install JetBrains/utils/kotlin-lsp 2>/dev/null || warn "kotlin-lsp: brew install failed" ;;
            *) warn "kotlin-lsp: Manual install required from https://github.com/JetBrains/kotlin-lsp" ;;
        esac
    else
        success "kotlin-lsp: installed"
    fi
}

install_lsp_web() {
    echo ""
    info "Installing Web LSP servers (Vue, PHP, YAML)..."
    echo ""

    # Vue
    if ! check_command vue-language-server; then
        info "Installing vue-language-server..."
        npm install -g @vue/language-server 2>/dev/null || warn "vue-language-server: npm not found"
    else
        success "vue-language-server: installed"
    fi

    # PHP
    if ! check_command intelephense; then
        info "Installing intelephense..."
        npm install -g intelephense 2>/dev/null || warn "intelephense: npm not found"
    else
        success "intelephense: installed"
    fi

    # YAML
    if ! check_command yaml-language-server; then
        info "Installing yaml-language-server..."
        npm install -g yaml-language-server 2>/dev/null || warn "yaml-language-server: npm not found"
    else
        success "yaml-language-server: installed"
    fi
}

install_lsp_systems() {
    local os=$(get_os)

    echo ""
    info "Installing Systems LSP servers (C/C++, Zig)..."
    echo ""

    # C/C++
    if ! check_command clangd; then
        info "Installing clangd..."
        case "$os" in
            macos) brew install llvm 2>/dev/null || warn "clangd: brew install failed" ;;
            linux) sudo apt install -y clangd 2>/dev/null || warn "clangd: apt install failed" ;;
        esac
    else
        success "clangd: $(clangd --version 2>/dev/null | head -1 || echo 'installed')"
    fi

    # Zig
    if ! check_command zls; then
        info "Installing zls..."
        case "$os" in
            macos) brew install zls 2>/dev/null || warn "zls: brew install failed" ;;
            *) warn "zls: Manual install required from https://github.com/zigtools/zls/releases" ;;
        esac
    else
        success "zls: installed"
    fi
}

prompt_dependency_installation() {
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "Optional: Install Dependencies"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo ""
    echo "Symora works best with ripgrep and language servers."
    echo "You can install them now or later with 'symora doctor'."
    echo ""
    echo "Available packages:"
    echo ""
    echo "  [1] Core only (ripgrep + Rust/TS/Python/Go LSP)"
    echo "  [2] Core + JVM (adds Java, Kotlin)"
    echo "  [3] Core + Web (adds Vue, PHP, YAML)"
    echo "  [4] Core + Systems (adds C/C++, Zig)"
    echo "  [5] All of the above"
    echo "  [6] Skip (install later)"
    echo ""

    read -p "Choose [1-6] (default: 6): " choice

    case "$choice" in
        1)
            install_ripgrep
            install_lsp_core
            ;;
        2)
            install_ripgrep
            install_lsp_core
            install_lsp_jvm
            ;;
        3)
            install_ripgrep
            install_lsp_core
            install_lsp_web
            ;;
        4)
            install_ripgrep
            install_lsp_core
            install_lsp_systems
            ;;
        5)
            install_ripgrep
            install_lsp_core
            install_lsp_jvm
            install_lsp_web
            install_lsp_systems
            ;;
        6|"")
            echo ""
            echo "Skipped. Run 'symora doctor' later to see install instructions."
            ;;
        *)
            warn "Invalid choice. Skipped."
            ;;
    esac

    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
}

# ============================================================================
# Main
# ============================================================================

main() {
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "          Symora - LSP-based Code Intelligence"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo ""

    local binary_path=""
    local target=$(detect_platform)
    local version=$(get_latest_version)

    if [ -n "$version" ] && command -v curl >/dev/null; then
        echo "Latest version: v$version"
        echo ""
        echo "Installation method:"
        echo "  [1] Download prebuilt binary (RECOMMENDED - fast)"
        echo "  [2] Build from source (requires Rust toolchain)"
        echo ""
        read -p "Choose [1-2] (default: 1): " method

        case "$method" in
            2)
                binary_path=$(build_from_source)
                ;;
            1|"")
                binary_path=$(download_binary "$version" "$target") || {
                    warn "Download failed, falling back to source build"
                    binary_path=$(build_from_source)
                }
                ;;
            *)
                error "Invalid choice"
                exit 1
                ;;
        esac
    else
        [ -z "$version" ] && warn "Cannot fetch latest version, building from source"
        binary_path=$(build_from_source)
    fi

    install_binary "$binary_path"

    echo ""
    if echo "$PATH" | grep -q "$INSTALL_DIR"; then
        success "$INSTALL_DIR is in PATH"
    else
        warn "$INSTALL_DIR not in PATH"
        echo ""
        echo "Add to shell profile (~/.bashrc, ~/.zshrc):"
        echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
    fi
    echo ""

    if command -v "$BINARY_NAME" &>/dev/null; then
        echo "Installed version:"
        "$BINARY_NAME" --version
        echo ""
    fi

    # Skill installation
    prompt_skill_installation

    # Dependency installation
    prompt_dependency_installation

    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    success "Installation Complete!"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo ""
    echo "Next steps:"
    echo ""
    echo "1. Initialize project:     symora init"
    echo "2. Check dependencies:     symora doctor"
    echo "3. Find symbols:           symora find symbol src/main.rs"
    echo "4. Get hover info:         symora hover src/main.rs:10:5"
    echo ""
}

main "$@"
