# Operations and deployment hardening

Everything a deployment needs to know about this service's runtime footprint,
written so an operator (or an agent) can size limits and harden a deployment
without reading the code. The executable reference is
[`docker-compose.yml`](../docker-compose.yml); the full environment variable
table is in the [README](../README.md#configuration).

## What the process does at runtime

| Aspect | Behaviour |
|---|---|
| User | `imgproxy` (uid 1000) inside the image; needs no Linux capability |
| Inbound | One HTTP port from `BIND_ADDR` (image default `0.0.0.0:8081`); `/health` answers `200 OK` |
| Outbound | HTTP/HTTPS to arbitrary **public** hosts and ports (Blossom servers, `/insecure` source URLs, NIP-94 locations); WSS to the Nostr seed relays in `src/blossom.rs` (`SEED_RELAYS`) for kind 10063/1063 lookups; DNS |
| Never contacted | Private, loopback, link-local, CGNAT, documentation and reserved ranges, IPv4-mapped/NAT64/6to4 forms of them (`src/network_policy.rs`). Enforced per redirect hop and at DNS resolution; env/system proxies are ignored |
| Writes | `CACHE_DIR` (cache entries and their temp files) and the temp dir (`$TMPDIR`, default `/tmp`) for FFmpeg's extracted frames |
| Child processes | `ffmpeg` only, at most `MAX_FFMPEG_CONCURRENT` at once, never through a shell, each with `-threads 1` and `RLIMIT_AS` 1 GiB (Linux), `RLIMIT_CPU` ≈ `FFMPEG_TIMEOUT_SECS`, `RLIMIT_FSIZE` 32 MiB; killed on timeout or client disconnect |
| Video input | Range requests only, through a loopback gateway; at most `MAX_VIDEO_PROBE_BYTES` per thumbnail. Videos are never downloaded in full, whatever their size |

## Resource sizing

### Memory

Measured baseline in production (2026-10-02): ~320 MiB RSS at idle-to-light
load. Peak usage is the sum of these bounded contributors:

| Contributor | Bound | Default worst case |
|---|---|---|
| Image decode/resize/encode | `MAX_CPU_CONCURRENT` × `MAX_DECODE_ALLOC_BYTES` | cores × 256 MiB |
| Fetched originals waiting for CPU | `MAX_CPU_QUEUE` × `MAX_IMAGE_BYTES` | 64 × 16 MiB = 1 GiB |
| FFmpeg processes (separate RSS) | `MAX_FFMPEG_CONCURRENT` × actual decoder use (address space capped at 1 GiB each) | typically 50–150 MiB each → 8 × ~150 MiB |
| HLS gateway segment buffer | one segment ≤ remaining `MAX_VIDEO_PROBE_BYTES` per HLS job | 64 MiB per concurrent HLS thumbnail |
| In-memory caches | Blossom candidate failures ≤ 10k entries; rate-limit windows ≤ 10k clients per tier | a few MiB |

The contributors are bounds, not typical values; real peaks are far lower.
Recommended: a hard container memory limit of **2–3 GiB** together with
`MAX_FFMPEG_CONCURRENT=4`–`8` and `MAX_CPU_CONCURRENT` ≤ the container's CPU
quota. To make the limit provably sufficient, lower `MAX_CPU_QUEUE` and
`MAX_FFMPEG_CONCURRENT` rather than raising the limit. An OOM kill restarts
the container; the disk cache survives.

### CPU

Decode/encode runs on a bounded pool (`MAX_CPU_CONCURRENT`, default = visible
cores); FFmpeg runs single-threaded per process. A CPU quota equal to the cores
you want to spend is enough; set `MAX_CPU_CONCURRENT` to match it so queued
work is shed with `503` instead of contending.

### Disk

| Path | Size |
|---|---|
| `CACHE_DIR` | `MAX_CACHE_BYTES` (default 8 GiB). Enforced by a cleanup pass **every 60 s**, so give the volume ~1–2 GiB headroom above it; a full disk turns cache writes into `500` responses |
| Temp dir | Small: FFmpeg writes ≤ 8 WebP frames per extraction, each ≤ 32 MiB by rlimit, typically a few hundred KiB. 128 MiB tmpfs covers 8 concurrent extractions with wide margin |

Eviction is oldest-written-first; `/thumb` entries live
`CACHE_TTL_IMMUTABLE_SECS` (30 d), `/insecure` entries `CACHE_TTL_SECS` (24 h).

### Processes / PIDs

Tokio worker threads (one per core), Tokio's blocking pool for file I/O (grows
on demand), up to `MAX_FFMPEG_CONCURRENT` FFmpeg processes. A PID limit of
**512** leaves ample room.

## Container hardening checklist

All of these work with the stock image; none needs a code change.

| Setting | Value | Why |
|---|---|---|
| Run as non-root | image default (`USER imgproxy`) | already in the Dockerfile |
| Drop capabilities | `cap_drop: [ALL]` | the process needs none (port > 1024, no raw sockets) |
| No privilege escalation | `security_opt: [no-new-privileges:true]` | blocks setuid binaries; the image also ships without setuid/setgid bits, which covers platforms that cannot set this flag |
| Read-only root FS | `read_only: true` | the service writes only `CACHE_DIR` and the temp dir |
| Temp dir | `tmpfs: /tmp:size=128m,mode=1777` | required with `read_only`; see Disk |
| Cache volume | writable mount at `CACHE_DIR` | the only persistent state; safe to delete (cold cache) |
| Memory limit | 2–3 GiB, see Memory | a decoder bug or load spike must not starve the host |
| CPU limit | as wanted, plus `MAX_CPU_CONCURRENT` to match | |
| PID limit | 512 (`deploy.resources.limits.pids`; Compose rejects a top-level `pids_limit` next to a `deploy.resources.limits` block) | fork-bomb containment for FFmpeg |
| Log rotation | json-file `max-size: 50m`, `max-file: 3` | `RUST_LOG=debug` is chatty; use `info` in production |
| Port exposure | reach the port only through the reverse proxy; do not publish it on the host | |
| Egress filter (defence in depth) | host firewall rule (e.g. `DOCKER-USER`) dropping container traffic to RFC1918, `169.254.0.0/16`, `100.64.0.0/10` | the app already refuses these; this catches a future bug or an FFmpeg compromise |

Verify on the host after deploying:

```bash
docker inspect -f 'user={{.Config.User}} ro={{.HostConfig.ReadonlyRootfs}} capdrop={{.HostConfig.CapDrop}} secopt={{.HostConfig.SecurityOpt}} pids={{.HostConfig.PidsLimit}} mem={{.HostConfig.Memory}} tmpfs={{.HostConfig.Tmpfs}}' <container>
docker exec <container> sh -c 'touch /probe' # must fail: read-only file system
curl -fsS https://<public-name>/health        # OK
```

Then request one image and one video thumbnail through the public name: both
must return `200` (FFmpeg works without capabilities and with the read-only root).

### Coolify specifics

A Coolify **Docker-image Application** only applies these custom Docker options:
`--cap-add`, `--cap-drop`, `--security-opt`, `--sysctl`, `--device`,
`--ulimit`, `--init`, `--privileged`, `--gpus`, `--shm-size`, `--ip`, `--ip6`,
`--dns`, `--hostname`, `--entrypoint`
([docs](https://coolify.io/docs/applications/builds/custom-docker-options)).
So:

- `--cap-drop=ALL` goes into *Custom Docker Options*. Coolify (verified on
  v4.3.23) turns these options into Compose keys with a regex that cuts every
  value at its first hyphen: `--security-opt=no-new-privileges:true` becomes
  `security_opt: "no"` and the deploy fails with `invalid security-opt: "no"`.
- Memory/CPU go into *Resource Limits*. If the host has swap, set the swap
  limit equal to the memory limit; `0` lets Docker add the same amount again
  as swap.
- `no-new-privileges`, `read_only`, `tmpfs` and `pids_limit` are **not**
  possible there; they need a Docker Compose resource (use
  `docker-compose.yml` from this repo as the base). The setuid part of
  `no-new-privileges` is covered anyway: the image strips all setuid/setgid
  bits.
- Turn on Coolify's health check (path `/health`, port `8081`). Only then does
  the rolling update wait for the new container to become healthy before it
  stops the old one; without it Traefik answers `503` for a few seconds per
  deploy. Compose resources get no rolling update at all.
- Coolify pulls the image on every deploy of a Docker-image app (deploy log:
  "Pulling latest images from the registry"). Still compare the running
  container's image ID with the pulled tag after a deploy.

## Reverse proxy

Behind a reverse proxy every request's TCP peer is the proxy. Set
`TRUSTED_PROXY_CIDRS` to the proxy's network (for Docker/Traefik: the shared
Docker network's subnet, which survives proxy re-creation; a single proxy IP
does not). The rate-limit identity then becomes the rightmost
`X-Forwarded-For` hop that is not itself a trusted proxy; client-supplied
entries to its left are ignored, and peers outside the list get no header
trust at all. IPv6 clients are bucketed per /64.

Without it, all users share one budget and the `RATE_IP_*` limits either
throttle everyone or have to be raised until they protect nothing.

Only one proxy layer is supported per trusted CIDR list entry: if a CDN sits
in front of the proxy, add the CDN's egress ranges too, or the CDN edge becomes
the "client".

## Production environment checklist

| Variable | Recommendation |
|---|---|
| `BIND_ADDR` | `0.0.0.0:<port>`; the image healthcheck follows this port |
| `CACHE_DIR` | the mounted volume |
| `TRUSTED_PROXY_CIDRS` | proxy network CIDR (see above) |
| `RATE_IP_REQUESTS_PER_MIN` / `_IMAGE_GENERATIONS_` / `_VIDEO_GENERATIONS_` | defaults `600/30/5` are per real client once `TRUSTED_PROXY_CIDRS` is set |
| `ALLOW_UNSIGNED_URLS` | `true` while clients use `/insecure` or `/thumb`; see Known Issues in the README |
| `METRICS_BEARER_TOKEN` or `METRICS_BIND_ADDR` | one of them for Prometheus; the token is a secret |
| `MAX_FFMPEG_CONCURRENT`, `MAX_CPU_CONCURRENT`, `MAX_CPU_QUEUE` | size against the memory limit (see Memory) |
| `RUST_LOG` | `info` |

Boolean flags accept `true/false`, `1/0`, `yes/no`, `on/off`; anything else,
and any invalid `TRUSTED_PROXY_CIDRS` / `URL_SIGNING_KEYS` entry, aborts startup
on purpose — check the container log if it restart-loops after a config change.

**Removed variables** (ignored if still set; delete them from deployments):
`MAX_VERIFY_VIDEO_BYTES`, `VIDEO_VERIFY_AFTER_MISSES`,
`MAX_CONCURRENT_VIDEO_VERIFICATIONS`. Videos are no longer downloaded in full
for hash verification; see "Video Thumbnails" in the README.

## Supply chain

- Images are built from the committed `Cargo.lock` with `--locked`; CI runs
  tests and `cargo audit` before `docker-build` may publish.
- The `:main` tag moves on every push and on a daily scheduled rebuild (fresh
  Debian/FFmpeg security updates). Pin `sha-<commit>` tags if deployments must
  be reproducible; then schedule redeploys to pick up base-image fixes.
- FFmpeg comes from Debian trixie in the runtime stage. Decoder bugs in FFmpeg
  are the main residual risk of this service (README → Known Issues), so keep
  the image fresh and the hardening above in place.
