//! An HTTP CONNECT proxy from the environment, as ureq read it:
//! `ALL_PROXY`, then `HTTPS_PROXY`, then `HTTP_PROXY` (either case) for
//! every request, minus the hosts `NO_PROXY` names.

use std::fmt::Write as _;
use std::io::{self, Read as _};
use std::net::TcpStream;

use base64::Engine as _;

use super::conn::{Target, invalid};

const VARS: [&str; 6] = [
    "ALL_PROXY",
    "all_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
];

#[derive(Debug)]
pub(super) struct Proxy {
    pub host: String,
    pub port: u16,
    /// A `Proxy-Authorization` value, from the URL's user and password.
    authorization: Option<String>,
    bypass: Vec<Bypass>,
}

#[derive(Debug, PartialEq, Eq)]
enum Bypass {
    All,
    Exact(String),
    /// `*.example.com` and `.example.com`: what ends with it.
    Suffix(String),
    /// `10.*` and `10.`: what starts with it.
    Prefix(String),
}

impl Proxy {
    /// The proxy the environment names. One it names but this client
    /// cannot speak (https or SOCKS) is an error, never a silent bypass.
    pub fn from_env() -> io::Result<Option<Self>> {
        let Some(url) = VARS.iter().find_map(|var| std::env::var(var).ok()) else {
            return Ok(None);
        };
        let bypass = ["NO_PROXY", "no_proxy"]
            .iter()
            .find_map(|var| std::env::var(var).ok())
            .map_or_else(Vec::new, |list| {
                list.split(',').filter_map(Bypass::parse).collect()
            });
        Self::parse(&url, bypass).map(Some)
    }

    fn parse(url: &str, bypass: Vec<Bypass>) -> io::Result<Self> {
        let url = if url.contains("://") {
            url.to_owned()
        } else {
            format!("http://{url}")
        };
        let uri: http::Uri = url
            .parse()
            .map_err(|_| invalid(format!("proxy URL {url}")))?;
        if uri.scheme_str() != Some("http") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("only http:// proxies are supported: {url}"),
            ));
        }
        let authority = uri
            .authority()
            .ok_or_else(|| invalid(format!("proxy URL {url}")))?;
        let authorization = authority.as_str().rsplit_once('@').map(|(credentials, _)| {
            let (user, password) = credentials.split_once(':').unwrap_or((credentials, ""));
            let token = base64::prelude::BASE64_STANDARD.encode(format!("{user}:{password}"));
            format!("Basic {token}")
        });
        Ok(Self {
            host: authority.host().to_owned(),
            port: authority.port_u16().unwrap_or(80),
            authorization,
            bypass,
        })
    }

    pub fn applies_to(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        !self.bypass.iter().any(|bypass| bypass.matches(&host))
    }

    /// Asks the proxy for a tunnel to `target`; the socket then speaks to
    /// the target, TLS included.
    pub async fn tunnel(&self, socket: &rt::Async<TcpStream>, target: &Target) -> io::Result<()> {
        let authority = format!("{}:{}", target.host, target.port);
        let mut request = format!(
            "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: Keep-Alive\r\n"
        );
        if let Some(authorization) = &self.authorization {
            let _ = write!(request, "Proxy-Authorization: {authorization}\r\n");
        }
        request.push_str("\r\n");
        socket.write_all(request.as_bytes()).await?;
        // Read only up to the head's end: what follows belongs to the
        // target, and the proxy sends nothing more before the client does.
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() > 16 << 10 {
                return Err(io::Error::other("the proxy's reply head is too large"));
            }
            let mut byte = [0];
            if socket.read_with(|mut s| s.read(&mut byte)).await? == 0 {
                return Err(io::Error::other("the proxy closed the connection"));
            }
            head.push(byte[0]);
        }
        let reply = ureq_proto::parser::try_parse_response::<32>(&head)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            .ok_or_else(|| io::Error::other("the proxy's reply is incomplete"))?
            .1;
        if reply.status() != http::StatusCode::OK {
            return Err(io::Error::other(format!(
                "the proxy refused the tunnel: {}",
                reply.status()
            )));
        }
        Ok(())
    }
}

impl Bypass {
    fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim().to_ascii_lowercase();
        Some(match entry.as_str() {
            "" => return None,
            "*" => Self::All,
            _ if entry.starts_with('*') => Self::Suffix(entry[1..].to_owned()),
            _ if entry.starts_with('.') => Self::Suffix(entry),
            _ if entry.ends_with('*') => Self::Prefix(entry[..entry.len() - 1].to_owned()),
            _ if entry.ends_with('.') => Self::Prefix(entry),
            _ => Self::Exact(entry),
        })
    }

    fn matches(&self, host: &str) -> bool {
        match self {
            Self::All => true,
            Self::Exact(exact) => host == exact,
            Self::Suffix(suffix) => host.ends_with(suffix.as_str()),
            Self::Prefix(prefix) => host.starts_with(prefix.as_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_proxy_entries_bypass_what_they_name() {
        let proxy = Proxy::parse(
            "user:secret@proxy.test:3128",
            "localhost,*.internal.test,.corp.test,10.*"
                .split(',')
                .filter_map(Bypass::parse)
                .collect(),
        )
        .unwrap();
        assert_eq!((proxy.host.as_str(), proxy.port), ("proxy.test", 3128));
        assert_eq!(
            proxy.authorization.as_deref(),
            Some("Basic dXNlcjpzZWNyZXQ=")
        );
        for host in ["LOCALHOST", "a.internal.test", "b.corp.test", "10.1.2.3"] {
            assert!(!proxy.applies_to(host), "{host}");
        }
        for host in ["api.openai.com", "corp.test.evil", "110.1.2.3"] {
            assert!(proxy.applies_to(host), "{host}");
        }
    }

    #[test]
    fn a_proxy_this_client_cannot_speak_is_refused() {
        for url in ["https://proxy.test", "socks5://proxy.test:1080"] {
            assert_eq!(
                Proxy::parse(url, Vec::new()).unwrap_err().kind(),
                io::ErrorKind::Unsupported
            );
        }
    }
}
