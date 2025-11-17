# CLAUDE.md - rust-imgproxy

## Project Overview

**rust-imgproxy** is a minimal, fast image resizing service written in Rust, inspired by imgproxy. It provides an imgproxy-compatible URL API (insecure mode) for on-the-fly image and video thumbnail processing.

### Core Functionality
- Image resizing and format conversion (JPEG, PNG, WebP, AVIF)
- Video thumbnail extraction using FFmpeg
- Dual-cache architecture (original + processed)
- TTL-based cache cleanup
- Concurrent request handling with semaphore-based FFmpeg limits

## Architecture

### Project Structure

```
src/
├── main.rs       # Entry point and initialization
├── config.rs     # Configuration and app state
├── error.rs      # Error types and IntoResponse impl
├── server.rs     # HTTP server and route handlers (unified image/video handling)
├── transform.rs  # Image transformation logic (resize, encode, parse)
├── thumbnail.rs  # Video thumbnail extraction (FFmpeg integration)
├── cache.rs      # Cache operations (read, write, cleanup)
└── metrics.rs    # Prometheus metrics collection and export
```

### Key Components

#### 1. Server (server.rs)
- Axum-based HTTP server
- Single unified endpoint: `/insecure/<directives>/plain/<url>`
- Handles both images and videos automatically
- Video detection by file extension
- CORS enabled for all requests

#### 2. Transform (transform.rs)
- Image decoding (multiple formats)
- Resize operations: fit, fill, fill-down, force, auto
- Lanczos3 resampling for high quality
- Format encoding with quality control
- URL parsing and directive extraction

#### 3. Thumbnail (thumbnail.rs)
- FFmpeg-based video thumbnail extraction
- Semaphore-controlled concurrency (default: 8 concurrent processes)
- Extracts frame at 0.5s, max 720p height
- WebP output with quality 80
- Automatic permit management

#### 4. Cache (cache.rs)
- Dual-cache system:
  - `cache/original/` - Downloaded source media (keyed by source URL hash)
  - `cache/processed/` - Transformed images (keyed by request path hash)
- SHA-256 hashing for keys
- Atomic writes using temp files + rename
- TTL-based cleanup (runs every 60s)
- Cache headers: `Cache-Control: public, max-age=31536000, immutable` (1 year, indefinite browser caching)

#### 5. Config (config.rs)
- Environment-based configuration
- App state management with Arc
- FFmpeg semaphore initialization

#### 6. Error (error.rs)
- Custom error types with thiserror
- IntoResponse implementation for HTTP error responses
- Proper HTTP status codes

#### 7. Metrics (metrics.rs)
- Prometheus metrics collection and export
- HTTP request metrics (total requests, duration histograms)
- Cache metrics (hits/misses by cache type)
- Processing metrics (images/videos processed by format)
- FFmpeg semaphore metrics (permits available, waiters)
- Bytes transferred metrics (downloaded/served)
- Error tracking by error type
- Metrics endpoint: `/metrics` (Prometheus text format)

## Technology Stack

### Core Dependencies
- **axum** (0.8) - Web framework
- **tokio** (1.x) - Async runtime
- **tower-http** - CORS middleware
- **reqwest** (0.12) - HTTP client with rustls
- **image** (0.25) - Image processing with AVIF support
- **webp** (0.3) - WebP encoding
- **ravif** (0.12) - AVIF encoding
- **sha2** (0.10) - Cache key hashing
- **tracing** - Structured logging
- **prometheus** (0.13) - Metrics collection and export
- **lazy_static** (1.4) - Global metrics initialization

### Build Requirements
- **meson** and **ninja** - Required for AVIF support (dav1d decoder)
- **ffmpeg** - Required for video thumbnail support (system binary)

## Configuration

All configuration is via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `BIND_ADDR` | `127.0.0.1:8080` | Server bind address |
| `CACHE_DIR` | `./cache` | Cache directory path |
| `CACHE_TTL_SECS` | `86400` (24h) | Cache TTL in seconds |
| `FETCH_TIMEOUT_SECS` | `10` | HTTP fetch timeout |
| `MAX_IMAGE_BYTES` | `16777216` (16 MiB) | Max image size |
| `MAX_FFMPEG_CONCURRENT` | `8` | Max concurrent FFmpeg processes |
| `RUST_LOG` | `info` | Log level (trace, debug, info, warn, error) |

## Development Workflow

### Building

```bash
# Using build script (ensures meson/ninja in PATH)
./build.sh

# Or manually
cargo build --release

# First build takes longer (compiles dav1d for AVIF)
```

### Running

```bash
# Using build script
./build.sh run --release

# Or directly
cargo run --release

# With custom config
BIND_ADDR=0.0.0.0:3000 CACHE_TTL_SECS=3600 cargo run --release
```

### Testing

```bash
# Health check
curl http://127.0.0.1:8080/health

# Prometheus metrics
curl http://127.0.0.1:8080/metrics

# Test image resize
curl "http://127.0.0.1:8080/insecure/f:webp/q:85/rs:fill:480:480/plain/https%3A%2F%2Fexample.com%2Fimage.jpg" -o test.webp

# Test video thumbnail
curl "http://127.0.0.1:8080/insecure/f:webp/rs:fit:400:400/plain/https%3A%2F%2Fexample.com%2Fvideo.mp4" -o thumb.webp

# Cache stats (via Makefile)
make cache-stats
```

### Docker

```bash
# Build and run
docker build -t rust-imgproxy .
docker run -p 8080:8080 -v $(pwd)/cache:/cache rust-imgproxy

# Or use docker-compose
docker-compose up -d
docker-compose logs -f
```

## Common Tasks

### Adding a New Resize Mode
1. Add enum variant to `ResizeMode` in `transform.rs`
2. Implement resize logic in `apply_resize()` function
3. Update URL parsing in `parse_params()`
4. Add tests and documentation

### Adding a New Output Format
1. Add enum variant to `OutputFormat` in `transform.rs`
2. Implement encoding in `encode_image()` function
3. Update MIME type mapping
4. Update URL parsing
5. Test with various input formats

### Adding a New Directive
1. Define directive struct/enum in `transform.rs`
2. Parse directive in `parse_params()`
3. Apply directive in image processing pipeline
4. Update documentation

### Modifying Cache Behavior
- Cache operations are in `cache.rs`
- Keys are SHA-256 hashes (see `cache_key()`)
- Cleanup logic in `cleanup_cache_task()`
- Atomic writes ensure consistency

### Adjusting FFmpeg Behavior
- Thumbnail extraction in `thumbnail.rs`
- Semaphore limit: `MAX_FFMPEG_CONCURRENT` env var
- FFmpeg command args in `extract_video_thumbnail()`
- Supported extensions in `is_video_url()`

## Important Notes

### AVIF Support
- First build takes 1-2 minutes (compiles dav1d from source)
- Requires meson and ninja installed
- `.cargo/config.toml` sets `SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always`
- Subsequent builds are fast (incremental)

### Video Thumbnail Support
- Requires system `ffmpeg` binary in PATH
- No Rust FFmpeg bindings (avoids complex build deps)
- External process via `std::process::Command`
- Concurrency controlled by semaphore (prevents resource exhaustion)
- When limit reached, requests wait in queue (non-blocking async)

### Cache Architecture
- **Dual cache** prevents redundant downloads and processing
- Original cache: One download per unique source URL
- Processed cache: One transformation per unique request
- Both caches respect TTL
- Atomic writes prevent corruption
- Hash collisions are theoretically possible but extremely unlikely with SHA-256

### Concurrency Model
- Tokio async runtime with multi-threading
- Semaphore pattern for FFmpeg rate limiting
- Non-blocking async waits when FFmpeg limit reached
- Image processing not limited by semaphore
- Each request is independent (no shared state beyond caches)

### Error Handling
- Custom error types in `error.rs`
- Errors converted to HTTP responses automatically
- Proper status codes (404, 500, etc.)
- Tracing for debugging
- Network errors, decode errors, and processing errors all handled gracefully

## URL API Reference

### Format
```
/insecure/<directives>/plain/<percent-encoded-source-url>
```

### Directives
- `f:<format>` - Output format (jpeg, png, webp, avif)
- `q:<0-100>` - Quality for lossy formats (default: 82)
- `rs:<mode>:<width>:<height>` or `rt:<mode>:<width>:<height>` - Resize

### Resize Modes
- `fit` - Fit within dimensions (default, maintains aspect ratio, no crop)
- `fill` - Fill dimensions (maintains aspect ratio, center crop, may upscale)
- `fill-down` - Like fill but never upscales
- `force` - Exact dimensions (ignores aspect ratio)
- `auto` - Smart choice based on orientation

### Examples
```bash
# WebP resize to 480x480, fill mode, 85% quality
/insecure/f:webp/q:85/rs:fill:480:480/plain/https%3A%2F%2Fexample.com%2Fimage.jpg

# AVIF fit to 800x600, 80% quality
/insecure/f:avif/q:80/rs:fit:800:600/plain/https%3A%2F%2Fexample.com%2Fphoto.png

# Height only (width calculated from aspect ratio)
/insecure/f:webp/rs:fit::600/plain/https%3A%2F%2Fexample.com%2Fimage.jpg

# Width only (height calculated from aspect ratio)
/insecure/f:jpeg/rs:fit:800:/plain/https%3A%2F%2Fexample.com%2Fimage.jpg

# Video thumbnail
/insecure/f:webp/rs:fit:400:400/plain/https%3A%2F%2Fexample.com%2Fvideo.mp4
```

## Monitoring and Metrics

### Prometheus Metrics Endpoint

The service exposes a `/metrics` endpoint that provides Prometheus-compatible metrics in text format.

**Available Metrics:**

1. **HTTP Request Metrics**
   - `imgproxy_http_requests_total` - Total HTTP requests by endpoint, method, and status
   - `imgproxy_http_request_duration_seconds` - HTTP request latencies (histogram)

2. **Cache Metrics**
   - `imgproxy_cache_hits_total` - Cache hits by type (original/processed)
   - `imgproxy_cache_misses_total` - Cache misses by type

3. **Processing Metrics**
   - `imgproxy_images_processed_total` - Images processed by output format
   - `imgproxy_videos_processed_total` - Video thumbnails extracted
   - `imgproxy_processing_errors_total` - Processing errors by type

4. **FFmpeg Metrics**
   - `imgproxy_ffmpeg_semaphore_permits_available` - Available FFmpeg permits (gauge)
   - `imgproxy_ffmpeg_semaphore_waiters` - Tasks waiting for FFmpeg (gauge)
   - `imgproxy_ffmpeg_extractions_total` - FFmpeg extractions by status

5. **Bandwidth Metrics**
   - `imgproxy_bytes_downloaded_total` - Bytes downloaded from sources
   - `imgproxy_bytes_served_total` - Bytes served to clients

**Example Prometheus Scrape Config:**

```yaml
scrape_configs:
  - job_name: 'imgproxy'
    static_configs:
      - targets: ['localhost:8080']
    metrics_path: '/metrics'
    scrape_interval: 15s
```

**Useful Queries:**

```promql
# Request rate by endpoint
rate(imgproxy_http_requests_total[5m])

# Cache hit ratio
sum(rate(imgproxy_cache_hits_total[5m])) / (sum(rate(imgproxy_cache_hits_total[5m])) + sum(rate(imgproxy_cache_misses_total[5m])))

# 95th percentile latency
histogram_quantile(0.95, rate(imgproxy_http_request_duration_seconds_bucket[5m]))

# FFmpeg queue depth
imgproxy_ffmpeg_semaphore_waiters
```

## Debugging Tips

### Enable Debug Logging
```bash
RUST_LOG=debug cargo run --release
# Or trace for more detail
RUST_LOG=trace cargo run --release
```

### Check Cache Contents
```bash
# List original cache
ls -lh cache/original/

# List processed cache
ls -lh cache/processed/

# Cache stats
du -sh cache/original/ cache/processed/
```

### Monitor FFmpeg Usage
- Watch logs for "Waiting for FFmpeg permit" messages
- Check `MAX_FFMPEG_CONCURRENT` if requests are queuing
- Monitor system resources during video processing

### Common Issues
- **AVIF build fails**: Install meson and ninja
- **Video thumbnails fail**: Install ffmpeg, check PATH
- **Cache not working**: Check file permissions on `CACHE_DIR`
- **Slow performance**: Check cache hit rate (X-Cache header), increase TTL
- **FFmpeg queuing**: Increase `MAX_FFMPEG_CONCURRENT` if system can handle it

## Future Roadmap

Potential enhancements:
- Signed URLs (HMAC verification) - security
- DPR support - responsive images
- Background color for transparent images
- Gravity/crop position control
- In-memory cache (moka) - performance
- ETag/Conditional GET support
- Request deduplication/locking
- Blur, sharpen, and other filters

## License

MIT

## Getting Help

For issues or questions:
1. Check logs with `RUST_LOG=debug`
2. Review this document
3. Check README.md for user-facing documentation
4. Review relevant source file (see Project Structure)
