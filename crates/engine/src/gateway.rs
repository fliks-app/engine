//! Front-door gateway: a route table picks engine routes, everything else is forwarded (docs/spec/gateway.md).
use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, Version,
        header::{CONNECTION, HOST, UPGRADE},
        uri::Authority,
    },
    response::{IntoResponse, Response},
};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
};
use serde::Deserialize;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tower::ServiceExt;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Engine,
    Forward,
}

#[derive(Debug, Deserialize)]
struct RuleSpec {
    target: Target,
    #[serde(default)]
    methods: Vec<String>,
    path: String,
}

#[derive(Debug)]
struct Rule {
    target: Target,
    methods: Vec<Method>,
    segments: Vec<String>,
    tail: bool,
}

/// Ordered rules, first match wins, no match forwards.
#[derive(Debug)]
pub struct RouteTable(Vec<Rule>);

impl RouteTable {
    pub fn from_json(json: &str) -> Result<Self, String> {
        let specs: Vec<RuleSpec> = serde_json::from_str(json).map_err(|e| e.to_string())?;
        specs
            .into_iter()
            .map(|s| {
                let mut segments: Vec<String> = s
                    .path
                    .strip_prefix('/')
                    .ok_or_else(|| format!("route path must start with '/': {}", s.path))?
                    .split('/')
                    .map(str::to_owned)
                    .collect();
                let tail = segments.last().is_some_and(|l| l == "**");
                if tail {
                    segments.pop();
                }
                if segments.iter().any(|s| s == "**") {
                    return Err(format!("'**' must be the last segment: {}", s.path));
                }
                let methods = s
                    .methods
                    .iter()
                    .map(|m| {
                        Method::from_bytes(m.to_uppercase().as_bytes()).map_err(|e| e.to_string())
                    })
                    .collect::<Result<_, _>>()?;
                Ok(Rule {
                    target: s.target,
                    methods,
                    segments,
                    tail,
                })
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }

    /// The Fliks table (plan §16).
    pub fn fliks() -> Self {
        Self::from_json(include_str!("fliks-routes.json")).expect("builtin table")
    }

    pub fn target(&self, method: &Method, path: &str) -> Target {
        let segs: Vec<&str> = path.strip_prefix('/').unwrap_or(path).split('/').collect();
        self.0
            .iter()
            .find(|r| {
                (r.methods.is_empty() || r.methods.contains(method))
                    && (segs.len() == r.segments.len() || r.tail && segs.len() > r.segments.len())
                    && r.segments.iter().zip(&segs).all(|(p, s)| match p.as_str() {
                        p if p == "*" || p.starts_with(':') => !s.is_empty(),
                        p => p == *s,
                    })
            })
            .map_or(Target::Forward, |r| r.target)
    }
}

/// `http://host[:port]` of the host the gateway forwards to.
#[derive(Clone, Debug)]
pub struct Upstream {
    authority: Authority,
    url: String,
}

impl Upstream {
    pub fn parse(s: &str) -> Result<Self, String> {
        let uri: Uri = s.parse().map_err(|e| format!("invalid upstream: {e}"))?;
        match (uri.scheme_str(), uri.authority(), uri.path()) {
            (Some("http"), Some(a), "/" | "") if uri.query().is_none() => Ok(Self {
                authority: a.clone(),
                url: format!("http://{a}"),
            }),
            _ => Err("upstream must be http://host[:port] (no path, no TLS)".into()),
        }
    }

    pub fn port(&self) -> u16 {
        self.authority.port_u16().unwrap_or(80)
    }

    /// Origin with no trailing slash, for `FLIKS_ENGINE_URL`.
    pub fn url(&self) -> &str {
        &self.url
    }
}

struct Gateway {
    table: RouteTable,
    host: Authority,
    engine_local: Option<Router>,
    engine_remote: Option<Authority>,
    client: Client<HttpConnector, Body>,
}

fn new_gateway(
    table: RouteTable,
    host: Upstream,
    engine_local: Option<Router>,
    engine_remote: Option<Upstream>,
) -> Arc<Gateway> {
    let mut http = HttpConnector::new();
    http.set_connect_timeout(Some(Duration::from_secs(5)));
    http.set_nodelay(true);
    Arc::new(Gateway {
        table,
        host: host.authority,
        engine_local,
        engine_remote: engine_remote.map(|u| u.authority),
        client: Client::builder(TokioExecutor::new()).build(http),
    })
}

/// Public router. `engine` serves the table's engine routes; without it they are forwarded.
pub fn router(table: RouteTable, upstream: Upstream, engine: Option<Router>) -> Router {
    Router::new()
        .fallback(handle)
        .with_state(new_gateway(table, upstream, engine, None))
}

/// Forwards engine-table routes to `engine` and everything else to `host` (no local data plane).
pub fn router_remote(table: RouteTable, host: Upstream, engine: Upstream) -> Router {
    Router::new()
        .fallback(handle)
        .with_state(new_gateway(table, host, None, Some(engine)))
}

async fn handle(State(gw): State<Arc<Gateway>>, req: Request) -> Response {
    if gw.table.target(req.method(), req.uri().path()) == Target::Engine {
        if let Some(engine) = &gw.engine_local {
            return engine.clone().oneshot(req).await.into_response();
        }
        let authority = gw.engine_remote.as_ref().unwrap_or(&gw.host).clone();
        return forward(gw.as_ref(), req, authority).await;
    }
    forward(gw.as_ref(), req, gw.host.clone()).await
}

const HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
];

fn is_upgrade(h: &HeaderMap) -> bool {
    h.contains_key(UPGRADE)
        && h.get_all(CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| {
                v.split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
            })
}

/// Drops hop-by-hop headers; on an upgrade re-adds the pair that carries it.
fn strip_hop(h: &mut HeaderMap, keep_upgrade: bool) {
    let upgrade = h.get(UPGRADE).cloned();
    let named: Vec<HeaderName> = h
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|t| HeaderName::from_bytes(t.trim().as_bytes()).ok())
        .collect();
    for n in HOP.iter().map(|n| HeaderName::from_static(n)).chain(named) {
        h.remove(n);
    }
    h.remove(UPGRADE);
    if let (true, Some(u)) = (keep_upgrade, upgrade) {
        h.insert(CONNECTION, HeaderValue::from_static("upgrade"));
        h.insert(UPGRADE, u);
    }
}

fn set_forwarded(h: &mut HeaderMap, peer: Option<SocketAddr>) {
    if let Some(p) = peer {
        let prev = h
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok());
        let chain = prev
            .chain([p.ip().to_string().as_str()])
            .collect::<Vec<_>>()
            .join(", ");
        h.insert("x-forwarded-for", HeaderValue::from_str(&chain).unwrap());
    }
    if !h.contains_key("x-forwarded-proto") {
        h.insert("x-forwarded-proto", HeaderValue::from_static("http"));
    }
    if !h.contains_key("x-forwarded-host")
        && let Some(host) = h.get(HOST).cloned()
    {
        h.insert("x-forwarded-host", host);
    }
}

async fn forward(gw: &Gateway, mut req: Request, authority: Authority) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let upgrade = is_upgrade(req.headers());
    let client_io = upgrade.then(|| hyper::upgrade::on(&mut req));
    let (mut parts, body) = req.into_parts();
    let pq = parts.uri.path_and_query().cloned();
    parts.uri = match Uri::builder()
        .scheme("http")
        .authority(authority.clone())
        .path_and_query(pq.as_ref().map_or("/", |p| p.as_str()))
        .build()
    {
        Ok(u) => u,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    parts.version = Version::HTTP_11;
    strip_hop(&mut parts.headers, upgrade);
    set_forwarded(&mut parts.headers, peer);
    let mut res = match gw.client.request(Request::from_parts(parts, body)).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("upstream {}: {e}", authority);
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let switching = res.status() == StatusCode::SWITCHING_PROTOCOLS;
    if let (true, Some(client_io)) = (switching, client_io) {
        let upstream_io = hyper::upgrade::on(&mut res);
        tokio::spawn(async move {
            if let (Ok(c), Ok(u)) = (client_io.await, upstream_io.await) {
                let _ =
                    tokio::io::copy_bidirectional(&mut TokioIo::new(c), &mut TokioIo::new(u)).await;
            }
        });
    }
    strip_hop(res.headers_mut(), switching);
    res.map(Body::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fliks_table_matches_the_route_inventory() {
        let t = RouteTable::fliks();
        let at = |m: Method, p: &str| t.target(&m, p);
        let (e, f) = (Target::Engine, Target::Forward);
        for p in [
            "/api/stream/7",
            "/api/stream/7/master.m3u8",
            "/api/stream/7/720p/seg1.m4s",
            "/api/stream/7/subtitles/embedded/2/index.m3u8",
            "/api/stream/7/thumbnails/sprite.jpg",
            "/api/livetv/sessions/ab12/index.m3u8",
            "/api/livetv/sessions/ab12/seg-00012.m4s",
            "/api/livetv/sessions/ab12/direct.ts",
        ] {
            assert_eq!(at(Method::GET, p), e, "{p}");
            assert_eq!(at(Method::HEAD, p), e, "{p}");
        }
        assert_eq!(at(Method::GET, "/api/stream/7/edl"), f);
        assert_eq!(at(Method::GET, "/api/stream/info/qualities"), f);
        assert_eq!(at(Method::GET, "/api/stream/info"), f);
        assert_eq!(at(Method::POST, "/api/stream/7/playback-info"), f);
        assert_eq!(at(Method::DELETE, "/api/stream/sessions/s1"), f);
        assert_eq!(at(Method::GET, "/api/stream"), f);
        assert_eq!(at(Method::GET, "/api/stream//master.m3u8"), f);
        assert_eq!(at(Method::GET, "/api/system/liveness"), f);
        assert_eq!(at(Method::DELETE, "/api/livetv/sessions/ab12"), f);
        assert_eq!(at(Method::PUT, "/api/livetv/sessions/ab12/state"), f);
        assert_eq!(at(Method::POST, "/api/livetv/channels/3/play"), f);
        assert_eq!(at(Method::GET, "/api/livetv/channels"), f);
        assert_eq!(at(Method::GET, "/v1/hello"), f);
    }

    #[test]
    fn rejects_bad_tables() {
        assert!(RouteTable::from_json(r#"[{"target":"engine","path":"/a/**/b"}]"#).is_err());
        assert!(RouteTable::from_json(r#"[{"target":"engine","path":"a"}]"#).is_err());
        assert!(Upstream::parse("https://x:1").is_err());
        assert!(Upstream::parse("http://x:1/p").is_err());
        assert_eq!(Upstream::parse("http://x:9000").unwrap().port(), 9000);
        assert_eq!(
            Upstream::parse("http://x:9000/").unwrap().url(),
            "http://x:9000"
        );
    }
}
