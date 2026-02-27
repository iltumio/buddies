#!/usr/bin/env bash
set -euo pipefail

BOLD='\033[1m'
DIM='\033[2m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
CYAN='\033[0;36m'
NC='\033[0m'

info()  { printf "${CYAN}→${NC} %s\n" "$*"; }
ok()    { printf "${GREEN}✓${NC} %s\n" "$*"; }
warn()  { printf "${YELLOW}!${NC} %s\n" "$*"; }
fail()  { printf "${RED}✗${NC} %s\n" "$*" >&2; exit 1; }

GITHUB_REPO="iltumio/buddies"

BUDDIES_BIN=""
BUDDIES_USER=""
BUDDIES_SIGNER="git"
CONFIGURE_CLAUDE=false
CONFIGURE_OPENCODE=false
CONFIGURE_OPENCLAW=false
SKIP_BUILD=false
USE_PREBUILT=false
BUDDIES_TRANSPORT="stdio"
BUDDIES_PORT="8080"

usage() {
    cat <<EOF
${BOLD}buddies install & configure${NC}

Usage: ./install.sh [options]

Options:
  --user <name>         Set BUDDIES_USER (default: OS username)
  --signer <mode>       Signing mode: git, none, gpg, ssh, generated (default: git)
  --transport <mode>    Transport mode: stdio or http (default: stdio)
  --port <port>         HTTP listen port when transport=http (default: 8080)
  --claude              Configure Claude Code
  --opencode            Configure OpenCode
  --openclaw            Configure OpenClaw
  --all                 Configure Claude Code, OpenCode, and OpenClaw
  --prebuilt            Download precompiled binary from GitHub releases
  --skip-build          Skip cargo build (use existing binary in PATH)
  -h, --help            Show this help

Examples:
  ./install.sh --all
  ./install.sh --all --prebuilt
  ./install.sh --claude --user alice --signer generated
  ./install.sh --opencode --skip-build
  ./install.sh --opencode --transport http --port 9090
  ./install.sh --openclaw --user alice
EOF
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --user)       BUDDIES_USER="$2"; shift 2 ;;
        --signer)     BUDDIES_SIGNER="$2"; shift 2 ;;
        --transport)  BUDDIES_TRANSPORT="$2"; shift 2 ;;
        --port)       BUDDIES_PORT="$2"; shift 2 ;;
        --claude)     CONFIGURE_CLAUDE=true; shift ;;
        --opencode)   CONFIGURE_OPENCODE=true; shift ;;
        --openclaw)   CONFIGURE_OPENCLAW=true; shift ;;
        --all)        CONFIGURE_CLAUDE=true; CONFIGURE_OPENCODE=true; CONFIGURE_OPENCLAW=true; shift ;;
        --skip-build) SKIP_BUILD=true; shift ;;
        --prebuilt)   USE_PREBUILT=true; SKIP_BUILD=true; shift ;;
        -h|--help)    usage ;;
        *)            fail "Unknown option: $1" ;;
    esac
done

if ! $CONFIGURE_CLAUDE && ! $CONFIGURE_OPENCODE && ! $CONFIGURE_OPENCLAW; then
    printf "\n${BOLD}Which tools do you want to configure?${NC}\n"
    printf "  1) Claude Code\n"
    printf "  2) OpenCode\n"
    printf "  3) OpenClaw\n"
    printf "  4) All\n"
    printf "\nChoice [4]: "
    read -r choice
    choice="${choice:-4}"
    case "$choice" in
        1) CONFIGURE_CLAUDE=true ;;
        2) CONFIGURE_OPENCODE=true ;;
        3) CONFIGURE_OPENCLAW=true ;;
        4) CONFIGURE_CLAUDE=true; CONFIGURE_OPENCODE=true; CONFIGURE_OPENCLAW=true ;;
        *) fail "Invalid choice" ;;
    esac
fi

if [[ -z "$BUDDIES_USER" ]]; then
    default_user="$(whoami 2>/dev/null || echo "anonymous")"
    printf "\n${BOLD}BUDDIES_USER${NC} [${DIM}%s${NC}]: " "$default_user"
    read -r BUDDIES_USER
    BUDDIES_USER="${BUDDIES_USER:-$default_user}"
fi

check_deps() {
    local missing=()
    if ! $USE_PREBUILT; then
        command -v cargo >/dev/null 2>&1 || missing+=("cargo (Rust toolchain)")
    fi
    if $USE_PREBUILT; then
        command -v curl >/dev/null 2>&1 || missing+=("curl")
    fi
    if $CONFIGURE_CLAUDE; then
        command -v claude >/dev/null 2>&1 || missing+=("claude (Claude Code CLI)")
    fi
    if [[ ${#missing[@]} -gt 0 ]]; then
        warn "Missing dependencies:"
        for dep in "${missing[@]}"; do
            printf "  - %s\n" "$dep"
        done
        if [[ " ${missing[*]} " == *"cargo"* ]] && ! $SKIP_BUILD; then
            fail "Rust toolchain required. Install from https://rustup.rs or use --prebuilt"
        fi
        if [[ " ${missing[*]} " == *"curl"* ]]; then
            fail "curl is required for --prebuilt"
        fi
    fi
}

detect_target() {
    local arch
    arch="$(uname -m)"
    case "$arch" in
        x86_64)  echo "x86_64-unknown-linux-gnu" ;;
        aarch64) echo "aarch64-unknown-linux-gnu" ;;
        *)       fail "Unsupported architecture: $arch. Build from source instead." ;;
    esac
}

download_buddies() {
    local target
    target="$(detect_target)"
    local bin_name="buddies-${target}"
    local install_dir="$HOME/.local/bin"

    info "Detecting latest release..."
    local latest_tag
    latest_tag=$(curl -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" \
        | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')

    if [[ -z "$latest_tag" ]]; then
        fail "Could not detect latest release. Check https://github.com/${GITHUB_REPO}/releases"
    fi

    local url="https://github.com/${GITHUB_REPO}/releases/download/${latest_tag}/${bin_name}"
    info "Downloading buddies ${latest_tag} for ${target}..."

    mkdir -p "$install_dir"
    if ! curl -fSL --progress-bar -o "${install_dir}/buddies" "$url"; then
        fail "Download failed. Check that a release exists for ${target} at:\n  ${url}"
    fi

    chmod +x "${install_dir}/buddies"
    BUDDIES_BIN="${install_dir}/buddies"
    ok "Installed: $BUDDIES_BIN (${latest_tag})"

    if ! echo "$PATH" | tr ':' '\n' | grep -qx "$install_dir"; then
        warn "$install_dir is not in your PATH. Add it with:"
        printf "  export PATH=\"%s:\$PATH\"\n" "$install_dir"
    fi
}

build_buddies() {
    if $USE_PREBUILT; then
        download_buddies
        return
    fi

    if $SKIP_BUILD; then
        BUDDIES_BIN="$(command -v buddies 2>/dev/null || true)"
        if [[ -z "$BUDDIES_BIN" ]]; then
            fail "buddies not found in PATH. Run without --skip-build to build it."
        fi
        ok "Using existing binary: $BUDDIES_BIN"
        return
    fi

    info "Building buddies (release)..."
    cargo install --path . --force 2>&1 | tail -1
    BUDDIES_BIN="$(command -v buddies 2>/dev/null || echo "$HOME/.cargo/bin/buddies")"

    if [[ ! -x "$BUDDIES_BIN" ]]; then
        fail "Build succeeded but buddies binary not found in PATH. Add ~/.cargo/bin to your PATH."
    fi
    ok "Installed: $BUDDIES_BIN"
}

configure_claude() {
    info "Configuring Claude Code..."

    if command -v claude >/dev/null 2>&1; then
        claude mcp remove buddies --scope user 2>/dev/null || true
        claude mcp add --transport stdio --scope user \
            -e "BUDDIES_USER=$BUDDIES_USER" \
            -e "BUDDIES_SIGNER=$BUDDIES_SIGNER" \
            buddies -- "$BUDDIES_BIN"
        ok "Claude Code configured (user scope)"
    else
        local config_file="$HOME/.claude.json"
        local tmp_file
        tmp_file="$(mktemp)"

        local existing="{}"
        if [[ -f "$config_file" ]]; then
            existing="$(cat "$config_file")"
        fi

        local buddies_entry
        buddies_entry=$(cat <<ENTRY
{
  "type": "stdio",
  "command": "$BUDDIES_BIN",
  "args": [],
  "env": {
    "BUDDIES_USER": "$BUDDIES_USER",
    "BUDDIES_SIGNER": "$BUDDIES_SIGNER"
  }
}
ENTRY
)

        if command -v jq >/dev/null 2>&1; then
            echo "$existing" | jq --argjson entry "$buddies_entry" \
                '.mcpServers.buddies = $entry' > "$tmp_file"
            mv "$tmp_file" "$config_file"
            ok "Wrote $config_file"
        elif command -v python3 >/dev/null 2>&1; then
            python3 -c "
import json, sys
cfg = json.loads('''$existing''')
cfg.setdefault('mcpServers', {})
cfg['mcpServers']['buddies'] = json.loads('''$buddies_entry''')
json.dump(cfg, open('$tmp_file', 'w'), indent=2)
"
            mv "$tmp_file" "$config_file"
            ok "Wrote $config_file"
        else
            warn "Neither jq nor python3 found. Writing config manually."
            cat > "$config_file" <<MANUAL
{
  "mcpServers": {
    "buddies": $buddies_entry
  }
}
MANUAL
            ok "Wrote $config_file (note: any previous config was overwritten)"
        fi
    fi
}

configure_opencode() {
    info "Configuring OpenCode..."

    local config_file
    local scope="global"

    if [[ -f "opencode.json" ]] || [[ -f "opencode.jsonc" ]]; then
        scope="project"
        config_file="$(pwd)/opencode.json"
        if [[ -f "opencode.jsonc" ]] && [[ ! -f "opencode.json" ]]; then
            config_file="$(pwd)/opencode.jsonc"
        fi
    else
        local global_dir="$HOME/.config/opencode"
        mkdir -p "$global_dir"
        config_file="$global_dir/opencode.json"
    fi

    local tmp_file
    tmp_file="$(mktemp)"

    local existing="{}"
    if [[ -f "$config_file" ]]; then
        existing="$(cat "$config_file")"
    fi

    local buddies_entry
    if [[ "$BUDDIES_TRANSPORT" == "http" ]]; then
        buddies_entry=$(cat <<ENTRY
{
  "type": "remote",
  "url": "http://127.0.0.1:$BUDDIES_PORT/mcp"
}
ENTRY
)
    else
        buddies_entry=$(cat <<ENTRY
{
  "type": "local",
  "command": ["$BUDDIES_BIN"],
  "enabled": true,
  "environment": {
    "BUDDIES_USER": "$BUDDIES_USER",
    "BUDDIES_SIGNER": "$BUDDIES_SIGNER"
  }
}
ENTRY
)
    fi

    if command -v jq >/dev/null 2>&1; then
        echo "$existing" | jq --argjson entry "$buddies_entry" \
            '.mcp.buddies = $entry' > "$tmp_file"
        mv "$tmp_file" "$config_file"
    elif command -v python3 >/dev/null 2>&1; then
        python3 -c "
import json
cfg = json.loads('''$existing''')
cfg.setdefault('mcp', {})
cfg['mcp']['buddies'] = json.loads('''$buddies_entry''')
json.dump(cfg, open('$tmp_file', 'w'), indent=2)
"
        mv "$tmp_file" "$config_file"
    else
        warn "Neither jq nor python3 found. Writing config manually."
        cat > "$config_file" <<MANUAL
{
  "mcp": {
    "buddies": $buddies_entry
  }
}
MANUAL
    fi

    ok "OpenCode configured ($scope): $config_file"
}

configure_openclaw() {
    info "Configuring OpenClaw..."

    local config_dir="$HOME/.openclaw"
    local config_file="$config_dir/config.json"

    if command -v openclaw >/dev/null 2>&1; then
        openclaw mcp remove buddies 2>/dev/null || true
        openclaw mcp add --transport stdio \
            -e "BUDDIES_USER=$BUDDIES_USER" \
            -e "BUDDIES_SIGNER=$BUDDIES_SIGNER" \
            buddies -- "$BUDDIES_BIN"
        ok "OpenClaw configured via CLI"
    else
        mkdir -p "$config_dir"

        local tmp_file
        tmp_file="$(mktemp)"

        local existing="{}"
        if [[ -f "$config_file" ]]; then
            existing="$(cat "$config_file")"
        fi

        local buddies_entry
        buddies_entry=$(cat <<ENTRY
{
  "command": "$BUDDIES_BIN",
  "args": [],
  "env": {
    "BUDDIES_USER": "$BUDDIES_USER",
    "BUDDIES_SIGNER": "$BUDDIES_SIGNER"
  }
}
ENTRY
)

        if command -v jq >/dev/null 2>&1; then
            echo "$existing" | jq --argjson entry "$buddies_entry" \
                '.mcpServers.buddies = $entry' > "$tmp_file"
            mv "$tmp_file" "$config_file"
            ok "Wrote $config_file"
        elif command -v python3 >/dev/null 2>&1; then
            python3 -c "
import json
cfg = json.loads('''$existing''')
cfg.setdefault('mcpServers', {})
cfg['mcpServers']['buddies'] = json.loads('''$buddies_entry''')
json.dump(cfg, open('$tmp_file', 'w'), indent=2)
"
            mv "$tmp_file" "$config_file"
            ok "Wrote $config_file"
        else
            warn "Neither jq nor python3 found. Writing config manually."
            cat > "$config_file" <<MANUAL
{
  "mcpServers": {
    "buddies": $buddies_entry
  }
}
MANUAL
            ok "Wrote $config_file (note: any previous config was overwritten)"
        fi
    fi
}

printf "\n${BOLD}buddies — install & configure${NC}\n\n"

check_deps
build_buddies

printf "\n"

$CONFIGURE_CLAUDE && configure_claude
$CONFIGURE_OPENCODE && configure_opencode
$CONFIGURE_OPENCLAW && configure_openclaw

printf "\n${GREEN}${BOLD}Done!${NC}\n\n"
printf "  user:      ${BOLD}%s${NC}\n" "$BUDDIES_USER"
printf "  signer:    ${BOLD}%s${NC}\n" "$BUDDIES_SIGNER"
printf "  transport: ${BOLD}%s${NC}\n" "$BUDDIES_TRANSPORT"
printf "  binary:    ${BOLD}%s${NC}\n" "$BUDDIES_BIN"
if [[ "$BUDDIES_TRANSPORT" == "http" ]]; then
    printf "  port:      ${BOLD}%s${NC}\n" "$BUDDIES_PORT"
fi
printf "\n"

if $CONFIGURE_CLAUDE; then
    printf "  ${DIM}Claude Code: restart claude to pick up changes${NC}\n"
fi
if $CONFIGURE_OPENCODE; then
    if [[ "$BUDDIES_TRANSPORT" == "http" ]]; then
        printf "  ${DIM}OpenCode: configured for remote connection to http://127.0.0.1:$BUDDIES_PORT/mcp${NC}\n"
        printf "  ${DIM}OpenCode: run 'BUDDIES_TRANSPORT=http BUDDIES_PORT=$BUDDIES_PORT BUDDIES_USER=$BUDDIES_USER buddies' before starting opencode${NC}\n"
    else
        printf "  ${DIM}OpenCode: restart opencode to pick up changes${NC}\n"
    fi
fi
if $CONFIGURE_OPENCLAW; then
    printf "  ${DIM}OpenClaw: restart openclaw gateway to pick up changes${NC}\n"
fi
printf "\n"
