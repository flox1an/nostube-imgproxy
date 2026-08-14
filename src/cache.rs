use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use http::HeaderName;
use sha2::{Digest, Sha256};
use tokio::{fs as tokio_fs, io::AsyncWriteExt, time::sleep};
use tokio_util::io::ReaderStream;
use tracing::error;
use walkdir::WalkDir;

use crate::{
    config::AppCfg,
    error::SvcError,
    metrics,
    transform::{Directives, OutFmt},
};

const IMMUTABLE_CACHE_CONTROL: HeaderValue =
    HeaderValue::from_static("public, max-age=31536000, immutable");
/// For derivatives whose source is NOT hash-addressed (`/insecure`): the bytes
/// behind the URL can change or disappear, so a one-year `immutable` pin would
/// serve stale or hostile content long after the source moved.
const SHORT_CACHE_CONTROL: HeaderValue = HeaderValue::from_static("public, max-age=3600");
const X_CACHE: HeaderName = HeaderName::from_static("x-cache");

/// How aggressively a client may cache a derivative response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientCachePolicy {
    /// Hash-addressed content (`/thumb`): the bytes are content-addressed, so
    /// an `immutable` pin is correct.
    Immutable,
    /// URL-addressed content (`/insecure`): the source can change behind the
    /// URL, so client-side caching is capped short.
    ShortLived,
}

/// Canonical, versioned key for a derivative.
///
/// Built from the *parsed* directives rather than the raw request string, so
/// requests that differ only in noise (`bogus:1/` vs `bogus:2/`, `f:JPG` vs
/// `f:jpeg`, a repeated directive) collapse onto one cache entry. Raw-request
/// keying let an attacker mint unbounded distinct keys with identical output,
/// each costing a full decode+encode+disk-write after the first fetch.
///
/// `route` separates the two namespaces (`insecure` vs `thumb`) so a source URL
/// and a blob name can never collide. `source` is the *validated* source URL or
/// canonical blob name. Server-selection hints (`xs=`, `as=`) deliberately do
/// not appear: they decide where the bytes come from, not how the output looks.
///
/// The `v1` prefix versions the format; changing the field order or set
/// silently orphans every existing entry, so bump it when the shape changes.
pub fn derivative_cache_key(route: &str, source: &str, dirs: &Directives) -> String {
    let resize = &dirs.resize;
    format!(
        "v1|{route}|{source}|{}|{}|{}|{}|{}",
        dirs.out_fmt.label(),
        dirs.quality,
        resize.mode.label(),
        resize.w,
        resize.h,
    )
}

/// SHA-256 digest of `bytes`, hex-encoded.
fn digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// `base/<h0h1>/<h2h3>`: the digest's first two hex-char pairs as directory
/// levels, so no single directory grows past ~256 entries and janitor walks
/// stay short.
fn sharded_path(base: &Path, digest: &str) -> PathBuf {
    base.join(&digest[0..2]).join(&digest[2..4])
}

/// Generate cache file path for processed images.
///
/// `key` is the canonical string from [`derivative_cache_key`]; the digest of
/// that key doubles as the file stem and therefore the ETag validator.
pub fn cache_path_for(cfg: &AppCfg, key: &str, fmt: &OutFmt) -> PathBuf {
    let hash = digest(key.as_bytes());
    sharded_path(&cfg.cache_dir.join("processed"), &hash)
        .join(format!("{hash}.{}", fmt.extension()))
}

/// Generate cache file path for original images
pub fn original_cache_path_for(cfg: &AppCfg, source: &str) -> PathBuf {
    let hash = digest(source.as_bytes());
    sharded_path(&cfg.cache_dir.join("original"), &hash).join(&hash)
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
fn decorate(
    headers: &mut HeaderMap,
    mime: &str,
    etag: Option<&HeaderValue>,
    cache_state: &str,
    policy: ClientCachePolicy,
) {
    if let Ok(content_type) = HeaderValue::from_str(mime) {
        headers.insert(header::CONTENT_TYPE, content_type);
    }
    let cache_control = match policy {
        ClientCachePolicy::Immutable => IMMUTABLE_CACHE_CONTROL,
        ClientCachePolicy::ShortLived => SHORT_CACHE_CONTROL,
    };
    headers.insert(header::CACHE_CONTROL, cache_control);
    if let Ok(state) = HeaderValue::from_str(cache_state) {
        headers.insert(X_CACHE, state);
    }
    if let Some(etag) = etag {
        headers.insert(header::ETAG, etag.clone());
    }
}

/// Headers for a freshly produced (not cached) derivative.
pub fn fresh_response_headers(
    headers: &mut HeaderMap,
    mime: &str,
    path: &Path,
    cache_state: &str,
    policy: ClientCachePolicy,
) {
    decorate(headers, mime, etag_for(path).as_ref(), cache_state, policy);
}

/// Try to serve a response from cache.
///
/// Streams the file rather than buffering it, and answers `If-None-Match` with
/// a bodyless 304 — by far the cheapest possible hit for an edge node.
///
/// A zero-length entry is treated as a miss: a crash between temp-write and
/// rename can publish an empty file under the final name, and serving it would
/// pin a broken body to clients for a year. Treating it as a miss lets the next
/// request regenerate it.
pub async fn try_serve_cache(
    path: &Path,
    mime: &str,
    request_headers: &HeaderMap,
    policy: ClientCachePolicy,
) -> Result<Option<Response>, SvcError> {
    let Ok(file) = tokio_fs::File::open(path).await else {
        return Ok(None);
    };
    let len = match file.metadata().await {
        Ok(meta) if meta.is_file() && meta.len() > 0 => meta.len(),
        _ => return Ok(None),
    };

    let etag = etag_for(path);

    if let Some(etag) = etag.as_ref() {
        if if_none_match_hits(request_headers, etag) {
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = StatusCode::NOT_MODIFIED;
            decorate(resp.headers_mut(), mime, Some(etag), "hit", policy);
            return Ok(Some(resp));
        }
    }

    let mut resp = Response::new(Body::from_stream(ReaderStream::new(file)));
    *resp.status_mut() = StatusCode::OK;
    let headers = resp.headers_mut();
    decorate(headers, mime, etag.as_ref(), "hit", policy);
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    Ok(Some(resp))
}

/// Try to read original image from cache.
///
/// A zero-length entry is a miss, for the same crash-durability reason as
/// [`try_serve_cache`]: an empty file under the final name means a torn write
/// and must be regenerated, not reused.
pub async fn try_read_original_cache(path: &Path) -> Result<Option<Vec<u8>>, SvcError> {
    match tokio_fs::read(path).await {
        Ok(bytes) if !bytes.is_empty() => Ok(Some(bytes)),
        Ok(_) => Ok(None),
        Err(_) => Ok(None),
    }
}

/// Write data to cache atomically.
///
/// The temp name combines the PID with an unpredictable per-process suffix, so
/// two concurrent writers for the same key can never interleave into one file
/// and then both rename it — a shared tmp path is what used to publish torn
/// entries. `create_new` plus `O_NOFOLLOW` (Unix) also refuse to follow a
/// symlink pre-seeded at the temp path, which a plain
/// `create(true).truncate(true)` write would happily do.
///
/// The temp file is `sync_all`ed before the rename. Without that, a crash can
/// leave the *final* name with zero bytes, and since reads treat any file as
/// valid that empty body would be served (`immutable` on `/thumb`) for a year.
/// The rename itself is deliberately not followed by a directory fsync: every
/// cached byte is regenerable, and the directory entry cost is the one sync we
/// can skip without risking a wrong-but-permanent body.
pub async fn write_cache_atomic(path: &Path, bytes: &[u8]) -> Result<(), SvcError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    tokio_fs::create_dir_all(parent).await?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("entry");
    let tmp = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        random_suffix()
    ));

    let mut options = tokio_fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = match options.open(&tmp).await {
        Ok(file) => file,
        Err(error) => {
            let _ = tokio_fs::remove_file(&tmp).await;
            return Err(error.into());
        }
    };

    let write_result = async {
        file.write_all(bytes).await?;
        file.sync_all().await
    }
    .await;
    if let Err(error) = write_result {
        let _ = tokio_fs::remove_file(&tmp).await;
        return Err(error.into());
    }
    drop(file);

    if let Err(error) = tokio_fs::rename(&tmp, path).await {
        let _ = tokio_fs::remove_file(&tmp).await;
        return Err(error.into());
    }
    Ok(())
}

/// Unpredictable per-process suffix for temp names, so an attacker cannot plant
/// a symlink at a guessed path ahead of the write. Seeded from the process's
/// `RandomState` (system randomness) and mixed with the current time.
fn random_suffix() -> u64 {
    use std::hash::{BuildHasher, Hasher};

    static SEED: std::sync::LazyLock<std::collections::hash_map::RandomState> =
        std::sync::LazyLock::new(std::collections::hash_map::RandomState::new);
    let mut hasher = SEED.build_hasher();
    hasher.write_u64(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64),
    );
    hasher.finish()
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
///
/// A single failing entry must never abort the whole pass: `write_cache_atomic`
/// constantly creates and renames temp files, so a file can vanish between
/// WalkDir reading the dirent and this stat — the old `?` chain turned that
/// routine race into a full cleanup outage for both subdirectories.
async fn run_cleanup(cfg: &AppCfg) -> Result<(), std::io::Error> {
    let cache_dir = cfg.cache_dir.clone();
    let cache_ttl = cfg.cache_ttl;
    let max_cache_bytes = cfg.max_cache_bytes;

    tokio::task::spawn_blocking(move || {
        let now = SystemTime::now();
        // Survivors (regular files inside the TTL), oldest-modified first, plus
        // per-directory byte totals for the gauge.
        let mut entries: Vec<(PathBuf, u64, SystemTime, usize)> = Vec::new();
        let mut dir_bytes = [0u64; 2];

        for (dir_index, sub_dir) in ["original", "processed"].iter().enumerate() {
            let dir = cache_dir.join(sub_dir);
            if !dir.exists() {
                continue;
            }

            for entry in WalkDir::new(&dir).into_iter().filter_map(Result::ok) {
                if !entry.file_type().is_file() {
                    continue;
                }
                // The entry can be gone by now (temp renames race this walk);
                // `let Ok(...) else continue` keeps one vanished file from
                // aborting the pass. `entry.metadata()` is an lstat, so it also
                // cannot be redirected through a planted symlink.
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                // TTL is time since the cache entry's last completed write.
                // Birth time is not portable and, on APFS, remains unchanged
                // when tests or repair tooling update mtime; preferring it made
                // stale `.tmp` reaping filesystem-dependent.
                let Ok(modified) = meta.modified() else {
                    continue;
                };
                let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
                let is_tmp = entry.file_name().to_string_lossy().ends_with(".tmp");

                // Leftover temp files are torn writes from a crash or kill.
                // They are transient by nature, so only reap ones old enough
                // that no writer can still be mid-write on them, and never
                // count them against the byte budget.
                if is_tmp {
                    if age > cache_ttl {
                        let _ = std::fs::remove_file(entry.path());
                    }
                    continue;
                }

                if age > cache_ttl {
                    let _ = std::fs::remove_file(entry.path());
                    continue;
                }
                let size = meta.len();
                dir_bytes[dir_index] += size;
                entries.push((entry.path().to_path_buf(), size, modified, dir_index));
            }
        }

        // Byte budget: evict oldest-modified entries across both directories
        // until the total fits `max_cache_bytes`. The TTL alone cannot bound
        // the cache — an attacker requesting distinct URLs fills the disk long
        // before the oldest entry expires.
        entries.sort_by_key(|(_, _, modified, _)| *modified);
        let mut total: u64 = dir_bytes.iter().sum();
        for (path, size, _, dir_index) in entries {
            if total <= max_cache_bytes {
                break;
            }
            let _ = std::fs::remove_file(&path);
            total -= size;
            dir_bytes[dir_index] -= size;
        }

        metrics::set_cache_bytes("original", dir_bytes[0]);
        metrics::set_cache_bytes("processed", dir_bytes[1]);
        Ok(())
    })
    .await
    .unwrap_or_else(|error| Err(std::io::Error::other(error)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::parse_rest;
    use crate::transform::{Resize, ResizeMode};
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
            max_cache_bytes: 64 * 1024,
            max_video_probe_bytes: 64 * 1024 * 1024,
            max_blob_candidates: 8,
            max_server_hints: 4,
            cpu_queue_depth: 64,
            metrics_bind_addr: None,
            max_ffmpeg_concurrent: 1,
        }
    }

    /// Minimal parsed directives for key-derivation tests.
    fn directives(out_fmt: OutFmt, quality: u8, mode: ResizeMode, w: u32, h: u32) -> Directives {
        Directives {
            out_fmt,
            quality,
            resize: Resize { mode, w, h },
        }
    }

    /// Force a known mtime without sleeping, so byte-budget ordering tests do
    /// not depend on filesystem timestamp granularity.
    fn set_mtime(path: &Path, time: SystemTime) {
        std::fs::File::open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    // -----------------------------------------------------------------------
    // Cache key derivation
    // -----------------------------------------------------------------------

    #[test]
    fn cache_path_for_is_deterministic_for_the_same_key() {
        let cfg = cfg_with_cache_dir(Path::new("/tmp/cache"), Duration::from_secs(60));
        let dirs = directives(OutFmt::Webp, 82, ResizeMode::Fit, 480, 480);
        let key = derivative_cache_key("insecure", "https://e.com/a.png", &dirs);
        let a = cache_path_for(&cfg, &key, &dirs.out_fmt);
        let b = cache_path_for(&cfg, &key, &dirs.out_fmt);
        assert_eq!(a, b);
    }

    #[test]
    fn cache_path_for_separates_distinct_requests() {
        let cfg = cfg_with_cache_dir(Path::new("/tmp/cache"), Duration::from_secs(60));
        let dirs = directives(OutFmt::Webp, 82, ResizeMode::Fit, 480, 480);
        let key_a = derivative_cache_key("insecure", "https://e.com/a.png", &dirs);
        let key_b = derivative_cache_key("insecure", "https://e.com/b.png", &dirs);
        let a = cache_path_for(&cfg, &key_a, &dirs.out_fmt);
        let b = cache_path_for(&cfg, &key_b, &dirs.out_fmt);
        assert_ne!(a, b, "different source URLs must not collide");
    }

    #[test]
    fn cache_path_for_separates_output_formats() {
        let cfg = cfg_with_cache_dir(Path::new("/tmp/cache"), Duration::from_secs(60));
        let webp_dirs = directives(OutFmt::Webp, 82, ResizeMode::Fit, 480, 480);
        let avif_dirs = directives(OutFmt::Avif, 82, ResizeMode::Fit, 480, 480);
        let key_webp = derivative_cache_key("insecure", "https://e.com/a.png", &webp_dirs);
        let key_avif = derivative_cache_key("insecure", "https://e.com/a.png", &avif_dirs);
        let webp = cache_path_for(&cfg, &key_webp, &webp_dirs.out_fmt);
        let avif = cache_path_for(&cfg, &key_avif, &avif_dirs.out_fmt);
        assert_ne!(webp, avif, "one request must not serve two formats");
        assert_eq!(webp.extension().unwrap(), "webp");
        assert_eq!(avif.extension().unwrap(), "avif");
    }

    #[test]
    fn cache_path_for_lands_under_the_sharded_processed_directory() {
        let cfg = cfg_with_cache_dir(Path::new("/tmp/cache"), Duration::from_secs(60));
        let dirs = directives(OutFmt::Png, 82, ResizeMode::Fit, 480, 480);
        let key = derivative_cache_key("insecure", "https://e.com/a.png", &dirs);
        let path = cache_path_for(&cfg, &key, &dirs.out_fmt);
        // processed/<h0h1>/<h2h3>/<digest>.png — two hex-char levels of sharding.
        let levels: Vec<_> = path
            .strip_prefix(Path::new("/tmp/cache/processed"))
            .unwrap()
            .components()
            .collect();
        assert_eq!(levels.len(), 3, "sharding must add exactly two levels");
        for level in &levels[..2] {
            let name = level.as_os_str().to_str().unwrap();
            assert_eq!(
                name.len(),
                2,
                "shard level must be two hex chars, got {name}"
            );
            assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
        }
        assert_eq!(
            levels[2].as_os_str().to_str().unwrap().split('.').count(),
            2,
            "file name must be <digest>.<ext>"
        );
    }

    #[test]
    fn original_cache_path_for_lands_under_the_sharded_original_directory() {
        let cfg = cfg_with_cache_dir(Path::new("/tmp/cache"), Duration::from_secs(60));
        let path = original_cache_path_for(&cfg, "https://e.com/a.png");
        let levels: Vec<_> = path
            .strip_prefix(Path::new("/tmp/cache/original"))
            .unwrap()
            .components()
            .collect();
        assert_eq!(levels.len(), 3, "sharding must add exactly two levels");
        for level in &levels[..2] {
            let name = level.as_os_str().to_str().unwrap();
            assert_eq!(
                name.len(),
                2,
                "shard level must be two hex chars, got {name}"
            );
            assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn original_and_processed_paths_never_collide_for_one_url() {
        let cfg = cfg_with_cache_dir(Path::new("/tmp/cache"), Duration::from_secs(60));
        let url = "https://e.com/a.png";
        let dirs = directives(OutFmt::Png, 82, ResizeMode::Fit, 480, 480);
        let key = derivative_cache_key("insecure", url, &dirs);
        assert_ne!(
            original_cache_path_for(&cfg, url),
            cache_path_for(&cfg, &key, &dirs.out_fmt)
        );
    }

    #[test]
    fn derivative_cache_key_is_identical_for_normalization_equivalent_requests() {
        // Unknown directive segments are ignored by the parser, so `bogus:1/`
        // and `bogus:2/` must mint the same key — raw-request keying let them
        // mint unbounded distinct keys with identical output.
        let a = parse_rest("bogus:1/rs:fit:10:10/plain/https://e.com/a.png").unwrap();
        let b = parse_rest("bogus:2/rs:fit:10:10/plain/https://e.com/a.png").unwrap();
        assert_eq!(
            derivative_cache_key("insecure", &a.1, &a.0),
            derivative_cache_key("insecure", &b.1, &b.0)
        );

        // Case differences in the format directive normalize to one OutFmt.
        let c = parse_rest("f:JPG/rs:fit:10:10/plain/https://e.com/a.png").unwrap();
        let d = parse_rest("f:jpeg/rs:fit:10:10/plain/https://e.com/a.png").unwrap();
        assert_eq!(
            derivative_cache_key("insecure", &c.1, &c.0),
            derivative_cache_key("insecure", &d.1, &d.0)
        );

        // Repeated directives: the last one wins, so the shorter form must
        // produce the same key as the noisier one.
        let e = parse_rest("q:50/q:80/rs:fit:10:10/plain/https://e.com/a.png").unwrap();
        let f = parse_rest("q:80/rs:fit:10:10/plain/https://e.com/a.png").unwrap();
        assert_eq!(
            derivative_cache_key("insecure", &e.1, &e.0),
            derivative_cache_key("insecure", &f.1, &f.0)
        );
    }

    #[test]
    fn derivative_cache_key_differs_for_real_parameter_differences() {
        let base = directives(OutFmt::Webp, 80, ResizeMode::Fit, 100, 100);
        let base_key = derivative_cache_key("insecure", "https://e.com/a.png", &base);
        assert_ne!(
            base_key,
            derivative_cache_key(
                "insecure",
                "https://e.com/a.png",
                &directives(OutFmt::Webp, 90, ResizeMode::Fit, 100, 100)
            ),
            "quality must be part of the key"
        );
        assert_ne!(
            base_key,
            derivative_cache_key(
                "insecure",
                "https://e.com/a.png",
                &directives(OutFmt::Webp, 80, ResizeMode::Fill, 100, 100)
            ),
            "resize mode must be part of the key"
        );
        assert_ne!(
            base_key,
            derivative_cache_key(
                "insecure",
                "https://e.com/a.png",
                &directives(OutFmt::Webp, 80, ResizeMode::Fit, 200, 100)
            ),
            "dimensions must be part of the key"
        );
        assert_ne!(
            base_key,
            derivative_cache_key(
                "insecure",
                "https://e.com/b.png",
                &directives(OutFmt::Webp, 80, ResizeMode::Fit, 100, 100)
            ),
            "source must be part of the key"
        );
        assert_ne!(
            base_key,
            derivative_cache_key(
                "thumb",
                "https://e.com/a.png",
                &directives(OutFmt::Webp, 80, ResizeMode::Fit, 100, 100)
            ),
            "route must separate the two namespaces"
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
        assert!(try_serve_cache(
            &missing,
            "image/webp",
            &HeaderMap::new(),
            ClientCachePolicy::Immutable
        )
        .await
        .unwrap()
        .is_none());
    }

    #[tokio::test]
    async fn try_serve_cache_marks_hits_with_immutable_caching_headers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.webp");
        write_cache_atomic(&path, b"cached image").await.unwrap();

        let response = try_serve_cache(
            &path,
            "image/webp",
            &HeaderMap::new(),
            ClientCachePolicy::Immutable,
        )
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
    async fn try_serve_cache_uses_the_short_lived_policy_for_url_addressed_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.webp");
        write_cache_atomic(&path, b"cached image").await.unwrap();

        let response = try_serve_cache(
            &path,
            "image/webp",
            &HeaderMap::new(),
            ClientCachePolicy::ShortLived,
        )
        .await
        .unwrap()
        .expect("cache hit");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=3600",
            "non-hash-addressed content must not be pinned immutable"
        );
    }

    #[tokio::test]
    async fn try_serve_cache_treats_a_zero_byte_entry_as_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        // A crash between temp-write and rename can publish an empty file under
        // the final name; it must be regenerated, never served.
        let path = dir.path().join("torn.webp");
        std::fs::write(&path, b"").unwrap();

        assert!(
            try_serve_cache(
                &path,
                "image/webp",
                &HeaderMap::new(),
                ClientCachePolicy::Immutable
            )
            .await
            .unwrap()
            .is_none(),
            "a zero-byte cache entry must be treated as a miss"
        );
    }

    #[tokio::test]
    async fn try_read_original_cache_treats_a_zero_byte_entry_as_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("torn.bin");
        std::fs::write(&path, b"").unwrap();

        assert_eq!(
            try_read_original_cache(&path).await.unwrap(),
            None,
            "a zero-byte original entry must be treated as a miss"
        );
    }

    #[tokio::test]
    async fn try_serve_cache_answers_a_matching_validator_with_304() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.webp");
        write_cache_atomic(&path, b"cached image").await.unwrap();

        let mut request_headers = HeaderMap::new();
        request_headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"blob\""));

        let response = try_serve_cache(
            &path,
            "image/webp",
            &request_headers,
            ClientCachePolicy::Immutable,
        )
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
            let response = try_serve_cache(
                &path,
                "image/webp",
                &request_headers,
                ClientCachePolicy::Immutable,
            )
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

        let response = try_serve_cache(
            &path,
            "image/webp",
            &request_headers,
            ClientCachePolicy::Immutable,
        )
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

    #[tokio::test]
    async fn cleanup_enforces_the_byte_budget_evicting_the_oldest_entry() {
        let dir = tempfile::tempdir().unwrap();
        // TTL is long, so nothing is TTL-expired: only the byte budget can act.
        let mut cfg = cfg_with_cache_dir(dir.path(), Duration::from_secs(3600));
        cfg.max_cache_bytes = 10; // one payload fits, both together do not
        let oldest = dir.path().join("processed").join("old.webp");
        let newest = dir.path().join("processed").join("new.webp");
        write_cache_atomic(&oldest, b"aaaaaaaa").await.unwrap(); // 8 bytes
        write_cache_atomic(&newest, b"bbbbbbbb").await.unwrap(); // 8 bytes
        let now = SystemTime::now();
        set_mtime(&oldest, now - Duration::from_secs(600));
        set_mtime(&newest, now - Duration::from_secs(300));

        run_cleanup(&cfg).await.unwrap();

        assert!(
            !oldest.exists(),
            "oldest entry must be evicted to fit the byte budget"
        );
        assert!(newest.exists(), "newest entry must survive the eviction");
    }

    #[tokio::test]
    async fn cleanup_reaps_tmp_leftovers_by_modified_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = cfg_with_cache_dir(dir.path(), Duration::from_secs(3600));
        // A fresh temp file — possibly a writer mid-rename — must survive even
        // a tiny byte budget: temp files are transient, not budget entries.
        cfg.max_cache_bytes = 1;
        let stale = dir.path().join("processed").join(".dead.1.2.tmp");
        let fresh = dir.path().join("processed").join(".live.1.3.tmp");
        write_cache_atomic(&stale, b"leftover").await.unwrap();
        write_cache_atomic(&fresh, b"in-flight").await.unwrap();
        let now = SystemTime::now();
        set_mtime(&stale, now - Duration::from_secs(7200)); // twice the TTL

        run_cleanup(&cfg).await.unwrap();

        assert!(
            !stale.exists(),
            "a tmp leftover older than the TTL must be reaped"
        );
        assert!(
            fresh.exists(),
            "a fresh tmp file must not be reaped or budget-evicted"
        );
    }

    #[tokio::test]
    async fn write_cache_atomic_creates_the_sharded_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with_cache_dir(dir.path(), Duration::from_secs(60));
        let path = original_cache_path_for(&cfg, "https://e.com/blob");
        // Two hex-char levels under `original/`, neither of which exists yet.
        assert_eq!(
            path.parent().unwrap().parent().unwrap().parent().unwrap(),
            dir.path().join("original")
        );

        write_cache_atomic(&path, b"bytes").await.unwrap();

        assert!(path.exists(), "sharded entry must exist at {path:?}");
    }
}
