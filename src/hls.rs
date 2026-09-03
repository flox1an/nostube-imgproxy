//! HLS (`.m3u8`) playlist support.
//!
//! A playlist is not media: it is a recipe naming further URLs (segments,
//! init parts, keys, variant playlists). FFmpeg fed a playlist directly would
//! fetch those URLs itself, evading the guarded client, the byte budget and
//! the redirect policy that every other remote fetch in this service goes
//! through. [`rewrite_playlist`] instead pins every referenced URI to a
//! short-lived loopback gateway ([`HlsMediaProxy`]) that re-resolves them
//! through the guarded `reqwest` client, so FFmpeg only ever talks to
//! loopback while every real byte still flows through the same guards,
//! deadline and shared probe budget as range-probed videos.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, RwLock,
};

use axum::{
    body::Body,
    extract::{Path as AxPath, State},
    http::{header, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use tokio::{sync::oneshot, task::JoinHandle};
use url::Url;

use crate::error::SvcError;

/// Hard cap on how many distinct URIs one playlist (recursively) may pin to
/// the gateway. Bounds memory against a hostile playlist; beyond the cap the
/// referenced lines are dropped, which makes FFmpeg fail cleanly instead of
/// silently fetching a remote URL itself.
const MAX_PLAYLIST_TARGETS: usize = 4096;

/// A playlist published under a non-`.m3u8` name (real-world Blossom blobs
/// use `.txt`) is detected by this prefix, not by its extension.
const HLS_MAGIC: &[u8] = b"#EXTM3U";

/// FFmpeg input format name for HLS playlists.
pub const HLS_DEMUXER: &str = "hls";

/// AES-128 key fetches go through FFmpeg's `crypto` protocol; without it an
/// encrypted playlist fails even though the key bytes flow through our
/// gateway like any other target.
pub const HLS_PROTOCOL_WHITELIST: &str = "file,http,tcp,crypto";

/// Playlist bodies above this are rejected outright: no real media playlist
/// needs 4 MB, and the budget below is what bounds total work.
const MAX_PLAYLIST_BYTES: usize = 4 * 1024 * 1024;

/// `#EXTM3U` sniffing is byte-exact and cheap; no content-type trust needed
/// (Blossom mirrors serve playlists as `text/plain` and segments as `bin`).
pub fn is_hls_playlist(bytes: &[u8]) -> bool {
    bytes.starts_with(HLS_MAGIC)
}

/// Resolve one URI reference against a playlist's own URL. Absolute URLs pass
/// through; relative names (the Blossom HLS layout: bare `<hash>.mp4`) join
/// the playlist's address so segments resolve against the same server.
fn resolve_target(base: &Url, uri: &str) -> Option<String> {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        return None;
    }
    base.join(trimmed).ok().map(|url| url.to_string())
}

/// Replace every `URI="…"` attribute value on a tag line via `map`.
fn map_tag_uris(line: &str, map: &mut impl FnMut(&str) -> Option<String>) -> String {
    let mut output = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(position) = rest.find("URI=\"") {
        let (head, tail) = rest.split_at(position + "URI=\"".len());
        output.push_str(head);
        let Some(close) = tail.find('"') else {
            output.push_str(tail);
            return output;
        };
        let (uri, remainder) = tail.split_at(close);
        // A dropped registration (unresolvable, or over the target cap)
        // becomes an empty URI: FFmpeg fails to open it cleanly instead of
        // fetching the original remote location outside the gateway.
        if let Some(replacement) = map(uri) {
            output.push_str(&replacement);
        }
        output.push('"');
        rest = &remainder[1..];
    }
    output.push_str(rest);
    output
}

/// FFmpeg's HLS demuxer refuses playlist URLs whose path extension is not in
/// its allowlist, and our gateway paths carry no real name. Mirror the
/// target's own extension when recognised; default to `.mp4` (the Blossom
/// fMP4 segment shape) or map `.m3u` onto `.m3u8`. Content sniffing, not the
/// extension, picks the actual demuxer — the suffix only passes FFmpeg's
/// URL gate.
fn gateway_extension(target: &str) -> &'static str {
    let path = target.split(['?', '#']).next().unwrap_or(target);
    let Some(extension) = path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
    else {
        return ".mp4";
    };
    match extension.as_str() {
        "m3u" | "m3u8" => ".m3u8",
        "aac" => ".aac",
        "avi" => ".avi",
        "flv" => ".flv",
        "m4s" => ".m4s",
        "m4a" => ".m4a",
        "m4v" => ".m4v",
        "mov" => ".mov",
        "mp2" => ".mp2",
        "mp3" => ".mp3",
        "mpeg" | "mpegts" => ".mpegts",
        "ogg" => ".ogg",
        "ts" => ".ts",
        "webm" => ".webm",
        _ => ".mp4",
    }
}

/// A rewritten playlist plus the targets its lines were pinned to, in
/// registration order (the gateway addresses targets by index).
pub struct RewrittenPlaylist {
    pub body: Vec<u8>,
    pub targets: Vec<String>,
}

/// Rewrite an HLS playlist so every referenced URI points at a to-be-started
/// loopback gateway instead of its original location.
///
/// Segment lines (anything not starting with `#`) and `URI="…"` attributes on
/// tag lines (`#EXT-X-MAP`, `#EXT-X-KEY`, `#EXT-X-STREAM-INF`, …) are
/// resolved against `base` — the playlist's own URL — and replaced with
/// relative gateway names (`s0.mp4`, `s1.ts`, …). Lines whose URI cannot be
/// resolved, or that arrive after the target cap, are dropped: a missing
/// segment fails FFmpeg cleanly, a remote URL would bypass the gateway.
pub fn rewrite_playlist(bytes: &[u8], base: &str) -> Option<RewrittenPlaylist> {
    if !is_hls_playlist(bytes) {
        return None;
    }
    let base = Url::parse(base).ok()?;
    let text = std::str::from_utf8(bytes).ok()?;
    let mut targets: Vec<String> = Vec::new();

    let mut body = String::with_capacity(text.len());
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            body.push('\n');
            continue;
        }
        let mut register = |uri: &str| -> Option<String> {
            let resolved = resolve_target(&base, uri)?;
            if targets.len() >= MAX_PLAYLIST_TARGETS {
                return None;
            }
            targets.push(resolved.clone());
            Some(format!(
                "s{}{}",
                targets.len() - 1,
                gateway_extension(&resolved)
            ))
        };
        if let Some(tag) = line.strip_prefix('#') {
            let rewritten = map_tag_uris(tag, &mut register);
            body.push('#');
            body.push_str(&rewritten);
        } else if let Some(gateway_name) = register(line) {
            body.push_str(&gateway_name);
            // else: the line is dropped — unresolvable, or over the target cap
        }
        body.push('\n');
    }
    Some(RewrittenPlaylist {
        body: body.into_bytes(),
        targets,
    })
}

#[derive(Clone)]
struct HlsProxyState {
    token: String,
    playlist: Arc<Vec<u8>>,
    /// Append-only target table; indices are pinned at rewrite time and never
    /// reused, so in-flight lookups stay valid while variants extend it.
    targets: Arc<RwLock<Vec<String>>>,
    http: reqwest::Client,
    deadline: std::time::Instant,
    remaining_bytes: Arc<AtomicU64>,
}

/// A short-lived, loopback-only gateway for one HLS playlist.
///
/// Serves the rewritten playlist at `/{token}/p` and resolves its pinned
/// targets at `/{token}/{name}` through the guarded client, sharing one byte
/// budget across every response (playlist bodies included). A fetched target
/// that turns out to be another playlist (master → variant) is rewritten on
/// the fly with fresh target slots.
pub struct HlsMediaProxy {
    pub input_url: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl HlsMediaProxy {
    /// Start the gateway for an already-fetched playlist body whose original
    /// address was `base` (relative segment names resolve against it).
    pub async fn start(
        playlist: Vec<u8>,
        base: &str,
        http: reqwest::Client,
        deadline: std::time::Instant,
        remaining_bytes: Arc<AtomicU64>,
    ) -> Result<Self, SvcError> {
        let rewritten = rewrite_playlist(&playlist, base)
            .ok_or(SvcError::BadRequest("invalid HLS playlist"))?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(SvcError::Io)?;
        let address = listener.local_addr().map_err(SvcError::Io)?;
        let token = crate::thumbnail::proxy_token();
        let state = HlsProxyState {
            token: token.clone(),
            playlist: Arc::new(rewritten.body),
            targets: Arc::new(RwLock::new(rewritten.targets)),
            http,
            deadline,
            remaining_bytes,
        };
        let app = Router::new()
            .route("/{token}/p", get(serve_playlist))
            .route("/{token}/{name}", get(serve_target))
            .with_state(state);
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = receiver.await;
                })
                .await;
        });
        Ok(Self {
            input_url: format!("http://127.0.0.1:{}/{token}/p", address.port()),
            shutdown: Some(shutdown),
            task,
        })
    }
}

impl Drop for HlsMediaProxy {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.abort();
    }
}

/// Debit `len` bytes from the shared probe budget; false once exhausted.
fn spend_budget(remaining_bytes: &AtomicU64, len: u64) -> bool {
    remaining_bytes
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
            left.checked_sub(len)
        })
        .is_ok()
}

/// Fetch a playlist through the guarded client (redirects followed), bounded
/// by the deadline and the hard playlist-size cap, and debit its bytes from
/// the shared probe budget so segment fetches inherit what is left.
pub async fn fetch_playlist(
    http: &reqwest::Client,
    url: &str,
    deadline: std::time::Instant,
    remaining_bytes: &Arc<AtomicU64>,
) -> Result<Vec<u8>, SvcError> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err(SvcError::UpstreamError(504));
    }
    let response = tokio::time::timeout(remaining, http.get(url).send())
        .await
        .map_err(|_| SvcError::UpstreamError(504))?
        .map_err(SvcError::Fetch)?;
    if !response.status().is_success() {
        return Err(SvcError::UpstreamError(response.status().as_u16()));
    }
    if response.content_length().unwrap_or(0) > MAX_PLAYLIST_BYTES as u64 {
        return Err(SvcError::UpstreamError(413));
    }
    let body = tokio::time::timeout(remaining, response.bytes())
        .await
        .map_err(|_| SvcError::UpstreamError(504))?
        .map_err(SvcError::Fetch)?;
    if body.len() > MAX_PLAYLIST_BYTES {
        return Err(SvcError::UpstreamError(413));
    }
    if !spend_budget(remaining_bytes, body.len() as u64) {
        return Err(SvcError::UpstreamError(413));
    }
    Ok(body.to_vec())
}

/// Gateway target name (`s12.mp4`) → table index (`12`).
fn parse_target_index(name: &str) -> Option<usize> {
    name.strip_prefix('s')?.split('.').next()?.parse().ok()
}

async fn serve_playlist(
    State(state): State<HlsProxyState>,
    AxPath(token): AxPath<String>,
) -> Result<Response, StatusCode> {
    if token != state.token {
        return Err(StatusCode::NOT_FOUND);
    }
    let mut response = Response::new(Body::from((*state.playlist).clone()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/vnd.apple.mpegurl"),
    );
    Ok(response)
}

/// Serve one pinned target: fetched fully through the guarded client under
/// the shared budget. A target that turns out to be another playlist is
/// rewritten on the fly (master → variant chains) with fresh target slots.
async fn serve_target(
    State(state): State<HlsProxyState>,
    AxPath((token, name)): AxPath<(String, String)>,
) -> Result<Response, StatusCode> {
    if token != state.token {
        return Err(StatusCode::NOT_FOUND);
    }
    let index = parse_target_index(&name).ok_or(StatusCode::NOT_FOUND)?;
    let url = {
        let table = state.targets.read().expect("target table lock");
        table.get(index).cloned()
    };
    let Some(url) = url else {
        return Err(StatusCode::NOT_FOUND);
    };
    let remaining = state
        .deadline
        .saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err(StatusCode::GATEWAY_TIMEOUT);
    }
    let budget = state.remaining_bytes.load(Ordering::Acquire);
    let upstream = tokio::time::timeout(remaining, state.http.get(&url).send())
        .await
        .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !upstream.status().is_success() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let mime = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    if upstream.content_length().unwrap_or(0) > budget {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let body = tokio::time::timeout(remaining, upstream.bytes())
        .await
        .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if body.len() as u64 > budget || !spend_budget(&state.remaining_bytes, body.len() as u64) {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    if is_hls_playlist(&body) {
        // Variant playlist: its own targets are relative to *its* URL, not to
        // the master's, and get fresh slots in the shared append-only table.
        let rewritten = rewrite_playlist(&body, &url).ok_or(StatusCode::BAD_GATEWAY)?;
        state
            .targets
            .write()
            .expect("target table lock")
            .extend(rewritten.targets);
        let mut response = Response::new(Body::from(rewritten.body));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/vnd.apple.mpegurl"),
        );
        return Ok(response);
    }

    let mut response = Response::new(Body::from(body));
    if let Ok(value) = header::HeaderValue::from_str(&mime) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    Ok(response)
}

/// Stream an upstream response while debiting every byte from the shared
/// probe budget; an exhausted budget aborts the stream so FFmpeg fails
/// instead of silently reading past the allowance. Used for range-probed
/// video responses, which can be huge and must not be buffered.
pub fn budget_counting_body(
    stream: impl futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    remaining_bytes: Arc<AtomicU64>,
) -> Body {
    use futures_util::StreamExt;
    Body::from_stream(stream.map(move |chunk| match chunk {
        Ok(chunk) => {
            let len = chunk.len() as u64;
            spend_budget(&remaining_bytes, len)
                .then_some(chunk)
                .ok_or_else(|| std::io::Error::other("video probe byte budget exhausted"))
        }
        Err(error) => Err(std::io::Error::other(error)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use tokio::sync::Semaphore;

    fn rewrite(text: &str, base: &str) -> (String, Vec<String>) {
        let out = rewrite_playlist(text.as_bytes(), base).expect("playlist rewrites");
        (
            String::from_utf8(out.body).expect("rewritten body is utf-8"),
            out.targets,
        )
    }

    #[test]
    fn rewrite_resolves_relative_segment_and_map_uris() {
        let (body, targets) = rewrite(
            "#EXTM3U\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:4.0,\n\
             seg1.mp4\n",
            "https://server.example/blob/playlist.txt",
        );
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0], "https://server.example/blob/init.mp4");
        assert_eq!(targets[1], "https://server.example/blob/seg1.mp4");
        assert!(body.contains("#EXT-X-MAP:URI=\"s0.mp4\""));
        assert!(body.contains("s1.mp4\n"));
        assert!(!body.contains("seg1.mp4"));
    }

    #[test]
    fn rewrite_keeps_absolute_uris_blank_lines_and_tags_intact() {
        let (body, targets) = rewrite(
            "#EXTM3U\n\n\
             #EXT-X-VERSION:6\n\
             https://other.example/x.ts\n",
            "https://server.example/blob/playlist.txt",
        );
        assert_eq!(targets, vec!["https://other.example/x.ts".to_string()]);
        assert!(body.contains("\n\n"));
        assert!(body.contains("#EXT-X-VERSION:6"));
        assert!(body.contains("s0.ts"));
        assert!(!body.contains("other.example"));
    }

    #[test]
    fn rewrite_defaults_unknown_extensions_for_gate_passing() {
        // Blossom serves segments as `.bin`; FFmpeg's extension gate only
        // knows media suffixes, so the gateway name defaults to `.mp4` while
        // the registered URL stays untouched.
        let (body, targets) = rewrite(
            "#EXTM3U\n\
             seg.bin\n",
            "https://server.example/blob/playlist.txt",
        );
        assert_eq!(
            targets,
            vec!["https://server.example/blob/seg.bin".to_string()]
        );
        assert!(body.contains("s0.mp4"));
        assert!(!body.contains(".bin"));
    }

    #[test]
    fn rewrite_neutralises_unresolvable_uris_instead_of_leaking_them() {
        let (body, targets) = rewrite(
            "#EXTM3U\n\
             #EXT-X-MAP:URI=\"http://[\"\n\
             http://[.bad\n\
             seg.mp4\n",
            "https://server.example/blob/playlist.txt",
        );
        // `http://[` cannot parse: the attribute's URI is emptied (FFmpeg
        // fails to open it cleanly) and the unresolvable segment line is
        // dropped outright, so nothing unparseable survives into the body.
        assert_eq!(
            targets,
            vec!["https://server.example/blob/seg.mp4".to_string()]
        );
        assert!(body.contains("#EXT-X-MAP:URI=\"\""));
        assert!(!body.contains("http://["));
    }

    #[test]
    fn rewrite_rejects_non_playlist_or_non_url_input() {
        assert!(rewrite_playlist(b"not a playlist", "https://s.example/p.m3u8").is_none());
        assert!(rewrite_playlist(b"#EXTM3U", "not a url").is_none());
    }

    #[test]
    fn map_tag_uris_rewrites_only_quoted_uri_attributes() {
        let mut count = 0;
        let out = map_tag_uris(
            "EXT-X-KEY:METHOD=AES-128,URI=\"k.key\",IV=0x1,UNRELATED=\"URI=nope\"",
            &mut |_uri| {
                count += 1;
                Some(format!("s{count}.key"))
            },
        );
        assert_eq!(
            out,
            "EXT-X-KEY:METHOD=AES-128,URI=\"s1.key\",IV=0x1,UNRELATED=\"URI=nope\""
        );
        assert_eq!(count, 1);
    }

    #[test]
    fn is_hls_playlist_requires_exact_prefix() {
        assert!(is_hls_playlist(b"#EXTM3U\n#EXT-X-VERSION:6"));
        assert!(!is_hls_playlist(b"#EXTX"));
        // No BOM tolerance: FFmpeg would reject it too, so refusing early is
        // the honest answer.
        assert!(!is_hls_playlist(b"\xef\xbb\xbf#EXTM3U"));
    }

    #[test]
    fn spend_budget_rejects_once_exhausted() {
        let budget = AtomicU64::new(10);
        assert!(spend_budget(&budget, 6));
        assert!(spend_budget(&budget, 4));
        assert!(!spend_budget(&budget, 1));
        assert_eq!(budget.load(Ordering::Acquire), 0);
    }

    #[test]
    fn parse_target_index_accepts_indexed_names_only() {
        assert_eq!(parse_target_index("s12.mp4"), Some(12));
        assert_eq!(parse_target_index("s0.ts"), Some(0));
        assert_eq!(parse_target_index("s3"), Some(3));
        assert_eq!(parse_target_index("p"), None);
        assert_eq!(parse_target_index("nope.mp4"), None);
    }

    /// FFmpeg-generated fMP4 HLS VOD served over loopback HTTP, extracted
    /// through the gateway exactly like a production request. Skips itself
    /// (never fails) when no `ffmpeg` binary is installed.
    #[tokio::test]
    async fn extraction_of_served_hls_vod_yields_webp_thumbnail() {
        let _ = tracing_subscriber::fmt::try_init();
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_or(true, |status| !status.success())
        {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        crate::init_crypto_provider();

        let dir = tempfile::tempdir().expect("tempdir");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=128x96:rate=10:duration=2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-f",
                "hls",
                "-hls_time",
                "1",
                "-hls_playlist_type",
                "vod",
                "-hls_segment_type",
                "fmp4",
            ])
            .arg(dir.path().join("live.m3u8"))
            .status()
            .expect("spawn ffmpeg");
        assert!(status.success(), "fixture generation failed");

        // Static file server: the playlist at /live.m3u8, everything else
        // (init + segments) from the fixture directory.
        let root = dir.path().to_path_buf();
        let app = Router::new().route(
            "/{file}",
            get({
                let root = root.clone();
                move |AxPath(file): AxPath<String>| async move {
                    let path = root.join(&file);
                    match tokio::fs::read(&path).await {
                        Ok(bytes) => ([(header::CONTENT_TYPE, "application/octet-stream")], bytes)
                            .into_response(),
                        Err(_) => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!(
            "http://127.0.0.1:{}/live.m3u8",
            listener.local_addr().unwrap().port()
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let playlist = tokio::fs::read(dir.path().join("live.m3u8"))
            .await
            .expect("read fixture playlist");
        assert!(is_hls_playlist(&playlist), "fixture must be an m3u8");

        let semaphore = Arc::new(Semaphore::new(1));
        let thumbnail = crate::thumbnail::extract_thumbnail_from_verified_playlist(
            &playlist,
            &base,
            &semaphore,
            &reqwest::Client::new(),
            16 * 1024 * 1024,
            1024 * 1024,
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(30),
        )
        .await
        .expect("HLS extraction succeeds");
        assert!(thumbnail.starts_with(b"RIFF"), "thumbnail must be a WebP");
        assert!(thumbnail.len() > 100, "thumbnail must have image bytes");
    }
}
