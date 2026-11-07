//! Azure Blob Storage (and ADLS Gen2 through its blob endpoint), with the
//! same ways to sign in as Azure Storage Explorer:
//! account name + key, connection string, SAS URL / token, Microsoft Entra ID
//! (interactive device code, service principal, managed identity, Azure CLI /
//! default chain, user name + password), anonymous public access, and the
//! Azurite emulator.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use openvclient_core::resource::*;
use openvclient_core::{Category, ColumnMeta, ConnectParams, Driver, DriverInfo, Error, FieldKind, FieldSpec, Notifier, Result, ResultSet, ServerInfo, ServiceSession, SessionHandle, Value};
use openvclient_net::entra::{self, AZURE_STORAGE, DEFAULT_PUBLIC_CLIENT, Flow};
use parking_lot::Mutex;
use sha2::Sha256;

const VERSION: &str = "2023-11-03";
const PREVIEW_MAX: u64 = 1024 * 1024;
/// Azurite's published development account (not a secret).
const DEV_ACCOUNT: &str = "devstoreaccount1";
const DEV_KEY: &str = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

const AUTH: &[&str] = &["account_key", "connection_string", "sas", "entra_interactive", "entra_service_principal", "entra_managed_identity", "entra_default", "entra_password", "anonymous", "azurite"];

pub struct AzureBlobDriver {
    info: DriverInfo,
}

impl Default for AzureBlobDriver {
    fn default() -> Self {
        Self {
            info: DriverInfo {
                id: "azure_blob",
                name: "Azure Blob Storage",
                category: Category::Storage,
                description: "Azure Blob Storage and ADLS Gen2 containers",
                badge: "AZ",
                fields: vec![
                    FieldSpec::text("auth", "Authentication").kind(FieldKind::Choice(AUTH)).default("account_key").help(
                        "account_key: account name + access key · connection_string · sas: SAS URL or token · entra_interactive: sign in with a browser code · entra_service_principal: app id + secret · entra_managed_identity: Azure-hosted identity · entra_default: env vars, managed identity, then Azure CLI · entra_password: user + password (no MFA) · anonymous: public containers · azurite: local emulator",
                    ),
                    FieldSpec::text("account", "Storage account").placeholder("mystorageacct").show_if("auth", &["account_key", "entra_interactive", "entra_service_principal", "entra_managed_identity", "entra_default", "entra_password", "anonymous"]),
                    FieldSpec::text("user", "User").placeholder("user@tenant.com").show_if("auth", &["entra_password"]),
                    FieldSpec::text("password", "Key / connection string / SAS / secret").kind(FieldKind::Secret).show_if("auth", &["account_key", "connection_string", "sas", "entra_service_principal", "entra_password"]).help(
                        "account_key: the account key · connection_string: the full string · sas: a SAS URL (https://acct.blob.core.windows.net/container?sv=…) or ?sv=… token · service principal: client secret · entra_password: the password",
                    ),
                    FieldSpec::text("tenant_id", "Tenant ID").placeholder("organizations").show_if("auth", &["entra_interactive", "entra_service_principal", "entra_password"]),
                    FieldSpec::text("client_id", "Client (application) ID").placeholder("default public client").show_if("auth", &["entra_interactive", "entra_service_principal", "entra_managed_identity", "entra_password"]),
                    FieldSpec::text("container", "Container").placeholder("all containers").help("Limit the tree to one container (needed for container-level SAS and anonymous access)"),
                    FieldSpec::text("cloud", "Cloud").kind(FieldKind::Choice(&["public", "china", "usgov"])).default("public").advanced(),
                    FieldSpec::text("endpoint", "Blob endpoint").placeholder("https://acct.blob.core.windows.net (overrides account/cloud)").advanced(),
                    FieldSpec::text("azurite_url", "Azurite URL").default("http://127.0.0.1:10000").show_if("auth", &["azurite"]),
                ],
            },
        }
    }
}

#[derive(Clone)]
enum Auth {
    SharedKey(Vec<u8>),
    Sas(String),
    Entra(Flow),
    Anonymous,
}

struct Az {
    /// Endpoint without trailing slash, e.g. https://acct.blob.core.windows.net
    /// or http://127.0.0.1:10000/devstoreaccount1 (Azurite, path-style).
    base: String,
    account: String,
    auth: Auth,
    client: reqwest::Client,
    token: Mutex<entra::Cache>,
    notifier: Option<Notifier>,
}

fn suffix(cloud: &str) -> &'static str {
    match cloud {
        "china" => "core.chinacloudapi.cn",
        "usgov" => "core.usgovcloudapi.net",
        _ => "core.windows.net",
    }
}

/// Percent-encode a blob path, keeping '/' separators.
fn enc_path(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn enc_q(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// Parse `Key=Value;…` connection strings.
fn parse_conn_string(cs: &str) -> std::collections::HashMap<String, String> {
    cs.split(';')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
        })
        .collect()
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Inner text of every `<tag>…</tag>` (non-nested).
fn tags<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    xml.split(&open).skip(1).filter_map(|s| s.split(&close).next()).collect()
}

fn tag<'a>(xml: &'a str, t: &str) -> Option<&'a str> {
    tags(xml, t).into_iter().next()
}

fn az_err(status: u16, body: &str, auth: &Auth) -> Error {
    let code = tag(body, "Code").unwrap_or("").to_string();
    let msg = tag(body, "Message").map(|m| m.lines().next().unwrap_or("").to_string()).unwrap_or_default();
    let text = match (code.is_empty(), msg.is_empty()) {
        (true, true) => format!("HTTP {status}"),
        (false, true) => code.clone(),
        _ => format!("{code}: {msg}"),
    };
    match (status, code.as_str()) {
        (403, "AuthorizationPermissionMismatch") if matches!(auth, Auth::Entra(_)) => {
            Error::auth(format!("{text}. Your Entra identity needs a data role such as 'Storage Blob Data Reader' or 'Contributor' on the account; management roles like Owner are not enough."))
        }
        (403, _) | (401, _) => Error::auth(text),
        (404, "ContainerNotFound" | "BlobNotFound" | "ResourceNotFound") => Error::server(text),
        _ => Error::server(text),
    }
}

impl Az {
    fn from_params(p: &ConnectParams) -> Result<Self> {
        let secret = p.secret.clone().unwrap_or_default();
        let cloud = suffix(p.get_or("cloud", "public"));
        let account_base = |acct: &str| p.get("endpoint").map(|e| e.trim_end_matches('/').to_string()).unwrap_or_else(|| format!("https://{acct}.blob.{cloud}"));
        let need_account = || p.get("account").map(str::to_owned).ok_or_else(|| Error::invalid("Storage account name is required"));
        let tenant = p.get_or("tenant_id", "").to_string();
        let public_client = || p.get("client_id").unwrap_or(DEFAULT_PUBLIC_CLIENT).to_string();
        let (base, account, auth) = match p.get_or("auth", "account_key") {
            "account_key" => {
                let a = need_account()?;
                let key = base64::engine::general_purpose::STANDARD.decode(secret.trim()).map_err(|_| Error::invalid("The account key is not valid base64; copy key1 or key2 from Access keys"))?;
                (account_base(&a), a, Auth::SharedKey(key))
            }
            "azurite" => {
                let url = p.get_or("azurite_url", "http://127.0.0.1:10000").trim_end_matches('/').to_string();
                (format!("{url}/{DEV_ACCOUNT}"), DEV_ACCOUNT.to_string(), Auth::SharedKey(base64::engine::general_purpose::STANDARD.decode(DEV_KEY).unwrap_or_default()))
            }
            "connection_string" => {
                let m = parse_conn_string(&secret);
                if m.get("usedevelopmentstorage").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
                    let host = m.get("developmentstorageproxyuri").cloned().unwrap_or_else(|| "http://127.0.0.1:10000".into());
                    (format!("{}/{DEV_ACCOUNT}", host.trim_end_matches('/')), DEV_ACCOUNT.into(), Auth::SharedKey(base64::engine::general_purpose::STANDARD.decode(DEV_KEY).unwrap_or_default()))
                } else {
                    let account = m.get("accountname").cloned().unwrap_or_default();
                    let proto = m.get("defaultendpointsprotocol").cloned().unwrap_or_else(|| "https".into());
                    let sfx = m.get("endpointsuffix").cloned().unwrap_or_else(|| cloud.into());
                    let base = m.get("blobendpoint").map(|b| b.trim_end_matches('/').to_string()).unwrap_or_else(|| format!("{proto}://{account}.blob.{sfx}"));
                    let auth = if let Some(k) = m.get("accountkey") {
                        Auth::SharedKey(base64::engine::general_purpose::STANDARD.decode(k).map_err(|_| Error::invalid("AccountKey in the connection string is not valid base64"))?)
                    } else if let Some(s) = m.get("sharedaccesssignature") {
                        Auth::Sas(s.trim_start_matches('?').to_string())
                    } else {
                        return Err(Error::invalid("The connection string has neither AccountKey nor SharedAccessSignature"));
                    };
                    if account.is_empty() && !m.contains_key("blobendpoint") {
                        return Err(Error::invalid("The connection string has no AccountName or BlobEndpoint"));
                    }
                    let account = if account.is_empty() { base.split("//").nth(1).and_then(|h| h.split('.').next()).unwrap_or("").to_string() } else { account };
                    (base, account, auth)
                }
            }
            "sas" => {
                let s = secret.trim();
                if s.starts_with("http") {
                    let url: url::Url = s.parse().map_err(|e| Error::invalid(format!("SAS URL: {e}")))?;
                    let sas = url.query().ok_or_else(|| Error::invalid("The SAS URL has no ?sv=… token"))?.to_string();
                    let host = url.host_str().unwrap_or("");
                    let mut base = format!("{}://{host}{}", url.scheme(), url.port().map(|p| format!(":{p}")).unwrap_or_default());
                    let mut segs: Vec<&str> = url.path().trim_matches('/').split('/').filter(|x| !x.is_empty()).collect();
                    if (host.starts_with("127.") || host == "localhost") && !segs.is_empty() {
                        base = format!("{base}/{}", segs.remove(0));
                    }
                    let account = host.split('.').next().unwrap_or("").to_string();
                    let mut az = (base, account, Auth::Sas(sas));
                    if let Some(c) = segs.first() {
                        // Container-level SAS: remember the container.
                        az.0 = format!("{}#container={c}", az.0);
                    }
                    az
                } else {
                    let a = need_account()?;
                    (account_base(&a), a, Auth::Sas(s.trim_start_matches('?').to_string()))
                }
            }
            "anonymous" => {
                let a = need_account()?;
                (account_base(&a), a, Auth::Anonymous)
            }
            "entra_service_principal" => {
                let a = need_account()?;
                let cid = p.get("client_id").ok_or_else(|| Error::invalid("Client (application) ID is required for a service principal"))?.to_string();
                (account_base(&a), a, Auth::Entra(Flow::ServicePrincipal { tenant, client_id: cid, secret }))
            }
            "entra_managed_identity" => {
                let a = need_account()?;
                (account_base(&a), a, Auth::Entra(Flow::ManagedIdentity { client_id: p.get("client_id").map(str::to_owned) }))
            }
            "entra_default" => {
                let a = need_account()?;
                (account_base(&a), a, Auth::Entra(Flow::Default))
            }
            "entra_password" => {
                let a = need_account()?;
                let user = p.get("user").ok_or_else(|| Error::invalid("User is required"))?.to_string();
                (account_base(&a), a, Auth::Entra(Flow::Password { tenant, client_id: public_client(), user, password: secret }))
            }
            _ => {
                let a = need_account()?;
                (account_base(&a), a, Auth::Entra(Flow::DeviceCode { tenant, client_id: public_client() }))
            }
        };
        let client =
            reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(600)).user_agent("OpenVClient").build().map_err(|e| Error::other(e.to_string()))?;
        Ok(Self { base, account, auth, client, token: Mutex::new(entra::Cache::default()), notifier: p.notifier.clone() })
    }

    /// Split a "#container=" marker added for container-level SAS URLs.
    fn take_container_hint(&mut self) -> Option<String> {
        let (b, c) = self.base.split_once("#container=")?;
        let c = c.to_string();
        self.base = b.to_string();
        Some(c)
    }

    /// Build, sign and send a request. `path` is "/container/blob" (unencoded);
    /// `query` pairs are unencoded.
    async fn send(&self, method: reqwest::Method, path: &str, query: &[(&str, String)], headers: &[(&str, String)], body: Option<reqwest::Body>, len: u64) -> Result<reqwest::Response> {
        let epath = enc_path(path);
        let mut qs: Vec<String> = query.iter().map(|(k, v)| format!("{}={}", enc_q(k), enc_q(v))).collect();
        if let Auth::Sas(t) = &self.auth {
            qs.push(t.clone());
        }
        let url = format!("{}{}{}", self.base, epath, if qs.is_empty() { String::new() } else { format!("?{}", qs.join("&")) });
        let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let mut req = self.client.request(method.clone(), &url).header("x-ms-version", VERSION).header("x-ms-date", &date);
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        match &self.auth {
            Auth::SharedKey(key) => {
                let mut ms: Vec<(String, String)> = vec![("x-ms-date".into(), date.clone()), ("x-ms-version".into(), VERSION.into())];
                ms.extend(headers.iter().filter(|(k, _)| k.starts_with("x-ms-")).map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string())));
                ms.sort();
                let canon_headers: String = ms.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
                // Resource: /account + the request path as sent (Azurite's path already starts with the account).
                let base_path = self.base.splitn(4, '/').nth(3).map(|p| format!("/{p}")).unwrap_or_default();
                let mut canon_res = format!("/{}{}{}", self.account, base_path, if epath.is_empty() { "/".to_string() } else { epath.clone() });
                let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.clone())).collect();
                q.sort();
                for (k, v) in q {
                    canon_res.push_str(&format!("\n{k}:{v}"));
                }
                let header = |n: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone()).unwrap_or_default();
                let clen = if len == 0 { String::new() } else { len.to_string() };
                let sts = format!(
                    "{}\n{}\n{}\n{}\n{}\n{}\n\n{}\n{}\n{}\n{}\n{}\n{}{}",
                    method.as_str(),
                    header("content-encoding"),
                    header("content-language"),
                    clen,
                    header("content-md5"),
                    header("content-type"),
                    header("if-modified-since"),
                    header("if-match"),
                    header("if-none-match"),
                    header("if-unmodified-since"),
                    header("range"),
                    canon_headers,
                    canon_res
                );
                let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|e| Error::other(e.to_string()))?;
                mac.update(sts.as_bytes());
                let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
                req = req.header("Authorization", format!("SharedKey {}:{sig}", self.account));
            }
            Auth::Entra(flow) => {
                let cached = self.token.lock().get();
                let token = match cached {
                    Some(t) => t,
                    None => {
                        let (t, exp) = entra::acquire(flow, AZURE_STORAGE, self.notifier.as_ref()).await?;
                        self.token.lock().put(t.clone(), exp);
                        t
                    }
                };
                req = req.bearer_auth(token);
            }
            Auth::Sas(_) | Auth::Anonymous => {}
        }
        if len > 0 || matches!(method, reqwest::Method::PUT) {
            req = req.header("content-length", len);
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        let resp = req.send().await.map_err(|e| if e.is_connect() || e.is_timeout() { Error::connection(format!("{e}")) } else { Error::other(e.to_string()) })?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(az_err(status, &body, &self.auth))
    }

    async fn text(&self, path: &str, query: &[(&str, String)]) -> Result<String> {
        self.send(reqwest::Method::GET, path, query, &[], None, 0).await?.text().await.map_err(|e| Error::other(e.to_string()))
    }

    async fn containers(&self) -> Result<Vec<String>> {
        let xml = self.text("/", &[("comp", "list".into()), ("maxresults", "5000".into())]).await?;
        Ok(tags(&xml, "Container").iter().filter_map(|c| tag(c, "Name")).map(xml_unescape).collect())
    }

    /// One level of a container under `prefix`: (folders, blobs, more?).
    async fn list(&self, container: &str, prefix: &str, max: usize) -> Result<(Vec<String>, Vec<BlobItem>, bool)> {
        let mut q = vec![("restype", "container".to_string()), ("comp", "list".into()), ("delimiter", "/".into()), ("maxresults", max.to_string())];
        if !prefix.is_empty() {
            q.push(("prefix", prefix.to_string()));
        }
        let xml = self.text(&format!("/{container}"), &q).await?;
        let folders = tags(&xml, "BlobPrefix").iter().filter_map(|b| tag(b, "Name")).map(xml_unescape).collect();
        let blobs = tags(&xml, "Blob")
            .iter()
            .map(|b| BlobItem {
                name: tag(b, "Name").map(xml_unescape).unwrap_or_default(),
                size: tag(b, "Content-Length").and_then(|s| s.parse().ok()).unwrap_or(0),
                modified: tag(b, "Last-Modified").unwrap_or("").to_string(),
                tier: tag(b, "AccessTier").unwrap_or("").to_string(),
                content_type: tag(b, "Content-Type").unwrap_or("").to_string(),
            })
            .collect();
        let more = tag(&xml, "NextMarker").is_some_and(|m| !m.is_empty());
        Ok((folders, blobs, more))
    }

    /// Service SAS for one blob (shared key only).
    fn blob_sas(&self, container: &str, blob: &str, hours: u64) -> Option<String> {
        let Auth::SharedKey(key) = &self.auth else { return None };
        let expiry = (chrono::Utc::now() + chrono::Duration::hours(hours as i64)).format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let base_path = self.base.splitn(4, '/').nth(3).map(|p| format!("/{p}")).unwrap_or_default();
        let canon = format!("/blob/{}{}/{container}/{blob}", self.account, base_path.strip_prefix(&format!("/{}", self.account)).unwrap_or(&base_path));
        // permissions, start, expiry, resource, identifier, IP, protocol, version, sr, snapshot, encryption scope, rscc, rscd, rsce, rscl, rsct
        let sts = format!("r\n\n{expiry}\n{canon}\n\n\n\n{VERSION}\nb\n\n\n\n\n\n\n");
        let mut mac = Hmac::<Sha256>::new_from_slice(key).ok()?;
        mac.update(sts.as_bytes());
        let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        Some(format!("{}/{}/{}?sv={VERSION}&sr=b&sp=r&se={}&sig={}", self.base, enc_path(container), enc_path(blob), enc_q(&expiry), enc_q(&sig)))
    }
}

struct BlobItem {
    name: String,
    size: u64,
    modified: String,
    tier: String,
    content_type: String,
}

pub struct AzureSession {
    az: Az,
    only_container: Option<String>,
    read_only: bool,
    info: ServerInfo,
}

#[async_trait]
impl Driver for AzureBlobDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    async fn connect(&self, p: ConnectParams) -> Result<SessionHandle> {
        let mut az = Az::from_params(&p)?;
        let only_container = p.get("container").map(str::to_owned).or_else(|| az.take_container_hint());
        // Validate access up front with the cheapest call the credential allows.
        match &only_container {
            Some(c) => {
                az.list(c, "", 1).await?;
            }
            None => {
                az.containers().await.map_err(|e| {
                    if matches!(az.auth, Auth::Anonymous | Auth::Sas(_)) && e.kind == openvclient_core::ErrorKind::Auth {
                        Error::auth(format!("{} (anonymous and container-level SAS access need the Container field)", e.message))
                    } else {
                        e
                    }
                })?;
            }
        }
        let method = match &az.auth {
            Auth::SharedKey(_) if az.account == DEV_ACCOUNT => "Azurite emulator".to_string(),
            Auth::SharedKey(_) => "account key".into(),
            Auth::Sas(_) => "shared access signature".into(),
            Auth::Entra(f) => format!("Microsoft Entra ID ({f:?})"),
            Auth::Anonymous => "anonymous".into(),
        };
        let info = ServerInfo {
            product: "Azure Blob Storage".into(),
            version: VERSION.into(),
            details: vec![("Account".into(), az.account.clone()), ("Endpoint".into(), az.base.clone()), ("Signed in with".into(), method)],
        };
        Ok(SessionHandle::Resource(Arc::new(AzureSession { az, only_container, read_only: p.read_only, info })))
    }
}

/// Node ids: `c/<container>`, `p/<container>/<prefix/>`, `b/<container>/<blob>`.
fn parse(id: &str) -> Option<(char, &str, &str)> {
    let (k, rest) = id.split_once('/')?;
    let (c, key) = rest.split_once('/').unwrap_or((rest, ""));
    Some((k.chars().next()?, c, key))
}

fn leaf(p: &str) -> &str {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or(p)
}

fn human(n: u64) -> String {
    openvclient_core::containers::human_bytes(n)
}

impl AzureSession {
    fn guard(&self) -> Result<()> {
        if self.read_only { Err(Error::invalid("Connection is read-only")) } else { Ok(()) }
    }
}

#[async_trait]
impl ServiceSession for AzureSession {
    fn server_info(&self) -> ServerInfo {
        self.info.clone()
    }
    async fn ping(&self) -> Result<Duration> {
        let t = Instant::now();
        match &self.only_container {
            Some(c) => drop(self.az.list(c, "", 1).await?),
            None => drop(self.az.containers().await?),
        }
        Ok(t.elapsed())
    }
    async fn close(&self) {}
}

#[async_trait]
impl ResourceSession for AzureSession {
    fn file_browser(&self) -> bool {
        true
    }

    async fn list_dir(&self, node: Option<&str>, filter: &str) -> Result<FileListing> {
        let Some(id) = node else {
            let list = match &self.only_container {
                Some(c) => vec![c.clone()],
                None => self.az.containers().await?,
            };
            let f = filter.trim().to_lowercase();
            let entries = list
                .into_iter()
                .filter(|c| f.is_empty() || c.to_lowercase().contains(&f))
                .map(|c| FileEntry { id: format!("c/{c}"), name: c, is_dir: true, size: None, modified: None, detail: Some("Container".into()), kind: "container".into() })
                .collect();
            return Ok(FileListing { entries, truncated: false });
        };
        let (k, c, key) = parse(id).ok_or_else(|| Error::invalid("Bad node"))?;
        if k != 'c' && k != 'p' {
            return Err(Error::invalid("Not a folder"));
        }
        let prefix = format!("{key}{}", filter.trim());
        let (folders, blobs, more) = self.az.list(c, &prefix, 5000).await?;
        let mut entries: Vec<FileEntry> =
            folders.into_iter().map(|f| FileEntry { id: format!("p/{c}/{f}"), name: leaf(&f).to_string(), is_dir: true, size: None, modified: None, detail: None, kind: "prefix".into() }).collect();
        entries.extend(blobs.into_iter().filter(|b| b.name != key && !b.name.ends_with("/.keep")).map(|b| FileEntry {
            id: format!("b/{c}/{}", b.name),
            name: leaf(&b.name).to_string(),
            is_dir: false,
            size: Some(b.size),
            modified: chrono::DateTime::parse_from_rfc2822(&b.modified).ok().map(|d| d.with_timezone(&chrono::Utc)),
            // Only informative details: a real content type and the access tier.
            detail: {
                let ct = (!b.content_type.is_empty() && b.content_type != "application/octet-stream").then_some(b.content_type);
                let tier = (!b.tier.is_empty()).then(|| format!("{} tier", b.tier));
                match (ct, tier) {
                    (Some(c), Some(t)) => Some(format!("{c} · {t}")),
                    (c, t) => c.or(t),
                }
            },
            kind: "blob".into(),
        }));
        Ok(FileListing { entries, truncated: more })
    }

    fn breadcrumbs(&self, node: &str) -> Vec<(String, String)> {
        let Some((_, c, key)) = parse(node) else { return Vec::new() };
        let mut out = vec![(format!("c/{c}"), c.to_string())];
        let mut acc = String::new();
        let segs: Vec<&str> = key.split('/').filter(|s| !s.is_empty()).collect();
        let dir_segs = if node.starts_with("b/") { segs.len().saturating_sub(1) } else { segs.len() };
        for s in &segs[..dir_segs] {
            acc.push_str(s);
            acc.push('/');
            out.push((format!("p/{c}/{acc}"), s.to_string()));
        }
        out
    }

    fn file_actions(&self) -> FileActions {
        FileActions {
            upload: Some("upload"),
            mkdir: Some("mkdir"),
            download: Some("download"),
            delete_file: Some("delete_blob"),
            delete_dir: Some("delete_prefix"),
            rename: None,
            share: matches!(self.az.auth, Auth::SharedKey(_)).then_some("sas"),
        }
    }

    async fn children(&self, parent: Option<&str>) -> Result<Vec<Node>> {
        let Some(id) = parent else {
            let list = match &self.only_container {
                Some(c) => vec![c.clone()],
                None => self.az.containers().await?,
            };
            return Ok(list.into_iter().map(|c| Node::new(format!("c/{c}"), &c, "container", NodeIcon::Bucket).folder()).collect());
        };
        let (k, c, prefix) = parse(id).ok_or_else(|| Error::invalid("Bad node"))?;
        if k != 'c' && k != 'p' {
            return Ok(Vec::new());
        }
        let (folders, blobs, more) = self.az.list(c, prefix, 1000).await?;
        let mut out: Vec<Node> = folders.iter().map(|f| Node::new(format!("p/{c}/{f}"), leaf(f), "prefix", NodeIcon::Folder).folder()).collect();
        out.extend(blobs.iter().filter(|b| b.name != prefix).map(|b| Node::new(format!("b/{c}/{}", b.name), leaf(&b.name), "blob", NodeIcon::File).detail(human(b.size))));
        if more {
            out.push(Node::new(format!("{id}#more"), "First 1,000 shown; open the folder and filter", "info", NodeIcon::Info));
        }
        Ok(out)
    }

    async fn open(&self, node: &str, opts: OpenOptions) -> Result<Content> {
        let (k, c, key) = parse(node).ok_or_else(|| Error::invalid("Bad node"))?;
        match k {
            'c' | 'p' => {
                let prefix = format!("{key}{}", opts.filter.trim());
                let (folders, blobs, more) = self.az.list(c, &prefix, (opts.limit as usize).clamp(1, 5000)).await?;
                let mut rs = ResultSet::new(vec![
                    ColumnMeta::new("name", "text"),
                    ColumnMeta::new("size", "int8"),
                    ColumnMeta::new("last_modified", "text"),
                    ColumnMeta::new("tier", "text"),
                    ColumnMeta::new("content_type", "text"),
                ]);
                for f in folders {
                    rs.rows.push(vec![Value::Text(f[key.len().min(f.len())..].to_string()), Value::Null, Value::Null, Value::Text("folder".into()), Value::Null]);
                }
                for b in blobs {
                    rs.rows.push(vec![
                        Value::Text(b.name[key.len().min(b.name.len())..].to_string()),
                        Value::Int(b.size as i64),
                        Value::Text(b.modified),
                        Value::Text(b.tier),
                        Value::Text(b.content_type),
                    ]);
                }
                let note = format!("{} item(s) in {c}/{key}{}", rs.rows.len(), if more { " · more exist, narrow with the filter" } else { "" });
                Ok(Content::TableWithNote(rs, note))
            }
            'b' => {
                let path = format!("/{c}/{key}");
                let head = self.az.send(reqwest::Method::HEAD, &path, &[], &[], None, 0).await?;
                let h = head.headers();
                let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                let size: u64 = get("content-length").parse().unwrap_or(0);
                let ctype = get("content-type");
                let mut props = vec![("Blob".to_string(), format!("{c}/{key}")), ("Size".into(), format!("{} ({size} bytes)", human(size))), ("Content type".into(), ctype.clone())];
                for (n, l) in [
                    ("x-ms-blob-type", "Blob type"),
                    ("x-ms-access-tier", "Access tier"),
                    ("last-modified", "Last modified"),
                    ("etag", "ETag"),
                    ("x-ms-server-encrypted", "Encrypted"),
                    ("x-ms-lease-status", "Lease"),
                ] {
                    let v = get(n);
                    if !v.is_empty() {
                        props.push((l.into(), v.trim_matches('"').to_string()));
                    }
                }
                for (n, v) in h.iter().filter(|(n, _)| n.as_str().starts_with("x-ms-meta-")) {
                    props.push((n.as_str().trim_start_matches("x-ms-").to_string(), v.to_str().unwrap_or("").to_string()));
                }
                let textual = ctype.starts_with("text/")
                    || ctype.contains("json")
                    || ctype.contains("xml")
                    || ctype.contains("csv")
                    || [".txt", ".json", ".csv", ".log", ".md", ".yml", ".yaml", ".xml", ".sql"].iter().any(|e| key.ends_with(e));
                if size > 0 && size <= openvclient_core::resource::IMAGE_PREVIEW_MAX && (ctype.starts_with("image/") || openvclient_core::resource::is_image_name(key)) && !ctype.contains("svg") {
                    let body = self.az.send(reqwest::Method::GET, &path, &[], &[], None, 0).await?.bytes().await.map_err(|e| Error::other(e.to_string()))?;
                    return Ok(Content::Image(body.to_vec()));
                }
                if size > 0 && size <= openvclient_core::resource::IMAGE_PREVIEW_MAX && (ctype.contains("pdf") || openvclient_core::resource::is_pdf_name(key)) {
                    let body = self.az.send(reqwest::Method::GET, &path, &[], &[], None, 0).await?.bytes().await.map_err(|e| Error::other(e.to_string()))?;
                    return Ok(Content::Pdf(body.to_vec()));
                }
                if size > 0 && size <= PREVIEW_MAX && textual {
                    let body = self.az.send(reqwest::Method::GET, &path, &[], &[], None, 0).await?.bytes().await.map_err(|e| Error::other(e.to_string()))?;
                    let text = String::from_utf8_lossy(&body).into_owned();
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
                        && (ctype.contains("json") || key.ends_with(".json"))
                    {
                        return Ok(Content::Json(serde_json::to_string_pretty(&v).unwrap_or(text)));
                    }
                    return Ok(Content::Text(text));
                }
                props.push(("Preview".into(), if textual { "Too large to preview; use Download".into() } else { "Binary blob; use Download".into() }));
                Ok(Content::Props(props))
            }
            _ => Ok(Content::Empty(String::new())),
        }
    }

    fn filter_hint(&self, kind: &str) -> Option<&'static str> {
        matches!(kind, "container" | "prefix").then_some("Name prefix within this folder, e.g. 2024/")
    }

    fn actions(&self, kind: &str) -> Vec<ActionSpec> {
        let upload = || {
            ActionSpec::new("upload", "Upload file…")
                .primary()
                .field(FieldSpec::text("file", "File").kind(FieldKind::File { save: false, filters: &[] }).required())
                .field(FieldSpec::text("name", "Blob name").placeholder("same as file name"))
                .field(FieldSpec::text("tier", "Access tier").kind(FieldKind::Choice(&["default", "Hot", "Cool", "Cold", "Archive"])).default("default"))
        };
        let shared_key = matches!(self.az.auth, Auth::SharedKey(_));
        match kind {
            "" => vec![ActionSpec::new("create_container", "New container…").field(FieldSpec::text("name", "Container name").required().help("3–63 lowercase letters, numbers and hyphens"))],
            "container" => {
                vec![upload(), ActionSpec::new("mkdir", "New folder…").field(FieldSpec::text("name", "Folder").required()), ActionSpec::new("delete_container", "Delete container").destructive()]
            }
            "prefix" => vec![
                upload(),
                ActionSpec::new("mkdir", "New folder…").field(FieldSpec::text("name", "Folder").required()),
                ActionSpec::new("delete_prefix", "Delete folder and contents").destructive(),
            ],
            "blob" => {
                let mut v = vec![ActionSpec::new("download", "Download…").primary().field(FieldSpec::text("file", "Save as").kind(FieldKind::File { save: true, filters: &[] }).required())];
                if shared_key {
                    v.push(ActionSpec::new("sas", "Share link (SAS)…").field(FieldSpec::text("hours", "Valid for (hours)").kind(FieldKind::Number).default("1")));
                }
                v.push(ActionSpec::new("set_tier", "Change access tier…").field(FieldSpec::text("tier", "Tier").kind(FieldKind::Choice(&["Hot", "Cool", "Cold", "Archive"])).default("Cool")));
                v.push(ActionSpec::new("delete_blob", "Delete blob").destructive());
                v
            }
            _ => vec![],
        }
    }

    async fn run_action(&self, node: &str, action: &str, a: Args) -> Result<ActionOutcome> {
        if !matches!(action, "download" | "sas") {
            self.guard()?;
        }
        use reqwest::Method as M;
        if action == "create_container" {
            let name = arg(&a, "name")?.trim().to_string();
            self.az.send(M::PUT, &format!("/{name}"), &[("restype", "container".into())], &[], None, 0).await?;
            return Ok(ActionOutcome::msg(format!("Created container {name}")).refresh(""));
        }
        let (k, c, key) = parse(node).ok_or_else(|| Error::invalid("Bad node"))?;
        let folder = |prefix: &str| if prefix.is_empty() { format!("c/{c}") } else { format!("p/{c}/{prefix}") };
        let parent_of = |k: &str| k.trim_end_matches('/').rsplit_once('/').map(|(p, _)| format!("{p}/")).unwrap_or_default();
        match action {
            "upload" => {
                let local = std::path::PathBuf::from(arg(&a, "file")?);
                let name = a.get("name").filter(|n| !n.trim().is_empty()).cloned().unwrap_or_else(|| local.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
                let blob = format!("{key}{}", name.trim_start_matches('/'));
                let data = tokio::fs::read(&local).await.map_err(|e| Error::io(format!("{}: {e}", local.display())))?;
                let len = data.len() as u64;
                if len > 4 * 1024 * 1024 * 1024 {
                    return Err(Error::unsupported("Files over 4 GiB need block uploads, which are not available yet"));
                }
                let mut headers = vec![("x-ms-blob-type", "BlockBlob".to_string())];
                if let Some(t) = a.get("tier").filter(|t| t.as_str() != "default") {
                    headers.push(("x-ms-access-tier", t.clone()));
                }
                self.az.send(M::PUT, &format!("/{c}/{blob}"), &[], &headers, Some(data.into()), len).await?;
                Ok(ActionOutcome::msg(format!("Uploaded {} to {c}/{blob}", human(len))).refresh(folder(key)))
            }
            "mkdir" => {
                // Blob storage has no folders: a zero-length placeholder blob makes one visible.
                let name = arg(&a, "name")?.trim().trim_matches('/');
                let blob = format!("{key}{name}/.keep");
                self.az.send(M::PUT, &format!("/{c}/{blob}"), &[], &[("x-ms-blob-type", "BlockBlob".into())], Some(Vec::new().into()), 0).await?;
                Ok(ActionOutcome::msg(format!("Created folder {name}/")).refresh(folder(key)))
            }
            "download" => {
                let local = std::path::PathBuf::from(arg(&a, "file")?);
                let mut resp = self.az.send(M::GET, &format!("/{c}/{key}"), &[], &[], None, 0).await?;
                let mut f = tokio::fs::File::create(&local).await.map_err(|e| Error::io(format!("{}: {e}", local.display())))?;
                let mut n = 0u64;
                use tokio::io::AsyncWriteExt;
                while let Some(chunk) = resp.chunk().await.map_err(|e| Error::connection(e.to_string()))? {
                    n += chunk.len() as u64;
                    f.write_all(&chunk).await.map_err(|e| Error::io(e.to_string()))?;
                }
                f.flush().await.map_err(|e| Error::io(e.to_string()))?;
                Ok(ActionOutcome::msg(format!("Saved {} to {}", human(n), local.display())))
            }
            "sas" => {
                let hours: u64 = a.get("hours").and_then(|h| h.parse().ok()).unwrap_or(1).clamp(1, 24 * 7);
                let url = self.az.blob_sas(c, key, hours).ok_or_else(|| Error::unsupported("Share links need account-key access"))?;
                Ok(ActionOutcome::msg(format!("Read-only link valid for {hours} h")).show(Content::Text(url)))
            }
            "set_tier" => {
                self.az.send(M::PUT, &format!("/{c}/{key}"), &[("comp", "tier".into())], &[("x-ms-access-tier", arg(&a, "tier")?.to_string())], None, 0).await?;
                Ok(ActionOutcome::msg("Access tier changed"))
            }
            "delete_blob" => {
                self.az.send(M::DELETE, &format!("/{c}/{key}"), &[], &[], None, 0).await?;
                Ok(ActionOutcome::msg(format!("Deleted {}", leaf(key))).refresh(folder(&parent_of(key))))
            }
            "delete_prefix" if k == 'p' => {
                let mut deleted = 0;
                loop {
                    let xml = self.az.text(&format!("/{c}"), &[("restype", "container".into()), ("comp", "list".into()), ("prefix", key.to_string()), ("maxresults", "1000".into())]).await?;
                    let names: Vec<String> = tags(&xml, "Blob").iter().filter_map(|b| tag(b, "Name")).map(xml_unescape).collect();
                    if names.is_empty() {
                        break;
                    }
                    for n in names {
                        self.az.send(M::DELETE, &format!("/{c}/{n}"), &[], &[], None, 0).await?;
                        deleted += 1;
                    }
                }
                Ok(ActionOutcome::msg(format!("Deleted {deleted} blob(s)")).refresh(folder(&parent_of(key))))
            }
            "delete_container" => {
                self.az.send(M::DELETE, &format!("/{c}"), &[("restype", "container".into())], &[], None, 0).await?;
                Ok(ActionOutcome::msg(format!("Deleted container {c}")).refresh(""))
            }
            other => Err(Error::unsupported(format!("Unknown action {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_strings_and_ids() {
        let m = parse_conn_string("DefaultEndpointsProtocol=https;AccountName=acct;AccountKey=abc==;EndpointSuffix=core.windows.net");
        assert_eq!(m["accountname"], "acct");
        assert_eq!(m["accountkey"], "abc==");
        assert_eq!(parse("b/c1/a/b.txt"), Some(('b', "c1", "a/b.txt")));
        assert_eq!(enc_path("/c/a b+c"), "/c/a%20b%2Bc");
    }

    fn params(auth: &str, secret: Option<String>) -> ConnectParams {
        let mut v = std::collections::BTreeMap::new();
        v.insert("auth".into(), auth.into());
        v.insert("azurite_url".into(), std::env::var("OPENVCLIENT_AZURITE").unwrap_or_else(|_| "http://127.0.0.1:50000".into()));
        ConnectParams { values: v, secret, read_only: false, notifier: None }
    }

    #[tokio::test]
    async fn live_azurite() {
        let h = match AzureBlobDriver::default().connect(params("azurite", None)).await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("skipping: no Azurite ({e})");
                return;
            }
        };
        let s = h.resource().unwrap().clone();
        let _ = s.run_action("p/ovc-test/docs/", "delete_prefix", Args::new()).await;
        let _ = s.run_action("c/ovc-test", "delete_container", Args::new()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut a = Args::new();
        a.insert("name".into(), "ovc-test".into());
        s.run_action("", "create_container", a).await.unwrap();
        let src = std::env::temp_dir().join("az hello.json");
        std::fs::write(&src, r#"{"azure": true}"#).unwrap();
        let mut m = Args::new();
        m.insert("name".into(), "docs".into());
        s.run_action("c/ovc-test", "mkdir", m).await.unwrap();
        let mut u = Args::new();
        u.insert("file".into(), src.display().to_string());
        s.run_action("p/ovc-test/docs/", "upload", u).await.unwrap();
        // File-manager listing: folder shows the blob with size and time; .keep is hidden.
        let l = s.list_dir(Some("p/ovc-test/docs/"), "").await.unwrap();
        let f = l.entries.iter().find(|e| e.name == "az hello.json").expect("listed");
        assert!(!f.is_dir && f.size == Some(15) && f.modified.is_some(), "{f:?}");
        assert!(!l.entries.iter().any(|e| e.name == ".keep"));
        let top = s.list_dir(Some("c/ovc-test"), "").await.unwrap();
        assert!(top.entries.iter().any(|e| e.is_dir && e.name == "docs"));
        assert_eq!(s.breadcrumbs(&f.id), vec![("c/ovc-test".to_string(), "ovc-test".to_string()), ("p/ovc-test/docs/".to_string(), "docs".to_string())]);
        assert!(s.list_dir(None, "").await.unwrap().entries.iter().any(|e| e.id == "c/ovc-test"));
        let kids = s.children(Some("p/ovc-test/docs/")).await.unwrap();
        let blob = kids.iter().find(|n| n.label == "az hello.json").expect("uploaded").id.clone();
        let Content::Json(j) = s.open(&blob, OpenOptions::default()).await.unwrap() else { panic!("json") };
        assert!(j.contains("azure"));
        let link = s.run_action(&blob, "sas", Args::new()).await.unwrap();
        let Some(Content::Text(url)) = link.content else { panic!() };
        let got = reqwest::get(&url).await.unwrap();
        assert!(got.status().is_success(), "SAS link: {} {}", got.status(), url);
        assert_eq!(got.text().await.unwrap(), r#"{"azure": true}"#);
        // The SAS URL itself works as a sign-in method (container-level path).
        let sas_conn = AzureBlobDriver::default().connect(params("sas", Some(url.clone()))).await;
        assert!(sas_conn.is_ok() || sas_conn.as_ref().err().is_some_and(|e| e.kind == openvclient_core::ErrorKind::Auth), "{:?}", sas_conn.err());
        // Connection string for the emulator.
        AzureBlobDriver::default()
            .connect(params(
                "connection_string",
                Some(format!("UseDevelopmentStorage=true;DevelopmentStorageProxyUri={}", std::env::var("OPENVCLIENT_AZURITE").unwrap_or_else(|_| "http://127.0.0.1:50000".into()))),
            ))
            .await
            .unwrap();
        s.run_action("p/ovc-test/docs/", "delete_prefix", Args::new()).await.unwrap();
        s.run_action("c/ovc-test", "delete_container", Args::new()).await.unwrap();
    }
}
