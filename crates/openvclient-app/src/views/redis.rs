//! Redis key browser, value editor, console and server views.

use egui::{Key, RichText};
use egui_material_icons::icons::*;
use openvclient_core::Result;
use openvclient_core::kv::{KeyDetail, KeyEntry, KvEdit, KvValue, ScanPage};
use uuid::Uuid;

use crate::app::AppCtx;
use crate::bg::{Job, poll};
use crate::theme::{self, ico};

#[derive(PartialEq, Clone, Copy)]
enum View {
    Keys,
    Console,
    Info,
    Clients,
    Slowlog,
}

pub struct RedisTab {
    pub conn: Uuid,
    pub db: u32,
    view: View,
    pattern: String,
    type_filter: Option<&'static str>,
    keys: Vec<KeyEntry>,
    cursor: u64,
    scanned_once: bool,
    scan: Option<Job<Result<ScanPage>>>,
    selected: Option<String>,
    detail: Option<Result<KeyDetail>>,
    detail_job: Option<Job<Result<KeyDetail>>>,
    action: Option<Job<Result<String>>>,
    edit_text: String,
    field: String,
    value: String,
    ttl_input: String,
    rename_to: String,
    new_key: Option<(String, &'static str, String, String, String)>,
    console_input: String,
    console_log: Vec<(String, std::result::Result<String, String>)>,
    console_job: Option<Job<Result<String>>>,
    history_pos: Option<usize>,
    info: Option<Result<Vec<(String, String, String)>>>,
    info_job: Option<Job<Result<Vec<(String, String, String)>>>>,
    clients: Option<Result<Vec<Vec<(String, String)>>>>,
    clients_job: Option<Job<Result<Vec<Vec<(String, String)>>>>>,
    slow: Option<Result<Vec<(i64, i64, u64, String)>>>,
    slow_job: Option<Job<Result<Vec<(i64, i64, u64, String)>>>>,
    key_filter: String,
}

impl RedisTab {
    pub fn new(conn: Uuid, db: u32) -> Self {
        Self {
            conn,
            db,
            view: View::Keys,
            pattern: "*".into(),
            type_filter: None,
            keys: Vec::new(),
            cursor: 0,
            scanned_once: false,
            scan: None,
            selected: None,
            detail: None,
            detail_job: None,
            action: None,
            edit_text: String::new(),
            field: String::new(),
            value: String::new(),
            ttl_input: String::new(),
            rename_to: String::new(),
            new_key: None,
            console_input: String::new(),
            console_log: Vec::new(),
            console_job: None,
            history_pos: None,
            info: None,
            info_job: None,
            clients: None,
            clients_job: None,
            slow: None,
            slow_job: None,
            key_filter: String::new(),
        }
    }

    fn scan(&mut self, cx: &AppCtx, reset: bool) {
        let Some(s) = cx.conns.session(self.conn).and_then(|h| h.kv()).cloned() else { return };
        if reset {
            self.keys.clear();
            self.cursor = 0;
        }
        let (db, cur, pat, tf) = (self.db, self.cursor, self.pattern.clone(), self.type_filter);
        self.scan = Some(cx.bg.spawn("SCAN", async move { s.scan(db, cur, &pat, 1000, tf).await }));
        self.scanned_once = true;
    }

    fn load_detail(&mut self, cx: &AppCtx, key: &str) {
        let Some(s) = cx.conns.session(self.conn).and_then(|h| h.kv()).cloned() else { return };
        let (db, k) = (self.db, key.to_string());
        self.detail_job = Some(cx.bg.spawn("", async move { s.key_detail(db, &k, 1000).await }));
    }

    fn act<F>(&mut self, cx: &AppCtx, label: &str, f: impl FnOnce(std::sync::Arc<dyn openvclient_core::kv::KeyValueSession>) -> F)
    where
        F: std::future::Future<Output = Result<String>> + Send + 'static,
    {
        let Some(s) = cx.conns.session(self.conn).and_then(|h| h.kv()).cloned() else { return };
        self.action = Some(cx.bg.spawn(label.to_string(), f(s)));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx, tab_id: u64) {
        let t = theme::tokens(ui);
        if cx.conns.session(self.conn).and_then(|h| h.kv()).is_none() {
            crate::views::unsupported(ui, "Not connected");
            return;
        }
        if let Some(r) = poll(&mut self.scan) {
            match r {
                Ok(p) => {
                    self.cursor = p.cursor;
                    self.keys.extend(p.keys);
                    self.keys.sort_by(|a, b| a.key.cmp(&b.key));
                    self.keys.dedup_by(|a, b| a.key == b.key);
                }
                Err(e) => cx.toasts.error("SCAN failed", &e),
            }
        }
        if let Some(r) = poll(&mut self.detail_job) {
            if let Ok(d) = &r {
                if let KvValue::String(b) = &d.value {
                    self.edit_text = String::from_utf8_lossy(b).into_owned();
                }
                self.ttl_input = d.ttl.map(|t| t.to_string()).unwrap_or_default();
                self.rename_to = d.key.clone();
            }
            self.detail = Some(r);
        }
        if let Some(r) = poll(&mut self.action) {
            match r {
                Ok(m) => {
                    cx.toasts.ok(if m.is_empty() { "Done".to_string() } else { m });
                    if let Some(k) = self.selected.clone() {
                        self.load_detail(cx, &k);
                    }
                }
                Err(e) => cx.toasts.error("Redis command failed", &e),
            }
        }
        if let Some(r) = poll(&mut self.console_job)
            && let Some(last) = self.console_log.last_mut()
        {
            last.1 = r.map_err(|e| e.to_string());
        }
        if !self.scanned_once && self.view == View::Keys {
            self.scan(cx, true);
        }

        ui.horizontal(|ui| {
            crate::views::header(ui, ICON_KEY, &cx.profile_name(self.conn), "");
            egui::ComboBox::from_id_salt(("rdb", tab_id)).selected_text(format!("db{}", self.db)).width(70.0).show_ui(ui, |ui| {
                for i in 0..16 {
                    if ui.selectable_value(&mut self.db, i, format!("db{i}")).clicked() {
                        self.selected = None;
                        self.detail = None;
                        self.scan(cx, true);
                    }
                }
            });
            ui.separator();
            for (v, icon, label) in [
                (View::Keys, ICON_KEY, "Keys"),
                (View::Console, ICON_TERMINAL, "Console"),
                (View::Info, ICON_INFO, "Server"),
                (View::Clients, ICON_GROUP, "Clients"),
                (View::Slowlog, ICON_TIMER, "Slow log"),
            ] {
                if ui.selectable_value(&mut self.view, v, ico(icon, label)).clicked() {
                    self.info = None;
                    self.clients = None;
                    self.slow = None;
                }
            }
        });
        ui.separator();
        match self.view {
            View::Keys => self.keys_ui(ui, cx, &t, tab_id),
            View::Console => self.console_ui(ui, cx, &t),
            View::Info => {
                if self.info.is_none() && self.info_job.is_none() {
                    let s = cx.conns.session(self.conn).and_then(|h| h.kv()).cloned().unwrap();
                    self.info_job = Some(cx.bg.spawn("INFO", async move { s.info().await }));
                }
                if let Some(r) = poll(&mut self.info_job) {
                    self.info = Some(r);
                }
                match &self.info {
                    None => crate::views::busy(ui, "Loading INFO…"),
                    Some(Err(e)) => crate::views::error_box(ui, e),
                    Some(Ok(rows)) => {
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            let mut section = String::new();
                            for (s, k, v) in rows {
                                if *s != section {
                                    section = s.clone();
                                    ui.add_space(theme::space::SM);
                                    ui.label(RichText::new(section.to_uppercase()).small().strong().color(t.muted_fg));
                                }
                                ui.horizontal(|ui| {
                                    ui.add_sized([260.0, 18.0], egui::Label::new(RichText::new(k).monospace().color(t.muted_fg)).truncate());
                                    ui.label(RichText::new(v).monospace());
                                });
                            }
                        });
                    }
                }
            }
            View::Clients => {
                if self.clients.is_none() && self.clients_job.is_none() {
                    let s = cx.conns.session(self.conn).and_then(|h| h.kv()).cloned().unwrap();
                    self.clients_job = Some(cx.bg.spawn("CLIENT LIST", async move { s.clients().await }));
                }
                if let Some(r) = poll(&mut self.clients_job) {
                    self.clients = Some(r);
                }
                if ui.button(ico(ICON_REFRESH, "Refresh")).clicked() {
                    self.clients = None;
                }
                match &self.clients {
                    None => crate::views::busy(ui, "Loading clients…"),
                    Some(Err(e)) => crate::views::error_box(ui, e),
                    Some(Ok(list)) => {
                        let list = list.clone();
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            egui::Grid::new("clients").striped(true).num_columns(7).show(ui, |ui| {
                                for h in ["id", "addr", "name", "db", "age", "idle", "cmd"] {
                                    ui.label(RichText::new(h).strong());
                                }
                                ui.label("");
                                ui.end_row();
                                for c in list {
                                    let g = |k: &str| c.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone()).unwrap_or_default();
                                    for k in ["id", "addr", "name", "db", "age", "idle", "cmd"] {
                                        ui.label(RichText::new(g(k)).monospace());
                                    }
                                    let id = g("id");
                                    if ui.small_button(RichText::new("Kill…").color(t.destructive)).clicked() {
                                        let s = cx.conns.session(self.conn).and_then(|h| h.kv()).cloned().unwrap();
                                        cx.confirm(Some(self.conn), format!("Kill client {id}?"), "The client connection is closed by the server.", "Kill", None, move |cx| {
                                            crate::views::dialogs::track(cx, format!("CLIENT KILL {id}"), "Client killed", None, async move { s.kill_client(&id).await });
                                        });
                                    }
                                    ui.end_row();
                                }
                            });
                        });
                    }
                }
            }
            View::Slowlog => {
                if self.slow.is_none() && self.slow_job.is_none() {
                    let s = cx.conns.session(self.conn).and_then(|h| h.kv()).cloned().unwrap();
                    self.slow_job = Some(cx.bg.spawn("SLOWLOG", async move { s.slowlog(128).await }));
                }
                if let Some(r) = poll(&mut self.slow_job) {
                    self.slow = Some(r);
                }
                match &self.slow {
                    None => crate::views::busy(ui, "Loading slow log…"),
                    Some(Err(e)) => crate::views::error_box(ui, e),
                    Some(Ok(v)) if v.is_empty() => crate::views::unsupported(ui, "No slow commands recorded (see slowlog-log-slower-than)."),
                    Some(Ok(v)) => {
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            egui::Grid::new("slow").striped(true).num_columns(4).show(ui, |ui| {
                                for h in ["id", "when", "duration", "command"] {
                                    ui.label(RichText::new(h).strong());
                                }
                                ui.end_row();
                                for (id, ts, us, cmd) in v {
                                    ui.label(id.to_string());
                                    ui.label(chrono::DateTime::from_timestamp(*ts, 0).map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default());
                                    ui.label(format!("{:.2} ms", *us as f64 / 1000.0));
                                    ui.label(RichText::new(cmd).monospace());
                                    ui.end_row();
                                }
                            });
                        });
                    }
                }
            }
        }
    }

    fn keys_ui(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx, t: &theme::Tokens, tab_id: u64) {
        let avail = ui.available_width();
        egui::Panel::left(egui::Id::unique(("rkeys", tab_id))).resizable(true).default_size(avail * 0.4).show(ui, |ui| {
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.pattern).hint_text("MATCH pattern, e.g. user:*").font(egui::TextStyle::Monospace).desired_width(ui.available_width() - 120.0));
                if (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))) | ui.button(ico(ICON_SEARCH, "Scan")).clicked() {
                    self.scan(cx, true);
                }
            });
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt(("rtype", tab_id)).selected_text(self.type_filter.unwrap_or("any type")).show_ui(ui, |ui| {
                    for tf in [None, Some("string"), Some("hash"), Some("list"), Some("set"), Some("zset"), Some("stream")] {
                        if ui.selectable_value(&mut self.type_filter, tf, tf.unwrap_or("any type")).clicked() {
                            self.scan(cx, true);
                        }
                    }
                });
                ui.add(egui::TextEdit::singleline(&mut self.key_filter).hint_text("filter loaded").desired_width(110.0));
                if ui.button(String::from(ICON_ADD)).on_hover_text("New key").clicked() {
                    self.new_key = Some((String::new(), "string", String::new(), String::new(), String::new()));
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} keys loaded", self.keys.len())).small().color(t.muted_fg));
                if self.scan.is_some() {
                    ui.add(egui::Spinner::new().size(12.0));
                } else if self.cursor != 0 && ui.small_button("Load more").on_hover_text("Continue SCAN from the last cursor").clicked() {
                    self.scan(cx, false);
                } else if self.cursor == 0 && self.scanned_once {
                    ui.label(RichText::new("· scan complete").small().color(t.muted_fg));
                }
            });
            ui.separator();
            let f = self.key_filter.to_lowercase();
            let keys: Vec<KeyEntry> = self.keys.iter().filter(|k| f.is_empty() || k.key.to_lowercase().contains(&f)).cloned().collect();
            let mut clicked = None;
            // A clear empty state with a way forward, instead of a blank list.
            if keys.is_empty() && self.scan.is_none() && self.scanned_once {
                ui.add_space(theme::space::LG);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(String::from(ICON_KEY_OFF)).size(28.0).color(t.muted_fg));
                    let msg = if !self.key_filter.is_empty() {
                        "No loaded key matches the filter"
                    } else if self.pattern.trim() != "*" && !self.pattern.trim().is_empty() {
                        "No key matches this pattern"
                    } else {
                        "This database has no keys"
                    };
                    ui.label(RichText::new(msg).strong());
                    ui.label(RichText::new("Try a pattern such as user:*, session:* or *:cache").small().color(t.muted_fg));
                    ui.add_space(theme::space::SM);
                    if ui.button(ico(ICON_ADD, "Add a key")).clicked() {
                        self.new_key = Some((String::new(), "string", String::new(), String::new(), String::new()));
                    }
                });
            }
            egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 22.0, keys.len(), |ui, range| {
                for k in &keys[range] {
                    ui.horizontal(|ui| {
                        // Fixed badge column so key names line up.
                        ui.allocate_ui_with_layout(egui::vec2(58.0, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            theme::badge(ui, &k.key_type, type_color(t, &k.key_type));
                        });
                        let sel = self.selected.as_deref() == Some(k.key.as_str());
                        if ui.add(egui::Button::selectable(sel, RichText::new(&k.key).monospace()).truncate()).clicked() {
                            clicked = Some(k.key.clone());
                        }
                        if let Some(ttl) = k.ttl {
                            ui.label(RichText::new(format!("{ttl}s")).small().color(t.muted_fg)).on_hover_text("Seconds until expiry");
                        }
                    });
                }
            });
            if let Some(k) = clicked {
                self.selected = Some(k.clone());
                self.detail = None;
                self.load_detail(cx, &k);
            }
        });
        egui::CentralPanel::no_frame().show(ui, |ui| {
            ui.add_space(theme::space::XS);
            if let Some(nk) = &mut self.new_key {
                let mut create = false;
                let mut cancel = false;
                egui::Frame::new().fill(t.card).stroke(egui::Stroke::new(1.0, t.border)).corner_radius(8).inner_margin(12).show(ui, |ui| {
                    ui.label(RichText::new("New key").strong());
                    egui::Grid::new("nk").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        ui.label("Key");
                        ui.add(egui::TextEdit::singleline(&mut nk.0).font(egui::TextStyle::Monospace));
                        ui.end_row();
                        ui.label("Type");
                        egui::ComboBox::from_id_salt("nktype").selected_text(nk.1).show_ui(ui, |ui| {
                            for ty in ["string", "hash", "list", "set", "zset", "stream"] {
                                ui.selectable_value(&mut nk.1, ty, ty);
                            }
                        });
                        ui.end_row();
                        let (a, b) = match nk.1 {
                            "hash" => ("Field", Some("Value")),
                            "zset" => ("Member", Some("Score")),
                            "stream" => ("Field", Some("Value")),
                            _ => ("Value", None),
                        };
                        ui.label(a);
                        ui.add(egui::TextEdit::singleline(&mut nk.2));
                        ui.end_row();
                        if let Some(b) = b {
                            ui.label(b);
                            ui.add(egui::TextEdit::singleline(&mut nk.3));
                            ui.end_row();
                        }
                        ui.label("TTL (s)");
                        ui.add(egui::TextEdit::singleline(&mut nk.4).hint_text("none"));
                        ui.end_row();
                    });
                    ui.horizontal(|ui| {
                        create = theme::primary_button(ui, "Create").clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
                if create {
                    let (k, ty, a, b, ttl) = nk.clone();
                    let first = if b.is_empty() { vec![a] } else { vec![a, b] };
                    let ttl = ttl.trim().parse::<i64>().ok();
                    let db = self.db;
                    self.act(cx, "Create key", move |s| async move { s.create_key(db, &k, ty, first, ttl).await.map(|_| format!("Created {k}")) });
                    self.new_key = None;
                    self.scanned_once = false;
                } else if cancel {
                    self.new_key = None;
                }
                return;
            }
            let Some(key) = self.selected.clone() else {
                crate::views::unsupported(ui, "Select a key to view and edit its value.");
                return;
            };
            match &self.detail {
                None => crate::views::busy(ui, "Loading value…"),
                Some(Err(e)) => crate::views::error_box(ui, &e.clone()),
                Some(Ok(d)) => {
                    let d = d.clone();
                    self.detail_ui(ui, cx, t, &key, d);
                }
            }
        });
    }

    fn detail_ui(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx, t: &theme::Tokens, key: &str, d: KeyDetail) {
        let db = self.db;
        ui.horizontal_wrapped(|ui| {
            theme::badge(ui, &d.key_type, type_color(t, &d.key_type));
            ui.label(RichText::new(key).monospace().strong());
            let mut meta = vec![format!("{} {}", d.length, if d.key_type == "string" { "bytes" } else { "elements" })];
            if let Some(m) = d.memory {
                meta.push(format!("{} in memory", openvclient_core::containers::human_bytes(m)));
            }
            if let Some(e) = &d.encoding {
                meta.push(e.clone());
            }
            ui.label(RichText::new(meta.join(" · ")).small().color(t.muted_fg));
        });
        ui.horizontal(|ui| {
            ui.label("TTL");
            ui.add(egui::TextEdit::singleline(&mut self.ttl_input).hint_text("no expiry").desired_width(80.0));
            if ui.button("Set").clicked() {
                let (k, ttl) = (key.to_string(), self.ttl_input.trim().parse::<i64>().ok());
                self.act(cx, "Set TTL", move |s| async move { s.set_ttl(db, &k, ttl).await.map(|_| "TTL updated".to_string()) });
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.rename_to).font(egui::TextStyle::Monospace).desired_width(180.0));
            if ui.add_enabled(self.rename_to != key && !self.rename_to.is_empty(), egui::Button::new("Rename")).clicked() {
                let (from, to) = (key.to_string(), self.rename_to.clone());
                self.selected = Some(to.clone());
                self.scanned_once = false;
                self.act(cx, "Rename", move |s| async move { s.rename(db, &from, &to).await.map(|_| format!("Renamed to {to}")) });
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new(ico(ICON_DELETE, "Delete…")).color(t.destructive)).clicked() {
                    let s = cx.conns.session(self.conn).and_then(|h| h.kv()).cloned();
                    if let Some(s) = s {
                        let k = key.to_string();
                        self.selected = None;
                        self.keys.retain(|x| x.key != k);
                        cx.confirm(Some(self.conn), format!("Delete key {k}?"), "UNLINK removes the key immediately.", "Delete", Some(k.clone()), move |cx| {
                            crate::views::dialogs::track(cx, format!("UNLINK {k}"), "Key deleted", None, async move { s.delete_keys(db, vec![k]).await.map(|_| ()) });
                        });
                    }
                }
                if ui.button(String::from(ICON_REFRESH)).on_hover_text("Reload value").clicked() {
                    self.load_detail(cx, key);
                }
            });
        });
        if d.truncated {
            ui.label(RichText::new(format!("Showing the first 1000 of {} elements.", d.length)).small().color(t.warning));
        }
        ui.separator();
        let k = key.to_string();
        let mut edit: Option<KvEdit> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match &d.value {
            KvValue::String(_) => {
                ui.add(egui::TextEdit::multiline(&mut self.edit_text).code_editor().desired_width(f32::INFINITY).desired_rows(12));
                if theme::primary_button(ui, ico(ICON_SAVE, "Save value")).on_hover_text("SET … KEEPTTL").clicked() {
                    edit = Some(KvEdit::SetString { value: self.edit_text.clone() });
                }
            }
            KvValue::Hash(items) => {
                pair_editor(ui, &mut self.field, &mut self.value, "field", "value", || ());
                if ui.button(ico(ICON_ADD, "HSET")).clicked() && !self.field.is_empty() {
                    edit = Some(KvEdit::HashSet { field: self.field.clone(), value: self.value.clone() });
                }
                egui::Grid::new("hash").striped(true).num_columns(3).show(ui, |ui| {
                    for (f, v) in items {
                        ui.label(RichText::new(f).monospace().strong());
                        ui.label(RichText::new(v).monospace());
                        ui.horizontal(|ui| {
                            if ui.small_button(String::from(ICON_EDIT)).on_hover_text("Edit").clicked() {
                                self.field = f.clone();
                                self.value = v.clone();
                            }
                            if ui.small_button(String::from(ICON_DELETE)).on_hover_text("HDEL").clicked() {
                                edit = Some(KvEdit::HashDelete { field: f.clone() });
                            }
                        });
                        ui.end_row();
                    }
                });
            }
            KvValue::List(items) => {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.value).hint_text("value"));
                    if ui.button("LPUSH").clicked() {
                        edit = Some(KvEdit::ListPush { value: self.value.clone(), head: true });
                    }
                    if ui.button("RPUSH").clicked() {
                        edit = Some(KvEdit::ListPush { value: self.value.clone(), head: false });
                    }
                });
                egui::Grid::new("list").striped(true).num_columns(3).show(ui, |ui| {
                    for (i, v) in items.iter().enumerate() {
                        ui.label(RichText::new(i.to_string()).monospace().color(t.muted_fg));
                        ui.label(RichText::new(v).monospace());
                        ui.horizontal(|ui| {
                            if ui.small_button("LSET").on_hover_text("Replace with the value above").clicked() {
                                edit = Some(KvEdit::ListSet { index: i as i64, value: self.value.clone() });
                            }
                            if ui.small_button(String::from(ICON_DELETE)).on_hover_text("LREM (first match)").clicked() {
                                edit = Some(KvEdit::ListRemove { value: v.clone() });
                            }
                        });
                        ui.end_row();
                    }
                });
            }
            KvValue::Set(items) => {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.value).hint_text("member"));
                    if ui.button("SADD").clicked() {
                        edit = Some(KvEdit::SetAdd { member: self.value.clone() });
                    }
                });
                for m in items {
                    ui.horizontal(|ui| {
                        if ui.small_button(String::from(ICON_DELETE)).on_hover_text("SREM").clicked() {
                            edit = Some(KvEdit::SetRemove { member: m.clone() });
                        }
                        ui.label(RichText::new(m).monospace());
                    });
                }
            }
            KvValue::ZSet(items) => {
                pair_editor(ui, &mut self.value, &mut self.field, "member", "score", || ());
                if ui.button("ZADD").clicked()
                    && let Ok(score) = self.field.trim().parse()
                {
                    edit = Some(KvEdit::ZAdd { member: self.value.clone(), score });
                }
                egui::Grid::new("zset").striped(true).num_columns(3).show(ui, |ui| {
                    for (m, s) in items {
                        ui.label(RichText::new(s.to_string()).monospace().color(t.muted_fg));
                        ui.label(RichText::new(m).monospace());
                        if ui.small_button(String::from(ICON_DELETE)).on_hover_text("ZREM").clicked() {
                            edit = Some(KvEdit::ZRemove { member: m.clone() });
                        }
                        ui.end_row();
                    }
                });
            }
            KvValue::Stream(entries) => {
                pair_editor(ui, &mut self.field, &mut self.value, "field", "value", || ());
                if ui.button("XADD *").clicked() && !self.field.is_empty() {
                    edit = Some(KvEdit::StreamAdd { fields: vec![(self.field.clone(), self.value.clone())] });
                }
                for (id, fields) in entries {
                    ui.horizontal_wrapped(|ui| {
                        if ui.small_button(String::from(ICON_DELETE)).on_hover_text("XDEL").clicked() {
                            edit = Some(KvEdit::StreamDelete { id: id.clone() });
                        }
                        ui.label(RichText::new(id).monospace().color(t.muted_fg));
                        for (f, v) in fields {
                            ui.label(RichText::new(format!("{f}={v}")).monospace());
                        }
                    });
                }
            }
            KvValue::Other { type_name, rendered } => {
                ui.label(RichText::new(format!("{type_name} values are shown read-only.")).color(t.muted_fg));
                ui.label(RichText::new(rendered).monospace());
            }
        });
        if let Some(e) = edit {
            self.act(cx, "Update", move |s| async move { s.edit(db, &k, e).await.map(|_| "Saved".to_string()) });
        }
    }

    fn console_ui(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx, t: &theme::Tokens) {
        ui.label(RichText::new(format!("Commands run against db{}. SELECT, MONITOR and SUBSCRIBE are blocked because they would change the shared connection.", self.db)).small().color(t.muted_fg));
        let h = ui.available_height() - 40.0;
        egui::ScrollArea::vertical().max_height(h).stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            for (cmd, out) in &self.console_log {
                ui.label(RichText::new(format!("db{}> {cmd}", self.db)).monospace().color(t.info));
                match out {
                    Ok(s) => ui.label(RichText::new(s).monospace()),
                    Err(e) => ui.label(RichText::new(format!("(error) {e}")).monospace().color(t.destructive)),
                };
            }
        });
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut self.console_input).font(egui::TextStyle::Monospace).hint_text("e.g. HGETALL user:1").desired_width(ui.available_width() - 80.0));
            let hist: Vec<String> = self.console_log.iter().map(|(c, _)| c.clone()).collect();
            if r.has_focus() && !hist.is_empty() {
                if ui.input(|i| i.key_pressed(Key::ArrowUp)) {
                    let p = self.history_pos.map(|p| p.saturating_sub(1)).unwrap_or(hist.len() - 1);
                    self.history_pos = Some(p);
                    self.console_input = hist[p].clone();
                }
                if ui.input(|i| i.key_pressed(Key::ArrowDown))
                    && let Some(p) = self.history_pos
                {
                    let n = (p + 1).min(hist.len() - 1);
                    self.history_pos = Some(n);
                    self.console_input = hist[n].clone();
                }
            }
            let send = (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))) | ui.button("Send").clicked();
            if send && !self.console_input.trim().is_empty() && self.console_job.is_none() {
                let line = std::mem::take(&mut self.console_input);
                let db = self.db;
                if let Some(s) = cx.conns.session(self.conn).and_then(|h| h.kv()).cloned() {
                    let l2 = line.clone();
                    self.console_job = Some(cx.bg.spawn("", async move { s.command(db, &l2).await }));
                    self.console_log.push((line, Ok("…".into())));
                    self.history_pos = None;
                    self.scanned_once = false;
                }
                r.request_focus();
            }
        });
    }
}

fn pair_editor(ui: &mut egui::Ui, a: &mut String, b: &mut String, la: &str, lb: &str, _f: impl FnOnce()) {
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(a).hint_text(la).desired_width(180.0));
        ui.add(egui::TextEdit::singleline(b).hint_text(lb).desired_width(220.0));
    });
}

fn type_color(t: &theme::Tokens, ty: &str) -> egui::Color32 {
    match ty {
        "string" => t.info,
        "hash" => t.accent,
        "list" => t.warning,
        "set" | "zset" => t.ring,
        _ => t.muted_fg,
    }
}
