//! Microsoft SQL Server connector (tiberius, pure-Rust TDS). Metadata from
//! the sys.* catalog views; plans from SHOWPLAN_ALL / STATISTICS PROFILE.

mod aad;
mod meta;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::TryStreamExt;
use openvclient_core::explain::{ExplainResult, PlanNode};
use openvclient_core::sql::*;
use openvclient_core::sqltext::{Dialect, leading_keyword, split_statements};
use openvclient_core::{Category, ColumnMeta, ConnectParams, Driver, DriverInfo, Error, ErrorKind, FieldKind, FieldSpec, Result, ResultSet, ServerInfo, ServiceSession, SessionHandle, Value};
use parking_lot::Mutex;
use tiberius::{AuthMethod, Client, ColumnData, Config, EncryptionLevel, QueryItem};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

pub(crate) const D: Dialect = Dialect::SqlServer;
pub(crate) type Conn = Client<Compat<TcpStream>>;

pub struct SqlServerDriver {
    info: DriverInfo,
}

impl Default for SqlServerDriver {
    fn default() -> Self {
        const AUTH: &[&str] = &["sql", "windows", "integrated", "entra_password", "entra_service_principal", "entra_managed_identity", "entra_interactive", "entra_default", "entra_token"];
        const WITH_USER: &[&str] = &["sql", "windows", "entra_password"];
        const WITH_SECRET: &[&str] = &["sql", "windows", "entra_password", "entra_service_principal", "entra_token"];
        let mut fields = vec![
            FieldSpec::text("host", "Host").default("localhost").required(),
            FieldSpec::text("port", "Port").kind(FieldKind::Number).default("1433").help("Leave empty with an instance name to use SQL Server Browser"),
            FieldSpec::text("auth", "Authentication").kind(FieldKind::Choice(AUTH)).default("sql").help(
                "sql: SQL Server login · windows: Windows/NTLM user (DOMAIN\\user) · integrated: current Windows login · entra_*: Microsoft Entra ID (Azure AD) password, service principal, managed identity, interactive device code, default credential chain or a raw access token",
            ),
            FieldSpec::text("user", "User").default("sa").show_if("auth", WITH_USER).help("Windows: DOMAIN\\user · Entra password: user@tenant.com"),
            FieldSpec::text("password", "Password / secret / token").kind(FieldKind::Secret).show_if("auth", WITH_SECRET),
            FieldSpec::text("tenant_id", "Tenant ID").placeholder("organizations").show_if("auth", &["entra_password", "entra_service_principal", "entra_interactive"]),
            FieldSpec::text("client_id", "Client (application) ID").placeholder("default public client").show_if("auth", &["entra_password", "entra_service_principal", "entra_interactive", "entra_managed_identity"]).help("Service principal app id, or a user-assigned managed identity"),
            FieldSpec::text("database", "Database").default("master"),
            FieldSpec::text("instance", "Instance name").placeholder("e.g. SQLEXPRESS (resolved via SQL Server Browser)").advanced(),
        ];
        fields.push(
            FieldSpec::text("encrypt", "Encryption")
                .kind(FieldKind::Choice(&["required", "on", "off"]))
                .default("required")
                .help("required: encrypt and verify the server certificate · on: encrypt but trust any certificate (self-signed servers; open to interception) · off: only the login is encrypted")
                .advanced(),
        );
        fields.push(FieldSpec::text("connect_timeout", "Connect timeout (s)").kind(FieldKind::Number).default("15").advanced());
        Self { info: DriverInfo { id: "sqlserver", name: "SQL Server", category: Category::Relational, description: "Microsoft SQL Server 2012+ and Azure SQL (SQL logins)", badge: "MS", fields } }
    }
}

#[derive(Clone)]
enum Auth {
    Sql {
        user: String,
        password: String,
    },
    #[cfg_attr(not(windows), allow(dead_code))]
    Windows {
        user: String,
        password: String,
    },
    Integrated,
    Entra(aad::Flow),
    Token(String),
}

#[derive(Clone)]
pub(crate) struct Target {
    host: String,
    port: Option<u16>,
    instance: Option<String>,
    auth: Auth,
    token_cache: Arc<Mutex<aad::Cache>>,
    notifier: Option<openvclient_core::Notifier>,
    pub database: String,
    encrypt: String,
    timeout: Duration,
    pub read_only: bool,
}

impl Target {
    async fn auth_method(&self) -> Result<AuthMethod> {
        Ok(match &self.auth {
            Auth::Sql { user, password } => AuthMethod::sql_server(user, password),
            #[cfg(windows)]
            Auth::Windows { user, password } => AuthMethod::windows(user, password),
            #[cfg(windows)]
            Auth::Integrated => AuthMethod::Integrated,
            #[cfg(not(windows))]
            Auth::Windows { .. } | Auth::Integrated => {
                return Err(Error::unsupported(
                    "Windows and integrated authentication use the Windows security provider (SSPI) and are available in the Windows build. On macOS/Linux use a SQL login or a Microsoft Entra ID method.",
                ));
            }
            Auth::Token(t) => AuthMethod::aad_token(t),
            Auth::Entra(flow) => {
                let cached = self.token_cache.lock().get();
                let token = match cached {
                    Some(t) => t,
                    None => {
                        let (t, exp) = aad::acquire(flow, self.notifier.as_ref()).await?;
                        self.token_cache.lock().put(t.clone(), exp);
                        t
                    }
                };
                AuthMethod::aad_token(token)
            }
        })
    }

    pub async fn connect(&self, database: &str) -> Result<Conn> {
        let mut c = Config::new();
        c.host(&self.host);
        if let Some(p) = self.port {
            c.port(p);
        }
        if let Some(i) = &self.instance {
            c.instance_name(i);
        }
        c.database(database);
        c.application_name("OpenVClient");
        c.authentication(self.auth_method().await?);
        match self.encrypt.as_str() {
            "off" => c.encryption(EncryptionLevel::Off),
            "required" => c.encryption(EncryptionLevel::Required),
            _ => {
                c.encryption(EncryptionLevel::Required);
                c.trust_cert();
            }
        }
        if self.read_only {
            c.readonly(true);
        }
        let fut = async {
            let tcp = if self.instance.is_some() && self.port.is_none() {
                // SQL Server Browser (UDP 1434) resolves the instance's port.
                use tiberius::SqlBrowser;
                TcpStream::connect_named(&c).await.map_err(|e| Error::connection(format!("SQL Server Browser on {} could not resolve the instance: {e}", self.host)))?
            } else {
                TcpStream::connect(c.get_addr()).await.map_err(|e| Error::connection(format!("{}: {e}", c.get_addr())))?
            };
            tcp.set_nodelay(true).ok();
            // A long statement (DBCC SHRINKFILE, index rebuilds, big UPDATEs) sends
            // nothing until it finishes; firewalls, NAT and cloud load balancers
            // (Azure: 4 minutes) drop such idle connections, which ended the
            // query. TCP keepalives keep the path open, like SSMS does.
            let ka = socket2::TcpKeepalive::new().with_time(Duration::from_secs(30)).with_interval(Duration::from_secs(10));
            if let Err(e) = socket2::SockRef::from(&tcp).set_tcp_keepalive(&ka) {
                tracing::debug!(target: "openvclient::conn", error = %e, "could not enable TCP keepalive");
            }
            Client::connect(c, tcp.compat_write()).await.map_err(ms_err)
        };
        // Device-code sign-in waits for the user, so it gets the full sign-in window.
        let interactive = matches!(self.auth, Auth::Entra(aad::Flow::DeviceCode { .. })) && self.token_cache.lock().get().is_none();
        let limit = if interactive { Duration::from_secs(900) } else { self.timeout };
        tokio::time::timeout(limit, fut).await.map_err(|_| Error::connection(format!("Timed out connecting to {}", self.host)))?
    }
}

pub(crate) fn ms_err(e: tiberius::error::Error) -> Error {
    match e {
        tiberius::error::Error::Server(t) => {
            let kind = match t.code() {
                18456 | 18452 => ErrorKind::Auth,
                4060 | 233 | 10054 | 596 => ErrorKind::Connection,
                // Severity 20+ terminates the connection.
                _ if t.class() >= 20 => ErrorKind::Connection,
                3617 => ErrorKind::Cancelled,
                _ => ErrorKind::Server,
            };
            if kind == ErrorKind::Cancelled {
                return Error::cancelled();
            }
            Error::new(kind, t.message().to_string()).with_detail(format!("Msg {}, Level {}, State {}, Line {}", t.code(), t.class(), t.state(), t.line()))
        }
        tiberius::error::Error::Io { message, .. } => Error::connection(message),
        tiberius::error::Error::Tls(m) => Error::connection(format!("TLS: {m}")),
        other => Error::other(other.to_string()),
    }
}

pub(crate) fn type_name(c: &tiberius::Column) -> String {
    use tiberius::ColumnType::*;
    match c.column_type() {
        Null => "null",
        Bit | Bitn => "bit",
        Int1 => "tinyint",
        Int2 => "smallint",
        Int4 => "int",
        Int8 => "bigint",
        Intn => "int",
        Float4 => "real",
        Float8 | Floatn => "float",
        Money | Money4 => "money",
        Decimaln | Numericn => "decimal",
        Datetime | Datetimen => "datetime",
        Datetime4 => "smalldatetime",
        Daten => "date",
        Timen => "time",
        Datetime2 => "datetime2",
        DatetimeOffsetn => "datetimeoffset",
        Guid => "uniqueidentifier",
        BigVarChar | BigChar => "varchar",
        NVarchar | NChar => "nvarchar",
        Text => "text",
        NText => "ntext",
        BigVarBin | BigBinary => "varbinary",
        Image => "image",
        Xml => "xml",
        Udt => "udt",
        SSVariant => "sql_variant",
    }
    .to_string()
}

/// Cell → Value, rendering temporal types the way SQL Server prints them.
pub(crate) fn to_value(d: &ColumnData<'static>) -> Value {
    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
    use tiberius::FromSql;
    let s = |o: Option<String>| o.map(Value::Text).unwrap_or(Value::Null);
    match d {
        ColumnData::U8(v) => v.map(|x| Value::Int(x as i64)).unwrap_or(Value::Null),
        ColumnData::I16(v) => v.map(|x| Value::Int(x as i64)).unwrap_or(Value::Null),
        ColumnData::I32(v) => v.map(|x| Value::Int(x as i64)).unwrap_or(Value::Null),
        ColumnData::I64(v) => v.map(Value::Int).unwrap_or(Value::Null),
        ColumnData::F32(v) => v.map(|x| Value::Float(x as f64)).unwrap_or(Value::Null),
        ColumnData::F64(v) => v.map(Value::Float).unwrap_or(Value::Null),
        ColumnData::Bit(v) => v.map(Value::Bool).unwrap_or(Value::Null),
        ColumnData::String(v) => s(v.as_ref().map(|x| x.to_string())),
        ColumnData::Guid(v) => s(v.map(|g| g.to_string().to_uppercase())),
        ColumnData::Binary(v) => v.as_ref().map(|b| Value::Bytes(b.to_vec())).unwrap_or(Value::Null),
        ColumnData::Numeric(v) => s(v.map(|n| n.to_string())),
        ColumnData::Xml(v) => s(v.as_ref().map(|x| x.to_string())),
        ColumnData::DateTime(_) | ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_) => s(NaiveDateTime::from_sql(d).ok().flatten().map(|t| t.format("%Y-%m-%d %H:%M:%S%.f").to_string())),
        ColumnData::Date(_) => s(NaiveDate::from_sql(d).ok().flatten().map(|t| t.format("%Y-%m-%d").to_string())),
        ColumnData::Time(_) => s(NaiveTime::from_sql(d).ok().flatten().map(|t| t.format("%H:%M:%S%.f").to_string())),
        ColumnData::DateTimeOffset(_) => s(DateTime::<FixedOffset>::from_sql(d).ok().flatten().map(|t| t.format("%Y-%m-%d %H:%M:%S%.f %:z").to_string())),
    }
}

/// Run one batch, keeping the first result set (up to `max` rows) and the
/// total row count of DML statements.
pub(crate) async fn run_batch(c: &mut Conn, sql: &str, max: usize) -> Result<StatementOutcome> {
    let kw = leading_keyword(sql);
    if matches!(kw.as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE") {
        let r = c.execute(sql, &[]).await.map_err(ms_err)?;
        return Ok(StatementOutcome::Affected(r.total()));
    }
    let mut stream = c.simple_query(sql).await.map_err(ms_err)?;
    let mut set: Option<ResultSet> = None;
    let mut first_index = None;
    while let Some(item) = stream.try_next().await.map_err(ms_err)? {
        match item {
            QueryItem::Metadata(m) => {
                if set.is_none() {
                    first_index = Some(m.result_index());
                    set = Some(ResultSet::new(m.columns().iter().map(|c| ColumnMeta::new(c.name(), type_name(c))).collect()));
                } else if let Some(s) = &mut set {
                    // Further result sets (procedures) are not shown.
                    s.truncated = true;
                }
            }
            QueryItem::Row(row) => {
                let Some(s) = set.as_mut() else { continue };
                if Some(row.result_index()) != first_index {
                    continue;
                }
                if s.rows.len() < max {
                    s.rows.push(row.cells().map(|(_, d)| to_value(d)).collect());
                } else {
                    s.truncated = true;
                }
            }
        }
    }
    Ok(match set {
        Some(s) => StatementOutcome::Rows(s),
        None => StatementOutcome::Done(format!("{kw} completed")),
    })
}

/// Metadata helper: every cell as text.
pub(crate) async fn text_rows(c: &mut Conn, sql: &str, params: &[&dyn tiberius::ToSql]) -> Result<Vec<Vec<Option<String>>>> {
    let rows = c.query(sql, params).await.map_err(ms_err)?.into_first_result().await.map_err(ms_err)?;
    Ok(rows.iter().map(|r| r.cells().map(|(_, d)| to_value(d).to_text()).collect()).collect())
}

pub(crate) async fn result(c: &mut Conn, sql: &str, params: &[&dyn tiberius::ToSql]) -> Result<ResultSet> {
    let mut stream = c.query(sql, params).await.map_err(ms_err)?;
    let cols = stream.columns().await.map_err(ms_err)?.map(|c| c.to_vec()).unwrap_or_default();
    let rows = stream.into_first_result().await.map_err(ms_err)?;
    let mut rs = ResultSet::new(cols.iter().map(|c| ColumnMeta::new(c.name(), type_name(c))).collect());
    rs.rows = rows.iter().map(|r| r.cells().map(|(_, d)| to_value(d)).collect()).collect();
    Ok(rs)
}

#[async_trait]
impl Driver for SqlServerDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    async fn connect(&self, p: ConnectParams) -> Result<SessionHandle> {
        let secret = p.secret.clone().unwrap_or_default();
        let user = p.get_or("user", "sa").to_string();
        let tenant = p.get_or("tenant_id", "").to_string();
        let client_id = p.get("client_id").map(str::to_owned);
        let auth = match p.get_or("auth", "sql") {
            "windows" => Auth::Windows { user: user.clone(), password: secret.clone() },
            "integrated" => Auth::Integrated,
            "entra_password" => Auth::Entra(aad::Flow::Password { tenant, client_id: client_id.unwrap_or_else(|| aad::DEFAULT_PUBLIC_CLIENT.into()), user: user.clone(), password: secret.clone() }),
            "entra_service_principal" => {
                let cid = client_id.ok_or_else(|| Error::invalid("Client (application) ID is required for a service principal"))?;
                if secret.is_empty() {
                    return Err(Error::invalid("The client secret is required for a service principal"));
                }
                Auth::Entra(aad::Flow::ServicePrincipal { tenant, client_id: cid, secret: secret.clone() })
            }
            "entra_managed_identity" => Auth::Entra(aad::Flow::ManagedIdentity { client_id }),
            "entra_interactive" => Auth::Entra(aad::Flow::DeviceCode { tenant, client_id: client_id.unwrap_or_else(|| aad::DEFAULT_PUBLIC_CLIENT.into()) }),
            "entra_default" => Auth::Entra(aad::Flow::Default),
            "entra_token" => {
                if secret.is_empty() {
                    return Err(Error::invalid("Paste the access token into the password field"));
                }
                Auth::Token(secret.clone())
            }
            _ => Auth::Sql { user: user.clone(), password: secret.clone() },
        };
        let instance = p.get("instance").map(str::to_owned);
        let port = match (p.get("port"), &instance) {
            (None, Some(_)) => None,
            _ => Some(p.port(1433)?),
        };
        let target = Target {
            host: p.get_or("host", "localhost").to_string(),
            port,
            instance,
            auth,
            token_cache: Arc::new(Mutex::new(aad::Cache::default())),
            notifier: p.notifier.clone(),
            database: p.get_or("database", "master").to_string(),
            encrypt: p.get_or("encrypt", "required").to_string(),
            timeout: Duration::from_secs(p.get_or("connect_timeout", "15").parse().unwrap_or(15)),
            read_only: p.read_only,
        };
        let mut c = target.connect(&target.database).await?;
        let r = text_rows(
            &mut c,
            "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(64)), CAST(SERVERPROPERTY('Edition') AS nvarchar(128)), \
             CAST(SERVERPROPERTY('ProductLevel') AS nvarchar(32)), SUSER_SNAME(), CAST(SERVERPROPERTY('Collation') AS nvarchar(128)), \
             CAST(SERVERPROPERTY('EngineEdition') AS int)",
            &[],
        )
        .await?;
        let r = r.into_iter().next().unwrap_or_default();
        let g = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
        let major: u32 = g(0).split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let year = match major {
            16 => "2022",
            15 => "2019",
            14 => "2017",
            13 => "2016",
            12 => "2014",
            11 => "2012",
            17 => "2025",
            _ => "",
        };
        let azure = g(5) == "5";
        let info = ServerInfo {
            product: if azure { format!("Azure SQL Database {}", g(0)) } else { format!("SQL Server {year} {}", g(0)).replace("  ", " ") },
            version: g(0),
            details: vec![("Edition".into(), g(1)), ("Level".into(), g(2)), ("Login".into(), g(3)), ("Collation".into(), g(4))],
        };
        let mut conns = HashMap::new();
        conns.insert(target.database.clone(), Arc::new(tokio::sync::Mutex::new(c)));
        Ok(SessionHandle::Sql(Arc::new(MsSession { target, conns: Mutex::new(conns), info, azure })))
    }
}

pub(crate) struct MsSession {
    pub target: Target,
    /// One shared metadata connection per database.
    conns: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Conn>>>>,
    info: ServerInfo,
    pub azure: bool,
}

impl MsSession {
    pub async fn conn(&self, database: Option<&str>) -> Result<tokio::sync::OwnedMutexGuard<Conn>> {
        let db = database.unwrap_or(&self.target.database).to_string();
        let existing = self.conns.lock().get(&db).cloned();
        let slot = match existing {
            Some(s) => s,
            None => {
                let c = self.target.connect(&db).await?;
                let s = Arc::new(tokio::sync::Mutex::new(c));
                self.conns.lock().insert(db.clone(), s.clone());
                s
            }
        };
        let mut guard = slot.lock_owned().await;
        // Reconnect transparently if the cached connection died (KILL, failover).
        let alive = match guard.simple_query("SELECT 1").await {
            Ok(s) => s.into_results().await.is_ok(),
            Err(_) => false,
        };
        if !alive {
            *guard = self.target.connect(&db).await?;
        }
        Ok(guard)
    }

    pub fn db_of(&self, p: &ObjPath) -> String {
        p.database.clone().unwrap_or_else(|| self.target.database.clone())
    }

    pub async fn run_on(&self, database: Option<&str>, sql: &str) -> Result<u64> {
        let mut c = self.conn(database).await?;
        let mut total = 0;
        for span in split_statements(D, sql) {
            if let StatementOutcome::Affected(n) = run_batch(&mut c, &sql[span.start..span.end], 0).await? {
                total += n;
            }
        }
        Ok(total)
    }
}

#[async_trait]
impl ServiceSession for MsSession {
    fn server_info(&self) -> ServerInfo {
        self.info.clone()
    }

    async fn ping(&self) -> Result<Duration> {
        let t = Instant::now();
        let mut c = self.conn(None).await?;
        c.simple_query("SELECT 1").await.map_err(ms_err)?.into_results().await.map_err(ms_err)?;
        Ok(t.elapsed())
    }

    async fn close(&self) {
        self.conns.lock().clear();
    }
}

#[async_trait]
impl SqlSession for MsSession {
    fn dialect(&self) -> Dialect {
        D
    }

    fn capabilities(&self) -> SqlCapabilities {
        let rw = !self.target.read_only;
        SqlCapabilities {
            multiple_databases: true,
            schemas: true,
            create_database: rw,
            drop_database: rw,
            database_offline: rw && !self.azure,
            create_schema: rw,
            explain: true,
            explain_analyze: true,
            activity: true,
            // T-SQL has no "cancel this request" statement; KILL ends the session.
            cancel_query: false,
            terminate_session: true,
            locks: true,
            metrics: true,
            users: true,
            roles: true,
            grants: rw,
            edit_data: rw,
            truncate: rw,
            backup: !self.azure,
            restore: rw && !self.azure,
            server_side_backup: true,
            sequences: true,
            procedures: true,
            functions: true,
            triggers: true,
            ..Default::default()
        }
    }

    fn default_database(&self) -> Option<String> {
        Some(self.target.database.clone())
    }

    async fn children(&self, node: Option<(ObjectKind, ObjPath)>) -> Result<Vec<TreeNode>> {
        meta::children(self, node).await
    }

    async fn ddl(&self, kind: ObjectKind, path: &ObjPath) -> Result<String> {
        meta::ddl(self, kind, path).await
    }

    async fn columns(&self, path: &ObjPath) -> Result<Vec<ColumnInfo>> {
        meta::columns(self, path).await
    }

    async fn table_page(&self, q: TableQuery) -> Result<TablePage> {
        meta::table_page(self, q).await
    }

    async fn count_rows(&self, path: &ObjPath, filter: Option<&str>) -> Result<u64> {
        let mut sql = format!("SELECT COUNT_BIG(*) FROM {}", D.qualified(path.schema.as_deref(), path.name()?));
        if let Some(f) = filter.filter(|f| !f.trim().is_empty()) {
            meta::single_statement(f)?;
            sql.push_str(&format!(" WHERE {f}"));
        }
        let mut c = self.conn(Some(&self.db_of(path))).await?;
        let rows = text_rows(&mut c, &sql, &[]).await?;
        Ok(rows.first().and_then(|r| r[0].as_ref()).and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    async fn apply_changes(&self, path: &ObjPath, changes: Vec<RowChange>) -> Result<u64> {
        meta::apply_changes(self, path, changes).await
    }

    async fn catalog(&self, database: Option<&str>) -> Result<Catalog> {
        meta::catalog(self, database).await
    }

    async fn open_console(&self, database: Option<String>) -> Result<Arc<dyn SqlConsole>> {
        let db = database.unwrap_or_else(|| self.target.database.clone());
        let mut c = self.target.connect(&db).await?;
        let spid = text_rows(&mut c, "SELECT CAST(@@SPID AS int)", &[]).await?.first().and_then(|r| r[0].clone()).unwrap_or_default();
        Ok(Arc::new(MsConsole {
            conn: tokio::sync::Mutex::new(Some(c)),
            spid: Mutex::new(spid),
            target: self.target.clone(),
            database: Mutex::new(db),
            rt: tokio::runtime::Handle::current(),
            suspect: Default::default(),
            cancel_requested: Default::default(),
        }))
    }

    async fn run(&self, database: Option<&str>, sql: &str) -> Result<u64> {
        self.run_on(database, sql).await
    }

    async fn drop_object(&self, kind: ObjectKind, path: &ObjPath, cascade: bool) -> Result<()> {
        meta::drop_object(self, kind, path, cascade).await
    }

    async fn truncate(&self, path: &ObjPath) -> Result<()> {
        let sql = format!("TRUNCATE TABLE {}", D.qualified(path.schema.as_deref(), path.name()?));
        self.run_on(Some(&self.db_of(path)), &sql).await.map(|_| ())
    }

    async fn create_database(&self, spec: CreateDatabaseSpec) -> Result<()> {
        if spec.name.trim().is_empty() {
            return Err(Error::invalid("Database name is required"));
        }
        let mut sql = format!("CREATE DATABASE {}", D.quote_ident(&spec.name));
        if let Some(c) = spec.collation.filter(|c| !c.is_empty()) {
            sql.push_str(&format!(" COLLATE {c}"));
        }
        self.run_on(Some("master"), &sql).await.map(|_| ())
    }

    async fn set_database_online(&self, database: &str, online: bool) -> Result<()> {
        let sql = if online {
            format!("ALTER DATABASE {} SET ONLINE", D.quote_ident(database))
        } else {
            // Disconnect other users so the state change does not wait forever.
            format!("ALTER DATABASE {} SET OFFLINE WITH ROLLBACK IMMEDIATE", D.quote_ident(database))
        };
        self.conns.lock().remove(database);
        self.run_on(Some("master"), &sql).await.map(|_| ())
    }

    async fn create_schema(&self, database: Option<&str>, name: &str) -> Result<()> {
        self.run_on(database, &format!("CREATE SCHEMA {}", D.quote_ident(name))).await.map(|_| ())
    }

    async fn activity(&self) -> Result<ActivitySnapshot> {
        let mut c = self.conn(Some("master")).await?;
        let rows = result(
            &mut c,
            "SELECT s.session_id AS spid, s.login_name AS [login], DB_NAME(s.database_id) AS [database], s.host_name AS host, \
             s.program_name AS program, s.status, r.command, r.wait_type, r.blocking_session_id AS blocked_by, \
             CONVERT(varchar(12), DATEADD(ms, r.total_elapsed_time, 0), 114) AS running_for, \
             CASE WHEN s.session_id = @@SPID THEN '(this connection) ' ELSE '' END + COALESCE(t.text, '') AS query \
             FROM sys.dm_exec_sessions s LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id \
             OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t WHERE s.is_user_process = 1 \
             ORDER BY CASE WHEN r.session_id IS NULL THEN 1 ELSE 0 END, s.session_id",
            &[],
        )
        .await?;
        Ok(ActivitySnapshot { rows, id_column: 0 })
    }

    async fn kill(&self, id: &str, _mode: KillMode) -> Result<()> {
        let n: i32 = id.parse().map_err(|_| Error::invalid(format!("Invalid session id {id}")))?;
        self.run_on(Some("master"), &format!("KILL {n}")).await.map(|_| ())
    }

    async fn locks(&self) -> Result<ResultSet> {
        let mut c = self.conn(Some("master")).await?;
        result(
            &mut c,
            "SELECT l.request_session_id AS spid, DB_NAME(l.resource_database_id) AS [database], l.resource_type, \
             l.request_mode AS mode, l.request_status AS status, \
             CASE WHEN l.resource_type = 'OBJECT' THEN OBJECT_NAME(l.resource_associated_entity_id, l.resource_database_id) END AS [object], \
             w.blocking_session_id AS blocked_by, w.wait_duration_ms \
             FROM sys.dm_tran_locks l LEFT JOIN sys.dm_os_waiting_tasks w ON w.resource_address = l.lock_owner_address \
             WHERE l.request_session_id <> @@SPID ORDER BY l.request_status DESC, l.request_session_id",
            &[],
        )
        .await
    }

    async fn metrics(&self) -> Result<Vec<Metric>> {
        meta::metrics(self).await
    }

    async fn principals(&self) -> Result<ResultSet> {
        let mut c = self.conn(Some("master")).await?;
        result(
            &mut c,
            "SELECT p.name, p.type_desc AS [type], CAST(p.is_disabled AS bit) AS disabled, p.default_database_name AS default_db, \
             STUFF((SELECT ', ' + r.name FROM sys.server_role_members m JOIN sys.server_principals r ON r.principal_id = m.role_principal_id \
                    WHERE m.member_principal_id = p.principal_id FOR XML PATH('')), 1, 2, '') AS server_roles, \
             CONVERT(varchar(19), p.create_date, 120) AS created \
             FROM sys.server_principals p WHERE p.type IN ('S','U','G','R') AND p.name NOT LIKE '##%' ORDER BY p.type, p.name",
            &[],
        )
        .await
    }

    async fn privileges(&self, principal: &str, _host: Option<&str>) -> Result<ResultSet> {
        let mut c = self.conn(None).await?;
        result(
            &mut c,
            "SELECT 'server' AS scope, NULL AS [object], pe.permission_name, pe.state_desc FROM sys.server_permissions pe \
               JOIN sys.server_principals p ON p.principal_id = pe.grantee_principal_id WHERE p.name = @P1 \
             UNION ALL \
             SELECT 'database ' + DB_NAME(), COALESCE(SCHEMA_NAME(o.schema_id) + '.' + o.name, s.name, DB_NAME()), pe.permission_name, pe.state_desc \
             FROM sys.database_permissions pe JOIN sys.database_principals dp ON dp.principal_id = pe.grantee_principal_id \
             LEFT JOIN sys.objects o ON pe.class = 1 AND o.object_id = pe.major_id \
             LEFT JOIN sys.schemas s ON pe.class = 3 AND s.schema_id = pe.major_id \
             WHERE dp.name = @P1 OR dp.sid = SUSER_SID(@P1)",
            &[&principal],
        )
        .await
    }

    async fn create_principal(&self, spec: PrincipalSpec) -> Result<()> {
        if spec.name.trim().is_empty() {
            return Err(Error::invalid("Login name is required"));
        }
        let who = D.quote_ident(&spec.name);
        let pw = spec.password.clone().unwrap_or_default();
        let mut sql = format!("CREATE LOGIN {who} WITH PASSWORD = {}, CHECK_POLICY = ON;", D.quote_literal(&pw));
        if !spec.can_login {
            sql.push_str(&format!("\nALTER LOGIN {who} DISABLE;"));
        }
        if spec.superuser {
            sql.push_str(&format!("\nALTER SERVER ROLE sysadmin ADD MEMBER {who};"));
        }
        if spec.create_db {
            sql.push_str(&format!("\nALTER SERVER ROLE dbcreator ADD MEMBER {who};"));
        }
        if spec.create_role {
            sql.push_str(&format!("\nALTER SERVER ROLE securityadmin ADD MEMBER {who};"));
        }
        self.run_on(Some("master"), &sql).await.map(|_| ())
    }

    async fn drop_principal(&self, name: &str, _host: Option<&str>) -> Result<()> {
        self.run_on(Some("master"), &format!("DROP LOGIN {}", D.quote_ident(name))).await.map(|_| ())
    }

    async fn set_password(&self, name: &str, _host: Option<&str>, password: &str) -> Result<()> {
        let sql = format!("ALTER LOGIN {} WITH PASSWORD = {}", D.quote_ident(name), D.quote_literal(password));
        self.run_on(Some("master"), &sql).await.map(|_| ())
    }

    async fn grant(&self, spec: GrantSpec, revoke: bool) -> Result<()> {
        if spec.privileges.is_empty() {
            return Err(Error::invalid("Select at least one privilege"));
        }
        let (db, on) = match &spec.target {
            GrantTarget::Database(d) => (Some(d.clone()), String::new()),
            GrantTarget::Schema(s) | GrantTarget::AllTablesInSchema(s) => (None, format!(" ON SCHEMA::{}", D.quote_ident(s))),
            GrantTarget::Table(p) => (p.database.clone(), format!(" ON OBJECT::{}", D.qualified(p.schema.as_deref().or(Some("dbo")), p.name()?))),
        };
        let user = D.quote_ident(&spec.principal);
        // Permissions belong to a database user; map the login if needed.
        let ensure = format!(
            "IF NOT EXISTS (SELECT 1 FROM sys.database_principals WHERE name = {lit}) AND EXISTS (SELECT 1 FROM sys.server_principals WHERE name = {lit}) CREATE USER {user} FOR LOGIN {user};",
            lit = D.quote_literal(&spec.principal)
        );
        let privs = spec.privileges.join(", ");
        let stmt = if revoke { format!("REVOKE {privs}{on} FROM {user}") } else { format!("GRANT {privs}{on} TO {user}{}", if spec.with_grant_option { " WITH GRANT OPTION" } else { "" }) };
        self.run_on(db.as_deref(), &format!("{ensure}\n{stmt}")).await.map(|_| ())
    }

    fn privilege_names(&self, target: &GrantTarget) -> Vec<&'static str> {
        match target {
            GrantTarget::Database(_) => vec!["CONNECT", "CREATE TABLE", "CREATE VIEW", "CREATE PROCEDURE", "CREATE FUNCTION", "BACKUP DATABASE", "VIEW DEFINITION", "SHOWPLAN"],
            GrantTarget::Schema(_) | GrantTarget::AllTablesInSchema(_) => vec!["SELECT", "INSERT", "UPDATE", "DELETE", "EXECUTE", "ALTER", "CONTROL", "VIEW DEFINITION"],
            GrantTarget::Table(_) => vec!["SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "ALTER", "CONTROL", "VIEW DEFINITION"],
        }
    }

    async fn backup(&self, req: BackupRequest) -> Result<String> {
        let db = req.database.clone().unwrap_or_else(|| self.target.database.clone());
        let path = req.path.display().to_string();
        let t = Instant::now();
        self.run_on(Some("master"), &format!("BACKUP DATABASE {} TO DISK = {} WITH INIT, CHECKSUM, STATS = 10", D.quote_ident(&db), D.quote_literal(&path))).await?;
        Ok(format!("BACKUP DATABASE {db} wrote {path} on the server in {}", openvclient_core::elapsed_ms(t.elapsed())))
    }

    async fn restore(&self, req: BackupRequest) -> Result<String> {
        let db = req.database.clone().unwrap_or_else(|| self.target.database.clone());
        let path = req.path.display().to_string();
        self.conns.lock().remove(&db);
        let t = Instant::now();
        let sql = format!(
            "ALTER DATABASE {q} SET SINGLE_USER WITH ROLLBACK IMMEDIATE;\nRESTORE DATABASE {q} FROM DISK = {p} WITH REPLACE;\nALTER DATABASE {q} SET MULTI_USER;",
            q = D.quote_ident(&db),
            p = D.quote_literal(&path)
        );
        self.run_on(Some("master"), &sql).await?;
        Ok(format!("RESTORE DATABASE {db} from {path} (server path) in {}", openvclient_core::elapsed_ms(t.elapsed())))
    }
}

struct MsConsole {
    conn: tokio::sync::Mutex<Option<Conn>>,
    spid: Mutex<String>,
    target: Target,
    database: Mutex<String>,
    rt: tokio::runtime::Handle,
    /// Set after any error; the next run checks the session is still alive.
    suspect: std::sync::atomic::AtomicBool,
    /// The user pressed Cancel (or the query timeout fired) for the current run.
    cancel_requested: Arc<std::sync::atomic::AtomicBool>,
}

/// The database named by a plain `USE x` statement, unquoted.
fn use_target(sql: &str) -> Option<String> {
    let s = sql.trim().trim_end_matches(';').trim();
    let rest = s.get(..4).filter(|p| p.eq_ignore_ascii_case("use ")).map(|_| s[4..].trim())?;
    let name = if let Some(r) = rest.strip_prefix('[') {
        r.strip_suffix(']')?.replace("]]", "]")
    } else if let Some(r) = rest.strip_prefix('"') {
        r.strip_suffix('"')?.replace("\"\"", "\"")
    } else {
        if rest.contains(char::is_whitespace) {
            return None;
        }
        rest.to_string()
    };
    (!name.is_empty()).then_some(name)
}

impl MsConsole {
    async fn ensure(&self, slot: &mut Option<Conn>) -> Result<()> {
        if self.suspect.swap(false, std::sync::atomic::Ordering::Relaxed)
            && let Some(c) = slot.as_mut()
        {
            let alive = match c.simple_query("SELECT 1").await {
                Ok(st) => st.into_results().await.is_ok(),
                Err(_) => false,
            };
            if !alive {
                *slot = None;
            }
        }
        if slot.is_none() {
            let db = self.database.lock().clone();
            let mut c = self.target.connect(&db).await?;
            let spid = text_rows(&mut c, "SELECT CAST(@@SPID AS int)", &[]).await?.first().and_then(|r| r[0].clone()).unwrap_or_default();
            *self.spid.lock() = spid;
            *slot = Some(c);
        }
        Ok(())
    }
}

#[async_trait]
impl SqlConsole for MsConsole {
    async fn execute(&self, statements: Vec<String>, opts: ExecOptions) -> Vec<StatementResult> {
        if self.target.read_only
            && let Some((i, word)) = statements.iter().enumerate().find_map(|(i, s)| writes(s).map(|w| (i, w)))
        {
            let mut out: Vec<StatementResult> = Vec::new();
            for (j, sql) in statements.into_iter().enumerate() {
                if j == i {
                    out.push(StatementResult { sql, outcome: Err(Error::invalid(format!("{word} is blocked on a read-only connection"))), elapsed: Duration::ZERO });
                    break;
                }
            }
            return out;
        }
        let mut slot = self.conn.lock().await;
        self.cancel_requested.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut out = Vec::new();
        for sql in statements {
            let t = Instant::now();
            let outcome = async {
                self.ensure(&mut slot).await?;
                let c = slot.as_mut().expect("connected");
                let r = run_batch(c, &sql, opts.max_rows).await;
                if let Err(e) = &r {
                    self.suspect.store(true, std::sync::atomic::Ordering::Relaxed);
                    if e.is_connection() {
                        // A new session opens on the next run either way; say
                        // which happened, so a dropped link is not shown as "Cancelled".
                        *slot = None;
                        if self.cancel_requested.load(std::sync::atomic::Ordering::SeqCst) {
                            return Err(Error::cancelled().with_detail("Cancelled. SQL Server has no per-statement cancel, so the session was ended (KILL); a new session opens on the next run."));
                        }
                        let mins = t.elapsed().as_secs() / 60;
                        let after = if mins > 0 { format!(" after {mins} min") } else { String::new() };
                        return Err(Error::connection(format!("The connection to the server was lost while the statement ran{after}."))
                            .with_detail(format!("{}\n\nThe statement may have finished on the server. Check before running it again. A new session opens on the next run.", e.message)));
                    }
                    // Azure SQL Database (and some proxies) refuse USE; open a
                    // session on that database instead, as SSMS's database picker does.
                    if let Some(db) = use_target(&sql)
                        && (e.message.contains("40508") || e.message.to_lowercase().contains("use statement is not supported"))
                    {
                        let mut fresh = self.target.connect(&db).await?;
                        let spid = text_rows(&mut fresh, "SELECT CAST(@@SPID AS int)", &[]).await?.first().and_then(|r| r[0].clone()).unwrap_or_default();
                        *self.spid.lock() = spid;
                        *self.database.lock() = db.clone();
                        *slot = Some(fresh);
                        self.suspect.store(false, std::sync::atomic::Ordering::Relaxed);
                        return Ok(StatementOutcome::Done(format!("Switched to database {db} (new session)")));
                    }
                }
                if r.is_ok()
                    && leading_keyword(&sql) == "USE"
                    && let Ok(rows) = text_rows(slot.as_mut().unwrap(), "SELECT DB_NAME()", &[]).await
                    && let Some(db) = rows.first().and_then(|r| r[0].clone())
                {
                    *self.database.lock() = db;
                }
                r
            }
            .await;
            let failed = outcome.is_err();
            out.push(StatementResult { sql, elapsed: t.elapsed(), outcome });
            if failed {
                break;
            }
        }
        out
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<ExplainResult> {
        let mut slot = self.conn.lock().await;
        self.ensure(&mut slot).await?;
        let c = slot.as_mut().unwrap();
        let (on, off) = if analyze { ("SET STATISTICS PROFILE ON", "SET STATISTICS PROFILE OFF") } else { ("SET SHOWPLAN_ALL ON", "SET SHOWPLAN_ALL OFF") };
        let wrap = analyze && !matches!(leading_keyword(sql).as_str(), "SELECT" | "WITH");
        if wrap {
            c.simple_query("BEGIN TRANSACTION").await.map_err(ms_err)?.into_results().await.map_err(ms_err)?;
        }
        c.simple_query(on).await.map_err(ms_err)?.into_results().await.map_err(ms_err)?;
        let sets = match c.simple_query(sql).await {
            Ok(s) => s.into_results().await.map_err(ms_err),
            Err(e) => Err(ms_err(e)),
        };
        if let Ok(s) = c.simple_query(off).await {
            let _ = s.into_results().await;
        }
        if wrap && let Ok(s) = c.simple_query("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await {
            let _ = s.into_results().await;
        }
        let sets = sets?;
        // The plan rows are the result set containing a StmtText column.
        let plan = sets.into_iter().rev().find(|rows| rows.first().is_some_and(|r| r.columns().iter().any(|c| c.name() == "StmtText"))).unwrap_or_default();
        let col = |r: &tiberius::Row, name: &str| r.columns().iter().position(|c| c.name() == name).and_then(|i| r.cells().nth(i).map(|(_, d)| to_value(d)));
        let mut nodes: Vec<(i64, i64, PlanNode)> = Vec::new();
        let mut raw = String::new();
        for r in &plan {
            let text = col(r, "StmtText").and_then(|v| v.to_text()).unwrap_or_default();
            raw.push_str(&text);
            raw.push('\n');
            let id = col(r, "NodeId").map(|v| v.as_f64() as i64).unwrap_or(0);
            let parent = col(r, "Parent").map(|v| v.as_f64() as i64).unwrap_or(0);
            let num = |n: &str| col(r, n).map(|v| v.as_f64()).filter(|v| v.is_finite());
            let mut props = Vec::new();
            for k in ["LogicalOp", "Argument", "OutputList", "Warnings"] {
                if let Some(v) = col(r, k).and_then(|v| v.to_text()).filter(|v| !v.is_empty()) {
                    props.push((k.to_string(), v));
                }
            }
            let title = col(r, "PhysicalOp").and_then(|v| v.to_text()).filter(|v| !v.is_empty()).unwrap_or_else(|| text.trim().chars().take(80).collect());
            nodes.push((
                id,
                parent,
                PlanNode { title, total_cost: num("TotalSubtreeCost"), plan_rows: num("EstimateRows"), actual_rows: num("Rows"), loops: num("Executes"), props, ..Default::default() },
            ));
        }
        fn build(parent: i64, nodes: &[(i64, i64, PlanNode)], depth: usize) -> Vec<PlanNode> {
            if depth > 64 {
                return Vec::new();
            }
            nodes.iter().filter(|n| n.1 == parent && n.0 != parent).map(|n| PlanNode { children: build(n.0, nodes, depth + 1), ..n.2.clone() }).collect()
        }
        let roots: Vec<PlanNode> = nodes.iter().filter(|n| n.1 == 0 && n.0 != 0 || n.0 == 1 && n.1 == 0).map(|n| PlanNode { children: build(n.0, &nodes, 0), ..n.2.clone() }).collect();
        let tree = if roots.is_empty() { None } else { Some(PlanNode { title: "Query plan".into(), children: roots, ..Default::default() }) };
        let mut summary = Vec::new();
        if wrap {
            summary.push(("Note".into(), "Statement ran inside a transaction that was rolled back".into()));
        }
        Ok(ExplainResult { statement: format!("{on}; {sql}"), raw, tree, table: None, summary })
    }

    fn cancel(&self) {
        // SQL Server has no per-request cancel statement; end the session.
        self.cancel_requested.store(true, std::sync::atomic::Ordering::SeqCst);
        let spid = self.spid.lock().clone();
        let target = self.target.clone();
        self.rt.spawn(async move {
            if let Ok(n) = spid.parse::<i32>()
                && let Ok(mut c) = target.connect("master").await
            {
                let _ = c.simple_query(format!("KILL {n}")).await;
            }
        });
    }

    fn current_database(&self) -> Option<String> {
        Some(self.database.lock().clone())
    }

    async fn close(&self) {
        *self.conn.lock().await = None;
    }
}

#[cfg(test)]
mod use_tests {
    #[test]
    fn parses_use_targets() {
        assert_eq!(super::use_target("USE [volza-www];").as_deref(), Some("volza-www"));
        assert_eq!(super::use_target("  use shop ").as_deref(), Some("shop"));
        assert_eq!(super::use_target("USE \"odd name\"").as_deref(), Some("odd name"));
        assert_eq!(super::use_target("USE [a]]b]").as_deref(), Some("a]b"));
        assert_eq!(super::use_target("USER_NAME()"), None);
        assert_eq!(super::use_target("USE a b"), None);
    }
}

#[cfg(test)]
mod tests;

/// First data- or schema-changing keyword in a batch, ignoring strings and
/// comments. Read-only connections refuse such statements.
fn writes(sql: &str) -> Option<String> {
    const WRITE: &[&str] = &[
        "INSERT",
        "UPDATE",
        "DELETE",
        "MERGE",
        "DROP",
        "ALTER",
        "CREATE",
        "TRUNCATE",
        "GRANT",
        "REVOKE",
        "DENY",
        "EXEC",
        "EXECUTE",
        "BACKUP",
        "RESTORE",
        "DBCC",
        "BULK",
        "INTO",
        "KILL",
        "SHUTDOWN",
        "RECONFIGURE",
        "OPENROWSET",
        "OPENQUERY",
        "OPENDATASOURCE",
        "WRITETEXT",
        "UPDATETEXT",
        "SP_EXECUTESQL",
        "XP_CMDSHELL",
    ];
    use openvclient_core::sqltext::{Dialect, TokenKind, tokenize};
    tokenize(Dialect::SqlServer, sql).into_iter().filter(|(k, _)| matches!(k, TokenKind::Keyword | TokenKind::Ident)).find_map(|(_, sp)| {
        let w = sql[sp.start..sp.end].to_ascii_uppercase();
        WRITE.contains(&w.as_str()).then_some(w)
    })
}

#[cfg(test)]
mod readonly_tests {
    #[test]
    fn detects_writes() {
        assert_eq!(super::writes("SELECT 'DROP TABLE x' AS t -- delete me"), None);
        assert_eq!(super::writes("SELECT * INTO copy FROM t").as_deref(), Some("INTO"));
        assert_eq!(super::writes("WITH c AS (SELECT 1 a) DELETE FROM t").as_deref(), Some("DELETE"));
        assert_eq!(super::writes("exec sp_who").as_deref(), Some("EXEC"));
    }
}
