//! Users, roles and privileges.

use egui::RichText;
use egui_material_icons::icons::*;
use openvclient_core::sql::{GrantSpec, GrantTarget, ObjPath, PrincipalSpec};
use openvclient_core::sqltext::Dialect;
use openvclient_core::{Result, ResultSet};
use uuid::Uuid;

use crate::app::AppCtx;
use crate::bg::{Job, poll};
use crate::theme::{self, ico};
use crate::widgets::grid::{Grid, GridState, Overlay};

#[derive(Clone, Copy, PartialEq)]
enum TargetKind {
    Database,
    Schema,
    AllTables,
    Table,
}

pub struct UsersTab {
    pub conn: Uuid,
    list: Option<Result<ResultSet>>,
    list_job: Option<Job<Result<ResultSet>>>,
    privs: Option<Result<ResultSet>>,
    privs_job: Option<Job<Result<ResultSet>>>,
    selected: Option<(String, Option<String>)>,
    grid: GridState,
    pgrid: GridState,
    generation: u64,
    form: Option<PrincipalSpec>,
    action: Option<Job<Result<()>>>,
    new_password: String,
    grant_target: TargetKind,
    grant_obj: String,
    grant_privs: Vec<&'static str>,
    grant_option: bool,
}

impl UsersTab {
    pub fn new(conn: Uuid) -> Self {
        Self {
            conn,
            list: None,
            list_job: None,
            privs: None,
            privs_job: None,
            selected: None,
            grid: GridState::default(),
            pgrid: GridState::default(),
            generation: 0,
            form: None,
            action: None,
            new_password: String::new(),
            grant_target: TargetKind::Database,
            grant_obj: String::new(),
            grant_privs: Vec::new(),
            grant_option: false,
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx) {
        let t = theme::tokens(ui);
        let Some(s) = cx.conns.session(self.conn).and_then(|h| h.sql()).cloned() else {
            crate::views::unsupported(ui, "Not connected");
            return;
        };
        let caps = s.capabilities();
        let mysql = s.dialect() == Dialect::MySql;
        if let Some(r) = poll(&mut self.list_job) {
            self.list = Some(r);
            self.generation += 1;
        }
        if let Some(r) = poll(&mut self.privs_job) {
            self.privs = Some(r);
            self.generation += 1;
        }
        if let Some(r) = poll(&mut self.action) {
            match r {
                Ok(()) => {
                    cx.toasts.ok("Done");
                    self.form = None;
                    self.list = None;
                    self.privs = None;
                }
                Err(e) => cx.toasts.error("Action failed", &e),
            }
        }
        if self.list.is_none() && self.list_job.is_none() {
            let s2 = s.clone();
            self.list_job = Some(cx.bg.spawn("Loading users", async move { s2.principals().await }));
        }
        ui.horizontal(|ui| {
            crate::views::header(ui, ICON_GROUP, if caps.roles { "Roles & users" } else { "Users" }, &cx.profile_name(self.conn));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(String::from(ICON_REFRESH)).clicked() {
                    self.list = None;
                    self.privs = None;
                }
                if caps.grants && ui.button(ico(ICON_PERSON_ADD, if caps.roles { "New role" } else { "New user" })).clicked() {
                    self.form = Some(PrincipalSpec { can_login: true, host: mysql.then(|| "%".to_string()), ..Default::default() });
                }
            });
        });
        ui.add_space(theme::space::SM);

        if let Some(f) = &mut self.form {
            let mut close = false;
            let mut submit = false;
            egui::Frame::new().fill(t.card).stroke(egui::Stroke::new(1.0, t.border)).corner_radius(8).inner_margin(12).show(ui, |ui| {
                egui::Grid::new("pform").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Name *");
                    ui.add(egui::TextEdit::singleline(&mut f.name).desired_width(220.0));
                    ui.end_row();
                    if mysql {
                        ui.label("Host").on_hover_text("% = any host");
                        let mut h = f.host.clone().unwrap_or_default();
                        ui.add(egui::TextEdit::singleline(&mut h).desired_width(220.0));
                        f.host = Some(h);
                        ui.end_row();
                    }
                    ui.label("Password");
                    let mut p = f.password.clone().unwrap_or_default();
                    ui.add(egui::TextEdit::singleline(&mut p).password(true).desired_width(220.0));
                    f.password = Some(p);
                    ui.end_row();
                    ui.label("");
                    ui.vertical(|ui| {
                        ui.checkbox(&mut f.can_login, if mysql { "Account unlocked (can log in)" } else { "Can log in" });
                        ui.checkbox(&mut f.superuser, if mysql { "All privileges on *.* (with grant option)" } else { "Superuser" });
                        ui.checkbox(&mut f.create_db, "Can create databases");
                        ui.checkbox(&mut f.create_role, if mysql { "Can create users" } else { "Can create roles" });
                    });
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    submit = theme::primary_button(ui, "Create").clicked();
                    close = ui.button("Cancel").clicked();
                });
            });
            if submit && !f.name.trim().is_empty() {
                let (s2, spec) = (s.clone(), f.clone());
                self.action = Some(cx.bg.spawn(format!("Creating {}", spec.name), async move { s2.create_principal(spec).await }));
            }
            if close {
                self.form = None;
            }
            ui.add_space(theme::space::SM);
        }

        let Some(list) = &self.list else {
            crate::views::busy(ui, "Loading…");
            return;
        };
        let rs = match list {
            Err(e) => {
                crate::views::error_box(ui, &e.clone());
                return;
            }
            Ok(rs) => rs.clone(),
        };
        let host_col = rs.column_index("host");
        let avail = ui.available_height();
        egui::Panel::top(egui::Id::unique(("users_list", self.conn))).resizable(true).default_size(avail * 0.45).show(ui, |ui| {
            Grid {
                id: egui::Id::unique(("users", self.conn)),
                rs: &rs,
                editable: false,
                server_sort: None,
                overlay: Overlay::default(),
                generation: self.generation,
                row_offset: 0,
                server_filter: false,
            }
            .show(ui, &mut self.grid);
        });
        let sel = self.grid.selected.and_then(|(r, _)| rs.rows.get(r)).map(|row| (row[0].to_text().unwrap_or_default(), host_col.and_then(|h| row[h].to_text())));
        if sel != self.selected {
            self.selected = sel.clone();
            self.privs = None;
            if let Some((n, h)) = sel.clone() {
                let s2 = s.clone();
                self.privs_job = Some(cx.bg.spawn("", async move { s2.privileges(&n, h.as_deref()).await }));
            }
        }
        let Some((name, host)) = self.selected.clone() else {
            ui.label(RichText::new("Select a user to see and change privileges").color(t.muted_fg));
            return;
        };
        let who = match &host {
            Some(h) => format!("'{name}'@'{h}'"),
            None => name.clone(),
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(&who).strong().monospace());
            ui.add(egui::TextEdit::singleline(&mut self.new_password).password(true).hint_text("New password").desired_width(160.0));
            if ui.add_enabled(!self.new_password.is_empty(), egui::Button::new(ico(ICON_KEY, "Set password"))).clicked() {
                let (s2, n, h, p) = (s.clone(), name.clone(), host.clone(), std::mem::take(&mut self.new_password));
                self.action = Some(cx.bg.spawn("Setting password", async move { s2.set_password(&n, h.as_deref(), &p).await }));
            }
            if ui.button(RichText::new(ico(ICON_DELETE, "Drop…")).color(t.destructive)).clicked() {
                let (s2, n, h) = (s.clone(), name.clone(), host.clone());
                let conn = self.conn;
                cx.confirm(Some(conn), format!("Drop {who}?"), "The account and its privileges are removed. Objects it owns may block this.", "Drop", Some(name.clone()), move |cx| {
                    crate::views::dialogs::track(cx, format!("Dropping {n}"), "User dropped", None, async move { s2.drop_principal(&n, h.as_deref()).await });
                });
            }
        });
        if caps.grants {
            ui.horizontal_wrapped(|ui| {
                ui.label("Grant");
                let target_label = |k: TargetKind| match (k, mysql) {
                    (TargetKind::Database, _) => "database",
                    (TargetKind::Schema, _) => "schema",
                    (TargetKind::AllTables, true) => "all tables in database",
                    (TargetKind::AllTables, false) => "all tables in schema",
                    (TargetKind::Table, _) => "table",
                };
                egui::ComboBox::from_id_salt("gt").selected_text(target_label(self.grant_target)).show_ui(ui, |ui| {
                    let kinds: &[TargetKind] = if mysql { &[TargetKind::Database, TargetKind::Table] } else { &[TargetKind::Database, TargetKind::Schema, TargetKind::AllTables, TargetKind::Table] };
                    for k in kinds {
                        if ui.selectable_value(&mut self.grant_target, *k, target_label(*k)).clicked() {
                            self.grant_privs.clear();
                        }
                    }
                });
                ui.add(egui::TextEdit::singleline(&mut self.grant_obj).hint_text(if self.grant_target == TargetKind::Table { "schema.table" } else { "name" }).desired_width(160.0));
                let target = self.target(mysql);
                for p in s.privilege_names(&target) {
                    let mut on = self.grant_privs.contains(&p);
                    if ui.checkbox(&mut on, p).changed() {
                        if on {
                            self.grant_privs.push(p);
                        } else {
                            self.grant_privs.retain(|x| *x != p);
                        }
                    }
                }
                ui.checkbox(&mut self.grant_option, "with grant option");
                let ready = !self.grant_privs.is_empty() && !self.grant_obj.trim().is_empty();
                for (revoke, label) in [(false, "Grant"), (true, "Revoke")] {
                    if ui.add_enabled(ready, egui::Button::new(label)).clicked() {
                        let spec = GrantSpec {
                            privileges: self.grant_privs.iter().map(|p| p.to_string()).collect(),
                            target: target.clone(),
                            principal: name.clone(),
                            host: host.clone(),
                            with_grant_option: self.grant_option,
                        };
                        let s2 = s.clone();
                        self.action = Some(cx.bg.spawn(label, async move { s2.grant(spec, revoke).await }));
                    }
                }
            });
        }
        ui.add_space(theme::space::XS);
        match &self.privs {
            None => crate::views::busy(ui, "Loading privileges…"),
            Some(Err(e)) => crate::views::error_box(ui, &e.clone()),
            Some(Ok(p)) => {
                let p = p.clone();
                Grid {
                    id: egui::Id::unique(("privs", self.conn)),
                    rs: &p,
                    editable: false,
                    server_sort: None,
                    overlay: Overlay::default(),
                    generation: self.generation,
                    row_offset: 0,
                    server_filter: false,
                }
                .show(ui, &mut self.pgrid);
            }
        }
    }

    fn target(&self, mysql: bool) -> GrantTarget {
        let o = self.grant_obj.trim().to_string();
        match self.grant_target {
            TargetKind::Database => GrantTarget::Database(o),
            TargetKind::Schema => GrantTarget::Schema(o),
            TargetKind::AllTables => GrantTarget::AllTablesInSchema(o),
            TargetKind::Table => {
                let (a, b) = o.split_once('.').map(|(a, b)| (Some(a.to_string()), b.to_string())).unwrap_or((None, o.clone()));
                let path = if mysql { ObjPath { database: a, name: Some(b), ..Default::default() } } else { ObjPath { schema: a, name: Some(b), ..Default::default() } };
                GrantTarget::Table(path)
            }
        }
    }
}
