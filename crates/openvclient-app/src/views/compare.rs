//! Compare the tables and columns of two schemas (on the same or different
//! connections): what exists on one side only, and columns whose type or
//! primary-key flag differs. Read-only; it never changes either side.

use std::collections::BTreeSet;

use egui::RichText;
use egui_material_icons::icons::*;
use openvclient_core::Result;
use uuid::Uuid;

use crate::app::AppCtx;
use crate::bg::Job;
use crate::theme::{self, ico};
use crate::views::erd::{SchemaModel, default_schema, load_schema};

#[derive(Clone)]
pub struct Side {
    pub conn: Uuid,
    pub database: String,
    pub schema: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct Diff {
    pub only_left: Vec<String>,
    pub only_right: Vec<String>,
    /// table → lines describing column differences
    pub changed: Vec<(String, Vec<String>)>,
    pub same: usize,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.only_left.is_empty() && self.only_right.is_empty() && self.changed.is_empty()
    }
}

/// Table and column differences; type names are compared case-insensitively.
pub fn diff(l: &SchemaModel, r: &SchemaModel) -> Diff {
    let mut d = Diff::default();
    let lt: BTreeSet<&String> = l.tables.keys().collect();
    let rt: BTreeSet<&String> = r.tables.keys().collect();
    d.only_left = lt.difference(&rt).map(|s| s.to_string()).collect();
    d.only_right = rt.difference(&lt).map(|s| s.to_string()).collect();
    for t in lt.intersection(&rt) {
        let (lc, rc) = (&l.tables[*t], &r.tables[*t]);
        let mut lines = Vec::new();
        for c in lc {
            match rc.iter().find(|x| x.name == c.name) {
                None => lines.push(format!("column {} only on the left ({})", c.name, c.data_type)),
                Some(x) => {
                    if !x.data_type.eq_ignore_ascii_case(&c.data_type) {
                        lines.push(format!("column {}: {} → {}", c.name, c.data_type, x.data_type));
                    }
                    if x.primary_key != c.primary_key {
                        lines.push(format!("column {}: primary key {} → {}", c.name, if c.primary_key { "yes" } else { "no" }, if x.primary_key { "yes" } else { "no" }));
                    }
                }
            }
        }
        for x in rc {
            if !lc.iter().any(|c| c.name == x.name) {
                lines.push(format!("column {} only on the right ({})", x.name, x.data_type));
            }
        }
        if lines.is_empty() {
            d.same += 1;
        } else {
            d.changed.push((t.to_string(), lines));
        }
    }
    d
}

pub struct CompareTab {
    pub left: Side,
    pub right: Side,
    jobs: Option<(Job<Result<SchemaModel>>, Job<Result<SchemaModel>>)>,
    got: (Option<Result<SchemaModel>>, Option<Result<SchemaModel>>),
    result: Option<std::result::Result<Diff, String>>,
}

impl CompareTab {
    pub fn new(conn: Uuid, database: Option<String>, schema: String) -> Self {
        let side = Side { conn, database: database.unwrap_or_default(), schema };
        Self { left: side.clone(), right: side, jobs: None, got: (None, None), result: None }
    }

    pub fn start(&mut self, cx: &mut AppCtx) {
        let session = |c: Uuid| cx.conns.session(c).and_then(|h| h.sql()).cloned();
        let (Some(a), Some(b)) = (session(self.left.conn), session(self.right.conn)) else {
            self.result = Some(Err("Connect both connections first.".into()));
            return;
        };
        let db = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
        let (ld, ls) = (db(&self.left.database), self.left.schema.trim().to_string());
        let (rd, rs) = (db(&self.right.database), self.right.schema.trim().to_string());
        self.result = None;
        self.got = (None, None);
        self.jobs = Some((cx.bg.spawn("Reading left schema", load_schema(a, ld, ls)), cx.bg.spawn("Reading right schema", load_schema(b, rd, rs))));
    }

    fn side_ui(ui: &mut egui::Ui, cx: &AppCtx, id: &str, side: &mut Side) {
        let t = theme::tokens(ui);
        let sql: Vec<(Uuid, String)> = cx.store.ws.profiles.iter().filter(|p| cx.conns.session(p.id).is_some_and(|s| s.sql().is_some())).map(|p| (p.id, p.name.clone())).collect();
        egui::Grid::new(id).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label(RichText::new("Connection").color(t.muted_fg));
            let current = cx.profile_name(side.conn);
            egui::ComboBox::from_id_salt((id, "conn")).selected_text(current).width(240.0).show_ui(ui, |ui| {
                for (cid, name) in &sql {
                    if ui.selectable_label(side.conn == *cid, name).clicked() && side.conn != *cid {
                        side.conn = *cid;
                        let d = cx.conns.session(*cid).and_then(|h| h.sql()).map(|s| (s.dialect(), s.default_database()));
                        if let Some((d, db)) = d {
                            side.database = db.clone().unwrap_or_default();
                            side.schema = default_schema(d, db.as_deref());
                        }
                    }
                }
            });
            ui.end_row();
            ui.label(RichText::new("Database").color(t.muted_fg));
            ui.add(egui::TextEdit::singleline(&mut side.database).hint_text("default").desired_width(240.0));
            ui.end_row();
            ui.label(RichText::new("Schema").color(t.muted_fg));
            ui.add(egui::TextEdit::singleline(&mut side.schema).desired_width(240.0));
            ui.end_row();
        });
    }

    fn report(&self, d: &Diff) -> String {
        let mut s = format!("Schema compare: {} vs {}\n\n", self.left.schema, self.right.schema);
        for t in &d.only_left {
            s.push_str(&format!("- table {t} only on the left\n"));
        }
        for t in &d.only_right {
            s.push_str(&format!("- table {t} only on the right\n"));
        }
        for (t, lines) in &d.changed {
            s.push_str(&format!("- table {t}\n"));
            for l in lines {
                s.push_str(&format!("  - {l}\n"));
            }
        }
        s.push_str(&format!("\n{} tables identical\n", d.same));
        s
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx) {
        let t = theme::tokens(ui);
        if let Some((a, b)) = &mut self.jobs {
            if let Some(r) = a.take() {
                self.got.0 = Some(r);
            }
            if let Some(r) = b.take() {
                self.got.1 = Some(r);
            }
            if self.got.0.is_some() && self.got.1.is_some() {
                self.jobs = None;
                self.result = Some(match (self.got.0.take().unwrap(), self.got.1.take().unwrap()) {
                    (Ok(l), Ok(r)) => Ok(diff(&l, &r)),
                    (Err(e), _) => Err(format!("Left side: {}", e.message)),
                    (_, Err(e)) => Err(format!("Right side: {}", e.message)),
                });
            }
        }
        crate::views::header(ui, ICON_COMPARE_ARROWS, "Compare schemas", "Tables and columns; nothing is changed on either side");
        ui.add_space(theme::space::SM);
        ui.horizontal_top(|ui| {
            ui.group(|ui| {
                ui.label(RichText::new("LEFT").small().strong().color(t.muted_fg));
                Self::side_ui(ui, cx, "cmp_left", &mut self.left);
            });
            ui.add_space(theme::space::SM);
            if ui.button(String::from(ICON_SWAP_HORIZ)).on_hover_text("Swap sides").clicked() {
                std::mem::swap(&mut self.left, &mut self.right);
            }
            ui.add_space(theme::space::SM);
            ui.group(|ui| {
                ui.label(RichText::new("RIGHT").small().strong().color(t.muted_fg));
                Self::side_ui(ui, cx, "cmp_right", &mut self.right);
            });
        });
        ui.add_space(theme::space::SM);
        ui.horizontal(|ui| {
            let busy = self.jobs.is_some();
            if theme::primary_button(ui, ico(ICON_COMPARE_ARROWS, "Compare")).clicked() && !busy {
                self.start(cx);
            }
            if busy {
                ui.spinner();
            }
            if let Some(Ok(d)) = &self.result
                && ui.button(ico(ICON_CONTENT_COPY, "Copy report")).clicked()
            {
                ui.ctx().copy_text(self.report(d));
                cx.toasts.ok("Copied the comparison");
            }
        });
        ui.separator();
        match &self.result {
            None => crate::views::unsupported(ui, "Pick two schemas and press Compare."),
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(t.destructive));
            }
            Some(Ok(d)) if d.is_empty() => {
                ui.label(RichText::new(ico(ICON_CHECK_CIRCLE, &format!("Identical: {} tables with the same columns", d.same))).color(t.accent));
            }
            Some(Ok(d)) => {
                ui.label(RichText::new(format!("{} only left · {} only right · {} different · {} identical", d.only_left.len(), d.only_right.len(), d.changed.len(), d.same)).color(t.muted_fg));
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for name in &d.only_left {
                        ui.label(RichText::new(ico(ICON_REMOVE_CIRCLE, &format!("{name}  only on the left"))).color(t.destructive));
                    }
                    for name in &d.only_right {
                        ui.label(RichText::new(ico(ICON_ADD_CIRCLE, &format!("{name}  only on the right"))).color(t.accent));
                    }
                    for (name, lines) in &d.changed {
                        egui::CollapsingHeader::new(
                            RichText::new(ico(ICON_CHANGE_CIRCLE, &format!("{name}  ·  {} difference{}", lines.len(), if lines.len() == 1 { "" } else { "s" }))).color(t.warning),
                        )
                        .id_salt(("cmp", name))
                        .default_open(true)
                        .show(ui, |ui| {
                            for l in lines {
                                ui.label(RichText::new(l).monospace());
                            }
                        });
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::erd::Column;

    fn col(n: &str, ty: &str, pk: bool) -> Column {
        Column { name: n.into(), data_type: ty.into(), primary_key: pk }
    }

    #[test]
    fn finds_table_and_column_differences() {
        let mut l = SchemaModel::default();
        l.tables.insert("users".into(), vec![col("id", "integer", true), col("email", "text", false)]);
        l.tables.insert("old".into(), vec![col("id", "integer", true)]);
        let mut r = SchemaModel::default();
        r.tables.insert("users".into(), vec![col("id", "BIGINT", true), col("name", "text", false)]);
        r.tables.insert("new".into(), vec![]);
        let d = diff(&l, &r);
        assert_eq!(d.only_left, vec!["old".to_string()]);
        assert_eq!(d.only_right, vec!["new".to_string()]);
        assert_eq!(d.changed[0].0, "users");
        assert_eq!(d.changed[0].1, vec!["column id: integer → BIGINT", "column email only on the left (text)", "column name only on the right (text)"]);
        assert_eq!(d.same, 0);
    }
}
