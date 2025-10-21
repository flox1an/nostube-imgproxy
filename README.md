# rust-imgproxy

A minimal, fast image resizing service written in Rust, inspired by imgproxy. Supports filesystem-based caching with TTL cleanup.

## Features

- **imgproxy-compatible URL API** (insecure mode)
- **Full format support**: JPEG, PNG, WebP, AVIF (input and output)
- **Resize operations**: Fit, Fill, Fill-Down, Force, Auto (Lanczos3)
- **Quality control**: Configurable quality for lossy formats
- **Dual-cache architecture**: Original images + processed results
- **Filesystem cache**: SHA-256 keyed, with atomic writes
- **TTL-based cleanup**: Background janitor removes expired files
- **Environment-based config**: No config files needed

## Quick Start

### Prerequisites

For **AVIF support** (both input and output), you need `meson` and `ninja` installed:

```bash
# macOS/Linux
pip3 install --user meson ninja

# Or via package manager
brew install meson ninja  # macOS
sudo apt install meson ninja-build  # Ubuntu/Debian
```

### Build & Run

```bash
# Build (dav1d will be built automatically for AVIF support)
cargo build --release

# Run
cargo run --release

# Or run the binary directly
./target/release/rust-imgproxy
```

**Note**: First build may take longer (1-2 minutes) as it compiles the dav1d decoder for AVIF support.

## Example Requests

```bash
# Fill mode: Resize to fill 480x480, center crop, WebP format
curl "http://127.0.0.1:8080/insecure/f:webp/q:85/rs:fill:480:480/plain/https%3A%2F%2Fexample.com%2Fimage.jpg"

# Fit mode: Resize to fit within 800x600, maintain aspect ratio
curl "http://127.0.0.1:8080/insecure/f:jpeg/q:90/rs:fit:800:600/plain/https%3A%2F%2Fexample.com%2Fimage.jpg"

# Resize by height only (width calculated from aspect ratio)
curl "http://127.0.0.1:8080/insecure/f:webp/rs:fit::600/plain/https%3A%2F%2Fexample.com%2Fimage.jpg"

# Resize by width only (height calculated from aspect ratio)
curl "http://127.0.0.1:8080/insecure/f:jpeg/rs:fit:800:/plain/https%3A%2F%2Fexample.com%2Fimage.jpg"

# Force mode: Resize to exact 300x200 (ignore aspect ratio)
curl "http://127.0.0.1:8080/insecure/rt:force:300:200/plain/https%3A%2F%2Fexample.com%2Fimage.jpg"

# Auto mode: Automatically choose fill or fit based on orientation
curl "http://127.0.0.1:8080/insecure/f:avif/q:80/rs:auto:1024:768/plain/https%3A%2F%2Fexample.com%2Fimage.jpg"
```

### URL Structure

```
/insecure/<directives>/plain/<percent-encoded-source-url>
```

**Supported Directives:**
- `f:<format>` - Output format: `jpeg`, `png`, `webp`, `avif`
- `q:<0-100>` - Quality for lossy formats (default: 82)
- `rs:<mode>:<width>:<height>` or `rt:<mode>:<width>:<height>` - Resize operation
  - Width or height can be omitted (but not both) to calculate from aspect ratio
  - Examples: `rs:fit:800:600`, `rs:fit::600` (height only), `rs:fit:800:` (width only)
  - **Modes:**
    - `fit` - Resize to fit within dimensions (maintains aspect ratio, no crop, default)
    - `fill` - Resize to fill dimensions (maintains aspect ratio, center crop)
    - `fill-down` - Like fill but doesn't upscale; crops if smaller
    - `force` - Resize to exact dimensions (ignores aspect ratio)
    - `auto` - Automatically choose fill or fit based on orientation

## Configuration

Configure via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `BIND_ADDR` | `127.0.0.1:8080` | Server bind address |
| `CACHE_DIR` | `./cache` | Cache directory path |
| `CACHE_TTL_SECS` | `86400` (24h) | Cache TTL in seconds |
| `FETCH_TIMEOUT_SECS` | `10` | HTTP fetch timeout |
| `MAX_IMAGE_BYTES` | `16777216` (16 MiB) | Max image size |
| `RUST_LOG` | `info` | Log level |

Example:

```bash
BIND_ADDR=0.0.0.0:3000 CACHE_TTL_SECS=3600 cargo run --release
```

## Resize Modes Explained

| Mode | Behavior | Upscale? | Crop? | Use Case |
|------|----------|----------|-------|----------|
| **fit** | Fits within dimensions, maintains aspect ratio | No | No | Thumbnails, previews (default) |
| **fill** | Fills dimensions, maintains aspect ratio | Yes | Yes (center) | Exact size needed, e.g., avatars |
| **fill-down** | Like fill but never upscales | No | Yes | Smaller images, maintain quality |
| **force** | Exact dimensions, ignores aspect ratio | Yes | No | Specific dimensions required |
| **auto** | Smart choice based on orientation | Depends | Depends | General purpose |

## Project Structure

```
src/
├── main.rs       # Entry point and initialization
├── config.rs     # Configuration and app state
├── error.rs      # Error types and IntoResponse impl
├── server.rs     # HTTP server and route handlers
├── transform.rs  # Image transformation logic (resize, encode, parse)
└── cache.rs      # Cache operations (read, write, cleanup)
```

## Cache Behavior

The service uses a **dual-cache architecture** for optimal performance:

### Cache Structure

```
cache/
├── original/   # Downloaded source images (raw)
└── processed/  # Transformed images (by request URL)
```

### Original Image Cache
- **Purpose**: Prevents redundant downloads of the same source image
- **Key**: SHA-256 hash of source URL
- **Benefit**: Multiple transformations of the same image only download once

### Processed Image Cache
- **Purpose**: Serves previously transformed images instantly
- **Key**: SHA-256 hash of the full request path (includes all directives)
- **Format**: Includes file extension based on output format

### General Cache Properties
- **Atomic writes**: Uses temp files + rename for safety
- **TTL cleanup**: Runs every 60 seconds, removes files older than `CACHE_TTL_SECS` from both caches
- **Cache headers**: `Cache-Control: public, max-age=3600, stale-while-revalidate=600`
- **Hit/Miss indicator**: `X-Cache: hit` or `X-Cache: miss`

## Dependencies

- **axum** - Web framework
- **tokio** - Async runtime
- **reqwest** - HTTP client
- **image** - Image decoding/encoding
- **webp** - WebP encoding
- **ravif** - AVIF encoding
- **sha2** - Cache key hashing

## Build Notes

- **AVIF Support**: Requires `meson` and `ninja` to build the dav1d decoder
  - First build takes ~1-2 minutes as it compiles dav1d from source
  - Subsequent builds are fast (incremental compilation)
  - The `.cargo/config.toml` file sets `SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always` to build dav1d automatically

## Roadmap

Future enhancements:

- [ ] Signed URLs (HMAC verification)
- [ ] DPR support
- [ ] Background color for transparent images
- [ ] Gravity/crop position control
- [ ] In-memory cache (moka)
- [ ] ETag/Conditional GET support
- [ ] Request deduplication/locking
- [ ] Blur, sharpen, and other filters

## License

MIT

