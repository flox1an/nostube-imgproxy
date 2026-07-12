use std::{io, net::IpAddr, sync::Arc};

use reqwest::dns::{Name, Resolve, Resolving};
use url::{Host, Url};

use crate::error::SvcError;

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
        Some(_) => Ok(url),
        None => Err(SvcError::BadRequest("upstream host is required")),
    }
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
    use super::validate_untrusted_url;

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
}
