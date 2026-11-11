//! SSH connection + SFTP helpers.

use crate::util::*;
use futures::future::BoxFuture;
use futures::FutureExt;
use russh::client;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::ChannelMsg;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::RawSftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags, Packet, StatusCode};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Client {
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
    mismatch: Arc<AtomicBool>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let fp = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256).to_string(),
            PublicKeyOrCertificate::Certificate(c) => PublicKey::new(c.public_key().clone(), "").fingerprint(HashAlg::Sha256).to_string(),
        };
        *self.seen.lock().unwrap() = Some(fp.clone());
        if let Some(expected) = &self.expected {
            if expected != &fp {
                self.mismatch.store(true, Ordering::SeqCst);
                return Ok(false);
            }
        }
        Ok(true)
    }
}

pub struct Conn {
    pub server_id: String,
    pub password: Option<String>,
    pub handle: client::Handle<Client>,
    pub sftp: Arc<RawSftpSession>,
    pub home: String,
    pub posix_rename: bool,
    pub statvfs: AtomicBool,
    pub piece: usize,
}

pub struct Opened {
    pub conn: Conn,
    pub fingerprint: String,
}

fn network_error(server: &Server, e: &std::io::Error) -> ApiError {
    use std::io::ErrorKind::*;
    let host = &server.host;
    let mac_hint = if cfg!(target_os = "macos") { ", and that Dropfin is allowed under System Settings → Privacy & Security → Local Network" } else { "" };
    let msg = match e.kind() {
        ConnectionRefused => format!("{host} refused the connection on port {}. Is SSH (Remote Login) turned on on the server?", server.port),
        TimedOut => format!("Timed out connecting to {host}."),
        HostUnreachable | NetworkUnreachable => format!("Can't reach {host}. Make sure you're on the same Wi-Fi{mac_hint}."),
        _ if e.to_string().contains("lookup") || e.to_string().contains("nodename") || e.to_string().contains("not known") => {
            format!("Couldn't find a computer called “{host}”. Try its IP address instead.")
        }
        _ => format!("Can't connect to {host}: {e}"),
    };
    ApiError::new(502, msg).reason("network")
}

fn local_keys() -> Vec<russh::keys::PrivateKey> {
    let ssh = home_dir().join(".ssh");
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .iter()
        .filter_map(|n| russh::keys::load_secret_key(ssh.join(n), None).ok()) // passphrase-protected keys are left to ssh-agent
        .collect()
}

pub async fn open(server: &Server, password: Option<String>) -> R<Opened> {
    let seen = Arc::new(Mutex::new(None));
    let mismatch = Arc::new(AtomicBool::new(false));
    let handler = Client { expected: server.host_key.clone(), seen: seen.clone(), mismatch: mismatch.clone() };
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 4,
        nodelay: true,
        window_size: 16 * 1024 * 1024,
        maximum_packet_size: 65535,
        // Prefer AES-GCM: hardware-accelerated on every Mac, several times faster than ChaCha20 here.
        preferred: russh::Preferred {
            cipher: std::borrow::Cow::Borrowed(&[
                russh::cipher::AES_256_GCM,
                russh::cipher::AES_256_CTR,
                russh::cipher::AES_128_CTR,
                russh::cipher::CHACHA20_POLY1305,
            ]),
            ..Default::default()
        },
        ..Default::default()
    });

    let connecting = client::connect(config, (server.host.as_str(), server.port), handler);
    let mut handle = match tokio::time::timeout(Duration::from_secs(15), connecting).await {
        Err(_) => return Err(ApiError::new(504, format!("Timed out connecting to {}.", server.host)).reason("network")),
        Ok(Err(e)) => {
            if mismatch.load(Ordering::SeqCst) {
                let fp = seen.lock().unwrap().clone().unwrap_or_default();
                return Err(ApiError::new(409, format!("The identity of {} has changed since you last connected. If you reinstalled the server this is expected; otherwise someone may be intercepting the connection.", server.host))
                    .reason("hostkey").with("fingerprint", fp).with("saved", server.host_key.clone().unwrap_or_default()));
            }
            return Err(match e {
                russh::Error::IO(io) => network_error(server, &io),
                other => ApiError::new(502, format!("Connection failed: {other}")).reason("network"),
            });
        }
        Ok(Ok(h)) => h,
    };
    let fingerprint = seen.lock().unwrap().clone().unwrap_or_default();

    // ---- authentication: ssh-agent, local keys, then password / keyboard-interactive
    let user = server.username.clone();
    let rsa_hash = handle.best_supported_rsa_hash().await.ok().flatten().flatten();
    let mut authed = false;

    if std::env::var_os("SSH_AUTH_SOCK").is_some() {
        if let Ok(mut agent) = russh::keys::agent::client::AgentClient::connect_env().await {
            if let Ok(ids) = agent.request_identities().await {
                for id in ids.iter().take(5) {
                    let key = id.public_key().into_owned();
                    if let Ok(r) = handle.authenticate_publickey_with(user.clone(), key, rsa_hash, &mut agent).await {
                        if r.success() {
                            authed = true;
                            break;
                        }
                    }
                }
            }
        }
    }
    if !authed {
        for key in local_keys() {
            let k = PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash);
            if let Ok(r) = handle.authenticate_publickey(user.clone(), k).await {
                if r.success() {
                    authed = true;
                    break;
                }
            }
        }
    }
    if !authed {
        if let Some(pw) = &password {
            if let Ok(r) = handle.authenticate_password(user.clone(), pw.clone()).await {
                authed = r.success();
            }
            if !authed {
                use russh::client::KeyboardInteractiveAuthResponse as K;
                let mut resp = handle.authenticate_keyboard_interactive_start(user.clone(), None).await;
                for _ in 0..4 {
                    match resp {
                        Ok(K::Success) => {
                            authed = true;
                            break;
                        }
                        Ok(K::InfoRequest { prompts, .. }) => {
                            let answers = prompts.iter().map(|_| pw.clone()).collect();
                            resp = handle.authenticate_keyboard_interactive_respond(answers).await;
                        }
                        _ => break,
                    }
                }
            }
        }
    }
    if !authed {
        let _ = handle.disconnect(russh::Disconnect::ByApplication, "", "en").await;
        return Err(if password.is_some() {
            ApiError::new(401, "Wrong username or password.").reason("auth")
        } else {
            ApiError::new(401, "This server needs a password.").reason("needPassword")
        });
    }

    // ---- SFTP
    let sftp_off = || ApiError::new(502, "Logged in, but SFTP is turned off on this server (check “Subsystem sftp” in sshd_config).");
    let channel = handle.channel_open_session().await.map_err(|_| sftp_off())?;
    channel.request_subsystem(true, "sftp").await.map_err(|_| sftp_off())?;
    let mut sftp = RawSftpSession::new(channel.into_stream());
    sftp.set_timeout(60);
    let version = sftp.init().await.map_err(|_| sftp_off())?;
    let mut piece = 32 * 1024;
    if version.extensions.contains_key(russh_sftp::extensions::LIMITS) {
        if let Ok(limits) = sftp.limits().await {
            let limits: russh_sftp::client::rawsession::Limits = limits.into();
            let max = limits.write_len.unwrap_or(32 * 1024).min(limits.read_len.unwrap_or(32 * 1024));
            piece = (max as usize).clamp(16 * 1024, 64 * 1024);
            sftp.set_limits(limits);
        }
    }
    let posix_rename = version.extensions.contains_key("posix-rename@openssh.com");
    let home = sftp.realpath(".").await.ok().and_then(|n| n.files.into_iter().next()).map(|f| f.filename).unwrap_or_else(|| "/".into());

    Ok(Opened {
        conn: Conn { server_id: server.id.clone(), password, handle, sftp: Arc::new(sftp), home, posix_rename, statvfs: AtomicBool::new(true), piece },
        fingerprint,
    })
}

// ------------------------------------------------------------------ error mapping

pub fn sftp_err(e: SftpError, p: &str) -> ApiError {
    let name = if p.is_empty() { "That item".to_string() } else { format!("“{}”", if basename(p).is_empty() { p.to_string() } else { basename(p) }) };
    match e {
        SftpError::Status(st) => match st.status_code {
            StatusCode::NoSuchFile => ApiError::new(404, format!("{name} doesn't exist (anymore).")),
            StatusCode::PermissionDenied => ApiError::new(403, format!("Permission denied for {name}. The SSH user can't write here — check the folder's owner and permissions on the server.")),
            StatusCode::Failure => ApiError::new(500, format!("The server couldn't complete that for {name}. It may already exist, not be empty, or be in use.")),
            _ => ApiError::new(500, format!("{}: {}", st.status_code, st.error_message)),
        },
        SftpError::Timeout => ApiError::new(504, "The server took too long to answer."),
        other => ApiError::new(502, format!("Connection problem: {other}")),
    }
}
pub fn is_code(e: &SftpError, code: StatusCode) -> bool {
    matches!(e, SftpError::Status(st) if st.status_code == code)
}

pub fn encode_strings(parts: &[&str]) -> Vec<u8> {
    let mut v = Vec::new();
    for p in parts {
        v.extend_from_slice(&(p.len() as u32).to_be_bytes());
        v.extend_from_slice(p.as_bytes());
    }
    v
}

// ------------------------------------------------------------------ operations

pub struct Entry {
    pub name: String,
    pub attrs: FileAttributes,
}

impl Conn {
    pub fn alive(&self) -> bool {
        !self.handle.is_closed()
    }
    pub async fn stat(&self, p: &str) -> R<FileAttributes> {
        self.sftp.stat(p).await.map(|a| a.attrs).map_err(|e| sftp_err(e, p))
    }
    pub async fn lstat(&self, p: &str) -> R<FileAttributes> {
        self.sftp.lstat(p).await.map(|a| a.attrs).map_err(|e| sftp_err(e, p))
    }
    pub async fn exists(&self, p: &str) -> R<bool> {
        match self.sftp.lstat(p).await {
            Ok(_) => Ok(true),
            Err(e) if is_code(&e, StatusCode::NoSuchFile) => Ok(false),
            Err(e) => Err(sftp_err(e, p)),
        }
    }
    pub async fn readdir(&self, p: &str) -> R<Vec<Entry>> {
        let h = self.sftp.opendir(p).await.map_err(|e| sftp_err(e, p))?.handle;
        let mut out = Vec::new();
        loop {
            match self.sftp.readdir(h.as_str()).await {
                Ok(name) => {
                    if name.files.is_empty() {
                        break;
                    }
                    for f in name.files {
                        if f.filename != "." && f.filename != ".." {
                            out.push(Entry { name: f.filename, attrs: f.attrs });
                        }
                    }
                }
                Err(e) if is_code(&e, StatusCode::Eof) => break,
                Err(e) => {
                    let _ = self.sftp.close(h.as_str()).await;
                    return Err(sftp_err(e, p));
                }
            }
        }
        let _ = self.sftp.close(h).await;
        Ok(out)
    }
    pub async fn names_in(&self, dir: &str) -> HashSet<String> {
        self.readdir(dir).await.map(|v| v.into_iter().map(|e| e.name).collect()).unwrap_or_default()
    }
    pub async fn mkdir(&self, p: &str) -> R<()> {
        self.sftp.mkdir(p, FileAttributes::empty()).await.map(|_| ()).map_err(|e| sftp_err(e, p))
    }
    pub async fn mkdirp(&self, dir: &str) -> R<()> {
        let mut cur = String::new();
        for part in dir.split('/').filter(|x| !x.is_empty()) {
            cur = format!("{cur}/{part}");
            match self.sftp.stat(cur.as_str()).await {
                Ok(a) if a.attrs.is_dir() => {}
                Ok(_) => return Err(ApiError::new(409, format!("“{part}” is a file, not a folder."))),
                Err(e) if is_code(&e, StatusCode::NoSuchFile) => {
                    if let Err(e2) = self.sftp.mkdir(cur.as_str(), FileAttributes::empty()).await {
                        if !self.exists(&cur).await? {
                            return Err(sftp_err(e2, &cur));
                        }
                    }
                }
                Err(e) => return Err(sftp_err(e, &cur)),
            }
        }
        Ok(())
    }
    pub fn rmrf<'a>(&'a self, p: String) -> BoxFuture<'a, R<()>> {
        async move {
            let a = self.lstat(&p).await?;
            if a.is_dir() {
                let list = self.readdir(&p).await?;
                for chunk in list.chunks(8) {
                    let futs = chunk.iter().map(|e| self.rmrf(pjoin(&p, &e.name)));
                    for r in futures::future::join_all(futs).await {
                        r?;
                    }
                }
                self.sftp.rmdir(p.as_str()).await.map_err(|e| sftp_err(e, &p))?;
            } else {
                self.sftp.remove(p.as_str()).await.map_err(|e| sftp_err(e, &p))?;
            }
            Ok(())
        }
        .boxed()
    }
    pub async fn rename(&self, from: &str, to: &str) -> R<()> {
        self.sftp.rename(from, to).await.map(|_| ()).map_err(|e| sftp_err(e, from))
    }
    /// Rename that replaces an existing target (posix-rename when available).
    pub async fn rename_into(&self, from: &str, to: &str) -> R<()> {
        if self.posix_rename {
            match self.sftp.extended("posix-rename@openssh.com", encode_strings(&[from, to])).await {
                Ok(Packet::Status(st)) if st.status_code == StatusCode::Ok => return Ok(()),
                _ => {}
            }
        }
        if self.exists(to).await? {
            self.rmrf(to.to_string()).await?;
        }
        self.rename(from, to).await
    }
    pub async fn statvfs(&self, p: &str) -> Option<(u64, u64)> {
        if !self.statvfs.load(Ordering::Relaxed) {
            return None;
        }
        match self.sftp.statvfs(p).await {
            Ok(v) => Some((v.blocks_avail * v.fragment_size, v.blocks * v.fragment_size)),
            Err(_) => {
                self.statvfs.store(false, Ordering::Relaxed);
                None
            }
        }
    }
    /// Recursively list files (path, size) and folders below `dir`. Returns false if the deadline passed.
    pub fn walk<'a>(&'a self, dir: String, files: &'a mut Vec<(String, u64)>, dirs: &'a mut Vec<String>, deadline: Option<std::time::Instant>) -> BoxFuture<'a, R<bool>> {
        async move {
            for e in self.readdir(&dir).await? {
                if deadline.is_some_and(|d| std::time::Instant::now() > d) {
                    return Ok(false);
                }
                if hidden_name(&e.name) {
                    continue;
                }
                let p = pjoin(&dir, &e.name);
                if e.attrs.is_dir() {
                    dirs.push(p.clone());
                    if !self.walk(p, files, dirs, deadline).await? {
                        return Ok(false);
                    }
                } else if e.attrs.is_regular() {
                    files.push((p, e.attrs.size.unwrap_or(0)));
                }
            }
            Ok(true)
        }
        .boxed()
    }
    pub async fn exec(&self, cmd: &str) -> Result<(), String> {
        let mut ch = self.handle.channel_open_session().await.map_err(|e| e.to_string())?;
        ch.exec(true, cmd).await.map_err(|e| e.to_string())?;
        let mut code = None;
        let mut stderr = Vec::new();
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                ChannelMsg::ExtendedData { data, ext: 1 } => stderr.extend_from_slice(&data),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        match code {
            Some(0) => Ok(()),
            c => {
                let msg = String::from_utf8_lossy(&stderr).trim().to_string();
                Err(if msg.is_empty() { format!("exit {}", c.map(|x| x.to_string()).unwrap_or("?".into())) } else { msg })
            }
        }
    }
    pub async fn open(&self, p: &str, flags: OpenFlags) -> R<String> {
        self.sftp.open(p, flags, FileAttributes::empty()).await.map(|h| h.handle).map_err(|e| sftp_err(e, p))
    }
}
