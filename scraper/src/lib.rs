//! HTML scraper for the freenet-core dashboard at `/`.
//!
//! Targets the markup produced by
//! `freenet-core/crates/core/src/server/home_page.rs`. Stable hooks we rely on:
//!
//! - `tr.peer-row` rows with six `<td>` cells: address, location, type,
//!   bytes-sent, bytes-received, connected-since
//! - `span.own-loc` containing "Your location: 0.NNNN"
//! - `p.external-addr` containing "External address: <code>IP</code>" and
//!   "UDP port: <code>PORT</code>"
//! - `span.badge` containing the version string (e.g. "v0.1.148")
//!
//! When freenet-core gets a real JSON endpoint, this module is throw-away.

use anyhow::{Result, bail};
use scraper::{ElementRef, Html, Selector};
use shared::{ContractView, PeerView};

#[derive(Debug, Default)]
pub struct ParsedDashboard {
    pub own_location: Option<f64>,
    pub external_address: Option<String>,
    pub version: Option<String>,
    pub peers: Vec<PeerView>,
    pub contracts: Vec<ContractView>,
}

pub fn parse(html: &str) -> Result<ParsedDashboard> {
    let doc = Html::parse_document(html);

    let peer_row_sel =
        Selector::parse("tr.peer-row").map_err(|e| anyhow::anyhow!("bad selector: {e:?}"))?;
    let td_sel = Selector::parse("td").map_err(|e| anyhow::anyhow!("bad selector: {e:?}"))?;
    let code_sel = Selector::parse("code").map_err(|e| anyhow::anyhow!("bad selector: {e:?}"))?;
    let own_loc_sel =
        Selector::parse("span.own-loc").map_err(|e| anyhow::anyhow!("bad selector: {e:?}"))?;
    let ext_addr_sel = Selector::parse("p.external-addr")
        .map_err(|e| anyhow::anyhow!("bad selector: {e:?}"))?;
    let badge_sel =
        Selector::parse("span.badge").map_err(|e| anyhow::anyhow!("bad selector: {e:?}"))?;

    let mut out = ParsedDashboard::default();

    if let Some(badge) = doc.select(&badge_sel).next() {
        let txt = badge.text().collect::<String>();
        let trimmed = txt.trim().trim_start_matches('v').to_string();
        if !trimmed.is_empty() {
            out.version = Some(trimmed);
        }
    }

    if let Some(own) = doc.select(&own_loc_sel).next() {
        let txt = own.text().collect::<String>();
        out.own_location = extract_float_after(&txt, "Your location:");
    }

    if let Some(ext) = doc.select(&ext_addr_sel).next() {
        let codes: Vec<String> = ext
            .select(&code_sel)
            .map(|c| c.text().collect::<String>().trim().to_string())
            .collect();
        if codes.len() >= 2 {
            out.external_address = Some(format!("{}:{}", codes[0], codes[1]));
        } else if codes.len() == 1 && codes[0].contains(':') {
            out.external_address = Some(codes[0].clone());
        }
    }

    for row in doc.select(&peer_row_sel) {
        if let Some(peer) = parse_peer_row(row, &td_sel, &code_sel) {
            out.peers.push(peer);
        }
    }

    // Subscribed Contracts table. Rows are bare `<tr>` (no `peer-row` class)
    // with three cells: <td title=FULL_KEY><code>SHORT…</code></td><td>X
    // ago</td><td>Y ago | —</td>. We grab every such row in the document;
    // collisions with other unrelated tables aren't a concern because the
    // freenet dashboard only emits contracts in this exact shape (see
    // `home_page.rs::build_contracts_card`).
    let contract_row_sel =
        Selector::parse("tr").map_err(|e| anyhow::anyhow!("bad selector: {e:?}"))?;
    for row in doc.select(&contract_row_sel) {
        if row.value().attr("class").is_some() {
            continue; // peer-row and other tagged rows
        }
        if let Some(contract) = parse_contract_row(row, &td_sel, &code_sel) {
            out.contracts.push(contract);
        }
    }

    if out.peers.is_empty()
        && out.contracts.is_empty()
        && out.own_location.is_none()
        && out.version.is_none()
    {
        bail!("no recognizable freenet dashboard markup found");
    }

    Ok(out)
}

fn parse_contract_row(
    row: ElementRef<'_>,
    td_sel: &Selector,
    code_sel: &Selector,
) -> Option<ContractView> {
    let cells: Vec<ElementRef> = row.select(td_sel).collect();
    // Contract rows have exactly 3 cells; everything else (table headers,
    // op-stats rows, transfer-stats rows, etc.) has 1, 2, 4, or 6 — so a
    // length check is a clean discriminator.
    if cells.len() != 3 {
        return None;
    }
    let key = cells[0].value().attr("title")?.trim().to_string();
    if key.is_empty() {
        return None;
    }
    // The first cell wraps the short form in <code>; if it's missing, the
    // row is structurally wrong and we skip rather than emit garbage.
    let _short = cells[0]
        .select(code_sel)
        .next()
        .map(|c| c.text().collect::<String>())?;
    let subscribed_ago = Some(cells[1].text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty() && s != "—");
    let last_update_ago = Some(cells[2].text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty() && s != "—");

    Some(ContractView {
        key,
        subscribed_ago,
        last_update_ago,
    })
}

fn parse_peer_row(
    row: ElementRef<'_>,
    td_sel: &Selector,
    code_sel: &Selector,
) -> Option<PeerView> {
    let cells: Vec<ElementRef> = row.select(td_sel).collect();
    if cells.is_empty() {
        return None;
    }

    let address = cells
        .first()
        .and_then(|c| c.select(code_sel).next())
        .map(|c| c.text().collect::<String>().trim().to_string())
        .or_else(|| {
            cells
                .first()
                .map(|c| c.text().collect::<String>().trim().to_string())
        })?;
    if address.is_empty() {
        return None;
    }

    let location = cells
        .get(1)
        .and_then(|c| c.text().collect::<String>().trim().parse::<f64>().ok());

    let ptype_text = cells
        .get(2)
        .map(|c| c.text().collect::<String>().to_lowercase())
        .unwrap_or_default();
    let is_gateway = ptype_text.contains("gateway");

    let connected = cells.get(5).map(|c| c.text().collect::<String>().trim().to_string());

    Some(PeerView {
        address,
        is_gateway,
        location,
        connected: connected.filter(|s| !s.is_empty()),
    })
}

fn extract_float_after(text: &str, marker: &str) -> Option<f64> {
    let idx = text.find(marker)?;
    let tail = &text[idx + marker.len()..];
    let mut chars = tail.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
    let num: String = chars
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    num.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(peers_html: &str, extras: &str) -> String {
        format!(
            r#"<!DOCTYPE html><html><body>
            <span class="badge">v0.1.148</span>
            {extras}
            <table><tbody>{peers_html}</tbody></table>
            </body></html>"#
        )
    }

    #[test]
    fn parses_own_location_and_external_addr() {
        let html = fixture(
            "",
            r#"<span class="own-loc">Your location: 0.4321</span>
               <p class="external-addr">External address: <code>1.2.3.4</code> &mdash; UDP port: <code>31337</code></p>"#,
        );
        let parsed = parse(&html).unwrap();
        assert_eq!(parsed.own_location, Some(0.4321));
        assert_eq!(parsed.external_address.as_deref(), Some("1.2.3.4:31337"));
        assert_eq!(parsed.version.as_deref(), Some("0.1.148"));
    }

    #[test]
    fn parses_peer_rows_with_gateway_flag() {
        let html = fixture(
            r#"<tr class="peer-row" onclick="x"><td><code>10.0.0.1:31337</code></td><td>0.5</td><td>Gateway</td><td>1KB</td><td>2KB</td><td>1m 12s</td></tr>
               <tr class="peer-row" onclick="x"><td><code>192.168.1.10:50000</code></td><td>0.12</td><td>Peer</td><td>0</td><td>0</td><td>5s</td></tr>"#,
            r#"<span class="own-loc">Your location: 0.5</span>"#,
        );
        let parsed = parse(&html).unwrap();
        assert_eq!(parsed.peers.len(), 2);
        assert_eq!(parsed.peers[0].address, "10.0.0.1:31337");
        assert!(parsed.peers[0].is_gateway);
        assert_eq!(parsed.peers[0].location, Some(0.5));
        assert_eq!(parsed.peers[0].connected.as_deref(), Some("1m 12s"));
        assert!(!parsed.peers[1].is_gateway);
        assert_eq!(parsed.peers[1].location, Some(0.12));
    }

    #[test]
    fn rejects_unrecognized_markup() {
        assert!(parse("<html><body>not a freenet dashboard</body></html>").is_err());
    }

    #[test]
    fn parses_subscribed_contracts_table() {
        // Mirrors the markup emitted by `home_page.rs::build_contracts_card`.
        // First row has a real "last update" string, second has the "—"
        // placeholder, which we want to surface as None.
        let html = fixture(
            "",
            r##"<div class="card">
                <h2>Subscribed Contracts</h2>
                <div class="table-wrap"><table>
                <thead><tr><th>Contract</th><th>Subscribed</th><th>Last Update</th></tr></thead>
                <tbody>
                  <tr><td title="7xGaMzeYJZuSg6JJHBjtyJcnFULL"><code>7xGaMzeYJZuSg6JJ…</code></td><td>46m 35s ago</td><td>just now</td></tr>
                  <tr><td title="SgWnZQeN7KiErxv7GAHXBypvFULL"><code>SgWnZQeN7KiErxv7…</code></td><td>45m 55s ago</td><td>—</td></tr>
                </tbody></table></div></div>"##,
        );
        let parsed = parse(&html).unwrap();
        assert_eq!(parsed.contracts.len(), 2);
        assert_eq!(parsed.contracts[0].key, "7xGaMzeYJZuSg6JJHBjtyJcnFULL");
        assert_eq!(parsed.contracts[0].subscribed_ago.as_deref(), Some("46m 35s ago"));
        assert_eq!(parsed.contracts[0].last_update_ago.as_deref(), Some("just now"));
        assert_eq!(parsed.contracts[1].key, "SgWnZQeN7KiErxv7GAHXBypvFULL");
        assert_eq!(parsed.contracts[1].subscribed_ago.as_deref(), Some("45m 55s ago"));
        // "—" placeholder should normalize to None
        assert_eq!(parsed.contracts[1].last_update_ago, None);
    }

    #[test]
    fn ignores_peer_rows_when_parsing_contracts() {
        // A peer-row table and a contracts table living side by side. The
        // length-3 vs length-6 cell discriminator must keep them separate.
        let html = fixture(
            r#"<tr class="peer-row" onclick="x"><td><code>1.1.1.1:1</code></td><td>0.5</td><td>Peer</td><td>0</td><td>0</td><td>1s</td></tr>"#,
            r#"<table><tbody><tr><td title="K1"><code>K1…</code></td><td>1m ago</td><td>30s ago</td></tr></tbody></table>"#,
        );
        let parsed = parse(&html).unwrap();
        assert_eq!(parsed.peers.len(), 1);
        assert_eq!(parsed.contracts.len(), 1);
        assert_eq!(parsed.contracts[0].key, "K1");
    }
}
