# Getting started

Nostube Image Proxy is a Rust service that turns public source URLs and
Blossom content hashes into images. No database is needed. Start with a
container or build the binary locally.

## Run the published container

The standalone image listens on **8081**, not the binary's default 8080.
A named Docker volume avoids host-directory ownership problems:

```bash
docker volume create imgproxy-cache
docker run -d --name imgproxy \
  -p 127.0.0.1:8080:8081 \
  -v imgproxy-cache:/cache \
  ghcr.io/flox1an/nostube-imgproxy:main

curl -fsS http://127.0.0.1:8080/health
docker logs -f imgproxy
```

Expected health response: `OK`. FFmpeg is included in the image. Mount
`/cache` persistently; it is the only persistent state. `:main` moves as
updates are published; pin an image digest for reproducible deployments.

Stop and remove the container with `docker stop imgproxy` and
`docker rm imgproxy`; the named volume survives both.

## Use the repository's Compose file

```bash
git clone https://github.com/flox1an/nostube-imgproxy.git
cd nostube-imgproxy
mkdir -p cache
# On Linux, make the bind-mounted cache writable by the container's UID 1000.
sudo chown 1000:1000 cache
docker compose up -d
docker compose logs -f
curl -fsS http://127.0.0.1:8080/health
```

Compose explicitly sets `BIND_ADDR=0.0.0.0:8080`, so its published port
is 8080. It adds a read-only root filesystem, writable `/tmp`, dropped
capabilities, log rotation and resource limits. This example publishes on
all host interfaces; behind a reverse proxy, remove the `ports` mapping and
use a private Docker network instead. See [Operations](../operations.md).

To build the image locally instead of pulling the published image:

```bash
docker build -t rust-imgproxy .
docker run --rm -p 127.0.0.1:8080:8081 -v imgproxy-cache:/cache rust-imgproxy
```

## Build the binary

Install a stable Rust toolchain with [rustup](https://rustup.rs) and a native
C/C++ toolchain (Xcode Command Line Tools on macOS or `build-essential` on
Debian/Ubuntu). Install FFmpeg for video thumbnails:

```bash
brew install ffmpeg                   # macOS
# or
sudo apt install build-essential pkg-config nasm ffmpeg  # Debian/Ubuntu
```

AVIF output is encoded by `ravif`; the current input decoder does **not**
use dav1d or accept AVIF. Old meson/ninja/dav1d notes in the build wrapper
are not prerequisites of the current decoder. Use Cargo directly:

```bash
cargo build --release --locked
cargo run --release --locked
```

The binary defaults to `127.0.0.1:8080` and `./cache`. It creates the cache
directories at startup. Set environment variables before starting; no
`.env` loader or configuration file is built into the binary:

```bash
BIND_ADDR=127.0.0.1:3000 MAX_FFMPEG_CONCURRENT=4 \
  ./target/release/rust-imgproxy
```

## Make a request

For a live, public JPEG source, this legacy request produces a WebP bounded
to 480×480:

```bash
curl --fail-with-body -D - \
  'http://127.0.0.1:8080/insecure/f:webp/q:82/rs:fit:480:480/plain/https%3A%2F%2Fwww.w3.org%2FPeople%2Fmimasa%2Ftest%2Fimgformat%2Fimg%2Fw3c_home.jpg' \
  -o /tmp/imgproxy-preview.webp
```

Repeat it and look for `X-Cache: hit`. A cold request normally reports
`X-Cache: miss`; simultaneous requests may report `coalesced`. The source
must be public: even a local proxy instance refuses upstream localhost and
private networks.

For a browser integration with Blossom media, prefer
[fixed presets](../reference/presets.md) over arbitrary-directive legacy
URLs. See [HTTP API](../reference/api.md) for signing and directive syntax.

## Next steps

- [Configuration](../configuration.md): all environment variables and defaults.
- [How it works](./how-it-works.md): discovery, media handling and cache rules.
- [Operations](../operations.md): production sizing, hardening and monitoring.
