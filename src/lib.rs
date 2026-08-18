//! Image and video thumbnail proxy with Blossom/Nostr blob resolution.
//!
//! The binary in `main.rs` is a thin wrapper over this library so that
//! integration tests under `tests/` can exercise the same code paths.

pub mod blossom;
pub mod cache;
pub mod config;
pub mod cpu;
pub mod error;
pub mod fetch;
pub mod metrics;
pub mod network_policy;
pub mod preset;
pub mod ratelimit;
pub mod server;
pub mod signing;
pub mod singleflight;
pub mod thumbnail;
pub mod transform;

/// Install the process-wide rustls [`CryptoProvider`].
///
/// `reqwest` is built with `rustls-no-provider` so that it does not drag in
/// aws-lc-rs alongside the ring provider that `nostr-sdk`'s websocket stack
/// pulls in; two providers on one rustls make the process default ambiguous
/// and every TLS handshake fails. The trade-off is that the provider must be
/// installed explicitly exactly once before any client is built.
///
/// Idempotent and safe to call from any entry point (binary, tests, benches).
pub fn init_crypto_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // Only errors if another provider was already installed, which is
        // equally acceptable: some provider is active either way.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
