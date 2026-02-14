#!/bin/sh
# Canopy service installer — installs canopy-service HTTP server
# Usage: curl -fsSL https://raw.githubusercontent.com/vu1n/canopy/main/install-service.sh | sh
set -eu

REPO="vu1n/canopy"
INSTALL_DIR="${CANOPY_INSTALL_DIR:-$HOME/.local/bin}"

# Parse flags
for arg in "$@"; do
  case "$arg" in
    --prefix=*)
      INSTALL_DIR="${arg#--prefix=}/bin"
      ;;
    --prefix)
      # handled below
      ;;
    --help|-h)
      echo "Usage: install-service.sh [OPTIONS]"
      echo ""
      echo "Options:"
      echo "  --prefix <dir>   Install to <dir>/bin (default: ~/.local)"
      echo "  -h, --help       Show this help"
      exit 0
      ;;
  esac
done

# Handle --prefix <value> (two-arg form)
for arg in "$@"; do
  if [ "$arg" = "--prefix" ]; then
    shift_next=true
  elif [ "${shift_next:-}" = "true" ]; then
    INSTALL_DIR="$arg/bin"
    shift_next=false
  fi
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

# Extract binary
echo "Extracting to ${INSTALL_DIR}..."
mkdir -p "$INSTALL_DIR"
tar -xzf "$TMPDIR/$TARBALL" -C "$TMPDIR"
cp "$TMPDIR/canopy-service" "$INSTALL_DIR/canopy-service"
chmod +x "$INSTALL_DIR/canopy-service"

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

echo ""
echo "Canopy service installed successfully!"
echo "  canopy-service → ${INSTALL_DIR}/canopy-service"
echo ""
echo "Usage:"
echo "  canopy-service --port 3000                    # Start on port 3000"
echo ""
echo "  # Register a repo"
echo "  curl -X POST localhost:3000/repos/add \\"
echo "    -H 'Content-Type: application/json' \\"
echo "    -d '{\"path\": \"/path/to/repo\", \"name\": \"my-repo\"}'"
echo ""
echo "  # Connect CLI to service"
echo "  canopy --service-url http://localhost:3000 query --symbol 'Config'"
