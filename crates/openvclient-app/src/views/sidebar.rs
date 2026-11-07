//! Connection tree: folders → connections → engine objects, loaded lazily.

use std::collections::{HashMap, HashSet};

use egui::{RichText, Sense};
use egui_material_icons::icons::*;
use openvclient_core::kv::KvDatabase;
use openvclient_core::sql::{FolderKind, ObjPath, ObjectKind, TreeNode};
use openvclient_core::{ConnectionProfile, Result, SessionHandle};
use uuid::Uuid;

use crate::app::{AppCtx, Cmd};
use crate::bg::Job;
use crate::conns::ConnState;
use crate::theme::{self, ico};
use crate::views::{dialogs::Dialog, docker::Section, kind_icon};

enum Children {
    Loading(Job<Result<Vec<TreeNode>>>),
    Ready(Vec<TreeNode>),
    Failed(String),
}

#[derive(Default)]
pub struct SidebarState {
    pub filter: String,
    children: HashMap<(Uuid, String), Children>,
    expanded: HashSet<(Uuid, String)>,
    collapsed_folders: HashSet<String>,
    kv_dbs: HashMap<Uuid, KvChildren>,
    res: HashMap<(Uuid, String), ResChildren>,
}

enum ResChildren {
    Loading(Job<Result<Vec<openvclient_core::resource::Node>>>),
    Ready(Vec<openvclient_core::resource::Node>),
    Failed(String),
}

enum KvChildren {
    Loading(Job<Result<Vec<KvDatabase>>>),
    Ready(Vec<KvDatabase>),
    Failed(String),
}

const ROOT: &str = "root";

fn node_key(kind: ObjectKind, path: &ObjPath) -> String {
    format!("{kind:?}|{path:?}")
}

impl SidebarState {
    pub fn refresh_resource(&mut self, id: Uuid, node: &str) {
        self.res.remove(&(id, node.to_string()));
        if node.is_empty() {
            self.res.retain(|(c, _), _| *c != id);
        }
    }

    pub fn forget(&mut self, id: Uuid) {
        self.res.retain(|(c, _), _| *c != id);
        self.children.retain(|(c, _), _| *c != id);
        self.kv_dbs.remove(&id);
    }
    pub fn expand(&mut self, id: Uuid) {
        self.expanded.insert((id, ROOT.into()));
    }
    fn is_expanded(&self, id: Uuid, key: &str) -> bool {
        self.expanded.contains(&(id, key.to_string()))
    }
    fn toggle(&mut self, id: Uuid, key: &str) {
        let k = (id, key.to_string());
        if !self.expanded.remove(&k) {
            self.expanded.insert(k);
        }
    }
    /// Names of every loaded object, for global search.
    pub fn loaded_objects(&self) -> Vec<(Uuid, TreeNode)> {
        self.children
            .iter()
            .filter_map(|((c, _), ch)| if let Children::Ready(v) = ch { Some((*c, v)) } else { None })
            .flat_map(|(c, v)| v.iter().filter(|n| !matches!(n.kind, ObjectKind::Folder(_))).map(move |n| (c, n.clone())))
            .collect()
    }
    /// Expand the connection and the given node keys (used by tests and search).
    #[cfg(test)]
    pub fn expand_path(&mut self, id: Uuid, keys: &[String]) {
        self.expand(id);
        for k in keys {
            self.expanded.insert((id, k.clone()));
        }
    }

    pub fn refresh_node(&mut self, id: Uuid, kind: ObjectKind, path: &ObjPath) {
        self.children.remove(&(id, node_key(kind, path)));
    }
}

pub fn ui(ui: &mut egui::Ui, cx: &mut AppCtx) {
    let t = theme::tokens(ui);
    ui.add_space(theme::space::SM);
    ui.horizontal(|ui| {
        ui.label(RichText::new("CONNECTIONS").small().strong().color(t.muted_fg));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button(String::from(ICON_CREATE_NEW_FOLDER)).on_hover_text("New folder").clicked() {
                cx.dialog = Some(Dialog::new_folder());
            }
            if ui.small_button(String::from(ICON_ADD)).on_hover_text(theme::kbd("New connection (⌘N)")).clicked() {
                cx.cmd(Cmd::EditConnection(None));
            }
        });
    });
    ui.add(egui::TextEdit::singleline(&mut cx.sidebar.filter).hint_text(ico(ICON_FILTER_LIST, "Filter")).desired_width(f32::INFINITY));
    ui.add_space(theme::space::XS);

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        if cx.store.ws.profiles.is_empty() {
            ui.add_space(theme::space::XL);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No saved connections").color(t.muted_fg));
                ui.add_space(theme::space::SM);
                if theme::primary_button(ui, ico(ICON_ADD, "Add a connection")).clicked() {
                    cx.cmd(Cmd::EditConnection(None));
                }
            });
            return;
        }
        let filter = cx.sidebar.filter.to_lowercase();
        let folders = cx.store.all_folders();
        // Root-level connections first, then folders.
        let mut root: Vec<ConnectionProfile> = cx.store.ws.profiles.iter().filter(|p| p.folder.trim_matches('/').is_empty()).cloned().collect();
        root.sort_by_key(|p| p.name.to_lowercase());
        for p in root {
            connection_row(ui, cx, &p, 0.0, &filter);
        }
        for f in folders {
            let depth = f.matches('/').count() as f32;
            let parent_collapsed = cx.sidebar.collapsed_folders.iter().any(|c| f.starts_with(&format!("{c}/")));
            if parent_collapsed {
                continue;
            }
            let mut profiles: Vec<ConnectionProfile> = cx.store.ws.profiles.iter().filter(|p| p.folder.trim_matches('/') == f).cloned().collect();
            profiles.sort_by_key(|p| p.name.to_lowercase());
            let matches_filter = filter.is_empty() || f.to_lowercase().contains(&filter) || cx.store.ws.profiles.iter().any(|p| p.folder.starts_with(&f) && p.name.to_lowercase().contains(&filter));
            if !matches_filter {
                continue;
            }
            let collapsed = cx.sidebar.collapsed_folders.contains(&f);
            let name = f.rsplit('/').next().unwrap_or(&f).to_string();
            let resp = row(ui, depth * 14.0, Some(!collapsed), if collapsed { ICON_FOLDER } else { ICON_FOLDER_OPEN }, t.muted_fg, &name, "", false);
            if resp.clicked() {
                if collapsed {
                    cx.sidebar.collapsed_folders.remove(&f);
                } else {
                    cx.sidebar.collapsed_folders.insert(f.clone());
                }
            }
            resp.context_menu(|ui| {
                if ui.button(ico(ICON_ADD, "New connection here")).clicked() {
                    cx.cmd(Cmd::NewConnectionIn(f.clone()));
                    ui.close();
                }
                if ui.button(ico(ICON_EDIT, "Rename folder…")).clicked() {
                    let leaf = f.rsplit('/').next().unwrap_or(&f).to_string();
                    cx.dialog = Some(Dialog::RenameFolder { from: f.clone(), name: leaf });
                    ui.close();
                }
                if ui.button(ico(ICON_CREATE_NEW_FOLDER, "New subfolder")).clicked() {
                    cx.dialog = Some(Dialog::new_folder_in(&f));
                    ui.close();
                }
                let empty = !cx.store.ws.profiles.iter().any(|p| p.folder.starts_with(&f));
                if ui.add_enabled(empty, egui::Button::new(ico(ICON_DELETE, "Remove empty folder"))).on_disabled_hover_text("Move or delete its connections first").clicked() {
                    cx.store.ws.folders.retain(|x| !x.starts_with(&f));
                    cx.mark_dirty();
                    ui.close();
                }
            });
            if !collapsed {
                for p in profiles {
                    connection_row(ui, cx, &p, (depth + 1.0) * 14.0, &filter);
                }
            }
        }
    });
}

/// One tree row. `expanded` = Some(state) draws a chevron.
#[allow(clippy::too_many_arguments)]
fn row(ui: &mut egui::Ui, indent: f32, expanded: Option<bool>, icon: egui_material_icons::MaterialIcon, icon_color: egui::Color32, label: &str, detail: &str, selected: bool) -> egui::Response {
    row_with_trailing(ui, indent, expanded, icon, icon_color, None, label, detail, selected, 0.0)
}

/// `trailing` reserves space at the right edge (status dot, PROD marker).
#[allow(clippy::too_many_arguments)]
fn row_with_trailing(
    ui: &mut egui::Ui,
    indent: f32,
    expanded: Option<bool>,
    icon: egui_material_icons::MaterialIcon,
    icon_color: egui::Color32,
    driver: Option<&str>,
    label: &str,
    detail: &str,
    selected: bool,
    trailing: f32,
) -> egui::Response {
    let t = theme::tokens(ui);
    let h = 24.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if selected {
        ui.painter().rect_filled(rect, 4.0, t.selection);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, t.muted);
    }
    if resp.has_focus() {
        ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(1.5, t.ring), egui::StrokeKind::Inside);
    }
    let mut x = rect.left() + 4.0 + indent;
    let cy = rect.center().y;
    let font = egui::FontId::proportional(13.0);
    if let Some(open) = expanded {
        let g = if open { ICON_EXPAND_MORE } else { ICON_CHEVRON_RIGHT };
        ui.painter().text(egui::pos2(x + 7.0, cy), egui::Align2::CENTER_CENTER, String::from(g), egui::FontId::proportional(theme::icon::MD), t.muted_fg);
    }
    x += 16.0;
    match driver {
        Some(d) if theme::is_cartoon() => theme::paint_connector_sticker(ui.painter(), egui::pos2(x + 8.0, cy), 19.0, d, ui.visuals().dark_mode),
        _ if theme::is_cartoon() => theme::paint_cartoon_icon(ui.painter(), egui::pos2(x + 8.0, cy), 18.0, icon, ui.visuals().dark_mode),
        _ => {
            ui.painter().text(egui::pos2(x + 8.0, cy), egui::Align2::CENTER_CENTER, String::from(icon), egui::FontId::proportional(theme::icon::SM + 1.0), icon_color);
        }
    }
    x += 22.0;
    let right = rect.right() - 6.0 - trailing;
    let max_w = right - x;
    let galley = elided(ui, label, font.clone(), t.fg, max_w);
    let lw = galley.size().x;
    ui.painter().galley(egui::pos2(x, cy - galley.size().y / 2.0), galley, t.fg);
    if !detail.is_empty() && lw + 40.0 < max_w {
        let dx = x + lw + 8.0;
        let g = elided(ui, detail, egui::FontId::proportional(12.0), t.muted_fg, right - dx);
        ui.painter().galley(egui::pos2(dx, cy - g.size().y / 2.0), g, t.muted_fg);
    }
    resp.on_hover_text(if detail.is_empty() { label.to_string() } else { format!("{label}\n{detail}") })
}

/// One line of text cut to `max_w` with a trailing "…", so clipped text is
/// always visibly shortened rather than sliced mid-glyph.
pub(crate) fn elided(ui: &egui::Ui, text: &str, font: egui::FontId, color: egui::Color32, max_w: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping { max_width: max_w.max(0.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    ui.painter().layout_job(job)
}

fn connection_row(ui: &mut egui::Ui, cx: &mut AppCtx, p: &ConnectionProfile, indent: f32, filter: &str) {
    let t = theme::tokens(ui);
    let id = p.id;
    let has_match = filter.is_empty()
        || p.name.to_lowercase().contains(filter)
        || p.summary().to_lowercase().contains(filter)
        || cx.sidebar.children.iter().any(|((c, _), ch)| *c == id && matches!(ch, Children::Ready(v) if v.iter().any(|n| n.label.to_lowercase().contains(filter))));
    if !has_match {
        return;
    }
    let state = cx.conns.state(id);
    let (status_color, status_text) = match state {
        Some(ConnState::Connected(l)) => (if l.degraded { t.warning } else { t.accent }, "connected"),
        Some(ConnState::Connecting { .. }) => (t.info, "connecting"),
        Some(ConnState::Failed(_)) => (t.destructive, "failed"),
        None => (t.muted_fg.gamma_multiply(0.6), "disconnected"),
    };
    let connected = matches!(state, Some(ConnState::Connected(_)));
    let open = connected && cx.sidebar.is_expanded(id, ROOT);
    let selected = cx.selected_conn == Some(id);
    let trailing = if p.environment.is_protected() { 50.0 } else { 18.0 };
    let resp =
        row_with_trailing(ui, indent, connected.then_some(open), crate::views::driver_icon(&p.driver), theme::env_color(ui, p.environment), Some(&p.driver), &p.name, &p.summary(), selected, trailing);
    // Status dot and environment marker drawn over the row's right edge.
    let r = resp.rect;
    ui.painter().circle_filled(egui::pos2(r.right() - 10.0, r.center().y), 4.0, status_color);
    if p.environment.is_protected() {
        ui.painter().text(egui::pos2(r.right() - 20.0, r.center().y), egui::Align2::RIGHT_CENTER, "PROD", egui::FontId::proportional(10.0), t.destructive);
    }
    let resp = resp.on_hover_text(format!("{} · {}", p.environment.label(), status_text));
    if resp.clicked() {
        cx.selected_conn = Some(id);
        if connected {
            cx.sidebar.toggle(id, ROOT);
        }
    }
    if resp.double_clicked() {
        if connected {
            // Both clicks of the double-click toggled the tree, so it is back as it was.
            crate::views::open_connection(cx, p);
        } else {
            cx.cmd(Cmd::Connect(id));
        }
    }
    resp.context_menu(|ui| connection_menu(ui, cx, p));
    if let Some(ConnState::Failed(e)) = cx.conns.state(id) {
        let msg = e.message.clone();
        ui.horizontal(|ui| {
            ui.add_space(indent + 42.0);
            // Truncated to the panel width: a long message must not widen the sidebar.
            ui.add(egui::Label::new(RichText::new(&msg).small().color(t.destructive)).truncate()).on_hover_text(format!("{msg}\n\nDouble-click the connection to retry"));
        });
    }
    if let Some(ConnState::Connecting { started, .. }) = cx.conns.state(id) {
        let secs = started.elapsed().as_secs();
        ui.horizontal(|ui| {
            ui.add_space(indent + 42.0);
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new(format!("connecting… {secs}s")).small().color(t.muted_fg));
            if ui.small_button("Cancel").clicked() {
                cx.conns.cancel_connect(id);
            }
        });
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
    if open {
        let handle = cx.conns.session(id).cloned();
        match handle {
            Some(SessionHandle::Sql(_)) => sql_children(ui, cx, id, None, indent + 14.0, filter),
            Some(SessionHandle::KeyValue(_)) => kv_children(ui, cx, id, indent + 14.0),
            Some(SessionHandle::Resource(_)) => resource_children(ui, cx, id, None, indent + 14.0, filter),
            Some(SessionHandle::Containers(_)) => {
                for s in Section::ALL {
                    if row(ui, indent + 14.0, None, s.icon(), t.muted_fg, s.label(), "", false).clicked() {
                        cx.cmd(Cmd::OpenDocker { conn: id, section: s });
                    }
                }
            }
            None => {}
        }
    }
}

fn connection_menu(ui: &mut egui::Ui, cx: &mut AppCtx, p: &ConnectionProfile) {
    let id = p.id;
    let fav = cx.store.is_favorite(id);
    if ui.button(ico(ICON_STAR, if fav { "Remove from favourites" } else { "Add to favourites" })).clicked() {
        cx.store.toggle_favorite(id);
        cx.mark_dirty();
        ui.close();
    }
    let session = cx.conns.session(id).cloned();
    if session.is_some() {
        if let Some(SessionHandle::Sql(s)) = &session {
            if ui.button(ico(ICON_CODE, &theme::kbd("New SQL editor  ⌘T"))).clicked() {
                cx.cmd(Cmd::NewSql { conn: id, database: None, text: String::new() });
                ui.close();
            }
            let caps = s.capabilities();
            if (caps.metrics || caps.activity) && ui.button(ico(ICON_MONITORING, "Monitor")).clicked() {
                cx.cmd(Cmd::OpenMonitor(id));
                ui.close();
            }
            if caps.users && ui.button(ico(ICON_GROUP, "Users & privileges")).clicked() {
                cx.cmd(Cmd::OpenUsers(id));
                ui.close();
            }
            if caps.create_database && ui.button(ico(ICON_ADD, "New database…")).clicked() {
                cx.dialog = Some(Dialog::create_database(id));
                ui.close();
            }
        }
        if let Some(SessionHandle::KeyValue(_)) = &session
            && ui.button(ico(ICON_KEY, "Browse keys")).clicked()
        {
            cx.cmd(Cmd::OpenRedis { conn: id, db: p.param("db").parse().unwrap_or(0) });
            ui.close();
        }
        if let Some(SessionHandle::Resource(r)) = &session {
            if r.supports_terminal() && ui.button(ico(ICON_TERMINAL, "Open terminal")).clicked() {
                cx.cmd(Cmd::OpenTerminal(id));
                ui.close();
            }
            if ui.button(ico(ICON_DNS, "Open")).clicked() {
                cx.cmd(Cmd::OpenResource { conn: id, node: None, action: None });
                ui.close();
            }
            for a in r.actions("") {
                if ui.button(a.label).clicked() {
                    cx.cmd(Cmd::OpenResource { conn: id, node: None, action: Some(a.id) });
                    ui.close();
                }
            }
        }
        if let Some(SessionHandle::Containers(_)) = &session
            && ui.button(ico(ICON_DEPLOYED_CODE, "Containers")).clicked()
        {
            cx.cmd(Cmd::OpenDocker { conn: id, section: Section::Containers });
            ui.close();
        }
        if ui.button(ico(ICON_REFRESH, "Refresh")).clicked() {
            cx.cmd(Cmd::RefreshTree(id));
            cx.cmd(Cmd::InvalidateCatalog(id));
            ui.close();
        }
        if ui.button(ico(ICON_LINK_OFF, "Disconnect")).clicked() {
            cx.cmd(Cmd::Disconnect(id));
            ui.close();
        }
    } else if ui.button(ico(ICON_LINK, "Connect")).clicked() {
        cx.cmd(Cmd::Connect(id));
        ui.close();
    }
    ui.separator();
    if ui.button(ico(ICON_EDIT, "Edit connection…")).clicked() {
        cx.cmd(Cmd::EditConnection(Some(id)));
        ui.close();
    }
    if ui.button(ico(ICON_DRIVE_FILE_MOVE, "Move to folder…")).clicked() {
        cx.dialog = Some(Dialog::MoveToFolder { conn: id, folder: p.folder.clone() });
        ui.close();
    }
    if ui.button(ico(ICON_CONTENT_COPY, "Duplicate")).clicked() {
        cx.cmd(Cmd::DuplicateConnection(id));
        ui.close();
    }
    if ui.button(ico(ICON_DELETE, "Delete…")).clicked() {
        let name = p.name.clone();
        cx.confirm(None, format!("Delete connection “{name}”?"), "The saved profile and its keychain password are removed. Databases are not touched.", "Delete", None, move |cx| {
            cx.cmd(Cmd::DeleteConnection(id));
        });
        ui.close();
    }
}

fn kv_children(ui: &mut egui::Ui, cx: &mut AppCtx, id: Uuid, indent: f32) {
    let t = theme::tokens(ui);
    if !cx.sidebar.kv_dbs.contains_key(&id)
        && let Some(s) = cx.conns.session(id).and_then(|h| h.kv()).cloned()
    {
        cx.sidebar.kv_dbs.insert(id, KvChildren::Loading(cx.bg.spawn("", async move { s.databases().await })));
    }
    if let Some(KvChildren::Loading(job)) = cx.sidebar.kv_dbs.get_mut(&id)
        && let Some(r) = job.take()
    {
        cx.sidebar.kv_dbs.insert(
            id,
            match r {
                Ok(v) => KvChildren::Ready(v),
                Err(e) => KvChildren::Failed(e.message),
            },
        );
    }
    match cx.sidebar.kv_dbs.get(&id) {
        Some(KvChildren::Ready(dbs)) => {
            let dbs: Vec<KvDatabase> = dbs.iter().filter(|d| d.keys > 0).cloned().collect();
            let any = !dbs.is_empty();
            for d in dbs {
                if row(ui, indent, None, ICON_DATABASE, t.muted_fg, &format!("db{}", d.index), &format!("{} keys", d.keys), false).clicked() {
                    cx.cmd(Cmd::OpenRedis { conn: id, db: d.index });
                }
            }
            if !any {
                ui.horizontal(|ui| {
                    ui.add_space(indent + 38.0);
                    ui.label(RichText::new("All databases are empty").small().color(t.muted_fg));
                });
            }
            if row(ui, indent, None, ICON_TERMINAL, t.muted_fg, "Console & server info", "", false).clicked() {
                cx.cmd(Cmd::OpenRedis { conn: id, db: 0 });
            }
        }
        Some(KvChildren::Failed(e)) => {
            let e = e.clone();
            ui.add(egui::Label::new(RichText::new(e.clone()).small().color(t.destructive)).truncate()).on_hover_text(e.clone());
        }
        _ => {
            ui.horizontal(|ui| {
                ui.add_space(indent + 20.0);
                crate::views::busy(ui, "Loading…");
            });
        }
    }
}

fn sql_children(ui: &mut egui::Ui, cx: &mut AppCtx, id: Uuid, parent: Option<(ObjectKind, ObjPath)>, indent: f32, filter: &str) {
    let t = theme::tokens(ui);
    let key = parent.as_ref().map(|(k, p)| node_key(*k, p)).unwrap_or_else(|| ROOT.into());
    let ck = (id, key.clone());
    if !cx.sidebar.children.contains_key(&ck) {
        let Some(s) = cx.conns.session(id).and_then(|h| h.sql()).cloned() else { return };
        let p = parent.clone();
        cx.sidebar.children.insert(ck.clone(), Children::Loading(cx.bg.spawn("", async move { s.children(p).await })));
    }
    if let Some(Children::Loading(job)) = cx.sidebar.children.get_mut(&ck) {
        if let Some(r) = job.take() {
            let v = match r {
                Ok(v) => Children::Ready(v),
                Err(e) => Children::Failed(e.message),
            };
            cx.sidebar.children.insert(ck.clone(), v);
        } else if job.is_dead() {
            cx.sidebar.children.remove(&ck);
            return;
        }
    }
    let nodes = match cx.sidebar.children.get(&ck) {
        Some(Children::Ready(v)) => v.clone(),
        Some(Children::Failed(e)) => {
            let e = e.clone();
            ui.horizontal(|ui| {
                ui.add_space(indent + 20.0);
                ui.add(egui::Label::new(RichText::new(e.clone()).small().color(t.destructive)).truncate()).on_hover_text(e.clone());
                if ui.small_button("Retry").clicked() {
                    cx.sidebar.children.remove(&ck);
                }
            });
            return;
        }
        _ => {
            ui.horizontal(|ui| {
                ui.add_space(indent + 20.0);
                crate::views::busy(ui, "Loading…");
            });
            return;
        }
    };
    if nodes.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(indent + 38.0);
            ui.label(RichText::new("Empty").small().color(t.muted_fg));
        });
    }
    for n in nodes {
        let nk = node_key(n.kind, &n.path);
        let expanded = n.expandable && cx.sidebar.is_expanded(id, &nk);
        let is_folder = matches!(n.kind, ObjectKind::Folder(_));
        if !filter.is_empty() && !is_folder && !expanded && !n.label.to_lowercase().contains(filter) && !matches!(n.kind, ObjectKind::Database | ObjectKind::Schema) {
            continue;
        }
        let color = match n.kind {
            ObjectKind::Table | ObjectKind::Database => t.info,
            ObjectKind::View | ObjectKind::MaterializedView => t.accent,
            ObjectKind::Function | ObjectKind::Procedure => t.warning,
            ObjectKind::Constraint | ObjectKind::Index => t.warning,
            _ => t.muted_fg,
        };
        let resp = row(ui, indent, n.expandable.then_some(expanded), kind_icon(n.kind), color, &n.label, n.detail.as_deref().unwrap_or(""), false);
        if resp.clicked() && n.expandable {
            cx.sidebar.toggle(id, &nk);
        }
        if resp.clicked() {
            match n.kind {
                ObjectKind::Folder(FolderKind::Security) => cx.cmd(Cmd::OpenUsers(id)),
                ObjectKind::Folder(FolderKind::ActivityMonitor) => cx.cmd(Cmd::OpenMonitor(id)),
                _ => {}
            }
        }
        if resp.double_clicked() {
            if n.kind.has_rows() {
                cx.cmd(Cmd::OpenTable { conn: id, kind: n.kind, path: n.path.clone() });
            } else if matches!(n.kind, ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Trigger | ObjectKind::Sequence | ObjectKind::Event | ObjectKind::Index | ObjectKind::Constraint) {
                cx.cmd(Cmd::OpenObject { conn: id, kind: n.kind, path: n.path.clone() });
            }
        }
        let node = n.clone();
        resp.context_menu(|ui| object_menu(ui, cx, id, &node));
        if expanded {
            sql_children(ui, cx, id, Some((n.kind, n.path.clone())), indent + 14.0, filter);
        }
    }
}

fn object_menu(ui: &mut egui::Ui, cx: &mut AppCtx, id: Uuid, n: &TreeNode) {
    let Some(s) = cx.conns.session(id).and_then(|h| h.sql()).cloned() else { return };
    let caps = s.capabilities();
    let d = s.dialect();
    let qualified = d.qualified(n.path.schema.as_deref().or(if d == openvclient_core::sqltext::Dialect::MySql { n.path.database.as_deref() } else { None }), n.path.name.as_deref().unwrap_or(""));
    if n.kind.has_rows() {
        if ui.button(ico(ICON_TABLE_ROWS, "Open data")).clicked() {
            cx.cmd(Cmd::OpenTable { conn: id, kind: n.kind, path: n.path.clone() });
            ui.close();
        }
        if ui.button(ico(ICON_CODE, "Query in new editor")).clicked() {
            cx.cmd(Cmd::NewSql { conn: id, database: n.path.database.clone(), text: format!("SELECT *\nFROM {qualified}\nLIMIT 100;\n") });
            ui.close();
        }
    }
    if !matches!(n.kind, ObjectKind::Folder(_) | ObjectKind::Column) && ui.button(ico(ICON_DATA_OBJECT, "Structure & DDL")).clicked() {
        cx.cmd(Cmd::OpenObject { conn: id, kind: n.kind, path: n.path.clone() });
        ui.close();
    }
    if ui.button(ico(ICON_CONTENT_COPY, "Copy name")).clicked() {
        ui.ctx().copy_text(if n.kind.has_rows() { qualified.clone() } else { n.label.clone() });
        ui.close();
    }
    match n.kind {
        ObjectKind::Database => {
            if d == openvclient_core::sqltext::Dialect::MySql {
                if ui.button(ico(ICON_SCHEMA, "ER diagram")).clicked() {
                    cx.cmd(Cmd::OpenErd { conn: id, database: n.path.database.clone(), schema: n.label.clone() });
                    ui.close();
                }
                if ui.button(ico(ICON_COMPARE_ARROWS, "Compare with…")).clicked() {
                    cx.cmd(Cmd::OpenCompare { conn: id, database: n.path.database.clone(), schema: n.label.clone() });
                    ui.close();
                }
            }
            if ui.button(ico(ICON_CODE, "New SQL editor")).clicked() {
                cx.cmd(Cmd::NewSql { conn: id, database: n.path.database.clone(), text: String::new() });
                ui.close();
            }
            if caps.create_schema && ui.button(ico(ICON_ADD, "New schema…")).clicked() {
                cx.dialog = Some(Dialog::create_schema(id, n.path.database.clone()));
                ui.close();
            }
            if caps.database_offline {
                let offline = n.detail.as_deref().unwrap_or("").contains("offline");
                let (label, online) = if offline { ("Bring online", true) } else { ("Take offline…", false) };
                if ui.button(ico(ICON_POWER_SETTINGS_NEW, label)).clicked() {
                    let (s, db) = (s.clone(), n.label.clone());
                    if online {
                        crate::views::dialogs::track(cx, format!("ALTER DATABASE {db} SET ONLINE"), "Database is online", Some(id), async move { s.set_database_online(&db, true).await });
                    } else {
                        cx.confirm(
                            Some(id),
                            format!("Take {db} offline?"),
                            "Open transactions are rolled back and every session is disconnected. Nobody can use the database until it is brought online.",
                            "Take offline",
                            Some(db.clone()),
                            move |cx| {
                                crate::views::dialogs::track(cx, format!("ALTER DATABASE {db} SET OFFLINE"), "Database is offline", Some(id), async move { s.set_database_online(&db, false).await });
                            },
                        );
                    }
                    ui.close();
                }
            }
            if caps.backup && ui.button(ico(ICON_BACKUP, "Back up…")).clicked() {
                if caps.server_side_backup {
                    cx.dialog = Some(Dialog::server_backup(id, n.label.clone(), false));
                } else {
                    crate::views::dialogs::start_backup(cx, id, n.path.database.clone(), &n.label);
                }
                ui.close();
            }
            if caps.restore && ui.button(ico(ICON_RESTORE, "Restore from file…")).clicked() {
                if caps.server_side_backup {
                    cx.dialog = Some(Dialog::server_backup(id, n.label.clone(), true));
                } else {
                    crate::views::dialogs::start_restore(cx, id, n.path.database.clone(), &n.label);
                }
                ui.close();
            }
            if caps.database_connection_toggle {
                ui.menu_button(ico(ICON_POWER_SETTINGS_NEW, "Connections"), |ui| {
                    for (allow, label) in [(true, "Allow new connections"), (false, "Block new connections")] {
                        if ui.button(label).clicked() {
                            let db = n.label.clone();
                            let s = s.clone();
                            crate::views::dialogs::track(cx, format!("ALTER DATABASE {db}"), if allow { "New connections allowed" } else { "New connections blocked" }, Some(id), async move {
                                s.set_database_allow_connections(&db, allow).await
                            });
                            ui.close();
                        }
                    }
                });
            }
        }
        ObjectKind::Schema => {
            if ui.button(ico(ICON_SCHEMA, "ER diagram")).clicked() {
                cx.cmd(Cmd::OpenErd { conn: id, database: n.path.database.clone(), schema: n.label.clone() });
                ui.close();
            }
            if ui.button(ico(ICON_COMPARE_ARROWS, "Compare with…")).clicked() {
                cx.cmd(Cmd::OpenCompare { conn: id, database: n.path.database.clone(), schema: n.label.clone() });
                ui.close();
            }
            if ui.button(ico(ICON_CODE, "New SQL editor")).clicked() {
                let text = if d == openvclient_core::sqltext::Dialect::Postgres { format!("SET search_path TO {};\n\n", d.quote_ident(&n.label)) } else { String::new() };
                cx.cmd(Cmd::NewSql { conn: id, database: n.path.database.clone(), text });
                ui.close();
            }
            if caps.backup && n.path.schema.as_deref() == Some("main") && ui.button(ico(ICON_BACKUP, "Back up…")).clicked() {
                crate::views::dialogs::start_backup(cx, id, None, &n.label);
                ui.close();
            }
            if caps.restore && n.path.schema.as_deref() == Some("main") && ui.button(ico(ICON_RESTORE, "Restore from file…")).clicked() {
                crate::views::dialogs::start_restore(cx, id, None, &n.label);
                ui.close();
            }
        }
        ObjectKind::Folder(f) => {
            if let Some(tpl) = template(d, f, &n.path)
                && ui.button(ico(ICON_ADD, &format!("New {}…", f.label().trim_end_matches('s').to_lowercase()))).clicked()
            {
                cx.cmd(Cmd::NewSql { conn: id, database: n.path.database.clone(), text: tpl });
                ui.close();
            }
        }
        _ => {}
    }
    if n.expandable && ui.button(ico(ICON_REFRESH, "Refresh")).clicked() {
        cx.sidebar.refresh_node(id, n.kind, &n.path);
        ui.close();
    }
    // Destructive actions last, behind confirmation.
    let droppable = match n.kind {
        ObjectKind::Database => caps.drop_database,
        ObjectKind::Folder(_) => false,
        _ => caps.edit_data || caps.drop_database,
    };
    if n.kind == ObjectKind::Table && caps.truncate {
        ui.separator();
        if ui.button(RichText::new(ico(ICON_CLEAR_ALL, "Truncate…")).color(theme::tokens(ui).destructive)).clicked() {
            let s = s.clone();
            let path = n.path.clone();
            let label = n.label.clone();
            cx.confirm(Some(id), format!("Truncate {label}?"), "Every row is removed immediately. This cannot be undone.", "Truncate", Some(label.clone()), move |cx| {
                crate::views::dialogs::track(cx, format!("TRUNCATE {label}"), "Table truncated", Some(id), async move { s.truncate(&path).await });
            });
            ui.close();
        }
    }
    if droppable && n.kind.sql_keyword().is_some() || matches!(n.kind, ObjectKind::Column | ObjectKind::Constraint) && caps.edit_data {
        if n.kind != ObjectKind::Table {
            ui.separator();
        }
        if ui.button(RichText::new(ico(ICON_DELETE, &format!("Drop {}…", n.kind.label().to_lowercase()))).color(theme::tokens(ui).destructive)).clicked() {
            let s = s.clone();
            let kind = n.kind;
            let path = n.path.clone();
            let label = n.label.clone();
            let cascade_ok = d == openvclient_core::sqltext::Dialect::Postgres;
            let body = if cascade_ok { "Dependent objects block the drop (no CASCADE). This cannot be undone." } else { "This cannot be undone." };
            cx.confirm(Some(id), format!("Drop {} {label}?", kind.label().to_lowercase()), body, "Drop", Some(label.clone()), move |cx| {
                crate::views::dialogs::track(cx, format!("DROP {label}"), "Dropped", Some(id), async move { s.drop_object(kind, &path, false).await });
            });
            ui.close();
        }
    }
}

/// Engine-specific CREATE templates opened in a SQL editor.
fn template(d: openvclient_core::sqltext::Dialect, f: FolderKind, p: &ObjPath) -> Option<String> {
    use openvclient_core::sqltext::Dialect::*;
    let q = |name: &str| d.qualified(if d == MySql { p.database.as_deref() } else { p.schema.as_deref() }, name);
    Some(match (d, f) {
        (Postgres, FolderKind::Tables) => {
            format!("CREATE TABLE {} (\n    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,\n    name text NOT NULL,\n    created_at timestamptz NOT NULL DEFAULT now()\n);\n", q("new_table"))
        }
        (MySql, FolderKind::Tables) => format!(
            "CREATE TABLE {} (\n    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,\n    name VARCHAR(255) NOT NULL,\n    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;\n",
            q("new_table")
        ),
        (Sqlite, FolderKind::Tables) => {
            format!("CREATE TABLE {} (\n    id INTEGER PRIMARY KEY,\n    name TEXT NOT NULL,\n    created_at TEXT NOT NULL DEFAULT (datetime('now'))\n) STRICT;\n", q("new_table"))
        }
        (_, FolderKind::Views) => format!("CREATE VIEW {} AS\nSELECT 1 AS example;\n", q("new_view")),
        (Postgres, FolderKind::MaterializedViews) => format!("CREATE MATERIALIZED VIEW {} AS\nSELECT 1 AS example\nWITH DATA;\n", q("new_view")),
        (Postgres, FolderKind::Functions) => {
            format!("CREATE OR REPLACE FUNCTION {}(a integer, b integer)\nRETURNS integer\nLANGUAGE sql\nIMMUTABLE\nAS $$\n    SELECT a + b\n$$;\n", q("new_function"))
        }
        (Postgres, FolderKind::Procedures) => format!("CREATE OR REPLACE PROCEDURE {}()\nLANGUAGE plpgsql\nAS $$\nBEGIN\n    -- body\nEND;\n$$;\n", q("new_procedure")),
        (Postgres, FolderKind::Sequences) => format!("CREATE SEQUENCE {} START 1;\n", q("new_sequence")),
        (MySql, FolderKind::Procedures) => format!("DELIMITER //\nCREATE PROCEDURE {}()\nBEGIN\n    SELECT 1;\nEND //\nDELIMITER ;\n", q("new_procedure")),
        (MySql, FolderKind::Functions) => format!("DELIMITER //\nCREATE FUNCTION {}(a INT, b INT) RETURNS INT DETERMINISTIC\nBEGIN\n    RETURN a + b;\nEND //\nDELIMITER ;\n", q("new_function")),
        (MySql, FolderKind::Events) => format!("CREATE EVENT {}\nON SCHEDULE EVERY 1 DAY\nDO\n    DELETE FROM some_table WHERE created_at < NOW() - INTERVAL 30 DAY;\n", q("new_event")),
        (Sqlite, FolderKind::Triggers) => "CREATE TRIGGER new_trigger AFTER INSERT ON some_table\nBEGIN\n    SELECT 1;\nEND;\n".to_string(),
        (MySql, FolderKind::Triggers) => format!("CREATE TRIGGER {}\nBEFORE INSERT ON some_table\nFOR EACH ROW\nSET NEW.created_at = NOW();\n", q("new_trigger")),
        (_, FolderKind::Indexes) => format!("CREATE INDEX new_index ON {} (column_name);\n", q(p.name.as_deref().unwrap_or("table_name"))),
        _ => return None,
    })
}

fn resource_children(ui: &mut egui::Ui, cx: &mut AppCtx, id: Uuid, parent: Option<String>, indent: f32, filter: &str) {
    use openvclient_core::resource::Node;
    let t = theme::tokens(ui);
    let key = (id, parent.clone().unwrap_or_default());
    if !cx.sidebar.res.contains_key(&key) {
        let Some(s) = cx.conns.session(id).and_then(|h| h.resource()).cloned() else { return };
        let p = parent.clone();
        cx.sidebar.res.insert(key.clone(), ResChildren::Loading(cx.bg.spawn("", async move { s.children(p.as_deref()).await })));
    }
    if let Some(ResChildren::Loading(job)) = cx.sidebar.res.get_mut(&key)
        && let Some(r) = job.take()
    {
        let v = match r {
            Ok(v) => ResChildren::Ready(v),
            Err(e) => ResChildren::Failed(e.message),
        };
        cx.sidebar.res.insert(key.clone(), v);
    }
    let nodes: Vec<Node> = match cx.sidebar.res.get(&key) {
        Some(ResChildren::Ready(v)) => v.clone(),
        Some(ResChildren::Failed(e)) => {
            let e = e.clone();
            ui.horizontal(|ui| {
                ui.add_space(indent + 20.0);
                ui.add(egui::Label::new(RichText::new(e.clone()).small().color(t.destructive)).truncate()).on_hover_text(e.clone());
                if ui.small_button("Retry").clicked() {
                    cx.sidebar.res.remove(&key);
                }
            });
            return;
        }
        _ => {
            ui.horizontal(|ui| {
                ui.add_space(indent + 20.0);
                crate::views::busy(ui, "Loading…");
            });
            return;
        }
    };
    if nodes.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(indent + 38.0);
            ui.label(RichText::new("Empty").small().color(t.muted_fg));
        });
    }
    let s = cx.conns.session(id).and_then(|h| h.resource()).cloned();
    // Storage: the sidebar shows folders; files are browsed in the file manager.
    let folders_only = s.as_ref().is_some_and(|s| s.file_browser());
    for n in nodes {
        if folders_only && !n.expandable && n.kind != "terminal" {
            continue;
        }
        let nk = format!("res|{}", n.id);
        let expanded = n.expandable && cx.sidebar.is_expanded(id, &nk);
        if !filter.is_empty() && !expanded && !n.expandable && !n.label.to_lowercase().contains(filter) {
            continue;
        }
        let resp = row(ui, indent, n.expandable.then_some(expanded), crate::views::resource::node_icon(n.icon), t.info, &n.label, n.detail.as_deref().unwrap_or(""), false);
        if resp.clicked() {
            if n.expandable {
                cx.sidebar.toggle(id, &nk);
            }
            if n.kind == "terminal" {
                cx.cmd(Cmd::OpenTerminal(id));
            } else if n.openable {
                cx.cmd(Cmd::OpenResource { conn: id, node: Some(n.clone()), action: None });
            }
        }
        if let Some(s) = &s {
            let actions = s.actions(&n.kind);
            let node = n.clone();
            resp.context_menu(|ui| {
                if node.openable && ui.button(ico(ICON_VISIBILITY, "Open")).clicked() {
                    cx.cmd(Cmd::OpenResource { conn: id, node: Some(node.clone()), action: None });
                    ui.close();
                }
                for a in &actions {
                    let label = if a.destructive { RichText::new(a.label).color(t.destructive) } else { RichText::new(a.label) };
                    if ui.button(label).clicked() {
                        cx.cmd(Cmd::OpenResource { conn: id, node: Some(node.clone()), action: Some(a.id) });
                        ui.close();
                    }
                }
                if node.expandable && ui.button(ico(ICON_REFRESH, "Refresh")).clicked() {
                    cx.sidebar.refresh_resource(id, &node.id);
                    ui.close();
                }
                if ui.button(ico(ICON_CONTENT_COPY, "Copy name")).clicked() {
                    ui.ctx().copy_text(node.label.clone());
                    ui.close();
                }
            });
        }
        if expanded {
            resource_children(ui, cx, id, Some(n.id.clone()), indent + 14.0, filter);
        }
    }
}
