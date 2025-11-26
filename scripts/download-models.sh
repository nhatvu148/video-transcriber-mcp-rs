#!/bin/bash

# Download Whisper models from Hugging Face
# Usage: ./download-models.sh [model_name]
# Example: ./download-models.sh base
# Or: ./download-models.sh all

set -e

MODELS_DIR="${HOME}/.cache/video-transcriber-mcp/models"
BASE_URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main"

# Create models directory
mkdir -p "$MODELS_DIR"

echo "📦 Whisper Model Downloader"
echo "Models will be saved to: $MODELS_DIR"
echo ""

download_model() {
    local model=$1
    local filename="ggml-${model}.bin"
    local url="${BASE_URL}/${filename}"
    local output="${MODELS_DIR}/${filename}"

    if [ -f "$output" ]; then
        echo "✅ ${model} model already exists ($(du -h "$output" | cut -f1))"
        return 0
    fi

    echo "⬇️  Downloading ${model} model..."
    echo "URL: $url"

    if command -v wget &> /dev/null; then
        wget -q --show-progress -O "$output" "$url"
    elif command -v curl &> /dev/null; then
        curl -L --progress-bar -o "$output" "$url"
    else
        echo "❌ Error: Neither wget nor curl is installed"
        exit 1
    fi

    echo "✅ ${model} model downloaded ($(du -h "$output" | cut -f1))"
    echo ""
}

# Parse arguments
MODEL=${1:-base}

if [ "$MODEL" = "all" ]; then
    echo "📥 Downloading all Whisper models..."
    echo ""
    download_model "tiny"
    download_model "base"
    download_model "small"
    download_model "medium"
    download_model "large"
    echo "🎉 All models downloaded!"
elif [ "$MODEL" = "tiny" ] || [ "$MODEL" = "base" ] || [ "$MODEL" = "small" ] || [ "$MODEL" = "medium" ] || [ "$MODEL" = "large" ]; then
    download_model "$MODEL"
    echo "🎉 Model downloaded!"
else
    echo "❌ Invalid model: $MODEL"
    echo ""
    echo "Usage: $0 [model_name]"
    echo ""
    echo "Available models:"
    echo "  tiny    - 75 MB   (fastest, lowest accuracy)"
    echo "  base    - 142 MB  (recommended for testing)"
    echo "  small   - 466 MB  (good balance)"
    echo "  medium  - 1.5 GB  (high accuracy)"
    echo "  large   - 2.9 GB  (best accuracy, slowest)"
    echo "  all     - Download all models"
    exit 1
fi

echo ""
echo "📊 Model sizes:"
echo "  tiny:   ~75 MB"
echo "  base:   ~142 MB"
echo "  small:  ~466 MB"
echo "  medium: ~1.5 GB"
echo "  large:  ~2.9 GB"
echo ""
echo "💡 Tip: Start with 'base' model for testing!"
