#!/bin/bash
set -e

VERSION="0.1.0"
REPO="nhatvu148/video-transcriber-mcp-rs"
BINARY_NAME="video-transcriber-mcp"

echo "🚀 Installing video-transcriber-mcp v${VERSION}"
echo ""

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
    darwin)
        OS="apple-darwin"
        ;;
    linux)
        OS="unknown-linux-gnu"
        ;;
    *)
        echo "❌ Unsupported OS: $OS"
        exit 1
        ;;
esac

case "$ARCH" in
    x86_64|amd64)
        ARCH="x86_64"
        ;;
    aarch64|arm64)
        ARCH="aarch64"
        ;;
    *)
        echo "❌ Unsupported architecture: $ARCH"
        exit 1
        ;;
esac

TARGET="${ARCH}-${OS}"
ARCHIVE="${BINARY_NAME}-${TARGET}.tar.gz"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/v${VERSION}/${ARCHIVE}"

echo "📥 Downloading for ${TARGET}..."
echo "   URL: ${DOWNLOAD_URL}"
echo ""

# Download
if command -v curl &> /dev/null; then
    curl -L "$DOWNLOAD_URL" -o "$ARCHIVE"
elif command -v wget &> /dev/null; then
    wget "$DOWNLOAD_URL" -O "$ARCHIVE"
else
    echo "❌ Error: curl or wget is required"
    exit 1
fi

# Extract
echo "📦 Extracting..."
tar xzf "$ARCHIVE"
rm "$ARCHIVE"

# Install
INSTALL_DIR="${HOME}/.local/bin"
mkdir -p "$INSTALL_DIR"

mv "$BINARY_NAME" "$INSTALL_DIR/"
chmod +x "${INSTALL_DIR}/${BINARY_NAME}"

echo ""
echo "✅ Installation complete!"
echo ""
echo "📍 Binary installed to: ${INSTALL_DIR}/${BINARY_NAME}"
echo ""
echo "💡 Make sure ${INSTALL_DIR} is in your PATH:"
echo "   export PATH=\"\$HOME/.local/bin:\$PATH\""
echo ""
echo "🔧 To use with Claude Code, add this to your settings:"
echo ""
cat <<'EOF'
{
  "video-transcriber-mcp": {
    "command": "/Users/YOUR_USERNAME/.local/bin/video-transcriber-mcp",
    "args": [],
    "env": {
      "RUST_LOG": "info"
    }
  }
}
EOF
echo ""
echo "📚 Documentation: https://github.com/${REPO}"
