//! Table/view data browser with transactional editing.

use std::collections::{HashMap, HashSet};

use egui::{Key, Modifiers, RichText};
use egui_material_icons::icons::*;
use openvclient_core::sql::{ColumnInfo, ObjPath, ObjectKind, RowChange, RowKey, SortDir, TablePage, TableQuery};
use openvclient_core::{Result, ResultSet, Value};
use uuid::Uuid;

use crate::app::AppCtx;
use crate::bg::{Job, poll};
use crate::export::Format;
use crate::theme::{self, ico};
use crate::widgets::grid::{Grid, GridEvent, GridState, Overlay, detail_window};

#[derive(PartialEq, Clone, Copy)]
enum View {
    Data,
    Structure,
    Ddl,
}

pub struct TableTab {
    pub conn: Uuid,
    pub kind: ObjectKind,
    pub path: ObjPath,
    view: View,
    filter: String,
    applied_filter: Option<String>,
    /// Filters applied in this tab, newest first.
    recent_filters: Vec<String>,
    order: Vec<(String, SortDir)>,
    limit: u64,
    offset: u64,
    page: Option<Result<TablePage>>,
    job: Option<Job<Result<TablePage>>>,
    count: Option<Result<u64>>,
    count_job: Option<Job<Result<u64>>>,
    columns: Option<Result<Vec<ColumnInfo>>>,
    columns_job: Option<Job<Result<Vec<ColumnInfo>>>>,
    ddl: Option<Result<String>>,
    ddl_job: Option<Job<Result<String>>>,
    edits: HashMap<(usize, usize), Option<String>>,
    deleted: HashSet<usize>,
    inserted: usize,
    save_job: Option<Job<Result<u64>>>,
    pub grid: GridState,
    generation: u64,
}

impl TableTab {
    pub fn new(cx: &AppCtx, conn: Uuid, kind: ObjectKind, path: ObjPath) -> Self {
        let mut t = Self {
            conn,
            kind,
            path,
            view: View::Data,
            filter: String::new(),
            applied_filter: None,
            recent_filters: Vec::new(),
            order: Vec::new(),
            limit: cx.store.ws.settings.page_size,
            offset: 0,
            page: None,
            job: None,
            count: None,
            count_job: None,
            columns: None,
            columns_job: None,
            ddl: None,
            ddl_job: None,
            edits: HashMap::new(),
            deleted: HashSet::new(),
            inserted: 0,
            save_job: None,
            grid: GridState::default(),
            generation: 0,
        };
        t.load(cx);
        t
    }

    fn pending(&self) -> usize {
        let edited_rows: HashSet<usize> = self.edits.keys().map(|(r, _)| *r).collect();
        edited_rows.len() + self.deleted.len() + self.inserted.saturating_sub(0)
    }

    fn base_rows(&self) -> usize {
        match &self.page {
            Some(Ok(p)) => p.rows.rows.len() - self.inserted,
            _ => 0,
        }
    }

    fn load(&mut self, cx: &AppCtx) {
        let Some(s) = cx.conns.session(self.conn).and_then(|h| h.sql()).cloned() else { return };
        let q = TableQuery { path: self.path.clone(), filter: self.applied_filter.clone(), order_by: self.order.clone(), limit: self.limit, offset: self.offset };
        self.job = Some(cx.bg.spawn(format!("Loading {}", self.path.display()), async move { s.table_page(q).await }));
        self.edits.clear();
        self.deleted.clear();
        self.inserted = 0;
        self.grid.editing = None;
    }

    fn count(&mut self, cx: &AppCtx) {
        let Some(s) = cx.conns.session(self.conn).and_then(|h| h.sql()).cloned() else { return };
        let (p, f) = (self.path.clone(), self.applied_filter.clone());
        self.count_job = Some(cx.bg.spawn("Counting rows", async move { s.count_rows(&p, f.as_deref()).await }));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx, tab_id: u64) {
        let t = theme::tokens(ui);
        if let Some(r) = poll(&mut self.job) {
            self.page = Some(r);
            self.generation += 1;
            self.grid.selected = None;
            self.grid.selected_rows.clear();
        }
        if let Some(r) = poll(&mut self.count_job) {
            self.count = Some(r);
        }
        if let Some(r) = poll(&mut self.columns_job) {
            self.columns = Some(r);
        }
        if let Some(r) = poll(&mut self.ddl_job) {
            self.ddl = Some(r);
        }
        if let Some(r) = poll(&mut self.save_job) {
            match r {
                Ok(n) => {
                    cx.toasts.ok(format!("Saved · {n} row(s) changed"));
                    self.load(cx);
                }
                Err(e) => cx.toasts.error("Nothing was saved (the transaction was rolled back)", &e),
            }
        }
        let caps = cx.conns.session(self.conn).and_then(|h| h.sql()).map(|s| s.capabilities()).unwrap_or_default();
        let dialect = cx.conns.session(self.conn).and_then(|h| h.sql()).map(|s| s.dialect()).unwrap_or(openvclient_core::sqltext::Dialect::Postgres);
        let refresh = ui.ui_contains_pointer() && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F5));

        ui.horizontal(|ui| {
            crate::views::header(ui, crate::views::kind_icon(self.kind), &self.path.display(), self.kind.label());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.selectable_value(&mut self.view, View::Ddl, ico(ICON_CODE, "DDL"));
                ui.selectable_value(&mut self.view, View::Structure, ico(ICON_VIEW_COLUMN, "Structure"));
                ui.selectable_value(&mut self.view, View::Data, ico(ICON_TABLE_ROWS, "Data"));
            });
        });
        ui.add_space(theme::space::XS);
        match self.view {
            View::Structure => {
                if self.columns.is_none()
                    && self.columns_job.is_none()
                    && let Some(s) = cx.conns.session(self.conn).and_then(|h| h.sql()).cloned()
                {
                    let p = self.path.clone();
                    self.columns_job = Some(cx.bg.spawn("", async move { s.columns(&p).await }));
                }
                structure_ui(ui, &self.columns, tab_id);
                return;
            }
            View::Ddl => {
                if self.ddl.is_none()
                    && self.ddl_job.is_none()
                    && let Some(s) = cx.conns.session(self.conn).and_then(|h| h.sql()).cloned()
                {
                    let (k, p) = (self.kind, self.path.clone());
                    self.ddl_job = Some(cx.bg.spawn("", async move { s.ddl(k, &p).await }));
                }
                crate::views::object::ddl_ui(ui, cx, self.conn, &self.ddl, self.ddl_job.is_some(), self.path.database.clone());
                return;
            }
            View::Data => {}
        }

        // Filter bar: native SQL WHERE clause, evaluated by the server.
        let mut apply = refresh;
        ui.horizontal(|ui| {
            ui.label(RichText::new("WHERE").monospace().strong().color(t.muted_fg));
            let r = ui.add(egui::TextEdit::singleline(&mut self.filter).font(egui::TextStyle::Monospace).hint_text(where_example(dialect)).desired_width(ui.available_width() - 250.0));
            if !self.recent_filters.is_empty() {
                ui.menu_button(String::from(ICON_HISTORY), |ui| {
                    for f in self.recent_filters.clone() {
                        if ui.button(RichText::new(&f).monospace()).clicked() {
                            self.filter = f;
                            apply = true;
                            ui.close();
                        }
                    }
                })
                .response
                .on_hover_text("Recent filters");
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                apply = true;
            }
            if ui.button(ico(ICON_FILTER_LIST, "Apply")).clicked() {
                apply = true;
            }
            if !self.filter.is_empty() && ui.button(String::from(ICON_CLOSE)).on_hover_text("Clear filter").clicked() {
                self.filter.clear();
                apply = true;
            }
            if ui.button(String::from(ICON_REFRESH)).on_hover_text("Reload (F5)").clicked() {
                apply = true;
            }
        });
        if apply {
            if self.pending() > 0 {
                cx.toasts.warn("Unsaved changes", "Save or discard your edits before reloading.");
            } else {
                self.applied_filter = (!self.filter.trim().is_empty()).then(|| self.filter.trim().to_string());
                if let Some(f) = &self.applied_filter {
                    self.recent_filters.retain(|x| x != f);
                    self.recent_filters.insert(0, f.clone());
                    self.recent_filters.truncate(10);
                }
                self.offset = 0;
                self.count = None;
                self.load(cx);
            }
        }

        let page_ok = matches!(&self.page, Some(Ok(_)));
        let ro_reason = match &self.page {
            Some(Ok(p)) => p.read_only_reason.clone(),
            _ => None,
        };
        let editable = page_ok && ro_reason.is_none() && caps.edit_data;
        // Action bar.
        ui.horizontal(|ui| {
            let loading = self.job.is_some();
            let n = self.base_rows() as u64;
            if ui.add_enabled(self.offset > 0 && !loading, egui::Button::new(String::from(ICON_CHEVRON_LEFT))).on_hover_text("Previous page").clicked() {
                self.offset = self.offset.saturating_sub(self.limit);
                self.load(cx);
            }
            let range = if n == 0 { "0 rows".into() } else { format!("{}–{}", self.offset + 1, self.offset + n) };
            ui.label(RichText::new(range).monospace());
            if ui.add_enabled(n >= self.limit && !loading, egui::Button::new(String::from(ICON_CHEVRON_RIGHT))).on_hover_text("Next page").clicked() {
                self.offset += self.limit;
                self.load(cx);
            }
            match &self.count {
                Some(Ok(c)) => {
                    ui.label(RichText::new(format!("of {c}")).color(t.muted_fg));
                }
                Some(Err(e)) => {
                    ui.label(RichText::new("count failed").color(t.destructive)).on_hover_text(e.to_string());
                }
                None if self.count_job.is_some() => crate::views::busy(ui, "counting"),
                None => {
                    if ui.small_button("Count").on_hover_text("Run COUNT(*) with the current filter").clicked() {
                        self.count(cx);
                    }
                }
            }
            ui.label("per page");
            let mut lim = self.limit;
            if ui.add(egui::DragValue::new(&mut lim).range(10..=10_000).speed(10)).lost_focus() && lim != self.limit {
                self.limit = lim;
                self.offset = 0;
                self.load(cx);
            }
            if loading {
                ui.add(egui::Spinner::new());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button(ico(ICON_DOWNLOAD, "Export"), |ui| {
                    if let Some(Ok(p)) = &self.page {
                        let rs = p.rows.clone();
                        for f in Format::ALL {
                            if ui.button(format!("This page as {}…", f.label())).clicked() {
                                let rows: Vec<usize> = (0..rs.rows.len() - self.inserted).collect();
                                crate::views::sql::export_to_file(cx, &rs, &rows, f, dialect);
                                ui.close();
                            }
                        }
                    }
                });
                if editable && ui.button(ico(ICON_UPLOAD, "Import CSV…")).on_hover_text("Insert rows from a CSV whose header names columns").clicked() {
                    self.import_csv(cx);
                }
                if let Some(r) = &ro_reason {
                    theme::badge(ui, "READ-ONLY", t.muted_fg).on_hover_text(r.clone());
                }
            });
        });

        // Pending changes bar.
        if editable {
            ui.horizontal(|ui| {
                if ui.button(ico(ICON_ADD, "Add row")).clicked()
                    && let Some(Ok(p)) = &mut self.page
                {
                    let n = p.rows.columns.len();
                    p.rows.rows.push(vec![Value::Null; n]);
                    if let Some(ids) = &mut p.row_ids {
                        ids.push(String::new());
                    }
                    self.inserted += 1;
                    self.generation += 1;
                }
                let sel = self.grid.selected_rows_sorted();
                if ui.add_enabled(!sel.is_empty(), egui::Button::new(ico(ICON_DELETE, &format!("Delete {}", sel.len())))).on_hover_text("Mark selected rows for deletion").clicked() {
                    self.mark_deleted(sel);
                }
                let pending = !self.edits.is_empty() || !self.deleted.is_empty() || self.inserted > 0;
                if pending {
                    ui.separator();
                    let edited_rows: HashSet<usize> = self.edits.keys().map(|(r, _)| *r).filter(|r| *r < self.base_rows()).collect();
                    ui.label(RichText::new(format!("{} edited · {} new · {} deleted", edited_rows.len(), self.inserted, self.deleted.len())).color(t.warning));
                    if self.save_job.is_some() {
                        crate::views::busy(ui, "Saving in one transaction…");
                    } else {
                        if theme::primary_button(ui, ico(ICON_SAVE, "Save changes")).clicked() {
                            self.save(cx);
                        }
                        if ui.button("Discard").clicked() {
                            self.load(cx);
                        }
                    }
                }
            });
        }
        ui.add_space(theme::space::XS);

        match &self.page {
            None => crate::views::busy(ui, "Loading rows…"),
            Some(Err(e)) => crate::views::error_box(ui, &e.clone()),
            Some(Ok(p)) => {
                let rs = p.rows.clone();
                let base = rs.rows.len() - self.inserted;
                let events = Grid {
                    id: egui::Id::unique(("tablegrid", tab_id, self.generation)),
                    rs: &rs,
                    editable,
                    server_sort: Some(&self.order),
                    overlay: Overlay { edited: Some(&self.edits), deleted: Some(&self.deleted), inserted_from: Some(base) },
                    generation: self.generation,
                    row_offset: self.offset,
                    server_filter: true,
                }
                .show(ui, &mut self.grid);
                detail_window(ui.ctx(), egui::Id::unique(("tdetail", tab_id)), &rs, &mut self.grid);
                for e in events {
                    match e {
                        GridEvent::HeaderClicked(c) => {
                            if self.pending() > 0 {
                                cx.toasts.warn("Unsaved changes", "Save or discard before re-sorting.");
                                continue;
                            }
                            let name = rs.columns[c].name.clone();
                            self.order = match self.order.first() {
                                Some((n, SortDir::Asc)) if *n == name => vec![(name, SortDir::Desc)],
                                Some((n, SortDir::Desc)) if *n == name => vec![],
                                _ => vec![(name, SortDir::Asc)],
                            };
                            self.load(cx);
                        }
                        GridEvent::CommitEdit { row, col, text } => {
                            let original = rs.rows[row][col].to_text();
                            if row < base && original.as_deref() == Some(text.as_str()) {
                                self.edits.remove(&(row, col));
                            } else {
                                self.edits.insert((row, col), Some(text));
                            }
                        }
                        GridEvent::SetNull { row, col } => {
                            self.edits.insert((row, col), None);
                        }
                        GridEvent::DeleteRows(rows) => self.mark_deleted(rows),
                        GridEvent::RevertRow(r) => {
                            self.edits.retain(|(er, _), _| *er != r);
                            self.deleted.remove(&r);
                        }
                        GridEvent::CopyRows(rows, f) => {
                            ui.ctx().copy_text(crate::export::render(&rs, &rows, None, f, Some(dialect), &dialect.qualified(self.path.schema.as_deref(), self.path.name.as_deref().unwrap_or(""))));
                            cx.toasts.ok(format!("Copied {} row(s) as {}", rows.len(), f.label()));
                        }
                        GridEvent::FilterBy { col, value, exclude } => {
                            if self.pending() > 0 {
                                cx.toasts.warn("Unsaved changes", "Save or discard before filtering.");
                                continue;
                            }
                            let cond = filter_condition(dialect, &rs.columns[col].name, &value, exclude);
                            let cur = self.filter.trim();
                            self.filter = if cur.is_empty() { cond } else { format!("({cur}) AND {cond}") };
                            self.applied_filter = Some(self.filter.clone());
                            self.offset = 0;
                            self.load(cx);
                        }
                    }
                }
            }
        }
    }

    fn mark_deleted(&mut self, rows: Vec<usize>) {
        let base = self.base_rows();
        let mut drop_new: Vec<usize> = Vec::new();
        for r in rows {
            if r >= base {
                drop_new.push(r);
            } else {
                self.deleted.insert(r);
            }
        }
        // Unsaved new rows are simply removed.
        drop_new.sort_unstable_by(|a, b| b.cmp(a));
        if let Some(Ok(p)) = &mut self.page {
            for r in drop_new {
                p.rows.rows.remove(r);
                self.edits.retain(|(er, _), _| *er != r);
                let shifted: HashMap<_, _> = self.edits.drain().map(|((er, c), v)| (if er > r { (er - 1, c) } else { (er, c) }, v)).collect();
                self.edits = shifted;
                self.inserted -= 1;
            }
        }
        self.grid.selected_rows.clear();
        self.generation += 1;
    }

    fn row_key(p: &TablePage, r: usize) -> Option<RowKey> {
        if !p.key_columns.is_empty() {
            let cols = p.key_columns.iter().map(|k| (k.clone(), p.rows.column_index(k).map(|i| p.rows.rows[r][i].clone()).unwrap_or(Value::Null))).collect();
            Some(RowKey::PrimaryKey(cols))
        } else {
            p.row_ids.as_ref().and_then(|ids| ids.get(r)).map(|id| RowKey::RowId(id.clone()))
        }
    }

    fn save(&mut self, cx: &mut AppCtx) {
        let Some(Ok(p)) = &self.page else { return };
        let Some(s) = cx.conns.session(self.conn).and_then(|h| h.sql()).cloned() else { return };
        let base = p.rows.rows.len() - self.inserted;
        let mut changes = Vec::new();
        let mut rows: Vec<usize> = self.edits.keys().map(|(r, _)| *r).filter(|r| *r < base && !self.deleted.contains(r)).collect();
        rows.sort();
        rows.dedup();
        for r in rows {
            let Some(key) = Self::row_key(p, r) else { continue };
            let mut values: Vec<(String, Option<String>)> = self.edits.iter().filter(|((er, _), _)| *er == r).map(|((_, c), v)| (p.rows.columns[*c].name.clone(), v.clone())).collect();
            values.sort();
            changes.push(RowChange::Update { key, values });
        }
        for r in base..p.rows.rows.len() {
            let values = self.edits.iter().filter(|((er, _), _)| *er == r).map(|((_, c), v)| (p.rows.columns[*c].name.clone(), v.clone())).collect();
            changes.push(RowChange::Insert { values });
        }
        let mut del: Vec<usize> = self.deleted.iter().copied().collect();
        del.sort();
        for r in del {
            if let Some(key) = Self::row_key(p, r) {
                changes.push(RowChange::Delete { key });
            }
        }
        let path = self.path.clone();
        let n = changes.len();
        self.save_job = Some(cx.bg.spawn(format!("Saving {n} change(s)"), async move { s.apply_changes(&path, changes).await }));
    }

    fn import_csv(&mut self, cx: &mut AppCtx) {
        let Some(path) = rfd::FileDialog::new().add_filter("CSV", &["csv", "tsv"]).pick_file() else { return };
        let data = match crate::export::read_csv(&path) {
            Ok(d) => d,
            Err(e) => {
                cx.toasts.error_text("Could not read CSV", e);
                return;
            }
        };
        let cols: Vec<String> = match &self.page {
            Some(Ok(p)) => p.rows.columns.iter().map(|c| c.name.clone()).collect(),
            _ => return,
        };
        let unknown: Vec<&String> = data.header.iter().filter(|h| !cols.iter().any(|c| c.eq_ignore_ascii_case(h))).collect();
        if !unknown.is_empty() {
            cx.toasts.error_text("CSV header does not match the table", format!("Unknown columns: {}", unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
            return;
        }
        let header: Vec<String> = data.header.iter().map(|h| cols.iter().find(|c| c.eq_ignore_ascii_case(h)).cloned().unwrap_or_default()).collect();
        let changes: Vec<RowChange> =
            data.rows.into_iter().map(|r| RowChange::Insert { values: header.iter().cloned().zip(r.into_iter().map(|v| if v.is_empty() { None } else { Some(v) })).collect() }).collect();
        let n = changes.len();
        let Some(s) = cx.conns.session(self.conn).and_then(|h| h.sql()).cloned() else { return };
        let target = self.path.clone();
        let file = path.display().to_string();
        cx.confirm(
            Some(self.conn),
            format!("Import {n} rows?"),
            format!("Rows from {file} are inserted into {} in one transaction. Empty cells become NULL.", self.path.display()),
            "Import",
            None,
            move |cx| {
                let job = cx.bg.spawn(format!("Importing {n} rows"), async move { s.apply_changes(&target, changes).await.map(|n| format!("{n} rows inserted")) });
                cx.dialog = Some(crate::views::dialogs::Dialog::Tracked { job, done: "Import finished".into(), conn: None });
            },
        );
    }
}

pub fn structure_ui(ui: &mut egui::Ui, cols: &Option<Result<Vec<ColumnInfo>>>, tab_id: u64) {
    match cols {
        None => crate::views::busy(ui, "Loading columns…"),
        Some(Err(e)) => crate::views::error_box(ui, e),
        Some(Ok(cols)) => {
            let rs = ResultSet::from_text_rows(
                &["#", "column", "type", "nullable", "default", "key", "notes"],
                cols.iter()
                    .enumerate()
                    .map(|(i, c)| {
                        vec![
                            Some((i + 1).to_string()),
                            Some(c.name.clone()),
                            Some(c.data_type.clone()),
                            Some(if c.nullable { "yes" } else { "no" }.into()),
                            c.default.clone(),
                            Some(if c.primary_key { "PK" } else { "" }.into()),
                            Some([c.auto_increment.then(|| "auto".to_string()), c.comment.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ")),
                        ]
                    })
                    .collect(),
            );
            let mut st = GridState::default();
            Grid { id: egui::Id::unique(("structure", tab_id)), rs: &rs, editable: false, server_sort: None, overlay: Overlay::default(), generation: 0, row_offset: 0, server_filter: false }
                .show(ui, &mut st);
        }
    }
}

/// A WHERE example in the connection's own SQL dialect.
fn where_example(d: openvclient_core::sqltext::Dialect) -> &'static str {
    use openvclient_core::sqltext::Dialect::*;
    match d {
        Postgres => "e.g. status = 'active' AND created_at > now() - interval '1 day'",
        MySql => "e.g. status = 'active' AND created_at > NOW() - INTERVAL 1 DAY",
        Sqlite => "e.g. status = 'active' AND created_at > datetime('now', '-1 day')",
        SqlServer => "e.g. status = 'active' AND created_at > DATEADD(day, -1, GETDATE())",
    }
}

/// `col = value` (or `<>`), quoted for the dialect; NULL uses IS [NOT] NULL.
fn filter_condition(d: openvclient_core::sqltext::Dialect, col: &str, v: &openvclient_core::Value, exclude: bool) -> String {
    let c = d.quote_ident(col);
    match v {
        openvclient_core::Value::Null => format!("{c} IS {}NULL", if exclude { "NOT " } else { "" }),
        other => {
            let lit = if other.is_numeric() { other.to_text().unwrap_or_default() } else { d.quote_literal(&other.to_text().unwrap_or_default()) };
            format!("{c} {} {lit}", if exclude { "<>" } else { "=" })
        }
    }
}
