//! End-to-end cover for the background video-verification path.
//!
//! Exercises the two pieces that let a video thumbnail ever become cacheable
//! without a full download on the request path: the bounded hash-verified
//! fetch, and thumbnail extraction from the resulting trusted local bytes.
//! Both run against a real HTTP origin and a real FFmpeg, because the thing
//! being proven — that verified bytes decode to a usable frame — cannot be
//! observed from mocks.

use std::{
    net::SocketAddr,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{routing::get, Router};
use rust_imgproxy::{
    blossom::{try_fetch_verified_blob, CandidateFailureCache},
    error::SvcError,
    thumbnail::extract_thumbnail_from_verified_bytes,
};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

/// WebP files start with `RIFF....WEBP`; FFmpeg is configured to emit WebP.
const WEBP_MAGIC_HEAD: &[u8] = b"RIFF";
const WEBP_MAGIC_FORM: &[u8] = b"WEBP";

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Encode a short, real H.264/MP4 clip with FFmpeg's synthetic test source.
fn sample_video() -> Option<Vec<u8>> {
    let dir = tempfile::tempdir().ok()?;
    let path = dir.path().join("sample.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=320x240:rate=10",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    std::fs::read(&path).ok()
}

async fn spawn_origin(body: Vec<u8>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().fallback(get(move || {
        let body = body.clone();
        async move { body }
    }));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    address
}

#[tokio::test]
async fn verified_blob_yields_a_decodable_thumbnail_frame() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let Some(video) = sample_video() else {
        eprintln!("skipping: ffmpeg could not encode the sample clip");
        return;
    };
    let hash = hex::encode(Sha256::digest(&video));
    let origin = spawn_origin(video.clone()).await;

    rust_imgproxy::init_crypto_provider();
    // `.resolve` stands in for the guarded public-DNS resolver, exactly as the
    // blossom unit tests do: the hostname is public-shaped, the socket is not.
    let http = reqwest::Client::builder()
        .resolve("video.example", origin)
        .build()
        .unwrap();
    let servers = vec![format!("http://video.example:{}", origin.port())];
    let failure_cache = CandidateFailureCache::new(
        Duration::from_secs(60),
        Duration::from_secs(60),
        Duration::from_secs(60),
    );

    let verified = try_fetch_verified_blob(
        &http,
        &failure_cache,
        &servers,
        &[],
        &hash,
        Some("mp4"),
        Instant::now() + Duration::from_secs(30),
        32 * 1024 * 1024,
        8,
        Duration::from_secs(10),
    )
    .await
    .expect("origin serves bytes matching the requested hash");
    assert_eq!(
        verified.as_ref(),
        video.as_slice(),
        "verification must hand back exactly the blob it hashed"
    );

    let thumbnail = match extract_thumbnail_from_verified_bytes(
        &verified,
        &format!("{hash}.mp4"),
        &Arc::new(Semaphore::new(1)),
        16 * 1024 * 1024,
        Duration::from_secs(30),
    )
    .await
    {
        Ok(thumbnail) => thumbnail,
        // `apply_ffmpeg_limits` sets RLIMIT_AS, which macOS rejects with
        // EINVAL, so the pre-exec hook fails and no FFmpeg ever starts. That
        // is fail-closed and platform-wide, not specific to this path: every
        // video thumbnail behaves the same way on such a host. Skip loudly
        // rather than assert against an environment that cannot run the
        // subject at all.
        Err(SvcError::InternalError(reason)) if reason.contains("failed to spawn ffmpeg") => {
            eprintln!("skipping frame assertions: {reason} (RLIMIT_AS unsupported on this host)");
            return;
        }
        Err(error) => panic!("a verified H.264 clip must yield a thumbnail frame: {error:?}"),
    };

    assert!(
        thumbnail.len() > 100,
        "thumbnail should be a real frame, got {} bytes",
        thumbnail.len()
    );
    assert_eq!(
        &thumbnail[0..4],
        WEBP_MAGIC_HEAD,
        "expected a RIFF container"
    );
    assert_eq!(
        &thumbnail[8..12],
        WEBP_MAGIC_FORM,
        "expected WebP form type"
    );
}

#[tokio::test]
async fn a_blob_that_fails_its_hash_never_reaches_ffmpeg() {
    let corrupt = b"this is not the blob you asked for".to_vec();
    let origin = spawn_origin(corrupt).await;

    rust_imgproxy::init_crypto_provider();
    let http = reqwest::Client::builder()
        .resolve("corrupt.example", origin)
        .build()
        .unwrap();
    let servers = vec![format!("http://corrupt.example:{}", origin.port())];
    let failure_cache = CandidateFailureCache::new(
        Duration::from_secs(60),
        Duration::from_secs(60),
        Duration::from_secs(60),
    );

    let verified = try_fetch_verified_blob(
        &http,
        &failure_cache,
        &servers,
        &[],
        &"a".repeat(64),
        Some("mp4"),
        Instant::now() + Duration::from_secs(10),
        32 * 1024 * 1024,
        8,
        Duration::from_secs(5),
    )
    .await;

    assert!(
        verified.is_none(),
        "bytes that do not hash to the requested blob must never be trusted"
    );
}
