FROM rust:1.91 as builder

WORKDIR /app
COPY . .

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ffmpeg \
    python3 \
    python3-pip \
    curl \
    ca-certificates \
    && pip3 install yt-dlp \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/video-transcriber-mcp /usr/local/bin/

RUN mkdir -p /root/.local/share/whisper-models

WORKDIR /app

ENTRYPOINT ["video-transcriber-mcp"]
