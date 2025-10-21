# rust-imgproxy

A minimal, fast image resizing service written in Rust, inspired by imgproxy. Supports filesystem-based caching with TTL cleanup.

## Features

- **imgproxy-compatible URL API** (insecure mode)
- **Multiple output formats**: JPEG, PNG, WebP, AVIF
- **Resize operations**: Fit, Fill, Fill-Down, Force, Auto (Lanczos3)
- **Quality control**: Configurable quality for lossy formats
- **Filesystem cache**: SHA-256 keyed, with atomic writes
- **TTL-based cleanup**: Background janitor removes expired files
- **Environment-based config**: No config files needed

## Quick Start

```bash
# Build
cargo build --release

# Run
cargo run --release

# Or run the binary directly
./target/release/rust-imgproxy
```

## Example Requests

```bash
# Fill mode: Resize to fill 480x480, center crop, WebP format
curl "http://127.0.0.1:8080/insecure/f:webp/q:85/rs:fill:480:480/plain/https%3A%2F%2Fexample.com%2Fimage.jpg"

# Fit mode: Resize to fit within 800x600, maintain aspect ratio
curl "http://127.0.0.1:8080/insecure/f:jpeg/q:90/rs:fit:800:600/plain/https%3A%2F%2Fexample.com%2Fimage.jpg"

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

- **Cache key**: SHA-256 hash of the full request path
- **Atomic writes**: Uses temp files + rename for safety
- **TTL cleanup**: Runs every 60 seconds, removes files older than `CACHE_TTL_SECS`
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

## Roadmap

Future enhancements:

- [ ] Signed URLs (HMAC verification)
- [ ] Additional resize modes (`fit`, `auto`)
- [ ] DPR support
- [ ] Background color for transparent images
- [ ] Gravity/crop position control
- [ ] In-memory cache (moka)
- [ ] ETag/Conditional GET support
- [ ] Request deduplication/locking

## License

MIT

