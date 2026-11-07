//! S3-compatible object storage (AWS S3, MinIO, Cloudflare R2, Wasabi,
//! Backblaze B2, Ceph…) with SigV4 presigned requests.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use openvclient_core::resource::*;
use openvclient_core::{Category, ColumnMeta, ConnectParams, Driver, DriverInfo, Error, FieldKind, FieldSpec, Result, ResultSet, ServerInfo, ServiceSession, SessionHandle, Value};
use rusty_s3::actions::{CreateBucket, DeleteBucket, DeleteObject, GetObject, HeadObject, ListObjectsV2, PutObject};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use url::Url;

const SIGN: Duration = Duration::from_secs(600);
/// Objects larger than this are not previewed inline.
const PREVIEW_MAX: u64 = 1024 * 1024;

pub struct S3Driver {
    info: DriverInfo,
}

impl Default for S3Driver {
    fn default() -> Self {
        Self {
            info: DriverInfo {
                id: "s3",
                name: "S3 / object storage",
                category: Category::Storage,
                description: "AWS S3 and S3-compatible storage: MinIO, R2, Wasabi, B2, Ceph",
                badge: "S3",
                fields: vec![
                    FieldSpec::text("endpoint", "Endpoint URL")
                        .default("https://s3.amazonaws.com")
                        .required()
                        .help("AWS: https://s3.<region>.amazonaws.com · MinIO: http://host:9000 · R2: https://<account>.r2.cloudflarestorage.com"),
                    FieldSpec::text("region", "Region").default("us-east-1"),
                    FieldSpec::text("access_key", "Access key ID").required(),
                    FieldSpec::text("password", "Secret access key").kind(FieldKind::Secret),
                    FieldSpec::text("bucket", "Bucket").placeholder("all buckets").help("Limit the tree to one bucket (needed when the key may not list buckets)"),
                    FieldSpec::text("path_style", "Path-style URLs").kind(FieldKind::Bool).default("true").help("On for MinIO/Ceph and IP endpoints; off for AWS virtual-hosted buckets"),
                    FieldSpec::text("session_token", "Session token").placeholder("for temporary STS credentials").advanced(),
                ],
            },
        }
    }
}

impl S3Driver {
    /// A provider preset: same S3 protocol, provider-specific defaults.
    pub fn flavor(id: &'static str, name: &'static str, description: &'static str, endpoint: &'static str, region: &'static str, path_style: bool, endpoint_help: &'static str) -> Self {
        let mut d = Self::default();
        d.info.id = id;
        d.info.name = name;
        d.info.description = description;
        for f in &mut d.info.fields {
            match f.key {
                "endpoint" => {
                    *f = FieldSpec::text("endpoint", "Endpoint URL").default(endpoint).required().help(endpoint_help);
                }
                "region" => *f = FieldSpec::text("region", "Region").default(region),
                "path_style" => *f = FieldSpec::text("path_style", "Path-style URLs").kind(FieldKind::Bool).default(if path_style { "true" } else { "false" }).advanced(),
                _ => {}
            }
        }
        d
    }

    /// Every S3-compatible provider preset shipped in the app.
    pub fn providers() -> Vec<Self> {
        vec![
            Self::flavor(
                "b2",
                "Backblaze B2",
                "Backblaze B2 cloud storage (S3-compatible API)",
                "https://s3.us-west-004.backblazeb2.com",
                "us-west-004",
                false,
                "https://s3.<region>.backblazeb2.com, e.g. us-west-004, eu-central-003. Use an application key ID and key.",
            ),
            Self::flavor(
                "idrive_e2",
                "IDrive e2",
                "IDrive e2 hot cloud storage",
                "https://s3.us-west-1.idrivee2.com",
                "us-west-1",
                true,
                "Your e2 region endpoint from the IDrive e2 console, e.g. https://xxxx.<region>.idrivee2-NN.com",
            ),
            Self::flavor("wasabi", "Wasabi", "Wasabi hot cloud storage", "https://s3.wasabisys.com", "us-east-1", false, "https://s3.<region>.wasabisys.com"),
            Self::flavor(
                "r2",
                "Cloudflare R2",
                "Cloudflare R2 object storage",
                "https://<account-id>.r2.cloudflarestorage.com",
                "auto",
                true,
                "https://<account-id>.r2.cloudflarestorage.com · region 'auto'",
            ),
            Self::flavor("do_spaces", "DigitalOcean Spaces", "DigitalOcean Spaces object storage", "https://nyc3.digitaloceanspaces.com", "nyc3", false, "https://<region>.digitaloceanspaces.com"),
            Self::flavor(
                "gcs",
                "Google Cloud Storage",
                "Google Cloud Storage via the S3 interoperability API (HMAC keys)",
                "https://storage.googleapis.com",
                "auto",
                true,
                "https://storage.googleapis.com · create HMAC keys under Cloud Storage → Settings → Interoperability",
            ),
            Self::flavor(
                "oci",
                "Oracle Object Storage",
                "Oracle Cloud object storage (S3 compatibility API)",
                "https://<namespace>.compat.objectstorage.<region>.oraclecloud.com",
                "us-ashburn-1",
                true,
                "https://<namespace>.compat.objectstorage.<region>.oraclecloud.com · use a Customer Secret Key",
            ),
            Self::flavor("linode", "Akamai / Linode Object Storage", "Linode (Akamai) object storage", "https://us-east-1.linodeobjects.com", "us-east-1", false, "https://<region>.linodeobjects.com"),
            Self::flavor("scaleway", "Scaleway Object Storage", "Scaleway object storage", "https://s3.fr-par.scw.cloud", "fr-par", false, "https://s3.<region>.scw.cloud"),
            Self::flavor("hetzner", "Hetzner Object Storage", "Hetzner object storage", "https://fsn1.your-objectstorage.com", "fsn1", false, "https://<location>.your-objectstorage.com"),
            Self::flavor(
                "storj",
                "Storj",
                "Storj decentralized storage (S3 gateway)",
                "https://gateway.storjshare.io",
                "us-1",
                true,
                "https://gateway.storjshare.io · create S3 credentials in the Storj console",
            ),
            Self::flavor("minio", "MinIO", "MinIO and other self-hosted S3 servers (SeaweedFS, Ceph RGW, Garage)", "http://localhost:9000", "us-east-1", true, "http(s)://host:9000"),
            Self::flavor(
                "alibaba_oss",
                "Alibaba Cloud OSS",
                "Alibaba Cloud Object Storage Service (S3-compatible)",
                "https://oss-cn-hangzhou.aliyuncs.com",
                "oss-cn-hangzhou",
                false,
                "https://oss-<region>.aliyuncs.com",
            ),
        ]
    }
}

#[async_trait]
impl Driver for S3Driver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    async fn connect(&self, p: ConnectParams) -> Result<SessionHandle> {
        let endpoint: Url = p.get_or("endpoint", "https://s3.amazonaws.com").trim().parse().map_err(|e| Error::invalid(format!("Endpoint is not a valid URL: {e}")))?;
        if !matches!(endpoint.scheme(), "http" | "https") {
            return Err(Error::invalid("The endpoint must start with http:// or https://"));
        }
        let key = p.get("access_key").ok_or_else(|| Error::invalid("Access key ID is required"))?.to_string();
        let secret = p.secret.clone().unwrap_or_default();
        let creds = match p.get("session_token") {
            Some(t) => Credentials::new_with_token(key, secret, t.to_string()),
            None => Credentials::new(key, secret),
        };
        let client =
            reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(600)).user_agent("OpenVClient").build().map_err(|e| Error::other(e.to_string()))?;
        let s = S3Session {
            endpoint,
            region: p.get_or("region", "us-east-1").to_string(),
            style: if p.get("path_style").is_none_or(|v| v == "true") { UrlStyle::Path } else { UrlStyle::VirtualHost },
            creds,
            only_bucket: p.get("bucket").map(str::to_owned),
            client,
            read_only: p.read_only,
            info: ServerInfo::default(),
        };
        // Validate credentials up front.
        let buckets = s.buckets().await?;
        let product = if s.endpoint.host_str().is_some_and(|h| h.ends_with("amazonaws.com")) { "Amazon S3" } else { "S3-compatible storage" };
        let info = ServerInfo {
            product: product.into(),
            version: String::new(),
            details: vec![("Endpoint".into(), s.endpoint.to_string()), ("Region".into(), s.region.clone()), ("Buckets".into(), buckets.len().to_string())],
        };
        Ok(SessionHandle::Resource(Arc::new(S3Session { info, ..s })))
    }
}

struct S3Session {
    endpoint: Url,
    region: String,
    style: UrlStyle,
    creds: Credentials,
    only_bucket: Option<String>,
    client: reqwest::Client,
    read_only: bool,
    info: ServerInfo,
}

fn s3_err(status: u16, body: &str) -> Error {
    let tag = |t: &str| body.split(&format!("<{t}>")).nth(1).and_then(|r| r.split(&format!("</{t}>")).next()).map(str::to_owned);
    let code = tag("Code").unwrap_or_else(|| format!("HTTP {status}"));
    let msg = tag("Message").unwrap_or_default();
    let text = if msg.is_empty() { code.clone() } else { format!("{code}: {msg}") };
    match (status, code.as_str()) {
        (403, _) | (_, "InvalidAccessKeyId" | "SignatureDoesNotMatch" | "AccessDenied") => Error::auth(text),
        _ => Error::server(text),
    }
}

impl S3Session {
    fn bucket(&self, name: &str) -> Result<Bucket> {
        Bucket::new(self.endpoint.clone(), self.style, name.to_string(), self.region.clone()).map_err(|e| Error::invalid(format!("Bucket {name}: {e}")))
    }

    fn guard(&self) -> Result<()> {
        if self.read_only { Err(Error::invalid("Connection is read-only")) } else { Ok(()) }
    }

    async fn send(&self, r: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let resp = r.send().await.map_err(|e| if e.is_connect() || e.is_timeout() { Error::connection(format!("{e}: {}", self.endpoint)) } else { Error::other(e.to_string()) })?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(s3_err(status, &body))
    }

    async fn buckets(&self) -> Result<Vec<String>> {
        if let Some(b) = &self.only_bucket {
            return Ok(vec![b.clone()]);
        }
        // ListBuckets is a signed GET on the service endpoint.
        let url = rusty_s3::signing::sign(
            &jiff::Timestamp::now(),
            rusty_s3::Method::Get,
            self.endpoint.clone(),
            self.creds.key(),
            self.creds.secret(),
            self.creds.token(),
            &self.region,
            SIGN.as_secs(),
            std::iter::empty(),
            std::iter::empty(),
        );
        let body = self.send(self.client.get(url)).await?.text().await.map_err(|e| Error::other(e.to_string()))?;
        Ok(body.split("<Name>").skip(1).filter_map(|s| s.split("</Name>").next()).map(xml_unescape).collect())
    }

    async fn list(&self, bucket: &str, prefix: &str, token: Option<&str>, max: usize) -> Result<rusty_s3::actions::ListObjectsV2Response> {
        let b = self.bucket(bucket)?;
        let mut a = ListObjectsV2::new(&b, Some(&self.creds));
        a.with_delimiter("/");
        a.with_max_keys(max);
        if !prefix.is_empty() {
            a.with_prefix(prefix.to_string());
        }
        if let Some(t) = token {
            a.with_continuation_token(t.to_string());
        }
        let body = self.send(self.client.get(a.sign(SIGN))).await?.text().await.map_err(|e| Error::other(e.to_string()))?;
        ListObjectsV2::parse_response(&body).map_err(|e| Error::other(format!("Unexpected listing response: {e}")))
    }
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Node ids: `b/<bucket>` and `p/<bucket>/<prefix/>` and `o/<bucket>/<key>`.
fn parse(id: &str) -> Option<(char, &str, &str)> {
    let (k, rest) = id.split_once('/')?;
    let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
    Some((k.chars().next()?, bucket, key))
}

fn leaf(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

fn human(n: u64) -> String {
    openvclient_core::containers::human_bytes(n)
}

#[async_trait]
impl ServiceSession for S3Session {
    fn server_info(&self) -> ServerInfo {
        self.info.clone()
    }
    async fn ping(&self) -> Result<Duration> {
        let t = Instant::now();
        self.buckets().await?;
        Ok(t.elapsed())
    }
    async fn close(&self) {}
}

#[async_trait]
impl ResourceSession for S3Session {
    fn file_browser(&self) -> bool {
        true
    }

    async fn list_dir(&self, node: Option<&str>, filter: &str) -> Result<FileListing> {
        let Some(id) = node else {
            let f = filter.trim().to_lowercase();
            let entries = self
                .buckets()
                .await?
                .into_iter()
                .filter(|b| f.is_empty() || b.to_lowercase().contains(&f))
                .map(|b| FileEntry { id: format!("b/{b}"), name: b, is_dir: true, size: None, modified: None, detail: Some("Bucket".into()), kind: "bucket".into() })
                .collect();
            return Ok(FileListing { entries, truncated: false });
        };
        let (k, bucket, key) = parse(id).ok_or_else(|| Error::invalid("Bad node"))?;
        if k != 'b' && k != 'p' {
            return Err(Error::invalid("Not a folder"));
        }
        let r = self.list(bucket, &format!("{key}{}", filter.trim()), None, 1000).await?;
        let mut entries: Vec<FileEntry> = r
            .common_prefixes
            .iter()
            .map(|c| FileEntry { id: format!("p/{bucket}/{}", c.prefix), name: leaf(&c.prefix).to_string(), is_dir: true, size: None, modified: None, detail: None, kind: "prefix".into() })
            .collect();
        entries.extend(r.contents.iter().filter(|o| o.key != key).map(|o| FileEntry {
            id: format!("o/{bucket}/{}", o.key),
            name: leaf(&o.key).to_string(),
            is_dir: false,
            size: o.size,
            modified: o.last_modified.as_deref().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|d| d.with_timezone(&chrono::Utc)),
            detail: o.storage_class.clone(),
            kind: "object".into(),
        }));
        Ok(FileListing { entries, truncated: r.next_continuation_token.is_some() })
    }

    fn breadcrumbs(&self, node: &str) -> Vec<(String, String)> {
        let Some((_, bucket, key)) = parse(node) else { return Vec::new() };
        let mut out = vec![(format!("b/{bucket}"), bucket.to_string())];
        let segs: Vec<&str> = key.split('/').filter(|s| !s.is_empty()).collect();
        let n = if node.starts_with("o/") { segs.len().saturating_sub(1) } else { segs.len() };
        let mut acc = String::new();
        for s in &segs[..n] {
            acc.push_str(s);
            acc.push('/');
            out.push((format!("p/{bucket}/{acc}"), s.to_string()));
        }
        out
    }

    fn file_actions(&self) -> FileActions {
        FileActions {
            upload: Some("upload"),
            mkdir: Some("mkdir"),
            download: Some("download"),
            delete_file: Some("delete_object"),
            delete_dir: Some("delete_prefix"),
            rename: None,
            share: Some("presign"),
        }
    }

    async fn children(&self, parent: Option<&str>) -> Result<Vec<Node>> {
        let Some(id) = parent else {
            return Ok(self.buckets().await?.into_iter().map(|b| Node::new(format!("b/{b}"), &b, "bucket", NodeIcon::Bucket).folder()).collect());
        };
        let (k, bucket, prefix) = parse(id).ok_or_else(|| Error::invalid("Bad node"))?;
        if k != 'b' && k != 'p' {
            return Ok(Vec::new());
        }
        let r = self.list(bucket, prefix, None, 1000).await?;
        let mut out: Vec<Node> = r.common_prefixes.iter().map(|c| Node::new(format!("p/{bucket}/{}", c.prefix), leaf(&c.prefix), "prefix", NodeIcon::Folder).folder()).collect();
        out.extend(
            r.contents.iter().filter(|o| o.key != prefix).map(|o| Node::new(format!("o/{bucket}/{}", o.key), leaf(&o.key), "object", NodeIcon::File).detail(o.size.map(human).unwrap_or_default())),
        );
        if r.next_continuation_token.is_some() {
            out.push(Node::new(format!("{id}#more"), "First 1,000 shown; open the folder to page", "info", NodeIcon::Info));
        }
        Ok(out)
    }

    async fn open(&self, node: &str, opts: OpenOptions) -> Result<Content> {
        let (k, bucket, key) = parse(node).ok_or_else(|| Error::invalid("Bad node"))?;
        match k {
            'b' | 'p' => {
                // Listing table with a name filter (prefix within this folder).
                let prefix = format!("{key}{}", opts.filter.trim());
                let r = self.list(bucket, &prefix, None, (opts.limit as usize).clamp(1, 1000)).await?;
                let mut rs = ResultSet::new(vec![
                    ColumnMeta::new("name", "text"),
                    ColumnMeta::new("size", "int8"),
                    ColumnMeta::new("last_modified", "text"),
                    ColumnMeta::new("storage_class", "text"),
                    ColumnMeta::new("etag", "text"),
                ]);
                for c in &r.common_prefixes {
                    rs.rows.push(vec![Value::Text(c.prefix[key.len()..].to_string()), Value::Null, Value::Null, Value::Text("folder".into()), Value::Null]);
                }
                for o in &r.contents {
                    rs.rows.push(vec![
                        Value::Text(o.key[key.len().min(o.key.len())..].to_string()),
                        o.size.map(|s| Value::Int(s as i64)).unwrap_or(Value::Null),
                        o.last_modified.clone().map(Value::Text).unwrap_or(Value::Null),
                        o.storage_class.clone().map(Value::Text).unwrap_or(Value::Null),
                        o.etag.clone().map(|e| Value::Text(e.trim_matches('"').to_string())).unwrap_or(Value::Null),
                    ]);
                }
                let note = format!("{} item(s) in s3://{bucket}/{key}{}", rs.rows.len(), if r.next_continuation_token.is_some() { " · more exist, narrow with the filter" } else { "" });
                Ok(Content::TableWithNote(rs, note))
            }
            'o' => {
                let b = self.bucket(bucket)?;
                let head = self.send(self.client.head(HeadObject::new(&b, Some(&self.creds), key).sign(SIGN))).await?;
                let h = head.headers();
                let size: u64 = h.get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(0);
                let ctype = h.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                let mut props = vec![("Key".to_string(), format!("s3://{bucket}/{key}")), ("Size".into(), format!("{} ({size} bytes)", human(size))), ("Content type".into(), ctype.clone())];
                for (name, label) in
                    [("last-modified", "Last modified"), ("etag", "ETag"), ("x-amz-storage-class", "Storage class"), ("x-amz-server-side-encryption", "Encryption"), ("x-amz-version-id", "Version")]
                {
                    if let Some(v) = h.get(name).and_then(|v| v.to_str().ok()) {
                        props.push((label.into(), v.trim_matches('"').to_string()));
                    }
                }
                for (n, v) in h.iter().filter(|(n, _)| n.as_str().starts_with("x-amz-meta-")) {
                    props.push((n.as_str().trim_start_matches("x-amz-").to_string(), v.to_str().unwrap_or("").to_string()));
                }
                let textual = ctype.starts_with("text/")
                    || ctype.contains("json")
                    || ctype.contains("xml")
                    || ctype.contains("yaml")
                    || ctype.contains("csv")
                    || [".txt", ".json", ".csv", ".log", ".md", ".yml", ".yaml", ".xml", ".sql", ".ini", ".toml"].iter().any(|e| key.ends_with(e));
                if size > 0 && size <= openvclient_core::resource::IMAGE_PREVIEW_MAX && (ctype.starts_with("image/") || openvclient_core::resource::is_image_name(key)) && !ctype.contains("svg") {
                    let body = self.send(self.client.get(GetObject::new(&b, Some(&self.creds), key).sign(SIGN))).await?.bytes().await.map_err(|e| Error::other(e.to_string()))?;
                    return Ok(Content::Image(body.to_vec()));
                }
                if size > 0 && size <= openvclient_core::resource::IMAGE_PREVIEW_MAX && (ctype.contains("pdf") || openvclient_core::resource::is_pdf_name(key)) {
                    let body = self.send(self.client.get(GetObject::new(&b, Some(&self.creds), key).sign(SIGN))).await?.bytes().await.map_err(|e| Error::other(e.to_string()))?;
                    return Ok(Content::Pdf(body.to_vec()));
                }
                if size > 0 && size <= PREVIEW_MAX && textual {
                    let body = self.send(self.client.get(GetObject::new(&b, Some(&self.creds), key).sign(SIGN))).await?.bytes().await.map_err(|e| Error::other(e.to_string()))?;
                    let text = String::from_utf8_lossy(&body).into_owned();
                    if (ctype.contains("json") || key.ends_with(".json"))
                        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
                    {
                        return Ok(Content::Json(serde_json::to_string_pretty(&v).unwrap_or(text)));
                    }
                    return Ok(Content::Text(text));
                }
                props.push(("Preview".into(), if textual { "Too large to preview; use Download".into() } else { "Binary object; use Download".into() }));
                Ok(Content::Props(props))
            }
            _ => Ok(Content::Empty(String::new())),
        }
    }

    fn filter_hint(&self, kind: &str) -> Option<&'static str> {
        matches!(kind, "bucket" | "prefix").then_some("Name prefix within this folder, e.g. 2024/")
    }

    fn actions(&self, kind: &str) -> Vec<ActionSpec> {
        let upload = || {
            ActionSpec::new("upload", "Upload file…")
                .primary()
                .field(FieldSpec::text("file", "File").kind(FieldKind::File { save: false, filters: &[] }).required())
                .field(FieldSpec::text("name", "Object name").placeholder("same as file name"))
        };
        match kind {
            "" => vec![ActionSpec::new("create_bucket", "New bucket…").field(FieldSpec::text("name", "Bucket name").required())],
            "bucket" => vec![
                upload(),
                ActionSpec::new("mkdir", "New folder…").field(FieldSpec::text("name", "Folder").required()),
                ActionSpec::new("delete_bucket", "Delete bucket (must be empty)").destructive(),
            ],
            "prefix" => vec![
                upload(),
                ActionSpec::new("mkdir", "New folder…").field(FieldSpec::text("name", "Folder").required()),
                ActionSpec::new("delete_prefix", "Delete folder and contents").destructive(),
            ],
            "object" => vec![
                ActionSpec::new("download", "Download…").primary().field(FieldSpec::text("file", "Save as").kind(FieldKind::File { save: true, filters: &[] }).required()),
                ActionSpec::new("presign", "Share link…").field(FieldSpec::text("hours", "Valid for (hours)").kind(FieldKind::Number).default("1")),
                ActionSpec::new("delete_object", "Delete object").destructive(),
            ],
            _ => vec![],
        }
    }

    async fn run_action(&self, node: &str, action: &str, a: Args) -> Result<ActionOutcome> {
        if !matches!(action, "download" | "presign") {
            self.guard()?;
        }
        if action == "create_bucket" {
            let name = arg(&a, "name")?.trim().to_string();
            let b = self.bucket(&name)?;
            let mut req = self.client.put(CreateBucket::new(&b, &self.creds).sign(SIGN));
            if self.region != "us-east-1" {
                req = req.body(format!("<CreateBucketConfiguration><LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>", self.region));
            }
            self.send(req).await?;
            return Ok(ActionOutcome::msg(format!("Created bucket {name}")).refresh(""));
        }
        let (k, bucket, key) = parse(node).ok_or_else(|| Error::invalid("Bad node"))?;
        let b = self.bucket(bucket)?;
        let folder_id = |prefix: &str| if prefix.is_empty() { format!("b/{bucket}") } else { format!("p/{bucket}/{prefix}") };
        match action {
            "upload" => {
                let path = std::path::PathBuf::from(arg(&a, "file")?);
                let name = a.get("name").filter(|n| !n.trim().is_empty()).cloned().unwrap_or_else(|| path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
                let object = format!("{key}{}", name.trim_start_matches('/'));
                let file = tokio::fs::File::open(&path).await.map_err(|e| Error::io(format!("{}: {e}", path.display())))?;
                let len = file.metadata().await.map_err(|e| Error::io(e.to_string()))?.len();
                let body = reqwest::Body::wrap_stream(tokio_util_stream(file));
                self.send(self.client.put(PutObject::new(&b, Some(&self.creds), &object).sign(SIGN)).header("content-length", len).body(body)).await?;
                Ok(ActionOutcome::msg(format!("Uploaded {} to s3://{bucket}/{object}", human(len))).refresh(folder_id(key)))
            }
            "mkdir" => {
                let name = arg(&a, "name")?.trim().trim_matches('/');
                let object = format!("{key}{name}/");
                self.send(self.client.put(PutObject::new(&b, Some(&self.creds), &object).sign(SIGN)).header("content-length", 0).body(Vec::new())).await?;
                Ok(ActionOutcome::msg(format!("Created folder {name}/")).refresh(folder_id(key)))
            }
            "download" => {
                let path = std::path::PathBuf::from(arg(&a, "file")?);
                let mut resp = self.send(self.client.get(GetObject::new(&b, Some(&self.creds), key).sign(SIGN))).await?;
                let mut f = tokio::fs::File::create(&path).await.map_err(|e| Error::io(format!("{}: {e}", path.display())))?;
                let mut n = 0u64;
                use tokio::io::AsyncWriteExt;
                while let Some(chunk) = resp.chunk().await.map_err(|e| Error::connection(e.to_string()))? {
                    n += chunk.len() as u64;
                    f.write_all(&chunk).await.map_err(|e| Error::io(e.to_string()))?;
                }
                f.flush().await.map_err(|e| Error::io(e.to_string()))?;
                Ok(ActionOutcome::msg(format!("Saved {} to {}", human(n), path.display())))
            }
            "presign" => {
                let hours: u64 = a.get("hours").and_then(|h| h.parse().ok()).unwrap_or(1).clamp(1, 168);
                let url = GetObject::new(&b, Some(&self.creds), key).sign(Duration::from_secs(hours * 3600));
                Ok(ActionOutcome::msg(format!("Link valid for {hours} h")).show(Content::Text(url.to_string())))
            }
            "delete_object" => {
                self.send(self.client.delete(DeleteObject::new(&b, Some(&self.creds), key).sign(SIGN))).await?;
                let parent = key.rsplit_once('/').map(|(p, _)| format!("{p}/")).unwrap_or_default();
                Ok(ActionOutcome::msg(format!("Deleted {}", leaf(key))).refresh(folder_id(&parent)))
            }
            "delete_prefix" if k == 'p' => {
                // Delete every object under the prefix, page by page.
                let mut deleted = 0;
                loop {
                    let mut l = ListObjectsV2::new(&b, Some(&self.creds));
                    l.with_prefix(key.to_string());
                    l.with_max_keys(1000);
                    let body = self.send(self.client.get(l.sign(SIGN))).await?.text().await.map_err(|e| Error::other(e.to_string()))?;
                    let r = ListObjectsV2::parse_response(&body).map_err(|e| Error::other(e.to_string()))?;
                    if r.contents.is_empty() {
                        break;
                    }
                    for o in &r.contents {
                        self.send(self.client.delete(DeleteObject::new(&b, Some(&self.creds), &o.key).sign(SIGN))).await?;
                        deleted += 1;
                    }
                }
                let parent = key.trim_end_matches('/').rsplit_once('/').map(|(p, _)| format!("{p}/")).unwrap_or_default();
                Ok(ActionOutcome::msg(format!("Deleted {deleted} object(s)")).refresh(folder_id(&parent)))
            }
            "delete_bucket" => {
                self.send(self.client.delete(DeleteBucket::new(&b, &self.creds).sign(SIGN))).await?;
                Ok(ActionOutcome::msg(format!("Deleted bucket {bucket}")).refresh(""))
            }
            other => Err(Error::unsupported(format!("Unknown action {other}"))),
        }
    }
}

/// Stream a file in 256 KiB chunks so uploads don't load it into memory.
fn tokio_util_stream(file: tokio::fs::File) -> futures_core_stream::FileStream {
    futures_core_stream::FileStream::new(file)
}

mod futures_core_stream {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, ReadBuf};

    pub use futures_core::Stream;

    pub struct FileStream {
        file: tokio::fs::File,
        buf: Vec<u8>,
    }

    impl FileStream {
        pub fn new(file: tokio::fs::File) -> Self {
            Self { file, buf: vec![0; 256 * 1024] }
        }
    }

    impl Stream for FileStream {
        type Item = std::io::Result<bytes::Bytes>;
        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let this = self.get_mut();
            let mut rb = ReadBuf::new(&mut this.buf);
            match Pin::new(&mut this.file).poll_read(cx, &mut rb) {
                Poll::Ready(Ok(())) if rb.filled().is_empty() => Poll::Ready(None),
                Poll::Ready(Ok(())) => Poll::Ready(Some(Ok(bytes::Bytes::copy_from_slice(rb.filled())))),
                Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
                Poll::Pending => Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_ids() {
        assert_eq!(parse("o/bkt/a/b/c.txt"), Some(('o', "bkt", "a/b/c.txt")));
        assert_eq!(parse("b/bkt"), Some(('b', "bkt", "")));
        assert_eq!(leaf("a/b/"), "b");
        assert!(matches!(s3_err(403, "<Error><Code>AccessDenied</Code><Message>no</Message></Error>").kind, openvclient_core::ErrorKind::Auth));
    }

    #[tokio::test]
    async fn live_minio() {
        let mut v = std::collections::BTreeMap::new();
        v.insert("endpoint".into(), std::env::var("OPENVCLIENT_S3_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:59000".into()));
        v.insert("access_key".into(), "ovcadmin".into());
        v.insert("path_style".into(), "true".into());
        let h = match S3Driver::default().connect(ConnectParams { values: v, secret: Some("ovcpass123".into()), read_only: false, notifier: None }).await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("skipping: no S3 endpoint ({e})");
                return;
            }
        };
        let s = h.resource().unwrap().clone();
        let mut a = Args::new();
        a.insert("name".into(), "ovc-test".into());
        let _ = s.run_action("p/ovc-test/docs/", "delete_prefix", Args::new()).await;
        let _ = s.run_action("b/ovc-test", "delete_bucket", Args::new()).await;
        s.run_action("", "create_bucket", a).await.unwrap();
        let dir = std::env::temp_dir().join("ovc-s3-test");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("hello.json");
        std::fs::write(&src, r#"{"hello": "world"}"#).unwrap();
        let mut m = Args::new();
        m.insert("name".into(), "docs".into());
        s.run_action("b/ovc-test", "mkdir", m).await.unwrap();
        let mut u = Args::new();
        u.insert("file".into(), src.display().to_string());
        s.run_action("p/ovc-test/docs/", "upload", u).await.unwrap();
        let top = s.children(Some("b/ovc-test")).await.unwrap();
        assert!(top.iter().any(|n| n.id == "p/ovc-test/docs/"), "{:?}", top.iter().map(|n| &n.id).collect::<Vec<_>>());
        let kids = s.children(Some("p/ovc-test/docs/")).await.unwrap();
        assert!(kids.iter().any(|n| n.id == "o/ovc-test/docs/hello.json"), "{:?}", kids.iter().map(|n| &n.id).collect::<Vec<_>>());
        let l = s.list_dir(Some("p/ovc-test/docs/"), "").await.unwrap();
        let f = l.entries.iter().find(|e| e.name == "hello.json").expect("listed");
        assert!(!f.is_dir && f.size == Some(18) && f.modified.is_some(), "{f:?}");
        assert!(s.list_dir(Some("b/ovc-test"), "").await.unwrap().entries.iter().any(|e| e.is_dir && e.name == "docs"));
        assert_eq!(s.breadcrumbs("o/ovc-test/docs/hello.json").len(), 2);
        let Content::Json(j) = s.open("o/ovc-test/docs/hello.json", OpenOptions::default()).await.unwrap() else { panic!("json preview") };
        assert!(j.contains("world"));
        assert!(matches!(s.open("b/ovc-test", OpenOptions { limit: 100, ..Default::default() }).await.unwrap(), Content::TableWithNote(..)));
        let out = dir.join("down.json");
        let mut d = Args::new();
        d.insert("file".into(), out.display().to_string());
        s.run_action("o/ovc-test/docs/hello.json", "download", d).await.unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), r#"{"hello": "world"}"#);
        let link = s.run_action("o/ovc-test/docs/hello.json", "presign", Args::new()).await.unwrap();
        let Some(Content::Text(url)) = link.content else { panic!() };
        assert_eq!(reqwest::get(&url).await.unwrap().text().await.unwrap(), r#"{"hello": "world"}"#);
        s.run_action("p/ovc-test/docs/", "delete_prefix", Args::new()).await.unwrap();
        s.run_action("b/ovc-test", "delete_bucket", Args::new()).await.unwrap();
    }
}
