//! PostgreSQL connector (tokio-postgres + deadpool). Metadata comes from the
//! system catalogs; query results use the text protocol so values appear
//! exactly as the server renders them.

mod admin;
mod console;
mod meta;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use futures::StreamExt;
use openvclient_core::sql::*;
use openvclient_core::sqltext::Dialect;
use openvclient_core::{
    Category, ColumnMeta, ConnectParams, Driver, DriverInfo, Error, ErrorKind, FieldKind, FieldSpec, Result, ResultSet, ServerInfo, ServiceSession, SessionHandle, Value, network_fields,
};
use parking_lot::Mutex;
use postgres_native_tls::MakeTlsConnector;
use tokio_postgres::{Client, SimpleQueryMessage, config::SslMode};

pub(crate) const D: Dialect = Dialect::Postgres;

pub struct PostgresDriver {
    info: DriverInfo,
}

impl Default for PostgresDriver {
    fn default() -> Self {
        let mut fields = network_fields("5432", "postgres");
        fields.push(FieldSpec::text("database", "Database").default("postgres"));
        fields.push(
            FieldSpec::text("sslmode", "SSL mode")
                .kind(FieldKind::Choice(&["prefer", "disable", "require", "verify-full"]))
                .default("prefer")
                .help("require encrypts without verifying the certificate; verify-full also checks host and CA.")
                .advanced(),
        );
        fields.push(FieldSpec::text("connect_timeout", "Connect timeout (s)").kind(FieldKind::Number).default("10").advanced());
        fields.push(FieldSpec::text("application_name", "Application name").default("OpenVClient").advanced());
        Self { info: DriverInfo { id: "postgres", name: "PostgreSQL", category: Category::Relational, description: "PostgreSQL 10+ and wire-compatible servers", badge: "PG", fields } }
    }
}

impl PostgresDriver {
    /// PostgreSQL-protocol engines. Catalog features follow PostgreSQL and
    /// are labelled as such until verified on the engine itself.
    pub fn flavor(id: &'static str, name: &'static str, description: &'static str, port: &'static str, user: &'static str, database: &'static str) -> Self {
        let mut d = Self::default();
        d.info.id = id;
        d.info.name = name;
        d.info.description = description;
        d.info.badge = "PG";
        for f in &mut d.info.fields {
            match f.key {
                "port" => f.default = port,
                "user" => f.default = user,
                "database" => f.default = database,
                _ => {}
            }
        }
        d
    }
}

#[derive(Clone)]
pub(crate) struct PgTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub database: String,
    pub sslmode: String,
    pub connect_timeout: Duration,
    pub application_name: String,
    pub read_only: bool,
}

impl PgTarget {
    fn from_params(p: &ConnectParams) -> Result<Self> {
        Ok(Self {
            host: p.get_or("host", "localhost").to_string(),
            port: p.port(5432)?,
            user: p.get_or("user", "postgres").to_string(),
            password: p.secret.clone().filter(|s| !s.is_empty()),
            database: p.get_or("database", "postgres").to_string(),
            sslmode: p.get_or("sslmode", "prefer").to_string(),
            connect_timeout: Duration::from_secs(p.get_or("connect_timeout", "10").parse().unwrap_or(10)),
            application_name: p.get_or("application_name", "OpenVClient").to_string(),
            read_only: p.read_only,
        })
    }

    pub fn config(&self, database: &str) -> tokio_postgres::Config {
        let mut c = tokio_postgres::Config::new();
        c.host(&self.host).port(self.port).user(&self.user).dbname(database).application_name(&self.application_name).connect_timeout(self.connect_timeout).ssl_mode(match self.sslmode.as_str() {
            "disable" => SslMode::Disable,
            "require" | "verify-full" | "verify-ca" => SslMode::Require,
            _ => SslMode::Prefer,
        });
        // Long statements send nothing until they finish; keepalives stop idle
        // firewalls and NAT from dropping the connection mid-query.
        c.keepalives(true).keepalives_idle(Duration::from_secs(30));
        if let Some(pw) = &self.password {
            c.password(pw);
        }
        if self.read_only {
            // Enforced by the server for every transaction on this session.
            c.options("-c default_transaction_read_only=on");
        }
        c
    }

    pub fn tls(&self) -> Result<MakeTlsConnector> {
        let mut b = native_tls::TlsConnector::builder();
        if self.sslmode != "verify-full" {
            // libpq semantics: prefer/require encrypt but do not verify.
            b.danger_accept_invalid_certs(true).danger_accept_invalid_hostnames(true);
        }
        let c = b.build().map_err(|e| Error::connection(format!("TLS setup failed: {e}")))?;
        Ok(MakeTlsConnector::new(c))
    }

    /// Open an unpooled client whose connection task runs on the runtime.
    pub async fn connect_client(&self, database: &str) -> Result<Client> {
        let (client, conn) = self.config(database).connect(self.tls()?).await.map_err(pg_err)?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!("postgres connection closed: {e}");
            }
        });
        Ok(client)
    }
}

pub(crate) fn pg_err(e: tokio_postgres::Error) -> Error {
    if let Some(db) = e.as_db_error() {
        let code = db.code().code();
        let kind = match code {
            "28P01" | "28000" => ErrorKind::Auth,
            "57014" => ErrorKind::Cancelled,
            c if c.starts_with("08") || c == "57P01" || c == "57P03" || c == "3D000" => ErrorKind::Connection,
            _ => ErrorKind::Server,
        };
        if kind == ErrorKind::Cancelled {
            return Error::cancelled();
        }
        let mut detail = format!("SQLSTATE {code}");
        if let Some(d) = db.detail() {
            detail.push_str(&format!("\nDetail: {d}"));
        }
        if let Some(h) = db.hint() {
            detail.push_str(&format!("\nHint: {h}"));
        }
        if let Some(tokio_postgres::error::ErrorPosition::Original(p)) = db.position() {
            detail.push_str(&format!("\nAt character {p}"));
        }
        return Error::new(kind, db.message().to_string()).with_detail(detail);
    }
    let text = e.to_string();
    let source = std::error::Error::source(&e).map(|s| s.to_string()).unwrap_or_default();
    let msg = if source.is_empty() { text } else { format!("{text}: {source}") };
    if e.is_closed() || msg.contains("connect") || msg.contains("timed out") || msg.contains("refused") { Error::connection(msg) } else { Error::server(msg) }
}

fn pool_err(e: deadpool_postgres::PoolError) -> Error {
    match e {
        deadpool_postgres::PoolError::Backend(e) => pg_err(e),
        deadpool_postgres::PoolError::Timeout(_) => Error::connection("Timed out waiting for a pooled connection"),
        other => Error::connection(other.to_string()),
    }
}

#[async_trait]
impl Driver for PostgresDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    async fn connect(&self, params: ConnectParams) -> Result<SessionHandle> {
        let target = PgTarget::from_params(&params)?;
        let session = PgSession { target: target.clone(), pools: Mutex::new(HashMap::new()), info: Mutex::new(ServerInfo::default()) };
        let client = session.client(None).await?;
        let rows = client
            .query(
                "SELECT version(), current_setting('server_version'), current_user::text, \
                 current_setting('server_encoding'), current_setting('TimeZone'), \
                 current_setting('server_version_num')::int",
                &[],
            )
            .await
            .map_err(pg_err)?;
        let r = &rows[0];
        let full: String = r.get(0);
        let version: String = r.get(1);
        let version_num: i32 = r.get(5);
        let product = full.split(" on ").next().unwrap_or(&full).to_string();
        *session.info.lock() = ServerInfo {
            product,
            version: version.clone(),
            details: vec![
                ("User".into(), r.get(2)),
                ("Encoding".into(), r.get(3)),
                ("Time zone".into(), r.get(4)),
                ("Version number".into(), version_num.to_string()),
                ("Server".into(), format!("{}:{}", target.host, target.port)),
            ],
        };
        drop(client);
        Ok(SessionHandle::Sql(Arc::new(session)))
    }
}

pub(crate) struct PgSession {
    pub target: PgTarget,
    pools: Mutex<HashMap<String, Pool>>,
    info: Mutex<ServerInfo>,
}

impl PgSession {
    fn pool(&self, database: &str) -> Result<Pool> {
        if let Some(p) = self.pools.lock().get(database) {
            return Ok(p.clone());
        }
        let mgr = Manager::from_config(self.target.config(database), self.target.tls()?, ManagerConfig { recycling_method: RecyclingMethod::Fast });
        let pool = Pool::builder(mgr).max_size(4).wait_timeout(Some(Duration::from_secs(30))).runtime(deadpool_postgres::Runtime::Tokio1).build().map_err(|e| Error::connection(e.to_string()))?;
        self.pools.lock().insert(database.to_string(), pool.clone());
        Ok(pool)
    }

    /// Pooled client for a database (default: the profile's database).
    pub async fn client(&self, database: Option<&str>) -> Result<deadpool_postgres::Object> {
        let db = database.unwrap_or(&self.target.database).to_string();
        self.pool(&db)?.get().await.map_err(pool_err)
    }

    pub fn drop_pool(&self, database: &str) {
        if let Some(p) = self.pools.lock().remove(database) {
            p.close();
        }
    }

    pub fn db_of<'a>(&'a self, p: &'a ObjPath) -> &'a str {
        p.database.as_deref().unwrap_or(&self.target.database)
    }

    /// openGauss and Huawei GaussDB: PostgreSQL 9.2 protocol and catalogs with their own role options.
    pub fn is_gauss(&self) -> bool {
        let p = self.info.lock().product.to_lowercase();
        p.contains("opengauss") || p.contains("gaussdb")
    }

    pub fn version_num(&self) -> i32 {
        self.info.lock().details.iter().find(|(k, _)| k == "Version number").and_then(|(_, v)| v.parse().ok()).unwrap_or(0)
    }

    /// Typed query returning every column as text (callers cast to text).
    pub async fn text_rows(&self, database: Option<&str>, sql: &str, params: &[&(dyn tokio_postgres::types::ToSql + Sync)]) -> Result<Vec<Vec<Option<String>>>> {
        let client = self.client(database).await?;
        let rows = client.query(sql, params).await.map_err(pg_err)?;
        Ok(rows.iter().map(|r| (0..r.len()).map(|i| r.get::<_, Option<String>>(i)).collect()).collect())
    }

    pub async fn text_result(&self, database: Option<&str>, sql: &str, params: &[&(dyn tokio_postgres::types::ToSql + Sync)]) -> Result<ResultSet> {
        let client = self.client(database).await?;
        let stmt = client.prepare(sql).await.map_err(pg_err)?;
        let rows = client.query(&stmt, params).await.map_err(pg_err)?;
        let cols: Vec<&str> = stmt.columns().iter().map(|c| c.name()).collect();
        Ok(ResultSet::from_text_rows(&cols, rows.iter().map(|r| (0..r.len()).map(|i| r.get::<_, Option<String>>(i)).collect()).collect()))
    }
}

/// Run one statement over the simple (text) protocol, keeping at most `max`
/// rows but counting all of them. `types` labels columns when known.
pub(crate) async fn simple_rows(client: &Client, sql: &str, max: usize, types: Option<Vec<String>>) -> Result<StatementOutcome> {
    let stream = client.simple_query_raw(sql).await.map_err(pg_err)?;
    futures::pin_mut!(stream);
    let mut rs: Option<ResultSet> = None;
    let mut seen = 0usize;
    let mut affected = None;
    while let Some(msg) = stream.next().await {
        match msg.map_err(pg_err)? {
            SimpleQueryMessage::RowDescription(cols) => {
                let mut set = ResultSet::new(
                    cols.iter()
                        .enumerate()
                        .map(|(i, c)| {
                            let ty = types.as_ref().and_then(|t| t.get(i)).cloned().unwrap_or_default();
                            ColumnMeta::new(c.name(), ty)
                        })
                        .collect(),
                );
                set.truncated = false;
                rs = Some(set);
            }
            SimpleQueryMessage::Row(row) => {
                seen += 1;
                let set = rs.get_or_insert_with(|| ResultSet::new(row.columns().iter().map(|c| ColumnMeta::new(c.name(), "")).collect()));
                if set.rows.len() < max {
                    set.rows.push((0..row.len()).map(|i| row.get(i).map(|v| Value::Text(v.to_string())).unwrap_or(Value::Null)).collect());
                } else {
                    set.truncated = true;
                }
            }
            SimpleQueryMessage::CommandComplete(n) => affected = Some(n),
            _ => {}
        }
    }
    let _ = seen;
    Ok(match rs {
        Some(set) => StatementOutcome::Rows(set),
        None => {
            let kw = openvclient_core::sqltext::leading_keyword(sql);
            match (kw.as_str(), affected) {
                ("INSERT" | "UPDATE" | "DELETE" | "MERGE" | "COPY", Some(n)) => StatementOutcome::Affected(n),
                _ => StatementOutcome::Done(format!("{kw} completed")),
            }
        }
    })
}

#[async_trait]
impl ServiceSession for PgSession {
    fn server_info(&self) -> ServerInfo {
        self.info.lock().clone()
    }

    async fn ping(&self) -> Result<Duration> {
        let t = Instant::now();
        let c = self.client(None).await?;
        c.simple_query("SELECT 1").await.map_err(pg_err)?;
        Ok(t.elapsed())
    }

    async fn close(&self) {
        for (_, p) in self.pools.lock().drain() {
            p.close();
        }
    }
}

#[async_trait]
impl SqlSession for PgSession {
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
            database_connection_toggle: rw,
            database_offline: false,
            server_side_backup: false,
            create_schema: rw,
            explain: true,
            explain_analyze: true,
            activity: true,
            cancel_query: true,
            terminate_session: true,
            locks: true,
            metrics: true,
            users: true,
            roles: true,
            grants: rw,
            edit_data: rw,
            truncate: rw,
            backup: admin::find_tool("pg_dump").is_some(),
            restore: rw && admin::find_tool("pg_restore").is_some(),
            materialized_views: true,
            sequences: true,
            procedures: self.version_num() >= 110000,
            functions: true,
            triggers: true,
            events: false,
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
        let mut sql = format!("SELECT count(*)::text FROM {}", D.qualified(path.schema.as_deref(), path.name()?));
        if let Some(f) = filter.filter(|f| !f.trim().is_empty()) {
            sql.push_str(&format!(" WHERE {f}"));
        }
        let rows = self.text_rows(Some(self.db_of(path)), &sql, &[]).await?;
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
        Ok(Arc::new(console::PgConsole::open(self.target.clone(), db).await?))
    }

    async fn run(&self, database: Option<&str>, sql: &str) -> Result<u64> {
        let c = self.client(database).await?;
        let msgs = c.simple_query(sql).await.map_err(pg_err)?;
        Ok(msgs.iter().filter_map(|m| if let SimpleQueryMessage::CommandComplete(n) = m { Some(*n) } else { None }).sum())
    }

    async fn drop_object(&self, kind: ObjectKind, path: &ObjPath, cascade: bool) -> Result<()> {
        admin::drop_object(self, kind, path, cascade).await
    }

    async fn truncate(&self, path: &ObjPath) -> Result<()> {
        let sql = format!("TRUNCATE TABLE {}", D.qualified(path.schema.as_deref(), path.name()?));
        self.run(Some(self.db_of(path)), &sql).await.map(|_| ())
    }

    async fn create_database(&self, spec: CreateDatabaseSpec) -> Result<()> {
        admin::create_database(self, spec).await
    }

    async fn set_database_allow_connections(&self, database: &str, allow: bool) -> Result<()> {
        let sql = format!("ALTER DATABASE {} WITH ALLOW_CONNECTIONS {}", D.quote_ident(database), allow);
        self.run(None, &sql).await.map(|_| ())
    }

    async fn create_schema(&self, database: Option<&str>, name: &str) -> Result<()> {
        self.run(database, &format!("CREATE SCHEMA {}", D.quote_ident(name))).await.map(|_| ())
    }

    async fn activity(&self) -> Result<ActivitySnapshot> {
        admin::activity(self).await
    }

    async fn kill(&self, id: &str, mode: KillMode) -> Result<()> {
        admin::kill(self, id, mode).await
    }

    async fn locks(&self) -> Result<ResultSet> {
        admin::locks(self).await
    }

    async fn metrics(&self) -> Result<Vec<Metric>> {
        admin::metrics(self).await
    }

    async fn principals(&self) -> Result<ResultSet> {
        admin::principals(self).await
    }

    async fn privileges(&self, principal: &str, _host: Option<&str>) -> Result<ResultSet> {
        admin::privileges(self, principal).await
    }

    async fn create_principal(&self, spec: PrincipalSpec) -> Result<()> {
        admin::create_principal(self, spec).await
    }

    async fn drop_principal(&self, name: &str, _host: Option<&str>) -> Result<()> {
        self.run(None, &format!("DROP ROLE {}", D.quote_ident(name))).await.map(|_| ())
    }

    async fn set_password(&self, name: &str, _host: Option<&str>, password: &str) -> Result<()> {
        let sql = format!("ALTER ROLE {} WITH PASSWORD {}", D.quote_ident(name), D.quote_literal(password));
        self.run(None, &sql).await.map(|_| ())
    }

    async fn grant(&self, spec: GrantSpec, revoke: bool) -> Result<()> {
        admin::grant(self, spec, revoke).await
    }

    fn privilege_names(&self, target: &GrantTarget) -> Vec<&'static str> {
        match target {
            GrantTarget::Database(_) => vec!["CONNECT", "CREATE", "TEMPORARY", "ALL PRIVILEGES"],
            GrantTarget::Schema(_) => vec!["USAGE", "CREATE", "ALL PRIVILEGES"],
            GrantTarget::AllTablesInSchema(_) | GrantTarget::Table(_) => {
                vec!["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER", "ALL PRIVILEGES"]
            }
        }
    }

    async fn backup(&self, req: BackupRequest) -> Result<String> {
        admin::backup(self, req).await
    }

    async fn restore(&self, req: BackupRequest) -> Result<String> {
        admin::restore(self, req).await
    }
}

#[cfg(test)]
mod tests;
