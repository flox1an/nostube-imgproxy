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

Signed v1 URLs remain available for trusted, arbitrary-directive use (e.g. an internal tool minting a one-off crop) once a signing key is configured:

```text
/v1/<key-id>/<signature>/img/<directives>/plain/<percent-encoded-source-url>?exp=<unix-seconds>
/v1/<key-id>/<signature>/thumb/<sha256>[.<extension>]?f=...&rs=...&q=...&xs=...&as=...&exp=<unix-seconds>
```

The signature covers the exact raw path and query after `/v1/<key-id>/<signature>`. Producing a signed URL requires the HMAC secret, so it is never done in a browser.

### Preset Thumbnails

```text
/v1/preset/<preset>/<sha256>[.<extension>]?xs=...&as=...
```

```bash
curl "http://127.0.0.1:8080/v1/preset/feed-preview-v1/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.webp"
```

This is the production API for browser clients such as Nostube. It is unauthenticated by design, and safe to leave that way: `<preset>` selects one of a small, published set of fixed output shapes (below), and `<sha256>` is a Blossom content hash — both are already public, so there is nothing to authorize. A client builds this URL directly; there is no signing or minting round-trip. Admission is a per-IP tiered rate limit (see `RATE_IP_*` below), not an authorization check, which keeps the route usable by anonymous browsers, embeds, and crawlers.

**Presets:**
- `feed-preview-v1`: WebP, quality 82, fit 480×480
- `profile-avatar-v1`: WebP, quality 85, fill 160×160
- `embed-card-v1`: WebP, quality 82, fit 1200×630

No other directive may be supplied on this route: `f`, `rs`, `q`, `width`, and `height` query parameters are rejected outright (`400`) rather than silently ignored. Only `xs=` (server hints) and `as=` (author pubkey) are accepted, for Blossom server discovery. See [`docs/nostube-preset-thumbnails-spec.md`](docs/nostube-preset-thumbnails-spec.md) for the Nostube client integration contract.

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
| `CACHE_TTL_SECS` | `86400` (24h) | TTL for URL-addressed (`/insecure`) cache entries |
| `CACHE_TTL_IMMUTABLE_SECS` | `2592000` (30d) | TTL for hash-verified (`thumb`) cache entries; these cannot go stale behind their key, so `MAX_CACHE_BYTES` is the real bound |
| `FETCH_TIMEOUT_SECS` | `10` | HTTP fetch timeout |
| `BLOSSOM_NEGATIVE_CACHE_NOT_FOUND_TTL_SECS` | `900` (15m) | Cache 404/410 Blossom candidates; `0` disables this class |
| `BLOSSOM_NEGATIVE_CACHE_PERMANENT_TTL_SECS` | `3600` (1h) | Cache 3xx and non-transient 4xx Blossom candidates; `0` disables this class |
| `BLOSSOM_NEGATIVE_CACHE_TRANSIENT_TTL_SECS` | `60` | Cache timeouts, transport failures, 429, and 5xx Blossom candidates; `0` disables this class |
| `MAX_IMAGE_BYTES` | `16777216` (16 MiB) | Max compressed image or generated thumbnail size |
| `MAX_VIDEO_PROBE_BYTES` | `67108864` (64 MiB) | Total remote bytes the local media gateway may relay for one thumbnail; not a full-video size cap. The only video budget on the request path |
| `MAX_VERIFY_VIDEO_BYTES` | `33554432` (32 MiB) | Ceiling on a background verification's full blob download; larger videos stay uncacheable and keep being range-probed |
| `VIDEO_VERIFY_AFTER_MISSES` | `2` | Range-probed misses one video must accumulate before a single background hash-verification runs. Above `1`, a video thumbnailed once never costs a full download |
| `MAX_CONCURRENT_VIDEO_VERIFICATIONS` | `2` | Simultaneous background video verifications |
| `MAX_FFMPEG_CONCURRENT` | `8` | Max concurrent FFmpeg processes; excess requests are shed with `503 Retry-After` |
| `MAX_CPU_QUEUE` | `64` | Additional image decode/resize/encode jobs admitted to wait for CPU |
| `MAX_CACHE_BYTES` | `8589934592` (8 GiB) | Disk budget shared by original and processed caches |
| `MAX_BLOB_CANDIDATES` | `8` | Maximum Blossom source candidates tried per request |
| `MAX_SERVER_HINTS` | `4` | Maximum `xs=` hints honoured per request |
| `METRICS_BIND_ADDR` | unset | Optional separate bind address for the operator-only `/metrics` listener |
| `URL_SIGNING_KEYS` | unset | Comma-separated `key-id:base64url-secret` HMAC keys; secrets must decode to at least 32 bytes |
| `ALLOW_UNSIGNED_URLS` | `true` | Temporary migration switch for legacy `/insecure` and `/thumb` routes |
| `REQUIRE_SIGNED_URL_EXPIRY` | `true` | Require one signed `exp` Unix-seconds query parameter |
| `PRESET_THUMBNAILS_ENABLED` | `true` | Enable the unsigned `GET /v1/preset/{preset}/{filename}` route |
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
├── verify.rs     # Background hash-verification gating for video blobs
└── cache.rs      # Cache operations (read, write, cleanup)
```

## Cache Behavior

The service uses a **dual-cache architecture** for optimal performance:

### Cache Structure

```
cache/
├── original/
│   ├── thumb/     # Hash-verified Blossom sources      → CACHE_TTL_IMMUTABLE_SECS
│   └── insecure/  # URL-addressed sources              → CACHE_TTL_SECS
└── processed/
    ├── thumb/     # Hash-verified derivatives          → CACHE_TTL_IMMUTABLE_SECS
    └── insecure/  # URL-addressed derivatives          → CACHE_TTL_SECS
```

The `thumb` namespace only ever receives bytes that were hash-verified
against the requested SHA-256, so those entries cannot go stale behind their
key and earn a much longer TTL. The `insecure` namespace is URL-addressed —
the bytes behind a URL can change — so it keeps the short TTL.

### Original Cache
- **Purpose**: Prevents redundant downloads and processing of validated sources
- **Key**: SHA-256 hash of the source URL or canonical Blossom blob name
- **Content**: Downloaded original image, or the extracted frame for a video
- **Preset-agnostic**: One entry serves every size, format, and quality of that source

### Processed Cache
- **Purpose**: Serves previously transformed, validated images instantly
- **Key**: SHA-256 of canonical parsed directives, source identity, output format, quality, and resize mode
- **Sharding**: Two digest-prefix directory levels prevent oversized cache directories
- **Benefit**: Equivalent URLs with ignored or normalized directives share one entry

### Video Thumbnails
A thumbnail needs a few seconds of footage near one keyframe, so the request
path only ever range-probes — it never downloads a full video. Proving those
bytes match the requested SHA-256, however, requires the whole blob, so that
happens **off the request path**:

1. First requests are served from a range probe: `max-age=3600`, no `ETag`, nothing cached.
2. Once a video accumulates `VIDEO_VERIFY_AFTER_MISSES` misses, one bounded background job downloads it in full, checks the hash, extracts the frame, and writes the original cache entry.
3. Later requests find that verified original and are served `immutable` with an `ETag`, at any preset.

A video requested only once therefore never costs a full download, and a video
above `MAX_VERIFY_VIDEO_BYTES` is never fully downloaded at all — it keeps
being range-probed per request.

### General Cache Properties
- **Atomic writes**: Create-new temporary files, `fsync`, then rename; zero-byte entries are misses
- **Cleanup**: Runs every 60 seconds, applies the per-namespace TTL, and evicts oldest entries above `MAX_CACHE_BYTES`
- **Cache headers**: Hash-verified derivatives are immutable; `/insecure` and not-yet-verified video responses are short-lived and carry no `ETag`
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

