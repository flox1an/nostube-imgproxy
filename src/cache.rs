use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use axum::{
    body::Body,
    http::{header, HeaderValue, StatusCode},
    response::Response,
};
use http::HeaderName;
use sha2::{Digest, Sha256};
use tokio::{fs as tokio_fs, time::sleep};
use tracing::error;
use walkdir::WalkDir;

use crate::{config::AppCfg, error::SvcError, transform::OutFmt};

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

/// Try to serve a response from cache
pub async fn try_serve_cache(path: &Path, mime: &str) -> Result<Option<Response>, SvcError> {
    if let Ok(bytes) = tokio_fs::read(path).await {
        let mut resp = Response::new(Body::from(bytes));
        *resp.status_mut() = StatusCode::OK;
        let headers = resp.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(mime).unwrap());
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
        headers.insert(
            HeaderName::from_static("x-cache"),
            HeaderValue::from_static("hit"),
        );
        return Ok(Some(resp));
    }
    Ok(None)
}

/// Try to read original image from cache
pub async fn try_read_original_cache(path: &Path) -> Result<Option<Vec<u8>>, SvcError> {
    match tokio_fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(_) => Ok(None),
    }
}

/// Write data to cache atomically
pub async fn write_cache_atomic(path: &Path, bytes: &[u8]) -> Result<(), SvcError> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        tokio_fs::create_dir_all(parent).await?;
    }

    let tmp = path.with_extension("tmp");

    // Sync write via std::fs to ensure durability
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
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

/// Run a single cleanup pass
async fn run_cleanup(cfg: &AppCfg) -> Result<(), std::io::Error> {
    let now = SystemTime::now();

    // Clean both original and processed cache directories
    let original_dir = cfg.cache_dir.join("original");
    let processed_dir = cfg.cache_dir.join("processed");

    for cache_dir in [original_dir, processed_dir] {
        if !cache_dir.exists() {
            continue;
        }

        for entry in WalkDir::new(&cache_dir).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let p = entry.path();
            let meta = fs::metadata(p)?;
            let created = meta.created().or_else(|_| meta.modified())?;
            if now.duration_since(created).unwrap_or(Duration::ZERO) > cfg.cache_ttl {
                let _ = fs::remove_file(p);
            }
        }
    }
    Ok(())
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

        assert!(
            !path.with_extension("tmp").exists(),
            "tmp file must be renamed away"
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
        assert!(try_serve_cache(&missing, "image/webp")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn try_serve_cache_marks_hits_with_immutable_caching_headers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.webp");
        write_cache_atomic(&path, b"cached image").await.unwrap();

        let response = try_serve_cache(&path, "image/webp")
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
