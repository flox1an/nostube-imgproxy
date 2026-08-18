# Preset thumbnail integration

## Goal

The image proxy serves Blossom-hash-addressed thumbnails through a small,
fixed set of published presets. The Nostube browser client builds these
URLs directly — there is no request/response round-trip, no signing, and no
authentication.

```text
Browser ---- GET /v1/preset/{preset}/{sha256}[.{ext}] ----> image proxy
```

## Why this is safe unauthenticated

This route was previously reached only via a NIP-98-authenticated
`POST /v1/mint` endpoint that returned an HMAC-signed capability URL. That
design was reconsidered and removed: it excluded every anonymous visitor,
server-side render, embed, and social-media crawler, none of which can hold
a Nostr signing key — for a security benefit that turned out not to exist.

The signature bought two things in theory: identity binding, and a
constrained output shape. Neither survives scrutiny for this route:

- **Identity binding** protects nothing here. The batch could only ever
  reference already-public, hash-addressed Blossom media. A Nostr pubkey is
  free to generate, so it was never a scarce credential — it added a UX
  barrier without closing a real attack surface.
- **Constraining the output shape** does not require cryptography. `<preset>`
  is one of three fixed, published names; the server looks up the exact
  output directives itself. There is no client-controllable value to bound
  beyond what path/query validation already rejects.

The only real cost control this route needs is bounding *how often* one
peer can trigger disk I/O or CPU-heavy generation work — which is a rate
limit, not an authorization decision. See "Rate limiting" below.

## Enablement

```text
PRESET_THUMBNAILS_ENABLED=true   # default
```

No signing key, public base URL, or CORS allowlist is required. Because the
route emits the same wide-open CORS policy as every other image route
(`Access-Control-Allow-Origin: *`), it is reachable from any origin — that is
intentional, matching how `<img>`/`<video poster>` tags already work without
a CORS preflight.

## URL format

```text
/v1/preset/{preset}/{sha256}[.{extension}]?xs=<server-hint>&as=<author-pubkey>
```

| Segment | Meaning |
|---|---|
| `{preset}` | One of the fixed preset names below. Any other value returns `400`. |
| `{sha256}` | 64 lowercase hex characters. Malformed hashes return `400`. |
| `{extension}` | Optional. Used only to detect whether the source is a video (for FFmpeg thumbnail extraction); it does not affect the response format. |
| `xs` | Optional, repeatable. Blossom server hint(s) to try, highest priority. |
| `as` | Optional. Author npub/hex pubkey, used for a kind-10063 relay server-list lookup. |

No other query parameter is accepted. `f`, `rs`, `q`, `width`, and `height`
are rejected outright with `400` — the endpoint deliberately refuses to
silently ignore an attempted directive override, so a client mistake is
loud rather than quietly wrong.

## Presets

| Preset | Format | Quality | Resize |
|---|---|---:|---|
| `feed-preview-v1` | WebP | 82 | fit 480×480 |
| `profile-avatar-v1` | WebP | 85 | fill 160×160 |
| `embed-card-v1` | WebP | 82 | fit 1200×630 |

These are the only three server-authoritative output shapes. There is no
mechanism to request a custom size, quality, or format from this route.

## Response

A standard image (or, for a video hash, an extracted-frame image) response
with normal HTTP caching headers:

- `200` with the encoded bytes on success.
- `404` if the hash cannot be resolved on any known Blossom server.
- `400` for a malformed preset, hash, or a rejected query parameter.
- `429` with `Retry-After: 1` if the caller's IP has exceeded its budget
  (see below).
- `503` with `Retry-After: 1` if the node is at capacity.

Successful image responses use `Cache-Control: public, max-age=31536000,
immutable` — Blossom content is hash-addressed, so a cache hit is valid
forever. Video-thumbnail responses use a short-lived policy instead, because
a range-probed video source cannot be proven to match its hash without a
full download.

## Rate limiting

Every request spends from three independent per-peer-IP budgets:

1. **`RATE_IP_REQUESTS_PER_MIN`** (default `600`) — charged once per request,
   cache hit or miss. Generous: serving an already-cached derivative is
   cheap.
2. **`RATE_IP_IMAGE_GENERATIONS_PER_MIN`** (default `30`) — charged only on a
   cache miss that requires fresh image decode/resize/encode.
3. **`RATE_IP_VIDEO_GENERATIONS_PER_MIN`** (default `5`) — charged only on a
   cache miss requiring an FFmpeg thumbnail extraction, the most expensive
   path, budgeted far below the image tier.

Exceeding any tier returns `429` before that tier's work starts; the other
two tiers are unaffected. All three are bounded, in-memory, per-process
state — a multi-replica deployment must move them to a shared store before
relying on cluster-wide limits.

## Browser integration

```ts
type Preset = "feed-preview-v1" | "profile-avatar-v1" | "embed-card-v1";

function presetThumbnailUrl(
  baseUrl: string,
  preset: Preset,
  sha256: string,
  extension?: string,
): string {
  const filename = extension ? `${sha256}.${extension}` : sha256;
  return `${baseUrl}/v1/preset/${preset}/${filename}`;
}
```

Use the returned URL directly in `<img src>`, `<video poster>`, preload
links, or `og:image` metadata. There is nothing to fetch, sign, cache-bust,
or refresh beforehand — the URL is stable for as long as the underlying
Blossom hash is stable, which is forever.

Do not construct any other path or query shape against this route. If a UI
context needs an output shape outside the three presets, that is a product
decision to add a new named preset server-side — never a reason to start
accepting client-supplied directives here.

## What NOT to do

- Do not reintroduce a mint/sign round-trip for this route. It solves a
  problem (constraining an open value space) that does not exist here — the
  value space is already three fixed names.
- Do not pass `sourceUrl`, resize directives, or format/quality values to
  this route. It only resolves hash-addressed Blossom media through a fixed
  preset.
- Do not build a second, parallel URL-construction helper in the client for
  this route. `presetThumbnailUrl` above (or an equivalent single function)
  should be the only place that knows this URL shape.

## Relationship to the legacy and signed routes

`/insecure/...`, `/thumb/...` (legacy, `ALLOW_UNSIGNED_URLS`-gated) and the
HMAC-signed `/v1/{key-id}/{signature}/...` routes still exist, unchanged.
They serve a different purpose: arbitrary source URLs, or arbitrary
directives minted by a trusted service holding the signing key. Nostube's
browser client should not use either — it has no signing key, and does not
need one for this route.
