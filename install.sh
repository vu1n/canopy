#!/bin/sh
# Canopy client installer — installs canopy CLI + canopy-mcp
# Usage: curl -fsSL https://raw.githubusercontent.com/vu1n/canopy/main/install.sh | sh
set -eu

REPO="vu1n/canopy"
INSTALL_DIR="${CANOPY_INSTALL_DIR:-$HOME/.local/bin}"
CLAUDE_SETUP=true

# Parse flags
for arg in "$@"; do
  case "$arg" in
    --no-claude-setup)
      CLAUDE_SETUP=false
      ;;
    --prefix=*)
      INSTALL_DIR="${arg#--prefix=}/bin"
      ;;
    --prefix)
      # handled below with shift
      ;;
    --help|-h)
      echo "Usage: install.sh [OPTIONS]"
      echo ""
      echo "Options:"
      echo "  --no-claude-setup   Skip Claude Code MCP configuration"
      echo "  --prefix <dir>      Install to <dir>/bin (default: ~/.local)"
      echo "  -h, --help          Show this help"
      exit 0
      ;;
  esac
done

# Handle --prefix <value> (two-arg form)
i=1
for arg in "$@"; do
  if [ "$arg" = "--prefix" ]; then
    shift_next=true
  elif [ "${shift_next:-}" = "true" ]; then
    INSTALL_DIR="$arg/bin"
    shift_next=false
  fi
  i=$((i + 1))
done

# Detect platform
OS="$(uname -s)"
case "$OS" in
  Darwin) os="macos" ;;
  Linux)  os="linux" ;;
  *)
    echo "Error: Unsupported OS: $OS"
    exit 1
    ;;
esac

# Detect architecture
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) arch="x86_64" ;;
  arm64|aarch64) arch="aarch64" ;;
  *)
    echo "Error: Unsupported architecture: $ARCH"
    exit 1
    ;;
esac

echo "Detected platform: ${os}-${arch}"

# Fetch latest release tag
echo "Fetching latest release..."
LATEST_URL="https://api.github.com/repos/${REPO}/releases/latest"
if command -v curl >/dev/null 2>&1; then
  TAG=$(curl -fsSL "$LATEST_URL" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"//;s/".*//')
elif command -v wget >/dev/null 2>&1; then
  TAG=$(wget -qO- "$LATEST_URL" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"//;s/".*//')
else
  echo "Error: curl or wget required"
  exit 1
fi

if [ -z "$TAG" ]; then
  echo "Error: Could not determine latest release"
  exit 1
fi

echo "Latest release: $TAG"

# Download tarball
TARBALL="canopy-${os}-${arch}.tar.gz"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${TAG}/${TARBALL}"

echo "Downloading ${TARBALL}..."
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$DOWNLOAD_URL" -o "$TMPDIR/$TARBALL"
else
  wget -q "$DOWNLOAD_URL" -O "$TMPDIR/$TARBALL"
fi

# Extract binaries
echo "Extracting to ${INSTALL_DIR}..."
mkdir -p "$INSTALL_DIR"
tar -xzf "$TMPDIR/$TARBALL" -C "$TMPDIR"
cp "$TMPDIR/canopy" "$INSTALL_DIR/canopy"
cp "$TMPDIR/canopy-mcp" "$INSTALL_DIR/canopy-mcp"
chmod +x "$INSTALL_DIR/canopy" "$INSTALL_DIR/canopy-mcp"

# Check PATH
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo ""
    echo "WARNING: ${INSTALL_DIR} is not in your PATH."
    echo "Add it with:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    echo ""
    ;;
esac

# Configure Claude Code
if [ "$CLAUDE_SETUP" = "true" ]; then
  CLAUDE_SETTINGS="$HOME/.claude/settings.json"
  if command -v claude >/dev/null 2>&1 || [ -d "$HOME/.claude" ]; then
    mkdir -p "$HOME/.claude"
    if [ -f "$CLAUDE_SETTINGS" ]; then
      # Check if canopy is already configured
      if grep -q '"canopy"' "$CLAUDE_SETTINGS" 2>/dev/null; then
        echo "Claude Code: canopy MCP server already configured"
      else
        # Insert canopy into existing mcpServers or add mcpServers block
        if grep -q '"mcpServers"' "$CLAUDE_SETTINGS" 2>/dev/null; then
          # Add canopy to existing mcpServers object
          sed_expr='s/"mcpServers" *: *{/"mcpServers": { "canopy": { "command": "canopy-mcp" },/'
          if [ "$os" = "macos" ]; then
            sed -i '' "$sed_expr" "$CLAUDE_SETTINGS"
          else
            sed -i "$sed_expr" "$CLAUDE_SETTINGS"
          fi
          echo "Claude Code: added canopy MCP server to existing settings"
        else
          # Create new settings with mcpServers
          cat > "$CLAUDE_SETTINGS" <<'SETTINGS'
{
  "mcpServers": {
    "canopy": {
      "command": "canopy-mcp"
    }
  }
}
SETTINGS
          echo "Claude Code: created settings with canopy MCP server"
        fi
      fi
    else
      cat > "$CLAUDE_SETTINGS" <<'SETTINGS'
{
  "mcpServers": {
    "canopy": {
      "command": "canopy-mcp"
    }
  }
}
SETTINGS
      echo "Claude Code: created settings with canopy MCP server"
    fi
  else
    echo "Claude Code not detected. To configure later, add to ~/.claude/settings.json:"
    echo '  { "mcpServers": { "canopy": { "command": "canopy-mcp" } } }'
  fi
fi

echo ""
echo "Canopy installed successfully!"
echo "  canopy     → ${INSTALL_DIR}/canopy"
echo "  canopy-mcp → ${INSTALL_DIR}/canopy-mcp"
echo ""
echo "Next steps:"
echo "  canopy --help          # CLI usage"
echo "  canopy init            # Initialize in a repo"
echo "  canopy query -p 'auth' # Query the codebase"
