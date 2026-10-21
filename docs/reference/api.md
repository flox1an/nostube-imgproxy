# HTTP API

All media routes return a static image, including requests for video or
audio sources. Source extensions help select the input pipeline; output
format is controlled by the preset or directives, not the filename suffix.

## Routes

| GET route | Availability | Purpose |
|---|---|---|
| `/v1/preset/{preset}/{filename}` | `PRESET_THUMBNAILS_ENABLED=true` (default) | Public, fixed-output Blossom thumbnail. Recommended for browser clients. |
| `/insecure/{directives}/plain/{encoded-source-url}` | `ALLOW_UNSIGNED_URLS=true` (default) | Arbitrary public source URL and directives. |
| `/thumb/{filename}` | `ALLOW_UNSIGNED_URLS=true` | Blossom source with query directives. |
| `/v1/{key-id}/{signature}/img/{directives}/plain/{encoded-source-url}` | `URL_SIGNING_KEYS` configured | Signed direct-source capability. |
| `/v1/{key-id}/{signature}/thumb/{filename}` | `URL_SIGNING_KEYS` configured | Signed Blossom capability. |
| `/health` | Always | `200`, plain-text body `OK`. Liveness, not an upstream availability check. |
| `/metrics` | Opt-in | Prometheus text; see [Metrics](#metrics). |

Disabled media routes return `404`. Media routes allow cross-origin access
(`Access-Control-Allow-Origin: *`); health and metrics do not inherit that
media CORS layer. There is no upload, mint or search endpoint.

## Browser presets

```text
/v1/preset/feed-preview-v1/{sha256}[.{extension}]?xs={server}&as={author}
```

Only `xs` (repeatable) and `as` are accepted. All other query keys are
rejected with `400`. See [Browser presets](./presets.md) for the three
published shapes and browser integration.

## Direct source URLs

Build the URL by percent-encoding the complete source URL, including its
own query string:

```js
const source = 'https://example.com/photo.jpg?version=2';
const url = 'http://127.0.0.1:8080/insecure/f:webp/q:85/rs:fill:480:480/plain/'
  + encodeURIComponent(source);
```

Sources must be public HTTP/HTTPS URLs. Private services and localhost are
not accepted, even for local development.

| Directive | Meaning |
|---|---|
| `f:jpeg`, `f:jpg`, `f:png`, `f:webp`, `f:avif` | Output format; direct-source default is JPEG. |
| `q:1` … `q:100` | Lossy encoder quality; default `82`. Zero is invalid. PNG does not use this quality value. |
| `rs:{mode}:{width}:{height}` | Resize instruction. `rt:` is an accepted alternative to `rs:`. |

Specify at least one nonzero dimension. An empty dimension or `0` is
calculated from the source aspect ratio, for example `rs:fit:800:` or
`rs:fit::600`. Both axes are subject to `MAX_IMAGE_DIMENSION` and allocation
checks. Unknown path directives are ignored; repeated recognized directives
use the last value. Prefer a single explicit value for each directive.

| Mode | Result | Upscaling |
|---|---|---|
| `fit` | Fit inside the box, preserve aspect ratio, no crop | No |
| `fill` | Cover the box, preserve aspect ratio, center-crop | Yes |
| `fill-down` | Center-crop like fill, but keep smaller sources small | No |
| `force` | Exact dimensions, distort aspect ratio if necessary | Yes |
| `auto` | Fill when source and target share portrait/landscape orientation; otherwise fit | Depends on selected mode |

Video URLs use the same route and directives. Their recognized extension
selects frame extraction; audio bytes with embedded artwork are detected
by content. See [media handling](../guide/how-it-works.md#supported-media).

## Blossom thumbnails

```text
/thumb/{sha256}[.{extension}]?f=webp&rs=fit%3A480%3A480&q=82&xs={server}&as={author}
```

The filename must contain exactly 64 hexadecimal hash characters, optionally
followed by an extension. Uppercase hashes/extensions are accepted and
normalized to lowercase. Supply the actual source extension for videos;
a bare hash can only be inferred as video when discovery finds a recognized
video URL.

| Query | Default | Meaning |
|---|---|---|
| `f` | `webp` | `jpeg`/`jpg`, `png`, `webp` or `avif` |
| `q` | `82` | Quality `1`–`100` |
| `rs` | `fit:480:480` | Same resize grammar as path directives |
| `width`, `height` | `480` each | Used in fit mode only when `rs` is absent. Set one axis to `0` to calculate it from aspect ratio. |
| `xs` | None | Repeatable Blossom server hint; hostname or public HTTP/HTTPS base URL. Excess hints are truncated by `MAX_SERVER_HINTS`. |
| `as` | None | Author pubkey (hex or npub) for a kind-10063 server-list lookup |

Unlike presets, legacy/signed thumbnail query parsing tolerates unrecognized
keys. Do not rely on them to affect output. The source extension does not
set the response format.

## Signed URLs

Signing is for trusted backends that need arbitrary directives or direct
source URLs. **Never ship a signing secret to a browser.** Browser presets
need neither signatures nor an authentication/minting round-trip.

Configure `URL_SIGNING_KEYS` with one or more `key-id:base64url-secret`
pairs. Each secret is unpadded base64url of at least 32 random bytes.

For a signed request, define `pathAndQuery` as the exact raw path and query
**after** `/v1/{key-id}/{signature}`. It starts with `/img/` or `/thumb/`.
The HMAC input is UTF-8:

```text
nostube-imgproxy-url-v1\n{key-id}\n{pathAndQuery}
```

Compute HMAC-SHA256 and encode the 32-byte digest as unpadded base64url.
Query order, percent-encoding and every signed byte matter. Do not
re-serialize the path or query after signing.

Node.js example (uses only the standard library; read a key already shared
with the server from a trusted environment):

```js
import { createHmac } from 'node:crypto';

const [keyId, encodedSecret] = process.env.URL_SIGNING_KEYS.split(',')[0].trim().split(':');
const source = 'https://example.com/photo.jpg';
const exp = Math.floor(Date.now() / 1000) + 300;
const pathAndQuery = `/img/f:webp/rs:fit:480:480/plain/${encodeURIComponent(source)}?exp=${exp}`;
const payload = `nostube-imgproxy-url-v1\n${keyId}\n${pathAndQuery}`;
const signature = createHmac('sha256', Buffer.from(encodedSecret, 'base64url'))
  .update(payload)
  .digest('base64url');
console.log(`https://images.example.com/v1/${keyId}/${signature}${pathAndQuery}`);
```

By default exactly one `exp=<Unix-seconds>` is required. Missing, duplicate
or malformed expiry and invalid signatures return `403`; expiry at or before
the current server time is expired. `REQUIRE_SIGNED_URL_EXPIRY=false` permits
missing expiry, but an expiry that is present is still checked. Signed
responses with expiry have client `max-age` capped to its remaining lifetime.

For rotation, configure old and new keys during the transition. The Rust
signer uses the first configured key; verifiers accept any configured key
ID. Remove the old key once its capabilities should no longer work.

## Response headers

| Header | Behavior |
|---|---|
| `Content-Type` | `image/jpeg`, `image/png`, `image/webp` or `image/avif` |
| `X-Cache` | `hit` from processed disk cache, `miss` for new production, `coalesced` when joining an in-flight job |
| `ETag` | Present for persisted derivatives. Send `If-None-Match` for a bodyless `304` on a disk-cache hit. Non-persisted results have no ETag. |
| `Cache-Control` | Persisted unsigned Blossom results: one year, immutable; direct-source and non-persisted Blossom results: one hour; signed URLs with expiry: remaining lifetime. |
| `Retry-After: 1` | Rate limit (`429`) or capacity shedding (`503`) |

Origin-scoped video frames that are persisted receive the immutable policy,
**not** a separate short-lived video policy. A range-probed video hash is
still not verified; see [caching and integrity](../guide/how-it-works.md#video-thumbnails).

## Errors

Service-generated errors are plain text and do not expose internal paths or
decoder panic details. Framework parsing errors can have their own wording.

| Status | Meaning |
|---|---|
| `400` | Invalid directives, preset/hash/query, unsupported source format, missing cover art or video stream |
| `401` | Metrics bearer token missing or incorrect |
| `403` | Invalid or expired signed capability |
| `404` | Disabled/unknown route, or upstream source returned 403/404/410 |
| `408` | Router request timeout |
| `413` | Source exceeds the input size budget |
| `422` | Image decode failure |
| `429` | Per-client admission/generation budget exceeded |
| `500` | Internal processing or disk I/O failure |
| `502` | Upstream transport, hash verification or other upstream failure; upstream timeout codes are normalized rather than passed through |
| `503` | Node at capacity |

## Metrics

Choose either or both options:

- `METRICS_BIND_ADDR=127.0.0.1:9101` exposes token-free `/metrics` on a
  separate listener. Keep it on a management network.
- `METRICS_BEARER_TOKEN` enables `/metrics` on the public listener. Use
  `Authorization: Bearer <token>`. Without a configured nonempty token this
  route returns `404`.

The [operations guide](../operations.md#monitoring-and-troubleshooting)
includes scrape commands and the Grafana dashboard link.
