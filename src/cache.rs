use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use http::HeaderName;
use sha2::{Digest, Sha256};
use tokio::{fs as tokio_fs, time::sleep};
use tokio_util::io::ReaderStream;
use tracing::error;
use walkdir::WalkDir;

use crate::{config::AppCfg, error::SvcError, transform::OutFmt};

const IMMUTABLE_CACHE_CONTROL: HeaderValue =
    HeaderValue::from_static("public, max-age=31536000, immutable");
const X_CACHE: HeaderName = HeaderName::from_static("x-cache");

/// Generate cache file path for processed images
pub fn cache_path_for(cfg: &AppCfg, request_url: &str, fmt: &OutFmt) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(request_url.as_bytes());
    let hash = hex::encode(hasher.finalize());

    cfg.cache_dir
        .join("processed")
        .join(format!("{}.{}", hash, fmt.extension()))
}

/// Generate cache file path for original images
pub fn original_cache_path_for(cfg: &AppCfg, source_url: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(source_url.as_bytes());
    let hash = hex::encode(hasher.finalize());

    cfg.cache_dir.join("original").join(hash)
}

/// Strong ETag for a cache entry.
///
/// The file stem is already the SHA-256 of the fully-qualified request, so it
/// is a free, exact validator — no extra hashing, and it lets repeat visitors
/// take a 304 instead of the whole payload.
pub fn etag_for(path: &Path) -> Option<HeaderValue> {
    let stem = path.file_stem()?.to_str()?;
    HeaderValue::from_str(&format!("\"{stem}\"")).ok()
}

/// True when the client already holds this exact entity.
fn if_none_match_hits(request_headers: &HeaderMap, etag: &HeaderValue) -> bool {
    request_headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| {
            candidate == "*" || candidate.trim_start_matches("W/") == etag.to_str().unwrap_or("")
        })
}

/// Apply the headers every cached-derivative response carries.
fn decorate(headers: &mut HeaderMap, mime: &str, etag: Option<&HeaderValue>, cache_state: &str) {
    if let Ok(content_type) = HeaderValue::from_str(mime) {
        headers.insert(header::CONTENT_TYPE, content_type);
    }
    headers.insert(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL);
    if let Ok(state) = HeaderValue::from_str(cache_state) {
        headers.insert(X_CACHE, state);
    }
    if let Some(etag) = etag {
        headers.insert(header::ETAG, etag.clone());
    }
}

/// Headers for a freshly produced (not cached) derivative.
pub fn fresh_response_headers(headers: &mut HeaderMap, mime: &str, path: &Path, cache_state: &str) {
    decorate(headers, mime, etag_for(path).as_ref(), cache_state);
}

/// Try to serve a response from cache.
///
/// Streams the file rather than buffering it, and answers `If-None-Match` with
/// a bodyless 304 — by far the cheapest possible hit for an edge node.
pub async fn try_serve_cache(
    path: &Path,
    mime: &str,
    request_headers: &HeaderMap,
) -> Result<Option<Response>, SvcError> {
    let Ok(file) = tokio_fs::File::open(path).await else {
        return Ok(None);
    };
    let len = match file.metadata().await {
        Ok(meta) if meta.is_file() => meta.len(),
        _ => return Ok(None),
    };

    let etag = etag_for(path);

    if let Some(etag) = etag.as_ref() {
        if if_none_match_hits(request_headers, etag) {
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = StatusCode::NOT_MODIFIED;
            decorate(resp.headers_mut(), mime, Some(etag), "hit");
            return Ok(Some(resp));
        }
    }

    let mut resp = Response::new(Body::from_stream(ReaderStream::new(file)));
    *resp.status_mut() = StatusCode::OK;
    let headers = resp.headers_mut();
    decorate(headers, mime, etag.as_ref(), "hit");
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    Ok(Some(resp))
}

/// Try to read original image from cache
pub async fn try_read_original_cache(path: &Path) -> Result<Option<Vec<u8>>, SvcError> {
    match tokio_fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(_) => Ok(None),
    }
}

/// Write data to cache atomically.
///
/// The temporary name carries a process-unique counter: a fixed
/// `<name>.tmp` lets two concurrent writers for the same key interleave into
/// one file and then both rename it, publishing a torn entry that is served as
/// `immutable` for a year.
///
/// There is deliberately no `fsync`: every cached byte is regenerable, and
/// syncing each derivative costs tens of milliseconds on the eMMC/SD storage an
/// edge node usually has.
pub async fn write_cache_atomic(path: &Path, bytes: &[u8]) -> Result<(), SvcError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let parent = path.parent().unwrap_or(Path::new("."));
    tokio_fs::create_dir_all(parent).await?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("entry");
    let tmp = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    if let Err(error) = tokio_fs::write(&tmp, bytes).await {
        let _ = tokio_fs::remove_file(&tmp).await;
        return Err(error.into());
    }
    if let Err(error) = tokio_fs::rename(&tmp, path).await {
        let _ = tokio_fs::remove_file(&tmp).await;
        return Err(error.into());
    }
    Ok(())
}

/// Background janitor loop that cleans up expired cache files
pub async fn janitor_loop(cfg: AppCfg) {
    loop {
        if let Err(e) = run_cleanup(&cfg).await {
            error!(?e, "cleanup error");
        }
        sleep(Duration::from_secs(60)).await; // run every minute
    }
}

/// Run a single cleanup pass.
///
/// The walk plus a `metadata` call per entry is unbounded blocking I/O, so it
/// runs on a blocking thread rather than stalling an async worker every minute.
async fn run_cleanup(cfg: &AppCfg) -> Result<(), std::io::Error> {
    let cache_dir = cfg.cache_dir.clone();
    let cache_ttl = cfg.cache_ttl;

    tokio::task::spawn_blocking(move || {
        let now = SystemTime::now();

        for sub_dir in ["original", "processed"] {
            let dir = cache_dir.join(sub_dir);
            if !dir.exists() {
                continue;
            }

            for entry in WalkDir::new(&dir).into_iter().filter_map(Result::ok) {
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                let meta = std::fs::metadata(path)?;
                let created = meta.created().or_else(|_| meta.modified())?;
                if now.duration_since(created).unwrap_or(Duration::ZERO) > cache_ttl {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        Ok(())
    })
    .await
    .unwrap_or_else(|error| Err(std::io::Error::other(error)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg_with_cache_dir(dir: &Path, ttl: Duration) -> AppCfg {
        AppCfg {
            bind_addr: "127.0.0.1:0".into(),
            cache_dir: dir.to_path_buf(),
            cache_ttl: ttl,
            fetch_timeout: Duration::from_secs(1),
            blossom_failover_timeout: Duration::from_secs(1),
            max_image_bytes: 1024,
            blossom_fallback_servers: Vec::new(),
            blossom_negative_not_found_ttl: Duration::from_secs(1),
            blossom_negative_permanent_ttl: Duration::from_secs(1),
            blossom_negative_transient_ttl: Duration::from_secs(1),
            max_image_dimension: 4096,
            max_decode_alloc_bytes: 64 * 1024 * 1024,
            cpu_concurrency: 1,
            max_inflight_requests: 8,
            request_timeout: Duration::from_secs(5),
            ffmpeg_timeout: Duration::from_secs(5),
        }
    }

    // -----------------------------------------------------------------------
    // Cache key derivation
    // -----------------------------------------------------------------------

    #[test]
    fn cache_path_for_is_deterministic_for_the_same_request() {
        let cfg = cfg_with_cache_dir(Path::new("/tmp/cache"), Duration::from_secs(60));
        let a = cache_path_for(&cfg, "https://e.com/a.png?w=1", &OutFmt::Webp);
        let b = cache_path_for(&cfg, "https://e.com/a.png?w=1", &OutFmt::Webp);
        assert_eq!(a, b);
    }

    #[test]
    fn cache_path_for_separates_distinct_requests() {
        let cfg = cfg_with_cache_dir(Path::new("/tmp/cache"), Duration::from_secs(60));
        let a = cache_path_for(&cfg, "https://e.com/a.png", &OutFmt::Webp);
        let b = cache_path_for(&cfg, "https://e.com/b.png", &OutFmt::Webp);
        assert_ne!(a, b, "different source URLs must not collide");
    }

    #[test]
    fn cache_path_for_separates_output_formats() {
        let cfg = cfg_with_cache_dir(Path::new("/tmp/cache"), Duration::from_secs(60));
        let webp = cache_path_for(&cfg, "https://e.com/a.png", &OutFmt::Webp);
        let avif = cache_path_for(&cfg, "https://e.com/a.png", &OutFmt::Avif);
        assert_ne!(webp, avif, "one request must not serve two formats");
        assert_eq!(webp.extension().unwrap(), "webp");
        assert_eq!(avif.extension().unwrap(), "avif");
    }

    #[test]
    fn cache_path_for_lands_under_the_processed_directory() {
        let cfg = cfg_with_cache_dir(Path::new("/tmp/cache"), Duration::from_secs(60));
        let path = cache_path_for(&cfg, "https://e.com/a.png", &OutFmt::Png);
        assert_eq!(path.parent().unwrap(), Path::new("/tmp/cache/processed"));
    }

    #[test]
    fn original_cache_path_for_lands_under_the_original_directory() {
        let cfg = cfg_with_cache_dir(Path::new("/tmp/cache"), Duration::from_secs(60));
        let path = original_cache_path_for(&cfg, "https://e.com/a.png");
        assert_eq!(path.parent().unwrap(), Path::new("/tmp/cache/original"));
    }

    #[test]
    fn original_and_processed_paths_never_collide_for_one_url() {
        let cfg = cfg_with_cache_dir(Path::new("/tmp/cache"), Duration::from_secs(60));
        let url = "https://e.com/a.png";
        assert_ne!(
            original_cache_path_for(&cfg, url),
            cache_path_for(&cfg, url, &OutFmt::Png)
        );
    }

    // -----------------------------------------------------------------------
    // Atomic writes and reads
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn write_cache_atomic_round_trips_through_the_original_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("blob.bin");
        let payload = b"cached bytes".to_vec();

        write_cache_atomic(&path, &payload).await.unwrap();

        let read_back = try_read_original_cache(&path).await.unwrap();
        assert_eq!(read_back, Some(payload));
    }

    #[tokio::test]
    async fn write_cache_atomic_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c.bin");

        write_cache_atomic(&path, b"x").await.unwrap();

        assert!(path.exists(), "file should exist at {path:?}");
    }

    #[tokio::test]
    async fn write_cache_atomic_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.bin");

        write_cache_atomic(&path, b"payload").await.unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp files left behind: {leftovers:?}");
    }

    #[tokio::test]
    async fn write_cache_atomic_keeps_concurrent_writers_from_tearing_an_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hot.webp");
        // Two payloads of different lengths: a shared tmp path would let the
        // writes interleave and publish something that is neither.
        let short = vec![b'a'; 4096];
        let long = vec![b'b'; 65_536];

        let writers: Vec<_> = (0..16)
            .map(|index| {
                let path = path.clone();
                let payload = if index % 2 == 0 {
                    short.clone()
                } else {
                    long.clone()
                };
                tokio::spawn(async move { write_cache_atomic(&path, &payload).await })
            })
            .collect();
        for writer in writers {
            writer.await.unwrap().unwrap();
        }

        let published = try_read_original_cache(&path).await.unwrap().unwrap();
        assert!(
            published == short || published == long,
            "published entry is torn: {} bytes",
            published.len()
        );
    }

    #[tokio::test]
    async fn write_cache_atomic_overwrites_an_existing_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.bin");

        write_cache_atomic(&path, b"first").await.unwrap();
        write_cache_atomic(&path, b"second").await.unwrap();

        assert_eq!(
            try_read_original_cache(&path).await.unwrap(),
            Some(b"second".to_vec())
        );
    }

    #[tokio::test]
    async fn try_read_original_cache_reports_a_miss_for_absent_files() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.bin");
        assert_eq!(try_read_original_cache(&missing).await.unwrap(), None);
    }

    // -----------------------------------------------------------------------
    // Serving from cache
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn try_serve_cache_reports_a_miss_for_absent_files() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.webp");
        assert!(try_serve_cache(&missing, "image/webp", &HeaderMap::new())
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn try_serve_cache_marks_hits_with_immutable_caching_headers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.webp");
        write_cache_atomic(&path, b"cached image").await.unwrap();

        let response = try_serve_cache(&path, "image/webp", &HeaderMap::new())
            .await
            .unwrap()
            .expect("cache hit");

        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(headers[header::CONTENT_TYPE], "image/webp");
        assert_eq!(headers[HeaderName::from_static("x-cache")], "hit");
        assert_eq!(
            headers[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(headers[header::ETAG], "\"blob\"");
        assert_eq!(headers[header::CONTENT_LENGTH], "12");
    }

    #[tokio::test]
    async fn try_serve_cache_answers_a_matching_validator_with_304() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.webp");
        write_cache_atomic(&path, b"cached image").await.unwrap();

        let mut request_headers = HeaderMap::new();
        request_headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"blob\""));

        let response = try_serve_cache(&path, "image/webp", &request_headers)
            .await
            .unwrap()
            .expect("cache hit");

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::ETAG], "\"blob\"");
    }

    #[tokio::test]
    async fn try_serve_cache_accepts_a_weak_or_listed_validator() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.webp");
        write_cache_atomic(&path, b"cached image").await.unwrap();

        for value in ["W/\"blob\"", "\"other\", \"blob\"", "*"] {
            let mut request_headers = HeaderMap::new();
            request_headers.insert(header::IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
            let response = try_serve_cache(&path, "image/webp", &request_headers)
                .await
                .unwrap()
                .expect("cache hit");
            assert_eq!(
                response.status(),
                StatusCode::NOT_MODIFIED,
                "validator {value} should have matched"
            );
        }
    }

    #[tokio::test]
    async fn try_serve_cache_sends_the_body_for_a_stale_validator() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.webp");
        write_cache_atomic(&path, b"cached image").await.unwrap();

        let mut request_headers = HeaderMap::new();
        request_headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"stale\""));

        let response = try_serve_cache(&path, "image/webp", &request_headers)
            .await
            .unwrap()
            .expect("cache hit");

        assert_eq!(response.status(), StatusCode::OK);
    }

    // -----------------------------------------------------------------------
    // Janitor cleanup
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn cleanup_retains_entries_within_the_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with_cache_dir(dir.path(), Duration::from_secs(3600));
        let fresh = dir.path().join("processed").join("fresh.webp");
        write_cache_atomic(&fresh, b"fresh").await.unwrap();

        run_cleanup(&cfg).await.unwrap();

        assert!(fresh.exists(), "entry inside the TTL must survive");
    }

    #[tokio::test]
    async fn cleanup_removes_entries_older_than_the_ttl() {
        let dir = tempfile::tempdir().unwrap();
        // A zero TTL makes every existing file immediately expired.
        let cfg = cfg_with_cache_dir(dir.path(), Duration::ZERO);
        let stale = dir.path().join("processed").join("stale.webp");
        let original = dir.path().join("original").join("stale.bin");
        write_cache_atomic(&stale, b"stale").await.unwrap();
        write_cache_atomic(&original, b"stale").await.unwrap();

        // Ensure a non-zero age even on coarse-grained filesystem clocks.
        tokio::time::sleep(Duration::from_millis(20)).await;
        run_cleanup(&cfg).await.unwrap();

        assert!(!stale.exists(), "expired processed entry must be removed");
        assert!(!original.exists(), "expired original entry must be removed");
    }

    #[tokio::test]
    async fn cleanup_succeeds_when_cache_directories_do_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with_cache_dir(&dir.path().join("absent"), Duration::from_secs(60));
        // Must not error just because nothing has been cached yet.
        run_cleanup(&cfg).await.unwrap();
    }
}
