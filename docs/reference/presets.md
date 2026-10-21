# Browser presets

Use the preset endpoint for Nostube thumbnails, avatars, embed cards and
anonymous clients. A client constructs a URL directly; no signing key,
Nostr authentication, minting request or expiring token is needed.

## URL format

```text
/v1/preset/{preset}/{sha256}[.{extension}]?xs={server-hint}&as={author-pubkey}
```

`PRESET_THUMBNAILS_ENABLED=true` enables the route by default. It is
independent of `ALLOW_UNSIGNED_URLS`, which controls the legacy routes.
All media routes emit `Access-Control-Allow-Origin: *`.

| Part | Contract |
|---|---|
| `preset` | Exactly one of the published names below; unknown names return `400`. |
| `sha256` | 64 hexadecimal characters. Lowercase is the canonical client form; the server also accepts and normalizes uppercase. |
| `extension` | Optional source extension, used in upstream filenames and media detection. Include the real video extension when known. It does not change output format. |
| `xs` | Optional, repeatable server hint. Only the first `MAX_SERVER_HINTS` entries are considered (default four). |
| `as` | Optional author npub/hex pubkey for kind-10063 server-list lookup. |

**No other query key is accepted.** `f`, `rs`, `q`, `width`, `height`,
`exp` and arbitrary extra parameters are rejected with `400`, not silently
ignored. Output is entirely selected by the preset name.

## Published presets

| Preset | Format | Quality | Resize |
|---|---|---|---|
| `feed-preview-v1` | WebP | 82 | Fit within 480×480, no upscaling |
| `profile-avatar-v1` | WebP | 85 | Fill 160×160, center crop |
| `embed-card-v1` | WebP | 82 | Fit within 1200×630, no upscaling |

Fit output is not guaranteed to have exactly the bounding-box dimensions.
If a product needs a new output shape, add a named server-side preset rather
than allowing client directive overrides on this endpoint.

## Browser integration

Use a single URL-construction helper. Validate the hash and preset at the
boundary where untrusted event metadata enters the application; URL encoding
is not input validation.

```ts
type Preset = 'feed-preview-v1' | 'profile-avatar-v1' | 'embed-card-v1';

function presetThumbnailUrl(
  baseUrl: string,
  preset: Preset,
  sha256: string,
  extension?: string,
  servers: string[] = [],
  author?: string,
): string {
  if (!/^[a-fA-F0-9]{64}$/.test(sha256)) throw new Error('Invalid SHA-256');
  const filename = sha256.toLowerCase() + (extension ? `.${extension}` : '');
  const url = new URL(`/v1/preset/${preset}/${encodeURIComponent(filename)}`, baseUrl);
  for (const server of servers) url.searchParams.append('xs', server);
  if (author) url.searchParams.set('as', author);
  return url.href;
}
```

`baseUrl` is the proxy's origin (for example `https://images.example.com`),
not the documentation site's GitHub Pages URL. Use the result in `<img src>`,
`<video poster>`, preload links or `og:image` metadata. The endpoint returns
image bytes, not JSON. There is no mint response to cache or refresh.

## Responses and caching

- `200`: encoded WebP image, including an extracted video frame or audio cover.
- `304`: persisted derivative matches the supplied `If-None-Match` ETag.
- `400`: invalid preset, hash, rejected query or unsupported source.
- `404`: unresolved source or disabled route.
- `429` / `503` with `Retry-After: 1`: rate budget / capacity exhausted.

Other upstream/processing failures follow the [HTTP error reference](./api.md#errors).

Persisted unsigned results use `public, max-age=31536000, immutable` and an
ETag. This includes video frames scoped to a source origin. Content-sniffed
Blossom HLS and video frames from discovered origins outside the request's
server list are not persisted: one-hour cache policy, no ETag. A stable
preset URL does not prove that a range-probed video matches its hash; see
[video integrity](../guide/how-it-works.md#video-thumbnails).

## Admission is not authorization

Public hashes and fixed output shapes do not need an identity check.
Anonymous users, server renders, crawlers and embeds can use the same URL.
The former NIP-98-authenticated mint/sign round-trip was removed; do not
reintroduce it in a browser integration.

There are three per-client budgets: general requests (`600/min`),
image-generation misses (`30/min`) and video-generation misses (`5/min`).
Processed image hits bypass these checks. Video cache hits skip generation
but may consume the general request tier before origin lookup. A processed
miss can consume a generation budget even if the original is cached or it
joins an existing flight. Budgets are local to one process.

Behind a reverse proxy, configure `TRUSTED_PROXY_CIDRS` or callers share the
proxy IP's limits. See [Configuration](../configuration.md#rate-limiting-and-proxies).

Unauthenticated does not mean harmless input: a caller can still point the
proxy at attacker-controlled media. Keep FFmpeg updated and use the
[container hardening](../operations.md#container-hardening-checklist).

## Other routes

Legacy `/insecure` and `/thumb`, plus HMAC-signed `/v1/{key-id}/{signature}`
routes, support arbitrary directives or direct source URLs. They are
separate integration contracts; never put their parameters on a preset URL.
A browser cannot safely hold an HMAC secret and does not need one for presets.
