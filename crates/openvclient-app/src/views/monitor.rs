//! Live monitoring: engine metrics with history, sessions and locks.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use egui::RichText;
use egui_material_icons::icons::*;
use openvclient_core::sql::{ActivitySnapshot, KillMode, Metric};
use openvclient_core::{Result, ResultSet};
use uuid::Uuid;

use crate::app::AppCtx;
use crate::bg::{Job, poll};
use crate::theme::{self, ico};
use crate::widgets::grid::{Grid, GridState, Overlay};

#[derive(PartialEq, Clone, Copy)]
enum View {
    Overview,
    Sessions,
    Locks,
}

pub struct MonitorTab {
    pub conn: Uuid,
    view: View,
    auto: bool,
    every: u64,
    last: Option<Instant>,
    metrics: Option<Result<Vec<Metric>>>,
    metrics_job: Option<Job<Result<Vec<Metric>>>>,
    history: HashMap<String, VecDeque<f64>>,
    activity: Option<Result<ActivitySnapshot>>,
    activity_job: Option<Job<Result<ActivitySnapshot>>>,
    locks: Option<Result<ResultSet>>,
    locks_job: Option<Job<Result<ResultSet>>>,
    kill_job: Option<Job<Result<()>>>,
    grid: GridState,
    lock_grid: GridState,
    generation: u64,
    hide_idle: bool,
}

impl MonitorTab {
    pub fn new(conn: Uuid) -> Self {
        Self {
            conn,
            view: View::Overview,
            auto: true,
            every: 5,
            last: None,
            metrics: None,
            metrics_job: None,
            history: HashMap::new(),
            activity: None,
            activity_job: None,
            locks: None,
            locks_job: None,
            kill_job: None,
            grid: GridState::default(),
            lock_grid: GridState::default(),
            generation: 0,
            hide_idle: true,
        }
    }

    fn refresh(&mut self, cx: &AppCtx) {
        let Some(s) = cx.conns.session(self.conn).and_then(|h| h.sql()).cloned() else { return };
        let caps = s.capabilities();
        self.last = Some(Instant::now());
        if caps.metrics && self.metrics_job.is_none() {
            let s = s.clone();
            self.metrics_job = Some(cx.bg.spawn("", async move { s.metrics().await }));
        }
        if caps.activity && self.activity_job.is_none() && self.view != View::Locks {
            let s = s.clone();
            self.activity_job = Some(cx.bg.spawn("", async move { s.activity().await }));
        }
        if caps.locks && self.locks_job.is_none() && self.view == View::Locks {
            self.locks_job = Some(cx.bg.spawn("", async move { s.locks().await }));
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx) {
        let t = theme::tokens(ui);
        let Some(s) = cx.conns.session(self.conn).and_then(|h| h.sql()).cloned() else {
            crate::views::unsupported(ui, "Not connected");
            return;
        };
        let caps = s.capabilities();
        if let Some(r) = poll(&mut self.metrics_job) {
            if let Ok(ms) = &r {
                for m in ms {
                    if let Some(v) = m.numeric {
                        let h = self.history.entry(format!("{}/{}", m.group, m.name)).or_default();
                        h.push_back(v);
                        if h.len() > 120 {
                            h.pop_front();
                        }
                    }
                }
            }
            self.metrics = Some(r);
        }
        if let Some(r) = poll(&mut self.activity_job) {
            self.activity = Some(r);
            self.generation += 1;
        }
        if let Some(r) = poll(&mut self.locks_job) {
            self.locks = Some(r);
            self.generation += 1;
        }
        if let Some(r) = poll(&mut self.kill_job) {
            match r {
                Ok(()) => cx.toasts.ok("Signal sent"),
                Err(e) => cx.toasts.error("Could not signal session", &e),
            }
            self.last = None;
        }
        let due = self.last.is_none_or(|l| self.auto && l.elapsed() >= Duration::from_secs(self.every));
        if due {
            self.refresh(cx);
        }
        if self.auto {
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }

        ui.horizontal(|ui| {
            crate::views::header(ui, ICON_MONITORING, &cx.profile_name(self.conn), &s.server_info().product);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(String::from(ICON_REFRESH)).on_hover_text("Refresh now").clicked() {
                    self.refresh(cx);
                }
                egui::ComboBox::from_id_salt("every").selected_text(format!("{}s", self.every)).width(56.0).show_ui(ui, |ui| {
                    for s in [2, 5, 10, 30, 60] {
                        ui.selectable_value(&mut self.every, s, format!("{s}s"));
                    }
                });
                ui.checkbox(&mut self.auto, "Auto-refresh");
            });
        });
        ui.horizontal(|ui| {
            if caps.metrics {
                ui.selectable_value(&mut self.view, View::Overview, ico(ICON_INSIGHTS, "Overview"));
            }
            if caps.activity {
                ui.selectable_value(&mut self.view, View::Sessions, ico(ICON_GROUP, "Sessions"));
            }
            if caps.locks && ui.selectable_value(&mut self.view, View::Locks, ico(ICON_LOCK, "Locks")).clicked() {
                self.last = None;
            }
        });
        ui.separator();
        match self.view {
            View::Overview => self.overview(ui, &t),
            View::Sessions => self.sessions(ui, cx, caps.cancel_query, caps.terminate_session),
            View::Locks => match &self.locks {
                None => crate::views::busy(ui, "Loading locks…"),
                Some(Err(e)) => crate::views::error_box(ui, e),
                Some(Ok(rs)) => {
                    let rs = rs.clone();
                    ui.label(RichText::new(format!("{} lock(s)", rs.rows.len())).color(t.muted_fg));
                    Grid {
                        id: egui::Id::unique(("locks", self.conn)),
                        rs: &rs,
                        editable: false,
                        server_sort: None,
                        overlay: Overlay::default(),
                        generation: self.generation,
                        row_offset: 0,
                        server_filter: false,
                    }
                    .show(ui, &mut self.lock_grid);
                }
            },
        }
    }

    fn overview(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) {
        match &self.metrics {
            None => crate::views::busy(ui, "Collecting metrics…"),
            Some(Err(e)) => crate::views::error_box(ui, e),
            Some(Ok(ms)) => {
                let mut groups: Vec<String> = Vec::new();
                for m in ms {
                    if !groups.contains(&m.group) {
                        groups.push(m.group.clone());
                    }
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for g in groups {
                        ui.label(RichText::new(g.to_uppercase()).small().strong().color(t.muted_fg));
                        let items: Vec<&Metric> = ms.iter().filter(|m| m.group == g).collect();
                        crate::widgets::cards::grid(ui, &items, 210.0, 84.0, |ui, m, resp| {
                            ui.add(egui::Label::new(RichText::new(&m.name).small().color(t.muted_fg)).truncate());
                            ui.add(egui::Label::new(RichText::new(pretty_value(&m.value)).size(18.0).strong()).truncate()).on_hover_text(&m.value);
                            if let Some(h) = self.history.get(&format!("{}/{}", m.group, m.name)).filter(|h| h.len() > 1 && h.iter().any(|v| *v != h[0])) {
                                sparkline(ui, h, t.info);
                            }
                            if let Some(hint) = &m.hint {
                                resp.clone().on_hover_text(hint);
                            }
                        });
                        ui.add_space(theme::space::MD);
                    }
                });
            }
        }
    }

    fn sessions(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx, can_cancel: bool, can_kill: bool) {
        let t = theme::tokens(ui);
        let Some(act) = &self.activity else {
            crate::views::busy(ui, "Loading sessions…");
            return;
        };
        let snap = match act {
            Err(e) => {
                crate::views::error_box(ui, &e.clone());
                return;
            }
            Ok(s) => s.clone(),
        };
        // Optionally hide idle connections (PostgreSQL `state`, MySQL `Command`).
        let mut rs = snap.rows.clone();
        let state_col = rs.column_index("state").or_else(|| rs.column_index("Command"));
        if self.hide_idle
            && let Some(c) = state_col
        {
            rs.rows.retain(|r| {
                let v = r[c].display().to_lowercase();
                !(v == "idle" || v == "sleep" || v.is_empty() || v == "null")
            });
        }
        let sel_id = self.grid.selected.and_then(|(r, _)| rs.rows.get(r)).map(|row| row[snap.id_column].to_text().unwrap_or_default());
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} of {} sessions", rs.rows.len(), snap.rows.rows.len())).color(t.muted_fg));
            if state_col.is_some() && ui.checkbox(&mut self.hide_idle, "Hide idle").changed() {
                self.generation += 1;
                self.grid.selected = None;
            }
            ui.separator();
            let busy = self.kill_job.is_some();
            if let Some(id) = &sel_id {
                ui.label(RichText::new(format!("Selected: {id}")).monospace());
                if can_cancel && ui.add_enabled(!busy, egui::Button::new(ico(ICON_STOP, "Cancel query"))).clicked() {
                    let (s, id2) = (cx.conns.session(self.conn).and_then(|h| h.sql()).cloned(), id.clone());
                    if let Some(s) = s {
                        self.kill_job = Some(cx.bg.spawn(format!("Cancel {id2}"), async move { s.kill(&id2, KillMode::CancelQuery).await }));
                    }
                }
                if can_kill
                    && ui.add_enabled(!busy, egui::Button::new(RichText::new(ico(ICON_POWER_SETTINGS_NEW, "Terminate…")).color(t.destructive))).clicked()
                    && let Some(s) = cx.conns.session(self.conn).and_then(|h| h.sql()).cloned()
                {
                    let id2 = id.clone();
                    cx.confirm(
                        Some(self.conn),
                        format!("Terminate session {id2}?"),
                        "The client is disconnected and its open transaction is rolled back.",
                        "Terminate",
                        Some(id2.clone()),
                        move |cx| {
                            crate::views::dialogs::track(cx, format!("Terminate {id2}"), "Session terminated", None, async move { s.kill(&id2, KillMode::Terminate).await });
                        },
                    );
                }
            } else {
                ui.label(RichText::new("Select a row to cancel or terminate it").color(t.muted_fg));
            }
        });
        Grid {
            id: egui::Id::unique(("sessions", self.conn, self.hide_idle)),
            rs: &rs,
            editable: false,
            server_sort: None,
            overlay: Overlay::default(),
            generation: self.generation,
            row_offset: 0,
            server_filter: false,
        }
        .show(ui, &mut self.grid);
    }
}

fn sparkline(ui: &mut egui::Ui, data: &VecDeque<f64>, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 14.0), egui::Sense::hover());
    let (min, max) = data.iter().fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(*v), b.max(*v)));
    let span = (max - min).max(1e-9);
    let n = data.len();
    let pts: Vec<egui::Pos2> =
        data.iter().enumerate().map(|(i, v)| egui::pos2(rect.left() + rect.width() * i as f32 / (n - 1) as f32, rect.bottom() - rect.height() * ((v - min) / span) as f32)).collect();
    ui.painter().add(egui::Shape::line(pts, egui::Stroke::new(1.5, color)));
}

/// Readable metric values: digit grouping for big integers, Yes/No for booleans.
fn pretty_value(v: &str) -> String {
    let t = v.trim();
    match t {
        "true" | "t" => return "Yes".into(),
        "false" | "f" => return "No".into(),
        _ => {}
    }
    if let Ok(n) = t.parse::<i64>()
        && n.unsigned_abs() >= 10_000
    {
        let digits = n.unsigned_abs().to_string();
        let mut out = String::new();
        for (i, ch) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(ch);
        }
        return if n < 0 { format!("-{out}") } else { out };
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn pretty_values() {
        assert_eq!(super::pretty_value("20233239"), "20,233,239");
        assert_eq!(super::pretty_value("9999"), "9999");
        assert_eq!(super::pretty_value("false"), "No");
        assert_eq!(super::pretty_value("99.97"), "99.97");
        assert_eq!(super::pretty_value("-123456"), "-123,456");
    }
}
