# How it works

Nostube Image Proxy is an Axum service, not a search engine or a media store.
It resolves a public source, produces a static image and caches reusable
results on disk. There is no database or upload API.

## Request flow

1. Parse the route, validate directives and source, and verify the signature
   when using a signed URL. Presets supply their own fixed directives.
2. Look for an already-processed derivative. A hit streams from disk;
   matching `If-None-Match` returns `304` without a body.
3. On a miss, apply admission limits and resolve the original. Equivalent
   concurrent misses share a single production job. Blossom flights also
   include the server list, so different video origins do not share a job.
4. Reuse a cached original or fetch it through the guarded HTTP client.
   For videos, extract a frame through FFmpeg. For supported audio
   containers, extract embedded cover art (front cover preferred).
5. Decode, resize with Lanczos3 and encode on a bounded CPU pool, not on
   Tokio's async worker threads. Persist validated, cacheable originals and
   derivatives using atomic writes, then return the image.

## Supported media

| Source | Processing |
|---|---|
| JPEG, PNG, WebP, GIF | Decode into a static image. GIF animation is dropped; only the first frame is used. |
| Video containers | Range-probe through a local gateway, then resize the extracted frame. |
| HLS (`.m3u8`, `.m3u`) | Rewrite playlists, segments, keys and variants through the guarded loopback gateway. Blossom playlists under other extensions can also be detected by `#EXTM3U` content. |
| Audio with embedded artwork | Extract cover art, then use the image pipeline. MP3 ID3v2 `APIC`, FLAC `PICTURE` and M4A `covr` are common sources. Missing art returns `400`; no placeholder is generated. |

Output formats are JPEG, PNG, WebP and AVIF. **AVIF is output-only**;
unsupported image inputs are rejected before decoding. This is a static
thumbnail service, not an animation or video transcoder.

## Blossom discovery

Hash-addressed routes accept a 64-character hexadecimal SHA-256 filename,
optionally followed by a source extension. Hashes and extensions are
normalized to lowercase.

Candidates are assembled in this order:

1. Explicit `xs=` server hints (up to `MAX_SERVER_HINTS`).
2. The author's kind-10063 Blossom server list, looked up using `as=`.
3. `BLOSSOM_FALLBACK_SERVERS`.
4. Direct URLs discovered from NIP-94 kind-1063 events for the hash.

Server lists are normalized and deduplicated. Invalid/private servers are
ignored. `MAX_BLOB_CANDIDATES` caps the combined candidate list; image
candidates share `BLOSSOM_FAILOVER_TIMEOUT_SECS`, with a per-attempt fetch
timeout so a stalled server does not consume the entire deadline.

For images and audio, the complete fetched bytes must match the requested
SHA-256 **before** processing and caching. Failure at one candidate allows
failover. Candidate failures are cached in memory by class (not found,
permanent or transient); new candidate URLs can still be tried. Relay
server lists and NIP-94 discovery have separate in-memory TTLs.

### Video thumbnails

Video extensions select an explicit FFmpeg demuxer. A bare Blossom hash can
also be recognized as video from extensioned NIP-94 URLs. Direct containers
include MP4/MOV/QuickTime/M4V/3GP, WebM/MKV, AVI, FLV, WMV, MPEG and OGV;
DASH manifests (`.mpd`) are not supported.

FFmpeg does not contact the remote source directly. It talks to an ephemeral
loopback HTTP gateway; the gateway performs bounded, policy-checked range
requests. A thumbnail reads only the container index and relevant media
ranges, never a full-video download for hash verification. Sources that
cannot be probed within the byte/time budgets fail rather than being
unboundedly downloaded.

Extraction starts around 0.5 seconds and samples up to eight frames at one
frame per second, bounded to 1280×720. The first frame with visible luma
content and variation wins; if every sample is dark or flat, the first frame
is used. Audio-only sources cannot yield a video frame.

::: warning Video hashes are not verified
Range probing cannot prove that a complete video matches its Blossom hash.
A cached video frame is therefore scoped to `blob@origin`, and is only reused
when that origin appears in the requesting client's server list. A
NIP-94-discovered video URL outside that list can serve a frame but cannot
persist it. Do not treat a thumbnail as proof of video integrity.
:::

A Blossom HLS playlist fetched through the image path is hash-verified, but
its segment bytes are not. That content-sniffed HLS result is served without
disk caching or an ETag. Extension-addressed HLS uses the video path and its
origin-scoped cache rules.

## Caching

```text
cache/
├── original/
│   ├── insecure/   URL-addressed images, audio or extracted frames
│   └── thumb/      Verified Blossom originals or origin-scoped frames
└── processed/
    ├── insecure/   URL-addressed derivatives
    └── thumb/      Blossom derivatives
```

The original cache avoids downloading or extracting the same source again
when another size is requested. The processed cache serves the final output.
Derivative keys hash the normalized source identity and parsed directives,
not the raw URL, so equivalent format aliases/directives reuse an entry.
Files are sharded by digest prefixes.

Writes use create-new temporary files, `fsync` and rename. Zero-byte files
are misses. The janitor applies namespace TTLs and oldest-modified-first
budget eviction every 60 seconds; the disk budget is not an instantaneous
quota. Reads do not refresh the on-disk TTL.

| Namespace | Default disk TTL | Client cache policy |
|---|---|---|
| `insecure` | 24 hours | `public, max-age=3600` |
| `thumb` (persisted) | 30 days | `public, max-age=31536000, immutable` |
| Non-persisted Blossom result | Not cached | `public, max-age=3600`, no ETag |
| Signed URL with `exp` | Underlying namespace TTL | `max-age` limited to remaining URL lifetime |

Disk retention and client/CDN retention are different settings. Signed and
unsigned routes can share cached output, but signed URLs are verified
before cache access. An expired signature never becomes valid because the
image is already cached.

## Admission and isolation

- A global in-flight cap sheds excess work with `503`; the router enforces
  a wall-clock request timeout (`408`).
- The CPU pool bounds concurrent decode/resize/encode jobs and waiting
  originals. Decoder dimensions, compressed input size and allocation limits
  address different resource risks; one does not replace the others.
- A semaphore bounds FFmpeg processes. Saturation returns `503` rather than
  building an unbounded video queue.
- Per-client rate limits bound request admission and image/video generation.
  Processed image hits return before these checks. Video requests may still
  spend the general request budget before their origin-scoped cache lookup;
  cache hits skip generation budgets. See [Configuration](../configuration.md).
- Rate-limit and single-flight state is local to one process; replicas do
  not share a global budget or in-flight work.

## Network and security boundary

Sources must be public HTTP/HTTPS URLs. The guarded client rejects private,
loopback, link-local, CGNAT, documentation/reserved addresses and blocked
IPv4-embedding forms, checks DNS resolution, and rechecks redirects. System
and environment HTTP proxies are ignored so they cannot bypass that policy.
FFmpeg's remote media access always passes through the loopback gateway;
HLS references receive the same checks.

Anonymous presets deliberately accept public hashes and server hints. They
constrain output shapes, **not** who controls the input bytes. FFmpeg is a
native decoder exposed to untrusted media: process limits and a non-root
container reduce impact but do not eliminate decoder vulnerabilities. Keep
the image fresh and apply the [deployment hardening](../operations.md).

## Source map

| Module | Responsibility |
|---|---|
| `main.rs`, `config.rs` | Startup, environment, listeners and shutdown |
| `server.rs`, `preset.rs`, `signing.rs` | Routes, fixed output presets and capability verification |
| `blossom.rs`, `network_policy.rs`, `fetch.rs` | Discovery, failover and guarded fetching |
| `thumbnail.rs`, `hls.rs`, `audio.rs` | Frame extraction, HLS gateway and cover art |
| `transform.rs`, `cpu.rs` | Image parsing, decoding, resizing and encoding |
| `cache.rs`, `singleflight.rs`, `ratelimit.rs` | Disk retention, request coalescing and admission |
| `metrics.rs`, `error.rs` | Prometheus instrumentation and public error responses |
