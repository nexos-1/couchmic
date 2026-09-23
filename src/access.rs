//! Access control for HTTP and WebSocket.
//!
//! The HTTP server only listens on 127.0.0.1; other devices reach it only through
//! `tailscale serve`. Without further checks these ways would stay open:
//! - Cross-site WebSocket: any web page in a browser on the PC or on a tailnet device opens
//!   `ws://127.0.0.1:8321/ws` or `wss://<pc>.ts.net/ws`, switches the default microphone and
//!   plays its own audio. WebSockets have no CORS; the server has to check the origin itself.
//! - DNS rebinding: a foreign domain that points to 127.0.0.1 is "same origin" for the browser
//!   and would reach /ws and /api/stats. The host allowlist stops that.
//! - Framing / cross-site navigation: another site embeds or links the page to trick a tap on
//!   "Start". Stopped by `Sec-Fetch-Site` and the frame headers.
//! - Tailscale Funnel: Funnel is enabled per host:port, not per path. If the user ever turns it
//!   on for port 443, glass-mic would be reachable from the whole internet. `tailscale serve`
//!   marks those requests with `Tailscale-Funnel-Request`; they are always rejected.
//! - Other tailnet users: in a shared tailnet anyone could inject audio. Requests forwarded by
//!   `tailscale serve` (they carry `X-Forwarded-Host`) must carry the `Tailscale-User-Login` of
//!   the PC's own Tailscale user. Tailscale strips client copies of these headers and sets them
//!   itself, so they cannot be forged through serve.
//!
//! Rules (all must hold):
//! 1. No `Tailscale-Funnel-Request` header.
//! 2. `Sec-Fetch-Site` is absent, `none` or `same-origin`.
//! 3. The Host header is a loopback address, the Tailscale name or an `--allow-origin` host.
//! 4. An Origin, if present, must be readable and belong to exactly these addresses (loopback
//!    only with our own port). Without an Origin (curl, LocalFlow) the host check is enough.
//! 5. Forwarded requests need the owner's Tailscale login (unless `--allow-any-tailnet-user`).

use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the Tailscale identity is looked up at most while it is unknown (at autostart
/// after logon Tailscale is often not running yet).
const PROBE_INTERVAL: Duration = Duration::from_secs(30);
/// At most this many "request rejected" lines per minute; the rest is only counted.
const REJECT_LOG_PER_MINUTE: u32 = 20;
/// Logged header values are cut to this many characters.
const LOG_FIELD_MAX: usize = 128;

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
    Funnel,
    CrossSite,
    MissingHost,
    Host,
    Origin,
    TailnetUser,
}

/// A header as the check sees it: absent, present but not valid UTF-8, or a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hdr {
    Absent,
    Unreadable,
    Value(String),
}

impl Hdr {
    fn from_map(map: &HeaderMap, name: &str) -> Self {
        match map.get(name) {
            None => Hdr::Absent,
            Some(v) => match v.to_str() {
                Ok(s) => Hdr::Value(s.to_string()),
                Err(_) => Hdr::Unreadable,
            },
        }
    }

    fn is_present(&self) -> bool {
        !matches!(self, Hdr::Absent)
    }

    fn log_value(&self) -> String {
        match self {
            Hdr::Absent => "-".into(),
            Hdr::Unreadable => "<unreadable>".into(),
            Hdr::Value(v) => clip(v),
        }
    }
}

/// The request facts the rules look at.
#[derive(Debug, Clone)]
pub struct RequestInfo {
    pub host: Hdr,
    pub origin: Hdr,
    pub sec_fetch_site: Hdr,
    /// `X-Forwarded-Host`: set by `tailscale serve` on every forwarded request.
    pub forwarded_host: Hdr,
    pub funnel: Hdr,
    pub tailscale_login: Hdr,
}

impl RequestInfo {
    pub fn from_headers(h: &HeaderMap) -> Self {
        Self {
            host: Hdr::from_map(h, "host"),
            origin: Hdr::from_map(h, "origin"),
            sec_fetch_site: Hdr::from_map(h, "sec-fetch-site"),
            forwarded_host: Hdr::from_map(h, "x-forwarded-host"),
            funnel: Hdr::from_map(h, "tailscale-funnel-request"),
            tailscale_login: Hdr::from_map(h, "tailscale-user-login"),
        }
    }
}

/// The PC's own Tailscale identity from `tailscale status --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscaleSelf {
    /// MagicDNS name without the trailing dot, e.g. "pc.tail1234.ts.net".
    pub host: String,
    /// Login name of the Tailscale user that owns this PC, e.g. "me@example.com".
    pub login: Option<String>,
}

/// Allowed addresses and users. Plain data, so the rules are testable without a server.
#[derive(Debug, Clone)]
pub struct AllowList {
    port: u16,
    tailscale: Option<TailscaleSelf>,
    extra: Vec<Origin>,
    allow_any_tailnet_user: bool,
}

impl AllowList {
    /// `extra`: additional origins like "https://mic.example.net"; invalid ones are reported.
    pub fn new(
        port: u16,
        tailscale: Option<TailscaleSelf>,
        extra: &[String],
        allow_any_tailnet_user: bool,
    ) -> Self {
        let extra = extra
            .iter()
            .filter_map(|o| {
                let parsed = parse_origin(o);
                if parsed.is_none() {
                    tracing::warn!(origin = %clip(o), "invalid --allow-origin, ignored");
                }
                parsed
            })
            .collect();
        Self {
            port,
            tailscale: tailscale.map(|t| TailscaleSelf {
                host: t.host.to_ascii_lowercase(),
                login: t.login.map(|l| l.to_ascii_lowercase()),
            }),
            extra,
            allow_any_tailnet_user,
        }
    }

    fn tailscale_origin(&self) -> Option<Origin> {
        self.tailscale.as_ref().map(|t| Origin {
            scheme: "https".into(),
            host: t.host.clone(),
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

    fn tailnet_user_ok(&self, login: &Hdr) -> bool {
        if self.allow_any_tailnet_user {
            return true;
        }
        let Some(owner) = self.tailscale.as_ref().and_then(|t| t.login.as_ref()) else {
            return false;
        };
        match login {
            Hdr::Value(v) => v.trim().to_ascii_lowercase() == *owner,
            _ => false,
        }
    }

    pub fn check(&self, r: &RequestInfo) -> Result<(), Denied> {
        if r.funnel.is_present() {
            return Err(Denied::Funnel);
        }
        match &r.sec_fetch_site {
            Hdr::Absent => {}
            Hdr::Value(v) if v == "none" || v == "same-origin" => {}
            _ => return Err(Denied::CrossSite),
        }
        let host = match &r.host {
            Hdr::Value(h) => h,
            Hdr::Absent => return Err(Denied::MissingHost),
            Hdr::Unreadable => return Err(Denied::Host),
        };
        if !self.host_ok(host) {
            return Err(Denied::Host);
        }
        match &r.origin {
            Hdr::Absent => {}
            Hdr::Unreadable => return Err(Denied::Origin),
            Hdr::Value(o) if !self.origin_ok(o) => return Err(Denied::Origin),
            Hdr::Value(_) => {}
        }
        if r.forwarded_host.is_present() && !self.tailnet_user_ok(&r.tailscale_login) {
            return Err(Denied::TailnetUser);
        }
        Ok(())
    }

    /// Whether looking up the Tailscale identity again could change a rejection.
    fn identity_incomplete(&self) -> bool {
        self.tailscale.as_ref().is_none_or(|t| t.login.is_none())
    }
}

fn clip(s: &str) -> String {
    let cut: String = s.chars().take(LOG_FIELD_MAX).collect();
    let mut out = cut.escape_debug().to_string();
    if s.chars().count() > LOG_FIELD_MAX {
        out.push_str("...");
    }
    out
}

/// Rate limit for the rejection log.
struct RejectLog {
    window_start: Instant,
    logged: u32,
    suppressed: u64,
}

/// Shared state for the middleware: allowlist plus lazy lookup of the Tailscale identity.
pub struct Access {
    list: Mutex<AllowList>,
    last_probe: Mutex<Option<Instant>>,
    probe: fn() -> Option<TailscaleSelf>,
    reject_log: Mutex<RejectLog>,
}

impl Access {
    pub fn new(list: AllowList, probe: fn() -> Option<TailscaleSelf>) -> Arc<Self> {
        Arc::new(Self {
            list: Mutex::new(list),
            last_probe: Mutex::new(None),
            probe,
            reject_log: Mutex::new(RejectLog {
                window_start: Instant::now(),
                logged: 0,
                suppressed: 0,
            }),
        })
    }

    fn check(&self, r: &RequestInfo) -> Result<(), Denied> {
        self.list.lock().unwrap().check(r)
    }

    /// Only while the Tailscale identity is incomplete and the last lookup is long enough ago.
    fn should_probe(&self) -> bool {
        if !self.list.lock().unwrap().identity_incomplete() {
            return false;
        }
        let mut last = self.last_probe.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < PROBE_INTERVAL) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }

    /// True if this rejection may be logged; prints a summary of suppressed ones per minute.
    fn may_log_rejection(&self) -> bool {
        let mut l = self.reject_log.lock().unwrap();
        if l.window_start.elapsed() >= Duration::from_secs(60) {
            if l.suppressed > 0 {
                tracing::warn!(
                    suppressed = l.suppressed,
                    "more requests rejected (not logged)"
                );
            }
            l.window_start = Instant::now();
            l.logged = 0;
            l.suppressed = 0;
        }
        if l.logged < REJECT_LOG_PER_MINUTE {
            l.logged += 1;
            true
        } else {
            l.suppressed += 1;
            false
        }
    }
}

/// Security headers on every response: never framed, no MIME sniffing, no referrer.
fn add_security_headers(res: &mut Response) {
    let h = res.headers_mut();
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
}

/// Middleware in front of all routes.
pub async fn guard(State(access): State<Arc<Access>>, req: Request, next: Next) -> Response {
    let info = RequestInfo::from_headers(req.headers());
    let mut verdict = access.check(&info);
    if matches!(verdict, Err(Denied::Host) | Err(Denied::TailnetUser)) && access.should_probe() {
        let probe = access.probe;
        if let Ok(Some(ts)) = tokio::task::spawn_blocking(probe).await {
            tracing::info!(host = %ts.host, login_known = ts.login.is_some(), "Tailscale identity looked up");
            let mut list = access.list.lock().unwrap();
            list.tailscale = Some(TailscaleSelf {
                host: ts.host.to_ascii_lowercase(),
                login: ts.login.map(|l| l.to_ascii_lowercase()),
            });
            drop(list);
            verdict = access.check(&info);
        }
    }
    let mut res = match verdict {
        Ok(()) => next.run(req).await,
        Err(why) => {
            if access.may_log_rejection() {
                tracing::warn!(
                    ?why,
                    host = %info.host.log_value(),
                    origin = %info.origin.log_value(),
                    sec_fetch_site = %info.sec_fetch_site.log_value(),
                    forwarded = info.forwarded_host.is_present(),
                    login = %info.tailscale_login.log_value(),
                    path = %clip(req.uri().path()),
                    "request rejected"
                );
            }
            (StatusCode::FORBIDDEN, "forbidden").into_response()
        }
    };
    add_security_headers(&mut res);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: &str = "pc.tail0000.ts.net";
    const OWNER: &str = "me@example.com";

    fn list() -> AllowList {
        AllowList::new(
            8321,
            Some(TailscaleSelf {
                host: TS.into(),
                login: Some(OWNER.into()),
            }),
            &[],
            false,
        )
    }

    fn v(s: &str) -> Hdr {
        Hdr::Value(s.into())
    }

    /// A direct local request (LocalFlow, curl, a local browser).
    fn local(host: &str, origin: Option<&str>) -> RequestInfo {
        RequestInfo {
            host: v(host),
            origin: origin.map(v).unwrap_or(Hdr::Absent),
            sec_fetch_site: Hdr::Absent,
            forwarded_host: Hdr::Absent,
            funnel: Hdr::Absent,
            tailscale_login: Hdr::Absent,
        }
    }

    /// A request forwarded by `tailscale serve` from a tailnet device of `login`.
    fn via_serve(origin: Option<&str>, login: Option<&str>) -> RequestInfo {
        RequestInfo {
            host: v(TS),
            origin: origin.map(v).unwrap_or(Hdr::Absent),
            sec_fetch_site: Hdr::Absent,
            forwarded_host: v(TS),
            funnel: Hdr::Absent,
            tailscale_login: login.map(v).unwrap_or(Hdr::Absent),
        }
    }

    #[test]
    fn local_clients_without_origin_pass() {
        let l = list();
        for host in ["127.0.0.1:8321", "localhost:8321", "[::1]:8321"] {
            assert_eq!(l.check(&local(host, None)), Ok(()), "{host}");
        }
    }

    #[test]
    fn own_pages_pass() {
        let l = list();
        assert_eq!(
            l.check(&local("127.0.0.1:8321", Some("http://127.0.0.1:8321"))),
            Ok(())
        );
        let own = format!("https://{TS}");
        assert_eq!(l.check(&via_serve(Some(&own), Some(OWNER))), Ok(()));
        assert_eq!(
            l.check(&via_serve(
                Some(&format!("{own}:443/")),
                Some("ME@example.com")
            )),
            Ok(()),
            "login compare is case-insensitive"
        );
        let mut r = via_serve(Some(&own), Some(OWNER));
        r.sec_fetch_site = v("same-origin");
        assert_eq!(l.check(&r), Ok(()));
        r.sec_fetch_site = v("none");
        assert_eq!(l.check(&r), Ok(()), "typed URL, bookmark, Shortcuts");
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
                l.check(&local("127.0.0.1:8321", Some(origin))),
                Err(Denied::Origin),
                "{origin}"
            );
        }
        let mut r = local("127.0.0.1:8321", None);
        r.origin = Hdr::Unreadable;
        assert_eq!(
            l.check(&r),
            Err(Denied::Origin),
            "unreadable origin fails closed"
        );
    }

    #[test]
    fn dns_rebinding_host_is_rejected() {
        let l = list();
        assert_eq!(
            l.check(&local(
                "evil.example:8321",
                Some("http://evil.example:8321")
            )),
            Err(Denied::Host)
        );
        assert_eq!(
            l.check(&local("evil.example:8321", None)),
            Err(Denied::Host)
        );
        let mut r = local("x", None);
        r.host = Hdr::Absent;
        assert_eq!(l.check(&r), Err(Denied::MissingHost));
        r.host = Hdr::Unreadable;
        assert_eq!(l.check(&r), Err(Denied::Host));
        for host in [
            "pc.tail0000.ts.net.",
            "127.0.0.1.evil.com",
            "[::ffff:127.0.0.1]:8321",
            "127.1:8321",
            "0.0.0.0:8321",
            "2130706433:8321",
            "user@127.0.0.1:8321",
        ] {
            assert_eq!(l.check(&local(host, None)), Err(Denied::Host), "{host}");
        }
    }

    #[test]
    fn funnel_is_always_rejected() {
        let l = list();
        let mut r = via_serve(Some(&format!("https://{TS}")), None);
        r.funnel = v("?1");
        assert_eq!(l.check(&r), Err(Denied::Funnel));
        let any = AllowList::new(8321, None, &[], true);
        let mut r = local("127.0.0.1:8321", None);
        r.funnel = v("?1");
        assert_eq!(
            any.check(&r),
            Err(Denied::Funnel),
            "even with --allow-any-tailnet-user"
        );
    }

    #[test]
    fn cross_site_requests_are_rejected() {
        let l = list();
        for site in ["cross-site", "same-site", "garbage"] {
            let mut r = local("127.0.0.1:8321", None);
            r.sec_fetch_site = v(site);
            assert_eq!(l.check(&r), Err(Denied::CrossSite), "{site}");
        }
    }

    #[test]
    fn forwarded_requests_need_the_owners_login() {
        let l = list();
        let own = format!("https://{TS}");
        assert_eq!(
            l.check(&via_serve(Some(&own), Some("someone@else.com"))),
            Err(Denied::TailnetUser)
        );
        assert_eq!(
            l.check(&via_serve(Some(&own), None)),
            Err(Denied::TailnetUser),
            "tagged devices carry no login"
        );
        // A tailnet client that fakes a loopback Host through serve is still forwarded.
        let mut r = via_serve(None, Some("someone@else.com"));
        r.host = v("127.0.0.1:8321");
        assert_eq!(l.check(&r), Err(Denied::TailnetUser));
        // Owner unknown (Tailscale was not running at startup): fail closed.
        let unknown = AllowList::new(
            8321,
            Some(TailscaleSelf {
                host: TS.into(),
                login: None,
            }),
            &[],
            false,
        );
        assert_eq!(
            unknown.check(&via_serve(Some(&own), Some(OWNER))),
            Err(Denied::TailnetUser)
        );
        assert!(unknown.identity_incomplete());
        // Opt-out for custom reverse proxies.
        let any = AllowList::new(
            8321,
            Some(TailscaleSelf {
                host: TS.into(),
                login: None,
            }),
            &[],
            true,
        );
        assert_eq!(any.check(&via_serve(Some(&own), None)), Ok(()));
    }

    #[test]
    fn without_tailscale_only_loopback_passes() {
        let l = AllowList::new(8321, None, &[], false);
        assert_eq!(
            l.check(&local(TS, Some(&format!("https://{TS}")))),
            Err(Denied::Host)
        );
        assert_eq!(
            l.check(&local("127.0.0.1:8321", Some("http://127.0.0.1:8321"))),
            Ok(())
        );
    }

    #[test]
    fn extra_origin_allows_host_and_origin() {
        let l = AllowList::new(
            8321,
            None,
            &["https://mic.example.net/".into(), "kaputt".into()],
            false,
        );
        assert_eq!(
            l.check(&local("mic.example.net", Some("https://mic.example.net"))),
            Ok(())
        );
        assert_eq!(
            l.check(&local("mic.example.net", Some("http://mic.example.net"))),
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

    #[test]
    fn clip_escapes_and_cuts() {
        assert_eq!(clip("a\nb"), "a\\nb");
        let long = "x".repeat(500);
        assert_eq!(clip(&long).len(), LOG_FIELD_MAX + 3);
    }
}
