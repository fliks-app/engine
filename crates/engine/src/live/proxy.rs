//! Loopback forward proxy for the ffmpeg/ffprobe inputs of vetted upstreams: each hop (redirect,
//! playlist entry, segment) is resolved once here, vetted, and dialed at what passed.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;

const MAX_HEAD: usize = 16 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Stops accepting when dropped.
pub(super) struct VetProxy {
    url: String,
    accept: AbortHandle,
}

impl VetProxy {
    /// `refuse` names the addresses never dialed.
    pub(super) async fn start(refuse: fn(IpAddr) -> bool) -> io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let url = format!("http://{}", listener.local_addr()?);
        let accept = tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                tokio::spawn(serve(conn, refuse));
            }
        })
        .abort_handle();
        Ok(Self { url, accept })
    }

    pub(super) fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for VetProxy {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

struct Failure(u16, String);

async fn serve(mut conn: TcpStream, refuse: fn(IpAddr) -> bool) {
    if let Err(Failure(code, why)) = relay(&mut conn, refuse).await {
        let reason = if code == 403 {
            "Forbidden"
        } else {
            "Bad Gateway"
        };
        let body = format!(
            "HTTP/1.1 {code} {reason}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{why}",
            why.len()
        );
        let _ = conn.write_all(body.as_bytes()).await;
    }
}

/// The request head and whatever followed it in the same reads.
async fn read_head(conn: &mut TcpStream) -> Result<(String, Vec<u8>), Failure> {
    let bad = |m: &str| Failure(400, m.into());
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err(bad("request head too large"));
        }
        let n = tokio::time::timeout(HEAD_TIMEOUT, conn.read(&mut chunk))
            .await
            .map_err(|_| bad("request head timed out"))?
            .map_err(|e| bad(&e.to_string()))?;
        if n == 0 {
            return Err(bad("connection closed in the request head"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head =
        String::from_utf8(buf[..end].to_vec()).map_err(|_| bad("request head is not UTF-8"))?;
    Ok((head, buf.split_off(end)))
}

async fn relay(conn: &mut TcpStream, refuse: fn(IpAddr) -> bool) -> Result<(), Failure> {
    let (head, rest) = read_head(conn).await?;
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split(' ');
    let (method, target, version) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or("HTTP/1.1"),
    );
    if method.eq_ignore_ascii_case("CONNECT") {
        let mut up = dial(target, 443, refuse).await?;
        conn.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .map_err(|e| Failure(502, e.to_string()))?;
        up.write_all(&rest)
            .await
            .map_err(|e| Failure(502, e.to_string()))?;
        let _ = copy_bidirectional(conn, &mut up).await;
        return Ok(());
    }
    let url = target
        .strip_prefix("http://")
        .ok_or_else(|| Failure(400, "only absolute http URLs and CONNECT".into()))?;
    let (authority, path) = url
        .find(['/', '?'])
        .map_or((url, "/"), |i| (&url[..i], &url[i..]));
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let mut up = dial(authority, 80, refuse).await?;
    // One request per connection: a later one could name another host.
    let mut out = format!("{method} {path} {version}\r\n");
    for l in lines.filter(|l| !l.is_empty()) {
        let name = l.split(':').next().unwrap_or_default().to_ascii_lowercase();
        if !matches!(
            name.as_str(),
            "connection" | "proxy-connection" | "proxy-authorization" | "keep-alive"
        ) {
            out.push_str(l);
            out.push_str("\r\n");
        }
    }
    out.push_str("Connection: close\r\n\r\n");
    up.write_all(out.as_bytes())
        .await
        .map_err(|e| Failure(502, e.to_string()))?;
    up.write_all(&rest)
        .await
        .map_err(|e| Failure(502, e.to_string()))?;
    let _ = copy_bidirectional(conn, &mut up).await;
    Ok(())
}

/// `host[:port]` or `[v6][:port]`, userinfo dropped.
fn split_authority(a: &str, default_port: u16) -> Option<(&str, u16)> {
    let a = a.rsplit_once('@').map_or(a, |(_, h)| h);
    let (host, port) = match a.strip_prefix('[') {
        Some(r) => {
            let (h, tail) = r.split_once(']')?;
            (h, tail.strip_prefix(':'))
        }
        None => match a.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (a, None),
        },
    };
    let port = match port {
        Some(p) => p.parse().ok()?,
        None => default_port,
    };
    (!host.is_empty()).then_some((host, port))
}

/// One resolution; every address must pass, and the first that connects is used.
async fn dial(
    authority: &str,
    default_port: u16,
    refuse: fn(IpAddr) -> bool,
) -> Result<TcpStream, Failure> {
    let (host, port) = split_authority(authority, default_port)
        .ok_or_else(|| Failure(400, format!("bad authority \"{authority}\"")))?;
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| Failure(502, format!("Could not resolve \"{host}\"")))?
        .collect();
    if let Some(a) = addrs.iter().find(|a| refuse(a.ip())) {
        return Err(Failure(
            403,
            format!("Refused: \"{host}\" is an internal address ({})", a.ip()),
        ));
    }
    for a in &addrs {
        if let Ok(Ok(s)) = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(a)).await {
            return Ok(s);
        }
    }
    Err(Failure(502, format!("Could not connect to \"{host}\"")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn origin() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut c, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut b = [0u8; 1024];
                    let n = c.read(&mut b).await.unwrap();
                    let req = String::from_utf8_lossy(&b[..n]).into_owned();
                    let body = format!("saw {}", req.lines().next().unwrap());
                    let _ = c
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        port
    }

    async fn via(proxy: &VetProxy, request: String) -> String {
        let addr = proxy.url().trim_start_matches("http://").to_string();
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(request.as_bytes()).await.unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn forwards_what_passes_and_refuses_the_rest() {
        let port = origin().await;
        let req = format!(
            "GET http://127.0.0.1:{port}/a?b=1 HTTP/1.1\r\nHost: x\r\nProxy-Connection: keep-alive\r\n\r\n"
        );
        let open = VetProxy::start(|_| false).await.unwrap();
        assert!(
            via(&open, req.clone())
                .await
                .ends_with("saw GET /a?b=1 HTTP/1.1")
        );
        let refusing = VetProxy::start(super::super::vet::is_internal)
            .await
            .unwrap();
        assert!(via(&refusing, req).await.starts_with("HTTP/1.1 403"));
        let connect = "CONNECT 169.254.169.254:80 HTTP/1.1\r\n\r\n".to_string();
        assert!(via(&refusing, connect).await.starts_with("HTTP/1.1 403"));
    }

    #[test]
    fn authorities() {
        assert_eq!(split_authority("h:81", 80), Some(("h", 81)));
        assert_eq!(split_authority("u:p@h", 80), Some(("h", 80)));
        assert_eq!(split_authority("[::1]:9", 80), Some(("::1", 9)));
        assert_eq!(split_authority(":9", 80), None);
    }

    /// Real ffprobe through the proxy (`ENGINE_E2E=1`): the playlist is on 127.0.0.1, its segment
    /// on 127.0.0.2, which only the second proxy refuses.
    #[tokio::test]
    async fn ffprobe_fetches_every_hop_through_the_proxy() {
        if std::env::var_os("ENGINE_E2E").is_none() {
            return;
        }
        let tool = |var: &str, name: &str| std::env::var(var).unwrap_or_else(|_| name.into());
        let (ffmpeg, ffprobe) = (
            tool("FFMPEG_PATH", "ffmpeg"),
            tool("FFPROBE_PATH", "ffprobe"),
        );
        let seg = std::process::Command::new(&ffmpeg)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=d=2:r=25",
                "-c:v",
                "mpeg2video",
            ])
            .args(["-f", "mpegts", "-"])
            .output()
            .unwrap()
            .stdout;
        let l = TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let playlist = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2,\nhttp://127.0.0.2:{port}/s.ts\n#EXT-X-ENDLIST\n"
        );
        tokio::spawn(async move {
            while let Ok((mut c, _)) = l.accept().await {
                let (seg, playlist) = (seg.clone(), playlist.clone());
                tokio::spawn(async move {
                    let mut b = [0u8; 2048];
                    let n = c.read(&mut b).await.unwrap_or(0);
                    let body = if String::from_utf8_lossy(&b[..n]).contains("s.ts") {
                        seg
                    } else {
                        playlist.into_bytes()
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = c.write_all(head.as_bytes()).await;
                    let _ = c.write_all(&body).await;
                });
            }
        });
        let url = format!("http://127.0.0.1:{port}/live.m3u8");
        let probe = |proxy: String| {
            let args = fliks_engine_core::live::probe_args(&url, &Default::default(), Some(&proxy));
            let ffprobe = ffprobe.clone();
            async move {
                tokio::process::Command::new(ffprobe)
                    .args(args)
                    .output()
                    .await
                    .unwrap()
            }
        };
        let open = VetProxy::start(|_| false).await.unwrap();
        let ok = probe(open.url().into()).await;
        assert!(
            ok.status.success(),
            "{}",
            String::from_utf8_lossy(&ok.stderr)
        );
        assert!(String::from_utf8_lossy(&ok.stdout).contains("mpeg2video"));
        let refusing = VetProxy::start(|ip| ip == IpAddr::from([127, 0, 0, 2]))
            .await
            .unwrap();
        assert!(!probe(refusing.url().into()).await.status.success());
    }
}
