//! Optional Jellyfin integration: LAN discovery, library list, scans.

use crate::util::{ApiError, R};
use serde_json::{json, Value};
use std::time::Duration;

fn auth_header(key: &str) -> String {
    let host: String = std::env::var("HOSTNAME").unwrap_or_else(|_| "mac".into()).chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    format!(r#"MediaBrowser Client="Dropfin", Device="Mac", DeviceId="dropfin-{host}", Version="{}", Token="{key}""#, crate::api::VERSION)
}

pub async fn fetch(url: Option<&str>, key: Option<&str>, path: &str, post: bool) -> R<Value> {
    let (url, key) = match (url.filter(|u| !u.is_empty()), key.filter(|k| !k.is_empty())) {
        (Some(u), Some(k)) => (u.trim_end_matches('/'), k),
        _ => return Err(ApiError::new(400, "Add your Jellyfin address and API key in Server Settings first.")),
    };
    let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(8)).build().map_err(|e| ApiError::new(500, e.to_string()))?;
    let full = format!("{url}{path}");
    let req = if post { client.post(&full).body("") } else { client.get(&full) };
    let r = req.header("Authorization", auth_header(key)).header("Accept", "application/json").send().await.map_err(|_| ApiError::new(502, format!("Can't reach Jellyfin at {url}.")))?;
    match r.status().as_u16() {
        401 | 403 => return Err(ApiError::new(400, "Jellyfin rejected the API key.")),
        s if !(200..300).contains(&s) => return Err(ApiError::new(502, format!("Jellyfin returned an error ({s})."))),
        _ => {}
    }
    let text = r.text().await.unwrap_or_default();
    Ok(if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::Null) })
}

/// Broadcast "Who is JellyfinServer?" on every IPv4 network and collect the answers.
pub async fn discover() -> Vec<Value> {
    let Ok(sock) = tokio::net::UdpSocket::bind("0.0.0.0:0").await else { return vec![] };
    let _ = sock.set_broadcast(true);
    let mut targets = vec!["255.255.255.255".to_string()];
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for i in ifs {
            if let if_addrs::IfAddr::V4(v4) = &i.addr {
                if !i.is_loopback() {
                    if let Some(b) = v4.broadcast {
                        targets.push(b.to_string());
                    }
                }
            }
        }
    }
    for t in &targets {
        let _ = sock.send_to(b"Who is JellyfinServer?", (t.as_str(), 7359)).await;
    }
    let mut found: Vec<Value> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    let mut buf = [0u8; 2048];
    while let Ok(Ok((n, from))) = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await {
        if let Ok(j) = serde_json::from_slice::<Value>(&buf[..n]) {
            let id = j["Id"].as_str().unwrap_or("").to_string();
            if found.iter().any(|f| f["id"] == id.as_str()) {
                continue;
            }
            let host = from.ip().to_string();
            let url = j["Address"].as_str().map(String::from).unwrap_or_else(|| format!("http://{host}:8096"));
            found.push(json!({ "id": id, "name": j["Name"].as_str().unwrap_or("Jellyfin"), "host": host, "url": url }));
        }
    }
    found
}
