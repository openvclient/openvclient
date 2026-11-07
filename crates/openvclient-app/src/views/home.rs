//! Home: connection overview with health, quick actions and shortcuts.

use egui::RichText;
use egui_material_icons::icons::*;

use crate::app::{AppCtx, Cmd};
use crate::conns::ConnState;
use crate::theme::{self, ico};

pub fn ui(ui: &mut egui::Ui, cx: &mut AppCtx) {
    let t = theme::tokens(ui);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(theme::space::LG);
        ui.horizontal(|ui| {
            ui.add_space(theme::space::SM);
            ui.vertical(|ui| {
                ui.label(RichText::new("Welcome to OpenVClient").size(24.0).strong());
                ui.label(RichText::new("Databases, caches and containers in one fast workspace.").color(t.muted_fg));
            });
        });
        ui.add_space(theme::space::XL);

        // Last session's tabs, opened only when asked (they may need to connect).
        if !cx.restore_offer.is_empty() {
            let n = cx.restore_offer.len();
            let mut names: Vec<String> = cx.restore_offer.iter().map(|t| cx.profile_name(t.conn())).collect();
            names.dedup();
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(String::from(ICON_HISTORY)).size(20.0).color(t.info));
                    ui.vertical(|ui| {
                        ui.label(RichText::new(format!("Pick up where you left off: {n} tab{}", if n == 1 { "" } else { "s" })).strong());
                        ui.label(RichText::new(format!("Connects to {}", names.join(", "))).small().color(t.muted_fg));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Dismiss").clicked() {
                            cx.restore_offer.clear();
                        }
                        if theme::primary_button(ui, ico(ICON_RESTORE, "Restore")).clicked() {
                            cx.restore_session();
                        }
                    });
                });
            });
            ui.add_space(theme::space::LG);
        }

        // Connection overview cards; favourites are pinned in their own section first.
        let mut profiles = cx.store.ws.profiles.clone();
        profiles.sort_by(|a, b| b.last_connected_at.cmp(&a.last_connected_at).then(a.name.cmp(&b.name)));
        let (card_w, card_h, gap) = (280.0_f32, 112.0_f32, theme::space::MD);
        let favs: Vec<_> = cx.store.ws.favorites.iter().filter_map(|id| profiles.iter().find(|p| p.id == *id).cloned()).collect();
        if !favs.is_empty() {
            section(ui, "FAVOURITES", &format!("{} pinned", favs.len()));
            card_grid(ui, cx, &favs, card_w, card_h, gap);
            ui.add_space(theme::space::LG);
        }
        let all = profiles.len();
        profiles.retain(|p| !cx.store.is_favorite(p.id));
        let title = if favs.is_empty() { "CONNECTIONS" } else { "OTHER CONNECTIONS" };
        ui.horizontal(|ui| {
            section(ui, title, &format!("{all} saved · {} connected", cx.conns.connected_ids().len()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let s = &mut cx.store.ws.settings;
                let before = s.home_by_folder;
                ui.selectable_value(&mut s.home_by_folder, true, ico(ICON_FOLDER, "By folder"));
                ui.selectable_value(&mut s.home_by_folder, false, ico(ICON_SCHEDULE, "Recent first"));
                if before != s.home_by_folder {
                    cx.dirty = true;
                }
            });
        });
        if favs.is_empty() && !profiles.is_empty() {
            ui.label(RichText::new(format!("{}  Star a connection to pin it here.", String::from(ICON_STAR))).small().color(t.muted_fg));
            ui.add_space(theme::space::XS);
        }
        if profiles.is_empty() {
            card(ui, |ui| {
                ui.label(RichText::new("No connections yet").strong());
                ui.label(RichText::new("Add PostgreSQL, MySQL/MariaDB, SQLite, Redis or Docker. Passwords go to the system keychain.").color(t.muted_fg));
                ui.add_space(theme::space::SM);
                if theme::primary_button(ui, ico(ICON_ADD, "Add your first connection")).clicked() {
                    cx.cmd(Cmd::EditConnection(None));
                }
            });
        }
        if cx.store.ws.settings.home_by_folder {
            // One group per sidebar folder, in folder order; loose connections last.
            let mut folders: Vec<String> = profiles.iter().map(|p| p.folder.trim_matches('/').to_string()).collect();
            folders.sort_by_key(|f| (f.is_empty(), f.to_lowercase()));
            folders.dedup();
            for f in folders {
                let mut group: Vec<_> = profiles.iter().filter(|p| p.folder.trim_matches('/') == f).cloned().collect();
                group.sort_by_key(|p| p.name.to_lowercase());
                ui.add_space(theme::space::XS);
                ui.label(RichText::new(ico(if f.is_empty() { ICON_LINK_OFF } else { ICON_FOLDER }, if f.is_empty() { "No folder" } else { &f })).strong().color(t.muted_fg));
                ui.add_space(theme::space::XS);
                card_grid(ui, cx, &group, card_w, card_h, gap);
            }
        } else {
            card_grid(ui, cx, &profiles, card_w, card_h, gap);
        }

        recent_queries(ui, cx);

        ui.add_space(theme::space::XL);
        // The full connector list matters on first run; afterwards it folds away.
        let n = cx.conns.registry.all().len();
        egui::CollapsingHeader::new(RichText::new(format!("ADD A CONNECTION  ·  {n} connectors")).small().strong().color(t.muted_fg)).id_salt("home_connectors").default_open(all == 0).show(
            ui,
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    let infos: Vec<openvclient_core::DriverInfo> = cx.conns.registry.all().iter().map(|d| d.info().clone()).collect();
                    for i in infos {
                        let r = if theme::is_cartoon() {
                            // Room on the left for the connector sticker, painted over the button.
                            let r = ui.add(egui::Button::new(format!("        {}", i.name))).on_hover_text(i.description);
                            theme::paint_connector_sticker(ui.painter(), egui::pos2(r.rect.left() + 17.0, r.rect.center().y), 18.0, i.id, ui.visuals().dark_mode);
                            r
                        } else {
                            ui.add(egui::Button::new(ico(crate::views::driver_icon(i.id), i.name))).on_hover_text(i.description)
                        };
                        if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            cx.cmd(Cmd::NewConnectionOf(i.id));
                        }
                    }
                });
            },
        );

        ui.add_space(theme::space::SM);
        ui.horizontal(|ui| {
            ui.label(RichText::new(theme::kbd("Keyboard shortcuts: ⌘/ (also in Settings)")).small().color(t.muted_fg));
            if ui.small_button("Show").clicked() {
                cx.show_shortcuts = true;
            }
        });
    });
}

/// Recent statements across all connections; click one to open it again.
fn recent_queries(ui: &mut egui::Ui, cx: &mut AppCtx) {
    let t = theme::tokens(ui);
    let recent: Vec<crate::store::HistoryEntry> = cx.store.ws.history.iter().filter(|h| cx.store.profile(h.profile).is_some()).take(6).cloned().collect();
    if recent.is_empty() {
        return;
    }
    ui.add_space(theme::space::LG);
    section(ui, "RECENT QUERIES", "click to open in a new editor");
    let w = ui.available_width().min(1200.0);
    egui::Frame::new().fill(t.card).stroke(egui::Stroke::new(1.0, t.border)).corner_radius(8).inner_margin(6).show(ui, |ui| {
        ui.set_width(w - 12.0);
        for h in recent {
            let first = h.sql.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("--")).unwrap_or("").to_string();
            let name = cx.profile_name(h.profile);
            let r = ui
                .horizontal(|ui| {
                    let (icon, color) = if h.ok { (ICON_CHECK_CIRCLE, t.accent) } else { (ICON_ERROR, t.destructive) };
                    ui.label(RichText::new(String::from(icon)).color(color));
                    let txt_w = (ui.available_width() - 260.0).max(120.0);
                    ui.allocate_ui_with_layout(egui::vec2(txt_w, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.set_width(txt_w);
                        ui.add(egui::Label::new(RichText::new(&first).monospace()).truncate());
                    });
                    ui.add(egui::Label::new(RichText::new(format!("{name} · {}", short_time(&h.at))).small().color(t.muted_fg)).truncate());
                })
                .response
                .interact(egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(&h.sql);
            if r.clicked() {
                cx.cmd(Cmd::NewSql { conn: h.profile, database: None, text: h.sql.clone() });
            }
        }
    });
}

/// "2026-11-07 09:12:03" → "Nov 7, 09:12" (falls back to the raw text).
fn short_time(at: &str) -> String {
    chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S").map(|d| d.format("%b %-d, %H:%M").to_string()).unwrap_or_else(|_| at.to_string())
}

/// Cards in an even grid that reflows with the window width; cards stretch
/// to share the row so names have room.
fn card_grid(ui: &mut egui::Ui, cx: &mut AppCtx, profiles: &[openvclient_core::ConnectionProfile], card_w: f32, card_h: f32, gap: f32) {
    let cols = (((ui.available_width() + gap) / (card_w + gap)).floor() as usize).clamp(1, 4);
    let card_w = ((ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32).floor().min(card_w * 1.5);
    for row in profiles.iter().take(48).collect::<Vec<_>>().chunks(cols) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for (i, p) in row.iter().enumerate() {
                if i > 0 {
                    ui.add_space(gap);
                }
                connection_card(ui, cx, p, card_w, card_h);
            }
        });
        ui.add_space(gap);
    }
}

fn connection_card(ui: &mut egui::Ui, cx: &mut AppCtx, p: &openvclient_core::ConnectionProfile, w: f32, h: f32) {
    let t = theme::tokens(ui);
    let state = cx.conns.state(p.id);
    let (dot, status) = match state {
        Some(ConnState::Connected(l)) => (t.accent, l.latency.map(|d| format!("Connected · {}", openvclient_core::elapsed_ms(d))).unwrap_or_else(|| "Connected".into())),
        Some(ConnState::Connecting { .. }) => (t.info, "Connecting…".into()),
        Some(ConnState::Failed(e)) => (t.destructive, e.message.clone()),
        None => (t.muted_fg, "Not connected · click to connect".into()),
    };
    let product = match state {
        Some(ConnState::Connected(l)) => l.handle.base().server_info().product,
        _ => crate::views::driver_label(cx, &p.driver),
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let hovered = resp.hovered();
    theme::paint_card(ui, rect, hovered);
    let env = theme::env_color(ui, p.environment);
    // Environment stripe on the left edge (also stated in text by the badge).
    ui.painter().rect_filled(egui::Rect::from_min_size(rect.min + egui::vec2(0.0, 10.0), egui::vec2(3.0, h - 20.0)), 2.0, env);
    let inner = rect.shrink2(egui::vec2(14.0, 12.0));
    // A child Ui draws inside the allocated card without claiming more layout space.
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::top_down(egui::Align::Min)));
    {
        let ui = &mut child;
        ui.spacing_mut().item_spacing = egui::vec2(theme::space::SM, 4.0);
        ui.horizontal(|ui| {
            crate::views::driver_glyph(ui, &p.driver, theme::icon::LG, env);
            // Name left-aligned and truncated; the badge keeps a fixed slot on the right.
            // Measure the badge so the name gets every spare pixel.
            let badge_w = ui.painter().layout_no_wrap(p.environment.short().to_string(), egui::FontId::proportional(11.0), t.fg).size().x + 16.0;
            let name_w = (ui.available_width() - badge_w - theme::icon::LG - 18.0).max(40.0);
            ui.allocate_ui_with_layout(egui::vec2(name_w, 22.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(RichText::new(&p.name).strong().size(14.5)).truncate());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                theme::badge(ui, p.environment.short(), env).on_hover_text(crate::views::env_tip(p.environment));
                star_button(ui, cx, p.id);
            });
        });
        ui.add(egui::Label::new(RichText::new(p.summary()).small().color(t.muted_fg)).truncate());
        ui.add(egui::Label::new(RichText::new(product).small().color(t.muted_fg)).truncate());
        ui.horizontal(|ui| {
            theme::dot(ui, dot);
            ui.add(egui::Label::new(RichText::new(status).small()).truncate());
        });
    }
    let resp = resp.on_hover_text(format!("{} · {}", p.name, p.environment.label()));
    if resp.clicked() {
        cx.selected_conn = Some(p.id);
        if cx.conns.session(p.id).is_some() {
            crate::views::open_connection(cx, p);
        } else {
            cx.cmd(Cmd::Connect(p.id));
        }
    }
}

fn section(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).small().strong().color(theme::muted(ui)));
        if !detail.is_empty() {
            ui.label(RichText::new(detail).small().color(theme::muted(ui)));
        }
    });
    ui.add_space(theme::space::XS);
}

fn card(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
    let t = theme::tokens(ui);
    egui::Frame::new().fill(t.card).stroke(egui::Stroke::new(1.0, t.border)).corner_radius(10).inner_margin(14).show(ui, |ui| {
        ui.set_max_width(720.0);
        body(ui);
    });
}

/// Star toggle: pins the connection to the Favourites section of Home.
pub fn star_button(ui: &mut egui::Ui, cx: &mut AppCtx, id: uuid::Uuid) {
    let fav = cx.store.is_favorite(id);
    let t = theme::tokens(ui);
    let (glyph, color, tip) = if fav { (ICON_STAR, t.warning, "Remove from favourites") } else { (ICON_STAR, t.muted_fg.gamma_multiply(0.4), "Add to favourites") };
    let r = ui.add(egui::Label::new(RichText::new(String::from(glyph)).size(theme::icon::LG).color(color)).sense(egui::Sense::click()).selectable(false));
    if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
        cx.store.toggle_favorite(id);
        cx.mark_dirty();
    }
}
