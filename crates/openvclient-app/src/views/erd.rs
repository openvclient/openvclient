//! Entity-relationship diagram of one schema: tables as boxes with their
//! columns, foreign keys as arrows. Boxes can be dragged; the layout is kept
//! for the tab. Also holds the schema loader shared with schema compare.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use egui::{Align2, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};
use egui_material_icons::icons::*;
use openvclient_core::Result;
use openvclient_core::sql::{ExecOptions, ObjPath, ObjectKind, SqlSession, StatementOutcome};
use openvclient_core::sqltext::Dialect;
use uuid::Uuid;

use crate::app::{AppCtx, Cmd};
use crate::bg::{Job, poll};
use crate::theme::{self, ico};

#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    pub data_type: String,
    pub primary_key: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ForeignKey {
    pub table: String,
    pub column: String,
    pub ref_table: String,
    pub ref_column: String,
}

/// Tables (in name order) with their columns, plus foreign keys.
#[derive(Clone, Debug, Default)]
pub struct SchemaModel {
    pub tables: BTreeMap<String, Vec<Column>>,
    pub fks: Vec<ForeignKey>,
}

/// The schema a connection uses when none is named.
pub fn default_schema(d: Dialect, database: Option<&str>) -> String {
    match d {
        Dialect::Postgres => "public".into(),
        Dialect::SqlServer => "dbo".into(),
        Dialect::Sqlite => "main".into(),
        Dialect::MySql => database.unwrap_or_default().to_string(),
    }
}

fn queries(d: Dialect, schema: &str) -> (String, String) {
    let s = d.quote_literal(schema);
    match d {
        Dialect::Postgres => (
            format!(
                "SELECT c.table_name::text, c.column_name::text, c.data_type::text, \
                 (EXISTS (SELECT 1 FROM information_schema.table_constraints tc \
                   JOIN information_schema.key_column_usage k ON k.constraint_name = tc.constraint_name AND k.table_schema = tc.table_schema \
                   WHERE tc.constraint_type = 'PRIMARY KEY' AND tc.table_schema = c.table_schema AND tc.table_name = c.table_name AND k.column_name = c.column_name))::text \
                 FROM information_schema.columns c \
                 JOIN information_schema.tables t ON t.table_schema = c.table_schema AND t.table_name = c.table_name AND t.table_type = 'BASE TABLE' \
                 WHERE c.table_schema = {s} ORDER BY c.table_name, c.ordinal_position"
            ),
            format!(
                "SELECT tc.table_name::text, k.column_name::text, ccu.table_name::text, ccu.column_name::text \
                 FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage k ON k.constraint_name = tc.constraint_name AND k.table_schema = tc.table_schema \
                 JOIN information_schema.constraint_column_usage ccu ON ccu.constraint_name = tc.constraint_name AND ccu.constraint_schema = tc.table_schema \
                 WHERE tc.constraint_type = 'FOREIGN KEY' AND tc.table_schema = {s}"
            ),
        ),
        Dialect::MySql => (
            format!(
                "SELECT c.TABLE_NAME, c.COLUMN_NAME, c.COLUMN_TYPE, IF(c.COLUMN_KEY = 'PRI', 'true', 'false') \
                 FROM information_schema.COLUMNS c JOIN information_schema.TABLES t ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME AND t.TABLE_TYPE = 'BASE TABLE' \
                 WHERE c.TABLE_SCHEMA = {s} ORDER BY c.TABLE_NAME, c.ORDINAL_POSITION"
            ),
            format!(
                "SELECT TABLE_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA = {s} AND REFERENCED_TABLE_NAME IS NOT NULL"
            ),
        ),
        Dialect::Sqlite => (
            "SELECT m.name, p.name, p.type, CASE WHEN p.pk > 0 THEN 'true' ELSE 'false' END \
             FROM sqlite_master m JOIN pragma_table_info(m.name) p WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' ORDER BY m.name, p.cid"
                .into(),
            "SELECT m.name, f.\"from\", f.\"table\", f.\"to\" FROM sqlite_master m JOIN pragma_foreign_key_list(m.name) f WHERE m.type = 'table'".into(),
        ),
        Dialect::SqlServer => (
            format!(
                "SELECT t.name, c.name, ty.name, CASE WHEN EXISTS (SELECT 1 FROM sys.index_columns ic JOIN sys.indexes i ON i.object_id = ic.object_id AND i.index_id = ic.index_id \
                   WHERE i.is_primary_key = 1 AND ic.object_id = c.object_id AND ic.column_id = c.column_id) THEN 'true' ELSE 'false' END \
                 FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id JOIN sys.columns c ON c.object_id = t.object_id \
                 JOIN sys.types ty ON ty.user_type_id = c.user_type_id WHERE s.name = {s} ORDER BY t.name, c.column_id"
            ),
            format!(
                "SELECT tp.name, cp.name, tr.name, cr.name FROM sys.foreign_key_columns fkc \
                 JOIN sys.tables tp ON tp.object_id = fkc.parent_object_id \
                 JOIN sys.columns cp ON cp.object_id = fkc.parent_object_id AND cp.column_id = fkc.parent_column_id \
                 JOIN sys.tables tr ON tr.object_id = fkc.referenced_object_id \
                 JOIN sys.columns cr ON cr.object_id = fkc.referenced_object_id AND cr.column_id = fkc.referenced_column_id \
                 JOIN sys.schemas s ON s.schema_id = tp.schema_id WHERE s.name = {s}"
            ),
        ),
    }
}

/// Read tables, columns, primary keys and foreign keys of one schema
/// through a short-lived console (read-only catalog queries).
pub async fn load_schema(s: Arc<dyn SqlSession>, database: Option<String>, schema: String) -> Result<SchemaModel> {
    let d = s.dialect();
    let (cols_sql, fk_sql) = queries(d, &schema);
    let console = s.open_console(database).await?;
    let res = console.execute(vec![cols_sql, fk_sql], ExecOptions { max_rows: 200_000 }).await;
    console.close().await;
    let mut model = SchemaModel::default();
    let mut it = res.into_iter();
    let text = |v: &openvclient_core::Value| v.to_text().unwrap_or_default();
    match it.next().map(|r| r.outcome) {
        Some(Ok(StatementOutcome::Rows(rs))) => {
            for r in &rs.rows {
                model.tables.entry(text(&r[0])).or_default().push(Column { name: text(&r[1]), data_type: text(&r[2]), primary_key: matches!(text(&r[3]).as_str(), "true" | "t" | "1") });
            }
        }
        Some(Err(e)) => return Err(e),
        _ => {}
    }
    // Foreign keys are optional: some engines (ClickHouse) have none.
    if let Some(Ok(StatementOutcome::Rows(rs))) = it.next().map(|r| r.outcome) {
        for r in &rs.rows {
            let fk = ForeignKey { table: text(&r[0]), column: text(&r[1]), ref_table: text(&r[2]), ref_column: text(&r[3]) };
            if model.tables.contains_key(&fk.table) && model.tables.contains_key(&fk.ref_table) && !model.fks.contains(&fk) {
                model.fks.push(fk);
            }
        }
    }
    Ok(model)
}

const BOX_W: f32 = 220.0;
const ROW_H: f32 = 18.0;
const HEAD_H: f32 = 28.0;
const MAX_COLS: usize = 14;

pub struct ErdTab {
    pub conn: Uuid,
    pub database: Option<String>,
    pub schema: String,
    job: Option<Job<Result<SchemaModel>>>,
    model: Option<Result<SchemaModel>>,
    pos: HashMap<String, Pos2>,
    zoom: f32,
    filter: String,
    hovered: Option<String>,
}

impl ErdTab {
    pub fn new(conn: Uuid, database: Option<String>, schema: String) -> Self {
        Self { conn, database, schema, job: None, model: None, pos: HashMap::new(), zoom: 1.0, filter: String::new(), hovered: None }
    }

    pub fn title(&self) -> String {
        format!("ER diagram · {}", self.schema)
    }

    fn load(&mut self, cx: &mut AppCtx) {
        let Some(s) = cx.conns.session(self.conn).and_then(|h| h.sql()).cloned() else { return };
        let (db, schema) = (self.database.clone(), self.schema.clone());
        self.job = Some(cx.bg.spawn(format!("Reading schema {schema}"), load_schema(s, db, schema)));
        self.model = None;
    }

    fn box_h(cols: usize) -> f32 {
        HEAD_H + ROW_H * cols.min(MAX_COLS) as f32 + if cols > MAX_COLS { ROW_H } else { 0.0 } + 8.0
    }

    /// Place tables in a grid, most-connected first so related boxes sit together.
    fn layout(&mut self, m: &SchemaModel) {
        let mut degree: HashMap<&str, usize> = HashMap::new();
        for f in &m.fks {
            *degree.entry(f.table.as_str()).or_default() += 1;
            *degree.entry(f.ref_table.as_str()).or_default() += 1;
        }
        let mut names: Vec<&String> = m.tables.keys().collect();
        names.sort_by(|a, b| degree.get(b.as_str()).unwrap_or(&0).cmp(degree.get(a.as_str()).unwrap_or(&0)).then(a.cmp(b)));
        let cols = ((names.len() as f32).sqrt().ceil() as usize).clamp(1, 6);
        let mut col_y = vec![20.0_f32; cols];
        for (i, n) in names.iter().enumerate() {
            let c = i % cols;
            let h = Self::box_h(m.tables[*n].len());
            self.pos.entry((*n).clone()).or_insert(pos2(20.0 + c as f32 * (BOX_W + 70.0), col_y[c]));
            col_y[c] += h + 40.0;
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx) {
        let t = theme::tokens(ui);
        if self.model.is_none() && self.job.is_none() {
            self.load(cx);
        }
        if let Some(r) = poll(&mut self.job) {
            if let Ok(m) = &r {
                let m = m.clone();
                self.layout(&m);
            }
            self.model = Some(r);
        }
        ui.horizontal(|ui| {
            crate::views::header(ui, ICON_SCHEMA, &self.title(), &cx.profile_name(self.conn));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(String::from(ICON_REFRESH)).on_hover_text("Reload").clicked() {
                    self.pos.clear();
                    self.load(cx);
                }
                if ui.button(ico(ICON_GRID_VIEW, "Tidy layout")).on_hover_text("Put every table back on the grid").clicked() {
                    self.pos.clear();
                    if let Some(Ok(m)) = &self.model {
                        let m = m.clone();
                        self.layout(&m);
                    }
                }
                if ui.small_button(String::from(ICON_ZOOM_IN)).on_hover_text("Zoom in").clicked() {
                    self.zoom = (self.zoom + 0.15).min(2.0);
                }
                ui.label(RichText::new(format!("{:.0}%", self.zoom * 100.0)).color(t.muted_fg));
                if ui.small_button(String::from(ICON_ZOOM_OUT)).on_hover_text("Zoom out").clicked() {
                    self.zoom = (self.zoom - 0.15).max(0.3);
                }
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ico(ICON_SEARCH, "Highlight tables")).desired_width(180.0));
            });
        });
        ui.separator();
        let m = match &self.model {
            None => return crate::views::busy(ui, "Reading tables and foreign keys…"),
            Some(Err(e)) => return crate::views::error_box(ui, e),
            Some(Ok(m)) if m.tables.is_empty() => return crate::views::unsupported(ui, "This schema has no tables."),
            Some(Ok(m)) => m.clone(),
        };
        ui.label(RichText::new(format!("{} tables · {} foreign keys · drag a box to move it, double-click its title to open the data", m.tables.len(), m.fks.len())).small().color(t.muted_fg));
        let z = self.zoom;
        let size = self.pos.iter().filter(|(n, _)| m.tables.contains_key(*n)).fold(Vec2::ZERO, |s, (n, p)| s.max(vec2(p.x + BOX_W, p.y + Self::box_h(m.tables[n].len())))) + vec2(40.0, 40.0);
        let filter = self.filter.to_lowercase();
        let mut open = None;
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            let (area, _) = ui.allocate_exact_size((size * z).max(ui.available_size()), Sense::hover());
            let painter = ui.painter_at(area);
            let rect_of = |p: Pos2, cols: usize| Rect::from_min_size(area.min + p.to_vec2() * z, vec2(BOX_W, Self::box_h(cols)) * z);
            let rows_y = |r: Rect, i: usize| r.top() + (HEAD_H + ROW_H * (i.min(MAX_COLS) as f32 + 0.5)) * z;
            let hovered = self.hovered.clone();
            // Foreign keys first, under the boxes.
            for f in &m.fks {
                let (Some(a), Some(b)) = (self.pos.get(&f.table), self.pos.get(&f.ref_table)) else { continue };
                let (ac, bc) = (&m.tables[&f.table], &m.tables[&f.ref_table]);
                let (ra, rb) = (rect_of(*a, ac.len()), rect_of(*b, bc.len()));
                let ia = ac.iter().position(|c| c.name == f.column).unwrap_or(0);
                let ib = bc.iter().position(|c| c.name == f.ref_column).unwrap_or(0);
                let (ya, yb) = (rows_y(ra, ia), rows_y(rb, ib));
                let (sx, ex, dir) = if ra.center().x <= rb.center().x { (ra.right(), rb.left(), 1.0) } else { (ra.left(), rb.right(), -1.0) };
                let hot = hovered.as_deref().is_some_and(|h| h == f.table || h == f.ref_table);
                let color = if hot { t.ring } else { t.muted_fg.gamma_multiply(0.7) };
                let mid = (sx + ex) / 2.0;
                let pts = vec![pos2(sx, ya), pos2(mid, ya), pos2(mid, yb), pos2(ex, yb)];
                painter.add(egui::Shape::line(pts, Stroke::new(if hot { 2.2 } else { 1.3 }, color)));
                // Arrow into the referenced table.
                let tip = pos2(ex, yb);
                let back = -dir * 8.0 * z;
                painter.add(egui::Shape::convex_polygon(vec![tip, pos2(tip.x + back, tip.y - 4.0 * z), pos2(tip.x + back, tip.y + 4.0 * z)], color, Stroke::NONE));
                painter.circle_filled(pos2(sx, ya), 3.0 * z, color);
            }
            let mut new_hover = None;
            let names: Vec<String> = m.tables.keys().cloned().collect();
            for name in names {
                let cols = &m.tables[&name];
                let p = *self.pos.get(&name).unwrap_or(&pos2(20.0, 20.0));
                let r = rect_of(p, cols.len());
                let head = Rect::from_min_size(r.min, vec2(r.width(), HEAD_H * z));
                let resp = ui.interact(head, egui::Id::unique(("erd_box", self.conn, &name)), Sense::click_and_drag()).on_hover_cursor(egui::CursorIcon::Grab);
                if resp.dragged() {
                    let np = p + resp.drag_delta() / z;
                    self.pos.insert(name.clone(), pos2(np.x.max(0.0), np.y.max(0.0)));
                }
                if resp.double_clicked() {
                    open = Some(name.clone());
                }
                if resp.hovered() || ui.rect_contains_pointer(r) {
                    new_hover = Some(name.clone());
                }
                let matched = !filter.is_empty() && name.to_lowercase().contains(&filter);
                let dim = !filter.is_empty() && !matched;
                let stroke = if matched || hovered.as_deref() == Some(name.as_str()) { Stroke::new(2.0, t.ring) } else { Stroke::new(1.0, t.border) };
                painter.rect(r, 8.0 * z, if dim { t.card.gamma_multiply(0.5) } else { t.card }, stroke, egui::StrokeKind::Inside);
                painter.rect_filled(head, egui::CornerRadius { nw: (8.0 * z) as u8, ne: (8.0 * z) as u8, sw: 0, se: 0 }, t.muted);
                painter.text(
                    pos2(head.left() + 10.0 * z, head.center().y),
                    Align2::LEFT_CENTER,
                    format!("{}  {name}", String::from(ICON_TABLE)),
                    FontId::proportional(13.0 * z),
                    if dim { t.muted_fg } else { t.fg },
                );
                for (i, c) in cols.iter().take(MAX_COLS).enumerate() {
                    let y = rows_y(r, i);
                    let is_fk = m.fks.iter().any(|f| f.table == name && f.column == c.name);
                    let icon = if c.primary_key {
                        String::from(ICON_KEY)
                    } else if is_fk {
                        String::from(ICON_LINK)
                    } else {
                        String::new()
                    };
                    let ic = if c.primary_key { t.warning } else { t.info };
                    painter.text(pos2(r.left() + 14.0 * z, y), Align2::CENTER_CENTER, icon, FontId::proportional(11.0 * z), ic);
                    let name_g = crate::views::sidebar::elided(ui, &c.name, FontId::proportional(12.0 * z), t.fg, (BOX_W * 0.52) * z);
                    painter.galley(pos2(r.left() + 26.0 * z, y - name_g.size().y / 2.0), name_g, t.fg);
                    let ty = crate::views::sidebar::elided(ui, &c.data_type, FontId::proportional(11.0 * z), t.muted_fg, (BOX_W * 0.36) * z);
                    painter.galley(pos2(r.right() - 8.0 * z - ty.size().x, y - ty.size().y / 2.0), ty, t.muted_fg);
                }
                if cols.len() > MAX_COLS {
                    painter.text(pos2(r.left() + 26.0 * z, rows_y(r, MAX_COLS)), Align2::LEFT_CENTER, format!("+{} more", cols.len() - MAX_COLS), FontId::proportional(11.0 * z), t.muted_fg);
                }
            }
            self.hovered = new_hover;
        });
        if let Some(name) = open {
            let d = cx.conns.session(self.conn).and_then(|h| h.sql()).map(|s| s.dialect()).unwrap_or(Dialect::Postgres);
            cx.cmd(Cmd::OpenTable { conn: self.conn, kind: ObjectKind::Table, path: table_path(d, self.database.as_deref(), &self.schema, &name) });
        }
    }
}

/// The tree path each engine uses for a table.
pub fn table_path(d: Dialect, database: Option<&str>, schema: &str, name: &str) -> ObjPath {
    match d {
        Dialect::MySql => ObjPath::db(database.unwrap_or(schema)).with_name(name),
        Dialect::Sqlite => ObjPath::default().with_schema(schema).with_name(name),
        _ => match database {
            Some(db) => ObjPath::db(db).with_schema(schema).with_name(name),
            None => ObjPath::default().with_schema(schema).with_name(name),
        },
    }
}
