# Media URL minting

## Goal

The image proxy issues short-lived HMAC capability URLs directly to the
Nostube browser application. The returned URL is then loaded normally
through `img`, `video poster`, preload, or metadata markup.

```text
Browser -- POST /v1/mint --> image proxy -- HMAC URL --> Browser
Browser ---- GET /v1/{key-id}/{signature}/thumb/... ----> image proxy
```

The HMAC secret exists only in the image proxy's deployment secrets:

```text
URL_SIGNING_KEYS=nostube-2026-08:<base64url-secret>
```

## No browser authentication

`POST /v1/mint` is deliberately unauthenticated. It never receives, and does
not require, a Nostr signer, session, or API key. This is a considered
trade-off, not an oversight:

- The endpoint only ever mints URLs for **already-public**, hash-addressed
  Blossom media — the same bytes anyone can already fetch directly from a
  Blossom server. Minting reveals nothing an attacker could not already get.
- The batch shape is fixed: one of three known presets, hash-addressed items
  only. No free `source_url`, directives, quality, or server hints. There is
  nothing to authorize beyond "is this a well-formed batch".
- Requiring a Nostr signer for every media load would break anonymous
  visitors, server-side rendering, embeds, and social-media crawlers/OG
  unfurlers — none of which can hold a signing key. A prior NIP-98-gated
  design excluded all of them; this version does not.

Admission is instead a per-peer-IP flood guard (`MINT_RATE_IP_ITEMS_PER_MIN`),
charged per minted item, not per request.

## Enablement

The mint endpoint is disabled by default. Enable it only after setting all
required values:

```text
URL_SIGNING_KEYS=nostube-2026-08:<base64url-secret>
MINT_ENABLED=true
MINT_PUBLIC_BASE_URL=https://img.example
MINT_ALLOWED_ORIGINS=https://nostube.example
```

`MINT_PUBLIC_BASE_URL` is the canonical public image-proxy origin used to
construct returned URLs; it is never inferred from `Host` or forwarding
headers.

`MINT_ALLOWED_ORIGINS` is a comma-separated browser CORS allowlist. It is a
convenience for well-behaved browsers, **not** an authorization boundary: CORS
is enforced by browsers, not by the API, so a non-browser client can call the
endpoint regardless of this setting. The per-IP rate limit is what actually
bounds abuse.

| Variable | Default | Meaning |
|---|---:|---|
| `MINT_ENABLED` | `false` | Registers `POST /v1/mint` only when true |
| `MINT_PUBLIC_BASE_URL` | unset | Required HTTPS image-proxy origin in production; used to build returned URLs |
| `MINT_ALLOWED_ORIGINS` | unset | Comma-separated allowed browser origins (CORS convenience, not auth) |
| `MAX_MINT_BATCH_ITEMS` | `100`, capped at 100 | Maximum items per request |
| `MINT_RATE_IP_ITEMS_PER_MIN` | `300` | Per-IP mint-item budget per minute |
| `SIGNED_URL_TTL_SECS` | `21600` | Minted URL lifetime; the image proxy rounds expiry to a stable TTL bucket |
| `RATE_IP_REQUESTS_PER_MIN` | `600` | Per-IP budget across every signed/legacy image request, hit or miss |
| `RATE_IP_IMAGE_GENERATIONS_PER_MIN` | `30` | Per-IP budget for cache-miss image decode/resize/encode |
| `RATE_IP_VIDEO_GENERATIONS_PER_MIN` | `5` | Per-IP budget for cache-miss FFmpeg video-thumbnail work — the expensive path |

All rate-limit stores are bounded, in-memory state. A multi-replica
deployment must move them to shared TTL-capable infrastructure before relying
on cluster-wide limits.

## `POST /v1/mint`

### Request

```json
{
  "preset": "feed-preview-v1",
  "items": [
    {
      "id": "event:30200:abc:media:0",
      "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
      "extension": "webp"
    },
    {
      "id": "event:30200:abc:media:1",
      "sha256": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
      "extension": "mp4"
    }
  ]
}
```

No `Authorization` header is sent or required.

Rules:

- One known `preset` applies to the entire batch:
  - `feed-preview-v1`: WebP, quality 82, fit 480×480.
  - `profile-avatar-v1`: WebP, quality 85, fill 160×160.
  - `embed-card-v1`: WebP, quality 82, fit 1200×630.
- `items` contains 1 to `MAX_MINT_BATCH_ITEMS` entries.
- `id` is 1–128 ASCII alphanumeric, `:`, `-`, `_`, or `.` characters and must be unique within the batch.
- `sha256` is exactly 64 hexadecimal characters.
- `extension` is optional for images. Supported explicit image extensions are `jpg`, `jpeg`, `png`, and `webp`; supported direct-container video extensions are the image proxy's FFmpeg allowlist, such as `mp4`, `webm`, and `mkv`.
- The endpoint accepts hash-addressed Blossom media only. It does not accept free `source_url`, resize directives, quality values, server hints, or arbitrary output formats.

The request is atomic. Invalid JSON or any invalid item returns `400`; the
image proxy mints no URLs. The image proxy does not fetch blobs while
minting. A request whose item count exceeds the peer IP's remaining budget
returns `429` with `Retry-After: 1`.

### Response

```json
{
  "preset": "feed-preview-v1",
  "expires_at": 1780000000,
  "items": [
    {
      "id": "event:30200:abc:media:0",
      "url": "https://img.example/v1/nostube-2026-08/<signature>/thumb/0123...webp?f=webp&rs=fit%3A480%3A480&q=82&exp=1780000000"
    },
    {
      "id": "event:30200:abc:media:1",
      "url": "https://img.example/v1/nostube-2026-08/<signature>/thumb/abcd...mp4?f=webp&rs=fit%3A480%3A480&q=82&exp=1780000000"
    }
  ]
}
```

Response items preserve request order and echo the caller's `id`; clients
must associate media by `id`, not by incidental response ordering.

The subsequent image or video-poster load is a standard unauthenticated `GET`
to the image proxy. The v1 URL is the time-bounded capability. The image
proxy verifies its HMAC and `exp`, then admits the request against the
per-IP request/generation budgets below, before fetching, decoding, or
invoking FFmpeg.

## Media request rate limiting

Every `GET` against a signed or legacy image/thumb route spends from three
independent per-IP budgets:

1. **`RATE_IP_REQUESTS_PER_MIN`** — charged once per request, hit or miss.
   Generous by default: serving an already-cached derivative is cheap.
2. **`RATE_IP_IMAGE_GENERATIONS_PER_MIN`** — charged only on a cache miss
   that requires fresh decode/resize/encode work.
3. **`RATE_IP_VIDEO_GENERATIONS_PER_MIN`** — charged only on a cache miss
   that requires an FFmpeg thumbnail extraction, the most expensive path.
   Budgeted far below the image tier.

The tiers are independent, not a hierarchy: exhausting the video budget does
not affect the general request or image budgets for the same IP. A rejection
returns `429` with `Retry-After: 1`.

## Browser integration requirements

1. Build the request body directly from the media references to mint.
2. `POST` it to `/v1/mint` with `Content-Type: application/json`. No auth
   header.
3. Use returned URLs directly in media elements.
4. Batch up to 100 references; the client may issue at most two mint
   requests concurrently for larger feeds.
5. Cache minted URLs client-side by `(preset, sha256, extension)` until
   shortly before `expires_at`, to avoid re-minting on every render.

The browser must never hold or attempt to derive the HMAC signing key.

## Security boundary

The mint endpoint mints URLs only for already-public, hash-addressed
Blossom media through a fixed set of presets — never for arbitrary or
attacker-supplied source URLs. It does not fetch blobs itself. Abuse
resistance for this endpoint is purely rate-based (per-IP, per-minute,
charged by item count), because there is no meaningful authorization
decision to make: any caller who already knows a Blossom hash could fetch
the same bytes without this service.

The general direct-media route (`/img/.../plain/{source-url}`) remains
outside the public mint API. Adding it later requires an additional policy
such as a trusted service credential, a source-domain allowlist, or
product-level authorization — unlike hash-addressed Blossom media, an
arbitrary source URL is not already public through another channel.

## Rolling migration

1. Deploy the image proxy with signed v1 routes, a signing key, and `ALLOW_UNSIGNED_URLS=true`.
2. Enable `POST /v1/mint` with the canonical origin and Nostube's browser origin allowlist.
3. Deploy the Nostube browser client to mint and consume v1 URLs for all supported Blossom media.
4. Observe normal HTTP metrics for `/v1/mint` and signed media routes, signature verification outcomes, and per-tier `429` rates (`imgproxy_rate_limit_rejections_total{tier=...}`).
5. After legacy client and cache windows pass, set `ALLOW_UNSIGNED_URLS=false`.
6. Rotate HMAC keys by prepending a new `key-id:secret` in `URL_SIGNING_KEYS`, then remove the old key after its last minted URL can have expired.
