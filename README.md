# rust-imgproxy

A minimal, fast image resizing service written in Rust, inspired by imgproxy. Supports filesystem-based caching with TTL cleanup.

## Features

- **Versioned signed URL API** with expiring HMAC capability URLs and a temporary imgproxy-compatible legacy API
- **Output formats**: JPEG, PNG, WebP, AVIF; input decoders are limited to JPEG, PNG, and WebP
- **Video thumbnails**: Extract thumbnails from videos using FFmpeg
- **Resize operations**: Fit, Fill, Fill-Down, Force, Auto (Lanczos3)
- **Quality control**: Configurable quality for lossy formats
- **Dual-cache architecture**: Original images + processed results
- **Filesystem cache**: SHA-256 keyed, with atomic writes
- **TTL-based cleanup**: Background janitor removes expired files
- **Environment-based config**: No config files needed
- **Concurrency control**: Semaphore-based limits for FFmpeg processes
- **CORS enabled**: `Access-Control-Allow-Origin: *` for all requests

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

For **video thumbnail support**, you need `ffmpeg` installed:

```bash
# macOS
brew install ffmpeg

# Ubuntu/Debian
sudo apt install ffmpeg

# Check installation
ffmpeg -version
```

### Build & Run

```bash
# Build (dav1d will be built automatically for AVIF support)
./build.sh

# Or manually with PATH set (if meson/ninja installed via pip --user)
export PATH="$HOME/Library/Python/3.9/bin:$PATH"  # macOS
export PATH="$HOME/.local/bin:$PATH"              # Linux
cargo build --release

# Run
./target/release/rust-imgproxy

# Or with cargo
./build.sh run --release
```

**Note**: 
- First build may take longer (1-2 minutes) as it compiles the dav1d decoder for AVIF support
- The `build.sh` script ensures `meson` and `ninja` are in PATH (required for AVIF support)
- Edit `build.sh` to adjust the PATH for your system if meson/ninja are installed elsewhere

### Docker

```bash
# Build image
docker build -t rust-imgproxy .

# Run container
docker run -p 8080:8080 -v $(pwd)/cache:/cache rust-imgproxy

# Or use docker-compose
docker-compose up -d

# Check logs
docker-compose logs -f

# Stop
docker-compose down
```

**Docker Features:**
- ✅ Multi-stage build (optimized image size)
- ✅ Non-root user for security
- ✅ FFmpeg included for video support
- ✅ Health check endpoint (`/health`)
- ✅ Volume mount for persistent cache
- ✅ All dependencies included (AVIF, WebP, etc.)

### Makefile (Optional)

Convenience commands for development:

```bash
make build              # Build release binary
make run                # Run locally
make docker-build       # Build Docker image
make docker-compose-up  # Start with docker-compose
make test-image         # Test with sample image
make test-health        # Check health endpoint
make cache-stats        # Show cache statistics
make clean              # Clean build artifacts and cache
```

## Example Requests

The `/insecure/` endpoint handles **both images and videos** automatically! Videos are detected by file extension and a thumbnail is extracted before resizing.

### Images

```bash
# Fill mode: Resize to fill 480x480, center crop, WebP format
curl "http://127.0.0.1:8080/insecure/f:webp/q:85/rs:fill:480:480/plain/https%3A%2F%2Fblossom.yakihonne.com%2Fimage.jpg"

# Fit mode: Resize to fit within 800x600, maintain aspect ratio
curl "http://127.0.0.1:8080/insecure/f:jpeg/q:90/rs:fit:800:600/plain/https%3A%2F%2Fblossom.yakihonne.com%2Fphoto.png"

# Resize by height only (width calculated from aspect ratio)
curl "http://127.0.0.1:8080/insecure/f:webp/rs:fit::600/plain/https%3A%2F%2Fexample.com%2Fimage.jpg"

# Resize by width only (height calculated from aspect ratio)
curl "http://127.0.0.1:8080/insecure/f:jpeg/rs:fit:800:/plain/https%3A%2F%2Fexample.com%2Fimage.jpg"

# Force mode: Resize to exact 300x200 (ignore aspect ratio)
curl "http://127.0.0.1:8080/insecure/rt:force:300:200/plain/https%3A%2F%2Fexample.com%2Fimage.jpg"

# Auto mode: Automatically choose fill or fit based on orientation
curl "http://127.0.0.1:8080/insecure/f:avif/q:80/rs:auto:1024:768/plain/https%3A%2F%2Fexample.com%2Fimage.jpg"
```

### Videos (Automatic Thumbnail Extraction!)

```bash
# Same API! Just use a video URL - thumbnail is automatically extracted and resized
curl "http://127.0.0.1:8080/insecure/f:webp/rs:fit:400:400/plain/https%3A%2F%2Fcommondatastorage.googleapis.com%2Fgtv-videos-bucket%2Fsample%2FBigBuckBunny.mp4" -o video_thumb.webp

# Different sizes from the same video (thumbnail cached, resizing fast!)
curl "http://127.0.0.1:8080/insecure/f:webp/rs:fill:200:200/plain/https%3A%2F%2Fexample.com%2Fvideo.mp4" -o thumb_small.webp
curl "http://127.0.0.1:8080/insecure/f:jpeg/rs:fit:800:600/plain/https%3A%2F%2Fexample.com%2Fvideo.mp4" -o thumb_large.jpg
```

**Supported video formats:** `.mp4`, `.mov`, `.avi`, `.webm`, `.mkv`, `.flv`, `.wmv`, `.m4v`, `.mpg`, `.mpeg`, `.3gp`, `.ogv`

### URL Structure

Legacy URLs are enabled only while `ALLOW_UNSIGNED_URLS=true`:

```text
/insecure/<directives>/plain/<percent-encoded-source-url>
/thumb/<sha256>[.<extension>]?f=...&rs=...&q=...&xs=...&as=...
```

Signed v1 URLs are the production API:

```text
/v1/<key-id>/<signature>/img/<directives>/plain/<percent-encoded-source-url>?exp=<unix-seconds>
/v1/<key-id>/<signature>/thumb/<sha256>[.<extension>]?f=...&rs=...&q=...&xs=...&as=...&exp=<unix-seconds>
```

The signature covers the exact raw path and query after `/v1/<key-id>/<signature>`. A browser obtains these URLs through the image proxy's public mint endpoint; it never receives an HMAC key. See [`docs/nostube-signed-url-spec.md`](docs/nostube-signed-url-spec.md) for the batch contract and rolling migration plan.

### Batch Minting

When enabled, `POST /v1/mint` accepts an unauthenticated JSON batch of hash-addressed Blossom media plus one fixed output preset. It returns the corresponding expiring v1 URLs. The mint route is deliberately not a general remote-URL proxy: direct `source_url`, arbitrary directives, and source-server hints are rejected. It is safe to leave unauthenticated because it only ever mints URLs for already-public, hash-addressed Blossom media behind a handful of fixed presets — the same bytes anyone can already fetch straight from a Blossom server. Admission is a per-IP flood guard, not an authorization check, which keeps the endpoint usable by anonymous browsers, embeds, and crawlers.

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

**Video Handling:**
- Detected by direct-container extension; HLS/DASH playlists are intentionally unsupported
- FFmpeg extracts one frame at 0.5 seconds through a loopback-only, range-aware media gateway
- The gateway preserves range seeking for large videos while enforcing source URL policy, timeout, and transfer budget
- Video thumbnails are not disk-cached under a Blossom hash unless the complete source can be hash-verified
- The thumbnail is processed like a regular image (resize and encode)

## Configuration

Configure via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `BIND_ADDR` | `127.0.0.1:8080` | Server bind address |
| `CACHE_DIR` | `./cache` | Cache directory path |
| `CACHE_TTL_SECS` | `86400` (24h) | Cache TTL in seconds |
| `FETCH_TIMEOUT_SECS` | `10` | HTTP fetch timeout |
| `BLOSSOM_NEGATIVE_CACHE_NOT_FOUND_TTL_SECS` | `900` (15m) | Cache 404/410 Blossom candidates; `0` disables this class |
| `BLOSSOM_NEGATIVE_CACHE_PERMANENT_TTL_SECS` | `3600` (1h) | Cache 3xx and non-transient 4xx Blossom candidates; `0` disables this class |
| `BLOSSOM_NEGATIVE_CACHE_TRANSIENT_TTL_SECS` | `60` | Cache timeouts, transport failures, 429, and 5xx Blossom candidates; `0` disables this class |
| `MAX_IMAGE_BYTES` | `16777216` (16 MiB) | Max compressed image or generated thumbnail size |
| `MAX_VIDEO_PROBE_BYTES` | `67108864` (64 MiB) | Total remote bytes the local media gateway may relay for one thumbnail; not a full-video size cap |
| `MAX_FFMPEG_CONCURRENT` | `8` | Max concurrent FFmpeg processes; excess requests are shed with `503 Retry-After` |
| `MAX_CPU_QUEUE` | `64` | Additional image decode/resize/encode jobs admitted to wait for CPU |
| `MAX_CACHE_BYTES` | `8589934592` (8 GiB) | Disk budget shared by original and processed caches |
| `MAX_BLOB_CANDIDATES` | `8` | Maximum Blossom source candidates tried per request |
| `MAX_SERVER_HINTS` | `4` | Maximum `xs=` hints honoured per request |
| `METRICS_BIND_ADDR` | unset | Optional separate bind address for the operator-only `/metrics` listener |
| `URL_SIGNING_KEYS` | unset | Comma-separated `key-id:base64url-secret` HMAC keys; secrets must decode to at least 32 bytes |
| `ALLOW_UNSIGNED_URLS` | `true` | Temporary migration switch for legacy `/insecure` and `/thumb` routes |
| `REQUIRE_SIGNED_URL_EXPIRY` | `true` | Require one signed `exp` Unix-seconds query parameter |
| `MINT_ENABLED` | `false` | Enable the public `POST /v1/mint` endpoint |
| `MINT_PUBLIC_BASE_URL` | unset | Required canonical image-proxy origin used to construct returned URLs |
| `MINT_ALLOWED_ORIGINS` | unset | Comma-separated browser origins allowed to call `/v1/mint` |
| `MAX_MINT_BATCH_ITEMS` | `100`, capped at 100 | Maximum hash-addressed items per mint request |
| `MINT_RATE_IP_ITEMS_PER_MIN` | `300` | Minted-item budget per peer IP and minute |
| `SIGNED_URL_TTL_SECS` | `21600` | Lifetime for minted signed URLs; expiry is bucketed for cache reuse |
| `RATE_IP_REQUESTS_PER_MIN` | `600` | Per-IP budget across every image/thumb request, cache hit or miss |
| `RATE_IP_IMAGE_GENERATIONS_PER_MIN` | `30` | Per-IP budget for cache-miss image decode/resize/encode work |
| `RATE_IP_VIDEO_GENERATIONS_PER_MIN` | `5` | Per-IP budget for cache-miss FFmpeg video-thumbnail work |
| `RUST_LOG` | `info` | Log level |

Blossom candidate failures are retained only in memory, per candidate URL, up to 10,000 entries.
Repeated requests skip an unexpired failed URL but still try newly supplied or discovered candidates.

Example:

```bash
BIND_ADDR=0.0.0.0:3000 CACHE_TTL_SECS=3600 MAX_FFMPEG_CONCURRENT=20 cargo run --release
```

### FFmpeg Concurrency Control

The service uses a semaphore to cap concurrent FFmpeg processes. Excess
requests are rejected with `503 Retry-After` rather than accumulating an
unbounded queue. Each process also has wall-clock, address-space, CPU-time,
file-size, stderr, and generated-thumbnail limits.

**Example scenario:**
- 15 video requests arrive simultaneously
- First 8 acquire FFmpeg permits
- Excess work is rejected promptly with `503 Retry-After`
- The client retries rather than forcing the service to retain unbounded work

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
├── server.rs     # HTTP server and route handlers (unified image/video handling)
├── transform.rs  # Image transformation logic (resize, encode, parse)
├── thumbnail.rs  # Video thumbnail extraction (FFmpeg integration)
└── cache.rs      # Cache operations (read, write, cleanup)
```

## Cache Behavior

The service uses a **dual-cache architecture** for optimal performance:

### Cache Structure

```
cache/
├── original/   # Validated image sources
└── processed/  # Canonically keyed derivatives
```

### Original Cache
- **Purpose**: Prevents redundant downloads and processing of validated images
- **Key**: SHA-256 hash of the source URL or canonical Blossom blob name
- **Content**: Downloaded original image after successful decode/transform
- **Video exception**: Range-probed video thumbnails are not stored here because the full video hash is not available without a full download

### Processed Cache
- **Purpose**: Serves previously transformed, validated images instantly
- **Key**: SHA-256 of canonical parsed directives, source identity, output format, quality, and resize mode
- **Sharding**: Two digest-prefix directory levels prevent oversized cache directories
- **Benefit**: Equivalent URLs with ignored or normalized directives share one entry

### General Cache Properties
- **Atomic writes**: Create-new temporary files, `fsync`, then rename; zero-byte entries are misses
- **Cleanup**: Runs every 60 seconds, applies `CACHE_TTL_SECS`, and evicts oldest entries above `MAX_CACHE_BYTES`
- **Cache headers**: Hash-verified image derivatives are immutable; `/insecure` and range-probed video responses are short-lived
- **Hit/Miss indicator**: `X-Cache: hit`, `miss`, or `coalesced`

## Dependencies

- **axum** - Web framework
- **tokio** - Async runtime
- **reqwest** - HTTP client
- **image** - Image decoding/encoding
- **webp** - WebP encoding
- **ravif** - AVIF encoding
- **sha2** - Cache key hashing

## Build Notes

- **AVIF output** is encoded by `ravif`.
- **AVIF input** is intentionally unsupported: only PNG, JPEG, and WebP are accepted source formats.

- **Video Support**: Requires a system `ffmpeg` binary in `PATH`.
  - FFmpeg is called as an external process through a loopback-only media gateway.
  - The gateway relays bounded HTTP range requests; no Rust FFmpeg bindings are required.
  - Thumbnail extraction seeks to 0.5s, emits one WebP frame, and caps output to 1280×720.
  - Configure concurrent processes with `MAX_FFMPEG_CONCURRENT` and gateway transfer work with `MAX_VIDEO_PROBE_BYTES`.

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

