# Nostube Image Proxy

A Rust image proxy for Nostr and the web: resize images, extract video
thumbnails and audio cover art, and resolve hash-addressed media across
Blossom servers. JPEG, PNG, WebP and GIF inputs; JPEG, PNG, WebP and AVIF
outputs. Filesystem original/derivative caches, bounded processing and
Prometheus metrics.

**[Documentation](https://flox1an.github.io/nostube-imgproxy/)** ·
[Getting started](docs/guide/getting-started.md) ·
[Configuration](docs/configuration.md) ·
[How it works](docs/guide/how-it-works.md) ·
[HTTP API](docs/reference/api.md) ·
[Browser presets](docs/reference/presets.md) ·
[Operations](docs/operations.md)

## Run locally

```bash
cargo build --release --locked
cargo run --release --locked
curl -fsS http://127.0.0.1:8080/health
```

Install FFmpeg in `PATH` for video thumbnails. The binary defaults to
`127.0.0.1:8080` and `./cache`. AVIF output uses `ravif`; AVIF input is
intentionally unsupported.

## Docker

```bash
docker compose up -d
docker compose logs -f
```

The [Compose file](docker-compose.yml) exposes port 8080 and includes
hardening and resource limits. The standalone image defaults to port
**8081**. See [Getting started](docs/guide/getting-started.md) for image-run
commands and writable-cache permissions.

## Development

```bash
cargo test --locked
cargo fmt --all
cargo clippy --all-targets --all-features
```

The public preset API needs no signing or minting request. Arbitrary-directive
legacy routes are enabled by default; signed capabilities are available for
trusted backends. Read the [API contract](docs/reference/api.md) before
integrating and the [security guidance](docs/operations.md) before exposing
the service publicly.

## Documentation development

The English VitePress site lives in `docs/`, separate from the Rust service:

```bash
cd docs
npm ci
npm run docs:dev
npm run docs:build
npm run docs:preview
```

The build rejects broken internal page links. GitHub Actions builds pull
requests and deploys changes on `main` to GitHub Pages. Agent instructions
in `docs/agents/` are intentionally excluded from the site.

## License

MIT
