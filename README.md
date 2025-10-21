# rust-imgproxy

A minimal, fast image resizing service written in Rust, inspired by imgproxy. Supports filesystem-based caching with TTL cleanup.

## Features

- **imgproxy-compatible URL API** (insecure mode)
- **Full format support**: JPEG, PNG, WebP, AVIF (input and output)
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

```
/insecure/<directives>/plain/<percent-encoded-source-url>
```

Works for **both images and videos**! Videos are automatically detected by file extension.

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
- Detected by file extension (`.mp4`, `.mov`, `.webm`, etc.)
- Thumbnail extracted at 0.5 seconds using FFmpeg
- Thumbnail cached in `cache/original/` (subsequent requests reuse it)
- Then processed like a regular image (resize, encode, cache in `cache/processed/`)

## Configuration

Configure via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `BIND_ADDR` | `127.0.0.1:8080` | Server bind address |
| `CACHE_DIR` | `./cache` | Cache directory path |
| `CACHE_TTL_SECS` | `86400` (24h) | Cache TTL in seconds |
| `FETCH_TIMEOUT_SECS` | `10` | HTTP fetch timeout |
| `MAX_IMAGE_BYTES` | `16777216` (16 MiB) | Max image size |
| `MAX_FFMPEG_CONCURRENT` | `8` | Max concurrent FFmpeg processes (requests wait if limit reached) |
| `RUST_LOG` | `info` | Log level |

Example:

```bash
BIND_ADDR=0.0.0.0:3000 CACHE_TTL_SECS=3600 MAX_FFMPEG_CONCURRENT=20 cargo run --release
```

### FFmpeg Concurrency Control

The service uses a **Semaphore pattern** to limit concurrent FFmpeg processes:

- **Default limit**: 8 concurrent FFmpeg processes
- **When limit reached**: Additional video requests wait in queue (non-blocking async)
- **Automatic**: Permits are automatically released when processing completes
- **Prevents**: Resource exhaustion (CPU/memory) under heavy video load
- **Image requests**: Not affected by FFmpeg limit (only video thumbnail extraction)

**Example scenario:**
- 15 video requests arrive simultaneously
- First 8 start FFmpeg immediately
- Remaining 7 wait in queue
- As FFmpeg processes complete, waiting requests proceed
- Total server capacity: Limited only by system resources + configured limits

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
├── original/   # Downloaded source images (raw)
└── processed/  # Transformed images (by request URL)
```

### Original Cache
- **Purpose**: Prevents redundant downloads/processing of source media
- **Key**: SHA-256 hash of source URL
- **Content**: 
  - For images: Downloaded original image
  - For videos: Extracted thumbnail (WebP, max 720p)
- **Benefit**: Multiple transformations of the same source only process once

### Processed Cache
- **Purpose**: Serves previously transformed images instantly
- **Key**: SHA-256 hash of the full request path (includes all directives)
- **Format**: Includes file extension based on output format
- **Benefit**: Same URL with same parameters = instant response

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

- **Video Support**: Requires system `ffmpeg` binary
  - Videos are automatically detected by file extension
  - FFmpeg is called as an external process via `std::process::Command`
  - No Rust FFmpeg bindings required (avoids complex build dependencies)
  - Make sure `ffmpeg` is in your PATH
  - Thumbnail extraction: seeks to 0.5s, max 720p height, WebP output with quality 80
  - **Concurrency Control**: Semaphore limits simultaneous FFmpeg processes (default: 10)
    - When limit is reached, additional requests wait in queue (non-blocking)
    - Prevents resource exhaustion under high video load
    - Configure via `MAX_FFMPEG_CONCURRENT` environment variable

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

