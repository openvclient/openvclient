//! MySQL / MariaDB connector (mysql_async). Metadata comes from
//! information_schema and SHOW statements; consoles use the text protocol,
//! table pages use prepared statements (which reject stacked statements).

mod admin;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, OptsBuilder, Pool, PoolConstraints, PoolOpts, SslOpts};
use openvclient_core::explain::{ExplainResult, mysql_tree_text};
use openvclient_core::sql::*;
use openvclient_core::sqltext::{Dialect, leading_keyword};
use openvclient_core::{
    Category, ColumnMeta, ConnectParams, Driver, DriverInfo, Error, ErrorKind, FieldKind, FieldSpec, Result, ResultSet, ServerInfo, ServiceSession, SessionHandle, Value, network_fields,
};
use parking_lot::Mutex;

pub(crate) const D: Dialect = Dialect::MySql;
const SYSTEM_DBS: &[&str] = &["information_schema", "mysql", "performance_schema", "sys"];

pub struct MySqlDriver {
    info: DriverInfo,
}

impl Default for MySqlDriver {
    fn default() -> Self {
        let mut fields = network_fields("3306", "root");
        fields.push(FieldSpec::text("database", "Default database").placeholder("(none)"));
        fields.push(
            FieldSpec::text("ssl", "TLS")
                .kind(FieldKind::Choice(&["disabled", "required", "verify"]))
                .default("disabled")
                .help("required encrypts without verifying the certificate; verify also checks host and CA.")
                .advanced(),
        );
        fields.push(FieldSpec::text("connect_timeout", "Connect timeout (s)").kind(FieldKind::Number).default("10").advanced());
        Self { info: DriverInfo { id: "mysql", name: "MySQL", category: Category::Relational, description: "MySQL 5.7+, Percona, TiDB and wire-compatible servers", badge: "MY", fields } }
    }
}

impl MySqlDriver {
    /// Same wire protocol and connector, listed separately for MariaDB.
    pub fn mariadb() -> Self {
        let mut d = Self::default();
        d.info.id = "mariadb";
        d.info.name = "MariaDB";
        d.info.description = "MariaDB 10.3+ (MySQL protocol, MariaDB EXPLAIN/ANALYZE)";
        d.info.badge = "MA";
        d
    }
}

#[derive(Clone)]
pub(crate) struct MyTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub database: Option<String>,
    pub ssl: String,
    pub timeout: Duration,
    pub read_only: bool,
}

impl MyTarget {
    pub fn opts(&self, database: Option<&str>, pooled: bool) -> Opts {
        let mut b = OptsBuilder::default()
            .ip_or_hostname(self.host.clone())
            .tcp_port(self.port)
            .user(Some(self.user.clone()))
            .pass(self.password.clone())
            .db_name(database.or(self.database.as_deref()).map(str::to_owned))
            .prefer_socket(false)
            // Count matched rows on UPDATE so "row not found" can be detected.
            .client_found_rows(true)
            // Keep long, silent statements from being dropped by idle firewalls/NAT.
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .stmt_cache_size(32);
        if pooled {
            b = b.pool_opts(PoolOpts::default().with_constraints(PoolConstraints::new(0, 4).expect("valid constraints")));
        }
        if self.ssl != "disabled" {
            let verify = self.ssl == "verify";
            b = b.ssl_opts(Some(SslOpts::default().with_danger_accept_invalid_certs(!verify).with_danger_skip_domain_validation(!verify)));
        }
        if self.read_only {
            b = b.init(vec!["SET SESSION TRANSACTION READ ONLY"]);
        }
        Opts::from(b)
    }

    pub async fn connect(&self, database: Option<&str>) -> Result<Conn> {
        tokio::time::timeout(self.timeout, Conn::new(self.opts(database, false))).await.map_err(|_| Error::connection(format!("Timed out connecting to {}:{}", self.host, self.port)))?.map_err(my_err)
    }
}

pub(crate) fn my_err(e: mysql_async::Error) -> Error {
    match e {
        mysql_async::Error::Server(s) => {
            let kind = match s.code {
                1045 | 1044 | 1698 => ErrorKind::Auth,
                1317 => ErrorKind::Cancelled,
                1049 | 2006 | 2013 => ErrorKind::Connection,
                _ => ErrorKind::Server,
            };
            if kind == ErrorKind::Cancelled {
                return Error::cancelled();
            }
            Error::new(kind, s.message).with_detail(format!("Error {} (SQLSTATE {})", s.code, s.state))
        }
        mysql_async::Error::Io(e) => Error::connection(e.to_string()),
        mysql_async::Error::Driver(e) => Error::connection(e.to_string()),
        other => Error::other(other.to_string()),
    }
}

fn type_name(c: &mysql_async::Column) -> String {
    use ColumnType::*;
    let unsigned = c.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    let binary = c.character_set() == 63;
    let base = match c.column_type() {
        MYSQL_TYPE_TINY => "tinyint",
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        MYSQL_TYPE_GEOMETRY => "geometry",
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            if binary {
                "blob"
            } else {
                "text"
            }
        }
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR => {
            if binary {
                "varbinary"
            } else {
                "varchar"
            }
        }
        MYSQL_TYPE_STRING => {
            if binary {
                "binary"
            } else {
                "char"
            }
        }
        MYSQL_TYPE_NULL => "null",
        _ => "",
    };
    if unsigned { format!("{base} unsigned") } else { base.to_string() }
}

pub(crate) fn columns_meta(cols: &[mysql_async::Column]) -> Vec<ColumnMeta> {
    cols.iter().map(|c| ColumnMeta::new(c.name_str(), type_name(c))).collect()
}

/// Convert a cell, keeping the server's own text where the protocol sends text.
pub(crate) fn to_value(v: &mysql_async::Value, col: Option<&mysql_async::Column>) -> Value {
    use mysql_async::Value as M;
    let binary = col.is_some_and(|c| c.character_set() == 63 && !matches!(c.column_type(), ColumnType::MYSQL_TYPE_NEWDECIMAL));
    match v {
        M::NULL => Value::Null,
        M::Bytes(b) if binary && !is_numeric_col(col) => Value::Bytes(b.clone()),
        M::Bytes(b) => match String::from_utf8(b.clone()) {
            Ok(s) => Value::Text(s),
            Err(_) => Value::Bytes(b.clone()),
        },
        M::Int(i) => Value::Int(*i),
        M::UInt(u) => Value::UInt(*u),
        M::Float(f) => Value::Float(*f as f64),
        M::Double(f) => Value::Float(*f),
        M::Date(y, mo, d, h, mi, s, us) => {
            let is_date = col.is_some_and(|c| matches!(c.column_type(), ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE));
            Value::Text(if is_date {
                format!("{y:04}-{mo:02}-{d:02}")
            } else if *us > 0 {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}")
            } else {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
            })
        }
        M::Time(neg, days, h, mi, s, us) => {
            let hours = *days as u64 * 24 + *h as u64;
            let sign = if *neg { "-" } else { "" };
            Value::Text(if *us > 0 { format!("{sign}{hours:02}:{mi:02}:{s:02}.{us:06}") } else { format!("{sign}{hours:02}:{mi:02}:{s:02}") })
        }
    }
}

fn is_numeric_col(col: Option<&mysql_async::Column>) -> bool {
    col.is_some_and(|c| {
        matches!(
            c.column_type(),
            ColumnType::MYSQL_TYPE_TINY
                | ColumnType::MYSQL_TYPE_SHORT
                | ColumnType::MYSQL_TYPE_INT24
                | ColumnType::MYSQL_TYPE_LONG
                | ColumnType::MYSQL_TYPE_LONGLONG
                | ColumnType::MYSQL_TYPE_FLOAT
                | ColumnType::MYSQL_TYPE_DOUBLE
                | ColumnType::MYSQL_TYPE_DECIMAL
                | ColumnType::MYSQL_TYPE_NEWDECIMAL
                | ColumnType::MYSQL_TYPE_YEAR
                | ColumnType::MYSQL_TYPE_DATE
                | ColumnType::MYSQL_TYPE_DATETIME
                | ColumnType::MYSQL_TYPE_TIMESTAMP
                | ColumnType::MYSQL_TYPE_TIME
        )
    })
}

pub(crate) fn bind(v: &Value) -> mysql_async::Value {
    use mysql_async::Value as M;
    match v {
        Value::Null => M::NULL,
        Value::Bool(b) => M::Int(*b as i64),
        Value::Int(i) => M::Int(*i),
        Value::UInt(u) => M::UInt(*u),
        Value::Float(f) => M::Double(*f),
        Value::Text(s) => M::Bytes(s.clone().into_bytes()),
        Value::Bytes(b) => M::Bytes(b.clone()),
    }
}

pub(crate) fn bind_text(v: &Option<String>) -> mysql_async::Value {
    match v {
        None => mysql_async::Value::NULL,
        Some(s) => mysql_async::Value::Bytes(s.clone().into_bytes()),
    }
}

/// Collect one statement's first result set (text protocol), counting rows
/// beyond `max` without keeping them.
pub(crate) async fn run_text(conn: &mut Conn, sql: &str, max: usize) -> Result<StatementOutcome> {
    let mut res = conn.query_iter(sql).await.map_err(my_err)?;
    let cols = res.columns();
    let mut set = cols.as_ref().filter(|c| !c.is_empty()).map(|c| ResultSet::new(columns_meta(c)));
    while let Some(row) = res.next().await.map_err(my_err)? {
        let Some(rs) = set.as_mut() else { continue };
        if row.columns_ref().len() != rs.columns.len() {
            // Further result sets from CALL; only the first is shown.
            rs.truncated = true;
            continue;
        }
        if rs.rows.len() < max {
            rs.rows.push((0..row.len()).map(|i| to_value(row.as_ref(i).unwrap_or(&mysql_async::Value::NULL), row.columns_ref().get(i))).collect());
        } else {
            rs.truncated = true;
        }
    }
    let affected = res.affected_rows();
    drop(res);
    Ok(match set {
        Some(rs) => StatementOutcome::Rows(rs),
        None => {
            let kw = leading_keyword(sql);
            match kw.as_str() {
                "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "LOAD" => StatementOutcome::Affected(affected),
                _ => StatementOutcome::Done(format!("{kw} completed")),
            }
        }
    })
}

#[async_trait]
impl Driver for MySqlDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    async fn connect(&self, p: ConnectParams) -> Result<SessionHandle> {
        let target = MyTarget {
            host: p.get_or("host", "localhost").to_string(),
            port: p.port(3306)?,
            user: p.get_or("user", "root").to_string(),
            password: p.secret.clone().filter(|s| !s.is_empty()),
            database: p.get("database").map(str::to_owned),
            ssl: p.get_or("ssl", "disabled").to_string(),
            timeout: Duration::from_secs(p.get_or("connect_timeout", "10").parse().unwrap_or(10)),
            read_only: p.read_only,
        };
        let mut conn = target.connect(None).await?;
        let (version, comment, user, charset): (String, String, String, String) =
            conn.query_first("SELECT VERSION(), @@version_comment, CURRENT_USER(), @@character_set_server").await.map_err(my_err)?.ok_or_else(|| Error::other("Server returned no version"))?;
        let ver = conn.server_version();
        let mariadb = version.to_ascii_lowercase().contains("mariadb");
        let _ = conn.disconnect().await;
        let product = if mariadb { format!("MariaDB {version}") } else { format!("MySQL {version}") };
        let info = ServerInfo {
            product,
            version: version.clone(),
            details: vec![("Distribution".into(), comment), ("User".into(), user), ("Charset".into(), charset), ("Server".into(), format!("{}:{}", target.host, target.port))],
        };
        let pool = Pool::new(target.opts(None, true));
        Ok(SessionHandle::Sql(Arc::new(MySession { target, pool, info, mariadb, version: ver })))
    }
}

pub(crate) struct MySession {
    pub target: MyTarget,
    pub pool: Pool,
    info: ServerInfo,
    pub mariadb: bool,
    pub version: (u16, u16, u16),
}

impl MySession {
    pub async fn conn(&self) -> Result<Conn> {
        tokio::time::timeout(self.target.timeout, self.pool.get_conn()).await.map_err(|_| Error::connection("Timed out waiting for a connection"))?.map_err(my_err)
    }

    /// Rows of a metadata query, each cell as text.
    pub async fn text_rows(&self, sql: &str, params: Vec<mysql_async::Value>) -> Result<Vec<Vec<Option<String>>>> {
        let mut c = self.conn().await?;
        let rows: Vec<mysql_async::Row> = if params.is_empty() { c.query(sql).await.map_err(my_err)? } else { c.exec(sql, mysql_async::Params::Positional(params)).await.map_err(my_err)? };
        Ok(rows.iter().map(|r| (0..r.len()).map(|i| to_value(r.as_ref(i).unwrap_or(&mysql_async::Value::NULL), r.columns_ref().get(i)).to_text()).collect()).collect())
    }

    pub async fn result(&self, sql: &str, params: Vec<mysql_async::Value>) -> Result<ResultSet> {
        self.result_with(sql, params, false).await
    }

    /// `prepared` forces the binary protocol even without parameters.
    pub async fn result_with(&self, sql: &str, params: Vec<mysql_async::Value>, prepared: bool) -> Result<ResultSet> {
        let mut c = self.conn().await?;
        if params.is_empty() && !prepared {
            match run_text(&mut c, sql, usize::MAX).await? {
                StatementOutcome::Rows(rs) => Ok(rs),
                _ => Ok(ResultSet::default()),
            }
        } else {
            let params = if params.is_empty() { mysql_async::Params::Empty } else { mysql_async::Params::Positional(params) };
            let mut res = c.exec_iter(sql, params).await.map_err(my_err)?;
            let cols = res.columns().map(|c| c.to_vec()).unwrap_or_default();
            let mut rs = ResultSet::new(columns_meta(&cols));
            while let Some(row) = res.next().await.map_err(my_err)? {
                rs.rows.push((0..row.len()).map(|i| to_value(row.as_ref(i).unwrap_or(&mysql_async::Value::NULL), cols.get(i))).collect());
            }
            Ok(rs)
        }
    }

    pub fn db_of(&self, p: &ObjPath) -> Result<String> {
        p.database.clone().or_else(|| self.target.database.clone()).ok_or_else(|| Error::invalid("No database selected"))
    }

    /// MySQL 8.0.16+ prints plans as trees; MariaDB and older use tables.
    fn tree_explain(&self) -> bool {
        !self.mariadb && self.version >= (8, 0, 16)
    }
}

#[async_trait]
impl ServiceSession for MySession {
    fn server_info(&self) -> ServerInfo {
        self.info.clone()
    }

    async fn ping(&self) -> Result<Duration> {
        let t = Instant::now();
        let mut c = self.conn().await?;
        c.ping().await.map_err(my_err)?;
        Ok(t.elapsed())
    }

    async fn close(&self) {
        let _ = self.pool.clone().disconnect().await;
    }
}

#[async_trait]
impl SqlSession for MySession {
    fn dialect(&self) -> Dialect {
        D
    }

    fn capabilities(&self) -> SqlCapabilities {
        let rw = !self.target.read_only;
        SqlCapabilities {
            multiple_databases: true,
            schemas: false,
            create_database: rw,
            drop_database: rw,
            explain: true,
            explain_analyze: self.mariadb || self.version >= (8, 0, 18),
            activity: true,
            cancel_query: true,
            terminate_session: true,
            locks: true,
            metrics: true,
            users: true,
            grants: rw,
            edit_data: rw,
            truncate: rw,
            backup: admin::find_tool(admin::dump_tools(self.mariadb)).is_some(),
            restore: rw && admin::find_tool(admin::client_tools(self.mariadb)).is_some(),
            procedures: true,
            functions: true,
            triggers: true,
            events: true,
            ..Default::default()
        }
    }

    fn default_database(&self) -> Option<String> {
        self.target.database.clone()
    }

    async fn children(&self, node: Option<(ObjectKind, ObjPath)>) -> Result<Vec<TreeNode>> {
        admin::children(self, node).await
    }

    async fn ddl(&self, kind: ObjectKind, path: &ObjPath) -> Result<String> {
        admin::ddl(self, kind, path).await
    }

    async fn columns(&self, path: &ObjPath) -> Result<Vec<ColumnInfo>> {
        admin::columns(self, path).await
    }

    async fn table_page(&self, q: TableQuery) -> Result<TablePage> {
        let db = self.db_of(&q.path)?;
        let name = q.path.name()?.to_string();
        let cols = admin::columns(self, &q.path).await?;
        let ttype = self
            .text_rows("SELECT TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?", vec![db.clone().into(), name.clone().into()])
            .await?
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next().flatten())
            .unwrap_or_default();
        let pk: Vec<String> = cols.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
        let mut sql = format!("SELECT * FROM {}", D.qualified(Some(&db), &name));
        if let Some(f) = q.filter.as_deref().filter(|f| !f.trim().is_empty()) {
            sql.push_str(&format!(" WHERE {f}"));
        }
        let order: Vec<String> = if q.order_by.is_empty() {
            pk.iter().map(|c| D.quote_ident(c)).collect()
        } else {
            q.order_by.iter().map(|(c, d)| format!("{} {}", D.quote_ident(c), if *d == SortDir::Asc { "ASC" } else { "DESC" })).collect()
        };
        if !order.is_empty() {
            sql.push_str(&format!(" ORDER BY {}", order.join(", ")));
        }
        sql.push_str(&format!(" LIMIT {} OFFSET {}", q.limit, q.offset));
        // Prepared statements cannot contain a second statement.
        let mut rows = self.result_with(&sql, vec![], true).await?;
        for c in &mut rows.columns {
            if let Some(info) = cols.iter().find(|x| x.name == c.name) {
                *c = ColumnMeta::new(c.name.clone(), info.data_type.clone());
            }
        }
        let read_only_reason = if self.target.read_only {
            Some("Connection is read-only".to_string())
        } else if ttype != "BASE TABLE" {
            Some("Views are read-only here".to_string())
        } else if pk.is_empty() {
            Some("Table has no primary key; MySQL has no stable row id to edit by".to_string())
        } else {
            None
        };
        Ok(TablePage { rows, key_columns: pk, row_ids: None, read_only_reason })
    }

    async fn count_rows(&self, path: &ObjPath, filter: Option<&str>) -> Result<u64> {
        let mut sql = format!("SELECT COUNT(*) FROM {}", D.qualified(Some(&self.db_of(path)?), path.name()?));
        if let Some(f) = filter.filter(|f| !f.trim().is_empty()) {
            sql.push_str(&format!(" WHERE {f}"));
        }
        let mut c = self.conn().await?;
        let n: Option<u64> = c.exec_first(sql, ()).await.map_err(my_err)?;
        Ok(n.unwrap_or(0))
    }

    async fn apply_changes(&self, path: &ObjPath, changes: Vec<RowChange>) -> Result<u64> {
        if self.target.read_only {
            return Err(Error::invalid("Connection is read-only"));
        }
        let target = D.qualified(Some(&self.db_of(path)?), path.name()?);
        let mut c = self.conn().await?;
        c.query_drop("START TRANSACTION").await.map_err(my_err)?;
        let mut total = 0u64;
        let res: Result<()> = async {
            for ch in changes {
                let (sql, params) = match ch {
                    RowChange::Update { key, values } => {
                        let sets: Vec<String> = values.iter().map(|(c, _)| format!("{} = ?", D.quote_ident(c))).collect();
                        let mut params: Vec<_> = values.iter().map(|(_, v)| bind_text(v)).collect();
                        let (w, kp) = where_key(&key)?;
                        params.extend(kp);
                        (format!("UPDATE {target} SET {} WHERE {w}", sets.join(", ")), params)
                    }
                    RowChange::Insert { values } => {
                        if values.is_empty() {
                            (format!("INSERT INTO {target} () VALUES ()"), vec![])
                        } else {
                            let cols: Vec<String> = values.iter().map(|(c, _)| D.quote_ident(c)).collect();
                            let ph = vec!["?"; values.len()].join(", ");
                            (format!("INSERT INTO {target} ({}) VALUES ({ph})", cols.join(", ")), values.iter().map(|(_, v)| bind_text(v)).collect())
                        }
                    }
                    RowChange::Delete { key } => {
                        let (w, kp) = where_key(&key)?;
                        (format!("DELETE FROM {target} WHERE {w}"), kp)
                    }
                };
                let params = if params.is_empty() { mysql_async::Params::Empty } else { mysql_async::Params::Positional(params) };
                let r = c.exec_iter(sql, params).await.map_err(my_err)?;
                let n = r.affected_rows();
                r.drop_result().await.map_err(my_err)?;
                if n == 0 {
                    return Err(Error::server("A row was not found (changed or deleted by someone else?). No changes were saved."));
                }
                total += n;
            }
            Ok(())
        }
        .await;
        match res {
            Ok(()) => {
                c.query_drop("COMMIT").await.map_err(my_err)?;
                Ok(total)
            }
            Err(e) => {
                let _ = c.query_drop("ROLLBACK").await;
                Err(e)
            }
        }
    }

    async fn catalog(&self, database: Option<&str>) -> Result<Catalog> {
        let db = database.map(str::to_owned).or_else(|| self.target.database.clone());
        let mut cat = Catalog {
            schemas: self.text_rows("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY 1", vec![]).await?.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect(),
            ..Default::default()
        };
        let Some(db) = db else { return Ok(cat) };
        let rows = self
            .text_rows(
                "SELECT t.TABLE_NAME, t.TABLE_TYPE, c.COLUMN_NAME FROM information_schema.TABLES t \
                 LEFT JOIN information_schema.COLUMNS c ON c.TABLE_SCHEMA = t.TABLE_SCHEMA AND c.TABLE_NAME = t.TABLE_NAME \
                 WHERE t.TABLE_SCHEMA = ? ORDER BY t.TABLE_NAME, c.ORDINAL_POSITION LIMIT 50000",
                vec![db.clone().into()],
            )
            .await?;
        for r in rows {
            let t = r[0].clone().unwrap_or_default();
            let entry = cat.columns.entry(t.clone()).or_default();
            if entry.is_empty() {
                let k = if r[1].as_deref() == Some("VIEW") { ObjectKind::View } else { ObjectKind::Table };
                cat.objects.push((String::new(), t, k));
            }
            if let Some(c) = r[2].clone() {
                entry.push(c);
            }
        }
        cat.functions = self
            .text_rows("SELECT ROUTINE_NAME FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ?", vec![db.into()])
            .await?
            .into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .collect();
        Ok(cat)
    }

    async fn open_console(&self, database: Option<String>) -> Result<Arc<dyn SqlConsole>> {
        let db = database.or_else(|| self.target.database.clone());
        let conn = self.target.connect(db.as_deref()).await?;
        let id = conn.id();
        Ok(Arc::new(MyConsole {
            conn: tokio::sync::Mutex::new(conn),
            id,
            pool: self.pool.clone(),
            database: Mutex::new(db),
            tree: self.tree_explain(),
            mariadb: self.mariadb,
            rt: tokio::runtime::Handle::current(),
        }))
    }

    async fn run(&self, database: Option<&str>, sql: &str) -> Result<u64> {
        let mut c = self.conn().await?;
        if let Some(db) = database {
            c.query_drop(format!("USE {}", D.quote_ident(db))).await.map_err(my_err)?;
        }
        let mut total = 0;
        for span in openvclient_core::sqltext::split_statements(D, sql) {
            let r = c.query_iter(&sql[span.start..span.end]).await.map_err(my_err)?;
            total += r.affected_rows();
            r.drop_result().await.map_err(my_err)?;
        }
        Ok(total)
    }

    async fn drop_object(&self, kind: ObjectKind, path: &ObjPath, _cascade: bool) -> Result<()> {
        admin::drop_object(self, kind, path).await
    }

    async fn truncate(&self, path: &ObjPath) -> Result<()> {
        let sql = format!("TRUNCATE TABLE {}", D.qualified(Some(&self.db_of(path)?), path.name()?));
        self.run(None, &sql).await.map(|_| ())
    }

    async fn create_database(&self, spec: CreateDatabaseSpec) -> Result<()> {
        if spec.name.trim().is_empty() {
            return Err(Error::invalid("Database name is required"));
        }
        let mut sql = format!("CREATE DATABASE {}", D.quote_ident(&spec.name));
        if let Some(e) = spec.encoding.filter(|x| !x.is_empty()) {
            sql.push_str(&format!(" CHARACTER SET {}", D.quote_ident(&e)));
        }
        if let Some(c) = spec.collation.filter(|x| !x.is_empty()) {
            sql.push_str(&format!(" COLLATE {}", D.quote_ident(&c)));
        }
        self.run(None, &sql).await.map(|_| ())
    }

    async fn activity(&self) -> Result<ActivitySnapshot> {
        let rows = self.result("SHOW FULL PROCESSLIST", vec![]).await?;
        Ok(ActivitySnapshot { rows, id_column: 0 })
    }

    async fn kill(&self, id: &str, mode: KillMode) -> Result<()> {
        let id: u64 = id.parse().map_err(|_| Error::invalid(format!("Invalid connection id {id}")))?;
        let sql = if mode == KillMode::CancelQuery { format!("KILL QUERY {id}") } else { format!("KILL {id}") };
        self.run(None, &sql).await.map(|_| ())
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

    async fn privileges(&self, principal: &str, host: Option<&str>) -> Result<ResultSet> {
        let sql = format!("SHOW GRANTS FOR {}", admin::account(principal, host));
        self.result(&sql, vec![]).await
    }

    async fn create_principal(&self, spec: PrincipalSpec) -> Result<()> {
        admin::create_principal(self, spec).await
    }

    async fn drop_principal(&self, name: &str, host: Option<&str>) -> Result<()> {
        self.run(None, &format!("DROP USER {}", admin::account(name, host))).await.map(|_| ())
    }

    async fn set_password(&self, name: &str, host: Option<&str>, password: &str) -> Result<()> {
        let sql = format!("ALTER USER {} IDENTIFIED BY {}", admin::account(name, host), D.quote_literal(password));
        self.run(None, &sql).await.map(|_| ())
    }

    async fn grant(&self, spec: GrantSpec, revoke: bool) -> Result<()> {
        admin::grant(self, spec, revoke).await
    }

    fn privilege_names(&self, target: &GrantTarget) -> Vec<&'static str> {
        match target {
            GrantTarget::Table(_) => vec!["SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "INDEX", "REFERENCES", "TRIGGER", "SHOW VIEW", "ALL PRIVILEGES"],
            _ => vec![
                "SELECT",
                "INSERT",
                "UPDATE",
                "DELETE",
                "CREATE",
                "DROP",
                "ALTER",
                "INDEX",
                "CREATE VIEW",
                "SHOW VIEW",
                "CREATE ROUTINE",
                "ALTER ROUTINE",
                "EXECUTE",
                "EVENT",
                "TRIGGER",
                "LOCK TABLES",
                "REFERENCES",
                "CREATE TEMPORARY TABLES",
                "ALL PRIVILEGES",
            ],
        }
    }

    async fn backup(&self, req: BackupRequest) -> Result<String> {
        admin::backup(self, req).await
    }

    async fn restore(&self, req: BackupRequest) -> Result<String> {
        admin::restore(self, req).await
    }
}

fn where_key(key: &RowKey) -> Result<(String, Vec<mysql_async::Value>)> {
    match key {
        RowKey::RowId(_) => Err(Error::unsupported("MySQL rows are edited by primary key only")),
        RowKey::PrimaryKey(cols) => {
            let w: Vec<String> = cols.iter().map(|(c, v)| if v.is_null() { format!("{} IS NULL", D.quote_ident(c)) } else { format!("{} = ?", D.quote_ident(c)) }).collect();
            Ok((w.join(" AND "), cols.iter().filter(|(_, v)| !v.is_null()).map(|(_, v)| bind(v)).collect()))
        }
    }
}

struct MyConsole {
    conn: tokio::sync::Mutex<Conn>,
    id: u32,
    pool: Pool,
    database: Mutex<Option<String>>,
    tree: bool,
    mariadb: bool,
    rt: tokio::runtime::Handle,
}

#[async_trait]
impl SqlConsole for MyConsole {
    async fn execute(&self, statements: Vec<String>, opts: ExecOptions) -> Vec<StatementResult> {
        let mut conn = self.conn.lock().await;
        let mut out = Vec::new();
        for sql in statements {
            let t = Instant::now();
            let outcome = run_text(&mut conn, &sql, opts.max_rows).await;
            let failed = outcome.is_err();
            if !failed
                && leading_keyword(&sql) == "USE"
                && let Ok(Some(db)) = conn.query_first::<Option<String>, _>("SELECT DATABASE()").await
            {
                *self.database.lock() = db;
            }
            out.push(StatementResult { sql, elapsed: t.elapsed(), outcome });
            if failed {
                break;
            }
        }
        out
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<ExplainResult> {
        let mut conn = self.conn.lock().await;
        let kw = leading_keyword(sql);
        let statement = match (self.mariadb, self.tree, analyze) {
            (true, _, true) => format!("ANALYZE {sql}"),
            (true, _, false) => format!("EXPLAIN {sql}"),
            (false, true, true) => format!("EXPLAIN ANALYZE {sql}"),
            (false, true, false) => format!("EXPLAIN FORMAT=TREE {sql}"),
            (false, false, _) => format!("EXPLAIN {sql}"),
        };
        // Analyze executes the statement: keep changes out of the database.
        let wrap = analyze && !matches!(kw.as_str(), "SELECT" | "WITH" | "TABLE" | "VALUES");
        if wrap {
            conn.query_drop("START TRANSACTION").await.map_err(my_err)?;
        }
        let res = run_text(&mut conn, &statement, 10_000).await;
        if wrap {
            let _ = conn.query_drop("ROLLBACK").await;
        }
        let StatementOutcome::Rows(rs) = res? else {
            return Err(Error::other("EXPLAIN returned no rows"));
        };
        let mut summary = Vec::new();
        if wrap {
            summary.push(("Note".into(), "Statement ran inside a transaction that was rolled back".into()));
        }
        if self.tree && !self.mariadb {
            let mut raw = rs.rows.iter().filter_map(|r| r.first().and_then(|v| v.to_text())).collect::<Vec<_>>().join("\n");
            let mut statement = statement;
            if analyze && raw.contains("not executable by iterator executor") {
                // MySQL cannot analyze this statement shape; show the estimate instead.
                statement = format!("EXPLAIN FORMAT=TREE {sql}");
                if let StatementOutcome::Rows(est) = run_text(&mut conn, &statement, 10_000).await? {
                    raw = est.rows.iter().filter_map(|r| r.first().and_then(|v| v.to_text())).collect::<Vec<_>>().join("\n");
                }
                summary.push(("Note".into(), "MySQL cannot EXPLAIN ANALYZE this statement; showing the estimated plan".into()));
            }
            let tree = mysql_tree_text(&raw);
            Ok(ExplainResult { statement, raw, tree: Some(tree), table: None, summary })
        } else {
            let raw = rs.rows.iter().map(|r| r.iter().map(|v| v.display().into_owned()).collect::<Vec<_>>().join(" | ")).collect::<Vec<_>>().join("\n");
            Ok(ExplainResult { statement, raw, tree: None, table: Some(rs), summary })
        }
    }

    fn cancel(&self) {
        let pool = self.pool.clone();
        let id = self.id;
        self.rt.spawn(async move {
            match pool.get_conn().await {
                Ok(mut c) => {
                    if let Err(e) = c.query_drop(format!("KILL QUERY {id}")).await {
                        tracing::warn!("cancel failed: {e}");
                    }
                }
                Err(e) => tracing::warn!("cancel failed: {e}"),
            }
        });
    }

    fn current_database(&self) -> Option<String> {
        self.database.lock().clone()
    }

    async fn close(&self) {
        let mut c = self.conn.lock().await;
        let _ = c.reset().await;
    }
}

pub(crate) fn is_system_db(name: &str) -> bool {
    SYSTEM_DBS.contains(&name)
}

#[cfg(test)]
mod tests;
