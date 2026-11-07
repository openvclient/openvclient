//! FTP and FTPS (explicit TLS, AUTH TLS) file servers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use openvclient_core::resource::*;
use openvclient_core::{Category, ColumnMeta, ConnectParams, Driver, DriverInfo, Error, FieldKind, FieldSpec, Result, ResultSet, ServerInfo, ServiceSession, SessionHandle, Value};
use suppaftp::list::ListParser;
use suppaftp::tokio::{AsyncFtpStream, AsyncRustlsConnector, AsyncRustlsFtpStream};
use suppaftp::{FtpError, Status};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

const PREVIEW_MAX: usize = 1024 * 1024;

pub struct FtpDriver {
    info: DriverInfo,
}

impl Default for FtpDriver {
    fn default() -> Self {
        Self {
            info: DriverInfo {
                id: "ftp",
                name: "FTP / FTPS",
                category: Category::Storage,
                description: "FTP file servers, with optional explicit TLS (FTPS)",
                badge: "FTP",
                fields: vec![
                    FieldSpec::text("host", "Host").default("localhost").required(),
                    FieldSpec::text("port", "Port").kind(FieldKind::Number).default("21"),
                    FieldSpec::text("security", "Security")
                        .kind(FieldKind::Choice(&["ftps", "ftps_implicit", "plain"]))
                        .default("ftps")
                        .help("ftps: explicit TLS (AUTH TLS on port 21) · ftps_implicit: TLS from the first byte (usually port 990) · plain: unencrypted FTP, credentials sent in clear text"),
                    FieldSpec::text("login", "Login").kind(FieldKind::Choice(&["password", "anonymous"])).default("password").help("anonymous: user 'anonymous' with an e-mail style password"),
                    FieldSpec::text("user", "User").show_if("login", &["password"]),
                    FieldSpec::text("password", "Password").kind(FieldKind::Secret).show_if("login", &["password"]),
                    FieldSpec::text("verify_cert", "Verify certificate")
                        .kind(FieldKind::Bool)
                        .default("true")
                        .show_if("security", &["ftps", "ftps_implicit"])
                        .help("Turn off only for servers with self-signed certificates you trust"),
                    FieldSpec::text("client_cert", "Client certificate (PEM)")
                        .kind(FieldKind::File { save: false, filters: &["pem", "crt"] })
                        .advanced()
                        .help("For servers that require TLS client authentication"),
                    FieldSpec::text("client_key", "Client key (PEM)").kind(FieldKind::File { save: false, filters: &["pem", "key"] }).advanced(),
                    FieldSpec::text("root", "Start folder").placeholder("login folder"),
                    FieldSpec::text("active", "Active mode").kind(FieldKind::Bool).default("false").advanced().help("Off = passive mode (works through NAT and firewalls)"),
                ],
            },
        }
    }
}

/// Accepts any server certificate; only used when the user turns
/// verification off for a self-signed server.
#[derive(Debug)]
struct NoVerify(Arc<tokio_rustls::rustls::crypto::CryptoProvider>);

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
        _: &tokio_rustls::rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> std::result::Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error> {
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        d: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<tokio_rustls::rustls::client::danger::HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        tokio_rustls::rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        d: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<tokio_rustls::rustls::client::danger::HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        tokio_rustls::rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

enum Conn {
    Plain(AsyncFtpStream),
    Tls(AsyncRustlsFtpStream),
}

/// Run the same code on either stream type.
macro_rules! on {
    ($c:expr, $f:ident => $body:expr) => {
        match $c {
            Conn::Plain($f) => $body,
            Conn::Tls($f) => $body,
        }
    };
}

#[derive(Clone, Copy, PartialEq)]
enum Security {
    Plain,
    Explicit,
    Implicit,
}

#[derive(Clone)]
struct Target {
    host: String,
    port: u16,
    security: Security,
    verify: bool,
    client_cert: Option<(std::path::PathBuf, std::path::PathBuf)>,
    user: String,
    password: String,
    active: bool,
}

fn ftp_err(e: FtpError) -> Error {
    match &e {
        FtpError::ConnectionError(io) => Error::connection(io.to_string()),
        FtpError::UnexpectedResponse(r) => {
            let body = String::from_utf8_lossy(&r.body).trim().to_string();
            match r.status.code() {
                530 => Error::auth(format!("Login refused: {body}")),
                550 => Error::server(format!("Not found or no permission: {body}")),
                553 => Error::server(format!("File name not allowed: {body}")),
                _ => Error::server(body),
            }
        }
        FtpError::SecureError(m) => Error::connection(format!("TLS: {m}")),
        _ => Error::other(e.to_string()),
    }
}

impl Target {
    fn tls_config(&self) -> Result<tokio_rustls::rustls::ClientConfig> {
        use tokio_rustls::rustls;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions().map_err(|e| Error::other(e.to_string()))?;
        let builder = if self.verify {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            builder.with_root_certificates(roots)
        } else {
            builder.dangerous().with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
        };
        match &self.client_cert {
            None => Ok(builder.with_no_client_auth()),
            Some((cert, key)) => {
                use rustls::pki_types::pem::PemObject;
                let certs = rustls::pki_types::CertificateDer::pem_file_iter(cert)
                    .and_then(|i| i.collect::<std::result::Result<Vec<_>, _>>())
                    .map_err(|e| Error::invalid(format!("Client certificate {}: {e}", cert.display())))?;
                let key = rustls::pki_types::PrivateKeyDer::from_pem_file(key).map_err(|e| Error::invalid(format!("Client key {}: {e}", key.display())))?;
                builder.with_client_auth_cert(certs, key).map_err(|e| Error::invalid(format!("Client certificate: {e}")))
            }
        }
    }

    async fn open(&self) -> Result<Conn> {
        let addr = format!("{}:{}", self.host, self.port);
        let fut = async {
            let mut c = if self.security == Security::Plain {
                Conn::Plain(AsyncFtpStream::connect(&addr).await.map_err(ftp_err)?)
            } else {
                let connector = AsyncRustlsConnector::from(tokio_rustls::TlsConnector::from(Arc::new(self.tls_config()?)));
                let refused = |e: FtpError| match e {
                    FtpError::UnexpectedResponse(_) => Error::connection("The server does not support FTPS (AUTH TLS). Try implicit FTPS on port 990, or 'plain' only on a trusted network."),
                    other => ftp_err(other),
                };
                if self.security == Security::Implicit {
                    #[allow(deprecated)]
                    let s = AsyncRustlsFtpStream::connect_secure_implicit(&addr, connector, &self.host).await.map_err(ftp_err)?;
                    Conn::Tls(s)
                } else {
                    let stream = AsyncRustlsFtpStream::connect(&addr).await.map_err(ftp_err)?;
                    Conn::Tls(stream.into_secure(connector, &self.host).await.map_err(refused)?)
                }
            };
            on!(&mut c, f => {
                f.login(self.user.as_str(), self.password.as_str()).await.map_err(ftp_err)?;
                f.transfer_type(suppaftp::types::FileType::Binary).await.map_err(ftp_err)?;
            });
            if self.active {
                c = match c {
                    Conn::Plain(f) => Conn::Plain(f.active_mode(Duration::from_secs(30))),
                    Conn::Tls(f) => Conn::Tls(f.active_mode(Duration::from_secs(30))),
                };
            }
            Ok::<_, Error>(c)
        };
        tokio::time::timeout(Duration::from_secs(20), fut).await.map_err(|_| Error::connection(format!("Timed out connecting to FTP {addr}")))?
    }
}

pub struct FtpSession {
    target: Target,
    conn: Mutex<Option<Conn>>,
    home: String,
    read_only: bool,
    info: ServerInfo,
}

#[async_trait]
impl Driver for FtpDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    async fn connect(&self, p: ConnectParams) -> Result<SessionHandle> {
        let security = match p.get_or("security", "ftps") {
            "plain" => Security::Plain,
            "ftps_implicit" => Security::Implicit,
            _ => Security::Explicit,
        };
        let anonymous = p.get("login") == Some("anonymous") || p.get("user").is_none();
        let target = Target {
            host: p.get_or("host", "localhost").to_string(),
            port: p.port(if security == Security::Implicit { 990 } else { 21 })?,
            security,
            verify: p.get("verify_cert") != Some("false"),
            client_cert: match (p.get("client_cert"), p.get("client_key")) {
                (Some(c), Some(k)) => Some((c.into(), k.into())),
                (Some(_), None) | (None, Some(_)) => return Err(Error::invalid("Give both the client certificate and its key")),
                _ => None,
            },
            user: if anonymous { "anonymous".into() } else { p.get_or("user", "anonymous").to_string() },
            password: if anonymous { "openvclient@".into() } else { p.secret.clone().unwrap_or_default() },
            active: p.flag("active"),
        };
        let mut c = target.open().await?;
        let (welcome, pwd, syst) = on!(&mut c, f => {
            let w = f.get_welcome_msg().unwrap_or("").trim().to_string();
            let pwd = f.pwd().await.map_err(ftp_err)?;
            let syst = f.custom_command("SYST", &[Status::Name]).await.map(|r| String::from_utf8_lossy(&r.body).trim().to_string()).unwrap_or_default();
            (w, pwd, syst)
        });
        let home = p.get("root").map(str::to_owned).unwrap_or(pwd);
        let info = ServerInfo {
            product: if target.security == Security::Plain { "FTP server".into() } else { "FTPS server".into() },
            version: String::new(),
            details: vec![
                ("Welcome".into(), welcome),
                ("System".into(), syst),
                (
                    "Security".into(),
                    match target.security {
                        Security::Plain => "none (clear text)".into(),
                        Security::Explicit => format!("explicit TLS{}", if target.verify { "" } else { ", certificate not verified" }),
                        Security::Implicit => format!("implicit TLS{}", if target.verify { "" } else { ", certificate not verified" }),
                    },
                ),
                ("Start folder".into(), home.clone()),
            ],
        };
        Ok(SessionHandle::Resource(Arc::new(FtpSession { target, conn: Mutex::new(Some(c)), home, read_only: p.read_only, info })))
    }
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

struct Entry {
    name: String,
    dir: bool,
    size: u64,
    modified: Option<String>,
    mtime: Option<chrono::DateTime<chrono::Utc>>,
}

fn parse_listing(lines: &[String]) -> Vec<Entry> {
    lines
        .iter()
        .filter_map(|l| ListParser::parse_posix(l).or_else(|_| ListParser::parse_dos(l)).ok())
        .filter(|f| f.name() != "." && f.name() != "..")
        .map(|f| Entry {
            name: f.name().to_string(),
            dir: f.is_directory(),
            size: f.size() as u64,
            modified: Some(chrono::DateTime::<chrono::Utc>::from(f.modified()).format("%Y-%m-%d %H:%M").to_string()),
            mtime: Some(chrono::DateTime::<chrono::Utc>::from(f.modified())),
        })
        .collect()
}

impl FtpSession {
    /// Run `op` on the cached connection, reconnecting once if it dropped.
    async fn with<T, F>(&self, op: F) -> Result<T>
    where
        F: for<'a> Fn(&'a mut Conn) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send + 'a>>,
    {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(self.target.open().await?);
        }
        match op(guard.as_mut().unwrap()).await {
            Err(e) if e.is_connection() => {
                *guard = Some(self.target.open().await?);
                op(guard.as_mut().unwrap()).await
            }
            r => r,
        }
    }

    fn guard(&self) -> Result<()> {
        if self.read_only { Err(Error::invalid("Connection is read-only")) } else { Ok(()) }
    }

    async fn ls(&self, dir: &str) -> Result<Vec<Entry>> {
        let d = dir.to_string();
        let lines = self
            .with(move |c| {
                let d = d.clone();
                Box::pin(async move { on!(c, f => f.list(Some(&d)).await.map_err(ftp_err)) })
            })
            .await?;
        let mut v = parse_listing(&lines);
        v.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(v)
    }

    async fn read(&self, path: &str, max: Option<usize>) -> Result<Vec<u8>> {
        let p = path.to_string();
        self.with(move |c| {
            let p = p.clone();
            Box::pin(async move {
                on!(c, f => {
                    let mut s = f.retr_as_stream(&p).await.map_err(ftp_err)?;
                    let mut buf = Vec::new();
                    let mut chunk = vec![0u8; 64 * 1024];
                    loop {
                        let n = s.read(&mut chunk).await.map_err(|e| Error::io(e.to_string()))?;
                        if n == 0 { break; }
                        buf.extend_from_slice(&chunk[..n]);
                        if max.is_some_and(|m| buf.len() > m) { break; }
                    }
                    if max.is_some_and(|m| buf.len() > m) {
                        let _ = f.abort(s).await;
                    } else {
                        s.finish().await.map_err(ftp_err)?;
                    }
                    Ok(buf)
                })
            })
        })
        .await
    }

    async fn simple(&self, cmd: &'static str, a: String, b: String) -> Result<()> {
        self.with(move |c| {
            let (a, b) = (a.clone(), b.clone());
            Box::pin(async move {
                on!(c, f => match cmd {
                    "mkdir" => f.mkdir(&a).await,
                    "rmdir" => f.rmdir(&a).await,
                    "rm" => f.rm(&a).await,
                    _ => f.rename(&a, &b).await,
                })
                .map_err(ftp_err)
            })
        })
        .await
    }
}

#[async_trait]
impl ServiceSession for FtpSession {
    fn server_info(&self) -> ServerInfo {
        self.info.clone()
    }
    async fn ping(&self) -> Result<Duration> {
        let t = Instant::now();
        self.with(|c| Box::pin(async move { on!(c, f => f.noop().await.map_err(ftp_err)) })).await?;
        Ok(t.elapsed())
    }
    async fn close(&self) {
        if let Some(mut c) = self.conn.lock().await.take() {
            let _ = on!(&mut c, f => f.quit().await);
        }
    }
}

#[async_trait]
impl ResourceSession for FtpSession {
    fn file_browser(&self) -> bool {
        true
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
            .filter(|e| f.is_empty() || e.name.to_lowercase().contains(&f))
            .map(|e| {
                let path = join(&dir, &e.name);
                FileEntry {
                    id: format!("{}:{path}", if e.dir { "d" } else { "f" }),
                    name: e.name,
                    is_dir: e.dir,
                    size: (!e.dir).then_some(e.size),
                    modified: e.mtime,
                    detail: None,
                    kind: if e.dir { "dir" } else { "file" }.into(),
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
        let dir = match parent_id {
            None => self.home.clone(),
            Some(id) => id.strip_prefix("d:").ok_or_else(|| Error::invalid("Not a folder"))?.to_string(),
        };
        let entries = self.ls(&dir).await?;
        let mut out = Vec::new();
        if parent_id.is_none() {
            out.push(Node::new(format!("d:{dir}"), format!("{dir} (start folder)"), "dir", NodeIcon::Folder).detail("open to list"));
        }
        out.extend(entries.into_iter().map(|e| {
            let path = join(&dir, &e.name);
            if e.dir {
                Node::new(format!("d:{path}"), e.name, "dir", NodeIcon::Folder).folder()
            } else {
                Node::new(format!("f:{path}"), e.name, "file", NodeIcon::File).detail(openvclient_core::containers::human_bytes(e.size))
            }
        }));
        Ok(out)
    }

    async fn open(&self, node: &str, opts: OpenOptions) -> Result<Content> {
        if let Some(dir) = node.strip_prefix("d:") {
            let f = opts.filter.trim().to_lowercase();
            let mut rs = ResultSet::new(vec![ColumnMeta::new("name", "text"), ColumnMeta::new("type", "text"), ColumnMeta::new("size", "int8"), ColumnMeta::new("modified", "text")]);
            for e in self.ls(dir).await?.into_iter().filter(|e| f.is_empty() || e.name.to_lowercase().contains(&f)) {
                rs.rows.push(vec![
                    Value::Text(e.name),
                    Value::Text(if e.dir { "folder" } else { "file" }.into()),
                    if e.dir { Value::Null } else { Value::Int(e.size as i64) },
                    e.modified.map(Value::Text).unwrap_or(Value::Null),
                ]);
            }
            let n = rs.rows.len();
            return Ok(Content::TableWithNote(rs, format!("{n} item(s) in {dir}")));
        }
        let path = node.strip_prefix("f:").ok_or_else(|| Error::invalid("Bad node"))?;
        if openvclient_core::resource::is_image_name(path) {
            let max = openvclient_core::resource::IMAGE_PREVIEW_MAX as usize;
            let data = self.read(path, Some(max)).await?;
            if data.len() <= max {
                return Ok(Content::Image(data));
            }
        }
        if openvclient_core::resource::is_pdf_name(path) {
            let max = openvclient_core::resource::IMAGE_PREVIEW_MAX as usize;
            let data = self.read(path, Some(max)).await?;
            if data.len() <= max {
                return Ok(Content::Pdf(data));
            }
        }
        let data = self.read(path, Some(PREVIEW_MAX)).await?;
        if data.len() > PREVIEW_MAX {
            return Ok(Content::Empty("File is larger than 1 MiB; use Download".into()));
        }
        if data.iter().take(4096).any(|b| *b == 0) {
            return Ok(Content::Empty(format!("Binary file ({}); use Download", openvclient_core::containers::human_bytes(data.len() as u64))));
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
                ActionSpec::new("rmdir", "Delete folder (must be empty)").destructive(),
            ],
            "file" => vec![
                ActionSpec::new("download", "Download…").primary().field(FieldSpec::text("file", "Save as").kind(FieldKind::File { save: true, filters: &[] }).required()),
                ActionSpec::new("rename", "Rename…").field(FieldSpec::text("name", "New name").required()),
                ActionSpec::new("rm", "Delete file").destructive(),
            ],
            _ => vec![],
        }
    }

    async fn run_action(&self, node: &str, action: &str, a: Args) -> Result<ActionOutcome> {
        if action != "download" {
            self.guard()?;
        }
        let (is_dir, path) = match node.split_once(':') {
            Some(("d", p)) => (true, p.to_string()),
            Some(("f", p)) => (false, p.to_string()),
            _ => return Err(Error::invalid("Bad node")),
        };
        let refresh_parent = format!("d:{}", parent(&path));
        match action {
            "upload" => {
                let local = std::path::PathBuf::from(arg(&a, "file")?);
                let name = local.file_name().map(|f| f.to_string_lossy().into_owned()).ok_or_else(|| Error::invalid("Pick a file"))?;
                let remote = join(&path, &name);
                let bytes = tokio::fs::read(&local).await.map_err(|e| Error::io(format!("{}: {e}", local.display())))?;
                let n = self
                    .with(move |c| {
                        let (remote, bytes) = (remote.clone(), bytes.clone());
                        Box::pin(async move { on!(c, f => f.put_file(&remote, &mut bytes.as_slice()).await.map_err(ftp_err)) })
                    })
                    .await?;
                Ok(ActionOutcome::msg(format!("Uploaded {name} ({})", openvclient_core::containers::human_bytes(n))).refresh(format!("d:{path}")))
            }
            "mkdir" => {
                let name = arg(&a, "name")?.trim().to_string();
                self.simple("mkdir", join(&path, &name), String::new()).await?;
                Ok(ActionOutcome::msg(format!("Created {name}")).refresh(format!("d:{path}")))
            }
            "rename" => {
                let to = join(&parent(&path), arg(&a, "name")?.trim());
                self.simple("rename", path.clone(), to.clone()).await?;
                Ok(ActionOutcome::msg(format!("Renamed to {to}")).refresh(refresh_parent))
            }
            "rmdir" if is_dir => {
                self.simple("rmdir", path.clone(), String::new()).await?;
                Ok(ActionOutcome::msg(format!("Deleted {path}")).refresh(refresh_parent))
            }
            "rm" => {
                self.simple("rm", path.clone(), String::new()).await?;
                Ok(ActionOutcome::msg(format!("Deleted {path}")).refresh(refresh_parent))
            }
            "download" => {
                let local = std::path::PathBuf::from(arg(&a, "file")?);
                let data = self.read(&path, None).await?;
                let mut f = tokio::fs::File::create(&local).await.map_err(|e| Error::io(format!("{}: {e}", local.display())))?;
                f.write_all(&data).await.map_err(|e| Error::io(e.to_string()))?;
                Ok(ActionOutcome::msg(format!("Saved {} to {}", openvclient_core::containers::human_bytes(data.len() as u64), local.display())))
            }
            other => Err(Error::unsupported(format!("Unknown action {other}"))),
        }
    }

    fn console_language(&self) -> Option<&'static str> {
        Some("Raw FTP control command, e.g. STAT, SYST, FEAT, SITE CHMOD 644 file.txt")
    }

    async fn console(&self, _node: Option<&str>, input: &str) -> Result<Content> {
        let cmd = input.trim().to_string();
        if cmd.is_empty() || cmd.contains(['\r', '\n']) {
            return Err(Error::invalid("Write one command on a single line"));
        }
        let verb = cmd.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
        if matches!(verb.as_str(), "USER" | "PASS" | "QUIT" | "PASV" | "PORT" | "EPSV" | "EPRT" | "RETR" | "STOR" | "LIST" | "NLST" | "MLSD" | "AUTH") {
            return Err(Error::invalid(format!("{verb} is managed by the connection; use the tree and actions instead")));
        }
        if self.read_only && matches!(verb.as_str(), "DELE" | "RMD" | "MKD" | "RNFR" | "RNTO" | "SITE") {
            return Err(Error::invalid(format!("{verb} is blocked on a read-only connection")));
        }
        let r = self
            .with(move |c| {
                let cmd = cmd.clone();
                Box::pin(async move {
                    let all: Vec<Status> = (100..600).map(Status::from).collect();
                    on!(c, f => f.custom_command(cmd, &all).await.map_err(ftp_err))
                })
            })
            .await?;
        Ok(Content::Text(format!("{} {}", r.status.code(), String::from_utf8_lossy(&r.body).trim())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(join("/", "a"), "/a");
        assert_eq!(join("/x", "a"), "/x/a");
        assert_eq!(parent("/x/a"), "/x");
        assert_eq!(parent("/a"), "/");
        let e = parse_listing(&["drwxr-xr-x    2 1000     1000         4096 Nov 05 10:00 docs".into(), "-rw-r--r--    1 1000     1000           12 Nov 05 10:00 a.txt".into()]);
        assert!(e[0].dir && e[0].name == "docs" && !e[1].dir && e[1].size == 12);
    }

    #[tokio::test]
    /// Explicit FTPS (AUTH TLS): login, list, upload, download, delete over TLS.
    /// Server: `stilliard/pure-ftpd` with `--tls=2` (TLS required), user ovc/ovcpass on port 52021.
    async fn live_ftps_explicit() {
        let mut v = std::collections::BTreeMap::new();
        v.insert("host".into(), "127.0.0.1".into());
        v.insert("port".into(), std::env::var("OPENVCLIENT_FTPS_PORT").unwrap_or_else(|_| "52021".into()));
        v.insert("security".into(), "ftps".into());
        v.insert("verify_cert".into(), "false".into()); // the test server's certificate is self-signed
        v.insert("user".into(), "ovc".into());
        let h = match FtpDriver::default().connect(ConnectParams { values: v.clone(), secret: Some("ovcpass".into()), read_only: false, notifier: None }).await {
            Ok(h) => h,
            Err(e) if e.message.contains("refused") || e.message.contains("reach") || e.message.contains("timed out") => {
                eprintln!("skipping: no FTPS server ({})", e.message);
                return;
            }
            Err(e) => panic!("FTPS connect failed: {} {:?}", e.message, e.detail),
        };
        let s = h.resource().unwrap().clone();
        let home = s.children(None).await.unwrap()[0].id.clone();
        let file = format!("{home}/ovc_tls.txt").replace("d:", "f:");
        let _ = s.run_action(&file, "rm", Args::new()).await;
        let src = std::env::temp_dir().join("ovc_tls.txt");
        std::fs::write(&src, "hello ftps").unwrap();
        let mut u = Args::new();
        u.insert("file".into(), src.display().to_string());
        s.run_action(&home, "upload", u).await.unwrap();
        let listing = s.list_dir(Some(&home), "").await.unwrap();
        assert!(listing.entries.iter().any(|e| e.name == "ovc_tls.txt"), "{:?}", listing.entries.iter().map(|e| &e.name).collect::<Vec<_>>());
        let out = std::env::temp_dir().join("ovc_tls_down.txt");
        let _ = std::fs::remove_file(&out);
        let mut d = Args::new();
        d.insert("file".into(), out.display().to_string());
        s.run_action(&file, "download", d).await.unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "hello ftps");
        s.run_action(&file, "rm", Args::new()).await.unwrap();

        // With certificate checks on, a self-signed certificate is refused.
        v.insert("verify_cert".into(), "true".into());
        assert!(
            FtpDriver::default().connect(ConnectParams { values: v, secret: Some("ovcpass".into()), read_only: false, notifier: None }).await.is_err(),
            "self-signed certificate must be refused when verifying"
        );
    }

    #[tokio::test]
    async fn live_ftp() {
        let mut v = std::collections::BTreeMap::new();
        v.insert("host".into(), "127.0.0.1".into());
        v.insert("port".into(), std::env::var("OPENVCLIENT_FTP_PORT").unwrap_or_else(|_| "52121".into()));
        v.insert("security".into(), "plain".into());
        v.insert("user".into(), "ovc".into());
        let h = match FtpDriver::default().connect(ConnectParams { values: v, secret: Some("ovcpass".into()), read_only: false, notifier: None }).await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("skipping: no FTP server ({e})");
                return;
            }
        };
        let s = h.resource().unwrap().clone();
        let home = s.children(None).await.unwrap()[0].id.clone();
        let _ = s.run_action(&format!("{home}/ovc_dir/hello.txt").replace("d:", "f:"), "rm", Args::new()).await;
        let _ = s.run_action(&format!("{home}/ovc_dir"), "rmdir", Args::new()).await;
        let mut m = Args::new();
        m.insert("name".into(), "ovc_dir".into());
        s.run_action(&home, "mkdir", m).await.unwrap();
        let src = std::env::temp_dir().join("hello.txt");
        std::fs::write(&src, "hello ftp").unwrap();
        let dir = format!("{home}/ovc_dir");
        let mut u = Args::new();
        u.insert("file".into(), src.display().to_string());
        s.run_action(&dir, "upload", u).await.unwrap();
        let l = s.list_dir(Some(&dir), "").await.unwrap();
        let f = l.entries.iter().find(|e| e.name == "hello.txt").expect("listed");
        assert!(!f.is_dir && f.size == Some(9), "{f:?}");
        assert!(s.root_dir().is_some() && !s.breadcrumbs(&f.id).is_empty());
        let kids = s.children(Some(&dir)).await.unwrap();
        let file = kids.iter().find(|n| n.label == "hello.txt").expect("uploaded").id.clone();
        let Content::Text(t) = s.open(&file, OpenOptions::default()).await.unwrap() else { panic!("text") };
        assert_eq!(t, "hello ftp");
        assert!(matches!(s.console(None, "SYST").await.unwrap(), Content::Text(_)));
        s.run_action(&file, "rm", Args::new()).await.unwrap();
        s.run_action(&dir, "rmdir", Args::new()).await.unwrap();
        s.close().await;
    }
}
