//! Live test against SQL Server (docker `ovc-mssql` on port 51433); skipped
//! when unreachable.

use std::collections::BTreeMap;

use openvclient_core::sql::*;
use openvclient_core::{ConnectParams, Driver, Value};

use crate::SqlServerDriver;

fn params(db: &str) -> ConnectParams {
    let mut v = BTreeMap::new();
    v.insert("host".into(), "127.0.0.1".into());
    v.insert("port".into(), std::env::var("OPENVCLIENT_MS_PORT").unwrap_or_else(|_| "51433".into()));
    v.insert("user".into(), "sa".into());
    v.insert("database".into(), db.into());
    // The test container uses a self-signed certificate.
    v.insert("encrypt".into(), "on".into());
    ConnectParams { values: v, secret: Some(std::env::var("OPENVCLIENT_MS_PASSWORD").unwrap_or_else(|_| "OpenVClient#2026".into())), read_only: false, notifier: None }
}

#[tokio::test]
async fn live_sqlserver() {
    let admin = match SqlServerDriver::default().connect(params("master")).await {
        Ok(h) => h.sql().unwrap().clone(),
        Err(e) => {
            eprintln!("skipping: no SQL Server ({e})");
            return;
        }
    };
    eprintln!("{}", admin.server_info().product);
    let _ = admin.drop_object(ObjectKind::Database, &ObjPath::db("openvclient_test"), true).await;
    admin.create_database(CreateDatabaseSpec { name: "openvclient_test".into(), ..Default::default() }).await.unwrap();
    let s = SqlServerDriver::default().connect(params("openvclient_test")).await.unwrap().sql().unwrap().clone();
    s.run(
        None,
        "CREATE SCHEMA app\nGO\nCREATE TABLE app.users(id int IDENTITY(1,1) PRIMARY KEY, name nvarchar(40) NOT NULL, score decimal(6,2) NULL, born date NULL, seen datetime2(3) NULL, uid uniqueidentifier DEFAULT NEWID(), data varbinary(16) NULL)\nGO\n\
         CREATE TABLE app.nokey(a nvarchar(10))\nGO\nCREATE INDEX users_name ON app.users(name)\nGO\n\
         CREATE VIEW app.v AS SELECT name FROM app.users\nGO\n\
         CREATE PROCEDURE app.p @x int AS BEGIN SET NOCOUNT ON; SELECT @x + 1 AS y; END\nGO\n",
    )
    .await
    .unwrap();

    // SSMS layout: Databases / Security / Activity Monitor at the top.
    let top = s.children(None).await.unwrap();
    assert_eq!(top.iter().map(|n| n.label.as_str()).collect::<Vec<_>>(), vec!["Databases", "Security", "Activity Monitor"]);
    let dbs = s.children(Some((ObjectKind::Folder(FolderKind::Databases), ObjPath::default()))).await.unwrap();
    assert!(dbs.iter().any(|d| d.label == "openvclient_test"));
    assert!(dbs.iter().any(|d| d.label == "System Databases") && !dbs.iter().any(|d| d.label == "master"));
    let sys = s.children(Some((ObjectKind::Folder(FolderKind::SystemDatabases), ObjPath::default()))).await.unwrap();
    assert!(sys.iter().any(|d| d.label == "master"));
    let base = ObjPath::db("openvclient_test").with_schema("app");
    let dbnode = s.children(Some((ObjectKind::Database, ObjPath::db("openvclient_test")))).await.unwrap();
    assert_eq!(dbnode.iter().map(|n| n.label.as_str()).collect::<Vec<_>>(), vec!["Tables", "Views", "Programmability", "Schemas"]);
    let schemas = s.children(Some((ObjectKind::Schema, ObjPath::db("openvclient_test").with_sub("#schemas")))).await.unwrap();
    assert!(schemas.iter().any(|n| n.label == "app"), "{:?}", schemas.iter().map(|n| &n.label).collect::<Vec<_>>());
    let tables = s.children(Some((ObjectKind::Folder(FolderKind::Tables), ObjPath::db("openvclient_test")))).await.unwrap();
    assert_eq!(tables.iter().map(|t| t.label.as_str()).collect::<Vec<_>>(), vec!["app.nokey", "app.users"]);
    let prog = s.children(Some((ObjectKind::Folder(FolderKind::Programmability), ObjPath::db("openvclient_test")))).await.unwrap();
    assert_eq!(prog.len(), 3);
    let procs = s.children(Some((ObjectKind::Folder(FolderKind::Procedures), base.clone()))).await.unwrap();
    assert!(s.ddl(ObjectKind::Procedure, &procs[0].path).await.unwrap().contains("SELECT @x + 1"));
    let users = base.clone().with_name("users");
    let ddl = s.ddl(ObjectKind::Table, &users).await.unwrap();
    assert!(ddl.contains("IDENTITY(1,1)") && ddl.contains("users_name"), "{ddl}");
    let idx = s.children(Some((ObjectKind::Folder(FolderKind::Indexes), users.clone()))).await.unwrap();
    assert!(idx.iter().any(|i| i.label == "users_name"));

    let c = s.open_console(None).await.unwrap();
    let res = c
        .execute(
            vec![
                "INSERT INTO app.users(name, score, born, seen, data) VALUES (N'ann', 1.50, '2000-01-02', '2024-05-06 07:08:09.123', 0x00FF), (N'bøb', NULL, NULL, NULL, NULL)".into(),
                "SELECT id, name, score, born, seen, data FROM app.users ORDER BY id".into(),
                "SELECT TOP 50 ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n FROM sys.all_objects".into(),
                "EXEC app.p @x = 41".into(),
                "SELEC broken".into(),
                "SELECT 1".into(),
            ],
            ExecOptions { max_rows: 10 },
        )
        .await;
    assert!(matches!(res[0].outcome, Ok(StatementOutcome::Affected(2))), "{:?}", res[0].outcome);
    let Ok(StatementOutcome::Rows(rs)) = &res[1].outcome else { panic!("{:?}", res[1].outcome) };
    assert_eq!(rs.rows[0][2], Value::Text("1.50".into()));
    assert_eq!(rs.rows[0][3], Value::Text("2000-01-02".into()));
    assert!(rs.rows[0][4].to_text().unwrap().starts_with("2024-05-06 07:08:09.123"), "{:?}", rs.rows[0][4]);
    assert_eq!(rs.rows[0][5], Value::Bytes(vec![0, 255]));
    assert_eq!(rs.rows[1][1], Value::Text("bøb".into()));
    let Ok(StatementOutcome::Rows(big)) = &res[2].outcome else { panic!() };
    assert!(big.truncated && big.rows.len() == 10);
    assert!(matches!(&res[3].outcome, Ok(StatementOutcome::Rows(r)) if r.rows[0][0] == Value::Int(42)), "{:?}", res[3].outcome);
    assert!(res[4].outcome.is_err());
    assert_eq!(res.len(), 5);

    let p = c.explain("SELECT u.name FROM app.users u WHERE u.id = 1", false).await.unwrap();
    assert!(p.tree.is_some(), "{}", p.raw);
    let p = c.explain("DELETE FROM app.users", true).await.unwrap();
    assert!(p.tree.is_some(), "{}", p.raw);
    assert_eq!(s.count_rows(&users, None).await.unwrap(), 2, "analyze must roll back");

    let page = s.table_page(TableQuery { path: users.clone(), filter: Some("name <> N'zed'".into()), order_by: vec![("name".into(), SortDir::Asc)], limit: 50, offset: 0 }).await.unwrap();
    assert_eq!(page.key_columns, vec!["id"]);
    assert_eq!(page.rows.rows[0][1], Value::Text("ann".into()));
    let ann = page.rows.rows[0][0].clone();
    s.apply_changes(
        &users,
        vec![
            RowChange::Update { key: RowKey::PrimaryKey(vec![("id".into(), ann.clone())]), values: vec![("score".into(), Some("7.25".into())), ("born".into(), Some("1999-12-31".into()))] },
            RowChange::Insert { values: vec![("name".into(), Some("cy".into()))] },
        ],
    )
    .await
    .unwrap();
    assert_eq!(s.count_rows(&users, Some("score = 7.25")).await.unwrap(), 1);
    assert!(s.table_page(TableQuery { path: users.clone(), filter: Some("1=1; DROP TABLE app.users".into()), order_by: vec![], limit: 5, offset: 0 }).await.is_err());
    let bad = s.apply_changes(&users, vec![RowChange::Insert { values: vec![("name".into(), Some("ok".into()))] }, RowChange::Insert { values: vec![("name".into(), None)] }]).await;
    assert!(bad.is_err());
    assert_eq!(s.count_rows(&users, None).await.unwrap(), 3, "failed batch must roll back");
    let nk = s.table_page(TableQuery { path: base.clone().with_name("nokey"), filter: None, order_by: vec![], limit: 5, offset: 0 }).await.unwrap();
    assert!(nk.read_only_reason.is_some());

    assert!(!s.activity().await.unwrap().rows.rows.is_empty());
    s.locks().await.unwrap();
    assert!(s.metrics().await.unwrap().iter().any(|m| m.name == "Page life expectancy (s)"));
    let _ = s.drop_principal("openvclient_login", None).await;
    s.create_principal(PrincipalSpec { name: "openvclient_login".into(), password: Some("Str0ng!Passw0rd#".into()), can_login: true, ..Default::default() }).await.unwrap();
    s.grant(GrantSpec { privileges: vec!["SELECT".into()], target: GrantTarget::Table(users.clone()), principal: "openvclient_login".into(), host: None, with_grant_option: false }, false)
        .await
        .unwrap();
    let pr = s.privileges("openvclient_login", None).await.unwrap();
    assert!(pr.rows.iter().any(|r| r[2] == Value::Text("SELECT".into())), "{:?}", pr.rows);
    assert!(s.principals().await.unwrap().rows.iter().any(|r| r[0] == Value::Text("openvclient_login".into())));
    let cat = s.catalog(None).await.unwrap();
    assert!(cat.columns["users"].contains(&"score".to_string()));

    // Cancel = KILL the console session; it reconnects for the next run.
    let c2 = c.clone();
    let h = tokio::spawn(async move { c2.execute(vec!["WAITFOR DELAY '00:00:20'".into()], ExecOptions::default()).await });
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    c.cancel();
    let r = tokio::time::timeout(std::time::Duration::from_secs(8), h).await.expect("cancel too slow").unwrap();
    assert!(r[0].outcome.is_err(), "{:?}", r[0].outcome);
    let again = c.execute(vec!["SELECT 1".into()], ExecOptions::default()).await;
    assert!(again[0].outcome.is_ok(), "console must reconnect after KILL: {:?}", again[0].outcome);

    // Offline / online and server-side backup.
    admin.set_database_online("openvclient_test", false).await.unwrap();
    let dbs = admin.children(Some((ObjectKind::Folder(FolderKind::Databases), ObjPath::default()))).await.unwrap();
    assert!(dbs.iter().any(|d| d.label == "openvclient_test" && d.detail.as_deref().unwrap_or("").contains("offline")), "{:?}", dbs.iter().map(|d| (&d.label, &d.detail)).collect::<Vec<_>>());
    admin.set_database_online("openvclient_test", true).await.unwrap();
    admin.backup(BackupRequest { database: Some("openvclient_test".into()), path: "/var/opt/mssql/data/openvclient_test.bak".into() }).await.unwrap();

    c.close().await;
    s.close().await;
    admin.drop_principal("openvclient_login", None).await.unwrap();
    admin.drop_object(ObjectKind::Database, &ObjPath::db("openvclient_test"), true).await.unwrap();
}

#[tokio::test]
async fn auth_method_validation() {
    let mut p = params("master");
    p.values.insert("auth".into(), "entra_service_principal".into());
    let e = SqlServerDriver::default().connect(p).await.err().expect("needs client id");
    assert!(e.message.contains("Client (application) ID"), "{e}");
    let mut p = params("master");
    p.values.insert("auth".into(), "entra_token".into());
    p.secret = None;
    assert!(SqlServerDriver::default().connect(p).await.is_err());
    #[cfg(not(windows))]
    {
        let mut p = params("master");
        p.values.insert("auth".into(), "integrated".into());
        let e = SqlServerDriver::default().connect(p).await.err().expect("integrated is Windows-only");
        assert!(e.message.contains("Windows build"), "{e}");
    }
}

/// USE switches the console's database; a real Cancel says "cancelled", while
/// a session that dies on its own (network drop, someone else's KILL) says the
/// connection was lost, not "cancelled".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_use_cancel_and_lost_connection() {
    let s = match SqlServerDriver::default().connect(params("master")).await {
        Ok(h) => h.sql().unwrap().clone(),
        Err(e) => {
            eprintln!("skipping: no SQL Server ({e})");
            return;
        }
    };
    let c = s.open_console(Some("master".into())).await.unwrap();
    let r = c.execute(vec!["USE tempdb".into(), "SELECT DB_NAME() AS db".into()], ExecOptions { max_rows: 10 }).await;
    assert!(r.iter().all(|x| x.outcome.is_ok()), "{:?}", r.iter().map(|x| x.outcome.as_ref().err()).collect::<Vec<_>>());
    assert_eq!(c.current_database().as_deref(), Some("tempdb"));

    // Cancel during a long statement.
    let c2 = c.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        c2.cancel();
    });
    let r = c.execute(vec!["WAITFOR DELAY '00:00:30'".into()], ExecOptions { max_rows: 10 }).await;
    canceller.await.unwrap();
    let e = r[0].outcome.as_ref().expect_err("cancelled");
    assert_eq!(e.kind, openvclient_core::ErrorKind::Cancelled, "{e}");

    // The session is ended from elsewhere without Cancel: not "cancelled".
    let spid = match c.execute(vec!["SELECT CAST(@@SPID AS int)".into()], ExecOptions { max_rows: 1 }).await.remove(0).outcome {
        Ok(StatementOutcome::Rows(rs)) => rs.rows[0][0].to_text().unwrap(),
        other => panic!("{other:?}"),
    };
    let killer = s.clone();
    let k = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        killer.run(Some("master"), &format!("KILL {spid}")).await.unwrap();
    });
    let r = c.execute(vec!["WAITFOR DELAY '00:00:30'".into()], ExecOptions { max_rows: 10 }).await;
    k.await.unwrap();
    let e = r[0].outcome.as_ref().expect_err("lost");
    assert_ne!(e.kind, openvclient_core::ErrorKind::Cancelled, "{e}");
    assert!(e.message.contains("connection to the server was lost"), "{e}");
    // And the next run opens a fresh session.
    let r = c.execute(vec!["SELECT 1 AS one".into()], ExecOptions { max_rows: 1 }).await;
    assert!(r[0].outcome.is_ok(), "{:?}", r[0].outcome);
}
