//! New/edit connection: pick a connector, fill its own fields, test, save.

use std::time::Instant;

use egui::RichText;
use egui_material_icons::icons::*;
use openvclient_core::{ConnectionProfile, Environment, Error, FieldKind, Result};

use crate::app::{AppCtx, Cmd};
use crate::bg::{Job, poll};
use crate::store::secrets;
use crate::theme::{self, ico};

pub struct ConnDialog {
    p: ConnectionProfile,
    is_new: bool,
    picking: bool,
    secret: String,
    /// Existing profile already has a keychain secret; empty field keeps it.
    has_saved_secret: bool,
    show_advanced: bool,
    test: Option<Job<Result<String>>>,
    test_result: Option<std::result::Result<String, Error>>,
    error: Option<String>,
    tab: FormTab,
    picker_filter: String,
    picker_focus: bool,
    /// Network-path secrets typed in this dialog; empty keeps the saved one.
    ssh_password: String,
    ssh_passphrase: String,
    proxy_password: String,
    /// "Paste a connection URL" box (new connections only).
    url: String,
    /// Footer Cancel was pressed.
    cancelled: bool,
    /// A new test result arrived: scroll it into view once.
    reveal_result: bool,
}

/// A connection URL split into the form's fields.
#[derive(Debug, Default, PartialEq)]
pub struct ParsedUrl {
    pub driver: &'static str,
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    pub password: String,
    pub database: String,
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse `scheme://user:pass@host:port/db?…` for the common engines.
/// Mongo URLs are kept whole (the driver accepts the URI itself).
pub fn parse_connection_url(url: &str) -> Option<ParsedUrl> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    let driver = match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => "postgres",
        "mysql" => "mysql",
        "mariadb" => "mariadb",
        "sqlserver" | "mssql" => "sqlserver",
        "redis" | "rediss" => "redis",
        "mongodb" | "mongodb+srv" => "mongodb",
        "clickhouse" => "clickhouse",
        _ => return None,
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (auth, hostpath) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, rest),
    };
    let (hostport, path) = hostpath.split_once('/').unwrap_or((hostpath, ""));
    // Only the first host of a multi-host list fills the form.
    let hostport = hostport.split(',').next().unwrap_or("");
    let (host, port) = if let Some(h) = hostport.strip_prefix('[') {
        let (h, p) = h.split_once(']')?;
        (h.to_string(), p.strip_prefix(':').and_then(|p| p.parse().ok()))
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()),
            None => (hostport.to_string(), None),
        }
    };
    let (user, password) = match auth {
        Some(a) => match a.split_once(':') {
            Some((u, p)) => (pct_decode(u), pct_decode(p)),
            None => (pct_decode(a), String::new()),
        },
        None => (String::new(), String::new()),
    };
    if host.is_empty() {
        return None;
    }
    Some(ParsedUrl { driver, host: pct_decode(&host), port, user, password, database: pct_decode(path.trim_matches('/')) })
}

#[derive(Clone, Copy, PartialEq)]
enum FormTab {
    Main,
    Network,
}

impl ConnDialog {
    pub fn new(cx: &AppCtx, p: Option<ConnectionProfile>, folder: Option<String>) -> Self {
        match p {
            Some(p) => Self::edit(p),
            None => {
                let mut p = ConnectionProfile::new("", "");
                p.folder = folder.unwrap_or_default();
                let _ = cx;
                Self {
                    p,
                    is_new: true,
                    picking: true,
                    secret: String::new(),
                    has_saved_secret: false,
                    show_advanced: false,
                    test: None,
                    test_result: None,
                    error: None,
                    tab: FormTab::Main,
                    picker_filter: String::new(),
                    picker_focus: true,
                    ssh_password: String::new(),
                    ssh_passphrase: String::new(),
                    proxy_password: String::new(),
                    url: String::new(),
                    cancelled: false,
                    reveal_result: false,
                }
            }
        }
    }

    pub fn new_from(_cx: &AppCtx, p: ConnectionProfile) -> Self {
        let mut d = Self::edit(p);
        d.is_new = true;
        d.has_saved_secret = false;
        d
    }

    fn edit(p: ConnectionProfile) -> Self {
        let has = p.save_secret;
        Self {
            p,
            is_new: false,
            picking: false,
            secret: String::new(),
            has_saved_secret: has,
            show_advanced: false,
            test: None,
            test_result: None,
            error: None,
            tab: FormTab::Main,
            picker_filter: String::new(),
            picker_focus: true,
            ssh_password: String::new(),
            ssh_passphrase: String::new(),
            proxy_password: String::new(),
            url: String::new(),
            cancelled: false,
            reveal_result: false,
        }
    }

    /// Test hook: show a failed connection test.
    #[cfg(test)]
    pub fn set_test_error(&mut self, e: Error) {
        self.test_result = Some(Err(e));
        self.reveal_result = true;
    }

    /// Skip the picker and open the form for `driver`.
    pub fn preselect(&mut self, cx: &AppCtx, driver: &str) {
        self.pick(cx, driver);
    }

    fn pick(&mut self, cx: &AppCtx, driver: &str) {
        self.p.driver = driver.to_string();
        if let Some(d) = cx.conns.registry.get(driver) {
            for f in &d.info().fields {
                if !f.default.is_empty() && f.kind != FieldKind::Secret {
                    self.p.params.entry(f.key.to_string()).or_insert_with(|| f.default.to_string());
                }
            }
            if self.p.name.is_empty() {
                self.p.name = d.info().name.split(' ').next().unwrap_or("Connection").to_string();
            }
        }
        self.picking = false;
    }

    fn validate(&self, cx: &AppCtx) -> std::result::Result<(), String> {
        if self.p.name.trim().is_empty() {
            return Err("Give the connection a name".into());
        }
        let d = cx.conns.registry.get(&self.p.driver).ok_or("Unknown connector")?;
        for f in &d.info().fields {
            if f.required && f.kind != FieldKind::Secret && self.p.param(f.key).trim().is_empty() {
                return Err(format!("{} is required", f.label));
            }
            if f.kind == FieldKind::Number && !self.p.param(f.key).is_empty() && self.p.param(f.key).trim().parse::<f64>().is_err() {
                return Err(format!("{} must be a number", f.label));
            }
        }
        Ok(())
    }

    /// Returns false when the dialog should close.
    pub fn show(&mut self, ctx: &egui::Context, cx: &mut AppCtx) -> bool {
        let mut open = true;
        let mut close = false;
        if let Some(r) = poll(&mut self.test) {
            self.test_result = Some(r);
            self.reveal_result = true;
        }
        let title = if self.is_new { "New connection" } else { "Edit connection" };
        egui::Modal::new(egui::Id::unique("conn_dialog")).show(ctx, |ui| {
            // Large enough that most forms fit without scrolling, never wider than the window.
            let screen = ctx.content_rect();
            let w = (if self.picking { 900.0_f32 } else { 820.0 }).min(screen.width() - 48.0);
            ui.set_width(w);
            // Content (e.g. a long error) must wrap rather than widen the dialog.
            ui.set_max_width(w);
            ui.horizontal(|ui| {
                ui.heading(title);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(String::from(ICON_CLOSE)).on_hover_text("Close (Esc)").clicked() {
                        open = false;
                    }
                });
            });
            ui.separator();
            // Header and close button stay put; the body scrolls when the window is short.
            let max_h = (screen.height() * 0.88 - 110.0).max(240.0);
            egui::ScrollArea::vertical().max_height(max_h).min_scrolled_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
                if self.picking {
                    self.picker(ui, cx);
                } else {
                    close = self.form(ui, cx);
                }
            });
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                open = false;
            }
        });
        open && !close && !self.cancelled
    }

    fn picker(&mut self, ui: &mut egui::Ui, cx: &AppCtx) {
        let t = theme::tokens(ui);
        let r = ui.add(egui::TextEdit::singleline(&mut self.picker_filter).hint_text(ico(ICON_SEARCH, "Find a connector")).desired_width(f32::INFINITY));
        if self.picker_focus {
            r.request_focus();
            self.picker_focus = false;
        }
        ui.add_space(theme::space::SM);
        let f = self.picker_filter.to_lowercase();
        let infos: Vec<openvclient_core::DriverInfo> = cx
            .conns
            .registry
            .all()
            .iter()
            .map(|d| d.info().clone())
            .filter(|i| f.is_empty() || i.name.to_lowercase().contains(&f) || i.id.contains(&f) || i.description.to_lowercase().contains(&f))
            .collect();
        let mut chosen = None;
        // Grouped by kind so the list scans quickly; each group has its own colour.
        use openvclient_core::Category as C;
        let groups: [(&str, &[C], egui::Color32); 4] = [
            ("Databases", &[C::Relational, C::Analytical], t.accent),
            ("NoSQL, search and queues", &[C::Document, C::KeyValue, C::Search, C::Messaging], t.warning),
            ("Object storage", &[C::Storage], t.info),
            ("Servers and containers", &[C::Remote, C::Containers], t.fg),
        ];
        for (title, cats, color) in groups {
            let members: Vec<_> = infos.iter().filter(|i| cats.contains(&i.category)).cloned().collect();
            if members.is_empty() {
                continue;
            }
            ui.label(RichText::new(format!("{}  ·  {}", title.to_uppercase(), members.len())).small().strong().color(t.muted_fg));
            ui.add_space(theme::space::XS);
            crate::widgets::cards::grid(ui, &members, 176.0, 40.0, |ui, info, resp| {
                ui.horizontal_centered(|ui| {
                    crate::views::driver_glyph(ui, info.id, theme::icon::MD, color);
                    ui.add(egui::Label::new(RichText::new(info.name).strong()).truncate());
                });
                let resp = resp.clone().on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{}\n{}", info.name, info.description));
                if resp.clicked() {
                    chosen = Some(info.id);
                }
            });
            ui.add_space(theme::space::SM);
        }
        if infos.is_empty() {
            ui.label(RichText::new("No connector matches").color(t.muted_fg));
        }
        if let Some(id) = chosen {
            self.pick(cx, id);
        }
    }

    /// Returns true when saved.
    fn form(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx) -> bool {
        let t = theme::tokens(ui);
        let Some(driver) = cx.conns.registry.get(&self.p.driver) else {
            ui.label("Unknown connector");
            return false;
        };
        let info = driver.info().clone();
        ui.horizontal(|ui| {
            crate::views::driver_glyph(ui, info.id, 18.0, t.accent);
            ui.label(RichText::new(info.name).strong());
            if self.is_new && ui.small_button("Change").clicked() {
                self.picking = true;
                self.p.params.clear();
            }
        });
        let networked = info.fields.iter().any(|f| f.key == "host");
        if networked {
            ui.add_space(theme::space::XS);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, FormTab::Main, ico(ICON_SETTINGS, "Main"));
                let mode = self.p.param(openvclient_net::keys::MODE).to_string();
                let label = match mode.as_str() {
                    "ssh" => "Network · SSH tunnel",
                    "socks5" => "Network · SOCKS5",
                    "http" => "Network · HTTP proxy",
                    _ => "Network",
                };
                ui.selectable_value(&mut self.tab, FormTab::Network, ico(ICON_LAN, label));
            });
            ui.separator();
        } else {
            self.tab = FormTab::Main;
        }
        ui.add_space(theme::space::SM);
        if self.tab == FormTab::Network {
            self.network_ui(ui);
            return self.footer(ui, cx, &driver);
        }
        let folders = cx.store.all_folders();
        if self.is_new && networked {
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.url).hint_text(ico(ICON_LINK, "Paste a connection URL, e.g. postgres://user:pass@host:5432/db")).desired_width(ui.available_width() - 90.0),
                );
                let go = ui.button("Fill in").on_hover_text("Fill the fields below from the URL");
                if (go.clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))) && !self.url.trim().is_empty() {
                    match parse_connection_url(&self.url) {
                        Some(u) => {
                            if u.driver != self.p.driver && cx.conns.registry.get(u.driver).is_some() {
                                self.pick(cx, u.driver);
                            }
                            if u.driver == "mongodb" {
                                self.p.params.insert("uri".into(), self.url.trim().to_string());
                            }
                            self.p.params.insert("host".into(), u.host);
                            if let Some(port) = u.port {
                                self.p.params.insert("port".into(), port.to_string());
                            }
                            if !u.user.is_empty() {
                                self.p.params.insert("user".into(), u.user);
                            }
                            if !u.database.is_empty() {
                                self.p.params.insert("database".into(), u.database);
                            }
                            if !u.password.is_empty() {
                                self.secret = u.password;
                            }
                            self.url.clear();
                            self.error = None;
                        }
                        None => self.error = Some("That does not look like a postgres://, mysql://, sqlserver://, redis://, mongodb:// or clickhouse:// URL".into()),
                    }
                }
            });
            ui.add_space(theme::space::SM);
        }
        let duplicate = cx.store.ws.profiles.iter().any(|p| p.id != self.p.id && p.name.trim().eq_ignore_ascii_case(self.p.name.trim()));
        egui::Grid::new("conn_form").num_columns(2).spacing([14.0, 10.0]).min_col_width(130.0).show(ui, |ui| {
            ui.label("Name *");
            ui.vertical(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.p.name).desired_width(f32::INFINITY));
                if duplicate {
                    ui.label(RichText::new(ico(ICON_WARNING, "Another connection already has this name")).small().color(t.warning));
                }
            });
            ui.end_row();
            ui.label("Environment");
            ui.horizontal(|ui| {
                for e in Environment::ALL {
                    let c = theme::env_color(ui, e);
                    let sel = self.p.environment == e;
                    let txt = RichText::new(e.label()).color(if sel { c } else { t.fg });
                    if ui.selectable_label(sel, txt).clicked() {
                        self.p.environment = e;
                    }
                }
            });
            ui.end_row();
            ui.label("Folder");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.p.folder).hint_text("e.g. Customers/ACME").desired_width(240.0));
                egui::ComboBox::from_id_salt("folder_pick").selected_text("Pick").width(80.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.p.folder, String::new(), "(root)");
                    for f in &folders {
                        ui.selectable_value(&mut self.p.folder, f.clone(), f);
                    }
                });
            });
            ui.end_row();
            for f in info.fields.iter().filter(|f| !f.advanced) {
                self.field(ui, f);
            }
            if info.fields.iter().any(|f| f.kind == FieldKind::Secret) {
                ui.label("");
                ui.checkbox(&mut self.p.save_secret, "Save password in the system keychain");
                ui.end_row();
            }
            ui.label("");
            ui.checkbox(&mut self.p.read_only, "Read-only (enforced by the server where supported)");
            ui.end_row();
        });
        let adv: Vec<_> = info.fields.iter().filter(|f| f.advanced).cloned().collect();
        if !adv.is_empty() {
            ui.add_space(theme::space::XS);
            let label = if self.show_advanced { ico(ICON_EXPAND_MORE, "Advanced") } else { ico(ICON_CHEVRON_RIGHT, "Advanced") };
            if ui.selectable_label(false, label).clicked() {
                self.show_advanced = !self.show_advanced;
            }
            if self.show_advanced {
                egui::Grid::new("conn_adv").num_columns(2).spacing([14.0, 10.0]).min_col_width(130.0).show(ui, |ui| {
                    for f in &adv {
                        self.field(ui, f);
                    }
                });
            }
        }
        if self.p.environment.is_protected() {
            ui.add_space(theme::space::SM);
            ui.label(RichText::new(ico(ICON_WARNING, "Production: destructive actions ask you to type the object's name.")).color(t.destructive).small());
        }
        self.footer(ui, cx, &driver)
    }

    /// Errors, test result and the Test / Save row. Returns true when saved.
    fn footer(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx, driver: &std::sync::Arc<dyn openvclient_core::Driver>) -> bool {
        let t = theme::tokens(ui);
        ui.add_space(theme::space::SM);
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(t.destructive));
        }
        match &self.test_result {
            Some(Ok(m)) => {
                ui.label(RichText::new(ico(ICON_CHECK, m)).color(t.accent));
            }
            Some(Err(e)) => {
                crate::views::error_box(ui, e);
                // On short windows the result sits below the fold; bring it into view.
                if std::mem::take(&mut self.reveal_result) {
                    ui.scroll_to_cursor(Some(egui::Align::Center));
                }
                if let Some(fp) = openvclient_net::unknown_host_key(e) {
                    let param = if e.message.contains("(jump host)") { "jump_fingerprint" } else { openvclient_net::keys::SSH_FINGERPRINT };
                    let t = theme::tokens(ui);
                    egui::Frame::new().fill(t.muted).corner_radius(6.0).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
                        ui.label(RichText::new("Host key fingerprint (SHA-256) — compare it with the one from the server's administrator").small().color(t.muted_fg));
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&fp).monospace().strong());
                            if ui.button(ico(ICON_VERIFIED_USER, "Trust this key")).clicked() {
                                self.p.params.insert(param.to_string(), fp.clone());
                                self.test_result = None;
                            }
                        });
                    });
                }
            }
            None => {}
        }
        ui.add_space(theme::space::SM);
        let mut saved = false;
        ui.horizontal(|ui| {
            let testing = self.test.is_some();
            if ui.add_enabled(!testing, egui::Button::new(ico(ICON_BOLT, "Test connection"))).clicked() {
                match self.validate(cx) {
                    Ok(()) => {
                        self.error = None;
                        self.test_result = None;
                        let p = self.p.clone();
                        let typed = (!self.secret.is_empty()).then(|| self.secret.clone());
                        let use_saved = self.has_saved_secret && typed.is_none();
                        let d = driver.clone();
                        let typed_net = (self.ssh_password.clone(), self.ssh_passphrase.clone(), self.proxy_password.clone());
                        self.test = Some(cx.bg.spawn(format!("Testing {}", p.name), async move {
                            let id = p.id;
                            let secret = if use_saved { tokio::task::spawn_blocking(move || secrets::get(id)).await.map_err(Error::from)?.map_err(Error::other)? } else { typed };
                            let mut net = tokio::task::spawn_blocking(move || crate::conns::net_secrets(id)).await.map_err(Error::from)?;
                            let pick = |typed: String, saved: Option<String>| if typed.is_empty() { saved } else { Some(typed) };
                            net.ssh_password = pick(typed_net.0, net.ssh_password);
                            net.ssh_passphrase = pick(typed_net.1, net.ssh_passphrase);
                            net.proxy_password = pick(typed_net.2, net.proxy_password);
                            let t0 = Instant::now();
                            let (h, tunnel) = crate::conns::open_session(d, p, secret, net, None).await?;
                            let via = tunnel.as_ref().map(|t| format!(" via {}", t.description)).unwrap_or_default();
                            let lat = h.base().ping().await?;
                            let info = h.base().server_info();
                            h.base().close().await;
                            Ok(format!("Connected to {}{via} in {} (ping {})", info.product, openvclient_core::elapsed_ms(t0.elapsed()), openvclient_core::elapsed_ms(lat)))
                        }));
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            if testing {
                ui.add(egui::Spinner::new());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if self.is_new { "Save & connect" } else { "Save" };
                let save_clicked = theme::primary_button(ui, label).clicked();
                if ui.button("Cancel").on_hover_text("Close without saving (Esc)").clicked() {
                    self.cancelled = true;
                }
                if save_clicked {
                    match self.validate(cx) {
                        Ok(()) => {
                            let id = self.p.id;
                            let mut ok = true;
                            for (name, v) in [
                                (openvclient_net::keys::SECRET_SSH_PASSWORD, &self.ssh_password),
                                (openvclient_net::keys::SECRET_SSH_PASSPHRASE, &self.ssh_passphrase),
                                (openvclient_net::keys::SECRET_PROXY_PASSWORD, &self.proxy_password),
                            ] {
                                if !v.is_empty()
                                    && let Err(e) = secrets::set_named(id, name, v)
                                {
                                    self.error = Some(e);
                                    ok = false;
                                }
                            }
                            if self.p.save_secret && !self.secret.is_empty() {
                                if let Err(e) = secrets::set(id, &self.secret) {
                                    self.error = Some(format!("{e}. Turn off “Save password” to type it at connect time instead."));
                                    ok = false;
                                }
                            } else if !self.p.save_secret {
                                let _ = secrets::delete_main(id);
                                if !self.secret.is_empty() {
                                    cx.conns.remember_secret(id, self.secret.clone());
                                }
                            }
                            if ok {
                                self.p.folder = self.p.folder.trim_matches('/').to_string();
                                let was_connected = cx.conns.is_connected(id);
                                cx.store.upsert(self.p.clone());
                                cx.mark_dirty();
                                cx.toasts.ok(format!("Saved {}", self.p.name));
                                if self.is_new || was_connected {
                                    if was_connected {
                                        cx.cmd(Cmd::Disconnect(id));
                                    }
                                    cx.cmd(Cmd::Connect(id));
                                }
                                cx.selected_conn = Some(id);
                                saved = true;
                            }
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
            });
        });
        saved
    }

    fn network_ui(&mut self, ui: &mut egui::Ui) {
        use openvclient_net::keys::*;
        let t = theme::tokens(ui);
        let mode = self.p.params.entry(MODE.into()).or_insert_with(|| "direct".into()).clone();
        ui.horizontal(|ui| {
            ui.label("Connect through");
            for (m, l) in [("direct", "Direct"), ("ssh", "SSH tunnel"), ("socks5", "SOCKS5 proxy"), ("http", "HTTP proxy")] {
                if ui.selectable_label(mode == m, l).clicked() {
                    self.p.params.insert(MODE.into(), m.into());
                }
            }
        });
        ui.add_space(theme::space::SM);
        let text = |ui: &mut egui::Ui, p: &mut openvclient_core::ConnectionProfile, key: &str, hint: &str, w: f32| {
            let v = p.params.entry(key.into()).or_default();
            ui.add(egui::TextEdit::singleline(v).hint_text(hint).desired_width(w));
        };
        let saved = |has: bool| if has { "•••••• saved (leave empty to keep)" } else { "" };
        match mode.as_str() {
            "ssh" => {
                egui::Grid::new("ssh_form").num_columns(2).spacing([14.0, 10.0]).min_col_width(130.0).show(ui, |ui| {
                    ui.label("SSH host *");
                    ui.horizontal(|ui| {
                        text(ui, &mut self.p, SSH_HOST, "bastion.example.com", 260.0);
                        ui.label("Port");
                        text(ui, &mut self.p, SSH_PORT, "22", 60.0);
                    });
                    ui.end_row();
                    ui.label("User *");
                    text(ui, &mut self.p, SSH_USER, "ubuntu", 260.0);
                    ui.end_row();
                    ui.label("Authentication");
                    let auth = self.p.params.entry(SSH_AUTH.into()).or_insert_with(|| "password".into()).clone();
                    ui.horizontal(|ui| {
                        for (a, l) in [("password", "Password"), ("key", "Private key"), ("agent", "SSH agent")] {
                            if ui.selectable_label(auth == a, l).clicked() {
                                self.p.params.insert(SSH_AUTH.into(), a.into());
                            }
                        }
                    });
                    ui.end_row();
                    match auth.as_str() {
                        "key" => {
                            ui.label("Private key *");
                            ui.horizontal(|ui| {
                                text(ui, &mut self.p, SSH_KEY, "~/.ssh/id_ed25519", 300.0);
                                if ui.button("Browse…").clicked()
                                    && let Some(f) = rfd::FileDialog::new().pick_file()
                                {
                                    self.p.params.insert(SSH_KEY.into(), f.display().to_string());
                                }
                            });
                            ui.end_row();
                            ui.label("Passphrase");
                            ui.add(egui::TextEdit::singleline(&mut self.ssh_passphrase).password(true).hint_text(saved(!self.is_new)).desired_width(260.0));
                            ui.end_row();
                        }
                        "agent" => {
                            ui.label("");
                            let agent_hint = if cfg!(windows) {
                                "Uses the keys loaded in the Windows OpenSSH agent (ssh-agent service), or in Pageant."
                            } else {
                                "Uses the keys loaded in your SSH agent (SSH_AUTH_SOCK)."
                            };
                            ui.label(RichText::new(agent_hint).small().color(t.muted_fg));
                            ui.end_row();
                        }
                        _ => {
                            ui.label("Password");
                            ui.add(egui::TextEdit::singleline(&mut self.ssh_password).password(true).hint_text(saved(!self.is_new)).desired_width(260.0));
                            ui.end_row();
                        }
                    }
                    ui.label("Timeout (ms)");
                    text(ui, &mut self.p, SSH_TIMEOUT, "10000", 100.0);
                    ui.end_row();
                    ui.label("Host key");
                    let fp = self.p.param(SSH_FINGERPRINT).to_string();
                    ui.horizontal(|ui| {
                        if fp.is_empty() {
                            ui.label(RichText::new("Pinned on first successful connect").small().color(t.muted_fg));
                        } else {
                            ui.label(RichText::new(&fp).monospace().small());
                            if ui.small_button("Forget").on_hover_text("Trust the next key the server presents").clicked() {
                                self.p.params.remove(SSH_FINGERPRINT);
                            }
                        }
                    });
                    ui.end_row();
                });
                ui.add_space(theme::space::XS);
                ui.label(
                    RichText::new("The database host and port on the Main tab are resolved from the SSH server, so use addresses it can reach (e.g. localhost for a DB on the same machine).")
                        .small()
                        .color(t.muted_fg),
                );
            }
            "socks5" | "http" => {
                egui::Grid::new("proxy_form").num_columns(2).spacing([14.0, 10.0]).min_col_width(130.0).show(ui, |ui| {
                    ui.label("Proxy host *");
                    ui.horizontal(|ui| {
                        text(ui, &mut self.p, PROXY_HOST, "proxy.example.com", 260.0);
                        ui.label("Port");
                        text(ui, &mut self.p, PROXY_PORT, if mode == "socks5" { "1080" } else { "3128" }, 60.0);
                    });
                    ui.end_row();
                    ui.label("Username");
                    text(ui, &mut self.p, PROXY_USER, "optional", 260.0);
                    ui.end_row();
                    ui.label("Password");
                    ui.add(egui::TextEdit::singleline(&mut self.proxy_password).password(true).hint_text(saved(!self.is_new)).desired_width(260.0));
                    ui.end_row();
                });
                ui.add_space(theme::space::XS);
                let note = if mode == "http" { "Uses HTTP CONNECT; the proxy must allow CONNECT to the database port." } else { "SOCKS5 with optional username/password (RFC 1929)." };
                ui.label(RichText::new(note).small().color(t.muted_fg));
            }
            _ => {
                ui.label(RichText::new("Connects straight to the host and port on the Main tab.").color(t.muted_fg));
            }
        }
    }

    fn field(&mut self, ui: &mut egui::Ui, f: &openvclient_core::FieldSpec) {
        if let Some((key, values)) = f.show_if {
            let current = self.p.params.get(key).map(String::as_str).unwrap_or("");
            if !values.contains(&current) {
                return;
            }
        }
        let label = if f.required { format!("{} *", f.label) } else { f.label.to_string() };
        let resp = ui.label(label);
        if !f.help.is_empty() {
            resp.on_hover_text(f.help);
        }
        let hint = if f.placeholder.is_empty() { f.default } else { f.placeholder };
        match &f.kind {
            FieldKind::Secret => {
                let hint = if self.has_saved_secret { "•••••• saved in keychain (leave empty to keep)" } else { "" };
                ui.add(egui::TextEdit::singleline(&mut self.secret).password(true).hint_text(hint).desired_width(f32::INFINITY));
            }
            FieldKind::Bool => {
                let v = self.p.params.entry(f.key.into()).or_insert_with(|| f.default.into());
                let mut b = v == "true";
                if ui.checkbox(&mut b, "").changed() {
                    *v = b.to_string();
                }
            }
            FieldKind::Choice(opts) => {
                let v = self.p.params.entry(f.key.into()).or_insert_with(|| f.default.into());
                egui::ComboBox::from_id_salt(f.key).selected_text(openvclient_core::choice_label(f.key, v)).show_ui(ui, |ui| {
                    for o in *opts {
                        ui.selectable_value(v, o.to_string(), openvclient_core::choice_label(f.key, o));
                    }
                });
            }
            FieldKind::File { save, filters } => {
                ui.horizontal(|ui| {
                    let v = self.p.params.entry(f.key.into()).or_default();
                    ui.add(egui::TextEdit::singleline(v).hint_text(hint).desired_width(330.0));
                    if ui.button("Browse…").clicked() {
                        let dlg = rfd::FileDialog::new().add_filter("Database", filters).add_filter("All files", &["*"]);
                        let picked = if *save { dlg.set_file_name("database.db").save_file() } else { dlg.pick_file() };
                        if let Some(path) = picked {
                            *v = path.display().to_string();
                        }
                    }
                });
            }
            FieldKind::Text | FieldKind::Number => {
                let v = self.p.params.entry(f.key.into()).or_default();
                let bad = f.kind == FieldKind::Number && !v.trim().is_empty() && v.trim().parse::<f64>().is_err();
                ui.vertical(|ui| {
                    ui.add(egui::TextEdit::singleline(v).hint_text(hint).desired_width(f32::INFINITY));
                    // Errors sit next to the field they belong to.
                    if bad {
                        ui.label(RichText::new(format!("{} must be a number", f.label)).small().color(theme::tokens(ui).destructive));
                    }
                });
            }
        }
        ui.end_row();
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn parses_common_urls() {
        let u = parse_connection_url("postgres://alice:p%40ss@db.example.com:6543/shop?sslmode=require").unwrap();
        assert_eq!(u, ParsedUrl { driver: "postgres", host: "db.example.com".into(), port: Some(6543), user: "alice".into(), password: "p@ss".into(), database: "shop".into() });
        let u = parse_connection_url("mysql://root@127.0.0.1/app").unwrap();
        assert_eq!((u.driver, u.host.as_str(), u.port, u.user.as_str(), u.password.as_str(), u.database.as_str()), ("mysql", "127.0.0.1", None, "root", "", "app"));
        let u = parse_connection_url("redis://:secret@[::1]:6380/2").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.password.as_str(), u.database.as_str()), ("::1", Some(6380), "secret", "2"));
        assert_eq!(parse_connection_url("mongodb+srv://u:p@cluster0.x.net/db").unwrap().driver, "mongodb");
        assert!(parse_connection_url("ftp://x").is_none());
        assert!(parse_connection_url("not a url").is_none());
    }
}
