//! The HTTP CONNECT proxies the environment names, read as curl reads
//! them: `https_proxy` (or `HTTPS_PROXY`) for https and `http_proxy` for
//! http, each falling back to `all_proxy` (or `ALL_PROXY`); `no_proxy`
//! (or `NO_PROXY`) names the hosts that go direct. Upper-case
//! `HTTP_PROXY` is ignored, as curl ignores it: a CGI server sets it from
//! a request's `Proxy:` header.

use std::fmt::Write as _;
use std::io::{self, Read as _};
use std::net::TcpStream;

use base64::Engine as _;

use super::conn::Target;

#[derive(Debug, Default)]
pub(super) struct Proxies {
    http: Option<Setting>,
    https: Option<Setting>,
    bypass: Vec<Bypass>,
}

/// A proxy the environment names: one this client cannot speak (https or
/// SOCKS) fails each request that would go through it, never a silent
/// bypass.
type Setting = Result<Proxy, (io::ErrorKind, String)>;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Proxy {
    pub host: String,
    pub port: u16,
    /// A `Proxy-Authorization` value, from the URL's user and password.
    authorization: Option<String>,
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

impl Proxies {
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(var: impl Fn(&str) -> Option<String>) -> Self {
        let first = |names: &[&str]| names.iter().find_map(|name| var(name));
        let all = first(&["all_proxy", "ALL_PROXY"]);
        let setting = |url: Option<String>| {
            url.or_else(|| all.clone())
                .map(|url| Proxy::parse(&url).map_err(|e| (e.kind(), e.to_string())))
        };
        Self {
            http: setting(first(&["http_proxy"])),
            https: setting(first(&["https_proxy", "HTTPS_PROXY"])),
            bypass: first(&["no_proxy", "NO_PROXY"]).map_or_else(Vec::new, |list| {
                list.split(',').filter_map(Bypass::parse).collect()
            }),
        }
    }

    /// The proxy a request to `target` goes through, if any.
    pub fn route(&self, target: &Target) -> io::Result<Option<&Proxy>> {
        let setting = if target.https {
            &self.https
        } else {
            &self.http
        };
        let Some(setting) = setting else {
            return Ok(None);
        };
        let host = target.host.to_ascii_lowercase();
        if self.bypass.iter().any(|bypass| bypass.matches(&host)) {
            return Ok(None);
        }
        setting
            .as_ref()
            .map(Some)
            .map_err(|(kind, message)| io::Error::new(*kind, message.clone()))
    }
}

impl Proxy {
    fn parse(url: &str) -> io::Result<Self> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, format!("proxy URL {url}"));
        let url = if url.contains("://") {
            url.to_owned()
        } else {
            format!("http://{url}")
        };
        let uri: http::Uri = url.parse().map_err(|_| invalid())?;
        if uri.scheme_str() != Some("http") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("only http:// proxies are supported: {url}"),
            ));
        }
        let authority = uri.authority().ok_or_else(invalid)?;
        let authorization = authority.as_str().rsplit_once('@').map(|(credentials, _)| {
            let (user, password) = credentials.split_once(':').unwrap_or((credentials, ""));
            let token = base64::prelude::BASE64_STANDARD.encode(format!("{user}:{password}"));
            format!("Basic {token}")
        });
        Ok(Self {
            host: authority.host().to_owned(),
            port: authority.port_u16().unwrap_or(80),
            authorization,
        })
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

    fn proxies(vars: &[(&str, &str)]) -> Proxies {
        Proxies::from_lookup(|name| {
            vars.iter()
                .find(|(var, _)| *var == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    fn target(https: bool, host: &str) -> Target {
        Target {
            https,
            host: host.to_owned(),
            port: if https { 443 } else { 80 },
        }
    }

    fn via(proxies: &Proxies, https: bool, host: &str) -> Option<String> {
        proxies
            .route(&target(https, host))
            .unwrap()
            .map(|proxy| format!("{}:{}", proxy.host, proxy.port))
    }

    #[test]
    fn no_proxy_entries_bypass_what_they_name() {
        let proxies = proxies(&[
            ("all_proxy", "user:secret@proxy.test:3128"),
            ("NO_PROXY", "localhost,*.internal.test,.corp.test,10.*"),
        ]);
        assert_eq!(
            proxies.route(&target(true, "api.openai.com")).unwrap(),
            Some(&Proxy {
                host: "proxy.test".to_owned(),
                port: 3128,
                authorization: Some("Basic dXNlcjpzZWNyZXQ=".to_owned()),
            })
        );
        for host in ["LOCALHOST", "a.internal.test", "b.corp.test", "10.1.2.3"] {
            assert_eq!(via(&proxies, true, host), None, "{host}");
        }
        for host in ["corp.test.evil", "110.1.2.3"] {
            assert!(via(&proxies, true, host).is_some(), "{host}");
        }
    }

    #[test]
    fn the_scheme_s_own_proxy_wins_over_all_proxy() {
        let proxies = proxies(&[
            ("ALL_PROXY", "all.test:1"),
            ("HTTPS_PROXY", "secure.test:2"),
            ("HTTP_PROXY", "cgi-header.test:3"),
        ]);
        assert_eq!(
            via(&proxies, true, "a.test").as_deref(),
            Some("secure.test:2")
        );
        // Upper-case HTTP_PROXY is ignored, as curl ignores it.
        assert_eq!(
            via(&proxies, false, "a.test").as_deref(),
            Some("all.test:1")
        );
        assert_eq!(
            via(
                &self::proxies(&[("http_proxy", "plain.test:4")]),
                false,
                "a.test"
            )
            .as_deref(),
            Some("plain.test:4")
        );
        assert_eq!(
            via(
                &self::proxies(&[("http_proxy", "plain.test:4")]),
                true,
                "a.test"
            ),
            None
        );
    }

    #[test]
    fn lowercase_proxy_settings_win_when_both_cases_are_set() {
        let proxies = proxies(&[
            ("all_proxy", "all-lower.test:1"),
            ("ALL_PROXY", "all-upper.test:2"),
            ("https_proxy", "https-lower.test:3"),
            ("HTTPS_PROXY", "https-upper.test:4"),
            ("no_proxy", "lower.test"),
            ("NO_PROXY", "upper.test"),
        ]);
        assert_eq!(via(&proxies, true, "lower.test"), None);
        assert_eq!(
            via(&proxies, true, "upper.test").as_deref(),
            Some("https-lower.test:3")
        );
        assert_eq!(
            via(&proxies, false, "upper.test").as_deref(),
            Some("all-lower.test:1")
        );
    }

    #[test]
    fn an_unusable_proxy_fails_only_what_would_go_through_it() {
        for url in ["https://proxy.test", "socks5://proxy.test:1080"] {
            let proxies = proxies(&[("ALL_PROXY", url), ("NO_PROXY", "localhost")]);
            assert_eq!(
                proxies
                    .route(&target(true, "api.openai.com"))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::Unsupported
            );
            assert_eq!(via(&proxies, true, "localhost"), None);
        }
    }
}
