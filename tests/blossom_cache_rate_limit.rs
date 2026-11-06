use std::{sync::Arc, time::Duration};

use rust_imgproxy::{
    blossom::{BlossomState, CandidateFailureCache},
    cache::{cache_path_for, derivative_cache_key, THUMB_ROUTE},
    config::{AppCfg, AppState},
    server::create_router,
    thumbnail::ThumbnailState,
    transform::{Directives, OutFmt, Resize, ResizeMode},
};

#[tokio::test]
async fn blossom_video_cache_hits_bypass_exhausted_request_budget_without_crossing_origins() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = AppCfg::from_env();
    cfg.cache_dir = dir.path().to_owned();
    cfg.rate_ip_requests_per_min = 1;
    cfg.blossom_fallback_servers.clear();
    let dirs = Directives {
        out_fmt: OutFmt::Webp,
        quality: 82,
        resize: Resize {
            mode: ResizeMode::Fit,
            w: 480,
            h: 480,
        },
    };
    let hash = "a".repeat(64);
    for name in [hash.clone(), format!("{hash}.mp4")] {
        let key = derivative_cache_key(THUMB_ROUTE, &format!("{name}@https://cdn.example"), &dirs);
        let path = cache_path_for(&cfg, THUMB_ROUTE, &key, &dirs.out_fmt);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(path, b"cached video frame").await.unwrap();
    }
    let ttl = Duration::from_secs(60);
    let blossom = Arc::new(
        BlossomState::new(
            ttl,
            ttl,
            ttl,
            Duration::from_millis(50),
            CandidateFailureCache::new(ttl, ttl, ttl),
        )
        .await,
    );
    let app = create_router(
        AppState::new(cfg),
        Arc::new(ThumbnailState::new(1)),
        blossom,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let client = reqwest::Client::new();
    // First miss exhausts the single request budget.
    let miss = format!(
        "http://{addr}/thumb/{}.mp4?xs=https%3A%2F%2Fother.example",
        "b".repeat(64)
    );
    let _ = client.get(&miss).send().await.unwrap();
    assert_eq!(client.get(&miss).send().await.unwrap().status(), 429);
    for name in [hash.clone(), format!("{hash}.mp4")] {
        let url = format!("http://{addr}/thumb/{name}?xs=https%3A%2F%2Fcdn.example");
        let response = client.get(url).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.bytes().await.unwrap(), &b"cached video frame"[..]);
        let other = format!("http://{addr}/thumb/{name}?xs=https%3A%2F%2Fother.example");
        assert_eq!(client.get(other).send().await.unwrap().status(), 429);
    }
    server.abort();
}
