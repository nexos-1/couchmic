//! Access control for HTTP and WebSocket.
//!
//! glass-mic only listens on 127.0.0.1; other devices reach it only through `tailscale serve`.
//! Without further checks two ways would stay open:
//! - Cross-site WebSocket: any web page in a browser on the PC or on a tailnet device opens
//!   `ws://127.0.0.1:8321/ws` or `wss://<pc>.ts.net/ws`, switches the default microphone and
//!   plays its own audio. WebSockets have no CORS; the server has to check the origin itself.
//! - DNS rebinding: a foreign domain that points to 127.0.0.1 is "same origin" for the browser
//!   and would reach /ws and /api/stats. The host allowlist stops that.
//!
//! Rules: the Host header must be a loopback address, the Tailscale name or a host allowed with
//! `--allow-origin`. An Origin (browsers always send one for WebSockets) must belong to exactly
//! these addresses, loopback only with our own port. Without an Origin (curl, LocalFlow) the
//! host check is enough.

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the Tailscale name is looked up at most while it is unknown (at autostart after
/// logon Tailscale is often not running yet).
const PROBE_INTERVAL: Duration = Duration::from_secs(30);

const LOOPBACK: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];

/// Origin as (scheme, host, port) with the default port resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    scheme: String,
    host: String,
    port: u16,
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

/// "host", "host:port", "[::1]:port" -> (host, port). IPv6 keeps its brackets.
fn split_host_port(s: &str) -> Option<(String, Option<u16>)> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let (host, rest) = if s.starts_with('[') {
        let end = s.find(']')?;
        (s[..=end].to_string(), &s[end + 1..])
    } else {
        match s.rfind(':') {
            Some(i) => (s[..i].to_string(), &s[i..]),
            None => (s.clone(), ""),
        }
    };
    let port = match rest.strip_prefix(':') {
        Some(p) => Some(p.parse().ok()?),
        None if rest.is_empty() => None,
        None => return None,
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

/// "https://host[:port]" -> Origin. Paths, "null" and unknown schemes are invalid.
fn parse_origin(s: &str) -> Option<Origin> {
    let s = s.trim().trim_end_matches('/');
    let (scheme, authority) = s.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default = default_port(&scheme)?;
    if authority.contains('/') || authority.contains('@') {
        return None;
    }
    let (host, port) = split_host_port(authority)?;
    Some(Origin {
        scheme,
        host,
        port: port.unwrap_or(default),
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Denied {
    MissingHost,
    Host,
    Origin,
}

/// Allowed addresses. Plain data, so the rules are testable without a server.
#[derive(Debug, Clone)]
pub struct AllowList {
    port: u16,
    tailscale_host: Option<String>,
    extra: Vec<Origin>,
}

impl AllowList {
    /// `extra`: additional origins like "https://mic.example.net"; invalid ones are reported.
    pub fn new(port: u16, tailscale_host: Option<String>, extra: &[String]) -> Self {
        let extra = extra
            .iter()
            .filter_map(|o| {
                let parsed = parse_origin(o);
                if parsed.is_none() {
                    tracing::warn!(origin = %o, "invalid --allow-origin, ignored");
                }
                parsed
            })
            .collect();
        Self {
            port,
            tailscale_host: tailscale_host.map(|h| h.to_ascii_lowercase()),
            extra,
        }
    }

    fn tailscale_origin(&self) -> Option<Origin> {
        self.tailscale_host.as_ref().map(|h| Origin {
            scheme: "https".into(),
            host: h.clone(),
            port: 443,
        })
    }

    fn host_ok(&self, host_header: &str) -> bool {
        let Some((host, port)) = split_host_port(host_header) else {
            return false;
        };
        if LOOPBACK.contains(&host.as_str()) {
            return true;
        }
        // Without a port in the Host header the scheme's default port applies; behind
        // `tailscale serve` the server cannot see which scheme the client used, so accept both.
        self.tailscale_origin()
            .iter()
            .chain(self.extra.iter())
            .any(|o| o.host == host && port.map(|p| p == o.port).unwrap_or(true))
    }

    fn origin_ok(&self, origin: &str) -> bool {
        let Some(o) = parse_origin(origin) else {
            return false;
        };
        if LOOPBACK.contains(&o.host.as_str()) {
            return o.port == self.port;
        }
        self.tailscale_origin().as_ref() == Some(&o) || self.extra.contains(&o)
    }

    pub fn check(&self, host: Option<&str>, origin: Option<&str>) -> Result<(), Denied> {
        let host = host.ok_or(Denied::MissingHost)?;
        if !self.host_ok(host) {
            return Err(Denied::Host);
        }
        match origin {
            Some(o) if !self.origin_ok(o) => Err(Denied::Origin),
            _ => Ok(()),
        }
    }

    pub fn has_tailscale_host(&self) -> bool {
        self.tailscale_host.is_some()
    }
}

/// Shared state for the middleware: allowlist plus lazy lookup of the Tailscale name.
pub struct Access {
    list: Mutex<AllowList>,
    last_probe: Mutex<Option<Instant>>,
    probe: fn() -> Option<String>,
}

impl Access {
    pub fn new(list: AllowList, probe: fn() -> Option<String>) -> Arc<Self> {
        Arc::new(Self {
            list: Mutex::new(list),
            last_probe: Mutex::new(None),
            probe,
        })
    }

    fn check(&self, host: Option<&str>, origin: Option<&str>) -> Result<(), Denied> {
        self.list.lock().unwrap().check(host, origin)
    }

    /// Only while the Tailscale name is missing and the last lookup is long enough ago.
    fn should_probe(&self) -> bool {
        if self.list.lock().unwrap().has_tailscale_host() {
            return false;
        }
        let mut last = self.last_probe.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < PROBE_INTERVAL) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }
}

fn header_str(req: &Request, name: header::HeaderName) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Middleware in front of all routes.
pub async fn guard(State(access): State<Arc<Access>>, req: Request, next: Next) -> Response {
    let host = header_str(&req, header::HOST);
    let origin = header_str(&req, header::ORIGIN);
    let mut verdict = access.check(host.as_deref(), origin.as_deref());
    if verdict.is_err() && access.should_probe() {
        let probe = access.probe;
        if let Ok(Some(ts)) = tokio::task::spawn_blocking(probe).await {
            tracing::info!(host = %ts, "Tailscale name looked up");
            access.list.lock().unwrap().tailscale_host = Some(ts.to_ascii_lowercase());
            verdict = access.check(host.as_deref(), origin.as_deref());
        }
    }
    match verdict {
        Ok(()) => next.run(req).await,
        Err(why) => {
            tracing::warn!(
                ?why,
                host = host.as_deref().unwrap_or("-"),
                origin = origin.as_deref().unwrap_or("-"),
                path = %req.uri().path(),
                "request rejected"
            );
            (StatusCode::FORBIDDEN, "forbidden").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> AllowList {
        AllowList::new(8321, Some("pc.tail0000.ts.net".into()), &[])
    }

    #[test]
    fn local_clients_without_origin_pass() {
        let l = list();
        assert_eq!(l.check(Some("127.0.0.1:8321"), None), Ok(()));
        assert_eq!(l.check(Some("localhost:8321"), None), Ok(()));
        assert_eq!(l.check(Some("[::1]:8321"), None), Ok(()));
    }

    #[test]
    fn own_pages_pass() {
        let l = list();
        assert_eq!(
            l.check(Some("127.0.0.1:8321"), Some("http://127.0.0.1:8321")),
            Ok(())
        );
        // Behind `tailscale serve`: Host is the Tailscale name or the loopback address.
        for host in [
            "pc.tail0000.ts.net",
            "PC.tail0000.ts.net:443",
            "127.0.0.1:8321",
        ] {
            assert_eq!(
                l.check(Some(host), Some("https://pc.tail0000.ts.net")),
                Ok(()),
                "{host}"
            );
        }
        assert_eq!(
            l.check(
                Some("pc.tail0000.ts.net"),
                Some("https://pc.tail0000.ts.net:443/")
            ),
            Ok(())
        );
    }

    #[test]
    fn foreign_origin_is_rejected() {
        let l = list();
        for origin in [
            "https://evil.example",
            "http://pc.tail0000.ts.net",
            "https://pc.tail0000.ts.net:8443",
            "https://other.tail0000.ts.net",
            "http://127.0.0.1:5111",
            "http://localhost",
            "null",
            "file://",
        ] {
            assert_eq!(
                l.check(Some("127.0.0.1:8321"), Some(origin)),
                Err(Denied::Origin),
                "{origin}"
            );
        }
    }

    #[test]
    fn dns_rebinding_host_is_rejected() {
        let l = list();
        assert_eq!(
            l.check(Some("evil.example:8321"), Some("http://evil.example:8321")),
            Err(Denied::Host)
        );
        assert_eq!(l.check(Some("evil.example:8321"), None), Err(Denied::Host));
        assert_eq!(l.check(None, None), Err(Denied::MissingHost));
    }

    #[test]
    fn without_tailscale_only_loopback_passes() {
        let l = AllowList::new(8321, None, &[]);
        assert_eq!(
            l.check(
                Some("pc.tail0000.ts.net"),
                Some("https://pc.tail0000.ts.net")
            ),
            Err(Denied::Host)
        );
        assert_eq!(
            l.check(Some("127.0.0.1:8321"), Some("http://127.0.0.1:8321")),
            Ok(())
        );
    }

    #[test]
    fn extra_origin_allows_host_and_origin() {
        let l = AllowList::new(
            8321,
            None,
            &["https://mic.example.net/".into(), "kaputt".into()],
        );
        assert_eq!(
            l.check(Some("mic.example.net"), Some("https://mic.example.net")),
            Ok(())
        );
        assert_eq!(
            l.check(Some("mic.example.net"), Some("http://mic.example.net")),
            Err(Denied::Origin)
        );
    }

    #[test]
    fn host_parsing() {
        assert_eq!(
            split_host_port("[::1]:8321"),
            Some(("[::1]".into(), Some(8321)))
        );
        assert_eq!(
            split_host_port("Host.Example"),
            Some(("host.example".into(), None))
        );
        assert_eq!(split_host_port("host:abc"), None);
        assert_eq!(split_host_port(""), None);
    }
}
