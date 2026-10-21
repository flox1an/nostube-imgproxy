---
layout: home

hero:
  name: Nostube Image Proxy
  text: Media in the right shape.
  tagline: A Rust image proxy for Nostr and the web. Resize images, extract video thumbnails and audio cover art, and resolve media across Blossom servers.
  actions:
    - theme: brand
      text: Get started
      link: /guide/getting-started
    - theme: alt
      text: Configuration
      link: /configuration
    - theme: alt
      text: HTTP API
      link: /reference/api

features:
  - title: Images, video and audio
    details: Turn source images, video frames and embedded album covers into JPEG, PNG, WebP or AVIF derivatives.
    link: /guide/how-it-works
  - title: Built for Blossom
    details: Resolve content hashes using server hints, author server lists and NIP-94 discovery, with bounded failover.
    link: /guide/how-it-works#blossom-discovery
  - title: Browser-ready presets
    details: Stable feed previews, profile avatars and embed cards. No signing key or minting request in the browser.
    link: /reference/presets
  - title: Cache once, reuse
    details: Separate original and processed disk caches, conditional requests and coalesced concurrent work.
    link: /guide/how-it-works#caching
  - title: Bounded resource use
    details: Explicit fetch, decode, CPU, FFmpeg and disk budgets, plus per-client rate limits for generation work.
    link: /configuration
  - title: Operate with confidence
    details: A hardened Compose example, health checks, Prometheus metrics and a Grafana dashboard.
    link: /operations
---
