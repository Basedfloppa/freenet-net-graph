//! Local HTTP probe for `/v1/contract/web/<key>/`.
//!
//! Browser dashboards can't ask the local node "is this contract a
//! webapp?" because (a) the iframe origin is `null` and CORS blocks the
//! cross-origin fetch, and (b) the freenet node doesn't advertise it as
//! part of `NodeDiagnostics`. The daemon, running natively, has neither
//! restriction — a plain HTTP GET to the local node decides it.
//!
//! 200 OK → webapp; we additionally try to lift a `<title>` from the
//! body so subscribers see a friendly name. Anything else (404, network
//! error, timeout) marks the contract `data-only`.
//!
//! Probe results are cached for the lifetime of the process so the
//! per-cycle cost stays bounded at "new contracts since last cycle"
//! rather than "all contracts every minute". Restart the daemon to
//! refresh classifications (e.g. after a webapp redeploy).
//!
//! Concurrency is capped so we don't pin the local node's HTTP server
//! when first encountering hundreds of contracts.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use futures::stream::{self, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tracing::{debug, trace};

/// What we learned about one contract.
#[derive(Debug, Clone, Default)]
pub struct ProbeResult {
    pub is_webapp: Option<bool>,
    pub title: Option<String>,
}

/// One cache entry: result plus the wall-clock time of the probe so we
/// can re-probe stale entries.
#[derive(Debug, Clone)]
struct CacheEntry {
    result: ProbeResult,
    probed_at: Instant,
}

/// In-memory cache of probe results keyed by base58 contract key.
#[derive(Debug, Default)]
pub struct ProbeCache {
    inner: HashMap<String, CacheEntry>,
}

impl ProbeCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn webapp_count(&self) -> usize {
        self.inner
            .values()
            .filter(|e| e.result.is_webapp == Some(true))
            .count()
    }

    pub fn get(&self, key: &str) -> Option<ProbeResult> {
        self.inner.get(key).map(|e| e.result.clone())
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_CONCURRENCY: usize = 16;
const MAX_BODY_BYTES: usize = 64 * 1024;
/// Cache entries older than this are eligible for re-probing once per
/// cycle (so a webapp redeploy is picked up without a daemon restart).
/// 30 minutes balances "freshness" against "don't hammer the local
/// HTTP server" — at 620 contracts that's ~21 re-probes per cycle if
/// every entry happens to be exactly past TTL.
const PROBE_TTL: Duration = Duration::from_secs(30 * 60);
/// Soft cap on stale re-probes per cycle — even if half the cache aged
/// out at once, we don't want to issue hundreds of HTTP requests in a
/// single tick. The oldest entries win the budget.
const MAX_STALE_REPROBES_PER_CYCLE: usize = 64;

/// Update `cache` with probe results. Two passes:
///
/// 1. **New keys** (not in cache) — always probed; the daemon sees a
///    contract for the first time.
/// 2. **Stale keys** (probed > [`PROBE_TTL`] ago, still present in
///    `keys`) — re-probed up to [`MAX_STALE_REPROBES_PER_CYCLE`],
///    oldest first. Lets a webapp redeploy/title-change reflect in
///    the dashboard without a daemon restart.
///
/// Probes run with bounded concurrency. Transport errors do *not*
/// poison the cache: a transient blip is not cached, and a stale
/// entry whose re-probe failed retains the old data.
pub async fn refresh_cache(host: &str, port: u16, keys: &[String], cache: &mut ProbeCache) {
    let now = Instant::now();
    let mut new_keys: Vec<String> = Vec::new();
    // (key, age) for sorting stale keys by oldest first
    let mut stale: Vec<(String, Duration)> = Vec::new();
    for k in keys {
        match cache.inner.get(k) {
            None => new_keys.push(k.clone()),
            Some(entry) => {
                let age = now.saturating_duration_since(entry.probed_at);
                if age >= PROBE_TTL {
                    stale.push((k.clone(), age));
                }
            }
        }
    }
    // Oldest first, capped — this turns the cache into an LRU-by-age
    // for re-probes, so the budget is spent on the entries most
    // likely to be stale in reality.
    stale.sort_by(|a, b| b.1.cmp(&a.1));
    stale.truncate(MAX_STALE_REPROBES_PER_CYCLE);

    let mut to_probe: Vec<String> = Vec::with_capacity(new_keys.len() + stale.len());
    to_probe.extend(new_keys.iter().cloned());
    to_probe.extend(stale.iter().map(|(k, _)| k.clone()));
    if to_probe.is_empty() {
        return;
    }
    debug!(
        new = new_keys.len(),
        stale = stale.len(),
        cached = cache.len(),
        "probing local /v1/contract/web/<key>/?__sandbox=1"
    );

    let host = host.to_string();
    let results = stream::iter(to_probe)
        .map(|key| {
            let host = host.clone();
            async move {
                let outcome = probe_one(&host, port, &key).await;
                (key, outcome)
            }
        })
        .buffer_unordered(PROBE_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    for (key, outcome) in results {
        if let Some(r) = outcome {
            cache.inner.insert(
                key,
                CacheEntry {
                    result: r,
                    probed_at: Instant::now(),
                },
            );
        }
    }
}

/// Returns `Some(ProbeResult)` on a definitive answer (200 webapp / non-200
/// data-only); `None` on transport errors so the caller can retry later
/// without poisoning the cache.
async fn probe_one(host: &str, port: u16, key: &str) -> Option<ProbeResult> {
    // `?__sandbox=1` is the route that returns the *contract's own* HTML
    // (the bytes the webapp ships). Without that param, freenet wraps
    // every webapp in a generic outer shell whose `<title>` is always
    // literally "Freenet" — useless for distinguishing contracts. The
    // sandboxed route is what the dashboard's iframe loads, so its
    // title is what subscribers actually see in their browser tab.
    let path = format!("/v1/contract/web/{key}/?__sandbox=1");
    match http_get(host, port, &path).await {
        Ok((200, body)) => Some(ProbeResult {
            is_webapp: Some(true),
            title: extract_title(&body),
        }),
        Ok((status, _)) => {
            trace!(key, status, "non-200 probe → data-only");
            Some(ProbeResult {
                is_webapp: Some(false),
                title: None,
            })
        }
        Err(e) => {
            trace!(key, error = %e, "probe failed (transport); will retry");
            None
        }
    }
}

/// Bare HTTP/1.1 GET over plain TCP. We bypass `reqwest`/`hyper` because
/// the daemon already pulls a heavyweight WS stack and we only need this
/// for localhost — no TLS, no redirects, no compression.
async fn http_get(host: &str, port: u16, path: &str) -> Result<(u16, Vec<u8>), String> {
    let addr = format!("{host}:{port}");
    let connect = TcpStream::connect(&addr);
    let mut stream = tokio::time::timeout(PROBE_TIMEOUT, connect)
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| format!("connect: {e}"))?;

    let req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         User-Agent: topology-publisher\r\n\
         Accept: text/html\r\n\
         Connection: close\r\n\r\n"
    );
    tokio::time::timeout(PROBE_TIMEOUT, stream.write_all(req.as_bytes()))
        .await
        .map_err(|_| "write timeout".to_string())?
        .map_err(|e| format!("write: {e}"))?;

    let mut buf = Vec::with_capacity(4096);
    let read = async {
        let mut chunk = [0u8; 4096];
        while buf.len() < MAX_BODY_BYTES {
            match stream.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) => return Err(format!("read: {e}")),
            }
        }
        Ok(())
    };
    tokio::time::timeout(PROBE_TIMEOUT, read)
        .await
        .map_err(|_| "read timeout".to_string())??;

    let status = parse_status(&buf).ok_or_else(|| "no HTTP status line".to_string())?;
    let body_start = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(buf.len());
    let body = buf[body_start..].to_vec();
    Ok((status, body))
}

fn parse_status(buf: &[u8]) -> Option<u16> {
    let end = buf.windows(2).position(|w| w == b"\r\n")?;
    let line = std::str::from_utf8(&buf[..end]).ok()?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Tolerant `<title>` extractor. We don't decode chunked-transfer or
/// honor `Content-Type` — for our purposes the body is "some bytes that
/// might contain a title tag", and a simple case-insensitive scan
/// recovers it whether the body is gzipped (it isn't, we don't accept
/// `Content-Encoding`), chunked, or plain. Length-capped so a runaway
/// title can't blow a payload.
fn extract_title(body: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?;
    let lower = s.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let after_open = open + s[open..].find('>')? + 1;
    let rest = &lower[after_open..];
    let close = rest.find("</title>")?;
    let title = s[after_open..after_open + close].trim();
    if title.is_empty() {
        return None;
    }
    let mut t = title.to_string();
    if t.chars().count() > 80 {
        let truncated: String = t.chars().take(80).collect();
        t = format!("{truncated}…");
    }
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_extraction() {
        let body = b"<html><head><title>Net-Graph Dashboard</title></head></html>";
        assert_eq!(extract_title(body).as_deref(), Some("Net-Graph Dashboard"));

        // case-insensitive open tag with attributes
        let body = b"<HTML><HEAD><Title lang=\"en\"> Hello </Title></HEAD>";
        assert_eq!(extract_title(body).as_deref(), Some("Hello"));

        // missing title
        let body = b"<html><body>no title here</body></html>";
        assert_eq!(extract_title(body), None);

        // empty title trimmed
        let body = b"<title>   </title>";
        assert_eq!(extract_title(body), None);
    }

    #[test]
    fn title_truncated_at_80_chars() {
        let long: String = "x".repeat(120);
        let body = format!("<title>{long}</title>");
        let t = extract_title(body.as_bytes()).unwrap();
        assert_eq!(t.chars().count(), 81); // 80 chars + ellipsis
        assert!(t.ends_with('…'));
    }

    #[test]
    fn parse_status_line() {
        assert_eq!(parse_status(b"HTTP/1.1 200 OK\r\nfoo"), Some(200));
        assert_eq!(parse_status(b"HTTP/1.1 404 Not Found\r\n"), Some(404));
        assert_eq!(parse_status(b""), None);
    }
}
