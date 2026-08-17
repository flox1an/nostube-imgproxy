# NIP-98 media URL minting

## Goal

The image proxy issues short-lived HMAC capability URLs directly to the Nostube browser application. The browser authenticates the **mint** request with NIP-98; it never receives an HMAC signing key. The returned URL is then loaded normally through `img`, `video poster`, preload, or metadata markup.

```text
Browser -- NIP-98 POST /v1/mint --> image proxy -- HMAC URL --> Browser
Browser -------- GET /v1/{key-id}/{signature}/thumb/... ----------> image proxy
```

The HMAC secret exists only in the image proxy's deployment secrets:

```text
URL_SIGNING_KEYS=nostube-2026-08:<base64url-secret>
```

## Enablement

The mint endpoint is disabled by default. Enable it only after setting all required values:

```text
URL_SIGNING_KEYS=nostube-2026-08:<base64url-secret>
NIP98_MINT_ENABLED=true
MINT_PUBLIC_BASE_URL=https://img.example
MINT_ALLOWED_ORIGINS=https://nostube.example
```

`MINT_PUBLIC_BASE_URL` is the canonical public image-proxy origin. The image proxy uses it to validate NIP-98's exact `u` tag and to construct returned URLs; it is never inferred from `Host` or forwarding headers.

`MINT_ALLOWED_ORIGINS` is a comma-separated browser CORS allowlist. It controls browser access to the cross-origin `POST` endpoint. NIP-98 and rate limits remain the authorization and abuse controls; CORS is not authorization.

| Variable | Default | Meaning |
|---|---:|---|
| `NIP98_MINT_ENABLED` | `false` | Registers `POST /v1/mint` only when true |
| `MINT_PUBLIC_BASE_URL` | unset | Required HTTPS image-proxy origin in production; exact NIP-98 request target |
| `MINT_ALLOWED_ORIGINS` | unset | Comma-separated allowed browser origins |
| `MAX_MINT_BATCH_ITEMS` | `100`, capped at 100 | Maximum items per request |
| `MINT_RATE_IP_ITEMS_PER_MIN` | `300` | Per-IP mint-item budget per minute |
| `MINT_RATE_PUBKEY_ITEMS_PER_MIN` | `120` | Per-Nostr-pubkey mint-item budget per minute |
| `NIP98_REPLAY_TTL_SECS` | `90`, minimum 60 | Event-ID replay retention |
| `SIGNED_URL_TTL_SECS` | `21600` | Minted URL lifetime; the image proxy rounds expiry to a stable TTL bucket |

The replay and rate-limit stores are bounded, in-memory state. A multi-replica deployment must move those stores to shared TTL-capable infrastructure before relying on cluster-wide limits.

## `POST /v1/mint`

### Authentication

The caller sends:

```http
Authorization: Nostr <base64-encoded NIP-98 event>
Content-Type: application/json
```

The NIP-98 event must:

- have `kind: 27235` and empty content;
- have a valid event ID and Schnorr signature;
- be at most 60 seconds old and no more than 30 seconds in the future;
- include exactly one `u` tag equal to `https://img.example/v1/mint`;
- include exactly one `method` tag with `POST`;
- include exactly one `payload` tag equal to the lowercase SHA-256 hex digest of the exact JSON request bytes;
- have an event ID that the image proxy has not accepted during `NIP98_REPLAY_TTL_SECS`.

The `payload` tag is not an additional signature or key. It is part of the one NIP-98 event signature and prevents a valid auth header from authorizing a different batch body.

Authentication failures and replay attempts return `401`. A request exceeding either the IP or pubkey item budget returns `429` with `Retry-After: 1`.

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

The request is atomic. Invalid JSON or any invalid item returns `400`; the image proxy mints no URLs. The image proxy does not fetch blobs while minting.

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

Response items preserve request order and echo the caller's `id`; clients must associate media by `id`, not by incidental response ordering.

The subsequent image or video-poster load is a standard unauthenticated `GET` to the image proxy. The v1 URL is the time-bounded capability. The image proxy verifies its HMAC and `exp` before fetching, decoding, or invoking FFmpeg.

## Browser integration requirements

1. Serialize the JSON body once.
2. Calculate SHA-256 over those exact UTF-8 bytes.
3. Create and Nostr-sign a NIP-98 event containing the exact mint URL, `POST`, and the payload hash.
4. Send the event in the `Authorization` header with the unchanged body.
5. Use returned URLs directly in media elements.
6. Batch up to 100 references; the client may issue at most two mint requests concurrently for larger feeds.

The browser must not attempt to recreate HMAC signatures and must not retain image-proxy signing material.

## Security boundary

All Nostr pubkeys are initially eligible to mint URLs. A pubkey is therefore an attribution and fairness key, not a scarce authorization credential: new keys are cheap to generate. The image proxy enforces both IP and pubkey budgets, charges by item rather than request, uses replay protection, and allows only fixed presets plus hash-addressed input.

The general direct-media route (`/img/.../plain/{source-url}`) remains outside the public mint API. Adding it later requires an additional policy such as a trusted service credential, a source-domain allowlist, or product-level authorization.

## Rolling migration

1. Deploy the image proxy with signed v1 routes, a signing key, and `ALLOW_UNSIGNED_URLS=true`.
2. Enable `POST /v1/mint` with the canonical origin and Nostube's browser origin allowlist.
3. Deploy the Nostube browser client to mint and consume v1 URLs for all supported Blossom media.
4. Observe normal HTTP metrics for `/v1/mint` and signed media routes, signature verification outcomes, 401 replay/auth failures, and 429 rates.
5. After legacy client and cache windows pass, set `ALLOW_UNSIGNED_URLS=false`.
6. Rotate HMAC keys by prepending a new `key-id:secret` in `URL_SIGNING_KEYS`, then remove the old key after its last minted URL can have expired.
