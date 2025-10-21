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

/// Generate cache file path from request URL and format
pub fn cache_path_for(cfg: &AppCfg, request_url: &str, fmt: &OutFmt) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(request_url.as_bytes());
    let hash = hex::encode(hasher.finalize());

    cfg.cache_dir
        .join(format!("{}.{}", hash, fmt.extension()))
}

/// Try to serve a response from cache
pub async fn try_serve_cache(path: &Path, mime: &str) -> Result<Option<Response>, SvcError> {
    if let Ok(bytes) = tokio_fs::read(path).await {
        let mut resp = Response::new(Body::from(bytes));
        *resp.status_mut() = StatusCode::OK;
        let headers = resp.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(mime).unwrap(),
        );
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=3600, stale-while-revalidate=600"),
        );
        headers.insert(
            HeaderName::from_static("x-cache"),
            HeaderValue::from_static("hit"),
        );
        return Ok(Some(resp));
    }
    Ok(None)
}

/// Write data to cache atomically
pub async fn write_cache_atomic(path: &Path, bytes: &[u8]) -> Result<(), SvcError> {
    let tmp = path.with_extension("tmp");

    // Sync write via std::fs to ensure durability
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
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
    for entry in WalkDir::new(&cfg.cache_dir)
        .into_iter()
        .filter_map(Result::ok)
    {
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
    Ok(())
}

