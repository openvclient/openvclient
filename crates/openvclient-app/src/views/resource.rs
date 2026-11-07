//! Generic browser for resource sessions (MongoDB, Elasticsearch, S3, FTP,
//! SSH, Kafka, RabbitMQ…): node content, engine actions and a console.

use egui::{Key, RichText};
use egui_material_icons::MaterialIcon;
use egui_material_icons::icons::*;
use openvclient_core::resource::{ActionOutcome, ActionSpec, Args, Content, Node, NodeIcon, OpenOptions};
use openvclient_core::{FieldKind, Result};
use uuid::Uuid;

use crate::app::AppCtx;
use crate::bg::{Job, poll};
use crate::theme::{self, ico};
use crate::widgets::grid::{Grid, GridEvent, GridState, Overlay, detail_window};

pub fn node_icon(i: NodeIcon) -> MaterialIcon {
    match i {
        NodeIcon::Server => ICON_DNS,
        NodeIcon::Database => ICON_DATABASE,
        NodeIcon::Collection => ICON_DATA_OBJECT,
        NodeIcon::Index => ICON_SEARCH,
        NodeIcon::Folder => ICON_FOLDER,
        NodeIcon::File => ICON_DESCRIPTION,
        NodeIcon::Bucket => ICON_INVENTORY_2,
        NodeIcon::Topic => ICON_STREAM,
        NodeIcon::Queue => ICON_QUEUE,
        NodeIcon::Exchange => ICON_HUB,
        NodeIcon::Key => ICON_KEY,
        NodeIcon::Table => ICON_TABLE,
        NodeIcon::Terminal => ICON_TERMINAL,
        NodeIcon::Info => ICON_INFO,
        NodeIcon::User => ICON_PERSON,
    }
}

struct ActionForm {
    spec: ActionSpec,
    node: String,
    values: Args,
}

pub struct ResourceTab {
    pub conn: Uuid,
    pub node: Option<Node>,
    content: Option<Result<Content>>,
    job: Option<Job<Result<Content>>>,
    filter: String,
    limit: u64,
    offset: u64,
    grid: GridState,
    generation: u64,
    form: Option<ActionForm>,
    action_job: Option<Job<Result<ActionOutcome>>>,
    console_open: bool,
    console_input: String,
    console_log: Vec<(String, std::result::Result<Content, String>)>,
    console_job: Option<Job<Result<Content>>>,
    pending_action: Option<&'static str>,
}

impl ResourceTab {
    pub fn new(conn: Uuid, node: Option<Node>, action: Option<&'static str>) -> Self {
        let mut t = Self {
            conn,
            node: None,
            content: None,
            job: None,
            filter: String::new(),
            limit: 100,
            offset: 0,
            grid: GridState::default(),
            generation: 0,
            form: None,
            action_job: None,
            console_open: node.is_none(),
            console_input: String::new(),
            console_log: Vec::new(),
            console_job: None,
            pending_action: action,
        };
        t.node = node;
        t
    }

    /// Navigate this tab to another node of the same connection.
    pub fn navigate(&mut self, node: Option<Node>, action: Option<&'static str>) {
        let same = self.node.as_ref().map(|n| &n.id) == node.as_ref().map(|n| &n.id);
        if !same {
            self.node = node;
            self.content = None;
            self.job = None;
            self.filter.clear();
            self.offset = 0;
            self.grid = GridState::default();
        }
        self.pending_action = action;
    }

    fn load(&mut self, cx: &AppCtx) {
        let (Some(s), Some(n)) = (cx.conns.session(self.conn).and_then(|h| h.resource()).cloned(), self.node.clone()) else { return };
        if !n.openable {
            self.content = Some(Ok(Content::Empty(format!("{} — choose an item inside it, or use the actions above.", n.label))));
            return;
        }
        let opts = OpenOptions { limit: self.limit, offset: self.offset, filter: self.filter.trim().to_string() };
        self.job = Some(cx.bg.spawn(format!("Loading {}", n.label), async move { s.open(&n.id, opts).await }));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx, tab_id: u64) {
        let t = theme::tokens(ui);
        let Some(s) = cx.conns.session(self.conn).and_then(|h| h.resource()).cloned() else {
            crate::views::unsupported(ui, "Not connected");
            return;
        };
        if let Some(r) = poll(&mut self.job) {
            self.content = Some(r);
            self.generation += 1;
            self.grid = GridState::default();
        }
        if let Some(j) = cx.resource_jobs.remove(&self.conn) {
            self.action_job = Some(j);
        }
        if let Some(r) = poll(&mut self.action_job) {
            match r {
                Ok(o) => {
                    cx.toasts.ok(o.message.clone());
                    if let Some(c) = o.content {
                        self.content = Some(Ok(c));
                        self.generation += 1;
                    } else {
                        self.content = None;
                    }
                    if let Some(id) = o.refresh {
                        cx.sidebar.refresh_resource(self.conn, &id);
                    }
                }
                Err(e) => cx.toasts.error("Action failed", &e),
            }
        }
        if let Some(r) = poll(&mut self.console_job)
            && let Some(last) = self.console_log.last_mut()
        {
            last.1 = r.map_err(|e| e.to_string());
        }
        if self.content.is_none() && self.job.is_none() && self.node.is_some() {
            self.load(cx);
        }
        let kind = self.node.as_ref().map(|n| n.kind.clone()).unwrap_or_default();
        let actions = s.actions(&kind);
        if let Some(a) = self.pending_action.take()
            && let Some(spec) = actions.iter().find(|x| x.id == a)
        {
            self.start_action(cx, spec.clone());
        }

        // Header + actions.
        ui.horizontal(|ui| {
            match &self.node {
                Some(n) => crate::views::header(ui, node_icon(n.icon), &n.label, n.detail.as_deref().unwrap_or(&n.kind)),
                None => crate::views::header(ui, ICON_DNS, &cx.profile_name(self.conn), &s.server_info().product),
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(String::from(ICON_REFRESH)).on_hover_text("Reload").clicked() {
                    self.content = None;
                }
                if s.console_language().is_some() {
                    ui.toggle_value(&mut self.console_open, ico(ICON_TERMINAL, "Console"));
                }
                let secondary: Vec<&ActionSpec> = actions.iter().filter(|a| !a.primary).collect();
                if !secondary.is_empty() {
                    ui.menu_button(ico(ICON_MORE_VERT, "Actions"), |ui| {
                        for a in secondary {
                            let label = if a.destructive { RichText::new(a.label).color(t.destructive) } else { RichText::new(a.label) };
                            if ui.button(label).clicked() {
                                self.start_action(cx, a.clone());
                                ui.close();
                            }
                        }
                    });
                }
                for a in actions.iter().filter(|a| a.primary).rev() {
                    if ui.button(a.label).clicked() {
                        self.start_action(cx, a.clone());
                    }
                }
                if self.action_job.is_some() {
                    ui.add(egui::Spinner::new());
                }
            });
        });
        // Filter / paging.
        if let Some(hint) = s.filter_hint(&kind).filter(|_| self.node.as_ref().is_some_and(|n| n.openable)) {
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.filter).font(egui::TextStyle::Monospace).hint_text(hint).desired_width(ui.available_width() - 330.0));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if enter || ui.button(ico(ICON_FILTER_LIST, "Apply")).clicked() {
                    self.offset = 0;
                    self.content = None;
                }
                if ui.add_enabled(self.offset > 0, egui::Button::new(String::from(ICON_CHEVRON_LEFT))).clicked() {
                    self.offset = self.offset.saturating_sub(self.limit);
                    self.content = None;
                }
                ui.label(RichText::new(format!("{}–{}", self.offset + 1, self.offset + self.limit)).monospace());
                if ui.button(String::from(ICON_CHEVRON_RIGHT)).clicked() {
                    self.offset += self.limit;
                    self.content = None;
                }
                ui.label("per page");
                if ui.add(egui::DragValue::new(&mut self.limit).range(10..=5000)).lost_focus() {
                    self.content = None;
                }
            });
        }
        ui.add_space(theme::space::XS);

        // Console at the bottom.
        if self.console_open
            && let Some(lang) = s.console_language()
        {
            let node_id = self.node.as_ref().map(|n| n.id.clone());
            egui::Panel::bottom(egui::Id::unique(("res_console", tab_id))).resizable(true).default_size(220.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(ico(ICON_TERMINAL, lang)).strong());
                    if let Some(n) = &self.node {
                        ui.label(RichText::new(format!("in {}", n.label)).small().color(t.muted_fg));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Clear").clicked() {
                            self.console_log.clear();
                        }
                    });
                });
                let h = ui.available_height() - 36.0;
                egui::ScrollArea::vertical().max_height(h).stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                    for (cmd, out) in &self.console_log {
                        ui.label(RichText::new(format!("> {cmd}")).monospace().color(t.info));
                        match out {
                            Ok(c) => content_inline(ui, c),
                            Err(e) => {
                                ui.label(RichText::new(e).monospace().color(t.destructive));
                            }
                        }
                    }
                });
                ui.horizontal(|ui| {
                    let r = ui.add(egui::TextEdit::singleline(&mut self.console_input).font(egui::TextStyle::Monospace).desired_width(ui.available_width() - 70.0));
                    let go = (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))) | ui.button("Run").clicked();
                    if go && !self.console_input.trim().is_empty() && self.console_job.is_none() {
                        let line = std::mem::take(&mut self.console_input);
                        let (s2, l2) = (s.clone(), line.clone());
                        self.console_job = Some(cx.bg.spawn("", async move { s2.console(node_id.as_deref(), &l2).await }));
                        self.console_log.push((line, Ok(Content::Empty("…".into()))));
                        r.request_focus();
                    }
                });
            });
        }

        egui::CentralPanel::no_frame().show(ui, |ui| {
            if self.node.is_none() && self.content.is_none() {
                crate::views::unsupported(ui, "Pick an item in the sidebar to browse it, or open the console.");
                return;
            }
            match &self.content {
                None => crate::views::busy(ui, "Loading…"),
                Some(Err(e)) => crate::views::error_box(ui, &e.clone()),
                Some(Ok(c)) => {
                    let c = c.clone();
                    match c {
                        Content::Table(rs) | Content::TableWithNote(rs, _) => {
                            if let Some(Ok(Content::TableWithNote(_, note))) = &self.content {
                                ui.label(RichText::new(note).small().color(t.muted_fg));
                            }
                            let ev = Grid {
                                id: egui::Id::unique(("resgrid", tab_id, self.generation)),
                                rs: &rs,
                                editable: false,
                                server_sort: None,
                                overlay: Overlay::default(),
                                generation: self.generation,
                                row_offset: self.offset,
                                server_filter: false,
                            }
                            .show(ui, &mut self.grid);
                            detail_window(ui.ctx(), egui::Id::unique(("resdetail", tab_id)), &rs, &mut self.grid);
                            for e in ev {
                                if let GridEvent::CopyRows(rows, f) = e {
                                    ui.ctx().copy_text(crate::export::render(&rs, &rows, None, f, None, ""));
                                    cx.toasts.ok(format!("Copied {} row(s)", rows.len()));
                                }
                            }
                        }
                        other => {
                            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| content_inline(ui, &other));
                        }
                    }
                }
            }
        });
        self.form_ui(ui.ctx(), cx);
    }

    fn start_action(&mut self, cx: &mut AppCtx, spec: ActionSpec) {
        let node = self.node.as_ref().map(|n| n.id.clone()).unwrap_or_default();
        if spec.fields.is_empty() {
            self.submit(cx, ActionForm { spec, node, values: Args::new() });
        } else {
            let values = spec.fields.iter().map(|f| (f.key.to_string(), f.default.to_string())).collect();
            self.form = Some(ActionForm { spec, node, values });
        }
    }

    fn submit(&mut self, cx: &mut AppCtx, f: ActionForm) {
        let Some(s) = cx.conns.session(self.conn).and_then(|h| h.resource()).cloned() else { return };
        let label = f.spec.label;
        let id = f.spec.id;
        let destructive = f.spec.destructive;
        let run = move |cx: &mut AppCtx| -> Job<Result<ActionOutcome>> { cx.bg.spawn(label.to_string(), async move { s.run_action(&f.node, id, f.values).await }) };
        if destructive {
            let what = self.node.as_ref().map(|n| n.label.clone()).unwrap_or_else(|| cx.profile_name(self.conn));
            let conn = self.conn;
            cx.confirm(Some(self.conn), format!("{label}?"), format!("{label} on {what}. This cannot be undone."), label, Some(what.clone()), move |cx| {
                // The tab picks the job up on its next frame.
                let job = run(cx);
                cx.resource_jobs.insert(conn, job);
            });
        } else {
            self.action_job = Some(run(cx));
        }
    }

    fn form_ui(&mut self, ctx: &egui::Context, cx: &mut AppCtx) {
        let Some(mut f) = self.form.take() else { return };
        let mut keep = true;
        let mut go = false;
        egui::Modal::new(egui::Id::unique("res_action")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.heading(f.spec.label);
            ui.add_space(theme::space::SM);
            egui::Grid::new("res_form").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                for fs in &f.spec.fields {
                    let r = ui.label(if fs.required { format!("{} *", fs.label) } else { fs.label.to_string() });
                    if !fs.help.is_empty() {
                        r.on_hover_text(fs.help);
                    }
                    let v = f.values.entry(fs.key.to_string()).or_default();
                    match &fs.kind {
                        FieldKind::Bool => {
                            let mut b = v == "true";
                            if ui.checkbox(&mut b, "").changed() {
                                *v = b.to_string();
                            }
                        }
                        FieldKind::Choice(opts) => {
                            egui::ComboBox::from_id_salt(fs.key).selected_text(openvclient_core::choice_label(fs.key, v)).show_ui(ui, |ui| {
                                for o in *opts {
                                    ui.selectable_value(v, o.to_string(), openvclient_core::choice_label(fs.key, o));
                                }
                            });
                        }
                        FieldKind::File { save, .. } => {
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(v).desired_width(300.0));
                                if ui.button("Browse…").clicked() {
                                    let p = if *save { rfd::FileDialog::new().set_file_name(v.as_str()).save_file() } else { rfd::FileDialog::new().pick_file() };
                                    if let Some(p) = p {
                                        *v = p.display().to_string();
                                    }
                                }
                            });
                        }
                        FieldKind::Secret => {
                            ui.add(egui::TextEdit::singleline(v).password(true).desired_width(300.0));
                        }
                        _ if fs.key.contains("body") || fs.key.contains("document") || fs.key.contains("json") => {
                            ui.add(egui::TextEdit::multiline(v).code_editor().desired_rows(6).desired_width(300.0).hint_text(fs.placeholder));
                        }
                        _ => {
                            ui.add(egui::TextEdit::singleline(v).hint_text(fs.placeholder).desired_width(300.0));
                        }
                    }
                    ui.end_row();
                }
            });
            ui.add_space(theme::space::MD);
            ui.horizontal(|ui| {
                let primary = if f.spec.destructive { theme::danger_button(ui, f.spec.label) } else { theme::primary_button(ui, f.spec.label) };
                if primary.clicked() {
                    let missing = f.spec.fields.iter().find(|x| x.required && f.values.get(x.key).is_none_or(|v| v.trim().is_empty()));
                    match missing {
                        Some(m) => cx.toasts.warn("Missing value", format!("{} is required", m.label)),
                        None => go = true,
                    }
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    keep = false;
                }
            });
        });
        if go {
            self.submit(cx, f);
        } else if keep {
            self.form = Some(f);
        }
    }
}

fn content_inline(ui: &mut egui::Ui, c: &Content) {
    let t = theme::tokens(ui);
    match c {
        Content::Json(s) | Content::Text(s) => {
            let mut s = s.clone();
            ui.add(egui::TextEdit::multiline(&mut s).code_editor().interactive(false).desired_width(f32::INFINITY));
        }
        Content::Props(kv) => {
            egui::Grid::new(ui.next_auto_id()).num_columns(2).spacing([24.0, 4.0]).striped(true).show(ui, |ui| {
                for (k, v) in kv {
                    ui.label(RichText::new(k).color(t.muted_fg));
                    ui.add(egui::Label::new(RichText::new(v).monospace()).wrap());
                    ui.end_row();
                }
            });
        }
        Content::Table(rs) | Content::TableWithNote(rs, _) => {
            let text = crate::export::render(rs, &(0..rs.rows.len().min(200)).collect::<Vec<_>>(), None, crate::export::Format::Markdown, None, "");
            ui.label(RichText::new(text).monospace().small());
        }
        Content::Empty(m) => {
            ui.label(RichText::new(m).color(t.muted_fg));
        }
        Content::Pdf(b) => {
            ui.label(RichText::new(format!("PDF ({}); open it in the file manager to see it", openvclient_core::containers::human_bytes(b.len() as u64))).color(t.muted_fg));
        }
        Content::Image(b) => {
            ui.label(RichText::new(format!("Image ({}); open it in the file manager to see it", openvclient_core::containers::human_bytes(b.len() as u64))).color(t.muted_fg));
        }
    }
}
