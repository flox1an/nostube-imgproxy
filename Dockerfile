# Stage 1: Build
FROM rustlang/rust:nightly-slim AS builder

# Install build dependencies for AVIF support (meson, ninja) and general tools
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    meson \
    ninja-build \
    python3 \
    python3-pip \
    nasm \
    git \
    && rm -rf /var/lib/apt/lists/*

# Set working directory
WORKDIR /app

# Copy dependency files
COPY Cargo.toml ./
COPY .cargo .cargo

# Create dummy main.rs for dependency caching
RUN mkdir -p src && echo "fn main() {}" > src/main.rs

# Build dependencies only (this layer will be cached)
RUN cargo build --release && rm -rf src

# Now copy actual source code
COPY src ./src

# Set environment for AVIF support
ENV SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always

# Build release binary with actual code (force rebuild by touching main.rs)
RUN touch src/main.rs && cargo build --release

# Stage 2: Runtime
FROM debian:trixie-slim

# Install runtime dependencies (FFmpeg for video thumbnails, curl for health checks)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    ffmpeg \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1000 imgproxy

# Create cache directory
RUN mkdir -p /cache/original /cache/processed && \
    chown -R imgproxy:imgproxy /cache

# Copy binary from builder
COPY --from=builder /app/target/release/rust-imgproxy /usr/local/bin/rust-imgproxy

# Switch to non-root user
USER imgproxy

# Set default environment variables
ENV BIND_ADDR=0.0.0.0:8081 \
    CACHE_DIR=/cache \
    CACHE_TTL_SECS=86400 \
    FETCH_TIMEOUT_SECS=10 \
    MAX_IMAGE_BYTES=16777216 \
    MAX_FFMPEG_CONCURRENT=8 \
    RUST_LOG=info

# Expose port
EXPOSE 8081

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8081/health || exit 1

# Run the binary
CMD ["/usr/local/bin/rust-imgproxy"]

