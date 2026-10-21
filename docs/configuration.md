# Configuration

Everything is configured through environment variables — there are no config
files. All listed variables are read at startup only.

How values are parsed:

- **Numbers and durations** (`*_SECS`, byte limits): absent or unparseable
  values fall back to the default. Concurrency, queue depth, candidate count
  and rate budgets are clamped to at least one; zero is not a disable switch
  for those settings.
- **Boolean flags** accept `true/false`, `1/0`, `yes/no`, `on/off`
  (case-insensitive, whitespace trimmed). Empty values use the default;
  any other value **aborts startup**.
- **`TRUSTED_PROXY_CIDRS`** and **`URL_SIGNING_KEYS`**: an invalid entry
  aborts startup on purpose. If the process restart-loops after a config
  change, check the container log.
- **`ALLOW_UNSIGNED_URLS=false`** without any `URL_SIGNING_KEYS` aborts
  startup, even if the independent preset route is enabled.

The container image overrides the bind address and cache directory as marked
below. The repository's Compose file overrides the image port to `8080`
(`BIND_ADDR=0.0.0.0:8080`) and mounts `./cache` at `/cache`.

## Server

| Variable | Default | Description |
|---|---|---|
| `BIND_ADDR` | `127.0.0.1:8080` (image: `0.0.0.0:8081`) | HTTP listener address. The image healthcheck follows the port in this value. |
| `RUST_LOG` | `info` | `tracing` env-filter. Use `info` in production; `debug` is chatty. |
| `REQUEST_TIMEOUT_SECS` | `30` | Wall-clock budget for one HTTP request, enforced by the router (includes permit waiting). Exceeding it returns `408`. |
| `MAX_INFLIGHT_REQUESTS` | `256` | Global in-flight request ceiling. Excess requests are shed immediately with `503`, not queued. |

## Cache

| Variable | Default | Description |
|---|---|---|
| `CACHE_DIR` | `./cache` (image: `/cache`) | Root of the dual cache (`original/` and `processed/`). Created at startup. FFmpeg also writes temporary frames to the system temp directory. |
| `CACHE_TTL_SECS` | `86400` (24 h) | TTL for URL-addressed (`/insecure`) entries. |
| `CACHE_TTL_IMMUTABLE_SECS` | `2592000` (30 d) | TTL for hash-verified (`thumb`) entries. These cannot go stale behind their key; `MAX_CACHE_BYTES` is the real bound. |
| `MAX_CACHE_BYTES` | `8589934592` (8 GiB) | Disk budget shared by both caches. The janitor runs every 60 s and evicts oldest-written entries first. Give the volume 1–2 GiB headroom above this; a full disk turns writes into `500`s. |

## Fetch and time budgets

| Variable | Default | Description |
|---|---|---|
| `FETCH_TIMEOUT_SECS` | `10` | HTTP timeout for a single image fetch, and the deadline for one image request end to end. |
| `BLOSSOM_FAILOVER_TIMEOUT_SECS` | `15` | Shared deadline for one Blossom image request across all candidates. Each candidate is capped at `FETCH_TIMEOUT_SECS` so one stalled host cannot eat the whole budget. |
| `VIDEO_DEADLINE_SECS` | `25` | Wall-clock budget for one video-thumbnail request end to end (preflight, every range-probe round trip, FFmpeg decode). Kept above `FFMPEG_TIMEOUT_SECS` so the process budget is never the dead letter. |
| `FFMPEG_TIMEOUT_SECS` | `20` | Wall-clock budget for a single FFmpeg invocation. Also feeds its CPU-time rlimit (+1 s). |
| `MAX_VIDEO_PROBE_BYTES` | `67108864` (64 MiB) | Total remote bytes the local media gateway may relay while producing one thumbnail. The only video transfer budget — videos are never downloaded in full. |

## Image decoding

| Variable | Default | Description |
|---|---|---|
| `MAX_IMAGE_BYTES` | `16777216` (16 MiB) | Max compressed image or generated-thumbnail size. Bounds the download, not the decode. |
| `MAX_IMAGE_DIMENSION` | `16384` | Per-axis pixel ceiling applied before a decoder allocates its framebuffer, and to resize targets. |
| `MAX_DECODE_ALLOC_BYTES` | `268435456` (256 MiB) | Best-effort decoder allocation budget, also checked against resize intermediates. It is not a process-wide RSS limit; keep a container memory limit too. |
| `MAX_CPU_CONCURRENT` | visible cores | Simultaneous decode/resize/encode jobs. Sized to the CPU, not the request rate. |
| `MAX_CPU_QUEUE` | `64` | Additional jobs admitted to wait for a CPU permit. Each waiter pins its downloaded original in memory, so an unbounded queue is an unbounded heap. |

## Video thumbnails

| Variable | Default | Description |
|---|---|---|
| `MAX_FFMPEG_CONCURRENT` | `8` | Max concurrent FFmpeg processes (semaphore). Excess requests get `503 Retry-After` instead of queueing. |
| `FFMPEG_TIMEOUT_SECS` | see above | Also bounds each FFmpeg process. |

Each FFmpeg process additionally gets `-threads 1`, an address-space rlimit
of 1 GiB (Linux), a CPU-time rlimit of `FFMPEG_TIMEOUT_SECS + 1`, a 32 MiB
file-size rlimit, and kill-on-drop behavior when its extraction task is cancelled.

## Blossom discovery

| Variable | Default | Description |
|---|---|---|
| `BLOSSOM_FALLBACK_SERVERS` | Four servers, listed below | Comma-separated fallback servers, tried after `xs=` hints and author servers. |
| `MAX_BLOB_CANDIDATES` | `8` | Maximum Blossom candidates (servers + NIP-94 discovered URLs) tried per request. Bounds third-party fan-out. |
| `MAX_SERVER_HINTS` | `4` | Maximum `xs=` hints honoured per request. Surplus hints are dropped, not rejected. |
| `BLOSSOM_SERVER_LIST_CACHE_TTL_HOURS` | `24` | How long a fetched kind-10063 author server list stays valid. |
| `BLOSSOM_SERVER_LIST_FAILURE_CACHE_TTL_SECS` | `300` | How long a failed relay lookup stays cached before retry. |
| `BLOSSOM_DISCOVERY_CACHE_TTL_SECS` | `3600` | How long NIP-94 discovery results for one hash are reused. |
| `BLOSSOM_DISCOVERY_TIMEOUT_SECS` | `3` | Timeout for one NIP-94 discovery round. |
| `BLOSSOM_NEGATIVE_CACHE_NOT_FOUND_TTL_SECS` | `900` (15 min) | Memory-cache TTL for candidates that answered 404/410. `0` disables the class. |
| `BLOSSOM_NEGATIVE_CACHE_PERMANENT_TTL_SECS` | `3600` | Memory-cache TTL for 3xx and non-transient 4xx candidates. `0` disables the class. |
| `BLOSSOM_NEGATIVE_CACHE_TRANSIENT_TTL_SECS` | `60` | Memory-cache TTL for timeouts, transport failures, 429 and 5xx candidates. `0` disables the class. |

Default fallback servers, in order:

- `https://cdn.satellite.earth`
- `https://image.nostr.build`
- `https://nostr.download`
- `https://cdn.hzrd149.com`

Negative-cache entries live in memory only (per candidate URL, up to 10,000
entries). A repeated request skips an unexpired failed candidate but still
tries newly supplied or discovered ones.

## URL modes and signing

| Variable | Default | Description |
|---|---|---|
| `PRESET_THUMBNAILS_ENABLED` | `true` | Registers the unsigned fixed-output `GET /v1/preset/{preset}/{filename}` route (see [Browser presets](./reference/presets.md)). Independent of the legacy unsigned-route switch. |
| `ALLOW_UNSIGNED_URLS` | `true` | Temporary migration switch for the legacy `/insecure` and `/thumb` routes. Turn off once every caller signs its URLs. |
| `REQUIRE_SIGNED_URL_EXPIRY` | `true` | Require an `exp=<unix-seconds>` query parameter in every signed URL, so leaked capability URLs have a bounded lifetime. |
| `URL_SIGNING_KEYS` | unset | Comma-separated `key-id:base64url-secret` HMAC keys. Secrets must decode to at least 32 bytes; key ids are 1–32 chars of `[A-Za-z0-9_-]`. The **first** key in the list is the active signer; all keys verify, which allows rotation by prepending the new key. |

Example (generate a secret, then configure):

```bash
export URL_SIGNING_KEYS="prod-2026-01:$(node -e 'console.log(require("node:crypto").randomBytes(32).toString("base64url"))')"
```

See [HTTP API](./reference/api.md#signed-urls) for the exact signing payload.

## Rate limiting and proxies

| Variable | Default | Description |
|---|---|---|
| `RATE_IP_REQUESTS_PER_MIN` | `600` | General per-client admission budget. Processed image hits bypass it; video requests can consume it before their origin-scoped cache lookup. |
| `RATE_IP_IMAGE_GENERATIONS_PER_MIN` | `30` | Per-client budget for cache-miss image decode/resize/encode work. |
| `RATE_IP_VIDEO_GENERATIONS_PER_MIN` | `5` | Per-client budget for cache-miss FFmpeg video-thumbnail work — the most expensive path. |
| `TRUSTED_PROXY_CIDRS` | unset | Comma-separated CIDRs/IPs of reverse proxies (for Docker/Traefik: the shared Docker network's subnet, which survives proxy re-creation). Only when the TCP peer is inside this list is `X-Forwarded-For` read; the client identity becomes the rightmost non-trusted hop. Unset, the TCP peer is the client. IPv6 clients are bucketed per /64. |

The budgets are in-memory, per-process state; replicas have independent
budgets, not cluster-wide limits. Generation budgets are checked on a
processed-cache miss, even when the original is already cached or the request
then joins an in-flight job. Exceeding a tier returns `429` with `Retry-After: 1`.

## Metrics

| Variable | Default | Description |
|---|---|---|
| `METRICS_BIND_ADDR` | unset | Separate, token-free `/metrics` listener on a management-network address (e.g. `127.0.0.1:9101`). |
| `METRICS_BEARER_TOKEN` | unset | Exposes `/metrics` on the public router behind a bearer token. Unset keeps that route answering `404`; a wrong token is `401`. |

Either mechanism is enough. The Grafana dashboard in
[`grafana-dashboard.json`](https://github.com/flox1an/nostube-imgproxy/blob/main/grafana-dashboard.json)
imports into an existing Grafana and covers every exported metric family.

## Removed variables

Still set but ignored — delete them from deployments:
`MAX_VERIFY_VIDEO_BYTES`, `VIDEO_VERIFY_AFTER_MISSES`,
`MAX_CONCURRENT_VIDEO_VERIFICATIONS`. Videos are no longer downloaded in full
for hash verification.
