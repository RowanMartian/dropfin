//! Local HTTP API — the same endpoints the web UI used with the Node helper.

use crate::jellyfin;
use crate::ssh::{self, is_code, Conn};
use crate::util::*;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Baked in by the GitHub build (see .github/workflows); absent in local builds, which disables update checks.
pub const BUILD_REPO: Option<&str> = option_env!("DROPFIN_REPO");
pub const BUILD_COMMIT: Option<&str> = option_env!("DROPFIN_COMMIT");
const IN_FLIGHT: usize = 32;

const INDEX_HTML: &str = include_str!("../web/index.html");
const ICON_SVG: &str = include_str!("../web/icon.svg");
const ICON_PNG: &[u8] = include_bytes!("../web/icon-180.png");
const MANIFEST: &str = include_str!("../web/manifest.webmanifest");
const FA_CSS: &str = include_str!("../web/fa/fontawesome.css");
const FA_FONT: &[u8] = include_bytes!("../web/fa/fa-solid-900.woff2");

pub struct Job {
    pub id: u64,
    pub name: String,
    pub total: u64,
    pub done: u64,
    pub status: &'static str,
    pub error: Option<String>,
    pub local_path: Option<PathBuf>,
    pub cancelled: bool,
    samples: VecDeque<(u64, u64)>,
}

pub struct App {
    pub token: String,
    pub cfg: Mutex<Config>,
    pub cfg_file: PathBuf,
    pub data_dir: PathBuf,
    pub downloads: PathBuf,
    pub conn: tokio::sync::RwLock<Option<Arc<Conn>>>,
    reconnect: tokio::sync::Mutex<()>,
    pub jobs: Mutex<BTreeMap<u64, Arc<Mutex<Job>>>>,
    job_seq: AtomicU64,
    pub uploads_active: AtomicUsize,
    pub last_upload_ms: AtomicU64,
    pub quit: Mutex<Option<Box<dyn Fn() + Send>>>,
    update: Mutex<(u64, Option<Value>)>,
}

impl App {
    pub fn new() -> Arc<App> {
        let data_dir = data_dir();
        let _ = std::fs::create_dir_all(&data_dir);
        let cfg_file = data_dir.join("config.json");
        let token: String = {
            use rand::Rng;
            let mut r = rand::rng();
            (0..24).map(|_| format!("{:02x}", r.random::<u8>())).collect()
        };
        Arc::new(App {
            token,
            cfg: Mutex::new(Config::load(&cfg_file)),
            cfg_file,
            data_dir,
            downloads: downloads_dir(),
            conn: tokio::sync::RwLock::new(None),
            reconnect: tokio::sync::Mutex::new(()),
            jobs: Mutex::new(BTreeMap::new()),
            job_seq: AtomicU64::new(0),
            uploads_active: AtomicUsize::new(0),
            last_upload_ms: AtomicU64::new(0),
            quit: Mutex::new(None),
            update: Mutex::new((0, None)),
        })
    }

    /// True while anything is being transferred (used to confirm before quitting).
    pub fn busy(&self) -> bool {
        let uploading = self.uploads_active.load(Ordering::SeqCst) > 0 || now_ms().saturating_sub(self.last_upload_ms.load(Ordering::SeqCst)) < 4000;
        let downloading = self.jobs.lock().unwrap().values().any(|j| matches!(j.lock().unwrap().status, "running" | "preparing"));
        uploading || downloading
    }

    fn save(&self) {
        self.cfg.lock().unwrap().save(&self.cfg_file);
    }
    fn server(&self, id: &str) -> Option<Server> {
        self.cfg.lock().unwrap().server(id).cloned()
    }

    async fn connect(&self, server_id: &str, password: Option<String>) -> R<(String, bool, String)> {
        let server = self.server(server_id).ok_or_else(|| ApiError::new(404, "Unknown server"))?;
        let opened = ssh::open(&server, password).await?;
        let home = opened.conn.home.clone();
        let first = server.host_key.is_none();
        {
            let mut cfg = self.cfg.lock().unwrap();
            if let Some(s) = cfg.server_mut(server_id) {
                if s.host_key.is_none() {
                    s.host_key = Some(opened.fingerprint.clone());
                }
            }
            cfg.last_server = Some(server_id.to_string());
        }
        self.save();
        let old = self.conn.write().await.replace(Arc::new(opened.conn));
        if let Some(old) = old {
            let _ = old.handle.disconnect(russh::Disconnect::ByApplication, "", "en").await;
        }
        Ok((home, first, opened.fingerprint))
    }

    /// The live connection, reconnecting transparently if it dropped.
    pub async fn conn(&self) -> R<Arc<Conn>> {
        match self.conn.read().await.clone() {
            None => return Err(ApiError::new(401, "Not connected").reason("disconnected")),
            Some(c) if c.alive() => return Ok(c),
            _ => {}
        }
        let _guard = self.reconnect.lock().await;
        let current = self.conn.read().await.clone();
        let c = match current {
            None => return Err(ApiError::new(401, "Not connected").reason("disconnected")),
            Some(c) if c.alive() => return Ok(c),
            Some(c) => c,
        };
        let mut last = None;
        for i in 0..3u64 {
            match self.connect(&c.server_id, c.password.clone()).await {
                Ok(_) => return Ok(self.conn.read().await.clone().unwrap()),
                Err(e) => {
                    let fatal = e.status == 401 || e.status == 409;
                    last = Some(e);
                    if fatal {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(800 * (i + 1))).await;
                }
            }
        }
        Err(last.unwrap())
    }
}

// ------------------------------------------------------------------ keychain

async fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new(cmd).args(args).output().await.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

async fn secret_get(app: &App, s: &Server) -> Option<String> {
    if cfg!(target_os = "macos") {
        run("/usr/bin/security", &["find-generic-password", "-a", &s.account(), "-s", "Dropfin", "-w"]).await.ok().map(|x| x.trim_end_matches('\n').to_string())
    } else {
        app.server(&s.id).and_then(|x| x.password_fallback)
    }
}
async fn secret_set(app: &App, s: &Server, pw: &str) {
    if cfg!(target_os = "macos") {
        let label = format!("Dropfin ({})", s.host);
        let _ = run("/usr/bin/security", &["add-generic-password", "-U", "-a", &s.account(), "-s", "Dropfin", "-l", &label, "-w", pw]).await;
    } else {
        if let Some(x) = app.cfg.lock().unwrap().server_mut(&s.id) {
            x.password_fallback = Some(pw.to_string());
        }
        app.save();
    }
}
async fn secret_remove(app: &App, s: &Server) {
    if cfg!(target_os = "macos") {
        let _ = run("/usr/bin/security", &["delete-generic-password", "-a", &s.account(), "-s", "Dropfin"]).await;
    } else {
        if let Some(x) = app.cfg.lock().unwrap().server_mut(&s.id) {
            x.password_fallback = None;
        }
        app.save();
    }
}

// ------------------------------------------------------------------ dispatcher

pub fn router(app: Arc<App>) -> axum::Router {
    axum::Router::new().fallback(handle).with_state(app)
}

async fn handle(State(app): State<Arc<App>>, method: Method, uri: Uri, headers: HeaderMap, body: Body) -> Response {
    match dispatch(&app, method, uri, headers, body).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

fn query(uri: &Uri) -> HashMap<String, String> {
    axum::extract::Query::<HashMap<String, String>>::try_from_uri(uri).map(|q| q.0).unwrap_or_default()
}

async fn json_body(body: Body) -> R<Value> {
    let bytes = axum::body::to_bytes(body, 2_000_000).await.map_err(|_| ApiError::new(413, "Too large"))?;
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes).map_err(|_| ApiError::new(400, "Invalid JSON"))
}

async fn dispatch(app: &Arc<App>, method: Method, uri: Uri, headers: HeaderMap, body: Body) -> R<Response> {
    // DNS-rebinding guard: only answer to localhost names.
    let host = headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or("");
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    if !["localhost", "127.0.0.1", "[::1]"].contains(&host) {
        return Err(ApiError::new(403, "Forbidden"));
    }
    let path = uri.path().to_string();
    if path == "/api/hello" {
        return Ok(ok(json!({ "app": "dropfin", "version": VERSION })));
    }
    if !path.starts_with("/api/") {
        return static_file(app, &path);
    }
    let q = query(&uri);
    let token = headers.get("x-dropfin-token").and_then(|h| h.to_str().ok()).map(String::from).or_else(|| q.get("t").cloned());
    if token.as_deref() != Some(app.token.as_str()) {
        return Err(ApiError::new(403, "Dropfin restarted — reload the page.").reason("token"));
    }

    if method == Method::PUT && path == "/api/upload" {
        return upload(app, &q, &headers, body).await;
    }
    if (method == Method::GET || method == Method::HEAD) && path == "/api/stream" {
        return stream(app, &q, &headers, method == Method::HEAD).await;
    }
    let b = if method == Method::POST { json_body(body).await? } else { json!({}) };
    let v = match (method.as_str(), path.as_str()) {
        ("GET", "/api/state") => state(app).await,
        ("GET", "/api/discover") => Ok(json!({ "servers": jellyfin::discover().await })),
        ("POST", "/api/prefs") => {
            if let Value::Object(m) = b {
                app.cfg.lock().unwrap().prefs.extend(m);
            }
            app.save();
            Ok(json!({ "ok": true }))
        }
        ("POST", "/api/servers/save") => save_server(app, &b).await,
        ("POST", "/api/servers/delete") => delete_server(app, &b).await,
        ("POST", "/api/servers/trust") => {
            if let Some(s) = app.cfg.lock().unwrap().server_mut(&s(&b, "id")) {
                s.host_key = None;
            }
            app.save();
            Ok(json!({ "ok": true }))
        }
        ("POST", "/api/connect") => connect(app, &b).await,
        ("POST", "/api/disconnect") => {
            if let Some(c) = app.conn.write().await.take() {
                let _ = c.handle.disconnect(russh::Disconnect::ByApplication, "", "en").await;
            }
            Ok(json!({ "ok": true }))
        }
        ("GET", "/api/sidebar") => sidebar(app).await,
        ("POST", "/api/pin") => pin(app, &b).await,
        ("GET", "/api/list") => list(app, &q).await,
        ("GET", "/api/info") => info(app, &q).await,
        ("POST", "/api/mkdir") => mkdir(app, &b).await,
        ("POST", "/api/rename") => rename(app, &b).await,
        ("POST", "/api/delete") => delete(app, &b).await,
        ("POST", "/api/transfer") => transfer(app, &b).await,
        ("GET", "/api/upload/status") => upload_status(app, &q).await,
        ("POST", "/api/upload/cancel") => {
            let c = app.conn().await?;
            let (_, partial) = upload_target(&s(&b, "dir"), &s(&b, "name"))?;
            let _ = c.sftp.remove(partial.as_str()).await;
            Ok(json!({ "ok": true }))
        }
        ("POST", "/api/download") => start_download(app, &b).await,
        ("GET", "/api/jobs") => Ok(json!({ "jobs": app.jobs.lock().unwrap().values().map(|j| job_view(&mut j.lock().unwrap())).collect::<Vec<_>>() })),
        ("POST", "/api/jobs/cancel") => {
            let id: u64 = s(&b, "id").parse().unwrap_or(0);
            if let Some(j) = app.jobs.lock().unwrap().get(&id) {
                j.lock().unwrap().cancelled = true;
            }
            Ok(json!({ "ok": true }))
        }
        ("POST", "/api/jobs/clear") => {
            app.jobs.lock().unwrap().retain(|_, j| !matches!(j.lock().unwrap().status, "done" | "error" | "cancelled"));
            Ok(json!({ "ok": true }))
        }
        ("POST", "/api/reveal") => reveal(app, &b),
        ("POST", "/api/scan") => {
            let c = app.conn().await?;
            let server = app.server(&c.server_id).ok_or_else(|| ApiError::new(404, "Unknown server"))?;
            jellyfin::fetch(server.jellyfin_url.as_deref(), server.jellyfin_key.as_deref(), "/Library/Refresh", true).await?;
            Ok(json!({ "ok": true }))
        }
        ("POST", "/api/jellyfin/test") => {
            let mut key = s(&b, "key");
            if key == "********" {
                key = app.server(&s(&b, "id")).and_then(|x| x.jellyfin_key).unwrap_or_default();
            }
            let url = s(&b, "url").trim().trim_end_matches('/').to_string();
            let info = jellyfin::fetch(Some(&url), Some(&key), "/System/Info", false).await?;
            let libs = jellyfin::fetch(Some(&url), Some(&key), "/Library/VirtualFolders", false).await?;
            let names: Vec<Value> = libs.as_array().map(|a| a.iter().map(|l| l["Name"].clone()).collect()).unwrap_or_default();
            Ok(json!({ "name": info["ServerName"], "version": info["Version"], "libraries": names }))
        }
        ("GET", "/api/update") => check_update(app, q.get("force").map(|x| x == "1").unwrap_or(false)).await,
        ("POST", "/api/update/open") => {
            let url = app.update.lock().unwrap().1.as_ref().and_then(|v| v["url"].as_str().map(String::from));
            match url {
                Some(u) if u.starts_with("https://github.com/") => {
                    if cfg!(target_os = "macos") {
                        let _ = std::process::Command::new("/usr/bin/open").arg(&u).spawn();
                    }
                    Ok(json!({ "ok": true, "url": u }))
                }
                _ => Err(ApiError::new(404, "No update to open")),
            }
        }
        ("POST", "/api/ping") => Ok(json!({ "ok": true })),
        ("POST", "/api/quit") => {
            let app2 = app.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                match app2.quit.lock().unwrap().as_ref() {
                    Some(f) => f(),
                    None => std::process::exit(0),
                }
            });
            Ok(json!({ "ok": true }))
        }
        _ => Err(ApiError::new(404, "Not found")),
    }?;
    Ok(ok(v))
}

fn static_file(app: &App, path: &str) -> R<Response> {
    let (ctype, body): (&str, Body) = match path {
        "/" | "/index.html" => {
            let html = INDEX_HTML.replace("__DROPFIN_TOKEN__", &app.token);
            return Ok(([("content-type", "text/html; charset=utf-8"), ("cache-control", "no-store"), ("x-frame-options", "DENY"), ("referrer-policy", "no-referrer")], html).into_response());
        }
        "/icon.svg" => ("image/svg+xml", Body::from(ICON_SVG)),
        "/icon-180.png" => ("image/png", Body::from(ICON_PNG)),
        "/manifest.webmanifest" => ("application/manifest+json", Body::from(MANIFEST)),
        "/fa/fontawesome.css" => ("text/css; charset=utf-8", Body::from(FA_CSS)),
        "/fa/fa-solid-900.woff2" => {
            return Ok(([("content-type", "font/woff2"), ("cache-control", "max-age=31536000, immutable")], Body::from(FA_FONT)).into_response());
        }
        _ => return Err(ApiError::new(404, "Not found")),
    };
    Ok(([("content-type", ctype), ("cache-control", "no-cache")], body).into_response())
}

// ------------------------------------------------------------------ update check

/// Compares the commit this app was built from with the commit behind the repo's "latest" release.
/// One small HTTPS request to GitHub, at most every few hours. Quietly does nothing for private repos or offline Macs.
async fn check_update(app: &App, force: bool) -> R<Value> {
    let (Some(repo), Some(commit)) = (BUILD_REPO.filter(|r| !r.is_empty()), BUILD_COMMIT.filter(|c| c.len() >= 7)) else {
        return Ok(json!({ "enabled": false }));
    };
    {
        let cached = app.update.lock().unwrap();
        if !force && now_ms().saturating_sub(cached.0) < 6 * 3600 * 1000 {
            if let Some(v) = &cached.1 {
                return Ok(v.clone());
            }
        }
    }
    let url = std::env::var("DROPFIN_UPDATE_URL").unwrap_or_else(|_| format!("https://api.github.com/repos/{repo}/releases/tags/latest"));
    let current = &commit[..7];
    let result = async {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(8)).user_agent(format!("Dropfin/{VERSION}")).build().ok()?;
        let r = client.get(&url).header("Accept", "application/vnd.github+json").send().await.ok()?;
        if !r.status().is_success() {
            return None;
        }
        r.json::<Value>().await.ok()
    }
    .await;
    let v = match result {
        Some(rel) => {
            let latest = rel["target_commitish"].as_str().unwrap_or("").to_string();
            let is_sha = latest.len() >= 7 && latest.chars().all(|c| c.is_ascii_hexdigit());
            let available = is_sha && !latest.starts_with(current) && !commit.starts_with(&latest);
            json!({ "enabled": true, "available": available, "reachable": true, "current": current, "latest": if is_sha { &latest[..7] } else { "" },
                    "url": rel["html_url"], "published": rel["published_at"] })
        }
        None => json!({ "enabled": true, "available": false, "reachable": false, "current": current }),
    };
    *app.update.lock().unwrap() = (now_ms(), Some(v.clone()));
    Ok(v)
}

// ------------------------------------------------------------------ servers & session

async fn state(app: &App) -> R<Value> {
    let conn = app.conn.read().await.clone();
    let cfg = app.cfg.lock().unwrap().clone();
    let downloads = app.downloads.to_string_lossy().replace(&home_dir().to_string_lossy().to_string(), "~");
    Ok(json!({
        "servers": cfg.servers.iter().map(|s| s.public()).collect::<Vec<_>>(),
        "lastServer": cfg.last_server, "prefs": cfg.prefs,
        "connected": conn.map(|c| json!({ "id": c.server_id, "home": c.home, "alive": c.alive() })),
        "downloads": downloads, "mac": cfg!(target_os = "macos"), "version": VERSION,
    }))
}

async fn save_server(app: &App, b: &Value) -> R<Value> {
    let host = s(b, "host").trim().to_string();
    let username = s(b, "username").trim().to_string();
    if host.is_empty() {
        return Err(ApiError::new(400, "Enter the server address."));
    }
    if username.is_empty() {
        return Err(ApiError::new(400, "Enter your SSH username."));
    }
    let port: u16 = s(b, "port").parse().ok().filter(|p| *p > 0).unwrap_or(22);
    let id = s(b, "id");
    let existing = if id.is_empty() { None } else { app.server(&id) };
    let id = match existing {
        Some(old) => {
            let moved = old.host != host || old.port != port;
            if moved || old.username != username {
                let pw = secret_get(app, &old).await;
                secret_remove(app, &old).await;
                let mut updated = old.clone();
                updated.host = host.clone();
                updated.port = port;
                updated.username = username.clone();
                if let Some(x) = app.cfg.lock().unwrap().server_mut(&old.id) {
                    x.host = host.clone();
                    x.port = port;
                    x.username = username.clone();
                    if moved {
                        x.host_key = None;
                    }
                }
                if let Some(pw) = pw {
                    secret_set(app, &updated, &pw).await;
                }
            }
            old.id
        }
        None => {
            use rand::Rng;
            let new_id: String = (0..6).map(|_| format!("{:02x}", rand::rng().random::<u8>())).collect();
            app.cfg.lock().unwrap().servers.push(Server { id: new_id.clone(), host: host.clone(), port, username, ..Default::default() });
            new_id
        }
    };
    let public = {
        let mut cfg = app.cfg.lock().unwrap();
        let x = cfg.server_mut(&id).unwrap();
        let name = s(b, "name").trim().to_string();
        x.name = if name.is_empty() { host } else { name };
        if b.get("jellyfinUrl").is_some() {
            x.jellyfin_url = opt_s(b, "jellyfinUrl").map(|u| u.trim().trim_end_matches('/').to_string());
        }
        if b.get("jellyfinKey").is_some() && s(b, "jellyfinKey") != "********" {
            x.jellyfin_key = opt_s(b, "jellyfinKey").map(|k| k.trim().to_string());
        }
        if b.get("autoScan").is_some() {
            x.auto_scan = Some(crate::util::b(b, "autoScan"));
        }
        x.public()
    };
    app.save();
    Ok(json!({ "server": public }))
}

async fn delete_server(app: &App, b: &Value) -> R<Value> {
    let id = s(b, "id");
    if let Some(server) = app.server(&id) {
        secret_remove(app, &server).await;
        app.cfg.lock().unwrap().servers.retain(|x| x.id != id);
        let mut conn = app.conn.write().await;
        if conn.as_ref().is_some_and(|c| c.server_id == id) {
            *conn = None;
        }
        app.save();
    }
    Ok(json!({ "ok": true }))
}

async fn connect(app: &App, b: &Value) -> R<Value> {
    let id = s(b, "id");
    let server = app.server(&id).ok_or_else(|| ApiError::new(404, "Unknown server"))?;
    let given = opt_s(b, "password");
    let password = match &given {
        Some(p) => Some(p.clone()),
        None => secret_get(app, &server).await,
    };
    let (home, first, fingerprint) = app.connect(&id, password).await?;
    if let Some(pw) = given {
        let remember = b.get("remember").map(|x| x.as_bool().unwrap_or(true)).unwrap_or(true);
        if remember {
            secret_set(app, &server, &pw).await;
        } else {
            secret_remove(app, &server).await;
        }
        if let Some(x) = app.cfg.lock().unwrap().server_mut(&id) {
            x.saved_password = Some(remember);
        }
        app.save();
    }
    let public = app.server(&id).map(|x| x.public()).unwrap_or(json!({}));
    Ok(json!({ "home": home, "firstTime": first, "fingerprint": fingerprint, "server": public }))
}

async fn sidebar(app: &App) -> R<Value> {
    let c = app.conn().await?;
    let server = app.server(&c.server_id).ok_or_else(|| ApiError::new(404, "Unknown server"))?;
    let mut libraries = Vec::new();
    let mut jf_error = Value::Null;
    let has_jf = server.jellyfin_url.is_some() && server.jellyfin_key.is_some();
    if has_jf {
        match jellyfin::fetch(server.jellyfin_url.as_deref(), server.jellyfin_key.as_deref(), "/Library/VirtualFolders", false).await {
            Ok(folders) => {
                for f in folders.as_array().cloned().unwrap_or_default() {
                    let locs: Vec<String> = f["Locations"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
                    let name = f["Name"].as_str().unwrap_or("Library").to_string();
                    for loc in &locs {
                        let available = c.stat(loc).await.map(|a| a.is_dir()).unwrap_or(false);
                        let label = if locs.len() > 1 { format!("{name} · {}", basename(loc)) } else { name.clone() };
                        libraries.push(json!({ "name": label, "path": loc, "type": f["CollectionType"].as_str().unwrap_or("mixed"), "available": available }));
                    }
                }
            }
            Err(e) => jf_error = Value::String(e.message),
        }
    }
    Ok(json!({ "home": c.home, "pins": server.pins, "libraries": libraries, "jellyfinError": jf_error, "jellyfin": has_jf }))
}

async fn pin(app: &App, b: &Value) -> R<Value> {
    let c = app.conn().await?;
    let p = abs_path(&s(b, "path"))?;
    let pins = {
        let mut cfg = app.cfg.lock().unwrap();
        let server = cfg.server_mut(&c.server_id).ok_or_else(|| ApiError::new(404, "Unknown server"))?;
        server.pins.retain(|x| x.path != p);
        if !crate::util::b(b, "remove") {
            let name = opt_s(b, "name").unwrap_or_else(|| if p == "/" { "/".into() } else { basename(&p) });
            server.pins.push(Pin { path: p, name });
        }
        server.pins.clone()
    };
    app.save();
    Ok(json!({ "pins": pins }))
}

// ------------------------------------------------------------------ browsing & changes

fn entry_json(name: &str, a: &FileAttributes, link: bool) -> Value {
    let dir = a.is_dir();
    let mut v = json!({ "name": name, "dir": dir, "size": if dir { 0 } else { a.size.unwrap_or(0) }, "mtime": a.mtime.unwrap_or(0) });
    if link {
        v["link"] = Value::Bool(true);
    }
    v
}

async fn list(app: &App, q: &HashMap<String, String>) -> R<Value> {
    let c = app.conn().await?;
    let dir = abs_path(q.get("path").filter(|p| !p.is_empty()).unwrap_or(&c.home))?;
    let raw = c.readdir(&dir).await?;
    let mut entries = Vec::new();
    let mut links = Vec::new();
    for e in raw {
        if hidden_name(&e.name) {
            continue;
        }
        if e.attrs.is_symlink() {
            links.push(e.name);
        } else {
            entries.push(entry_json(&e.name, &e.attrs, false));
        }
    }
    let resolved = futures::future::join_all(links.iter().map(|n| {
        let c = c.clone();
        let p = pjoin(&dir, n);
        async move { c.stat(&p).await.ok() }
    }))
    .await;
    for (n, a) in links.iter().zip(resolved) {
        if let Some(a) = a {
            entries.push(entry_json(n, &a, true));
        }
    }
    let (free, total) = match c.statvfs(&dir).await {
        Some((f, t)) => (json!(f), json!(t)),
        None => (Value::Null, Value::Null),
    };
    if dir != c.home && dir != "/" {
        if let Some(x) = app.cfg.lock().unwrap().server_mut(&c.server_id) {
            x.last_path = Some(dir.clone());
        }
        app.save();
    }
    Ok(json!({ "path": dir, "entries": entries, "free": free, "total": total }))
}

async fn info(app: &App, q: &HashMap<String, String>) -> R<Value> {
    let c = app.conn().await?;
    let p = abs_path(q.get("path").map(|x| x.as_str()).unwrap_or(""))?;
    let a = c.stat(&p).await?;
    let (mut size, mut files, mut folders, mut partial) = (a.size.unwrap_or(0), 1u64, 0u64, false);
    if a.is_dir() {
        let (mut fl, mut dl) = (Vec::new(), Vec::new());
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        partial = !c.walk(p.clone(), &mut fl, &mut dl, Some(deadline)).await?;
        size = fl.iter().map(|x| x.1).sum();
        files = fl.len() as u64;
        folders = dl.len() as u64;
    }
    Ok(json!({ "size": size, "files": files, "folders": folders, "partial": partial, "mtime": a.mtime.unwrap_or(0),
        "mode": format!("{:o}", a.permissions.unwrap_or(0) & 0o777), "uid": a.uid.unwrap_or(0), "gid": a.gid.unwrap_or(0) }))
}

async fn mkdir(app: &App, b: &Value) -> R<Value> {
    let c = app.conn().await?;
    let parent = abs_path(&s(b, "path"))?;
    if crate::util::b(b, "parents") {
        let parts = split_rel(&s(b, "name"));
        if parts.is_empty() {
            return Err(ApiError::new(400, "Enter a folder name."));
        }
        for p in &parts {
            valid_name(p)?;
        }
        c.mkdirp(&format!("{}/{}", parent.trim_end_matches('/'), parts.join("/"))).await?;
        return Ok(json!({ "name": parts[0] }));
    }
    let name = valid_name(s(b, "name").trim())?;
    let target = pjoin(&parent, &name);
    if c.exists(&target).await? {
        return Err(ApiError::new(409, format!("“{name}” already exists.")));
    }
    c.mkdir(&target).await?;
    Ok(json!({ "name": name }))
}

async fn rename(app: &App, b: &Value) -> R<Value> {
    let c = app.conn().await?;
    let p = abs_path(&s(b, "path"))?;
    if p == "/" {
        return Err(ApiError::new(400, "Can't rename that."));
    }
    let name = valid_name(s(b, "name").trim())?;
    let old = basename(&p);
    if name == old {
        return Ok(json!({ "name": name }));
    }
    let target = pjoin(&dirname(&p), &name);
    if name.to_lowercase() != old.to_lowercase() && c.exists(&target).await? {
        return Err(ApiError::new(409, format!("“{name}” already exists.")));
    }
    c.rename(&p, &target).await?;
    Ok(json!({ "name": name }))
}

async fn delete(app: &App, b: &Value) -> R<Value> {
    let c = app.conn().await?;
    let mut deleted = Vec::new();
    for raw in strs(b, "paths") {
        let p = abs_path(&raw)?;
        if p == "/" {
            return Err(ApiError::new(400, "Can't delete that."));
        }
        match c.rmrf(p.clone()).await {
            Ok(_) => {}
            Err(e) if e.status == 404 => {}
            Err(e) => return Err(e),
        }
        deleted.push(basename(&p));
    }
    Ok(json!({ "deleted": deleted }))
}

async fn transfer(app: &App, b: &Value) -> R<Value> {
    let c = app.conn().await?;
    let mode = s(b, "mode");
    if mode != "move" && mode != "copy" {
        return Err(ApiError::new(400, "Unknown mode"));
    }
    let dest = abs_path(&s(b, "dest"))?;
    if !c.stat(&dest).await?.is_dir() {
        return Err(ApiError::new(400, "Destination is not a folder."));
    }
    let (mut done, mut conflicts, mut errors) = (Vec::<String>::new(), Vec::<String>::new(), Vec::<String>::new());
    let mut names = c.names_in(&dest).await;
    for raw in strs(b, "paths") {
        let src = abs_path(&raw)?;
        let name = basename(&src);
        let a = match c.lstat(&src).await {
            Ok(a) => a,
            Err(_) => {
                errors.push(format!("“{name}” no longer exists."));
                continue;
            }
        };
        if a.is_dir() && (dest == src || dest.starts_with(&format!("{src}/"))) {
            errors.push(format!("“{name}” can't be put inside itself."));
            continue;
        }
        let mut target = pjoin(&dest, &name);
        if dirname(&src) == dest {
            if mode == "move" {
                continue;
            }
            target = pjoin(&dest, &unique_name(&names, &name, a.is_dir(), Some("copy")));
        } else if names.contains(&name) {
            conflicts.push(name);
            continue;
        }
        let r = if mode == "move" {
            c.rename(&src, &target).await.map_err(|e| e.message)
        } else {
            c.exec(&format!("cp -R -- {} {}", shell_quote(&src), shell_quote(&target))).await.map_err(|e| format!("Couldn't copy “{name}”: {e}"))
        };
        match r {
            Ok(_) => {
                let t = basename(&target);
                names.insert(t.clone());
                done.push(t);
            }
            Err(e) => errors.push(e),
        }
    }
    Ok(json!({ "done": done, "conflicts": conflicts, "errors": errors }))
}

// ------------------------------------------------------------------ uploads

fn upload_target(dir: &str, name: &str) -> R<(String, String)> {
    let dir = abs_path(dir)?;
    let parts = split_rel(name);
    if parts.is_empty() {
        return Err(ApiError::new(400, "Missing file name"));
    }
    for p in &parts {
        valid_name(p)?;
    }
    let target = format!("{}/{}", dir.trim_end_matches('/'), parts.join("/"));
    let partial = pjoin(&dirname(&target), &format!(".{}{}", basename(&target), PART_SUFFIX));
    Ok((target, partial))
}

async fn upload_status(app: &App, q: &HashMap<String, String>) -> R<Value> {
    let c = app.conn().await?;
    let (target, partial) = upload_target(q.get("dir").map(|x| x.as_str()).unwrap_or(""), q.get("name").map(|x| x.as_str()).unwrap_or(""))?;
    let size: u64 = q.get("size").and_then(|x| x.parse().ok()).unwrap_or(0);
    let mut offset = 0;
    if let Ok(a) = c.sftp.stat(partial.as_str()).await {
        offset = a.attrs.size.unwrap_or(0);
        if offset > size {
            let _ = c.sftp.remove(partial.as_str()).await;
            offset = 0;
        }
    }
    let exists = c.exists(&target).await?;
    let suggest = if exists { Some(unique_name(&c.names_in(&dirname(&target)).await, &basename(&target), false, None)) } else { None };
    Ok(json!({ "exists": exists, "offset": offset, "suggest": suggest }))
}

struct UploadGuard<'a>(&'a App);
impl Drop for UploadGuard<'_> {
    fn drop(&mut self) {
        self.0.uploads_active.fetch_sub(1, Ordering::SeqCst);
        self.0.last_upload_ms.store(now_ms(), Ordering::SeqCst);
    }
}

async fn upload(app: &App, q: &HashMap<String, String>, headers: &HeaderMap, body: Body) -> R<Response> {
    app.uploads_active.fetch_add(1, Ordering::SeqCst);
    let _guard = UploadGuard(app);
    let c = app.conn().await?;
    let (target, partial) = upload_target(q.get("dir").map(|x| x.as_str()).unwrap_or(""), q.get("name").map(|x| x.as_str()).unwrap_or(""))?;
    let num = |k: &str| q.get(k).and_then(|x| x.parse::<u64>().ok());
    let (offset, total) = match (num("offset"), num("total")) {
        (Some(o), Some(t)) => (o, t),
        _ => return Err(ApiError::new(400, "Bad upload parameters")),
    };
    let length: u64 = headers.get("content-length").and_then(|h| h.to_str().ok()).and_then(|x| x.parse().ok()).unwrap_or(0);
    if offset + length > total {
        return Err(ApiError::new(400, "Chunk doesn't fit the file size"));
    }
    let handle = if offset == 0 {
        if q.get("replace").map(|x| x.as_str()) != Some("1") && c.exists(&target).await? {
            return Err(ApiError::new(409, format!("“{}” already exists.", basename(&target))).reason("exists"));
        }
        c.mkdirp(&dirname(&target)).await?;
        c.open(&partial, OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE).await.map_err(|mut e| {
            if e.status == 404 {
                e = ApiError::new(404, "The destination folder doesn't exist.");
            }
            e
        })?
    } else {
        let size = c.sftp.stat(partial.as_str()).await.ok().and_then(|a| a.attrs.size);
        if size != Some(offset) {
            return Err(ApiError::new(409, "Upload offset mismatch").reason("offset").with("offset", size.unwrap_or(0)));
        }
        c.open(&partial, OpenFlags::WRITE).await?
    };

    // Stream the body to the server with many SFTP writes in flight.
    // `safe` only advances over a contiguous prefix of acknowledged writes, so a failed upload can resume exactly.
    let piece = c.piece;
    let mut stream = body.into_data_stream();
    let mut inflight: VecDeque<tokio::task::JoinHandle<Result<u64, russh_sftp::client::error::Error>>> = VecDeque::new();
    let mut buf: Vec<u8> = Vec::with_capacity(piece * 2);
    let mut pos = offset;
    let mut safe = offset;
    let mut failed: Option<String> = None;
    let send = |data: Vec<u8>, at: u64, inflight: &mut VecDeque<_>| {
        let sftp = c.sftp.clone();
        let h = handle.clone();
        let end = at + data.len() as u64;
        inflight.push_back(tokio::spawn(async move { sftp.write(h, at, data).await.map(|_| end) }));
    };
    loop {
        while inflight.len() >= IN_FLIGHT {
            match inflight.pop_front().unwrap().await {
                Ok(Ok(end)) => safe = end,
                Ok(Err(e)) => failed = Some(e.to_string()),
                Err(e) => failed = Some(e.to_string()),
            }
            if failed.is_some() {
                break;
            }
        }
        if failed.is_some() {
            break;
        }
        match stream.next().await {
            Some(Ok(chunk)) => {
                buf.extend_from_slice(&chunk);
                while buf.len() >= piece {
                    let rest = buf.split_off(piece);
                    let data = std::mem::replace(&mut buf, rest);
                    let len = data.len() as u64;
                    send(data, pos, &mut inflight);
                    pos += len;
                }
            }
            Some(Err(_)) => {
                failed = Some("aborted".into());
                break;
            }
            None => {
                if !buf.is_empty() {
                    send(std::mem::take(&mut buf), pos, &mut inflight);
                }
                break;
            }
        }
    }
    while let Some(h) = inflight.pop_front() {
        match h.await {
            Ok(Ok(end)) if failed.is_none() => safe = end,
            Ok(Ok(_)) => {}
            Ok(Err(e)) => failed = failed.or(Some(e.to_string())),
            Err(e) => failed = failed.or(Some(e.to_string())),
        }
    }
    if failed.is_some() {
        let attrs = FileAttributes { size: Some(safe), ..FileAttributes::empty() };
        let _ = c.sftp.fsetstat(handle.as_str(), attrs).await;
    }
    let _ = c.sftp.close(handle).await;
    if failed.is_some() || safe < offset + length {
        return Err(ApiError::new(500, "Upload interrupted").reason("offset").with("offset", safe));
    }
    if safe >= total {
        c.rename_into(&partial, &target).await?;
        return Ok(ok(json!({ "done": true, "name": basename(&target) })));
    }
    Ok(ok(json!({ "offset": safe })))
}

// ------------------------------------------------------------------ downloads to ~/Downloads

fn job_view(j: &mut Job) -> Value {
    let now = now_ms();
    j.samples.push_back((now, j.done));
    while j.samples.len() > 2 && now - j.samples[0].0 > 4000 {
        j.samples.pop_front();
    }
    let (t0, b0) = j.samples[0];
    let speed = if now - t0 > 300 { (j.done - b0) as f64 / (now - t0) as f64 * 1000.0 } else { 0.0 };
    json!({ "id": j.id.to_string(), "kind": "download", "name": j.name, "total": j.total, "done": j.done, "status": j.status,
        "error": j.error, "localPath": j.local_path.as_ref().map(|p| p.to_string_lossy().to_string()), "speed": speed })
}

async fn start_download(app: &Arc<App>, b: &Value) -> R<Value> {
    let paths: Vec<String> = strs(b, "paths").iter().map(|p| abs_path(p)).collect::<R<_>>()?;
    if paths.is_empty() {
        return Err(ApiError::new(400, "Nothing selected"));
    }
    app.conn().await?;
    let id = app.job_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let name = if paths.len() == 1 { basename(&paths[0]) } else { format!("{} items", paths.len()) };
    let job = Arc::new(Mutex::new(Job { id, name, total: 0, done: 0, status: "preparing", error: None, local_path: None, cancelled: false, samples: VecDeque::new() }));
    app.jobs.lock().unwrap().insert(id, job.clone());
    let view = job_view(&mut job.lock().unwrap());
    let app2 = app.clone();
    tokio::spawn(async move {
        let result = run_download(&app2, &job, paths).await;
        let mut j = job.lock().unwrap();
        match result {
            Ok(_) => j.status = "done",
            Err(e) if j.cancelled || e.status == 499 => {
                j.status = "cancelled";
                if let Some(p) = &j.local_path {
                    let _ = std::fs::remove_dir_all(p).or_else(|_| std::fs::remove_file(p));
                }
            }
            Err(e) => {
                j.status = "error";
                j.error = Some(e.message);
            }
        }
    });
    Ok(json!({ "job": view }))
}

async fn run_download(app: &App, job: &Arc<Mutex<Job>>, paths: Vec<String>) -> R<()> {
    let c = app.conn().await?;
    let dl = app.downloads.clone();
    std::fs::create_dir_all(&dl).map_err(|e| ApiError::new(500, e.to_string()))?;
    let mut plan: Vec<(String, PathBuf, u64)> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for p in &paths {
        let a = c.stat(p).await?;
        let top = dl.join(local_unique(&dl, &local_safe(&basename(p))));
        {
            let mut j = job.lock().unwrap();
            if j.local_path.is_none() {
                j.local_path = Some(top.clone());
            }
        }
        if a.is_dir() {
            dirs.push(top.clone());
            let (mut files, mut subdirs) = (Vec::new(), Vec::new());
            c.walk(p.clone(), &mut files, &mut subdirs, None).await?;
            let local = |remote: &str| -> PathBuf {
                let rel = remote[p.len()..].trim_start_matches('/');
                rel.split('/').fold(top.clone(), |acc, part| acc.join(local_safe(part)))
            };
            dirs.extend(subdirs.iter().map(|d| local(d)));
            plan.extend(files.iter().map(|(f, size)| (f.clone(), local(f), *size)));
        } else {
            plan.push((p.clone(), top, a.size.unwrap_or(0)));
        }
    }
    {
        let mut j = job.lock().unwrap();
        j.total = plan.iter().map(|x| x.2).sum();
        j.status = "running";
    }
    for d in &dirs {
        std::fs::create_dir_all(d).map_err(|e| ApiError::new(500, e.to_string()))?;
    }
    for (remote, local, size) in plan {
        let tmp = PathBuf::from(format!("{}.dropfin-download", local.to_string_lossy()));
        let mut attempt = 0;
        loop {
            let before = job.lock().unwrap().done;
            let conn = app.conn().await?;
            match pipelined_get(&conn, &remote, &tmp, size, job).await {
                Ok(_) => break,
                Err(e) => {
                    attempt += 1;
                    let cancelled = job.lock().unwrap().cancelled;
                    if cancelled || e.status == 499 || attempt > 3 || e.status == 403 || e.status == 404 {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(e);
                    }
                    job.lock().unwrap().done = before;
                    tokio::time::sleep(Duration::from_secs(attempt)).await;
                }
            }
        }
        std::fs::rename(&tmp, &local).map_err(|e| ApiError::new(500, e.to_string()))?;
    }
    Ok(())
}

async fn pipelined_get(c: &Arc<Conn>, remote: &str, local: &Path, size: u64, job: &Arc<Mutex<Job>>) -> R<()> {
    let handle = c.open(remote, OpenFlags::READ).await?;
    let file = Arc::new(std::fs::File::create(local).map_err(|e| ApiError::new(500, format!("Couldn't save to Downloads: {e}")))?);
    let piece = c.piece as u64;
    let mut next = 0u64;
    let mut inflight = VecDeque::new();
    let mut result: R<()> = Ok(());
    loop {
        if job.lock().unwrap().cancelled {
            result = Err(ApiError::new(499, "Cancelled"));
            break;
        }
        while inflight.len() < IN_FLIGHT && next < size {
            let (sftp, h, f, job) = (c.sftp.clone(), handle.clone(), file.clone(), job.clone());
            let (start, len) = (next, piece.min(size - next));
            next += len;
            inflight.push_back(tokio::spawn(async move {
                let mut pos = start;
                let end = start + len;
                while pos < end {
                    let data = match sftp.read(h.as_str(), pos, (end - pos) as u32).await {
                        Ok(d) => d.data,
                        Err(e) if is_code(&e, StatusCode::Eof) => break,
                        Err(e) => return Err(e.to_string()),
                    };
                    if data.is_empty() {
                        break;
                    }
                    write_at(&f, &data, pos).map_err(|e| e.to_string())?;
                    pos += data.len() as u64;
                    job.lock().unwrap().done += data.len() as u64;
                }
                Ok::<(), String>(())
            }));
        }
        match inflight.pop_front() {
            None => break,
            Some(t) => match t.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    result = Err(ApiError::new(502, e));
                    break;
                }
                Err(e) => {
                    result = Err(ApiError::new(500, e.to_string()));
                    break;
                }
            },
        }
    }
    for t in inflight {
        let _ = t.await;
    }
    let _ = c.sftp.close(handle).await;
    result
}

#[cfg(unix)]
fn write_at(f: &std::fs::File, data: &[u8], pos: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(data, pos)
}
#[cfg(windows)]
fn write_at(f: &std::fs::File, data: &[u8], pos: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < data.len() {
        done += f.seek_write(&data[done..], pos + done as u64)?;
    }
    Ok(())
}

fn reveal(app: &App, b: &Value) -> R<Value> {
    let p = PathBuf::from(s(b, "localPath"));
    if !p.starts_with(&app.downloads) {
        return Err(ApiError::new(400, "Not a download"));
    }
    if cfg!(target_os = "macos") {
        let args: Vec<String> = if p.exists() { vec!["-R".into(), p.to_string_lossy().into()] } else { vec![app.downloads.to_string_lossy().into()] };
        let _ = std::process::Command::new("/usr/bin/open").args(args).spawn();
    }
    Ok(json!({ "ok": true }))
}

// ------------------------------------------------------------------ streaming (Quick Look)

fn mime(p: &str) -> &'static str {
    let ext = p.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "heic" => "image/heic",
        "bmp" => "image/bmp",
        "svg" | "txt" | "nfo" | "srt" | "ass" | "ssa" | "vtt" | "log" | "md" | "xml" | "json" | "cue" | "html" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

struct CloseOnDrop {
    conn: Arc<Conn>,
    handle: String,
    closed: AtomicBool,
}
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            let (sftp, h) = (self.conn.sftp.clone(), self.handle.clone());
            tokio::spawn(async move {
                let _ = sftp.close(h).await;
            });
        }
    }
}

async fn stream(app: &App, q: &HashMap<String, String>, headers: &HeaderMap, head: bool) -> R<Response> {
    let c = app.conn().await?;
    let p = abs_path(q.get("path").map(|x| x.as_str()).unwrap_or(""))?;
    let a = c.stat(&p).await?;
    if !a.is_regular() {
        return Err(ApiError::new(400, "Not a file"));
    }
    let size = a.size.unwrap_or(0);
    let (mut start, mut end, mut status) = (0u64, size.saturating_sub(1), 200u16);
    if let Some(r) = headers.get("range").and_then(|h| h.to_str().ok()).and_then(|r| r.trim().strip_prefix("bytes=")) {
        if let Some((a, b)) = r.split_once('-') {
            if !(a.is_empty() && b.is_empty()) {
                if a.is_empty() {
                    start = size.saturating_sub(b.parse().unwrap_or(0));
                } else {
                    start = a.parse().unwrap_or(0);
                    end = if b.is_empty() { size.saturating_sub(1) } else { b.parse::<u64>().unwrap_or(0).min(size.saturating_sub(1)) };
                }
                if start >= size || start > end {
                    return Ok(Response::builder().status(416).header("content-range", format!("bytes */{size}")).body(Body::empty()).unwrap());
                }
                status = 206;
            }
        }
    }
    let len = if size == 0 { 0 } else { end - start + 1 };
    let mut rb = Response::builder()
        .status(status)
        .header("content-type", mime(&p))
        .header("accept-ranges", "bytes")
        .header("content-length", len)
        .header("cache-control", "no-store")
        .header("x-content-type-options", "nosniff")
        .header("content-security-policy", "default-src 'none'; img-src 'self'; media-src 'self'; style-src 'unsafe-inline'");
    if status == 206 {
        rb = rb.header("content-range", format!("bytes {start}-{end}/{size}"));
    }
    if head || len == 0 {
        return Ok(rb.body(Body::empty()).unwrap());
    }
    let handle = c.open(&p, OpenFlags::READ).await?;
    let guard = Arc::new(CloseOnDrop { conn: c.clone(), handle: handle.clone(), closed: AtomicBool::new(false) });
    let chunk = (c.piece as u64).max(32 * 1024);
    let body = futures::stream::unfold((start, guard), move |(pos, guard)| async move {
        if pos > end {
            return None;
        }
        let want = chunk.min(end - pos + 1) as u32;
        match guard.conn.sftp.read(guard.handle.as_str(), pos, want).await {
            Ok(d) if !d.data.is_empty() => {
                let n = d.data.len() as u64;
                Some((Ok::<Bytes, std::io::Error>(Bytes::from(d.data)), (pos + n, guard)))
            }
            Ok(_) => None,
            Err(e) if is_code(&e, StatusCode::Eof) => None,
            Err(e) => Some((Err(std::io::Error::other(e.to_string())), (end + 1, guard))),
        }
    });
    Ok(rb.body(Body::from_stream(body)).unwrap())
}

pub fn port_file(app: &App) -> PathBuf {
    app.data_dir.join("port")
}

pub async fn serve(app: Arc<App>, preferred: u16, on_ready: impl FnOnce(u16)) -> anyhow::Result<()> {
    let mut port = preferred;
    let listener = loop {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => break l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && port < preferred + 20 => port += 1,
            Err(e) => return Err(e.into()),
        }
    };
    let _ = std::fs::write(port_file(&app), port.to_string());
    println!("Dropfin {VERSION} running at http://localhost:{port}/");
    on_ready(port);
    axum::serve(listener, router(app)).await?;
    Ok(())
}
