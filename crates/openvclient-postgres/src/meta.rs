//! Catalog browsing, DDL generation and table data access.

use openvclient_core::sql::*;
use openvclient_core::{Error, Result};

use crate::{D, PgSession, pg_err, simple_rows};

const USER_SCHEMAS: &str = "n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%'";

fn rel_detail(tuples: Option<&String>, size: Option<&String>) -> String {
    let mut parts = Vec::new();
    if let Some(t) = tuples.and_then(|t| t.parse::<f64>().ok()).filter(|t| *t >= 0.0) {
        parts.push(format!("~{} rows", group_digits(t as u64)));
    }
    if let Some(s) = size {
        // Raw byte counts are formatted; anything else is shown as given.
        parts.push(s.parse::<u64>().map(openvclient_core::containers::human_bytes).unwrap_or_else(|_| s.clone()));
    }
    parts.join(" · ")
}

pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub async fn children(s: &PgSession, node: Option<(ObjectKind, ObjPath)>) -> Result<Vec<TreeNode>> {
    let Some((kind, path)) = node else {
        let rows = s
            .text_rows(
                None,
                "SELECT datname::text, CASE WHEN has_database_privilege(datname, 'CONNECT') \
                 THEN pg_database_size(datname)::text END, datallowconn::text \
                 FROM pg_database WHERE NOT datistemplate ORDER BY datname",
                &[],
            )
            .await?;
        return Ok(rows
            .into_iter()
            .map(|r| {
                let name = r[0].clone().unwrap_or_default();
                // Sizes in bytes, formatted like every other connector (KiB/MiB/GiB).
                let mut detail = r[1].as_deref().and_then(|b| b.parse::<u64>().ok()).map(openvclient_core::containers::human_bytes).unwrap_or_default();
                if r[2].as_deref() == Some("false") {
                    detail = format!("connections disabled {detail}");
                }
                TreeNode::new(ObjectKind::Database, name.clone(), ObjPath::db(name)).detail(detail.trim())
            })
            .collect());
    };
    let db = s.db_of(&path).to_string();
    let base = ObjPath::db(&db);
    match kind {
        ObjectKind::Database => {
            let rows = s
                .text_rows(
                    Some(&db),
                    "SELECT n.nspname::text, (SELECT count(*) FROM pg_class c WHERE c.relnamespace = n.oid AND c.relkind IN ('r','p'))::text \
                     FROM pg_namespace n WHERE n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%' \
                     ORDER BY (n.nspname IN ('pg_catalog','information_schema')), n.nspname",
                    &[],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let name = r[0].clone().unwrap_or_default();
                    let n = r[1].clone().unwrap_or_default();
                    TreeNode::new(ObjectKind::Schema, name.clone(), base.clone().with_schema(name)).detail(if n == "0" { String::new() } else { format!("{n} tables") })
                })
                .collect())
        }
        ObjectKind::Schema => {
            let mut v = vec![
                TreeNode::folder(FolderKind::Tables, path.clone()),
                TreeNode::folder(FolderKind::Views, path.clone()),
                TreeNode::folder(FolderKind::MaterializedViews, path.clone()),
                TreeNode::folder(FolderKind::Functions, path.clone()),
            ];
            if s.version_num() >= 110000 {
                v.push(TreeNode::folder(FolderKind::Procedures, path.clone()));
            }
            v.push(TreeNode::folder(FolderKind::Sequences, path.clone()));
            v.push(TreeNode::folder(FolderKind::ForeignTables, path));
            Ok(v)
        }
        ObjectKind::Folder(f @ (FolderKind::Tables | FolderKind::Views | FolderKind::MaterializedViews | FolderKind::ForeignTables | FolderKind::Sequences)) => {
            let (kinds, okind) = match f {
                FolderKind::Tables => ("'r','p'", ObjectKind::Table),
                FolderKind::Views => ("'v'", ObjectKind::View),
                FolderKind::MaterializedViews => ("'m'", ObjectKind::MaterializedView),
                FolderKind::ForeignTables => ("'f'", ObjectKind::ForeignTable),
                _ => ("'S'", ObjectKind::Sequence),
            };
            let schema = path.schema.clone().unwrap_or_default();
            // relispartition exists from PostgreSQL 10; openGauss and Kingbase report older versions.
            let partitions = if s.version_num() >= 100000 { "AND NOT c.relispartition" } else { "" };
            let sql = format!(
                "SELECT c.relname::text, c.reltuples::bigint::text, pg_total_relation_size(c.oid)::text, c.relkind::text \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind IN ({kinds}) {partitions} ORDER BY c.relname"
            );
            let rows = s.text_rows(Some(&db), &sql, &[&schema]).await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let name = r[0].clone().unwrap_or_default();
                    let mut detail = match okind {
                        ObjectKind::Table | ObjectKind::MaterializedView => rel_detail(r[1].as_ref(), r[2].as_ref()),
                        _ => String::new(),
                    };
                    if r[3].as_deref() == Some("p") {
                        detail = format!("partitioned · {detail}");
                    }
                    let node = TreeNode::new(okind, name.clone(), path.clone().with_name(name)).detail(detail);
                    if matches!(okind, ObjectKind::View | ObjectKind::Sequence) { node.leaf() } else { node }
                })
                .collect())
        }
        ObjectKind::Folder(f @ (FolderKind::Functions | FolderKind::Procedures)) => {
            let schema = path.schema.clone().unwrap_or_default();
            let prokind = if f == FolderKind::Functions { "'f','a','w'" } else { "'p'" };
            let pk_filter = if s.version_num() >= 110000 { format!("AND p.prokind IN ({prokind})") } else { String::new() };
            let sql = format!(
                "SELECT p.proname::text, pg_get_function_identity_arguments(p.oid), \
                 CASE WHEN p.prorettype <> 0 THEN format_type(p.prorettype, NULL) END, l.lanname::text \
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace JOIN pg_language l ON l.oid = p.prolang \
                 WHERE n.nspname = $1 {pk_filter} ORDER BY p.proname, 2"
            );
            let rows = s.text_rows(Some(&db), &sql, &[&schema]).await?;
            let okind = if f == FolderKind::Functions { ObjectKind::Function } else { ObjectKind::Procedure };
            Ok(rows
                .into_iter()
                .map(|r| {
                    let name = r[0].clone().unwrap_or_default();
                    let args = r[1].clone().unwrap_or_default();
                    let ret = r[2].clone().map(|t| format!("→ {t} · ")).unwrap_or_default();
                    TreeNode::new(okind, format!("{name}({args})"), path.clone().with_name(name).with_sub(args)).detail(format!("{ret}{}", r[3].clone().unwrap_or_default())).leaf()
                })
                .collect())
        }
        ObjectKind::Table | ObjectKind::MaterializedView | ObjectKind::ForeignTable => {
            let mut v = vec![TreeNode::folder(FolderKind::Columns, path.clone()), TreeNode::folder(FolderKind::Indexes, path.clone())];
            if kind != ObjectKind::MaterializedView {
                v.push(TreeNode::folder(FolderKind::Constraints, path.clone()));
                v.push(TreeNode::folder(FolderKind::Triggers, path));
            }
            Ok(v)
        }
        ObjectKind::Folder(FolderKind::Columns) => Ok(columns(s, &path)
            .await?
            .into_iter()
            .map(|c| {
                let mut d = c.data_type.clone();
                if c.primary_key {
                    d.push_str(" · PK");
                }
                if !c.nullable {
                    d.push_str(" · NOT NULL");
                }
                TreeNode::new(ObjectKind::Column, c.name.clone(), path.clone().with_sub(c.name)).detail(d)
            })
            .collect()),
        ObjectKind::Folder(FolderKind::Indexes) => {
            let rows = s
                .text_rows(
                    Some(&db),
                    "SELECT i.relname::text, CASE WHEN x.indisprimary THEN 'primary key' WHEN x.indisunique THEN 'unique' ELSE '' END, \
                     am.amname::text, pg_size_pretty(pg_relation_size(i.oid)) \
                     FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_class t ON t.oid = x.indrelid \
                     JOIN pg_namespace n ON n.oid = t.relnamespace JOIN pg_am am ON am.oid = i.relam \
                     WHERE n.nspname = $1 AND t.relname = $2 ORDER BY 1",
                    &[&path.schema.clone().unwrap_or_default(), &path.name()?.to_string()],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let name = r[0].clone().unwrap_or_default();
                    let d = [r[1].clone(), r[2].clone(), r[3].clone()].into_iter().flatten().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · ");
                    TreeNode::new(ObjectKind::Index, name.clone(), path.clone().with_sub(name)).detail(d)
                })
                .collect())
        }
        ObjectKind::Folder(FolderKind::Constraints) => {
            let rows = s
                .text_rows(
                    Some(&db),
                    "SELECT c.conname::text, CASE c.contype WHEN 'p' THEN 'primary key' WHEN 'f' THEN 'foreign key' \
                     WHEN 'u' THEN 'unique' WHEN 'c' THEN 'check' WHEN 'x' THEN 'exclusion' ELSE c.contype::text END, \
                     pg_get_constraintdef(c.oid) FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid \
                     JOIN pg_namespace n ON n.oid = t.relnamespace WHERE n.nspname = $1 AND t.relname = $2 ORDER BY c.contype, 1",
                    &[&path.schema.clone().unwrap_or_default(), &path.name()?.to_string()],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let name = r[0].clone().unwrap_or_default();
                    TreeNode::new(ObjectKind::Constraint, name.clone(), path.clone().with_sub(name)).detail(format!("{} · {}", r[1].clone().unwrap_or_default(), r[2].clone().unwrap_or_default()))
                })
                .collect())
        }
        ObjectKind::Folder(FolderKind::Triggers) => {
            let rows = s
                .text_rows(
                    Some(&db),
                    "SELECT tg.tgname::text, CASE tg.tgenabled WHEN 'D' THEN 'disabled' ELSE '' END FROM pg_trigger tg \
                     JOIN pg_class t ON t.oid = tg.tgrelid JOIN pg_namespace n ON n.oid = t.relnamespace \
                     WHERE n.nspname = $1 AND t.relname = $2 AND NOT tg.tgisinternal ORDER BY 1",
                    &[&path.schema.clone().unwrap_or_default(), &path.name()?.to_string()],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let name = r[0].clone().unwrap_or_default();
                    TreeNode::new(ObjectKind::Trigger, name.clone(), path.clone().with_sub(name)).detail(r[1].clone().unwrap_or_default()).leaf()
                })
                .collect())
        }
        _ => Ok(Vec::new()),
    }
}

pub async fn columns(s: &PgSession, path: &ObjPath) -> Result<Vec<ColumnInfo>> {
    // Identity columns arrived in PostgreSQL 10, generated columns in 12.
    let v = s.version_num();
    let identity = if v >= 100000 { "a.attidentity <> '' OR " } else { "" };
    let generated = if v >= 120000 { "a.attgenerated::text" } else { "''::text" };
    let sql = format!(
        "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), (NOT a.attnotnull)::text, \
         pg_get_expr(d.adbin, d.adrelid), \
         (EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = a.attrelid AND i.indisprimary AND a.attnum = ANY(i.indkey)))::text, \
         ({identity}coalesce(pg_get_expr(d.adbin, d.adrelid), '') LIKE 'nextval(%')::text, \
         col_description(a.attrelid, a.attnum), {generated} \
         FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"
    );
    let rows = s.text_rows(Some(s.db_of(path)), &sql, &[&path.schema.clone().unwrap_or_else(|| "public".into()), &path.name()?.to_string()]).await?;
    if rows.is_empty() {
        return Err(Error::invalid(format!("{} not found", path.display())));
    }
    Ok(rows
        .into_iter()
        .map(|r| {
            let generated = r[7].as_deref() == Some("s");
            ColumnInfo {
                name: r[0].clone().unwrap_or_default(),
                data_type: r[1].clone().unwrap_or_default(),
                nullable: r[2].as_deref() == Some("true"),
                default: r[3].clone(),
                primary_key: r[4].as_deref() == Some("true"),
                auto_increment: r[5].as_deref() == Some("true"),
                comment: if generated { Some(format!("generated: {}", r[3].clone().unwrap_or_default())) } else { r[6].clone() },
            }
        })
        .collect())
}

pub async fn ddl(s: &PgSession, kind: ObjectKind, path: &ObjPath) -> Result<String> {
    let db = s.db_of(path).to_string();
    let schema = path.schema.clone().unwrap_or_default();
    let one = |rows: Vec<Vec<Option<String>>>| -> Result<String> { rows.into_iter().next().and_then(|r| r.into_iter().next().flatten()).ok_or_else(|| Error::invalid("Object not found")) };
    match kind {
        ObjectKind::Table | ObjectKind::ForeignTable => table_ddl(s, path).await,
        ObjectKind::View | ObjectKind::MaterializedView => {
            let def = one(s
                .text_rows(
                    Some(&db),
                    "SELECT pg_get_viewdef(c.oid, true) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname = $2",
                    &[&schema, &path.name()?.to_string()],
                )
                .await?)?;
            let head = if kind == ObjectKind::View { "CREATE OR REPLACE VIEW" } else { "CREATE MATERIALIZED VIEW" };
            Ok(format!("{head} {} AS\n{}", D.qualified(Some(&schema), path.name()?), def.trim_end()))
        }
        ObjectKind::Function | ObjectKind::Procedure => {
            let def = one(s
                .text_rows(
                    Some(&db),
                    "SELECT pg_get_functiondef(p.oid) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                     WHERE n.nspname = $1 AND p.proname = $2 AND pg_get_function_identity_arguments(p.oid) = $3 LIMIT 1",
                    &[&schema, &path.name()?.to_string(), &path.sub.clone().unwrap_or_default()],
                )
                .await?)?;
            Ok(def)
        }
        ObjectKind::Index => one(s
            .text_rows(
                Some(&db),
                "SELECT pg_get_indexdef(i.oid) || ';' FROM pg_class i JOIN pg_namespace n ON n.oid = i.relnamespace WHERE n.nspname = $1 AND i.relname = $2",
                &[&schema, &path.sub.clone().unwrap_or_default()],
            )
            .await?),
        ObjectKind::Trigger => one(s
            .text_rows(
                Some(&db),
                "SELECT pg_get_triggerdef(tg.oid, true) || ';' FROM pg_trigger tg JOIN pg_class t ON t.oid = tg.tgrelid \
                 JOIN pg_namespace n ON n.oid = t.relnamespace WHERE n.nspname = $1 AND t.relname = $2 AND tg.tgname = $3",
                &[&schema, &path.name()?.to_string(), &path.sub.clone().unwrap_or_default()],
            )
            .await?),
        ObjectKind::Constraint => {
            let def = one(s
                .text_rows(
                    Some(&db),
                    "SELECT pg_get_constraintdef(c.oid) FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid \
                     JOIN pg_namespace n ON n.oid = t.relnamespace WHERE n.nspname = $1 AND t.relname = $2 AND c.conname = $3",
                    &[&schema, &path.name()?.to_string(), &path.sub.clone().unwrap_or_default()],
                )
                .await?)?;
            Ok(format!("ALTER TABLE {} ADD CONSTRAINT {} {def};", D.qualified(Some(&schema), path.name()?), D.quote_ident(path.sub.as_deref().unwrap_or_default())))
        }
        ObjectKind::Sequence => {
            let rows = s
                .text_rows(
                    Some(&db),
                    "SELECT data_type::text, start_value::text, min_value::text, max_value::text, increment_by::text, cycle::text, cache_size::text \
                     FROM pg_sequences WHERE schemaname = $1 AND sequencename = $2",
                    &[&schema, &path.name()?.to_string()],
                )
                .await?;
            let r = rows.into_iter().next().ok_or_else(|| Error::invalid("Sequence not found"))?;
            let g = |i: usize| r[i].clone().unwrap_or_default();
            Ok(format!(
                "CREATE SEQUENCE {}\n    AS {}\n    START WITH {}\n    INCREMENT BY {}\n    MINVALUE {}\n    MAXVALUE {}\n    CACHE {}\n    {};",
                D.qualified(Some(&schema), path.name()?),
                g(0),
                g(1),
                g(4),
                g(2),
                g(3),
                g(6),
                if g(5) == "true" { "CYCLE" } else { "NO CYCLE" }
            ))
        }
        ObjectKind::Schema => {
            let owner = one(s.text_rows(Some(&db), "SELECT pg_get_userbyid(nspowner)::text FROM pg_namespace WHERE nspname = $1", &[&schema]).await?)?;
            Ok(format!("CREATE SCHEMA {} AUTHORIZATION {};", D.quote_ident(&schema), D.quote_ident(&owner)))
        }
        ObjectKind::Database => {
            let name = path.database.clone().unwrap_or_default();
            let rows = s
                .text_rows(
                    None,
                    "SELECT pg_get_userbyid(datdba)::text, pg_encoding_to_char(encoding)::text, datcollate::text, datctype::text \
                     FROM pg_database WHERE datname = $1",
                    &[&name],
                )
                .await?;
            let r = rows.into_iter().next().ok_or_else(|| Error::invalid("Database not found"))?;
            let g = |i: usize| r[i].clone().unwrap_or_default();
            Ok(format!(
                "CREATE DATABASE {}\n    WITH OWNER = {}\n    ENCODING = {}\n    LC_COLLATE = {}\n    LC_CTYPE = {};",
                D.quote_ident(&name),
                D.quote_ident(&g(0)),
                D.quote_literal(&g(1)),
                D.quote_literal(&g(2)),
                D.quote_literal(&g(3))
            ))
        }
        _ => Err(Error::unsupported(format!("No DDL for {}", kind.label()))),
    }
}

async fn table_ddl(s: &PgSession, path: &ObjPath) -> Result<String> {
    let db = s.db_of(path).to_string();
    let schema = path.schema.clone().unwrap_or_default();
    let name = path.name()?.to_string();
    let cols = columns(s, path).await?;
    let target = D.qualified(Some(&schema), &name);
    let mut lines: Vec<String> = cols
        .iter()
        .map(|c| {
            let mut l = format!("    {} {}", D.quote_ident(&c.name), c.data_type);
            if let Some(d) = &c.default {
                if c.comment.as_deref().is_some_and(|x| x.starts_with("generated:")) {
                    l.push_str(&format!(" GENERATED ALWAYS AS ({d}) STORED"));
                } else {
                    l.push_str(&format!(" DEFAULT {d}"));
                }
            }
            if !c.nullable {
                l.push_str(" NOT NULL");
            }
            l
        })
        .collect();
    let cons = s
        .text_rows(
            Some(&db),
            "SELECT c.conname::text, pg_get_constraintdef(c.oid) FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace WHERE n.nspname = $1 AND t.relname = $2 AND c.contype <> 'n' \
             ORDER BY CASE c.contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'c' THEN 2 ELSE 3 END, 1",
            &[&schema, &name],
        )
        .await?;
    for c in &cons {
        lines.push(format!("    CONSTRAINT {} {}", D.quote_ident(c[0].as_deref().unwrap_or_default()), c[1].clone().unwrap_or_default()));
    }
    // Declarative partitioning (pg_get_partkeydef) exists from PostgreSQL 10.
    let partkey = if s.version_num() >= 100000 { "CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END" } else { "NULL::text" };
    let info_sql = format!(
        "SELECT c.relkind::text, {partkey}, obj_description(c.oid, 'pg_class'), \
         (SELECT string_agg(quote_ident(pn.nspname) || '.' || quote_ident(pc.relname), ', ') FROM pg_inherits i \
          JOIN pg_class pc ON pc.oid = i.inhparent JOIN pg_namespace pn ON pn.oid = pc.relnamespace WHERE i.inhrelid = c.oid) \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname = $2"
    );
    let info = s.text_rows(Some(&db), &info_sql, &[&schema, &name]).await?;
    let info = info.into_iter().next().unwrap_or_default();
    let mut out = format!("CREATE TABLE {target} (\n{}\n)", lines.join(",\n"));
    if let Some(parents) = info.get(3).cloned().flatten() {
        out.push_str(&format!(" INHERITS ({parents})"));
    }
    if let Some(pk) = info.get(1).cloned().flatten() {
        out.push_str(&format!(" PARTITION BY {pk}"));
    }
    out.push_str(";\n");
    let idx = s
        .text_rows(
            Some(&db),
            "SELECT pg_get_indexdef(x.indexrelid) FROM pg_index x JOIN pg_class t ON t.oid = x.indrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace WHERE n.nspname = $1 AND t.relname = $2 \
             AND NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = x.indexrelid) ORDER BY 1",
            &[&schema, &name],
        )
        .await?;
    for i in idx.into_iter().filter_map(|r| r.into_iter().next().flatten()) {
        out.push_str(&format!("\n{i};"));
    }
    let trg = s
        .text_rows(
            Some(&db),
            "SELECT pg_get_triggerdef(tg.oid, true) FROM pg_trigger tg JOIN pg_class t ON t.oid = tg.tgrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace WHERE n.nspname = $1 AND t.relname = $2 AND NOT tg.tgisinternal ORDER BY tg.tgname",
            &[&schema, &name],
        )
        .await?;
    for t in trg.into_iter().filter_map(|r| r.into_iter().next().flatten()) {
        out.push_str(&format!("\n{t};"));
    }
    if let Some(c) = info.get(2).cloned().flatten() {
        out.push_str(&format!("\n\nCOMMENT ON TABLE {target} IS {};", D.quote_literal(&c)));
    }
    for c in cols.iter().filter(|c| c.comment.as_deref().is_some_and(|x| !x.starts_with("generated:"))) {
        out.push_str(&format!("\nCOMMENT ON COLUMN {target}.{} IS {};", D.quote_ident(&c.name), D.quote_literal(c.comment.as_deref().unwrap_or_default())));
    }
    out.push('\n');
    Ok(out)
}

pub async fn table_page(s: &PgSession, q: TableQuery) -> Result<TablePage> {
    let db = s.db_of(&q.path).to_string();
    let schema = q.path.schema.clone().unwrap_or_else(|| "public".into());
    let name = q.path.name()?.to_string();
    let cols = columns(s, &q.path).await?;
    let kind = s
        .text_rows(Some(&db), "SELECT c.relkind::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname = $2", &[&schema, &name])
        .await?
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next().flatten())
        .unwrap_or_default();
    let is_table = matches!(kind.as_str(), "r" | "p");
    let pk: Vec<String> = cols.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
    let use_ctid = is_table && pk.is_empty();
    let target = D.qualified(Some(&schema), &name);
    let mut sql = if use_ctid { format!("SELECT ctid AS \"__openvclient_ctid\", * FROM {target}") } else { format!("SELECT * FROM {target}") };
    if let Some(f) = q.filter.as_deref().filter(|f| !f.trim().is_empty()) {
        sql.push_str(&format!(" WHERE {f}"));
    }
    if !q.order_by.is_empty() {
        let parts: Vec<String> = q.order_by.iter().map(|(c, d)| format!("{} {}", D.quote_ident(c), if *d == SortDir::Asc { "ASC NULLS FIRST" } else { "DESC NULLS LAST" })).collect();
        sql.push_str(&format!(" ORDER BY {}", parts.join(", ")));
    } else if use_ctid {
        sql.push_str(" ORDER BY ctid");
    } else if !pk.is_empty() {
        let parts: Vec<String> = pk.iter().map(|c| D.quote_ident(c)).collect();
        sql.push_str(&format!(" ORDER BY {}", parts.join(", ")));
    }
    sql.push_str(&format!(" LIMIT {} OFFSET {}", q.limit, q.offset));

    let client = s.client(Some(&db)).await?;
    // Parsing first rejects multi-statement filters and yields column types.
    let stmt = client.prepare(&sql).await.map_err(pg_err)?;
    let types: Vec<String> = stmt.columns().iter().map(|c| cols.iter().find(|x| x.name == c.name()).map(|x| x.data_type.clone()).unwrap_or_else(|| c.type_().name().to_string())).collect();
    drop(stmt);
    let outcome = simple_rows(&client, &sql, usize::MAX, Some(types)).await?;
    let StatementOutcome::Rows(mut rows) = outcome else {
        return Err(Error::other("Table query returned no rows description"));
    };
    let mut row_ids = None;
    if use_ctid && !rows.columns.is_empty() {
        rows.columns.remove(0);
        row_ids = Some(rows.rows.iter_mut().map(|r| r.remove(0).to_text().unwrap_or_default()).collect());
    }
    let read_only_reason = if s.target.read_only {
        Some("Connection is read-only".to_string())
    } else if !is_table {
        Some(format!(
            "{} rows are read-only here",
            match kind.as_str() {
                "v" => "View",
                "m" => "Materialized view",
                "f" => "Foreign table",
                _ => "Relation",
            }
        ))
    } else {
        None
    };
    Ok(TablePage { rows, key_columns: pk, row_ids, read_only_reason })
}

pub async fn apply_changes(s: &PgSession, path: &ObjPath, changes: Vec<RowChange>) -> Result<u64> {
    if s.target.read_only {
        return Err(Error::invalid("Connection is read-only"));
    }
    let db = s.db_of(path).to_string();
    let cols = columns(s, path).await?;
    let ty = |c: &str| -> Result<String> { cols.iter().find(|x| x.name == c).map(|x| x.data_type.clone()).ok_or_else(|| Error::invalid(format!("Unknown column {c}"))) };
    let target = D.qualified(path.schema.as_deref(), path.name()?);
    let client = s.client(Some(&db)).await?;
    client.batch_execute("BEGIN").await.map_err(pg_err)?;
    let mut total = 0u64;
    let result: Result<()> = async {
        for ch in changes {
            let mut params: Vec<Option<String>> = Vec::new();
            let mut ph = |v: Option<String>, t: &str, params: &mut Vec<Option<String>>| {
                params.push(v);
                format!("CAST(${}::text AS {t})", params.len())
            };
            let sql = match ch {
                RowChange::Update { key, values } => {
                    let mut sets = Vec::new();
                    for (c, v) in values {
                        sets.push(format!("{} = {}", D.quote_ident(&c), ph(v, &ty(&c)?, &mut params)));
                    }
                    let w = where_key(&key, &ty, &mut params, &mut ph)?;
                    format!("UPDATE {target} SET {} WHERE {w}", sets.join(", "))
                }
                RowChange::Insert { values } => {
                    if values.is_empty() {
                        format!("INSERT INTO {target} DEFAULT VALUES")
                    } else {
                        let mut names = Vec::new();
                        let mut vals = Vec::new();
                        for (c, v) in values {
                            names.push(D.quote_ident(&c));
                            vals.push(ph(v, &ty(&c)?, &mut params));
                        }
                        format!("INSERT INTO {target} ({}) VALUES ({})", names.join(", "), vals.join(", "))
                    }
                }
                RowChange::Delete { key } => {
                    let w = where_key(&key, &ty, &mut params, &mut ph)?;
                    format!("DELETE FROM {target} WHERE {w}")
                }
            };
            let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params.iter().map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync)).collect();
            let n = client.execute(&sql, &refs).await.map_err(pg_err)?;
            if n == 0 {
                return Err(Error::server("A row was not found (changed or deleted by someone else?). No changes were saved."));
            }
            total += n;
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            client.batch_execute("COMMIT").await.map_err(pg_err)?;
            Ok(total)
        }
        Err(e) => {
            let _ = client.batch_execute("ROLLBACK").await;
            Err(e)
        }
    }
}

fn where_key(key: &RowKey, ty: &dyn Fn(&str) -> Result<String>, params: &mut Vec<Option<String>>, ph: &mut dyn FnMut(Option<String>, &str, &mut Vec<Option<String>>) -> String) -> Result<String> {
    Ok(match key {
        RowKey::RowId(ctid) => format!("ctid = {}", ph(Some(ctid.clone()), "tid", params)),
        RowKey::PrimaryKey(cols) => {
            let mut parts = Vec::new();
            for (c, v) in cols {
                if v.is_null() {
                    parts.push(format!("{} IS NULL", D.quote_ident(c)));
                } else {
                    parts.push(format!("{} = {}", D.quote_ident(c), ph(v.to_text(), &ty(c)?, params)));
                }
            }
            parts.join(" AND ")
        }
    })
}

pub async fn catalog(s: &PgSession, database: Option<&str>) -> Result<Catalog> {
    let db = database.unwrap_or(&s.target.database).to_string();
    let rows = s
        .text_rows(
            Some(&db),
            &format!(
                "SELECT n.nspname::text, c.relname::text, c.relkind::text, a.attname::text \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 LEFT JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
                 WHERE c.relkind IN ('r','p','v','m','f') AND {USER_SCHEMAS} ORDER BY 1, 2, a.attnum LIMIT 50000"
            ),
            &[],
        )
        .await?;
    let mut cat = Catalog::default();
    for r in rows {
        let (schema, table, kind) = (r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default(), r[2].clone().unwrap_or_default());
        let entry = cat.columns.entry(table.clone()).or_default();
        if entry.is_empty() {
            let k = match kind.as_str() {
                "v" => ObjectKind::View,
                "m" => ObjectKind::MaterializedView,
                "f" => ObjectKind::ForeignTable,
                _ => ObjectKind::Table,
            };
            if !cat.schemas.contains(&schema) {
                cat.schemas.push(schema.clone());
            }
            cat.objects.push((schema, table.clone(), k));
        }
        if let Some(c) = r[3].clone() {
            entry.push(c);
        }
    }
    let funcs = s.text_rows(Some(&db), &format!("SELECT DISTINCT p.proname::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE {USER_SCHEMAS} LIMIT 5000"), &[]).await?;
    cat.functions = funcs.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
    Ok(cat)
}
