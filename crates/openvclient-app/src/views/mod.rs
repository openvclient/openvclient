pub mod compare;
pub mod conn_dialog;
pub mod dialogs;
pub mod docker;
pub mod erd;
pub mod files;
pub mod home;
pub mod monitor;
pub mod object;
pub mod palette;
pub mod redis;
pub mod resource;
pub mod sidebar;
pub mod sql;
pub mod table;
pub mod terminal;
pub mod users;

use egui::RichText;
use egui_material_icons::MaterialIcon;
use egui_material_icons::icons::*;
use openvclient_core::sql::{FolderKind, ObjectKind};

use crate::theme;

pub fn kind_icon(k: ObjectKind) -> MaterialIcon {
    match k {
        ObjectKind::Database => ICON_DATABASE,
        ObjectKind::Schema => ICON_SCHEMA,
        ObjectKind::Folder(FolderKind::Columns) => ICON_VIEW_COLUMN,
        ObjectKind::Folder(FolderKind::Indexes) => ICON_BOLT,
        ObjectKind::Folder(FolderKind::Constraints) => ICON_KEY,
        ObjectKind::Folder(_) => ICON_FOLDER,
        ObjectKind::Table => ICON_TABLE,
        ObjectKind::View | ObjectKind::MaterializedView => ICON_TABLE_VIEW,
        ObjectKind::ForeignTable => ICON_LINK,
        ObjectKind::Function | ObjectKind::Procedure => ICON_FUNCTIONS,
        ObjectKind::Sequence => ICON_FORMAT_LIST_NUMBERED,
        ObjectKind::Trigger | ObjectKind::Event => ICON_SCHEDULE,
        ObjectKind::Column => ICON_VIEW_COLUMN,
        ObjectKind::Index => ICON_BOLT,
        ObjectKind::Constraint => ICON_KEY,
    }
}

pub fn driver_icon(driver: &str) -> MaterialIcon {
    match driver {
        "redis" => ICON_KEY,
        "docker" => ICON_DEPLOYED_CODE,
        "sqlite" => ICON_STORAGE,
        "mongodb" => ICON_DATA_OBJECT,
        "elasticsearch" => ICON_MANAGE_SEARCH,
        "s3" | "b2" | "idrive_e2" | "wasabi" | "r2" | "do_spaces" | "gcs" | "oci" | "linode" | "scaleway" | "hetzner" | "storj" | "minio" | "alibaba_oss" => ICON_CLOUD,
        "azure_blob" => ICON_CLOUD_UPLOAD,
        "ftp" => ICON_FOLDER_SHARED,
        "ssh" => ICON_TERMINAL,
        "kafka" => ICON_STREAM,
        "rabbitmq" => ICON_QUEUE,
        "cassandra" => ICON_VIEW_COLUMN,
        "clickhouse" => ICON_BAR_CHART,
        "sqlserver" => ICON_DNS,
        "oracle" | "db2" | "jdbc" => ICON_DATABASE,
        _ => ICON_DATABASE,
    }
}

/// Connector logo: the material icon in the original themes, a cartoon
/// sticker in the cartoon theme. Decorative: the connector name is always next to it.
pub fn driver_glyph(ui: &mut egui::Ui, driver: &str, size: f32, color: egui::Color32) -> egui::Response {
    if theme::is_cartoon() {
        let s = size + 6.0;
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(s, s), egui::Sense::hover());
        theme::paint_connector_sticker(ui.painter(), rect.center(), s, driver, ui.visuals().dark_mode);
        resp
    } else {
        ui.label(RichText::new(String::from(driver_icon(driver))).size(size).color(color))
    }
}

/// An item icon: the line icon in the original themes, its cartoon drawing in
/// the cartoon theme.
pub fn item_glyph(ui: &mut egui::Ui, icon: MaterialIcon, size: f32, color: egui::Color32) -> egui::Response {
    if theme::is_cartoon() {
        let s = size + 2.0;
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(s, s), egui::Sense::hover());
        theme::paint_cartoon_icon(ui.painter(), rect.center(), s, icon, ui.visuals().dark_mode);
        resp
    } else {
        ui.label(RichText::new(String::from(icon)).size(size).color(color))
    }
}

/// Open a connected connection's main view: the SQL editor for databases, the
/// key browser for Redis, the file manager or service view for storage and
/// services, the containers list for Docker.
pub fn open_connection(cx: &mut crate::app::AppCtx, p: &openvclient_core::ConnectionProfile) {
    use crate::app::Cmd;
    use openvclient_core::SessionHandle;
    let id = p.id;
    let cmd = match cx.conns.session(id) {
        Some(SessionHandle::Sql(_)) => Cmd::NewSql { conn: id, database: None, text: String::new() },
        Some(SessionHandle::KeyValue(_)) => Cmd::OpenRedis { conn: id, db: p.param("db").parse().unwrap_or(0) },
        Some(SessionHandle::Containers(_)) => Cmd::OpenDocker { conn: id, section: crate::views::docker::Section::Containers },
        Some(SessionHandle::Resource(_)) => Cmd::OpenResource { conn: id, node: None, action: None },
        None => Cmd::Connect(id),
    };
    cx.cmd(cmd);
}

/// Every global and editor shortcut, as shown in the Keyboard shortcuts window.
pub const SHORTCUTS: &[(&str, &str, &str)] = &[
    ("General", "⌘K", "Search everything"),
    ("General", "⌘N", "New connection"),
    ("General", "⌘⇧H", "Home"),
    ("General", "⌘W", "Close tab"),
    ("General", "⌘⇧T", "Reopen closed tab"),
    ("General", "⌘B", "Hide or show the sidebar"),
    ("General", "⌘1 … ⌘9", "Switch to tab 1–8, or the last tab"),
    ("General", "F5", "Refresh the current view"),
    ("General", "⌘/", "Keyboard shortcuts"),
    ("SQL editor", "⌘T", "New SQL editor for the focused connection"),
    ("SQL editor", "⌘⏎", "Run statement at cursor (or selection)"),
    ("SQL editor", "⌘⇧⏎", "Run whole script"),
    ("SQL editor", "⌘E", "Explain statement"),
    ("SQL editor", "⌘.", "Cancel running query"),
    ("SQL editor", "⌘S", "Save SQL to file"),
    ("SQL editor", "⌘⇧F", "Format SQL (or the selection)"),
    ("SQL editor", "⌃Space", "Autocomplete"),
];

/// Shortcut list grouped by area.
pub fn shortcuts_grid(ui: &mut egui::Ui) {
    let t = theme::tokens(ui);
    let mut group = "";
    egui::Grid::new("keys").num_columns(2).spacing([24.0, 6.0]).striped(false).show(ui, |ui| {
        for (g, k, what) in SHORTCUTS {
            if *g != group {
                group = g;
                ui.label(RichText::new(g.to_uppercase()).small().strong().color(t.muted_fg));
                ui.end_row();
            }
            ui.label(RichText::new(theme::kbd(k)).monospace().strong());
            ui.label(RichText::new(*what).color(t.fg));
            ui.end_row();
        }
    });
    ui.add_space(theme::space::SM);
    ui.label(RichText::new("Drag a tab to the edge of another to split the workspace.").small().color(t.muted_fg));
}

/// Human name of a connector, e.g. "PostgreSQL".
pub fn driver_label(cx: &crate::app::AppCtx, driver: &str) -> String {
    cx.conns.registry.get(driver).map(|d| d.info().name.to_string()).unwrap_or_else(|| driver.to_string())
}

/// Heading with icon used at the top of tab bodies.
pub fn header(ui: &mut egui::Ui, icon: MaterialIcon, title: &str, subtitle: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(String::from(icon)).size(theme::icon::LG).color(theme::tokens(ui).accent));
        ui.label(RichText::new(title).size(16.0).strong());
        if !subtitle.is_empty() {
            ui.label(RichText::new(subtitle).color(theme::muted(ui)));
        }
    });
}

/// Error block shown inline, next to what failed.
pub fn error_box(ui: &mut egui::Ui, e: &openvclient_core::Error) {
    let t = theme::tokens(ui);
    // Never wider than the space it is given: long driver messages wrap, and a
    // long detail scrolls inside the box instead of growing the window (a
    // failed connection test used to push the dialog off-screen).
    let width = ui.available_width();
    egui::Frame::new().fill(t.destructive.gamma_multiply(0.12)).stroke(egui::Stroke::new(1.0, t.destructive.gamma_multiply(0.5))).corner_radius(6).inner_margin(10).show(ui, |ui| {
        ui.set_max_width(width - 22.0);
        ui.horizontal_top(|ui| {
            ui.label(RichText::new(String::from(ICON_ERROR)).color(t.destructive));
            let copy_w = 28.0;
            ui.allocate_ui_with_layout(egui::vec2((ui.available_width() - copy_w).max(80.0), 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.add(egui::Label::new(RichText::new(&e.message).color(t.destructive).strong()).wrap());
            });
            if ui.small_button(String::from(ICON_CONTENT_COPY)).on_hover_text("Copy diagnostics").clicked() {
                ui.ctx().copy_text(format!("OpenVClient {}\n{}", env!("CARGO_PKG_VERSION"), e.diagnostics()));
            }
        });
        if let Some(d) = &e.detail {
            egui::ScrollArea::vertical().id_salt(("error_detail", &e.message)).max_height(140.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(d).monospace().small()).wrap());
            });
        }
        if let Some(h) = e.hint() {
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(format!("{}  {h}", String::from(ICON_LIGHTBULB))).color(t.fg)).wrap());
        }
    });
}

/// "Not available for this engine" note (never a fake control).
pub fn unsupported(ui: &mut egui::Ui, what: &str) {
    ui.add_space(theme::space::XL);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(String::from(ICON_INFO)).size(28.0).color(theme::muted(ui)));
        ui.label(RichText::new(what).color(theme::muted(ui)));
    });
}

pub fn busy(ui: &mut egui::Ui, label: &str) {
    ui.horizontal(|ui| {
        ui.add(egui::Spinner::new().size(14.0));
        ui.label(RichText::new(label).color(theme::muted(ui)));
    });
}

/// What an environment badge changes, for its tooltip.
pub fn env_tip(e: openvclient_core::Environment) -> &'static str {
    use openvclient_core::Environment::*;
    match e {
        Development => "Development: no extra safety prompts",
        Testing => "Testing: no extra safety prompts",
        Staging => "Staging: amber badge; type-to-confirm when enabled in Settings → Safety",
        Production => "Production: red tab bar, and destructive actions need the name typed to confirm",
    }
}
