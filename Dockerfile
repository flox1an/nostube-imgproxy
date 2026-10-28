# Stage 1: Build
# Stable toolchain on trixie: the crate needs no nightly features, and
# `rustlang/rust:nightly-slim` floats to whatever nightly is current, which
# broke the image build on an ICE compiling tokio (nightly-2026-07-25).
# Base matches the trixie runtime stage so glibc lines up.
FROM rust:1-slim-trixie AS builder

# Install build dependencies for AVIF support, Brotli, and general tools
RUN apt-get update && apt-get install -y \
    build-essential \
    cmake \
    curl \
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

# Copy dependency files. The lockfile pins the reviewed/audited dependency
# set; without it every image build re-resolves the newest semver matches.
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo

# Create dummy main.rs for dependency caching
RUN mkdir -p src && echo "fn main() {}" > src/main.rs

# Build dependencies only (this layer will be cached)
RUN cargo build --release --locked && rm -rf src

# Debian trixie's FFmpeg dependency graph currently supplies Brotli 1.1.0,
# which is affected by CVE-2026-111497. Build Google Brotli 1.2.1 from its
# pinned release archive; the runtime stage gives these ABI-compatible
# libraries precedence over the vulnerable distro copies.
ARG BROTLI_VERSION=1.2.1
ARG BROTLI_SHA256=98d35d11a78dc453aebb7d10c3748f3f73ff78c8267c0dc697b8631f67bf7e8f
RUN curl --fail --location --silent --show-error \
        --output /tmp/brotli.tar.gz \
        "https://github.com/google/brotli/archive/refs/tags/v${BROTLI_VERSION}.tar.gz" && \
    echo "${BROTLI_SHA256}  /tmp/brotli.tar.gz" | sha256sum --check --status && \
    mkdir /tmp/brotli && \
    tar --extract --gzip --file /tmp/brotli.tar.gz --strip-components=1 --directory /tmp/brotli && \
    cmake -S /tmp/brotli -B /tmp/brotli-build \
        -DCMAKE_BUILD_TYPE=Release \
        -DBROTLI_BUILD_TOOLS=OFF \
        -DCMAKE_INSTALL_PREFIX=/usr/local && \
    cmake --build /tmp/brotli-build --parallel && \
    DESTDIR=/brotli-dist cmake --install /tmp/brotli-build && \
    printf '%s\n' \
        '#include <brotli/decode.h>' \
        '#include <stdio.h>' \
        'int main(void) { printf("%x %s\n", BrotliDecoderVersion(), BrotliDecoderErrorString(-17)); }' \
        > /tmp/brotli-api-check.c && \
    cc /tmp/brotli-api-check.c -I/tmp/brotli/c/include \
        -L/brotli-dist/usr/local/lib -Wl,-rpath,/brotli-dist/usr/local/lib \
        -lbrotlidec -o /tmp/brotli-api-check && \
    test "$(/tmp/brotli-api-check)" = "1002001 _ERROR_FORMAT_BLOCK_SWITCH" && \
    rm -rf /tmp/brotli /tmp/brotli-build /tmp/brotli.tar.gz /tmp/brotli-api-check /tmp/brotli-api-check.c

# Now copy actual source code
COPY src ./src

# Set environment for AVIF support
ENV SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always

# Build release binary with actual code (force rebuild by touching main.rs)
RUN touch src/main.rs && cargo build --release --locked

# Stage 2: Runtime
FROM debian:trixie-slim AS runtime

# Install runtime dependencies (FFmpeg for video thumbnails). No curl/wget: the
# healthcheck is the binary itself, so a compromised process finds no HTTP client.
# `upgrade` pulls trixie-security fixes the base image does not ship yet
# (e.g. OpenSSL CVE-2026-84782); CI rebuilds this stage uncached for the same reason.
RUN apt-get update && apt-get upgrade -y && apt-get install -y \
    ca-certificates \
    ffmpeg \
    && rm -rf /var/lib/apt/lists/*

# FFmpeg pulls libjxl, which dynamically resolves libbrotlidec. Replace the
# distro decoder files in the loader path with the ABI-compatible fixed build.
COPY --from=builder /brotli-dist/usr/local/lib/ /usr/local/lib/
RUN lib_dir="/lib/$(uname -m)-linux-gnu" && \
    rm -f "${lib_dir}"/libbrotli*.so* && \
    cp -a /usr/local/lib/libbrotli*.so* "${lib_dir}/" && \
    ldconfig

# Create non-root user
RUN useradd -m -u 1000 imgproxy

# No setuid/setgid binaries (su, mount, passwd, unix_chkpwd, ... in the Debian
# base). Even with every capability dropped they would hand a compromised
# process euid 0 over root-owned files. Stands in for no-new-privileges where
# the platform cannot set it (Coolify Docker-image apps).
RUN find / -xdev -perm /6000 -type f -exec chmod a-s {} +

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

# Health check. Follows BIND_ADDR's port, so a deployment overriding it
# (e.g. 0.0.0.0:8080) does not leave the container permanently unhealthy.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/rust-imgproxy", "--healthcheck"]

# Run the binary
CMD ["/usr/local/bin/rust-imgproxy"]

