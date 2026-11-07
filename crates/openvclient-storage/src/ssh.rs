//! SSH servers: interactive terminal (PTY shell), SFTP file browsing and
//! one-off remote commands.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use openvclient_core::resource::*;
use openvclient_core::{Category, ColumnMeta, ConnectParams, Driver, DriverInfo, Error, FieldKind, FieldSpec, Result, ResultSet, ServerInfo, ServiceSession, SessionHandle, Value};
use openvclient_net::{HostKeyCheck, SshAuth, SshTarget, ssh_connect_via};
use russh::ChannelMsg;
use russh::client::Handle;
use russh_sftp::client::SftpSession;

const PREVIEW_MAX: u64 = 1024 * 1024;
/// Profile key holding the pinned host key (shared with SSH tunnels).
pub const FINGERPRINT: &str = openvclient_net::keys::SSH_FINGERPRINT;
/// Pinned host key of the jump host.
pub const JUMP_FINGERPRINT: &str = "jump_fingerprint";
/// Marks an unknown-key error as coming from the jump host.
pub const JUMP_MARK: &str = "(jump host)";

pub struct SshDriver {
    info: DriverInfo,
}

const AUTH: &[&str] = &["password", "keyboard_interactive", "key", "key_text", "certificate", "agent", "none"];

impl Default for SshDriver {
    fn default() -> Self {
        Self {
            info: DriverInfo {
                id: "ssh",
                name: "SSH / SFTP",
                category: Category::Remote,
                description: "SSH terminal, SFTP file browser and remote commands",
                badge: "SSH",
                fields: vec![
                    FieldSpec::text("host", "Host").default("localhost").required(),
                    FieldSpec::text("port", "Port").kind(FieldKind::Number).default("22"),
                    FieldSpec::text("user", "User").required(),
                    FieldSpec::text("auth", "Authentication").kind(FieldKind::Choice(AUTH)).default("password").help(
                        "password · keyboard_interactive: PAM / OTP / 2FA prompts · key: private key file · key_text: paste a private key · certificate: OpenSSH user certificate + key · agent: ssh-agent, Pageant or the Windows OpenSSH agent · none: servers that allow it",
                    ),
                    FieldSpec::text("password", "Password / passphrase / pasted key").kind(FieldKind::Secret).show_if("auth", &["password", "keyboard_interactive", "key", "key_text", "certificate"]).help("password: the login password · key/certificate: the key's passphrase (empty if none) · key_text: the whole private key text (unencrypted)"),
                    FieldSpec::text("otp", "One-time code").placeholder("only if the server asks for a code").show_if("auth", &["keyboard_interactive"]).help("Answers non-password prompts (TOTP, Duo…). Update it before each connect."),
                    FieldSpec::text("key_path", "Private key").kind(FieldKind::File { save: false, filters: &[] }).placeholder("~/.ssh/id_ed25519").show_if("auth", &["key", "certificate"]),
                    FieldSpec::text("cert_path", "Certificate").kind(FieldKind::File { save: false, filters: &[] }).placeholder("~/.ssh/id_ed25519-cert.pub").show_if("auth", &["certificate"]),
                    FieldSpec::text("jump_host", "Jump host").placeholder("bastion.example.com (ProxyJump)").advanced(),
                    FieldSpec::text("jump_port", "Jump port").kind(FieldKind::Number).default("22").advanced(),
                    FieldSpec::text("jump_user", "Jump user").placeholder("same as user").advanced(),
                    FieldSpec::text("jump_key", "Jump key").kind(FieldKind::File { save: false, filters: &[] }).placeholder("empty = ssh-agent").advanced().help("The jump host logs in with this key file, or the SSH agent when empty"),
                    FieldSpec::text("root", "Start folder").placeholder("home folder").advanced(),
                ],
            },
        }
    }
}

fn expand_home(p: &str) -> std::path::PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(std::path::PathBuf::from).unwrap_or_default().join(rest),
        None => std::path::PathBuf::from(p),
    }
}

#[async_trait]
impl Driver for SshDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    async fn connect(&self, p: ConnectParams) -> Result<SessionHandle> {
        let host = p.get_or("host", "localhost").to_string();
        let port = p.port(22)?;
        let user = p.get("user").ok_or_else(|| Error::invalid("User is required"))?.to_string();
        let secret = p.secret.clone().filter(|s| !s.is_empty());
        let key_file = || p.get("key_path").map(expand_home).ok_or_else(|| Error::invalid("Choose a private key file"));
        let auth = match p.get_or("auth", "password") {
            "keyboard_interactive" => SshAuth::KeyboardInteractive { password: secret.clone().unwrap_or_default(), otp: p.get("otp").map(str::to_owned) },
            "key" => SshAuth::Key { path: key_file()?, passphrase: secret.clone() },
            "key_text" => SshAuth::KeyText { pem: secret.clone().ok_or_else(|| Error::invalid("Paste the private key into the secret field"))?, passphrase: None },
            "certificate" => SshAuth::Certificate {
                key: key_file()?,
                cert: p.get("cert_path").map(expand_home).ok_or_else(|| Error::invalid("Choose the certificate file (*-cert.pub)"))?,
                passphrase: secret.clone(),
            },
            "agent" => SshAuth::Agent,
            "none" => SshAuth::None,
            _ => SshAuth::Password(secret.clone().unwrap_or_default()),
        };
        let target = SshTarget { host: host.clone(), port, user: user.clone(), auth, expected: p.get(FINGERPRINT).map(str::to_owned) };
        let timeout = Duration::from_secs(20);
        let (jump, (handle, fingerprint)) = match p.get("jump_host") {
            Some(jh) => {
                let jauth = match p.get("jump_key") {
                    Some(k) => SshAuth::Key { path: expand_home(k), passphrase: None },
                    None => SshAuth::Agent,
                };
                let jt = SshTarget {
                    host: jh.to_string(),
                    port: p.get("jump_port").and_then(|v| v.parse().ok()).unwrap_or(22),
                    user: p.get("jump_user").unwrap_or(&user).to_string(),
                    auth: jauth,
                    expected: p.get(JUMP_FINGERPRINT).map(str::to_owned),
                };
                let (j, _) = ssh_connect_via(None, &jt, timeout).await.map_err(|e| {
                    if openvclient_net::unknown_host_key(&e).is_some() {
                        // Same prefix, marked so the app pins it as the jump host's key.
                        Error { message: format!("{} {JUMP_MARK} {jh}", openvclient_net::UNKNOWN_HOST_KEY), ..e }
                    } else {
                        Error { message: format!("Jump host {jh}: {}", e.message), ..e }
                    }
                })?;
                let inner = ssh_connect_via(Some(&j), &target, timeout).await?;
                (Some(Arc::new(j)), inner)
            }
            None => (None, ssh_connect_via(None, &target, timeout).await?),
        };
        let handle = Arc::new(handle);
        let sftp = match open_sftp(&handle).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::info!(target: "openvclient::conn", %host, error = %e, "SFTP unavailable; terminal and commands still work");
                None
            }
        };
        let home = match (&sftp, p.get("root")) {
            (_, Some(r)) => r.to_string(),
            (Some(s), None) => s.canonicalize(".").await.unwrap_or_else(|_| "/".into()),
            (None, None) => "/".into(),
        };
        let uname = run(&handle, "uname -srm 2>/dev/null || ver").await.map(|(o, _)| o.trim().to_string()).unwrap_or_default();
        let info = ServerInfo {
            product: format!("SSH {user}@{host}"),
            version: String::new(),
            details: vec![
                ("System".into(), uname),
                ("Host key (SHA256)".into(), fingerprint),
                ("SFTP".into(), if sftp.is_some() { "available".into() } else { "not available".into() }),
                ("Start folder".into(), home.clone()),
            ],
        };
        Ok(SessionHandle::Resource(Arc::new(SshSession { handle, _jump: jump, sftp, home, read_only: p.read_only, info })))
    }
}

async fn open_sftp(h: &Handle<HostKeyCheck>) -> Result<SftpSession> {
    let ch = h.channel_open_session().await.map_err(|e| Error::connection(e.to_string()))?;
    ch.request_subsystem(true, "sftp").await.map_err(|e| Error::connection(e.to_string()))?;
    SftpSession::new(ch.into_stream()).await.map_err(|e| Error::connection(format!("SFTP: {e}")))
}

/// Run a command; returns combined output and the exit status.
async fn run(h: &Handle<HostKeyCheck>, cmd: &str) -> Result<(String, Option<u32>)> {
    let mut ch = h.channel_open_session().await.map_err(|e| Error::connection(e.to_string()))?;
    ch.exec(true, cmd).await.map_err(|e| Error::connection(e.to_string()))?;
    let mut out = Vec::new();
    let mut status = None;
    let fut = async {
        while let Some(m) = ch.wait().await {
            match m {
                ChannelMsg::Data { data } => out.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, .. } => out.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                ChannelMsg::Eof | ChannelMsg::Close if status.is_some() => {
                    break;
                }
                _ => {}
            }
            if out.len() > 8 * 1024 * 1024 {
                out.extend_from_slice(b"\n[output truncated at 8 MiB]");
                break;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(300), fut).await.map_err(|_| Error::other("Command timed out after 5 minutes"))?;
    Ok((String::from_utf8_lossy(&out).into_owned(), status))
}

struct SshSession {
    handle: Arc<Handle<HostKeyCheck>>,
    /// Keeps the jump-host session alive while connected through it.
    _jump: Option<Arc<Handle<HostKeyCheck>>>,
    sftp: Option<SftpSession>,
    home: String,
    read_only: bool,
    info: ServerInfo,
}

fn sftp_err(e: russh_sftp::client::error::Error) -> Error {
    match e {
        // "Permission denied: Permission denied" → "Permission denied"; bare
        // "Failure" gets the usual reasons, since servers rarely say more.
        russh_sftp::client::error::Error::Status(st) => {
            let code = st.status_code.to_string();
            let msg = st.error_message.trim();
            let text = if msg.is_empty() || msg.eq_ignore_ascii_case(&code) { code.clone() } else { format!("{code}: {msg}") };
            if code.eq_ignore_ascii_case("failure") && (msg.is_empty() || msg.eq_ignore_ascii_case("failure")) {
                Error::server("The server refused the operation (the item may be in use, not empty, or not allowed for this user)")
            } else {
                Error::server(text)
            }
        }
        other => Error::server(other.to_string()),
    }
}

/// Delete a folder and everything in it. Symlinks are removed, not followed.
fn remove_tree<'a>(sftp: &'a SftpSession, path: String, depth: usize) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64>> + Send + 'a>> {
    Box::pin(async move {
        if depth > 64 {
            return Err(Error::invalid(format!("{path} is nested too deeply to delete here")));
        }
        let mut removed = 0;
        let entries: Vec<_> = sftp.read_dir(path.clone()).await.map_err(sftp_err)?.filter(|e| e.file_name() != "." && e.file_name() != "..").collect();
        for e in entries {
            let child = join(&path, &e.file_name());
            let m = e.metadata();
            if m.is_dir() && !m.is_symlink() {
                removed += remove_tree(sftp, child, depth + 1).await?;
            } else {
                sftp.remove_file(child).await.map_err(sftp_err)?;
                removed += 1;
            }
        }
        sftp.remove_dir(path).await.map_err(sftp_err)?;
        Ok(removed)
    })
}

/// Breadcrumbs for absolute paths: "/", "/a", "/a/b" (folders only).
fn path_crumbs(node: &str, home: &str) -> Vec<(String, String)> {
    let (is_file, path) = match node.split_once(':') {
        Some(("f", p)) => (true, p),
        Some((_, p)) => (false, p),
        None => return Vec::new(),
    };
    let dir = if is_file { parent(path) } else { path.to_string() };
    let mut out = vec![("d:/".to_string(), "/".to_string())];
    let mut acc = String::new();
    for s in dir.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(s);
        out.push((format!("d:{acc}"), s.to_string()));
    }
    // Start the trail at the home folder when the path is inside it.
    if let Some(i) = out.iter().position(|(id, _)| id == &format!("d:{home}"))
        && i > 0
    {
        out.drain(..i);
        out[0].1 = format!("{} (home)", out[0].1);
    }
    out
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

fn parent(path: &str) -> String {
    match path.trim_end_matches('/').rsplit_once('/') {
        Some(("", _)) | None => "/".into(),
        Some((p, _)) => p.to_string(),
    }
}

/// Quote for a POSIX shell.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl SshSession {
    fn sftp(&self) -> Result<&SftpSession> {
        self.sftp.as_ref().ok_or_else(|| Error::unsupported("This server has no SFTP subsystem; use the terminal or console"))
    }
    fn guard(&self) -> Result<()> {
        if self.read_only { Err(Error::invalid("Connection is read-only")) } else { Ok(()) }
    }
    async fn ls(&self, dir: &str) -> Result<Vec<(String, bool, u64, Option<std::time::SystemTime>)>> {
        let sftp = self.sftp()?;
        let rd = sftp.read_dir(dir).await.map_err(sftp_err)?;
        let mut v = Vec::new();
        for e in rd.filter(|e| e.file_name() != "." && e.file_name() != "..") {
            let mut m = e.metadata();
            // readdir describes the link itself; show what it points to (a
            // broken link stays a small file).
            if m.is_symlink()
                && let Ok(target) = sftp.metadata(join(dir, &e.file_name())).await
            {
                m = target;
            }
            v.push((e.file_name(), m.is_dir(), m.len(), m.modified().ok()));
        }
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
        Ok(v)
    }
}

#[async_trait]
impl ServiceSession for SshSession {
    fn server_info(&self) -> ServerInfo {
        self.info.clone()
    }
    async fn ping(&self) -> Result<Duration> {
        let t = Instant::now();
        if self.handle.is_closed() {
            return Err(Error::connection("SSH connection closed"));
        }
        run(&self.handle, "true").await?;
        Ok(t.elapsed())
    }
    async fn close(&self) {
        if let Some(s) = &self.sftp {
            let _ = s.close().await;
        }
        let _ = self.handle.disconnect(russh::Disconnect::ByApplication, "", "en").await;
    }
}

#[async_trait]
impl ResourceSession for SshSession {
    fn file_browser(&self) -> bool {
        self.sftp.is_some()
    }

    async fn list_dir(&self, node: Option<&str>, filter: &str) -> Result<FileListing> {
        let dir = match node {
            None => self.home.clone(),
            Some(id) => id.strip_prefix("d:").ok_or_else(|| Error::invalid("Not a folder"))?.to_string(),
        };
        let f = filter.trim().to_lowercase();
        let entries = self
            .ls(&dir)
            .await?
            .into_iter()
            .filter(|e| f.is_empty() || e.0.to_lowercase().contains(&f))
            .map(|(name, is_dir, size, m)| {
                let path = join(&dir, &name);
                FileEntry {
                    id: format!("{}:{path}", if is_dir { "d" } else { "f" }),
                    name,
                    is_dir,
                    size: (!is_dir).then_some(size),
                    modified: m.map(chrono::DateTime::<chrono::Utc>::from),
                    detail: None,
                    kind: if is_dir { "dir" } else { "file" }.into(),
                }
            })
            .collect();
        Ok(FileListing { entries, truncated: false })
    }

    fn breadcrumbs(&self, node: &str) -> Vec<(String, String)> {
        path_crumbs(node, &self.home)
    }

    fn root_dir(&self) -> Option<String> {
        Some(format!("d:{}", self.home))
    }

    fn file_actions(&self) -> FileActions {
        FileActions { upload: Some("upload"), mkdir: Some("mkdir"), download: Some("download"), delete_file: Some("rm"), delete_dir: Some("rmdir"), rename: Some("rename"), share: None }
    }

    async fn children(&self, parent_id: Option<&str>) -> Result<Vec<Node>> {
        let Some(id) = parent_id else {
            let mut out = vec![Node::new("#terminal", "Terminal", "terminal", NodeIcon::Server).detail("interactive shell")];
            if self.sftp.is_some() {
                out.push(Node::new(format!("d:{}", self.home), format!("Files · {}", self.home), "dir", NodeIcon::Folder).folder());
                if self.home != "/" {
                    out.push(Node::new("d:/", "Files · /", "dir", NodeIcon::Folder).folder());
                }
            }
            return Ok(out);
        };
        let dir = id.strip_prefix("d:").ok_or_else(|| Error::invalid("Not a folder"))?;
        Ok(self
            .ls(dir)
            .await?
            .into_iter()
            .map(|(name, is_dir, size, _)| {
                let path = join(dir, &name);
                if is_dir {
                    Node::new(format!("d:{path}"), name, "dir", NodeIcon::Folder).folder()
                } else {
                    Node::new(format!("f:{path}"), name, "file", NodeIcon::File).detail(openvclient_core::containers::human_bytes(size))
                }
            })
            .collect())
    }

    async fn open(&self, node: &str, opts: OpenOptions) -> Result<Content> {
        if node == "#terminal" {
            return Ok(Content::Empty("Use “Open terminal” to start an interactive shell.".into()));
        }
        if let Some(dir) = node.strip_prefix("d:") {
            let f = opts.filter.trim().to_lowercase();
            let mut rs = ResultSet::new(vec![ColumnMeta::new("name", "text"), ColumnMeta::new("type", "text"), ColumnMeta::new("size", "int8"), ColumnMeta::new("modified", "text")]);
            for (name, is_dir, size, m) in self.ls(dir).await?.into_iter().filter(|e| f.is_empty() || e.0.to_lowercase().contains(&f)) {
                let modified = m.map(|t| Value::Text(chrono::DateTime::<chrono::Utc>::from(t).format("%Y-%m-%d %H:%M").to_string())).unwrap_or(Value::Null);
                rs.rows.push(vec![Value::Text(name), Value::Text(if is_dir { "folder" } else { "file" }.into()), if is_dir { Value::Null } else { Value::Int(size as i64) }, modified]);
            }
            let n = rs.rows.len();
            return Ok(Content::TableWithNote(rs, format!("{n} item(s) in {dir}")));
        }
        let path = node.strip_prefix("f:").ok_or_else(|| Error::invalid("Bad node"))?;
        let sftp = self.sftp()?;
        let meta = sftp.metadata(path).await.map_err(sftp_err)?;
        if openvclient_core::resource::is_image_name(path) && meta.len() <= openvclient_core::resource::IMAGE_PREVIEW_MAX {
            return Ok(Content::Image(sftp.read(path).await.map_err(sftp_err)?));
        }
        if openvclient_core::resource::is_pdf_name(path) && meta.len() <= openvclient_core::resource::IMAGE_PREVIEW_MAX {
            return Ok(Content::Pdf(sftp.read(path).await.map_err(sftp_err)?));
        }
        if meta.len() > PREVIEW_MAX {
            return Ok(Content::Empty(format!("{} is larger than 1 MiB; use Download", openvclient_core::containers::human_bytes(meta.len()))));
        }
        let data = sftp.read(path).await.map_err(sftp_err)?;
        if data.iter().take(4096).any(|b| *b == 0) {
            return Ok(Content::Empty("Binary file; use Download".into()));
        }
        Ok(Content::Text(String::from_utf8_lossy(&data).into_owned()))
    }

    fn filter_hint(&self, kind: &str) -> Option<&'static str> {
        (kind == "dir").then_some("Name contains…")
    }

    fn actions(&self, kind: &str) -> Vec<ActionSpec> {
        match kind {
            "dir" => vec![
                ActionSpec::new("upload", "Upload file…").primary().field(FieldSpec::text("file", "File").kind(FieldKind::File { save: false, filters: &[] }).required()),
                ActionSpec::new("mkdir", "New folder…").field(FieldSpec::text("name", "Name").required()),
                ActionSpec::new("rename", "Rename…").field(FieldSpec::text("name", "New name").required()),
                ActionSpec::new("rmdir", "Delete folder and its contents").destructive(),
            ],
            "file" => vec![
                ActionSpec::new("download", "Download…").primary().field(FieldSpec::text("file", "Save as").kind(FieldKind::File { save: true, filters: &[] }).required()),
                ActionSpec::new("rename", "Rename…").field(FieldSpec::text("name", "New name").required()),
                ActionSpec::new("chmod", "Permissions…").field(FieldSpec::text("mode", "Mode (octal)").default("644").required()),
                ActionSpec::new("rm", "Delete file").destructive(),
            ],
            _ => vec![],
        }
    }

    async fn run_action(&self, node: &str, action: &str, a: Args) -> Result<ActionOutcome> {
        if action != "download" {
            self.guard()?;
        }
        let path = node.split_once(':').map(|(_, p)| p.to_string()).ok_or_else(|| Error::invalid("Bad node"))?;
        let sftp = self.sftp()?;
        let up = format!("d:{}", parent(&path));
        match action {
            "upload" => {
                let local = std::path::PathBuf::from(arg(&a, "file")?);
                let name = local.file_name().map(|f| f.to_string_lossy().into_owned()).ok_or_else(|| Error::invalid("Pick a file"))?;
                // Streamed, so large files do not have to fit in memory.
                let mut src = tokio::fs::File::open(&local).await.map_err(|e| Error::io(format!("{}: {e}", local.display())))?;
                let mut f = sftp.create(join(&path, &name)).await.map_err(sftp_err)?;
                use tokio::io::AsyncWriteExt;
                let n = tokio::io::copy(&mut src, &mut f).await.map_err(|e| Error::io(format!("Upload of {name} failed: {e}")))?;
                f.shutdown().await.map_err(|e| Error::io(e.to_string()))?;
                Ok(ActionOutcome::msg(format!("Uploaded {name} ({})", openvclient_core::containers::human_bytes(n))).refresh(node.to_string()))
            }
            "mkdir" => {
                let name = arg(&a, "name")?.trim().to_string();
                sftp.create_dir(join(&path, &name)).await.map_err(sftp_err)?;
                Ok(ActionOutcome::msg(format!("Created {name}")).refresh(node.to_string()))
            }
            "rename" => {
                let to = join(&parent(&path), arg(&a, "name")?.trim());
                // SFTP servers answer a clash with a bare "Failure"; say what it is.
                if to != path && sftp.symlink_metadata(to.clone()).await.is_ok() {
                    return Err(Error::invalid(format!("{} already exists in this folder", arg(&a, "name")?.trim())));
                }
                sftp.rename(path.clone(), to.clone()).await.map_err(sftp_err)?;
                Ok(ActionOutcome::msg(format!("Renamed to {to}")).refresh(up))
            }
            "rmdir" => {
                // A symlink to a folder is removed as a link; its target is untouched.
                if sftp.symlink_metadata(path.clone()).await.map(|m| m.is_symlink()).unwrap_or(false) {
                    sftp.remove_file(path.clone()).await.map_err(sftp_err)?;
                    return Ok(ActionOutcome::msg(format!("Deleted link {path}")).refresh(up));
                }
                let n = remove_tree(sftp, path.clone(), 0).await?;
                let what = if n == 0 { String::new() } else { format!(" and {n} item{} inside", if n == 1 { "" } else { "s" }) };
                Ok(ActionOutcome::msg(format!("Deleted {path}{what}")).refresh(up))
            }
            "rm" => {
                sftp.remove_file(path.clone()).await.map_err(sftp_err)?;
                Ok(ActionOutcome::msg(format!("Deleted {path}")).refresh(up))
            }
            "chmod" => {
                let mode = arg(&a, "mode")?.trim();
                if !(3..=4).contains(&mode.len()) || !mode.chars().all(|c| ('0'..='7').contains(&c)) {
                    return Err(Error::invalid("Mode must be octal, e.g. 644 or 0755"));
                }
                let (out, status) = run(&self.handle, &format!("chmod {mode} {}", sh_quote(&path))).await?;
                if status != Some(0) {
                    return Err(Error::server(format!("chmod failed: {}", out.trim())));
                }
                Ok(ActionOutcome::msg(format!("Set {path} to {mode}")))
            }
            "download" => {
                let local = std::path::PathBuf::from(arg(&a, "file")?);
                let mut src = sftp.open(path.clone()).await.map_err(sftp_err)?;
                let mut dst = tokio::fs::File::create(&local).await.map_err(|e| Error::io(format!("{}: {e}", local.display())))?;
                let n = tokio::io::copy(&mut src, &mut dst).await.map_err(|e| Error::io(format!("Download failed: {e}")))?;
                use tokio::io::AsyncWriteExt;
                dst.flush().await.map_err(|e| Error::io(e.to_string()))?;
                Ok(ActionOutcome::msg(format!("Saved {} to {}", openvclient_core::containers::human_bytes(n), local.display())))
            }
            other => Err(Error::unsupported(format!("Unknown action {other}"))),
        }
    }

    fn console_language(&self) -> Option<&'static str> {
        Some("Shell command run on the server, e.g. df -h · for interactive programs use the terminal")
    }

    async fn console(&self, _node: Option<&str>, input: &str) -> Result<Content> {
        if self.read_only {
            return Err(Error::invalid("Remote commands are blocked on a read-only connection"));
        }
        let (out, status) = run(&self.handle, input.trim()).await?;
        let tail = match status {
            Some(0) => String::new(),
            Some(s) => format!("\n[exit status {s}]"),
            None => "\n[no exit status]".into(),
        };
        Ok(Content::Text(format!("{out}{tail}")))
    }

    fn supports_terminal(&self) -> bool {
        true
    }

    async fn open_terminal(&self, cols: u16, rows: u16) -> Result<Terminal> {
        if self.read_only {
            return Err(Error::invalid("The terminal is not available on a read-only connection"));
        }
        let ch = self.handle.channel_open_session().await.map_err(|e| Error::connection(e.to_string()))?;
        ch.request_pty(true, "xterm-256color", cols as u32, rows as u32, 0, 0, &[]).await.map_err(|e| Error::connection(e.to_string()))?;
        ch.request_shell(true).await.map_err(|e| Error::connection(e.to_string()))?;
        let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel::<TermInput>();
        let (out_tx, out_rx) = std::sync::mpsc::channel::<TermEvent>();
        tokio::spawn(async move {
            let (mut read, write) = ch.split();
            let reason = loop {
                tokio::select! {
                    msg = read.wait() => match msg {
                        Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                            if out_tx.send(TermEvent::Data(data.to_vec())).is_err() { break "closed".to_string(); }
                        }
                        Some(ChannelMsg::ExitStatus { exit_status }) => break format!("Shell exited with status {exit_status}"),
                        Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break "Session closed by the server".to_string(),
                        _ => {}
                    },
                    inp = in_rx.recv() => match inp {
                        Some(TermInput::Data(d)) => { if let Err(e) = write.data_bytes(d).await { break format!("Write failed: {e}"); } }
                        Some(TermInput::Resize { cols, rows }) => { let _ = write.window_change(cols as u32, rows as u32, 0, 0).await; }
                        None => { let _ = write.close().await; break "Terminal closed".to_string(); }
                    },
                }
            };
            let _ = out_tx.send(TermEvent::Closed(reason));
        });
        Ok(Terminal { input: in_tx, output: out_rx })
    }
}

/// Test helper: connect once to learn the (unknown) host key, then pin it,
/// the way the UI does after the user confirms it.
#[cfg(test)]
pub(crate) async fn trust_for_test(mut p: ConnectParams) -> std::result::Result<ConnectParams, Error> {
    match SshDriver::default().connect(p.clone()).await {
        Ok(_) => Ok(p),
        Err(e) => match openvclient_net::unknown_host_key(&e) {
            Some(fp) => {
                p.values.insert(FINGERPRINT.into(), fp);
                Ok(p)
            }
            None => Err(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
        assert_eq!(parent("/home/x/f"), "/home/x");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_ssh() {
        let mut v = std::collections::BTreeMap::new();
        v.insert("host".into(), "127.0.0.1".into());
        v.insert("port".into(), std::env::var("OPENVCLIENT_TEST_SSH_PORT").unwrap_or_else(|_| "52222".into()));
        v.insert("user".into(), std::env::var("OPENVCLIENT_TEST_SSH_USER").unwrap_or_else(|_| "tunnel".into()));
        v.insert("auth".into(), "password".into());
        let secret = Some(std::env::var("OPENVCLIENT_TEST_SSH_PASSWORD").unwrap_or_else(|_| "tunnelpw".into()));
        let p = match super::trust_for_test(ConnectParams { values: v, secret, read_only: false, notifier: None }).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skipping: no SSH server ({e})");
                return;
            }
        };
        let h = match SshDriver::default().connect(p).await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("skipping: no SSH server ({e})");
                return;
            }
        };
        let s = h.resource().unwrap().clone();
        let Content::Text(t) = s.console(None, "echo hello-$((40+2))").await.unwrap() else { panic!() };
        assert!(t.contains("hello-42"), "{t}");
        let top = s.children(None).await.unwrap();
        let home = top.iter().find(|n| n.kind == "dir").expect("sftp").id.clone();
        let src = std::env::temp_dir().join("ovc-ssh.txt");
        std::fs::write(&src, "over sftp").unwrap();
        let mut u = Args::new();
        u.insert("file".into(), src.display().to_string());
        s.run_action(&home, "upload", u).await.unwrap();
        let f = format!("f:{}", join(home.strip_prefix("d:").unwrap(), "ovc-ssh.txt"));
        let l = s.list_dir(Some(&home), "ovc-ssh").await.unwrap();
        assert!(l.entries.iter().any(|e| e.id == f && e.size == Some(9) && e.modified.is_some()), "{:?}", l.entries);
        assert!(s.file_browser() && s.root_dir().as_deref() == Some(home.as_str()));
        let Content::Text(t) = s.open(&f, OpenOptions::default()).await.unwrap() else { panic!() };
        assert_eq!(t, "over sftp");
        s.run_action(&f, "rm", Args::new()).await.unwrap();

        let term = s.open_terminal(80, 24).await.unwrap();
        term.input.send(TermInput::Data(b"echo term-$((6*7))\n".to_vec())).unwrap();
        let mut seen = String::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !seen.contains("term-42") {
            match term.output.recv_timeout(Duration::from_millis(200)) {
                Ok(TermEvent::Data(d)) => seen.push_str(&String::from_utf8_lossy(&d)),
                Ok(TermEvent::Closed(r)) => panic!("closed: {r}"),
                Err(_) => {}
            }
        }
        assert!(seen.contains("term-42"), "{seen}");
        drop(term);
        s.close().await;
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    fn base(auth: &str) -> std::collections::BTreeMap<String, String> {
        let mut v = std::collections::BTreeMap::new();
        v.insert("host".into(), "127.0.0.1".into());
        v.insert("port".into(), "52222".into());
        v.insert("user".into(), "tunnel".into());
        v.insert("auth".into(), auth.into());
        v
    }

    /// "Password" must also work on servers that take the password only through
    /// keyboard-interactive (PAM; Ubuntu, Mint and many others), like OpenSSH does.
    /// Server: the Ubuntu image with `PasswordAuthentication no`,
    /// `KbdInteractiveAuthentication yes`, user ovc/ovcpass, on port 52223.
    #[tokio::test]
    async fn password_falls_back_to_keyboard_interactive() {
        let p = |v, secret: &str| ConnectParams { values: v, secret: Some(secret.to_owned()), read_only: false, notifier: None };
        let mut v = base("password");
        v.insert("port".into(), std::env::var("OPENVCLIENT_SSH_PAM_PORT").unwrap_or_else(|_| "52223".into()));
        v.insert("user".into(), "ovc".into());
        let trusted = match super::trust_for_test(p(v.clone(), "ovcpass")).await {
            Ok(t) => t.values,
            Err(e) => {
                eprintln!("skipping: no PAM SSH server ({e})");
                return;
            }
        };
        let h = SshDriver::default().connect(p(trusted.clone(), "ovcpass")).await.expect("password login through keyboard-interactive");
        h.base().ping().await.unwrap();
        let e = SshDriver::default().connect(p(trusted, "wrong")).await.err().expect("wrong password refused");
        assert!(e.message.contains("authentication failed") && e.message.contains("keyboard-interactive"), "{e}");
    }

    #[tokio::test]
    async fn keyboard_interactive_and_jump_host() {
        let p = |v, secret: Option<&str>| ConnectParams { values: v, secret: secret.map(str::to_owned), read_only: false, notifier: None };
        let fp = match super::trust_for_test(p(base("password"), Some("tunnelpw"))).await {
            Ok(t) => t.values.get(FINGERPRINT).cloned().unwrap_or_default(),
            Err(e) => {
                eprintln!("skipping: no SSH server ({e})");
                return;
            }
        };
        let base = |auth: &str| {
            let mut v = base(auth);
            v.insert(FINGERPRINT.into(), fp.clone());
            v
        };
        // Without a pinned key the server is not trusted.
        let mut fresh = base("password");
        fresh.remove(FINGERPRINT);
        let e = SshDriver::default().connect(p(fresh, Some("tunnelpw"))).await.err().expect("unknown key refused");
        assert!(openvclient_net::unknown_host_key(&e).is_some(), "{e}");
        // Keyboard-interactive answers the password prompt (servers without it fail cleanly).
        match SshDriver::default().connect(p(base("keyboard_interactive"), Some("tunnelpw"))).await {
            Ok(_) => eprintln!("keyboard-interactive ok"),
            Err(e) => assert!(e.message.contains("authentication failed"), "{e}"),
        }
        assert!(SshDriver::default().connect(p(base("none"), None)).await.is_err(), "'none' must not log in");
        // Jump host: the server tunnels to itself, using a key when one is provided.
        if let Ok(key) = std::env::var("OPENVCLIENT_TEST_SSH_KEY") {
            let mut v = base("password");
            v.insert("host".into(), "127.0.0.1".into());
            v.insert("port".into(), "2222".into()); // the container's own sshd port
            v.insert("jump_host".into(), "127.0.0.1".into());
            v.insert("jump_port".into(), "52222".into());
            v.insert("jump_key".into(), key);
            let h = SshDriver::default().connect(p(v, Some("tunnelpw"))).await.expect("via jump host");
            let Content::Text(t) = h.resource().unwrap().console(None, "echo jumped").await.unwrap() else { panic!() };
            assert!(t.contains("jumped"));
        }
    }
}

#[cfg(test)]
#[path = "ssh_lab_tests.rs"]
mod ssh_lab_tests;
