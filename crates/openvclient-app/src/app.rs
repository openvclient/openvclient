//! Application shell: shared state, the command queue views push into, the
//! docking workspace and the global chrome (top bar, status bar, overlays).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use egui::{Key, KeyboardShortcut, Modifiers, RichText};
use egui_material_icons::icons::*;
use egui_tiles::{Behavior, Container, SimplificationOptions, TabState, Tile, TileId, Tiles, Tree, UiResponse};
use openvclient_core::sql::{Catalog, ObjPath, ObjectKind};
use openvclient_core::{DriverRegistry, Result};
use uuid::Uuid;

use crate::bg::{Bg, Job};
use crate::conns::{ConnEvent, ConnState, Connections};
use crate::store::{Store, ThemeChoice, ThemeStyle};
use crate::theme::{self, ico};
use crate::views;
use crate::widgets::toasts::Toasts;

/// Actions views request; applied after the frame's UI is drawn so views
/// never need mutable access to the dock.
pub enum Cmd {
    NewSql {
        conn: Uuid,
        database: Option<String>,
        text: String,
    },
    OpenTable {
        conn: Uuid,
        kind: ObjectKind,
        path: ObjPath,
    },
    OpenObject {
        conn: Uuid,
        kind: ObjectKind,
        path: ObjPath,
    },
    OpenMonitor(Uuid),
    OpenUsers(Uuid),
    OpenTerminal(Uuid),
    OpenRedis {
        conn: Uuid,
        db: u32,
    },
    OpenDocker {
        conn: Uuid,
        section: views::docker::Section,
    },
    OpenResource {
        conn: Uuid,
        node: Option<openvclient_core::resource::Node>,
        action: Option<&'static str>,
    },
    OpenHome,
    Connect(Uuid),
    Disconnect(Uuid),
    EditConnection(Option<Uuid>),
    NewConnectionIn(String),
    /// New connection with this connector already chosen.
    NewConnectionOf(&'static str),
    DeleteConnection(Uuid),
    DuplicateConnection(Uuid),
    RefreshTree(Uuid),
    InvalidateCatalog(Uuid),
    FocusTab(u64),
    OpenErd {
        conn: Uuid,
        database: Option<String>,
        schema: String,
    },
    OpenCompare {
        conn: Uuid,
        database: Option<String>,
        schema: String,
    },
}

/// Tab-strip requests (context menu, open-tabs list), applied after the
/// tile tree has been drawn because they need the whole tree.
#[derive(Clone, Copy)]
pub enum TabAction {
    CloseOthers(TileId),
    CloseRight(TileId),
    CloseAll(TileId),
    Focus(TileId),
    Reopen,
}

/// A confirmation guarding a destructive action. Production connections
/// require typing the object name.
pub struct Confirm {
    pub title: String,
    pub body: String,
    pub confirm_label: String,
    pub require_text: Option<String>,
    pub typed: String,
    pub action: Option<Box<dyn FnOnce(&mut AppCtx) + Send>>,
}

pub enum CatalogState {
    Loading(Job<Result<Catalog>>),
    Ready(Catalog, Instant),
    Failed,
}

/// An SSH server key waiting for the user's decision.
pub struct HostKeyPrompt {
    pub conn: Uuid,
    pub host: String,
    pub fingerprint: String,
    /// Profile parameter that pins it.
    pub param: &'static str,
}

#[derive(Default)]
pub struct UpdateState {
    job: Option<crate::bg::Job<openvclient_core::Result<Option<crate::update::Release>>>>,
    download: Option<crate::bg::Job<openvclient_core::Result<std::path::PathBuf>>>,
    pub available: Option<crate::update::Release>,
    /// The user asked (show "up to date" too).
    manual: bool,
    /// Outcome of the last check, for the About panel.
    pub last: Option<String>,
    started: bool,
    show: bool,
}

impl AppCtx {
    pub fn check_updates(&mut self, manual: bool) {
        if self.update.job.is_some() {
            return;
        }
        self.update.manual = manual;
        self.update.job = Some(self.bg.spawn(if manual { "Checking for updates" } else { "" }, crate::update::check()));
    }
}

pub struct AppCtx {
    pub bg: Bg,
    pub store: Store,
    pub conns: Connections,
    pub toasts: Toasts,
    pub cmds: Vec<Cmd>,
    pub catalogs: HashMap<(Uuid, Option<String>), CatalogState>,
    pub sidebar: views::sidebar::SidebarState,
    pub conn_dialog: Option<views::conn_dialog::ConnDialog>,
    pub dialog: Option<views::dialogs::Dialog>,
    pub confirm: Option<Confirm>,
    pub palette: views::palette::Palette,
    pub password_prompt: Option<(Uuid, String)>,
    pub host_key_prompt: Option<HostKeyPrompt>,
    pub update: UpdateState,
    pub selected_conn: Option<Uuid>,
    pub next_tab: u64,
    pub dirty: bool,
    pub last_save: Instant,
    pub show_settings: bool,
    pub show_about: bool,
    pub show_shortcuts: bool,
    /// Sidebar folded away (⌘B) to give the content the whole width.
    pub sidebar_hidden: bool,
    /// Statements confirmed for production, keyed by tab id.
    pub pending_sql: Option<(u64, Vec<String>)>,
    /// Confirmed destructive resource actions, picked up by the connection's tab.
    pub resource_jobs: HashMap<Uuid, Job<Result<openvclient_core::resource::ActionOutcome>>>,
    pub tab_actions: Vec<TabAction>,
    /// Commands that reopen recently closed tabs, newest last (⌘⇧T).
    pub closed_tabs: Vec<Cmd>,
    /// Tabs from the last session, offered on Home until restored or dismissed.
    pub restore_offer: Vec<crate::store::SavedTab>,
    /// Restoring: tabs waiting for their connection to come up.
    pub restore_pending: Vec<crate::store::SavedTab>,
}

impl AppCtx {
    pub fn cmd(&mut self, c: Cmd) {
        self.cmds.push(c);
    }

    /// Connect what the offered tabs need, then open them as connections come up.
    pub fn restore_session(&mut self) {
        let tabs = std::mem::take(&mut self.restore_offer);
        let mut conns: Vec<Uuid> = tabs.iter().map(|t| t.conn()).collect();
        conns.dedup();
        for c in conns {
            if self.conns.session(c).is_none() && !self.conns.is_connecting(c) {
                self.cmd(Cmd::Connect(c));
            }
        }
        self.restore_pending = tabs;
    }

    /// Keep a closed tab so ⌘⇧T can bring it back (last 20).
    pub fn remember_closed(&mut self, c: Cmd) {
        self.closed_tabs.push(c);
        if self.closed_tabs.len() > 20 {
            self.closed_tabs.remove(0);
        }
    }

    pub fn profile_name(&self, id: Uuid) -> String {
        self.store.profile(id).map(|p| p.name.clone()).unwrap_or_else(|| "(deleted connection)".into())
    }

    /// Production, or Staging when Settings asks for confirmation there too.
    pub fn is_production(&self, id: Uuid) -> bool {
        self.store.profile(id).is_some_and(|p| p.environment.is_protected() || (p.environment == openvclient_core::Environment::Staging && self.store.ws.settings.confirm_staging))
    }

    /// Ask before a destructive action. `name` must be typed on production.
    pub fn confirm(&mut self, conn: Option<Uuid>, title: impl Into<String>, body: impl Into<String>, label: &str, name: Option<String>, action: impl FnOnce(&mut AppCtx) + Send + 'static) {
        let require_text = match conn {
            Some(c) if self.is_production(c) && self.store.ws.settings.confirm_production => name.or_else(|| Some(self.profile_name(c))),
            _ => None,
        };
        self.confirm = Some(Confirm { title: title.into(), body: body.into(), confirm_label: label.into(), require_text, typed: String::new(), action: Some(Box::new(action)) });
    }

    /// Autocomplete catalog for a connection/database, loaded lazily.
    pub fn catalog(&mut self, conn: Uuid, database: Option<String>) -> Option<&Catalog> {
        let key = (conn, database.clone());
        let stale = match self.catalogs.get_mut(&key) {
            None => true,
            Some(CatalogState::Ready(_, at)) => at.elapsed() > Duration::from_secs(600),
            Some(CatalogState::Loading(job)) => {
                if let Some(r) = job.take() {
                    let st = match r {
                        Ok(c) => CatalogState::Ready(c, Instant::now()),
                        Err(_) => CatalogState::Failed,
                    };
                    self.catalogs.insert(key.clone(), st);
                }
                false
            }
            Some(CatalogState::Failed) => false,
        };
        if stale && let Some(s) = self.conns.session(conn).and_then(|h| h.sql()).cloned() {
            let db = database.clone();
            let job = self.bg.spawn("", async move { s.catalog(db.as_deref()).await });
            self.catalogs.insert(key.clone(), CatalogState::Loading(job));
        }
        match self.catalogs.get(&key) {
            Some(CatalogState::Ready(c, _)) => Some(c),
            _ => None,
        }
    }

    pub fn tab_id(&mut self) -> u64 {
        self.next_tab += 1;
        self.next_tab
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }
}

pub enum TabKind {
    Home,
    Sql(Box<views::sql::SqlTab>),
    Table(Box<views::table::TableTab>),
    Object(Box<views::object::ObjectTab>),
    Monitor(Box<views::monitor::MonitorTab>),
    Users(Box<views::users::UsersTab>),
    Redis(Box<views::redis::RedisTab>),
    Docker(Box<views::docker::DockerTab>),
    Resource(Box<views::resource::ResourceTab>),
    Terminal(Box<views::terminal::TerminalTab>),
    Files(Box<views::files::FilesTab>),
    Erd(Box<views::erd::ErdTab>),
    Compare(Box<views::compare::CompareTab>),
}

pub struct Tab {
    pub id: u64,
    pub kind: TabKind,
}

impl Tab {
    /// What to save for reopening after a restart.
    fn saved(&self) -> Option<crate::store::SavedTab> {
        use crate::store::SavedTab as S;
        Some(match &self.kind {
            TabKind::Sql(t) => {
                let (database, text) = t.reopen_parts();
                S::Sql { conn: t.conn, database, text }
            }
            TabKind::Table(t) => S::Table { conn: t.conn, kind: t.kind, path: t.path.clone() },
            TabKind::Monitor(t) => S::Monitor { conn: t.conn },
            TabKind::Redis(t) => S::Redis { conn: t.conn, db: t.db },
            _ => return None,
        })
    }

    /// A command that opens this tab again after it is closed.
    fn reopen_cmd(&self) -> Option<Cmd> {
        Some(match &self.kind {
            TabKind::Home => Cmd::OpenHome,
            TabKind::Sql(t) => {
                let (database, text) = t.reopen_parts();
                Cmd::NewSql { conn: t.conn, database, text }
            }
            TabKind::Table(t) => Cmd::OpenTable { conn: t.conn, kind: t.kind, path: t.path.clone() },
            TabKind::Monitor(t) => Cmd::OpenMonitor(t.conn),
            TabKind::Users(t) => Cmd::OpenUsers(t.conn),
            TabKind::Terminal(t) => Cmd::OpenTerminal(t.conn),
            TabKind::Redis(t) => Cmd::OpenRedis { conn: t.conn, db: t.db },
            TabKind::Docker(t) => Cmd::OpenDocker { conn: t.conn, section: t.section },
            _ => return None,
        })
    }

    pub fn conn(&self) -> Option<Uuid> {
        match &self.kind {
            TabKind::Home => None,
            TabKind::Sql(t) => Some(t.conn),
            TabKind::Table(t) => Some(t.conn),
            TabKind::Object(t) => Some(t.conn),
            TabKind::Monitor(t) => Some(t.conn),
            TabKind::Users(t) => Some(t.conn),
            TabKind::Redis(t) => Some(t.conn),
            TabKind::Docker(t) => Some(t.conn),
            TabKind::Resource(t) => Some(t.conn),
            TabKind::Terminal(t) => Some(t.conn),
            TabKind::Files(t) => Some(t.conn),
            TabKind::Erd(t) => Some(t.conn),
            TabKind::Compare(t) => Some(t.left.conn),
        }
    }

    /// The tab's icon, also the leading glyph of its title.
    fn icon(&self) -> egui_material_icons::MaterialIcon {
        match &self.kind {
            TabKind::Home => ICON_DASHBOARD,
            TabKind::Sql(_) => ICON_CODE,
            TabKind::Table(_) => ICON_TABLE,
            TabKind::Object(_) => ICON_DATA_OBJECT,
            TabKind::Monitor(_) => ICON_MONITORING,
            TabKind::Users(_) => ICON_GROUP,
            TabKind::Redis(_) => ICON_KEY,
            TabKind::Docker(_) => ICON_DEPLOYED_CODE,
            TabKind::Resource(t) => t.node.as_ref().map(|n| views::resource::node_icon(n.icon)).unwrap_or(ICON_DNS),
            TabKind::Terminal(_) => ICON_TERMINAL,
            TabKind::Files(_) => ICON_FOLDER_OPEN,
            TabKind::Erd(_) => ICON_SCHEMA,
            TabKind::Compare(_) => ICON_COMPARE_ARROWS,
        }
    }

    fn title(&self, cx: &AppCtx) -> String {
        let conn = self.conn().map(|c| cx.profile_name(c)).unwrap_or_default();
        match &self.kind {
            TabKind::Home => ico(ICON_DASHBOARD, "Home"),
            TabKind::Sql(t) => ico(ICON_CODE, &format!("{}{}", t.title(&conn), if t.dirty { " •" } else { "" })),
            TabKind::Table(t) => ico(ICON_TABLE, &t.path.display()),
            TabKind::Object(t) => ico(ICON_DATA_OBJECT, &t.path.display()),
            TabKind::Monitor(_) => ico(ICON_MONITORING, &format!("Monitor · {conn}")),
            TabKind::Users(_) => ico(ICON_GROUP, &format!("Users · {conn}")),
            TabKind::Redis(t) => ico(ICON_KEY, &format!("{conn} · db{}", t.db)),
            TabKind::Docker(t) => ico(ICON_DEPLOYED_CODE, &format!("{conn} · {}", t.section.label())),
            TabKind::Resource(t) => match &t.node {
                Some(n) => ico(views::resource::node_icon(n.icon), &format!("{conn} · {}", n.label)),
                None => ico(ICON_DNS, &conn),
            },
            TabKind::Terminal(_) => ico(ICON_TERMINAL, &format!("Terminal · {conn}")),
            TabKind::Files(t) => ico(ICON_FOLDER_OPEN, &format!("{conn} · {}", t.title())),
            TabKind::Erd(t) => ico(ICON_SCHEMA, &format!("{conn} · {}", t.title())),
            TabKind::Compare(_) => ico(ICON_COMPARE_ARROWS, "Compare schemas"),
        }
    }

    /// Identity used to focus an existing tab instead of opening a duplicate.
    fn key(&self) -> Option<String> {
        match &self.kind {
            TabKind::Home => Some("home".into()),
            TabKind::Table(t) => Some(format!("table:{}:{:?}", t.conn, t.path)),
            TabKind::Object(t) => Some(format!("obj:{}:{:?}:{:?}", t.conn, t.kind, t.path)),
            TabKind::Monitor(t) => Some(format!("mon:{}", t.conn)),
            TabKind::Users(t) => Some(format!("users:{}", t.conn)),
            TabKind::Redis(t) => Some(format!("redis:{}:{}", t.conn, t.db)),
            TabKind::Docker(t) => Some(format!("docker:{}", t.conn)),
            TabKind::Resource(t) => Some(format!("res:{}", t.conn)),
            TabKind::Files(t) => Some(format!("files:{}", t.conn)),
            TabKind::Erd(t) => Some(format!("erd:{}:{:?}:{}", t.conn, t.database, t.schema)),
            TabKind::Compare(_) => None,
            TabKind::Sql(_) | TabKind::Terminal(_) => None,
        }
    }
}

struct Viewer<'a> {
    cx: &'a mut AppCtx,
    /// Pane the user last interacted with; new tabs open next to it.
    focused: &'a mut Option<TileId>,
}

impl Behavior<Tab> for Viewer<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tile_id: TileId, tab: &mut Tab) -> UiResponse {
        if ui.ui_contains_pointer() && ui.input(|i| i.pointer.any_pressed()) {
            *self.focused = Some(tile_id);
        }
        let id = tab.id;
        let bg = ui.visuals().panel_fill;
        ui.painter().rect_filled(ui.max_rect(), 0.0, bg);
        egui::Frame::new().inner_margin(egui::Margin::same(theme::space::SM as i8)).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            match &mut tab.kind {
                TabKind::Home => views::home::ui(ui, self.cx),
                TabKind::Sql(t) => t.ui(ui, self.cx, id),
                TabKind::Table(t) => t.ui(ui, self.cx, id),
                TabKind::Object(t) => t.ui(ui, self.cx, id),
                TabKind::Monitor(t) => t.ui(ui, self.cx),
                TabKind::Users(t) => t.ui(ui, self.cx),
                TabKind::Redis(t) => t.ui(ui, self.cx, id),
                TabKind::Docker(t) => t.ui(ui, self.cx, id),
                TabKind::Resource(t) => t.ui(ui, self.cx, id),
                TabKind::Terminal(t) => t.ui(ui, self.cx),
                TabKind::Files(t) => t.ui(ui, self.cx),
                TabKind::Erd(t) => t.ui(ui, self.cx),
                TabKind::Compare(t) => t.ui(ui, self.cx),
            }
        });
        UiResponse::None
    }

    fn tab_title_for_pane(&mut self, tab: &Tab) -> egui::WidgetText {
        let t = tab.title(self.cx);
        match tab.conn().and_then(|c| self.cx.store.profile(c)).map(|p| p.environment) {
            Some(openvclient_core::Environment::Production) => RichText::new(t).color(theme::tokens_for(true).destructive).into(),
            _ => t.into(),
        }
    }

    fn is_tab_closable(&self, _tiles: &Tiles<Tab>, _tile_id: TileId) -> bool {
        true
    }

    fn on_tab_close(&mut self, tiles: &mut Tiles<Tab>, tile_id: TileId) -> bool {
        if let Some(Tile::Pane(Tab { kind: TabKind::Sql(t), .. })) = tiles.get_mut(tile_id) {
            t.close(&self.cx.bg);
        }
        true
    }

    fn tab_bar_height(&self, _style: &egui::Style) -> f32 {
        32.0
    }

    fn gap_width(&self, _style: &egui::Style) -> f32 {
        2.0
    }

    fn tab_bar_color(&self, visuals: &egui::Visuals) -> egui::Color32 {
        visuals.extreme_bg_color
    }

    /// Custom tab: the active tab joins the content area with an accent bar and
    /// full-contrast title; the close button shows a pointer and turns red.
    fn tab_ui(&mut self, tiles: &mut Tiles<Tab>, ui: &mut egui::Ui, id: egui::Id, tile_id: TileId, state: &TabState) -> egui::Response {
        let t = theme::tokens(ui);
        let title = match tiles.get(tile_id) {
            Some(Tile::Pane(p)) => p.title(self.cx),
            _ => String::new(),
        };
        let prod = matches!(tiles.get(tile_id), Some(Tile::Pane(p)) if p.conn().and_then(|c| self.cx.store.profile(c)).is_some_and(|pr| pr.environment.is_protected()));
        let (is_home, tab_icon, tab_driver) = match tiles.get(tile_id) {
            Some(Tile::Pane(p)) => (matches!(p.kind, TabKind::Home), p.icon(), p.conn().and_then(|c| self.cx.store.profile(c)).map(|pr| pr.driver.clone())),
            _ => (false, ICON_DASHBOARD, None),
        };
        // Cartoon tabs draw their icon in code, so the title drops its leading glyph.
        let cartoon_glyph = theme::is_cartoon().then_some(tab_icon);
        let title = if cartoon_glyph.is_some() { title.split_once("  ").map(|(_, rest)| rest.to_string()).unwrap_or(title) } else { title };
        let font = egui::FontId::proportional(13.5);
        let text_color = if state.active { t.fg } else { t.muted_fg };
        // Long titles are cut with "…" so one tab cannot push the rest off-screen.
        let full_title = title.clone();
        let galley = crate::views::sidebar::elided(ui, &title, font, text_color, 260.0);
        let (pad, close) = (12.0, 18.0);
        let width = galley.size().x + pad * 2.0 + if state.closable { close + 6.0 } else { 0.0 } + if cartoon_glyph.is_some() { 24.0 } else { 0.0 };
        let (_, rect) = ui.allocate_space(egui::vec2(width, ui.available_height()));
        let resp = ui.interact(rect, id, egui::Sense::click_and_drag()).on_hover_cursor(egui::CursorIcon::PointingHand);
        let resp = if galley.elided { resp.on_hover_text(&full_title) } else { resp };
        if !state.is_being_dragged {
            let bg = if state.active {
                ui.visuals().panel_fill
            } else if resp.hovered() {
                t.muted
            } else {
                ui.visuals().extreme_bg_color
            };
            ui.painter().rect_filled(rect, egui::CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 }, bg);
            if state.active {
                let bar = if prod { t.destructive } else { t.accent };
                ui.painter().rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), 2.0)), 0.0, bar);
            }
            let mut tx = rect.left() + pad;
            if let Some(glyph) = cartoon_glyph {
                // Cartoon: Vee for Home, the connection's sticker for its tabs, else the cartoon icon.
                let c = egui::pos2(tx + 9.0, rect.center().y);
                let dark = ui.visuals().dark_mode;
                match tab_driver.as_deref() {
                    _ if is_home => {
                        theme::paint_brand_mark(ui, egui::Rect::from_center_size(c, egui::vec2(18.0, 18.0)));
                    }
                    Some(d) => theme::paint_connector_sticker(ui.painter(), c, 18.0, d, dark),
                    None => theme::paint_cartoon_icon(ui.painter(), c, 17.0, glyph, dark),
                }
                tx += 24.0;
            }
            ui.painter().galley(egui::pos2(tx, rect.center().y - galley.size().y / 2.0), galley, text_color);
            if state.closable {
                let crect = egui::Rect::from_center_size(egui::pos2(rect.right() - pad / 2.0 - close / 2.0, rect.center().y), egui::vec2(close, close));
                let cresp = ui.interact(crect, id.with("close"), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Close tab (⌘W)");
                let hot = cresp.hovered();
                if hot {
                    ui.painter().rect_filled(crect, 4.0, t.destructive.gamma_multiply(0.85));
                }
                // Quiet until it matters: muted on the active or hovered tab, hidden
                // on the others, red only when the pointer is on it.
                let show = hot || state.active || resp.hovered();
                if show {
                    let color = if hot { egui::Color32::WHITE } else { t.muted_fg };
                    let r = crect.shrink(5.0);
                    let s = egui::Stroke::new(1.6, color);
                    ui.painter().line_segment([r.left_top(), r.right_bottom()], s);
                    ui.painter().line_segment([r.right_top(), r.left_bottom()], s);
                }
                if (cresp.clicked() || resp.clicked_by(egui::PointerButton::Middle)) && self.on_tab_close(tiles, tile_id) {
                    if let Some(Tile::Pane(p)) = tiles.get(tile_id)
                        && let Some(c) = p.reopen_cmd()
                    {
                        self.cx.remember_closed(c);
                    }
                    tiles.remove(tile_id);
                }
            }
        }
        if resp.clicked() {
            *self.focused = Some(tile_id);
        }
        let reopen = !self.cx.closed_tabs.is_empty();
        resp.context_menu(|ui| {
            let mut act = |a| self.cx.tab_actions.push(a);
            if ui.button("Close other tabs").clicked() {
                act(TabAction::CloseOthers(tile_id));
            }
            if ui.button("Close tabs to the right").clicked() {
                act(TabAction::CloseRight(tile_id));
            }
            if ui.button("Close all tabs").clicked() {
                act(TabAction::CloseAll(tile_id));
            }
            ui.separator();
            if ui.add_enabled(reopen, egui::Button::new("Reopen closed tab").shortcut_text(theme::kbd("⌘⇧T"))).clicked() {
                act(TabAction::Reopen);
            }
        });
        self.on_tab_button(tiles, tile_id, resp)
    }

    /// A list of every tab in this strip, for when they no longer fit.
    fn top_bar_right_ui(&mut self, tiles: &Tiles<Tab>, ui: &mut egui::Ui, _tile_id: TileId, tabs: &egui_tiles::Tabs, _scroll: &mut f32) {
        if tabs.children.len() < 2 {
            return;
        }
        ui.add_space(theme::space::XS);
        ui.menu_button(String::from(ICON_EXPAND_MORE), |ui| {
            ui.set_min_width(240.0);
            for child in &tabs.children {
                if let Some(Tile::Pane(p)) = tiles.get(*child) {
                    let active = tabs.active == Some(*child);
                    if ui.selectable_label(active, p.title(self.cx)).clicked() {
                        self.cx.tab_actions.push(TabAction::Focus(*child));
                        ui.close();
                    }
                }
            }
        })
        .response
        .on_hover_text("All open tabs");
    }

    fn simplification_options(&self) -> SimplificationOptions {
        SimplificationOptions { all_panes_must_have_tabs: true, ..Default::default() }
    }
}

pub struct OpenvApp {
    tree: Tree<Tab>,
    focused: Option<TileId>,
    cx: AppCtx,
    /// Minimum window size last sent for the current monitor.
    min_window: egui::Vec2,
    /// Whether the start-up size has been checked against the monitor.
    window_fitted: bool,
    /// Windows: whether the end-session hook has been attempted.
    #[cfg(windows)]
    session_hooked: bool,
    /// Zoom factor last applied, to tell keyboard zoom from a Settings change.
    applied_zoom: f32,
}

impl OpenvApp {
    pub fn new(ctx: &egui::Context, rt: tokio::runtime::Handle, registry: DriverRegistry) -> Self {
        let store = Store::load();
        let bg = Bg::new(rt, ctx.clone());
        theme::set_cartoon(ctx, store.ws.settings.style == ThemeStyle::Cartoon);
        apply_theme(ctx, store.ws.settings.theme);
        let mut cx = AppCtx {
            bg,
            conns: Connections::new(registry),
            toasts: Toasts::default(),
            cmds: Vec::new(),
            catalogs: HashMap::new(),
            sidebar: Default::default(),
            conn_dialog: None,
            dialog: None,
            confirm: None,
            palette: Default::default(),
            password_prompt: None,
            host_key_prompt: None,
            update: UpdateState::default(),
            selected_conn: None,
            next_tab: 1,
            dirty: false,
            last_save: Instant::now(),
            show_settings: false,
            show_about: false,
            show_shortcuts: false,
            sidebar_hidden: false,
            pending_sql: None,
            resource_jobs: HashMap::new(),
            tab_actions: Vec::new(),
            closed_tabs: Vec::new(),
            restore_offer: Vec::new(),
            restore_pending: Vec::new(),
            store,
        };
        // Offer last session's tabs whose connection still exists.
        let saved = std::mem::take(&mut cx.store.ws.session);
        cx.restore_offer = saved.into_iter().filter(|t| cx.store.profile(t.conn()).is_some()).collect();
        if let Some(e) = cx.store.load_error.clone() {
            // A partly decrypted workspace still loaded; say so instead of "could not be loaded".
            if e.contains("could not be decrypted") {
                cx.toasts.warn("Some saved items were skipped", e);
            } else {
                cx.toasts.error_text("Workspace could not be loaded", e);
            }
        }
        let tree = Tree::new_tabs(egui::Id::unique("workspace"), vec![Tab { id: 1, kind: TabKind::Home }]);
        Self {
            tree,
            focused: None,
            cx,
            min_window: egui::Vec2::from(crate::MIN_WINDOW),
            window_fitted: false,
            #[cfg(windows)]
            session_hooked: false,
            applied_zoom: 1.0,
        }
    }

    fn find(&self, pred: impl Fn(&Tab) -> bool) -> Option<TileId> {
        self.tree.tiles.iter().find_map(|(id, t)| match t {
            Tile::Pane(p) if pred(p) => Some(*id),
            _ => None,
        })
    }

    fn activate(&mut self, id: TileId) {
        self.tree.make_active(|tid, _| tid == id);
        self.focused = Some(id);
    }

    fn open(&mut self, kind: TabKind) {
        let tab = Tab { id: self.cx.tab_id(), kind };
        if let Some(key) = tab.key()
            && let Some(id) = self.find(|t| t.key().as_deref() == Some(key.as_str()))
        {
            self.activate(id);
            return;
        }
        let pane = self.tree.tiles.insert_pane(tab);
        // Open next to the pane the user last worked in, else in the first tab bar.
        let target = self
            .focused
            .and_then(|f| self.tree.tiles.parent_of(f))
            .filter(|p| matches!(self.tree.tiles.get(*p), Some(Tile::Container(Container::Tabs(_)))))
            .or_else(|| self.tree.tiles.iter().find_map(|(id, t)| matches!(t, Tile::Container(Container::Tabs(_))).then_some(*id)));
        match target {
            Some(tabs_id) => {
                if let Some(Tile::Container(Container::Tabs(tabs))) = self.tree.tiles.get_mut(tabs_id) {
                    tabs.add_child(pane);
                    tabs.set_active(pane);
                }
            }
            None => {
                let tabs_id = self.tree.tiles.insert_tab_tile(vec![pane]);
                match self.tree.root {
                    Some(root) => {
                        let h = self.tree.tiles.insert_horizontal_tile(vec![root, tabs_id]);
                        self.tree.root = Some(h);
                    }
                    None => self.tree.root = Some(tabs_id),
                }
            }
        }
        self.activate(pane);
    }

    fn close_where(&mut self, pred: impl Fn(&Tab) -> bool) {
        let ids: Vec<TileId> = self.tree.tiles.iter().filter_map(|(id, t)| matches!(t, Tile::Pane(p) if pred(p)).then_some(*id)).collect();
        for id in ids {
            if let Some(Tile::Pane(p)) = self.tree.tiles.get(id)
                && let Some(c) = p.reopen_cmd()
            {
                self.cx.remember_closed(c);
            }
            if let Some(Tile::Pane(Tab { kind: TabKind::Sql(t), .. })) = self.tree.tiles.get_mut(id) {
                t.close(&self.cx.bg);
            }
            self.tree.remove_recursively(id);
        }
    }

    /// Apply tab-strip requests queued while drawing the tree.
    fn apply_tab_actions(&mut self) {
        for a in std::mem::take(&mut self.cx.tab_actions) {
            let siblings = |tree: &Tree<Tab>, id: TileId| -> Vec<TileId> {
                match tree.tiles.parent_of(id).and_then(|p| tree.tiles.get(p)) {
                    Some(Tile::Container(Container::Tabs(t))) => t.children.clone(),
                    _ => vec![id],
                }
            };
            let pane_ids = |tree: &Tree<Tab>, ids: Vec<TileId>| -> Vec<u64> { ids.into_iter().filter_map(|i| if let Some(Tile::Pane(p)) = tree.tiles.get(i) { Some(p.id) } else { None }).collect() };
            match a {
                TabAction::Focus(id) => self.activate(id),
                TabAction::Reopen => {
                    if let Some(c) = self.cx.closed_tabs.pop() {
                        self.cx.cmd(c);
                    }
                }
                TabAction::CloseOthers(id) | TabAction::CloseRight(id) | TabAction::CloseAll(id) => {
                    let all = siblings(&self.tree, id);
                    let pos = all.iter().position(|c| *c == id).unwrap_or(0);
                    let victims: Vec<TileId> = match a {
                        TabAction::CloseOthers(_) => all.iter().copied().filter(|c| *c != id).collect(),
                        TabAction::CloseRight(_) => all[pos + 1..].to_vec(),
                        _ => all.clone(),
                    };
                    let ids = pane_ids(&self.tree, victims);
                    self.close_where(|t| ids.contains(&t.id));
                    if matches!(a, TabAction::CloseOthers(_) | TabAction::CloseRight(_)) {
                        self.activate(id);
                    }
                }
            }
        }
    }

    fn run_commands(&mut self) {
        // Restored tabs open once their connection is ready; failed ones are dropped.
        if !self.cx.restore_pending.is_empty() {
            let pending = std::mem::take(&mut self.cx.restore_pending);
            for t in pending {
                match self.cx.conns.state(t.conn()) {
                    Some(ConnState::Connected(_)) => self.cx.cmd(t.into_cmd()),
                    Some(ConnState::Failed(_)) => {}
                    _ => self.cx.restore_pending.push(t),
                }
            }
        }
        let cmds = std::mem::take(&mut self.cx.cmds);
        for c in cmds {
            match c {
                Cmd::NewSql { conn, database, text } => {
                    let t = views::sql::SqlTab::new(&mut self.cx, conn, database, text);
                    self.open(TabKind::Sql(Box::new(t)));
                }
                Cmd::OpenTable { conn, kind, path } => {
                    let t = views::table::TableTab::new(&self.cx, conn, kind, path);
                    self.open(TabKind::Table(Box::new(t)));
                }
                Cmd::OpenObject { conn, kind, path } => self.open(TabKind::Object(Box::new(views::object::ObjectTab::new(conn, kind, path)))),
                Cmd::OpenErd { conn, database, schema } => self.open(TabKind::Erd(Box::new(views::erd::ErdTab::new(conn, database, schema)))),
                Cmd::OpenCompare { conn, database, schema } => self.open(TabKind::Compare(Box::new(views::compare::CompareTab::new(conn, database, schema)))),
                Cmd::OpenMonitor(conn) => self.open(TabKind::Monitor(Box::new(views::monitor::MonitorTab::new(conn)))),
                Cmd::OpenUsers(conn) => self.open(TabKind::Users(Box::new(views::users::UsersTab::new(conn)))),
                Cmd::OpenTerminal(conn) => self.open(TabKind::Terminal(Box::new(views::terminal::TerminalTab::new(conn)))),
                Cmd::OpenRedis { conn, db } => self.open(TabKind::Redis(Box::new(views::redis::RedisTab::new(conn, db)))),
                Cmd::OpenDocker { conn, section } => {
                    let key = format!("docker:{conn}");
                    if let Some(id) = self.find(|t| t.key().as_deref() == Some(key.as_str()))
                        && let Some(Tile::Pane(Tab { kind: TabKind::Docker(d), .. })) = self.tree.tiles.get_mut(id)
                    {
                        d.section = section;
                    }
                    self.open(TabKind::Docker(Box::new(views::docker::DockerTab::new(conn, section))));
                }
                Cmd::OpenResource { conn, node, action } if action.is_none() && self.cx.conns.session(conn).and_then(|h| h.resource()).is_some_and(|r| r.file_browser()) => {
                    // Storage connections open in the file manager; a file node opens its folder.
                    let folder = node.as_ref().filter(|n| n.expandable).map(|n| n.id.clone());
                    let key = format!("files:{conn}");
                    if let Some(id) = self.find(|t| t.key().as_deref() == Some(key.as_str())) {
                        if let Some(Tile::Pane(Tab { kind: TabKind::Files(f), .. })) = self.tree.tiles.get_mut(id)
                            && node.is_some()
                        {
                            f.navigate(folder);
                        }
                        self.activate(id);
                    } else {
                        self.open(TabKind::Files(Box::new(views::files::FilesTab::new(conn, folder))));
                    }
                }
                Cmd::OpenResource { conn, node, action } => {
                    let key = format!("res:{conn}");
                    if let Some(id) = self.find(|t| t.key().as_deref() == Some(key.as_str())) {
                        if let Some(Tile::Pane(Tab { kind: TabKind::Resource(r), .. })) = self.tree.tiles.get_mut(id) {
                            r.navigate(node, action);
                        }
                        self.activate(id);
                    } else {
                        self.open(TabKind::Resource(Box::new(views::resource::ResourceTab::new(conn, node, action))));
                    }
                }
                Cmd::OpenHome => self.open(TabKind::Home),
                Cmd::Connect(id) => self.connect(id),
                Cmd::Disconnect(id) => {
                    self.cx.conns.disconnect(&self.cx.bg, id);
                    self.cx.sidebar.forget(id);
                    self.cx.catalogs.retain(|(c, _), _| *c != id);
                }
                Cmd::EditConnection(id) => {
                    let p = id.and_then(|i| self.cx.store.profile(i).cloned());
                    self.cx.conn_dialog = Some(views::conn_dialog::ConnDialog::new(&self.cx, p, None));
                }
                Cmd::NewConnectionOf(driver) => {
                    let mut d = views::conn_dialog::ConnDialog::new(&self.cx, None, None);
                    d.preselect(&self.cx, driver);
                    self.cx.conn_dialog = Some(d);
                }
                Cmd::NewConnectionIn(folder) => {
                    self.cx.conn_dialog = Some(views::conn_dialog::ConnDialog::new(&self.cx, None, Some(folder)));
                }
                Cmd::DuplicateConnection(id) => {
                    if let Some(mut p) = self.cx.store.profile(id).cloned() {
                        p.id = Uuid::new_v4();
                        p.name = format!("{} copy", p.name);
                        p.save_secret = false;
                        self.cx.conn_dialog = Some(views::conn_dialog::ConnDialog::new_from(&self.cx, p));
                    }
                }
                Cmd::DeleteConnection(id) => {
                    self.cx.conns.disconnect(&self.cx.bg, id);
                    self.cx.conns.forget(id);
                    self.cx.sidebar.forget(id);
                    let _ = crate::store::secrets::delete(id);
                    self.cx.store.remove(id);
                    self.close_where(|t| t.conn() == Some(id));
                    self.cx.mark_dirty();
                }
                Cmd::RefreshTree(id) => self.cx.sidebar.forget(id),
                Cmd::InvalidateCatalog(id) => self.cx.catalogs.retain(|(c, _), _| *c != id),
                Cmd::FocusTab(tid) => {
                    if let Some(id) = self.find(|t| t.id == tid) {
                        self.activate(id);
                    }
                }
            }
        }
    }

    fn connect(&mut self, id: Uuid) {
        let Some(p) = self.cx.store.profile(id).cloned() else { return };
        if self.cx.conns.needs_prompt(&p) {
            self.cx.password_prompt = Some((id, String::new()));
            return;
        }
        self.cx.conns.connect(&self.cx.bg, &p);
    }

    fn handle_events(&mut self) {
        for ev in self.cx.conns.poll(&self.cx.bg) {
            match ev {
                ConnEvent::Connected(id, fingerprint) => {
                    let name = self.cx.profile_name(id);
                    if let Some(p) = self.cx.store.profile_mut(id) {
                        p.last_connected_at = Some(chrono::Utc::now().to_rfc3339());
                        // Pin the SSH host key on first use.
                        if let Some(fp) = fingerprint {
                            p.params.entry(openvclient_net::keys::SSH_FINGERPRINT.into()).or_insert(fp);
                        }
                    }
                    self.cx.mark_dirty();
                    self.cx.sidebar.expand(id);
                    self.cx.toasts.ok(format!("Connected to {name}"));
                }
                ConnEvent::Failed(id, e) => {
                    let name = self.cx.profile_name(id);
                    if let Some(fp) = openvclient_net::unknown_host_key(&e) {
                        // Ask before trusting a server key we have never seen.
                        let jump = e.message.contains("(jump host)");
                        let host = e.message.split(" for ").nth(1).and_then(|r| r.split(". ").next()).map(str::to_owned).unwrap_or_else(|| e.message.rsplit(' ').next().unwrap_or("").to_string());
                        self.cx.host_key_prompt = Some(HostKeyPrompt { conn: id, host, fingerprint: fp, param: if jump { "jump_fingerprint" } else { openvclient_net::keys::SSH_FINGERPRINT } });
                    } else if e.kind != openvclient_core::ErrorKind::Cancelled {
                        self.cx.toasts.error(format!("Could not connect to {name}"), &e);
                    }
                }
                ConnEvent::Notice(id, msg) => {
                    let name = self.cx.profile_name(id);
                    self.cx.toasts.sticky(format!("{name} needs you"), msg);
                }
                ConnEvent::Lost(id, e) => {
                    let name = self.cx.profile_name(id);
                    self.cx.sidebar.forget(id);
                    self.cx.toasts.error(format!("Lost connection to {name}"), &e);
                }
            }
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = |k| KeyboardShortcut::new(Modifiers::COMMAND, k);
        let (reopen, nth) = ctx.input_mut(|i| {
            let reopen = i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::T));
            let keys = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
            let nth = keys.iter().position(|k| i.consume_shortcut(&cmd(*k)));
            (reopen, nth)
        });
        if reopen {
            self.cx.tab_actions.push(TabAction::Reopen);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&cmd(Key::B))) {
            self.cx.sidebar_hidden = !self.cx.sidebar_hidden;
        }
        if let Some(n) = nth {
            // ⌘1…⌘8 pick that tab in the focused strip; ⌘9 picks the last one.
            let strip = self.focused.and_then(|f| self.tree.tiles.parent_of(f)).or_else(|| self.tree.tiles.iter().find_map(|(id, t)| matches!(t, Tile::Container(Container::Tabs(_))).then_some(*id)));
            if let Some(Tile::Container(Container::Tabs(t))) = strip.and_then(|s| self.tree.tiles.get(s)) {
                let target = if n == 8 { t.children.last().copied() } else { t.children.get(n).copied() };
                if let Some(id) = target {
                    self.cx.tab_actions.push(TabAction::Focus(id));
                }
            }
        }
        let (palette, new_conn, new_sql, close, home, keys) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&cmd(Key::K)) || i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::P)),
                i.consume_shortcut(&cmd(Key::N)),
                i.consume_shortcut(&cmd(Key::T)),
                i.consume_shortcut(&cmd(Key::W)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::H)),
                i.consume_shortcut(&cmd(Key::Slash)),
            )
        });
        if keys {
            self.cx.show_shortcuts = !self.cx.show_shortcuts;
        }
        if palette {
            self.cx.palette.open();
        }
        if new_conn {
            self.cx.cmd(Cmd::EditConnection(None));
        }
        if new_sql {
            let conn = self.focused_conn().or(self.cx.selected_conn).filter(|c| self.cx.conns.session(*c).is_some_and(|s| s.sql().is_some()));
            match conn {
                Some(c) => self.cx.cmd(Cmd::NewSql { conn: c, database: None, text: String::new() }),
                None => self.cx.toasts.info("No SQL connection selected", theme::kbd("Connect to a database first, then press ⌘T")),
            }
        }
        if close && let Some(f) = self.focused.filter(|f| self.tree.tiles.get(*f).is_some()) {
            if let Some(Tile::Pane(t)) = self.tree.tiles.get(f) {
                let tid = t.id;
                self.close_where(|t| t.id == tid);
            }
            self.focused = None;
        }
        if home {
            self.cx.cmd(Cmd::OpenHome);
        }
    }

    fn focused_conn(&mut self) -> Option<Uuid> {
        let f = self.focused?;
        match self.tree.tiles.get(f) {
            Some(Tile::Pane(t)) if self.tree.is_visible(f) => t.conn(),
            _ => None,
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("topbar").exact_size(40.0).show(ui, |ui| {
            // Indeterminate progress strip once anything has run for 300 ms.
            let slow = self.cx.bg.running().iter().any(|t| t.started.elapsed() > Duration::from_millis(300)) || self.cx.conns.any_connecting();
            if slow {
                let r = ui.max_rect();
                let strip = egui::Rect::from_min_max(egui::pos2(r.left(), r.bottom() - 2.0), r.right_bottom());
                let t = ui.input(|i| i.time) as f32;
                let w = strip.width() * 0.25;
                let x = strip.left() + ((t * 0.6).fract() * (strip.width() + w)) - w;
                let seg = egui::Rect::from_min_max(egui::pos2(x.max(strip.left()), strip.top()), egui::pos2((x + w).min(strip.right()), strip.bottom()));
                ui.painter().rect_filled(seg, 1.0, theme::tokens(ui).accent);
                ui.ctx().request_repaint_after(Duration::from_millis(16));
            }
            ui.horizontal_centered(|ui| {
                ui.add_space(theme::space::XS);
                theme::brand_mark(ui, 26.0);
                theme::wordmark(ui, 16.0);
                ui.add_space(theme::space::SM);
                let (g, tip) = if self.cx.sidebar_hidden { (ICON_LEFT_PANEL_OPEN, "Show sidebar (⌘B)") } else { (ICON_LEFT_PANEL_CLOSE, "Hide sidebar (⌘B)") };
                if ui.button(String::from(g)).on_hover_text(theme::kbd(tip)).clicked() {
                    self.cx.sidebar_hidden = !self.cx.sidebar_hidden;
                }
                ui.add_space(theme::space::SM);
                let search = ui.add_sized(
                    [340.0, 26.0],
                    egui::Button::new(RichText::new(format!("{}   Search connections, tables, commands…   {}", String::from(ICON_SEARCH), theme::kbd("⌘K"))).color(theme::muted(ui))),
                );
                if search.clicked() {
                    self.cx.palette.open();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(theme::space::XS);
                    if ui.button(String::from(ICON_SETTINGS)).on_hover_text("Settings").clicked() {
                        self.cx.show_settings = !self.cx.show_settings;
                    }
                    if ui.button(String::from(ICON_INFO)).on_hover_text(format!("About OpenVClient {}", env!("CARGO_PKG_VERSION"))).clicked() {
                        self.cx.show_about = !self.cx.show_about;
                    }
                    if let Some(rel) = &self.cx.update.available {
                        let label = format!("Update {}", rel.version);
                        if ui.add(egui::Button::new(RichText::new(ico(ICON_UPGRADE, &label)).color(theme::tokens(ui).info))).on_hover_text("A newer OpenVClient release is available").clicked() {
                            self.cx.update.show = true;
                        }
                    }
                    if theme::primary_button(ui, ico(ICON_ADD, "New connection")).on_hover_text(theme::kbd("⌘N")).clicked() {
                        self.cx.cmd(Cmd::EditConnection(None));
                    }
                });
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status").exact_size(26.0).show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                let t = theme::tokens(ui);
                let focused = self.focused_conn();
                if let Some(c) = focused {
                    if let Some(p) = self.cx.store.profile(c) {
                        theme::badge(ui, p.environment.short(), theme::env_color(ui, p.environment)).on_hover_text(crate::views::env_tip(p.environment));
                        ui.label(RichText::new(&p.name).small());
                    }
                    match self.cx.conns.state(c) {
                        Some(ConnState::Connected(l)) => {
                            let info = l.handle.base().server_info();
                            ui.label(RichText::new(format!("· {}", info.product)).small().color(t.muted_fg));
                            if let Some(tn) = &l.tunnel {
                                ui.label(RichText::new(format!("· {}", ico(ICON_LOCK, &tn.description))).small().color(t.info)).on_hover_text("Traffic goes through this network path");
                            }
                            if let Some(lat) = l.latency {
                                ui.label(RichText::new(format!("· {}", openvclient_core::elapsed_ms(lat))).small().color(t.muted_fg)).on_hover_text("Last health check round trip");
                            }
                        }
                        Some(ConnState::Failed(e)) => {
                            ui.label(RichText::new(format!("· {}", e.message)).small().color(t.destructive));
                        }
                        Some(ConnState::Connecting { .. }) => {
                            ui.label(RichText::new("· connecting…").small().color(t.muted_fg));
                        }
                        None => {
                            ui.label(RichText::new("· disconnected").small().color(t.muted_fg));
                        }
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let running = self.cx.bg.running();
                    let n = self.cx.conns.connected_ids().len();
                    ui.label(RichText::new(format!("{n} connected")).small().color(t.muted_fg));
                    // Cursor position of the focused SQL editor.
                    if let Some(Tile::Pane(Tab { kind: TabKind::Sql(st), .. })) = self.focused.and_then(|f| self.tree.tiles.get(f))
                        && let Some((line, col, sel)) = st.cursor_position()
                    {
                        ui.separator();
                        let txt = if sel > 0 { format!("Ln {line}, Col {col} ({sel} selected)") } else { format!("Ln {line}, Col {col}") };
                        ui.label(RichText::new(txt).small().color(t.muted_fg));
                    }
                    for task in running.iter().take(3) {
                        ui.separator();
                        if ui.small_button(String::from(ICON_CLOSE)).on_hover_text("Cancel").clicked() {
                            self.cx.bg.cancel_task(task.id);
                        }
                        ui.label(RichText::new(format!("{} · {:.0}s", task.label, task.started.elapsed().as_secs_f32())).small());
                        ui.add(egui::Spinner::new().size(12.0));
                    }
                    if running.len() > 3 {
                        ui.label(RichText::new(format!("+{} more", running.len() - 3)).small());
                    }
                    if !running.is_empty() {
                        ui.ctx().request_repaint_after(Duration::from_millis(250));
                    }
                });
            });
        });
    }

    fn host_key_prompt(&mut self, ctx: &egui::Context) {
        let Some(p) = self.cx.host_key_prompt.take() else { return };
        let name = self.cx.profile_name(p.conn);
        let (mut keep, mut trust) = (true, false);
        egui::Modal::new(egui::Id::unique("host_key_prompt")).show(ctx, |ui| {
            ui.set_width(560.0_f32.min(ctx.content_rect().width() - 48.0));
            let t = theme::tokens(ui);
            ui.horizontal(|ui| {
                ui.label(RichText::new(String::from(ICON_GPP_MAYBE)).size(theme::icon::LG).color(t.warning));
                ui.heading("Trust this SSH server?");
            });
            ui.add_space(theme::space::SM);
            let known_hosts = if cfg!(windows) { r"%USERPROFILE%\.ssh\known_hosts" } else { "~/.ssh/known_hosts" };
            ui.add(egui::Label::new(format!("{name} is connecting to {} for the first time. Its identity is not in {known_hosts} and has not been trusted before.", p.host)).wrap());
            ui.add_space(theme::space::SM);
            egui::Frame::new().fill(t.muted).corner_radius(6.0).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                ui.label(RichText::new("Host key fingerprint (SHA-256)").small().color(t.muted_fg));
                ui.horizontal(|ui| {
                    ui.add(egui::Label::new(RichText::new(&p.fingerprint).monospace().strong()).wrap());
                    if ui.small_button(String::from(ICON_CONTENT_COPY)).on_hover_text("Copy fingerprint").clicked() {
                        ui.ctx().copy_text(p.fingerprint.clone());
                    }
                });
            });
            ui.add_space(theme::space::SM);
            ui.add(egui::Label::new(RichText::new("Compare it with the fingerprint from the server's administrator (for example `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` on the server). If they differ, someone may be intercepting the connection.").color(t.muted_fg)).wrap());
            ui.add_space(theme::space::MD);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::primary_button(ui, ico(ICON_VERIFIED_USER, "Trust and connect")).clicked() {
                        trust = true;
                    }
                });
            });
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                keep = false;
            }
        });
        if trust {
            if let Some(prof) = self.cx.store.profile_mut(p.conn) {
                prof.params.insert(p.param.to_string(), p.fingerprint.clone());
            }
            self.cx.mark_dirty();
            tracing::info!(target: "openvclient::conn", name = %name, host = %p.host, fingerprint = %p.fingerprint, "SSH host key trusted by the user");
            self.connect(p.conn);
        } else if keep {
            self.cx.host_key_prompt = Some(p);
        }
    }

    fn password_prompt(&mut self, ctx: &egui::Context) {
        let Some((id, mut pw)) = self.cx.password_prompt.take() else { return };
        let name = self.cx.profile_name(id);
        let mut keep = true;
        let mut go = false;
        egui::Modal::new(egui::Id::unique("pw_prompt")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.heading(format!("Password for {name}"));
            ui.label(RichText::new("Used for this session only. Turn on “Save password” in the connection to keep it in the system keychain.").color(theme::muted(ui)));
            ui.add_space(theme::space::SM);
            let r = ui.add(egui::TextEdit::singleline(&mut pw).password(true).hint_text("Password").desired_width(f32::INFINITY));
            r.request_focus();
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                go = true;
            }
            ui.add_space(theme::space::SM);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "Connect").clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    keep = false;
                }
            });
        });
        if go {
            self.cx.conns.remember_secret(id, pw);
            if let Some(p) = self.cx.store.profile(id).cloned() {
                self.cx.conns.connect(&self.cx.bg, &p);
            }
        } else if keep {
            self.cx.password_prompt = Some((id, pw));
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut c) = self.cx.confirm.take() else { return };
        let mut done = false;
        let mut run = false;
        egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
            ui.set_width(440.0);
            let t = theme::tokens(ui);
            ui.horizontal(|ui| {
                ui.label(RichText::new(String::from(ICON_WARNING)).color(t.destructive).size(22.0));
                ui.heading(&c.title);
            });
            ui.add_space(theme::space::SM);
            ui.label(&c.body);
            if let Some(req) = &c.require_text {
                ui.add_space(theme::space::SM);
                ui.label(RichText::new(format!("This is a PRODUCTION connection. Type {req} to confirm.")).color(t.destructive));
                ui.add(egui::TextEdit::singleline(&mut c.typed).desired_width(f32::INFINITY)).request_focus();
            }
            ui.add_space(theme::space::MD);
            let allowed = c.require_text.as_ref().is_none_or(|r| r == &c.typed);
            ui.horizontal(|ui| {
                let b = ui.add_enabled_ui(allowed, |ui| theme::danger_button(ui, &c.confirm_label)).inner;
                if b.clicked() {
                    run = true;
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    done = true;
                }
            });
        });
        if run {
            if let Some(a) = c.action.take() {
                a(&mut self.cx);
            }
        } else if !done {
            self.cx.confirm = Some(c);
        }
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.cx.show_about {
            return;
        }
        let version = env!("CARGO_PKG_VERSION");
        let beta = crate::update::is_prerelease(version);
        let mut close = false;
        egui::Modal::new(egui::Id::unique("about")).show(ctx, |ui| {
            ui.set_width(460.0_f32.min(ctx.content_rect().width() - 48.0));
            let t = theme::tokens(ui);
            ui.horizontal(|ui| {
                theme::brand_mark(ui, 56.0);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        theme::wordmark(ui, 22.0);
                        if beta {
                            theme::badge(ui, "Beta", t.warning);
                        }
                    });
                    ui.label(RichText::new(format!("Version {version} · {} {}", std::env::consts::OS, std::env::consts::ARCH)).color(t.muted_fg));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if ui.button(String::from(ICON_CLOSE)).on_hover_text("Close (Esc)").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        close = true;
                    }
                });
            });
            ui.add_space(theme::space::SM);
            ui.label(RichText::new("Free, open-source desktop client for databases, object storage and servers.").color(t.fg));
            ui.add_space(theme::space::MD);

            // Updates
            egui::Frame::new().fill(t.muted).corner_radius(8.0).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let checking = self.cx.update.job.is_some();
                    if checking {
                        ui.spinner();
                        ui.label("Checking for updates…");
                    } else if let Some(rel) = self.cx.update.available.clone() {
                        ui.label(RichText::new(ico(ICON_UPGRADE, &format!("Version {} is available", rel.version))).color(t.info));
                    } else {
                        ui.label(RichText::new(self.cx.update.last.clone().unwrap_or_else(|| "Updates are checked at start-up".into())).color(t.muted_fg));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.cx.update.available.is_some() {
                            if theme::primary_button(ui, ico(ICON_DOWNLOAD, "Update")).clicked() {
                                self.cx.update.show = true;
                                close = true;
                            }
                        } else if ui.add_enabled(!checking, egui::Button::new(ico(ICON_REFRESH, "Check for updates"))).clicked() {
                            self.cx.check_updates(true);
                        }
                    });
                });
            });
            ui.add_space(theme::space::MD);

            // Links
            let link = |ui: &mut egui::Ui, icon, text: &str, url: &str| {
                if ui.add(egui::Button::new(ico(icon, text)).frame(false)).on_hover_text(url).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    crate::update::open_url(url);
                }
            };
            let repo = format!("https://github.com/{}", crate::update::REPO);
            ui.horizontal_wrapped(|ui| {
                link(ui, ICON_LANGUAGE, "Website", crate::update::WEBSITE);
                link(ui, ICON_CODE, "Source on GitHub", &repo);
                link(ui, ICON_NEW_RELEASES, "Release notes", &format!("{repo}/releases"));
                link(ui, ICON_BUG_REPORT, "Report a bug", &format!("{repo}/issues/new/choose"));
                if ui.button(ico(ICON_KEYBOARD, "Keyboard shortcuts")).clicked() {
                    self.cx.show_shortcuts = true;
                    close = true;
                }
            });
            ui.add_space(theme::space::SM);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("MIT OR Apache-2.0 · © OpenVClient contributors").small().color(t.muted_fg));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button(ico(ICON_CONTENT_COPY, "Copy version info")).clicked() {
                        ui.ctx().copy_text(format!("OpenVClient {version}\n{} {} ", std::env::consts::OS, std::env::consts::ARCH));
                        self.cx.toasts.ok("Version info copied");
                    }
                });
            });
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
        });
        if close {
            self.cx.show_about = false;
        }
    }

    fn shortcuts_window(&mut self, ctx: &egui::Context) {
        if !self.cx.show_shortcuts {
            return;
        }
        let mut close = false;
        egui::Modal::new(egui::Id::unique("shortcuts")).show(ctx, |ui| {
            ui.set_width(480.0_f32.min(ctx.content_rect().width() - 48.0));
            ui.horizontal(|ui| {
                ui.label(RichText::new(ico(ICON_KEYBOARD, "Keyboard shortcuts")).size(17.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(String::from(ICON_CLOSE)).on_hover_text("Close (Esc)").clicked() {
                        close = true;
                    }
                });
            });
            ui.add_space(theme::space::SM);
            views::shortcuts_grid(ui);
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
        });
        if close {
            self.cx.show_shortcuts = false;
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.cx.show_settings {
            return;
        }
        let mut close = false;
        let mut check_now = false;
        let mut open_keys = false;
        let (mut export_ws, mut import_ws) = (false, false);
        let before = self.cx.store.ws.settings.clone();
        egui::Modal::new(egui::Id::unique("settings")).show(ctx, |ui| {
            ui.set_width(640.0_f32.min(ctx.content_rect().width() - 48.0));
            let t = theme::tokens(ui);
            ui.horizontal(|ui| {
                ui.label(RichText::new(String::from(ICON_SETTINGS)).size(theme::icon::LG).color(t.accent));
                ui.label(RichText::new("Settings").size(18.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(String::from(ICON_CLOSE)).on_hover_text("Close (Esc)").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        close = true;
                    }
                });
            });
            ui.separator();
            ui.add_space(theme::space::SM);
            let max_h = (ctx.content_rect().height() * 0.88 - 90.0).max(240.0);
            egui::ScrollArea::vertical().max_height(max_h).min_scrolled_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
                let s = &mut self.cx.store.ws.settings;
                let label_w = 170.0;
                let row = |ui: &mut egui::Ui, label: &str, hint: &str, body: &mut dyn FnMut(&mut egui::Ui)| {
                    ui.horizontal(|ui| {
                        ui.allocate_ui_with_layout(egui::vec2(label_w, 28.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.set_min_width(label_w);
                            let r = ui.label(label);
                            if !hint.is_empty() {
                                r.on_hover_text(hint);
                            }
                        });
                        body(ui);
                    });
                    ui.add_space(theme::space::XS);
                };
                ui.label(RichText::new("APPEARANCE").small().strong().color(t.muted_fg));
                row(ui, "Style", "Original look, or the playful cartoon look of openvclient.xyz", &mut |ui| {
                    let label = |v: ThemeStyle| match v {
                        ThemeStyle::Original => "Original",
                        ThemeStyle::Cartoon => "Cartoon",
                    };
                    egui::ComboBox::from_id_salt("theme_style").selected_text(label(s.style)).width(160.0).show_ui(ui, |ui| {
                        for v in [ThemeStyle::Original, ThemeStyle::Cartoon] {
                            if ui.selectable_value(&mut s.style, v, label(v)).clicked() {
                                theme::set_cartoon(ui.ctx(), v == ThemeStyle::Cartoon);
                            }
                        }
                    });
                });
                row(ui, "Mode", "Light or dark; System follows your computer", &mut |ui| {
                    let label = |v: ThemeChoice| match v {
                        ThemeChoice::Dark => "Dark",
                        ThemeChoice::Light => "Light",
                        _ => "System",
                    };
                    egui::ComboBox::from_id_salt("theme_mode").selected_text(label(s.theme)).width(160.0).show_ui(ui, |ui| {
                        for v in [ThemeChoice::System, ThemeChoice::Dark, ThemeChoice::Light] {
                            if ui.selectable_value(&mut s.theme, v, label(v)).clicked() {
                                apply_theme(ui.ctx(), v);
                            }
                        }
                    });
                });
                row(ui, "Editor font size", "SQL editor and DDL views", &mut |ui| {
                    ui.spacing_mut().slider_width = 220.0;
                    ui.add(egui::Slider::new(&mut s.editor_font_size, 10.0..=22.0).step_by(0.5).fixed_decimals(1).suffix(" px"));
                });
                row(ui, "Updates", "Check GitHub for a newer release at startup", &mut |ui| {
                    ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).monospace());
                    ui.checkbox(&mut s.check_updates, "Check automatically");
                    if ui.button("Check now").clicked() {
                        check_now = true;
                    }
                });
                row(ui, "Interface size", &theme::kbd("Whole app; also ⌘= and ⌘−"), &mut |ui| {
                    ui.spacing_mut().slider_width = 220.0;
                    ui.add(egui::Slider::new(&mut s.ui_zoom, 0.7..=1.6).step_by(0.05).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
                });
                row(ui, "Scroll speed", "Multiplier for mouse wheel and trackpad scrolling", &mut |ui| {
                    ui.spacing_mut().slider_width = 220.0;
                    ui.add(egui::Slider::new(&mut s.scroll_speed, 0.2..=2.0).step_by(0.05).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
                });
                ui.add_space(theme::space::SM);
                ui.label(RichText::new("DATA").small().strong().color(t.muted_fg));
                row(ui, "Max rows per result", "Rows kept per SQL result; the rest are counted, not stored", &mut |ui| {
                    ui.add(egui::DragValue::new(&mut s.max_rows).range(100..=1_000_000).speed(100));
                });
                row(ui, "Table page size", "Rows per page in the table browser", &mut |ui| {
                    ui.add(egui::DragValue::new(&mut s.page_size).range(10..=10_000).speed(10));
                });
                row(ui, "NULL shown as", "How empty (NULL) cells read in grids", &mut |ui| {
                    ui.add(egui::TextEdit::singleline(&mut s.null_display).desired_width(120.0).char_limit(12));
                });
                row(ui, "Timestamps", "Copy and export always keep the full value", &mut |ui| {
                    ui.checkbox(&mut s.short_timestamps, "Hide fractional seconds in grids");
                });
                ui.add_space(theme::space::SM);
                ui.label(RichText::new("SAFETY").small().strong().color(t.muted_fg));
                row(ui, "Production", "", &mut |ui| {
                    ui.checkbox(&mut s.confirm_production, "Type the name to confirm destructive actions");
                });
                row(ui, "Staging", "Off: Staging connections only show their badge", &mut |ui| {
                    ui.checkbox(&mut s.confirm_staging, "Ask the same way on Staging connections");
                });
                row(ui, "Query timeout", "Cancel a SQL run that takes longer; 0 = never", &mut |ui| {
                    ui.add(egui::DragValue::new(&mut s.query_timeout_secs).range(0..=86_400).speed(1).suffix(" s"));
                });
                ui.add_space(theme::space::SM);
                ui.label(RichText::new("HELP").small().strong().color(t.muted_fg));
                row(ui, "Keyboard shortcuts", &theme::kbd("All shortcuts; also ⌘/ anywhere"), &mut |ui| {
                    if ui.button(ico(ICON_KEYBOARD, "Show shortcuts")).clicked() {
                        open_keys = true;
                    }
                });
                ui.add_space(theme::space::SM);
                ui.separator();
                // Paths wrap inside the dialog instead of widening it.
                ui.add(egui::Label::new(RichText::new(format!("Workspace file: {}", crate::store::config_dir().join("workspace.json").display())).small().color(t.muted_fg)).wrap());
                let logs = crate::store::config_dir().join("logs");
                ui.add(egui::Label::new(RichText::new(format!("Logs: {}", logs.display())).small().color(t.muted_fg)).wrap());
                ui.horizontal(|ui| {
                    if ui.small_button(ico(ICON_FOLDER_OPEN, "Show workspace folder")).clicked() {
                        crate::update::open_url(&crate::store::config_dir().display().to_string());
                    }
                    if ui.small_button(ico(ICON_UPLOAD_FILE, "Export connections…")).on_hover_text("Saves connections (no passwords) to a file").clicked() {
                        export_ws = true;
                    }
                    if ui.small_button(ico(ICON_DOWNLOAD, "Import connections…")).on_hover_text("Adds connections from an exported file").clicked() {
                        import_ws = true;
                    }
                    if ui.small_button(ico(ICON_FOLDER_OPEN, "Open logs folder")).clicked() {
                        // The folder may not exist yet (nothing logged); file managers
                        // then open somewhere unrelated, so create it first.
                        let _ = std::fs::create_dir_all(&logs);
                        crate::update::open_url(&logs.display().to_string());
                    }
                });
            });
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
        });
        if check_now {
            self.cx.check_updates(true);
        }
        if export_ws {
            self.export_workspace();
        }
        if import_ws {
            self.import_workspace();
        }
        if open_keys {
            self.cx.show_shortcuts = true;
            self.cx.show_settings = false;
        }
        if close {
            self.cx.show_settings = false;
        }
        if format!("{:?}", before) != format!("{:?}", self.cx.store.ws.settings) {
            self.cx.mark_dirty();
        }
    }

    /// Zoom and grid display options from Settings. Keyboard zoom (⌘=/⌘−)
    /// changes egui's factor directly; keep the setting in step with it.
    fn apply_display_settings(&mut self, ctx: &egui::Context) {
        let s = &mut self.cx.store.ws.settings;
        let z = ctx.zoom_factor();
        if (z - self.applied_zoom).abs() > 0.001 {
            // Changed with the keyboard.
            s.ui_zoom = z;
            self.applied_zoom = z;
            self.cx.dirty = true;
        } else if (s.ui_zoom - z).abs() > 0.001 {
            let target = s.ui_zoom.clamp(0.5, 2.0);
            ctx.set_zoom_factor(target);
            self.applied_zoom = target;
        }
        crate::widgets::grid::set_display(&s.null_display, s.short_timestamps);
    }

    fn export_workspace(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("JSON", &["json"]).set_file_name("openvclient-connections.json").save_file() else { return };
        match self.cx.store.export_profiles(&path) {
            Ok(n) => self.cx.toasts.info(format!("Exported {n} connections"), format!("{}\nPasswords stay in this computer's keychain.", path.display())),
            Err(e) => self.cx.toasts.error_text("Export failed", e),
        }
    }

    fn import_workspace(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file() else { return };
        match self.cx.store.import_profiles(&path) {
            Ok((added, skipped)) => {
                self.cx.mark_dirty();
                let body = if skipped > 0 { format!("{skipped} already existed and were left as they are.") } else { "Passwords are asked for on first connect.".into() };
                self.cx.toasts.info(format!("Imported {added} connections"), body);
            }
            Err(e) => self.cx.toasts.error_text("Import failed", e),
        }
    }

    fn autosave(&mut self) {
        if self.cx.dirty && self.cx.last_save.elapsed() > Duration::from_millis(800) {
            if let Err(e) = self.cx.store.save() {
                self.cx.toasts.error_text("Could not save workspace", e);
            }
            self.cx.dirty = false;
            self.cx.last_save = Instant::now();
        }
    }
}

/// Light/dark mode; the style (original or cartoon) is `theme::set_cartoon`.
pub fn apply_theme(ctx: &egui::Context, t: ThemeChoice) {
    ctx.set_theme(match t {
        ThemeChoice::System | ThemeChoice::LegacyCartoon => egui::ThemePreference::System,
        ThemeChoice::Dark => egui::ThemePreference::Dark,
        ThemeChoice::Light => egui::ThemePreference::Light,
    });
}

impl OpenvApp {
    fn poll_updates(&mut self, ctx: &egui::Context) {
        let u = &mut self.cx.update;
        if !u.started {
            u.started = true;
            if self.cx.store.ws.settings.check_updates && std::env::var("OPENVCLIENT_NO_UPDATE_CHECK").is_err() {
                self.cx.check_updates(false);
            }
        }
        if let Some(r) = crate::bg::poll(&mut self.cx.update.job) {
            let manual = self.cx.update.manual;
            self.cx.update.last = Some(match &r {
                Ok(Some(rel)) => format!("Version {} is available", rel.version),
                Ok(None) => "You're up to date".into(),
                Err(e) => format!("Could not check: {}", openvclient_core::redact(&e.message)),
            });
            match r {
                Ok(Some(rel)) => {
                    tracing::info!(target: "openvclient", version = %rel.version, "update available");
                    self.cx.update.available = Some(rel);
                    if manual {
                        self.cx.update.show = true;
                    }
                }
                Ok(None) if manual => self.cx.toasts.ok(format!("OpenVClient {} is the latest version", env!("CARGO_PKG_VERSION"))),
                Ok(None) => {}
                Err(e) if manual => self.cx.toasts.error("Update check failed", &e),
                Err(e) => tracing::info!(target: "openvclient", error = %e, "update check failed"),
            }
        }
        if let Some(r) = crate::bg::poll(&mut self.cx.update.download) {
            match r {
                Ok(path) => {
                    self.cx.toasts.sticky("Update downloaded", format!("Saved to {}. Quit OpenVClient and run it to install.", path.display()));
                    if let Some(dir) = path.parent() {
                        crate::update::open_url(&dir.display().to_string());
                    }
                }
                Err(e) => self.cx.toasts.error("Download failed", &e),
            }
        }
        if !self.cx.update.show {
            return;
        }
        let Some(rel) = self.cx.update.available.clone() else { return };
        let mut open = true;
        egui::Modal::new(egui::Id::unique("update_modal")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading(format!("OpenVClient {} is available", rel.version));
            ui.label(RichText::new(format!("You have {}.", env!("CARGO_PKG_VERSION"))).color(theme::muted(ui)));
            ui.add_space(theme::space::SM);
            if !rel.notes.trim().is_empty() {
                egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                    ui.label(&rel.notes);
                });
                ui.add_space(theme::space::SM);
            }
            ui.horizontal(|ui| {
                let downloading = self.cx.update.download.is_some();
                if let Some((name, url)) = rel.asset.clone() {
                    if downloading {
                        ui.spinner();
                        ui.label("Downloading…");
                    } else if theme::primary_button(ui, ico(ICON_DOWNLOAD, &format!("Download {name}"))).clicked() {
                        self.cx.update.download = Some(self.cx.bg.spawn(format!("Downloading {name}"), crate::update::download(name, url)));
                    }
                }
                if ui.button(ico(ICON_OPEN_IN_NEW, "Release page")).clicked() {
                    crate::update::open_url(&rel.page);
                }
                if ui.button("Later").clicked() {
                    open = false;
                }
            });
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                open = false;
            }
        });
        if !open {
            self.cx.update.show = false;
        }
    }
}

impl OpenvApp {
    /// Keep the window usable on small screens. The 1100×700 minimum and the
    /// 1440×900 first-run size do not fit a 1366×768 laptop, or 1920×1080 at
    /// 150 % (1280×720 logical): the window would extend under the taskbar.
    /// There, lower the minimum to what fits and start maximized. Large
    /// monitors keep the normal limits. No-op until the monitor size is known.
    fn fit_window_to_monitor(&mut self, ctx: &egui::Context) {
        let (monitor, inner, maximized) = ctx.input(|i| (i.viewport().monitor_size, i.viewport().inner_rect.map(|r| r.size()), i.viewport().maximized.unwrap_or(false)));
        let Some(monitor) = monitor.filter(|m| m.x > 0.0 && m.y > 0.0) else { return };
        // Room for the title bar, borders and the taskbar or dock.
        let usable = monitor - egui::vec2(16.0, 120.0);
        let min = egui::Vec2::from(crate::MIN_WINDOW).min(usable).max(egui::vec2(800.0, 480.0));
        if min != self.min_window {
            self.min_window = min;
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(min));
        }
        if !self.window_fitted
            && let Some(inner) = inner
        {
            self.window_fitted = true;
            if !maximized && (inner.x > usable.x || inner.y > usable.y) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            }
        }
    }
}

impl eframe::App for OpenvApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        #[cfg(windows)]
        if !self.session_hooked {
            self.session_hooked = true;
            if !crate::winsession::install(_frame) {
                // Headless tests have no native window; a real window always does.
                tracing::debug!(target: "openvclient", "no native window: end-session hook not installed");
            }
        }
        self.fit_window_to_monitor(&ctx);
        self.handle_events();
        self.poll_updates(&ctx);
        self.shortcuts(&ctx);
        self.apply_display_settings(&ctx);

        self.top_bar(ui);
        self.status_bar(ui);
        if !self.cx.sidebar_hidden {
            egui::Panel::left("sidebar").resizable(true).default_size(290.0).size_range(200.0..=560.0).show(ui, |ui| {
                views::sidebar::ui(ui, &mut self.cx);
            });
        }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(ui.visuals().extreme_bg_color)).show(ui, |ui| {
            if self.tree.root.is_none() {
                self.tree = Tree::new_tabs(egui::Id::unique("workspace"), vec![Tab { id: self.cx.tab_id(), kind: TabKind::Home }]);
            }
            let mut viewer = Viewer { cx: &mut self.cx, focused: &mut self.focused };
            self.tree.ui(&mut viewer, ui);
        });
        self.apply_tab_actions();

        if let Some(mut d) = self.cx.conn_dialog.take()
            && d.show(&ctx, &mut self.cx)
        {
            self.cx.conn_dialog = Some(d);
        }
        if let Some(mut d) = self.cx.dialog.take() {
            // Keep the dialog only if it did not hand over to a new one.
            if d.show(&ctx, &mut self.cx) && self.cx.dialog.is_none() {
                self.cx.dialog = Some(d);
            }
        }
        self.password_prompt(&ctx);
        self.host_key_prompt(&ctx);
        self.confirm_dialog(&ctx);
        self.settings_window(&ctx);
        self.about_window(&ctx);
        self.shortcuts_window(&ctx);
        let tabs: Vec<(u64, String)> = self.tree.tiles.tiles().filter_map(|t| if let Tile::Pane(p) = t { Some((p.id, p.title(&self.cx))) } else { None }).collect();
        views::palette::show(&ctx, &mut self.cx, &tabs);
        self.cx.toasts.show(&ctx);

        self.run_commands();
        self.autosave();
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    /// egui's default wheel speed feels fast on macOS trackpads; scale it.
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        let f = self.cx.store.ws.settings.scroll_speed.clamp(0.1, 3.0);
        for e in &mut raw.events {
            if let egui::Event::MouseWheel { delta, .. } = e {
                *delta *= f;
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Remember open tabs (in strip order) for "Pick up where you left off".
        let mut saved: Vec<crate::store::SavedTab> = self.tree.tiles.iter().filter_map(|(_, t)| if let Tile::Pane(p) = t { p.saved() } else { None }).collect();
        // Not yet restored? Keep the earlier offer rather than losing it.
        if saved.is_empty() {
            saved = std::mem::take(&mut self.cx.restore_offer);
        }
        self.cx.store.ws.session = saved;
        if let Err(e) = self.cx.store.save() {
            tracing::error!("saving workspace on exit failed: {e}");
        }
    }
}

/// Drives the real app headlessly against live local services and writes
/// screenshots. Run with: `OPENVCLIENT_SHOTS=dir cargo test -p openvclient-app ui_walkthrough -- --ignored --nocapture`
#[cfg(test)]
mod walkthrough {
    use super::*;
    use egui_kittest::Harness;
    use std::time::{Duration, Instant};

    fn settle(h: &mut Harness<'_, OpenvApp>, until: impl Fn(&OpenvApp) -> bool, max: Duration) -> bool {
        let t = Instant::now();
        while t.elapsed() < max {
            h.step();
            if until(h.state()) {
                // A few more frames so layouts settle after data arrives.
                for _ in 0..4 {
                    h.step();
                }
                return true;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        false
    }

    fn shot(h: &mut Harness<'_, OpenvApp>, dir: &std::path::Path, name: &str) {
        for _ in 0..3 {
            h.step();
        }
        let img = h.render().expect("render");
        let p = dir.join(format!("{name}.png"));
        img.save(&p).expect("save png");
        eprintln!("shot: {}", p.display());
    }

    /// Demo Postgres database; override with OPENVCLIENT_DEMO_DB.
    fn demo_db() -> String {
        std::env::var("OPENVCLIENT_DEMO_DB").unwrap_or_else(|_| "openv_demo".into())
    }

    /// A valid one-page PDF with a title, built with correct xref offsets.
    fn tiny_pdf(title: &str) -> Vec<u8> {
        let stream = format!("BT /F1 28 Tf 60 720 Td ({title}) Tj ET\n0.13 0.77 0.37 rg 60 420 80 200 re f 160 420 80 120 re f 260 420 80 260 re f 360 420 80 160 re f\n");
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
            format!("<< /Length {} >>\nstream\n{stream}endstream", stream.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offs = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offs.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for o in offs {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
        out
    }

    fn id_of(app: &OpenvApp, name: &str) -> Uuid {
        app.cx.store.ws.profiles.iter().find(|p| p.name.starts_with(name)).map(|p| p.id).expect("profile")
    }

    #[test]
    #[ignore = "needs local services and a seeded workspace; writes screenshots"]
    fn ui_walkthrough() {
        let out = std::path::PathBuf::from(std::env::var("OPENVCLIENT_SHOTS").expect("set OPENVCLIENT_SHOTS"));
        std::fs::create_dir_all(&out).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let handle = rt.handle().clone();
        // OPENVCLIENT_SHOTS_SIZE=1100x660 checks layout at a small window (default 1440x900).
        let size = std::env::var("OPENVCLIENT_SHOTS_SIZE")
            .ok()
            .and_then(|s| s.split_once('x').and_then(|(w, h)| Some(egui::vec2(w.trim().parse().ok()?, h.trim().parse().ok()?))))
            .unwrap_or(egui::vec2(1440.0, 900.0));
        let mut h = Harness::builder().with_size(size).with_pixels_per_point(1.5).with_max_steps(64).build_eframe(move |cc| {
            theme::install(&cc.egui_ctx);
            OpenvApp::new(&cc.egui_ctx, handle, crate::registry())
        });
        h.run_ok();
        shot(&mut h, &out, "01-home");

        // Connect everything.
        let ids: Vec<Uuid> = h.state().cx.store.ws.profiles.iter().map(|p| p.id).collect();
        for id in &ids {
            h.state_mut().cx.cmd(Cmd::Connect(*id));
        }
        let ok = settle(&mut h, |a| ids.iter().all(|i| !a.cx.conns.is_connecting(*i) && a.cx.conns.state(*i).is_some()), Duration::from_secs(20));
        let pending: Vec<String> = ids.iter().filter(|i| h.state().cx.conns.is_connecting(**i) || h.state().cx.conns.state(**i).is_none()).map(|i| h.state().cx.profile_name(*i)).collect();
        assert!(ok, "connections did not finish: {pending:?}");
        for id in &ids {
            let name = h.state().cx.profile_name(*id);
            match h.state().cx.conns.state(*id) {
                Some(ConnState::Connected(_)) => eprintln!("connected: {name}"),
                Some(ConnState::Failed(e)) => eprintln!("FAILED: {name}: {e}"),
                _ => eprintln!("??? {name}"),
            }
        }
        shot(&mut h, &out, "02-home-connected");

        // Expand the Postgres tree down to tables.
        let pg = id_of(h.state(), "Postgres");
        {
            let sb = &mut h.state_mut().cx.sidebar;
            sb.expand_path(
                pg,
                &[
                    format!("{:?}|{:?}", ObjectKind::Database, ObjPath::db(demo_db())),
                    format!("{:?}|{:?}", ObjectKind::Schema, ObjPath::db(demo_db()).with_schema("sales")),
                    format!("{:?}|{:?}", ObjectKind::Folder(openvclient_core::sql::FolderKind::Tables), ObjPath::db(demo_db()).with_schema("sales")),
                ],
            );
        }
        settle(&mut h, |a| a.cx.sidebar.loaded_objects().iter().any(|(c, n)| *c == pg && n.label == "orders"), Duration::from_secs(10));

        // SQL editor with a real query, run with ⌘⏎.
        let sql = "-- Revenue by country and tier\nSELECT c.country, c.tier, count(*) AS orders, round(sum(o.amount), 2) AS revenue\nFROM sales.orders o\nJOIN sales.customers c ON c.id = o.customer_id\nWHERE o.status = 'shipped'\nGROUP BY 1, 2\nORDER BY revenue DESC;";
        h.state_mut().cx.cmd(Cmd::NewSql { conn: pg, database: Some(demo_db()), text: sql.into() });
        settle(&mut h, |a| a.cx.conns.is_connected(pg), Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(600));
        for _ in 0..10 {
            h.step();
        }
        h.key_press_modifiers(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Enter);
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(300));
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(5));
        shot(&mut h, &out, "03-sql-results");

        // Explain the same query.
        h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::E);
        std::thread::sleep(Duration::from_millis(400));
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        shot(&mut h, &out, "04-explain");

        // Table data browser.
        h.state_mut().cx.cmd(Cmd::OpenTable { conn: pg, kind: ObjectKind::Table, path: ObjPath::db(demo_db()).with_schema("sales").with_name("orders") });
        std::thread::sleep(Duration::from_millis(300));
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        shot(&mut h, &out, "05-table");
        {
            let app = h.state_mut();
            if let Some(Tile::Pane(Tab { kind: TabKind::Table(t), .. })) = app.focused.and_then(|f| app.tree.tiles.get_mut(f)) {
                t.grid.frozen = 2;
            }
        }
        shot(&mut h, &out, "05b-frozen-columns");
        {
            let app = h.state_mut();
            if let Some(Tile::Pane(Tab { kind: TabKind::Table(t), .. })) = app.focused.and_then(|f| app.tree.tiles.get_mut(f)) {
                t.grid.frozen = 0;
            }
        }

        // Monitoring.
        h.state_mut().cx.cmd(Cmd::OpenMonitor(pg));
        std::thread::sleep(Duration::from_millis(500));
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(300));
        for _ in 0..6 {
            h.step();
        }
        shot(&mut h, &out, "06-monitor");

        // Redis.
        let rd = id_of(h.state(), "Redis");
        h.state_mut().cx.cmd(Cmd::OpenRedis { conn: rd, db: 0 });
        std::thread::sleep(Duration::from_millis(400));
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        shot(&mut h, &out, "07-redis");

        // Docker.
        let dk = id_of(h.state(), "Docker");
        h.state_mut().cx.cmd(Cmd::OpenDocker { conn: dk, section: views::docker::Section::Containers });
        std::thread::sleep(Duration::from_millis(600));
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        shot(&mut h, &out, "08-docker");

        // SQLite table, light theme.
        let sl = id_of(h.state(), "Shop");
        h.state_mut().cx.store.ws.settings.theme = ThemeChoice::Light;
        apply_theme(&h.ctx, ThemeChoice::Light);
        h.state_mut().cx.cmd(Cmd::OpenTable { conn: sl, kind: ObjectKind::View, path: ObjPath::default().with_schema("main").with_name("revenue_by_country") });
        std::thread::sleep(Duration::from_millis(300));
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        shot(&mut h, &out, "09-sqlite-light");

        // Connection dialog and global search.
        apply_theme(&h.ctx, ThemeChoice::Dark);
        h.state_mut().cx.cmd(Cmd::EditConnection(Some(pg)));
        for _ in 0..4 {
            h.step();
        }
        shot(&mut h, &out, "10-connection-dialog");
        h.state_mut().cx.conn_dialog = None;
        h.state_mut().cx.palette.open();
        h.state_mut().cx.palette.set_query("ord");
        for _ in 0..4 {
            h.step();
        }
        shot(&mut h, &out, "11-search");
        h.state_mut().cx.palette = Default::default();
        h.state_mut().cx.cmd(Cmd::EditConnection(None));
        for _ in 0..4 {
            h.step();
        }
        shot(&mut h, &out, "12-picker");
        h.state_mut().cx.conn_dialog = None;
        h.state_mut().cx.show_settings = true;
        for _ in 0..4 {
            h.step();
        }
        shot(&mut h, &out, "13-settings");
        h.state_mut().cx.show_settings = false;
        h.state_mut().cx.show_about = true;
        shot(&mut h, &out, "13b-about");
        h.state_mut().cx.show_about = false;
        h.state_mut().cx.show_settings = false;

        // New features: ER diagram, schema compare, chart, formatter, session restore.
        h.state_mut().cx.cmd(Cmd::OpenErd { conn: pg, database: Some(demo_db()), schema: "sales".into() });
        for _ in 0..4 {
            h.step();
        }
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        shot(&mut h, &out, "15-erd");
        h.state_mut().cx.cmd(Cmd::OpenCompare { conn: pg, database: Some(demo_db()), schema: "sales".into() });
        for _ in 0..3 {
            h.step();
        }
        {
            let app = h.state_mut();
            if let Some(tid) = app.find(|t| matches!(t.kind, TabKind::Compare(_)))
                && let Some(Tile::Pane(Tab { kind: TabKind::Compare(c), .. })) = app.tree.tiles.get_mut(tid)
            {
                c.right.schema = "public".into();
                c.start(&mut app.cx);
            }
        }
        for _ in 0..4 {
            h.step();
        }
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        shot(&mut h, &out, "16-compare");
        let messy = "select c.country, count(*) as orders, round(sum(o.amount),2) as revenue from sales.orders o join sales.customers c on c.id=o.customer_id where o.status='shipped' and o.amount>10 group by 1 order by revenue desc;";
        h.state_mut().cx.cmd(Cmd::NewSql { conn: pg, database: Some(demo_db()), text: messy.into() });
        std::thread::sleep(Duration::from_millis(600));
        for _ in 0..10 {
            h.step();
        }
        {
            let app = h.state_mut();
            if let Some(Tile::Pane(Tab { kind: TabKind::Sql(t), .. })) = app.focused.and_then(|f| app.tree.tiles.get_mut(f)) {
                t.test_format_and_chart(&mut app.cx, true, false);
            }
        }
        h.key_press_modifiers(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Enter);
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        shot(&mut h, &out, "17-formatted");
        {
            let app = h.state_mut();
            if let Some(Tile::Pane(Tab { kind: TabKind::Sql(t), .. })) = app.focused.and_then(|f| app.tree.tiles.get_mut(f)) {
                t.test_format_and_chart(&mut app.cx, false, true);
            }
        }
        shot(&mut h, &out, "18-chart");
        {
            let app = h.state_mut();
            app.cx.restore_offer = vec![crate::store::SavedTab::Sql { conn: pg, database: Some(demo_db()), text: "SELECT 1".into() }, crate::store::SavedTab::Monitor { conn: pg }];
        }
        apply_theme(&h.ctx, ThemeChoice::Light);
        h.state_mut().cx.cmd(Cmd::OpenHome);
        for _ in 0..6 {
            h.step();
        }
        shot(&mut h, &out, "14-home-light");
        h.state_mut().cx.store.ws.settings.home_by_folder = true;
        for _ in 0..4 {
            h.step();
        }
        shot(&mut h, &out, "14b-home-by-folder");
        h.state_mut().cx.store.ws.settings.home_by_folder = false;
        drop(h);
        rt.shutdown_timeout(Duration::from_secs(1));
    }

    /// File-manager screenshots against the Azurite emulator (127.0.0.1:50000).
    /// Run with OPENVCLIENT_CONFIG_DIR=<empty dir> OPENVCLIENT_SHOTS=<dir>
    /// cargo test -p openvclient-app files_walkthrough -- --ignored --nocapture
    #[test]
    #[ignore]
    fn files_walkthrough() {
        use openvclient_core::resource::{Args, Node, NodeIcon};
        let out = std::path::PathBuf::from(std::env::var("OPENVCLIENT_SHOTS").expect("set OPENVCLIENT_SHOTS"));
        std::fs::create_dir_all(&out).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let handle = rt.handle().clone();
        let mut h = Harness::builder().with_size(egui::vec2(1440.0, 900.0)).with_pixels_per_point(1.5).with_max_steps(64).build_eframe(move |cc| {
            theme::install(&cc.egui_ctx);
            OpenvApp::new(&cc.egui_ctx, handle, crate::registry())
        });
        let mut p = openvclient_core::ConnectionProfile::new("azure_blob", "Acme Analytics");
        p.params.insert("auth".into(), "azurite".into());
        p.params.insert("azurite_url".into(), "http://127.0.0.1:50000".into());
        p.environment = openvclient_core::Environment::Production;
        let id = p.id;
        h.state_mut().cx.store.ws.profiles.push(p);
        h.state_mut().cx.cmd(Cmd::Connect(id));
        settle(&mut h, |a| a.cx.conns.is_connected(id), Duration::from_secs(15));
        let s = h.state().cx.conns.session(id).and_then(|x| x.resource()).cloned().expect("azurite session");
        // Opening a connected storage connection (home card click, sidebar
        // double-click) must open the file manager, not the Docker view.
        {
            let app = h.state_mut();
            let p = app.cx.store.profile(id).cloned().unwrap();
            crate::views::open_connection(&mut app.cx, &p);
        }
        for _ in 0..4 {
            h.step();
        }
        assert!(h.state().find(|t| matches!(t.kind, TabKind::Files(_))).is_some(), "storage connection should open the file manager");
        assert!(h.state().find(|t| matches!(t.kind, TabKind::Docker(..))).is_none(), "storage connection must not open the Docker view");

        // Seed: a container with folders and a few typical files.
        let dir = std::env::temp_dir().join("ovc-files-shots");
        std::fs::create_dir_all(&dir).unwrap();
        let files =
            [("alert_dump_01.00.parquet", 1_089_246), ("asset_dump_01.00.parquet", 742_311), ("summary.json", 2_048), ("readme.txt", 512), ("export.csv", 96_000), ("chart.png", 0), ("report.pdf", 0)];
        rt.block_on(async {
            let mut a = Args::new();
            a.insert("name".into(), "analytics-exports".into());
            let _ = s.run_action("", "create_container", a).await;
            for folder in ["20260224124307", "20260301081500", "archive"] {
                let mut a = Args::new();
                a.insert("name".into(), folder.into());
                let _ = s.run_action("c/analytics-exports", "mkdir", a).await;
            }
            for (name, size) in files {
                let path = dir.join(name);
                let body = if name.ends_with(".json") {
                    r#"{"alerts": 128, "assets": 3402, "generated": "2026-02-24T12:43:17Z"}"#.as_bytes().to_vec()
                } else if name.ends_with(".txt") {
                    b"Daily export from the analytics pipeline.\n".to_vec()
                } else if name.ends_with(".pdf") {
                    tiny_pdf("Monthly analytics report")
                } else if name.ends_with(".png") {
                    // A small generated chart, so the image preview has something to show.
                    let img = image::RgbaImage::from_fn(320, 180, |x, y| {
                        let bar = x / 40;
                        let h = 40 + (bar * 37) % 130;
                        if 180 - y < h && x % 40 > 6 { image::Rgba([34, 197, 94, 255]) } else { image::Rgba([241, 245, 249, 255]) }
                    });
                    let mut buf = std::io::Cursor::new(Vec::new());
                    img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
                    buf.into_inner()
                } else {
                    vec![b'x'; size]
                };
                std::fs::write(&path, body).unwrap();
                let mut a = Args::new();
                a.insert("file".into(), path.display().to_string());
                let _ = s.run_action("p/analytics-exports/20260224124307/", "upload", a).await;
            }
        });

        // The sidebar loaded before seeding; reload it.
        h.state_mut().cx.sidebar.forget(id);
        // Top level, then inside a folder, then a file selected with preview.
        h.state_mut().cx.cmd(Cmd::OpenResource { conn: id, node: None, action: None });
        for _ in 0..4 {
            h.step(); // let the tab start its load before waiting for it
        }
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        shot(&mut h, &out, "f1-files-root");
        let folder = Node::new("p/analytics-exports/20260224124307/", "20260224124307", "prefix", NodeIcon::Folder).folder();
        h.state_mut().cx.cmd(Cmd::OpenResource { conn: id, node: Some(folder), action: None });
        for _ in 0..4 {
            h.step(); // let the tab start its load before waiting for it
        }
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        shot(&mut h, &out, "f2-files-folder");
        {
            let app = h.state_mut();
            if let Some(tid) = app.find(|t| matches!(t.kind, TabKind::Files(_)))
                && let Some(Tile::Pane(Tab { kind: TabKind::Files(f), .. })) = app.tree.tiles.get_mut(tid)
            {
                f.click_name(&app.cx, "summary.json");
            }
        }
        for _ in 0..4 {
            h.step(); // let the tab start its load before waiting for it
        }
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        shot(&mut h, &out, "f3-files-preview");
        {
            let app = h.state_mut();
            if let Some(tid) = app.find(|t| matches!(t.kind, TabKind::Files(_)))
                && let Some(Tile::Pane(Tab { kind: TabKind::Files(f), .. })) = app.tree.tiles.get_mut(tid)
            {
                f.click_name(&app.cx, "chart.png");
            }
        }
        for _ in 0..4 {
            h.step();
        }
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        shot(&mut h, &out, "f3b-image-preview");
        {
            let app = h.state_mut();
            if let Some(tid) = app.find(|t| matches!(t.kind, TabKind::Files(_)))
                && let Some(Tile::Pane(Tab { kind: TabKind::Files(f), .. })) = app.tree.tiles.get_mut(tid)
            {
                f.click_name(&app.cx, "report.pdf");
            }
        }
        for _ in 0..4 {
            h.step();
        }
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(15));
        for _ in 0..4 {
            h.step();
        }
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(15));
        shot(&mut h, &out, "f3c-pdf-preview");
        h.state_mut().cx.store.ws.settings.theme = crate::store::ThemeChoice::Light;
        apply_theme(&h.ctx, crate::store::ThemeChoice::Light);
        shot(&mut h, &out, "f4-files-light");
        h.state_mut().cx.show_about = true;
        shot(&mut h, &out, "f5-about");
        h.state_mut().cx.show_about = false;
        theme::set_cartoon(&h.ctx, true);
        shot(&mut h, &out, "f6-files-cartoon-day");
        h.ctx.set_theme(egui::Theme::Dark);
        shot(&mut h, &out, "f7-files-cartoon-night");
        let up = Node::new("c/analytics-exports", "analytics-exports", "container", NodeIcon::Bucket).folder();
        h.state_mut().cx.cmd(Cmd::OpenResource { conn: id, node: Some(up), action: None });
        for _ in 0..4 {
            h.step();
        }
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        shot(&mut h, &out, "f8-folders-cartoon-night");
        drop(h);
        rt.shutdown_timeout(Duration::from_secs(1));
    }

    /// Cartoon theme screenshots on a scratch workspace (no real connections):
    /// `OPENVCLIENT_CONFIG_DIR=<empty dir> OPENVCLIENT_SHOTS=<out> cargo test -p openvclient-app cartoon_walkthrough -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn cartoon_walkthrough() {
        use openvclient_core::{ConnectionProfile, Environment};
        let out = std::path::PathBuf::from(std::env::var("OPENVCLIENT_SHOTS").expect("set OPENVCLIENT_SHOTS"));
        std::fs::create_dir_all(&out).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let handle = rt.handle().clone();
        let mut h = Harness::builder().with_size(egui::vec2(1440.0, 900.0)).with_pixels_per_point(1.5).with_max_steps(64).build_eframe(move |cc| {
            theme::install(&cc.egui_ctx);
            OpenvApp::new(&cc.egui_ctx, handle, crate::registry())
        });
        let db = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/shop.db").canonicalize().expect("demo/shop.db");
        let mut shop = ConnectionProfile::new("sqlite", "Shop demo");
        shop.params.insert("path".into(), db.display().to_string());
        let shop_id = shop.id;
        let mut others = Vec::new();
        for (driver, name, env) in [
            ("postgres", "Orders (staging)", Environment::Staging),
            ("redis", "Session cache", Environment::Development),
            ("s3", "Media bucket", Environment::Production),
            ("mongodb", "Events", Environment::Testing),
            ("ssh", "bastion-01", Environment::Production),
        ] {
            let mut p = ConnectionProfile::new(driver, name);
            p.environment = env;
            others.push(p);
        }
        {
            let app = h.state_mut();
            app.cx.store.ws.profiles.push(shop);
            let pinned = others[0].id;
            app.cx.store.ws.profiles.extend(others);
            app.cx.store.toggle_favorite(shop_id);
            app.cx.store.toggle_favorite(pinned);
            app.cx.store.ws.settings.style = ThemeStyle::Cartoon;
        }
        h.ctx.set_theme(egui::Theme::Light);
        theme::set_cartoon(&h.ctx, true);
        shot(&mut h, &out, "c1-home-day");
        h.state_mut().cx.cmd(Cmd::Connect(shop_id));
        settle(&mut h, |a| a.cx.conns.is_connected(shop_id), Duration::from_secs(10));
        let sql = "-- Top customers\nSELECT c.name, c.country, count(*) AS orders, round(sum(o.total), 2) AS revenue\nFROM orders o JOIN customers c ON c.id = o.customer_id\nGROUP BY 1, 2 ORDER BY revenue DESC LIMIT 20;";
        h.state_mut().cx.cmd(Cmd::NewSql { conn: shop_id, database: None, text: sql.into() });
        std::thread::sleep(Duration::from_millis(400));
        for _ in 0..10 {
            h.step();
        }
        h.key_press_modifiers(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Enter);
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        shot(&mut h, &out, "c2-sql-day");
        h.ctx.set_theme(egui::Theme::Dark);
        shot(&mut h, &out, "c3-sql-night");
        {
            let cx = &h.state().cx;
            let d = views::conn_dialog::ConnDialog::new(cx, None, None);
            h.state_mut().cx.conn_dialog = Some(d);
        }
        shot(&mut h, &out, "c4-picker-night");
        h.ctx.set_theme(egui::Theme::Light);
        shot(&mut h, &out, "c5-picker-day");
        h.state_mut().cx.conn_dialog = None;
        h.state_mut().cx.show_settings = true;
        shot(&mut h, &out, "c6-settings-day");
        h.ctx.set_theme(egui::Theme::Dark);
        shot(&mut h, &out, "c6-settings-night");
        h.ctx.set_theme(egui::Theme::Light);
        h.state_mut().cx.show_settings = false;
        h.state_mut().cx.show_shortcuts = true;
        shot(&mut h, &out, "c7-shortcuts-day");
        drop(h);
        rt.shutdown_timeout(Duration::from_secs(1));
    }

    /// README screenshots on neutral demo data (scratch workspace, demo SQLite
    /// copied to /tmp/openvclient-demo so no personal paths show):
    /// `OPENVCLIENT_CONFIG_DIR=<empty dir> OPENVCLIENT_SHOTS=<out> cargo test -p openvclient-app readme_screenshots -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn readme_screenshots() {
        use openvclient_core::sql::{ObjPath, ObjectKind};
        use openvclient_core::{ConnectionProfile, Environment};
        let out = std::path::PathBuf::from(std::env::var("OPENVCLIENT_SHOTS").expect("set OPENVCLIENT_SHOTS"));
        std::fs::create_dir_all(&out).unwrap();
        let demo = std::path::Path::new("/tmp/openvclient-demo");
        std::fs::create_dir_all(demo).unwrap();
        let db = demo.join("shop.db");
        std::fs::copy(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/shop.db"), &db).expect("demo/shop.db");
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let handle = rt.handle().clone();
        let mut h = Harness::builder().with_size(egui::vec2(1440.0, 900.0)).with_pixels_per_point(1.5).with_max_steps(64).build_eframe(move |cc| {
            theme::install(&cc.egui_ctx);
            OpenvApp::new(&cc.egui_ctx, handle, crate::registry())
        });
        let mut shop = ConnectionProfile::new("sqlite", "Shop (SQLite demo)");
        shop.params.insert("path".into(), db.display().to_string());
        shop.folder = "Demo".into();
        let shop_id = shop.id;
        let mut all = vec![shop];
        for (driver, name, env, folder) in [
            ("postgres", "Orders DB", Environment::Staging, "Demo"),
            ("mysql", "Billing", Environment::Production, "Demo"),
            ("redis", "Session cache", Environment::Development, "Demo"),
            ("mongodb", "Events", Environment::Testing, "Demo"),
            ("s3", "Media bucket", Environment::Production, "Cloud"),
            ("azure_blob", "Analytics exports", Environment::Production, "Cloud"),
            ("ssh", "bastion-01", Environment::Production, "Servers"),
            ("docker", "Local Docker", Environment::Development, "Servers"),
        ] {
            let mut p = ConnectionProfile::new(driver, name);
            p.environment = env;
            p.folder = folder.into();
            match driver {
                "postgres" => {
                    p.params.insert("host".into(), "db.staging.example.com".into());
                    p.params.insert("user".into(), "app".into());
                    p.params.insert("database".into(), "orders".into());
                }
                "mysql" => {
                    p.params.insert("host".into(), "billing.example.com".into());
                    p.params.insert("user".into(), "report".into());
                }
                "redis" => {
                    p.params.insert("host".into(), "cache.example.com".into());
                }
                "mongodb" => {
                    p.params.insert("host".into(), "events.example.com".into());
                }
                "ssh" => {
                    p.params.insert("host".into(), "bastion.example.com".into());
                    p.params.insert("user".into(), "ops".into());
                }
                _ => {}
            }
            all.push(p);
        }
        let pinned = [all[0].id, all[1].id, all[6].id];
        {
            let app = h.state_mut();
            app.cx.store.ws.profiles = all;
            for id in pinned {
                app.cx.store.toggle_favorite(id);
            }
        }
        let style = |h: &mut Harness<'_, OpenvApp>, cartoon: bool, dark: bool| {
            theme::set_cartoon(&h.ctx, cartoon);
            h.ctx.set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
        };

        style(&mut h, false, true);
        shot(&mut h, &out, "home-original-dark");
        style(&mut h, true, false);
        shot(&mut h, &out, "home-cartoon-light");

        h.state_mut().cx.cmd(Cmd::Connect(shop_id));
        settle(&mut h, |a| a.cx.conns.is_connected(shop_id), Duration::from_secs(10));
        let sql = "-- Revenue by country, shipped orders only\nSELECT c.country,\n       count(*)               AS orders,\n       round(sum(o.total), 2) AS revenue,\n       round(avg(o.total), 2) AS avg_order\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'shipped'\nGROUP BY c.country\nORDER BY revenue DESC;";
        h.state_mut().cx.cmd(Cmd::NewSql { conn: shop_id, database: None, text: sql.into() });
        for _ in 0..10 {
            h.step();
        }
        h.key_press_modifiers(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Enter);
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        style(&mut h, false, true);
        shot(&mut h, &out, "sql-original-dark");
        style(&mut h, false, false);
        shot(&mut h, &out, "sql-original-light");

        h.state_mut().cx.cmd(Cmd::OpenTable { conn: shop_id, kind: ObjectKind::Table, path: ObjPath::default().with_schema("main").with_name("orders") });
        for _ in 0..4 {
            h.step();
        }
        settle(&mut h, |a| a.cx.bg.running().is_empty(), Duration::from_secs(10));
        style(&mut h, true, true);
        shot(&mut h, &out, "table-cartoon-dark");

        {
            let cx = &h.state().cx;
            let d = views::conn_dialog::ConnDialog::new(cx, None, None);
            h.state_mut().cx.conn_dialog = Some(d);
        }
        style(&mut h, true, false);
        shot(&mut h, &out, "picker-cartoon-light");
        h.state_mut().cx.conn_dialog = None;
        h.state_mut().cx.show_settings = true;
        style(&mut h, false, true);
        shot(&mut h, &out, "settings-original-dark");
        h.state_mut().cx.show_settings = false;
        h.state_mut().cx.show_shortcuts = true;
        style(&mut h, true, false);
        shot(&mut h, &out, "shortcuts-cartoon-light");
        drop(h);
        rt.shutdown_timeout(Duration::from_secs(1));
    }
}
