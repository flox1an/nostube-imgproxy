use std::{io, net::IpAddr, sync::Arc};

use reqwest::{
    dns::{Name, Resolve, Resolving},
    redirect,
};
use url::{Host, Url};

use crate::error::SvcError;

/// Maximum redirect hops followed for an untrusted source.
///
/// Blossom servers routinely answer a blob request with a 302 to the bucket or
/// CDN that actually holds the bytes, so refusing redirects outright makes
/// those blobs unfetchable. Every hop is re-validated instead, and the chain is
/// bounded so an upstream cannot hold a connection open by redirecting forever.
const MAX_REDIRECT_HOPS: usize = 5;

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
        }
        IpAddr::V6(ip) => {
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unicast_link_local()
                || ip.is_unique_local())
        }
    }
}

pub fn validate_untrusted_url(raw: &str) -> Result<Url, SvcError> {
    let url = Url::parse(raw).map_err(|_| SvcError::BadRequest("invalid upstream url"))?;
    validate_untrusted_target(&url)?;
    Ok(url)
}

/// The scheme/host half of [`validate_untrusted_url`], for targets that are
/// already parsed — notably each hop of a redirect chain, which never needs to
/// be re-serialised and re-parsed just to be checked.
pub fn validate_untrusted_target(url: &Url) -> Result<(), SvcError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(SvcError::BadRequest("unsupported source scheme"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(SvcError::BadRequest("upstream credentials are not allowed"));
    }

    match url.host() {
        Some(Host::Domain(host))
            if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") =>
        {
            Err(SvcError::BadRequest("private upstream is not allowed"))
        }
        Some(Host::Ipv4(ip)) if !is_public_ip(IpAddr::V4(ip)) => {
            Err(SvcError::BadRequest("private upstream is not allowed"))
        }
        Some(Host::Ipv6(ip)) if !is_public_ip(IpAddr::V6(ip)) => {
            Err(SvcError::BadRequest("private upstream is not allowed"))
        }
        Some(_) => Ok(()),
        None => Err(SvcError::BadRequest("upstream host is required")),
    }
}

/// Redirect policy for untrusted fetches: follow the chain, but re-validate
/// every hop so a public entry point cannot bounce us onto a private address.
///
/// A rejected hop is surfaced as a transport error rather than as the bare 3xx,
/// so callers cannot mistake a refused redirect for a fetchable response.
pub fn guarded_redirect_policy() -> redirect::Policy {
    redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECT_HOPS {
            return attempt.error(io::Error::other("too many upstream redirects"));
        }

        match validate_untrusted_target(attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(_) => attempt.error(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "upstream redirected to a disallowed address",
            )),
        }
    })
}

pub fn is_allowed_untrusted_server(raw: &str) -> bool {
    validate_untrusted_url(raw).is_ok()
}

/// DNS resolver used by every untrusted reqwest request. The returned socket
/// addresses are filtered before reqwest connects, closing the DNS-rebinding
/// gap left by URL string validation alone.
#[derive(Clone, Default)]
pub struct PublicDnsResolver;

impl Resolve for PublicDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let hostname = name.as_str().to_owned();

        Box::pin(async move {
            let addresses = tokio::net::lookup_host((hostname.as_str(), 0)).await?;
            let public_addresses: Vec<_> = addresses
                .filter(|address| is_public_ip(address.ip()))
                .collect();

            if public_addresses.is_empty() {
                return Err(Box::<dyn std::error::Error + Send + Sync>::from(
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "DNS resolved only to private addresses",
                    ),
                ));
            }

            Ok(Box::new(public_addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

pub fn public_dns_resolver() -> Arc<PublicDnsResolver> {
    Arc::new(PublicDnsResolver)
}

#[cfg(test)]
mod tests {
    use super::{guarded_redirect_policy, validate_untrusted_url};
    use axum::{response::Redirect, routing::get, Router};
    use std::net::SocketAddr;

    /// Serve `/blob` as a 302 to `target` and `/final` as the payload, so a
    /// redirect chain can be exercised without reaching the network.
    async fn spawn_redirecting_server(target: fn(SocketAddr) -> String) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let location = target(address);
        tokio::spawn(async move {
            let router = Router::new()
                .route(
                    "/blob",
                    get(move || {
                        let location = location.clone();
                        async move { Redirect::to(&location) }
                    }),
                )
                .route("/final", get(|| async { "redirected payload" }));
            axum::serve(listener, router).await.unwrap();
        });
        address
    }

    #[test]
    fn private_and_loopback_urls_are_rejected() {
        for url in [
            "http://localhost/video.mp4",
            "http://127.0.0.1/video.mp4",
            "http://10.0.0.1/video.mp4",
            "http://192.168.1.2/video.mp4",
            "http://[::1]/video.mp4",
            "http://[fd00::1]/video.mp4",
        ] {
            assert!(validate_untrusted_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn public_https_url_is_allowed() {
        assert!(validate_untrusted_url("https://cdn.example.com/video.mp4").is_ok());
    }

    #[tokio::test]
    async fn guarded_redirect_policy_follows_a_public_hop() {
        // Blossom servers commonly 302 a blob to the bucket or CDN that holds
        // the bytes, so a validated hop must be followed rather than refused.
        crate::init_crypto_provider();
        let address = spawn_redirecting_server(|address| {
            format!("http://cdn.example:{}/final", address.port())
        })
        .await;
        let http = reqwest::Client::builder()
            .redirect(guarded_redirect_policy())
            .resolve("cdn.example", address)
            .build()
            .unwrap();

        let response = http
            .get(format!("http://cdn.example:{}/blob", address.port()))
            .send()
            .await
            .expect("a validated redirect hop must be followed");

        assert!(response.status().is_success());
        assert!(response.url().path().ends_with("/final"));
        assert_eq!(response.text().await.unwrap(), "redirected payload");
    }

    #[tokio::test]
    async fn guarded_redirect_policy_refuses_a_private_hop() {
        // A public entry point must not be able to bounce the fetch onto a
        // private address, which is the SSRF hole redirect-following could open.
        crate::init_crypto_provider();
        let address = spawn_redirecting_server(|address| {
            format!("http://127.0.0.1:{}/final", address.port())
        })
        .await;
        let http = reqwest::Client::builder()
            .redirect(guarded_redirect_policy())
            .resolve("cdn.example", address)
            .build()
            .unwrap();

        let error = http
            .get(format!("http://cdn.example:{}/blob", address.port()))
            .send()
            .await
            .expect_err("a redirect to a private address must be refused");

        assert!(!error.is_status(), "refusal must not surface as a bare 3xx");
    }
}
