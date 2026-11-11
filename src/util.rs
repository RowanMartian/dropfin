//! Shared helpers: API errors, config storage, path handling.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const PART_SUFFIX: &str = ".dropfin-part";
pub const IGNORED: &[&str] = &[
    "@eaDir", "#recycle", "#snapshot", "$RECYCLE.BIN", "System Volume Information", "lost+found", "Thumbs.db", "desktop.ini",
];

// ------------------------------------------------------------------ errors

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    pub extra: Map<String, Value>,
}
pub type R<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), extra: Map::new() }
    }
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extra.insert(key.to_string(), value.into());
        self
    }
    pub fn reason(self, r: &str) -> Self {
        self.with("reason", r)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = self.extra;
        body.insert("error".into(), Value::String(self.message));
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, [("cache-control", "no-store")], axum::Json(Value::Object(body))).into_response()
    }
}

pub fn ok(v: Value) -> Response {
    ([("cache-control", "no-store")], axum::Json(v)).into_response()
}

// ------------------------------------------------------------------ JSON body helpers

pub fn s(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}
pub fn opt_s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string()).filter(|x| !x.is_empty())
}
pub fn b(v: &Value, k: &str) -> bool {
    v.get(k).map(|x| x.as_bool().unwrap_or(false)).unwrap_or(false)
}
pub fn strs(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

// ------------------------------------------------------------------ config

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Pin {
    pub path: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Server {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub host_key: Option<String>,
    #[serde(default)]
    pub pins: Vec<Pin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_path: Option<String>,
    #[serde(default)]
    pub jellyfin_url: Option<String>,
    #[serde(default)]
    pub jellyfin_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_scan: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_password: Option<bool>,
    /// Only used where there is no Keychain (Linux dev builds).
    #[serde(default, rename = "_password", skip_serializing_if = "Option::is_none")]
    pub password_fallback: Option<String>,
}
fn default_port() -> u16 {
    22
}

impl Server {
    pub fn account(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }
    /// What the UI may see: no secrets.
    pub fn public(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(json!({}));
        if let Value::Object(m) = &mut v {
            m.remove("_password");
            m.remove("jellyfinKey");
            m.insert("hasJellyfinKey".into(), Value::Bool(self.jellyfin_key.is_some()));
        }
        v
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub servers: Vec<Server>,
    #[serde(default)]
    pub last_server: Option<String>,
    #[serde(default)]
    pub prefs: Map<String, Value>,
}

impl Config {
    pub fn load(file: &Path) -> Config {
        std::fs::read_to_string(file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }
    pub fn save(&self, file: &Path) {
        let tmp = file.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        if let Ok(mut f) = opts.open(&tmp) {
            use std::io::Write;
            if f.write_all(text.as_bytes()).is_ok() {
                let _ = std::fs::rename(&tmp, file);
            }
        }
    }
    pub fn server(&self, id: &str) -> Option<&Server> {
        self.servers.iter().find(|s| s.id == id)
    }
    pub fn server_mut(&mut self, id: &str) -> Option<&mut Server> {
        self.servers.iter_mut().find(|s| s.id == id)
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("DROPFIN_HOME") {
        return PathBuf::from(d);
    }
    if cfg!(target_os = "macos") {
        home_dir().join("Library/Application Support/Dropfin")
    } else {
        home_dir().join(".config/dropfin")
    }
}
pub fn downloads_dir() -> PathBuf {
    std::env::var_os("DROPFIN_DOWNLOADS").map(PathBuf::from).unwrap_or_else(|| home_dir().join("Downloads"))
}

// ------------------------------------------------------------------ remote paths (always POSIX)

pub fn abs_path(p: &str) -> R<String> {
    if !p.starts_with('/') || p.split('/').any(|x| x == "..") || p.contains('\0') {
        return Err(ApiError::new(400, "Invalid path"));
    }
    let parts: Vec<&str> = p.split('/').filter(|x| !x.is_empty() && *x != ".").collect();
    Ok(format!("/{}", parts.join("/")))
}
pub fn pjoin(a: &str, b: &str) -> String {
    if a == "/" {
        format!("/{b}")
    } else {
        format!("{a}/{b}")
    }
}
pub fn dirname(p: &str) -> String {
    match p.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => p[..i].to_string(),
    }
}
pub fn basename(p: &str) -> String {
    p.rsplit('/').next().unwrap_or("").to_string()
}
pub fn valid_name(name: &str) -> R<String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') || name.len() > 255 || name.ends_with(PART_SUFFIX) {
        return Err(ApiError::new(400, format!("“{name}” isn't a valid name.")));
    }
    Ok(name.to_string())
}
pub fn split_rel(rel: &str) -> Vec<String> {
    rel.split('/').filter(|p| !p.is_empty() && *p != ".").map(String::from).collect()
}
pub fn unique_name(names: &HashSet<String>, name: &str, is_dir: bool, word: Option<&str>) -> String {
    let dot = if is_dir { None } else { name.rfind('.').filter(|&i| i > 0) };
    let (stem, ext) = match dot {
        Some(i) => (&name[..i], &name[i..]),
        None => (name, ""),
    };
    let mut base = stem.to_string();
    if let Some(w) = word {
        let c = format!("{stem} {w}{ext}");
        if !names.contains(&c) {
            return c;
        }
        base = format!("{stem} {w}");
    }
    let mut n = 2;
    loop {
        let c = format!("{base} {n}{ext}");
        if !names.contains(&c) {
            return c;
        }
        n += 1;
    }
}
pub fn hidden_name(name: &str) -> bool {
    name.starts_with('.') || IGNORED.contains(&name)
}
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ------------------------------------------------------------------ local downloads

pub fn local_safe(name: &str) -> String {
    let mut n: String = name.chars().map(|c| if c == '/' || c == '\0' { '_' } else { c }).collect();
    if n.starts_with('.') {
        n = format!("_{}", n.trim_start_matches('.'));
    }
    if n.is_empty() {
        "file".into()
    } else {
        n
    }
}
pub fn local_unique(dir: &Path, name: &str) -> String {
    if !dir.join(name).exists() {
        return name.to_string();
    }
    let p = Path::new(name);
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let stem = if ext.is_empty() { name.to_string() } else { name[..name.len() - ext.len()].to_string() };
    let mut n = 2;
    loop {
        let c = format!("{stem} {n}{ext}");
        if !dir.join(&c).exists() {
            return c;
        }
        n += 1;
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
