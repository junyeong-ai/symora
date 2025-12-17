#!/usr/bin/env bash
set -e

BINARY_NAME="symora"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
SKILL_NAME="symora"
USER_SKILL_DIR="$HOME/.claude/skills/$SKILL_NAME"
CONFIG_DIR="$HOME/.config/symora"

# ============================================================================
# Color Output
# ============================================================================

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

info() { echo -e "${BLUE}$1${NC}"; }
success() { echo -e "${GREEN}$1${NC}"; }
warn() { echo -e "${YELLOW}$1${NC}"; }
error() { echo -e "${RED}$1${NC}"; }

# ============================================================================
# Main
# ============================================================================

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "          Symora Uninstaller"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

# ============================================================================
# Binary Removal
# ============================================================================

echo "Binary Removal"
echo "──────────────────────────────────────────────────────"

if [ -f "$INSTALL_DIR/$BINARY_NAME" ]; then
    rm "$INSTALL_DIR/$BINARY_NAME"
    success "Removed $INSTALL_DIR/$BINARY_NAME"
else
    warn "Binary not found at $INSTALL_DIR/$BINARY_NAME"
fi

echo ""

# ============================================================================
# Skill Cleanup
# ============================================================================

echo "Claude Code Skill Cleanup"
echo "──────────────────────────────────────────────────────"

if [ -d "$USER_SKILL_DIR" ]; then
    echo "User-level skill found at: $USER_SKILL_DIR"
    echo ""
    read -p "Remove user-level skill? [y/N]: " choice
    echo

    case "$choice" in
        y|Y)
            # Check for backups
            backup_count=$(ls -d "${USER_SKILL_DIR}.backup_"* 2>/dev/null | wc -l || echo "0")

            if [ "$backup_count" -gt 0 ]; then
                echo "Found $backup_count backup(s):"
                ls -d "${USER_SKILL_DIR}.backup_"* 2>/dev/null | while read backup; do
                    echo "  • $(basename "$backup")"
                done
                echo ""
                read -p "Remove skill backups too? [y/N]: " backup_choice
                echo

                case "$backup_choice" in
                    y|Y)
                        rm -rf "${USER_SKILL_DIR}.backup_"* 2>/dev/null || true
                        success "Removed skill backups"
                        ;;
                    *)
                        info "Kept skill backups"
                        ;;
                esac
            fi

            rm -rf "$USER_SKILL_DIR"
            success "Removed user-level skill"
            ;;
        *)
            info "Kept user-level skill"
            ;;
    esac
else
    warn "User-level skill not found at: $USER_SKILL_DIR"
fi

echo ""
echo "Note: Project-level skill at ./.claude/skills/$SKILL_NAME is NOT removed."
echo "It's part of the project repository."
echo ""

# ============================================================================
# Configuration Cleanup
# ============================================================================

echo "Configuration Cleanup"
echo "──────────────────────────────────────────────────────"

read -p "Remove global configuration (~/.config/symora)? [y/N]: " choice
echo

case "$choice" in
    y|Y)
        if [ -d "$CONFIG_DIR" ]; then
            rm -rf "$CONFIG_DIR"
            success "Removed $CONFIG_DIR"
        else
            warn "Global config not found at $CONFIG_DIR"
        fi
        ;;
    *)
        info "Kept global configuration"
        ;;
esac

echo ""

# ============================================================================
# Daemon Cleanup
# ============================================================================

echo "Daemon Cleanup"
echo "──────────────────────────────────────────────────────"

# Check if daemon is running
SOCKET_PATH="/tmp/symora-daemon.sock"
PID_FILE="/tmp/symora-daemon.pid"

if [ -S "$SOCKET_PATH" ] || [ -f "$PID_FILE" ]; then
    echo "Daemon files found."
    read -p "Stop daemon and clean up? [Y/n]: " choice
    echo

    case "$choice" in
        n|N)
            info "Kept daemon files"
            ;;
        *)
            # Try to stop daemon gracefully
            if [ -f "$PID_FILE" ]; then
                PID=$(cat "$PID_FILE" 2>/dev/null)
                if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
                    info "Stopping daemon (PID: $PID)..."
                    kill "$PID" 2>/dev/null || true
                    sleep 1
                fi
            fi

            # Clean up files
            rm -f "$SOCKET_PATH" "$PID_FILE" 2>/dev/null || true
            success "Daemon cleaned up"
            ;;
    esac
else
    info "No daemon files found"
fi

echo ""

# ============================================================================
# Final Message
# ============================================================================

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
success "Uninstallation Complete!"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo "Remaining items (not automatically removed):"
echo "  • Project-level config: ./.symora/ (if exists)"
echo "  • Project-level skill: ./.claude/skills/$SKILL_NAME"
echo "  • Installed LSP servers (use package manager to remove)"
echo ""
echo "To reinstall: ./scripts/install.sh"
echo ""
