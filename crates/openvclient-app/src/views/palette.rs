//! Global search (⌘K): connections, loaded objects, open tabs and commands.

use egui::{Key, RichText};
use egui_material_icons::MaterialIcon;
use egui_material_icons::icons::*;
use openvclient_core::sql::ObjectKind;
use uuid::Uuid;

use crate::app::{AppCtx, Cmd};
use crate::theme;

#[derive(Default)]
pub struct Palette {
    open: bool,
    query: String,
    selected: usize,
    focus: bool,
}

impl Palette {
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.focus = true;
    }

    #[cfg(test)]
    pub fn set_query(&mut self, q: &str) {
        self.query = q.to_string();
    }
}

struct Entry {
    icon: MaterialIcon,
    label: String,
    detail: String,
    score: i32,
    action: Action,
}

enum Action {
    Cmd(Cmd),
    Connect(Uuid),
}

/// Character positions of `needle` in `hay` (case-insensitive): a contiguous
/// run when it occurs as a substring, otherwise a tight subsequence.
fn match_positions(needle: &str, hay: &str) -> Option<Vec<usize>> {
    let h: Vec<char> = hay.to_lowercase().chars().collect();
    let n: Vec<char> = needle.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if n.is_empty() {
        return Some(Vec::new());
    }
    // Prefer a contiguous hit, earliest word start first.
    let starts: Vec<usize> = (0..h.len()).filter(|&i| h[i..].starts_with(&n)).collect();
    if let Some(&i) = starts.iter().find(|&&i| i == 0 || !h[i - 1].is_alphanumeric()).or(starts.first()) {
        return Some((i..i + n.len()).collect());
    }
    let mut out = Vec::with_capacity(n.len());
    let mut hi = 0;
    for nc in &n {
        let pos = (hi..h.len()).find(|&i| h[i] == *nc)?;
        out.push(pos);
        hi = pos + 1;
    }
    // Reject scattered letters ("ord" in "information_schema"): the span must stay tight.
    let span = out.last().unwrap() - out[0] + 1;
    (span <= n.len() + n.len() / 2 + 1).then_some(out)
}

/// Score for `needle` against an entry: the label matters most; the detail
/// only counts when it contains the query outright.
fn fuzzy(needle: &str, hay: &str) -> Option<i32> {
    let q = needle.trim().to_lowercase();
    if q.is_empty() {
        return Some(0);
    }
    let l = hay.to_lowercase();
    let pos = match_positions(&q, hay)?;
    let contiguous = pos.windows(2).all(|w| w[1] == w[0] + 1);
    let at_word = pos[0] == 0 || !l.chars().nth(pos[0] - 1).is_some_and(|c| c.is_alphanumeric());
    let mut score = if l == q {
        1000
    } else if l.starts_with(&q) {
        600
    } else if contiguous && at_word {
        400
    } else if contiguous {
        250
    } else {
        120
    };
    score -= (l.chars().count() as i32 - q.chars().count() as i32).clamp(0, 60);
    Some(score)
}

fn score(q: &str, label: &str, detail: &str) -> Option<i32> {
    fuzzy(q, label).or_else(|| (!q.trim().is_empty() && detail.to_lowercase().contains(&q.trim().to_lowercase())).then_some(40))
}

fn entries(cx: &AppCtx, tabs: &[(u64, String)], q: &str) -> Vec<Entry> {
    let mut v = Vec::new();
    let mut add = |icon, label: String, detail: String, action| {
        if let Some(s) = score(q, &label, &detail) {
            v.push(Entry { icon, label, detail, score: s, action });
        }
    };
    for p in &cx.store.ws.profiles {
        let connected = cx.conns.is_connected(p.id);
        let detail = format!("{} · {} · {}", p.environment.short(), p.summary(), if connected { "connected" } else { "connect" });
        let action = if connected { Action::Cmd(Cmd::NewSql { conn: p.id, database: None, text: String::new() }) } else { Action::Connect(p.id) };
        add(crate::views::driver_icon(&p.driver), p.name.clone(), detail, action);
    }
    for (conn, n) in cx.sidebar.loaded_objects() {
        // Already open: the tab entry below covers it.
        if tabs.iter().any(|(_, t)| t.ends_with(&n.path.display())) {
            continue;
        }
        let action = if n.kind.has_rows() {
            Cmd::OpenTable { conn, kind: n.kind, path: n.path.clone() }
        } else if matches!(n.kind, ObjectKind::Database | ObjectKind::Schema) {
            Cmd::NewSql { conn, database: n.path.database.clone(), text: String::new() }
        } else {
            Cmd::OpenObject { conn, kind: n.kind, path: n.path.clone() }
        };
        add(crate::views::kind_icon(n.kind), n.label.clone(), format!("{} · {} · {}", n.kind.label(), n.path.display(), cx.profile_name(conn)), Action::Cmd(action));
    }
    for (id, title) in tabs {
        // Titles carry a leading icon glyph; match on the words.
        let plain = title.split_once("  ").map(|(_, r)| r.to_string()).unwrap_or_else(|| title.clone());
        add(ICON_TAB, plain, "open tab".into(), Action::Cmd(Cmd::FocusTab(*id)));
    }
    // Snippets open in a new editor on the selected (or only) SQL connection.
    let sql_conn = cx.selected_conn.filter(|c| cx.conns.session(*c).is_some_and(|s| s.sql().is_some())).or_else(|| {
        let ids = cx.conns.connected_ids();
        let sql: Vec<_> = ids.into_iter().filter(|c| cx.conns.session(*c).is_some_and(|s| s.sql().is_some())).collect();
        (sql.len() == 1).then(|| sql[0])
    });
    if let Some(c) = sql_conn {
        for sn in &cx.store.ws.snippets {
            add(ICON_BOOKMARK, sn.name.clone(), format!("snippet · {}", cx.profile_name(c)), Action::Cmd(Cmd::NewSql { conn: c, database: None, text: sn.sql.clone() }));
        }
    }
    add(ICON_ADD, "New connection".into(), theme::kbd("command · ⌘N"), Action::Cmd(Cmd::EditConnection(None)));
    add(ICON_DASHBOARD, "Home".into(), theme::kbd("command · ⌘⇧H"), Action::Cmd(Cmd::OpenHome));
    if let Some(c) = cx.selected_conn.filter(|c| cx.conns.is_connected(*c)) {
        add(ICON_MONITORING, format!("Monitor {}", cx.profile_name(c)), "command".into(), Action::Cmd(Cmd::OpenMonitor(c)));
    }
    // Best first; ties keep open tabs, then connections, then objects.
    v.sort_by_key(|x| std::cmp::Reverse(x.score));
    v.truncate(60);
    v
}

pub fn show(ctx: &egui::Context, cx: &mut AppCtx, tabs: &[(u64, String)]) {
    if !cx.palette.open {
        return;
    }
    let items = entries(cx, tabs, &cx.palette.query.clone());
    let (down, up, enter, esc) = ctx.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
            i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
            i.consume_key(egui::Modifiers::NONE, Key::Enter),
            i.consume_key(egui::Modifiers::NONE, Key::Escape),
        )
    });
    let p = &mut cx.palette;
    if down {
        p.selected = (p.selected + 1).min(items.len().saturating_sub(1));
    }
    if up {
        p.selected = p.selected.saturating_sub(1);
    }
    let mut chosen = enter.then_some(p.selected);
    let mut close = esc;
    let resp = egui::Modal::new(egui::Id::unique("palette")).show(ctx, |ui| {
        ui.set_width(620.0);
        let t = theme::tokens(ui);
        let r =
            ui.add(egui::TextEdit::singleline(&mut cx.palette.query).hint_text("Search connections, tables, open tabs, commands…").font(egui::FontId::proportional(16.0)).desired_width(f32::INFINITY));
        if cx.palette.focus {
            r.request_focus();
            cx.palette.focus = false;
        }
        if r.changed() {
            cx.palette.selected = 0;
        }
        ui.add_space(theme::space::SM);
        if items.is_empty() {
            ui.label(RichText::new("No matches. Objects appear here once their folder has been opened in the sidebar.").color(t.muted_fg));
        }
        let row_h = 30.0;
        let moved = down || up;
        let sel = cx.palette.selected;
        egui::ScrollArea::vertical().max_height(420.0).auto_shrink([false, true]).show_rows(ui, row_h, items.len(), |ui, range| {
            for i in range {
                let e = &items[i];
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                let selected = i == sel;
                if selected {
                    ui.painter().rect_filled(rect, 6.0, t.selection);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 6.0, t.muted);
                }
                let p = ui.painter().with_clip_rect(rect.shrink(2.0));
                let cy = rect.center().y;
                p.text(egui::pos2(rect.left() + 18.0, cy), egui::Align2::CENTER_CENTER, String::from(e.icon), egui::FontId::proportional(theme::icon::MD), t.muted_fg);
                // Matched letters in the accent colour.
                let hits = match_positions(&cx.palette.query, &e.label).unwrap_or_default();
                let mut job = egui::text::LayoutJob::default();
                for (ci, ch) in e.label.chars().enumerate() {
                    let hit = hits.contains(&ci);
                    let fmt = egui::TextFormat {
                        font_id: egui::FontId::proportional(14.0),
                        color: if hit { t.accent } else { t.fg },
                        underline: if hit { egui::Stroke::new(1.0, t.accent) } else { egui::Stroke::NONE },
                        ..Default::default()
                    };
                    job.append(&ch.to_string(), 0.0, fmt);
                }
                let label = p.layout_job(job);
                let lw = label.size().x.min(rect.width() * 0.55);
                p.with_clip_rect(egui::Rect::from_min_max(egui::pos2(rect.left() + 34.0, rect.top()), egui::pos2(rect.left() + 34.0 + lw, rect.bottom()))).galley(
                    egui::pos2(rect.left() + 34.0, cy - label.size().y / 2.0),
                    label,
                    t.fg,
                );
                let dx = rect.left() + 34.0 + lw + 12.0;
                let detail_clip = egui::Rect::from_min_max(egui::pos2(dx, rect.top()), egui::pos2(rect.right() - 8.0, rect.bottom()));
                p.with_clip_rect(detail_clip).text(egui::pos2(dx, cy), egui::Align2::LEFT_CENTER, &e.detail, egui::FontId::proportional(12.0), t.muted_fg);
                if selected && moved {
                    resp.scroll_to_me(None);
                }
                if resp.clicked() {
                    chosen = Some(i);
                }
            }
        });
        ui.add_space(theme::space::XS);
        ui.label(RichText::new("↑↓ to move · Enter to open · Esc to close").small().color(t.muted_fg));
    });
    if resp.should_close() {
        close = true;
    }
    if let Some(i) = chosen {
        if let Some(e) = items.into_iter().nth(i) {
            match e.action {
                Action::Cmd(c) => cx.cmd(c),
                Action::Connect(id) => {
                    cx.selected_conn = Some(id);
                    cx.cmd(Cmd::Connect(id));
                }
            }
        }
        close = true;
    }
    if close {
        cx.palette.open = false;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fuzzy_prefers_contiguous() {
        let a = super::fuzzy("ord", "orders").unwrap();
        let b = super::fuzzy("ord", "o_r_d_x").unwrap();
        assert!(a > b);
        assert!(super::fuzzy("xyz", "orders").is_none());
    }

    #[test]
    fn scattered_letters_do_not_match() {
        assert!(super::fuzzy("ord", "information_schema").is_none());
        assert!(super::fuzzy("ord", "performance_schema").is_none());
        assert!(super::fuzzy("ord", "Docker Desktop").is_none());
        assert!(super::fuzzy("ord", "sales.orders").unwrap() > super::fuzzy("ord", "reorder_log").unwrap());
        assert_eq!(super::match_positions("ord", "sales.orders"), Some(vec![6, 7, 8]));
    }
}
