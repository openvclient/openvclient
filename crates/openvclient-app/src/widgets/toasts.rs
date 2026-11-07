//! Non-blocking notifications, stacked bottom-right. Errors stay until
//! dismissed; successes fade after a few seconds.

use std::time::{Duration, Instant};

use egui::{Align2, RichText};
use egui_material_icons::icons::*;

use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Success,
    Info,
    Warning,
    Error,
}

struct Toast {
    sticky: bool,
    id: u64,
    level: Level,
    title: String,
    body: Option<String>,
    at: Instant,
    /// Successes with the same prefix that arrived together ("Connected to …"),
    /// merged into one toast so a burst does not flood the screen.
    merged: Vec<String>,
}

/// Prefixes whose bursts are merged into one toast.
const MERGE_PREFIXES: [&str; 2] = ["Connected to ", "Disconnected from "];

#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
    next: u64,
}

impl Toasts {
    pub fn push(&mut self, level: Level, title: impl Into<String>, body: Option<String>) {
        let title = title.into();
        let body = body.filter(|b| !b.is_empty());
        if level == Level::Success
            && body.is_none()
            && let Some(prefix) = MERGE_PREFIXES.iter().find(|p| title.starts_with(**p))
            && let Some(last) = self.items.iter_mut().rev().find(|t| t.level == Level::Success && !t.sticky && t.at.elapsed() < Duration::from_millis(2500))
            && last.title.starts_with(prefix)
        {
            if last.merged.is_empty() {
                last.merged.push(last.title[prefix.len()..].to_string());
            }
            last.merged.push(title[prefix.len()..].to_string());
            last.title = format!("{prefix}{} connections", last.merged.len());
            last.body = Some(last.merged.join(", "));
            last.at = Instant::now();
            return;
        }
        self.next += 1;
        self.items.push(Toast { sticky: false, id: self.next, level, title, body, at: Instant::now(), merged: Vec::new() });
        if self.items.len() > 6 {
            self.items.remove(0);
        }
    }
    /// Stays until dismissed (e.g. a sign-in code the user must type).
    pub fn sticky(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.push(Level::Info, title, Some(body.into()));
        if let Some(t) = self.items.last_mut() {
            t.sticky = true;
        }
    }
    pub fn ok(&mut self, title: impl Into<String>) {
        self.push(Level::Success, title, None);
    }
    pub fn info(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.push(Level::Info, title, Some(body.into()));
    }
    pub fn warn(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.push(Level::Warning, title, Some(body.into()));
    }
    pub fn error(&mut self, title: impl Into<String>, err: &openvclient_core::Error) {
        let title = title.into();
        tracing::warn!(target: "openvclient", "{title}: {}", err.diagnostics().replace('\n', " | "));
        let shown = openvclient_core::redact(&err.to_string());
        let body = match err.hint() {
            Some(h) => format!("{shown}\n\n{h}"),
            None => shown,
        };
        self.push(Level::Error, title, Some(body));
    }
    pub fn error_text(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.push(Level::Error, title, Some(body.into()));
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        let ttl = |l: Level| match l {
            Level::Success => Some(Duration::from_millis(2500)),
            Level::Info => Some(Duration::from_secs(5)),
            Level::Warning => Some(Duration::from_secs(8)),
            Level::Error => None,
        };
        self.items.retain(|t| t.sticky || ttl(t.level).is_none_or(|d| t.at.elapsed() < d));
        if self.items.is_empty() {
            return;
        }
        let mut close = None;
        let mut close_all = false;
        // Narrower on small windows so toasts never hide most of the content.
        let width = (ctx.content_rect().width() * 0.28).clamp(240.0, 360.0);
        egui::Area::new(egui::Id::unique("toasts")).anchor(Align2::RIGHT_BOTTOM, [-16.0, -36.0]).order(egui::Order::Tooltip).show(ctx, |ui| {
            ui.set_max_width(width + 20.0);
            let t = theme::tokens(ui);
            let hidden = self.items.len().saturating_sub(3);
            if hidden > 0 {
                // A real surface, so the count never draws over the content below.
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(width);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("+{hidden} more")).color(t.muted_fg));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Dismiss all").clicked() {
                                close_all = true;
                            }
                        });
                    });
                });
                ui.add_space(theme::space::SM);
            }
            for item in self.items.iter().rev().take(3) {
                let (icon, color) = match item.level {
                    Level::Success => (ICON_CHECK, t.accent),
                    Level::Info => (ICON_INFO, t.info),
                    Level::Warning => (ICON_WARNING, t.warning),
                    Level::Error => (ICON_ERROR, t.destructive),
                };
                egui::Frame::popup(ui.style()).stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.6))).show(ui, |ui| {
                    ui.set_width(width);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(String::from(icon)).color(color).size(theme::icon::LG));
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&item.title).strong());
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.small_button(String::from(ICON_CLOSE)).on_hover_text("Dismiss").clicked() {
                                        close = Some(item.id);
                                    }
                                    if item.body.is_some() && ui.small_button(String::from(ICON_CONTENT_COPY)).on_hover_text("Copy message").clicked() {
                                        ui.ctx().copy_text(format!("{}\n{}", item.title, item.body.clone().unwrap_or_default()));
                                    }
                                });
                            });
                            if let Some(b) = &item.body {
                                let shown: String = b.chars().take(600).collect();
                                ui.label(RichText::new(shown).color(t.muted_fg));
                            }
                        });
                    });
                });
                ui.add_space(theme::space::SM);
            }
        });
        if close_all {
            self.items.clear();
        }
        if let Some(id) = close {
            self.items.retain(|t| t.id != id);
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}
