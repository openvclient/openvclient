//! SQL code editor: syntax highlighting via the dialect tokenizer and a
//! context-aware completion popup fed by the connection's catalog.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use egui::{FontId, Key, Modifiers, RichText};
use openvclient_core::sql::{Catalog, ObjectKind};
use openvclient_core::sqltext::{Dialect, TokenKind, tokenize};

use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Keyword,
    Table,
    View,
    Column,
    Function,
    Schema,
}

impl ItemKind {
    fn tag(self) -> &'static str {
        match self {
            ItemKind::Keyword => "kw",
            ItemKind::Table => "table",
            ItemKind::View => "view",
            ItemKind::Column => "col",
            ItemKind::Function => "fn",
            ItemKind::Schema => "schema",
        }
    }
}

#[derive(Clone)]
pub struct Item {
    pub label: String,
    pub kind: ItemKind,
    pub detail: String,
}

struct Completion {
    items: Vec<Item>,
    selected: usize,
    /// Char range of the word being replaced.
    start: usize,
    end: usize,
}

#[derive(Default)]
pub struct EditorState {
    completion: Option<Completion>,
    job_cache: Option<(u64, LayoutJob)>,
    last_text_hash: u64,
    /// Request focus on next frame (after opening the tab).
    pub focus: bool,
    accept_next: bool,
}

pub struct EditorOutput {
    /// Cursor as byte offset.
    pub cursor: Option<usize>,
    /// Selected byte range, when non-empty.
    pub selection: Option<(usize, usize)>,
}

fn hash_of(s: &str, extra: u64) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    extra.hash(&mut h);
    h.finish()
}

fn char_to_byte(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(s.len())
}

fn highlight(dialect: Dialect, text: &str, font: FontId, dark: bool) -> LayoutJob {
    let c = theme::syntax(dark);
    let mut job = LayoutJob::default();
    for (kind, span) in tokenize(dialect, text) {
        let color = match kind {
            TokenKind::Keyword => c.keyword,
            TokenKind::Ident => c.ident,
            TokenKind::QuotedIdent => c.quoted,
            TokenKind::String => c.string,
            TokenKind::Number => c.number,
            TokenKind::Comment => c.comment,
            TokenKind::Operator => c.operator,
            TokenKind::Param => c.param,
            TokenKind::Whitespace | TokenKind::Other => c.ident,
        };
        let mut fmt = TextFormat::simple(font.clone(), color);
        if kind == TokenKind::Comment {
            fmt.italics = true;
        }
        job.append(&text[span.start..span.end], 0.0, fmt);
    }
    job
}

/// Map `alias -> table` from FROM/JOIN clauses: `FROM orders o`, `JOIN x AS y`.
fn aliases(dialect: Dialect, text: &str) -> Vec<(String, String)> {
    let toks: Vec<String> =
        tokenize(dialect, text).into_iter().filter(|(k, _)| !matches!(k, TokenKind::Whitespace | TokenKind::Comment)).map(|(_, s)| text[s.start..s.end].trim_matches(['"', '`']).to_string()).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let up = toks[i].to_ascii_uppercase();
        if up == "FROM" || up == "JOIN" || up == "UPDATE" || up == "INTO" {
            let mut j = i + 1;
            let mut table = toks.get(j).cloned().unwrap_or_default();
            // schema.table
            if toks.get(j + 1).map(String::as_str) == Some(".") {
                table = toks.get(j + 2).cloned().unwrap_or_default();
                j += 2;
            }
            let mut k = j + 1;
            if toks.get(k).is_some_and(|t| t.eq_ignore_ascii_case("AS")) {
                k += 1;
            }
            if let Some(alias) = toks.get(k)
                && alias.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !dialect.is_keyword(alias)
            {
                out.push((alias.clone(), table.clone()));
            }
            out.push((table.clone(), table));
        }
        i += 1;
    }
    out
}

fn build_items(dialect: Dialect, catalog: Option<&Catalog>, text: &str, prefix: &str, qualifier: Option<&str>) -> Vec<Item> {
    let p = prefix.to_lowercase();
    let mut items = Vec::new();
    let push = |items: &mut Vec<Item>, label: &str, kind: ItemKind, detail: &str| {
        if label.to_lowercase().starts_with(&p) && !label.eq_ignore_ascii_case(prefix) {
            items.push(Item { label: label.to_string(), kind, detail: detail.to_string() });
        }
    };
    if let Some(q) = qualifier {
        if let Some(cat) = catalog {
            let al = aliases(dialect, text);
            let table = al.iter().find(|(a, _)| a.eq_ignore_ascii_case(q)).map(|(_, t)| t.clone()).unwrap_or_else(|| q.to_string());
            if let Some(cols) = cat.columns.iter().find(|(t, _)| t.eq_ignore_ascii_case(&table)).map(|(_, c)| c) {
                for c in cols {
                    push(&mut items, c, ItemKind::Column, &table);
                }
            }
            // `schema.` completes objects in that schema.
            for (s, name, kind) in &cat.objects {
                if s.eq_ignore_ascii_case(q) {
                    push(&mut items, name, if *kind == ObjectKind::View { ItemKind::View } else { ItemKind::Table }, s);
                }
            }
        }
        return items;
    }
    if let Some(cat) = catalog {
        // Columns of tables referenced in this script come first.
        let al = aliases(dialect, text);
        for (_, t) in &al {
            if let Some(cols) = cat.columns.iter().find(|(n, _)| n.eq_ignore_ascii_case(t)).map(|(_, c)| c) {
                for c in cols {
                    if !items.iter().any(|i: &Item| i.label == *c) {
                        push(&mut items, c, ItemKind::Column, t);
                    }
                }
            }
        }
        for (s, name, kind) in &cat.objects {
            push(&mut items, name, if *kind == ObjectKind::View { ItemKind::View } else { ItemKind::Table }, s);
        }
        for s in &cat.schemas {
            push(&mut items, s, ItemKind::Schema, "");
        }
        for f in cat.functions.iter().take(2000) {
            push(&mut items, f, ItemKind::Function, "");
        }
    }
    for kw in dialect.keywords().iter().chain(dialect.extra_keywords()) {
        push(&mut items, kw, ItemKind::Keyword, "");
    }
    items.truncate(200);
    items
}

pub fn sql_editor(ui: &mut egui::Ui, id: egui::Id, text: &mut String, dialect: Dialect, catalog: Option<&Catalog>, st: &mut EditorState, font_size: f32) -> EditorOutput {
    let focused = ui.memory(|m| m.has_focus(id));
    let mut accept = std::mem::take(&mut st.accept_next);
    if focused && st.completion.is_some() {
        ui.input_mut(|i| {
            let c = st.completion.as_mut().unwrap();
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                c.selected = (c.selected + 1).min(c.items.len().saturating_sub(1));
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                c.selected = c.selected.saturating_sub(1);
            }
            if i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab) {
                accept = true;
            }
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                st.completion = None;
            }
        });
    }
    let explicit = focused && ui.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Space));

    let font = FontId::monospace(font_size);
    let dark = ui.visuals().dark_mode;
    let job_cache = &mut st.job_cache;
    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap: f32| -> Arc<egui::Galley> {
        let s = buf.as_str();
        let h = hash_of(s, dark as u64 ^ (font_size.to_bits() as u64) << 1);
        let mut job = match job_cache {
            Some((k, j)) if *k == h => j.clone(),
            _ => {
                let j = highlight(dialect, s, font.clone(), dark);
                *job_cache = Some((h, j.clone()));
                j
            }
        };
        job.wrap.max_width = wrap;
        ui.fonts_mut(|f| f.layout_job(job))
    };

    // Apply an accepted completion before drawing so the edit shows this frame.
    if accept
        && let Some(c) = st.completion.take()
        && let Some(item) = c.items.get(c.selected)
    {
        let bs = char_to_byte(text, c.start);
        let be = char_to_byte(text, c.end);
        text.replace_range(bs..be, &item.label);
        let new_cursor = c.start + item.label.chars().count();
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
            state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(new_cursor))));
            state.store(ui.ctx(), id);
        }
    }

    // Line-number gutter: reserve a left margin wide enough for the largest
    // line number, then paint the numbers beside each logical line.
    let lines = text.lines().count().max(1) + usize::from(text.ends_with('\n'));
    let digits = lines.to_string().len().max(2) as f32;
    let gutter = digits * font_size * 0.62 + 14.0;
    let out = egui::TextEdit::multiline(text)
        .id(id)
        .code_editor()
        .font(FontId::monospace(font_size))
        .desired_width(f32::INFINITY)
        .desired_rows(4)
        .min_size(egui::vec2(0.0, (ui.available_height() - 2.0).max(60.0)))
        .margin(egui::Margin { left: gutter as i8, right: 6, top: 4, bottom: 4 })
        .lock_focus(true)
        .layouter(&mut layouter)
        .show(ui);
    {
        let tk = crate::theme::tokens(ui);
        let painter = ui.painter_at(out.response.rect);
        let cur_line = out.cursor_range.map(|r| text.chars().take(r.primary.index.0).filter(|c| *c == '\n').count());
        let mut line = 0usize;
        let mut new_line = true;
        let x = out.response.rect.left() + gutter - 8.0;
        for row in &out.galley.rows {
            if new_line {
                let y = out.galley_pos.y + row.rect().center().y;
                let color = if cur_line == Some(line) { tk.fg } else { tk.muted_fg.gamma_multiply(0.7) };
                painter.text(egui::pos2(x, y), egui::Align2::RIGHT_CENTER, (line + 1).to_string(), FontId::monospace(font_size * 0.85), color);
                line += 1;
            }
            new_line = row.ends_with_newline;
        }
        painter.vline(out.response.rect.left() + gutter - 3.0, out.response.rect.y_range(), egui::Stroke::new(1.0, tk.border.gamma_multiply(0.6)));
    }
    if st.focus {
        out.response.request_focus();
        st.focus = false;
    }

    let range = out.cursor_range;
    let cursor_char = range.map(|r| r.primary.index.0);
    let cursor = cursor_char.map(|c| char_to_byte(text, c));
    let selection = range.and_then(|r| {
        let (a, b) = (r.primary.index.0.min(r.secondary.index.0), r.primary.index.0.max(r.secondary.index.0));
        (a != b).then(|| (char_to_byte(text, a), char_to_byte(text, b)))
    });

    // Decide whether to (re)open completion.
    let h = hash_of(text, 0);
    let changed = h != st.last_text_hash;
    st.last_text_hash = h;
    if let Some(ci) = cursor_char.filter(|_| out.response.has_focus()) {
        let chars: Vec<char> = text.chars().collect();
        let mut s = ci.min(chars.len());
        while s > 0 && (chars[s - 1].is_alphanumeric() || chars[s - 1] == '_') {
            s -= 1;
        }
        let prefix: String = chars[s..ci.min(chars.len())].iter().collect();
        let qualifier = if s > 0 && chars[s - 1] == '.' {
            let mut q = s - 1;
            while q > 0 && (chars[q - 1].is_alphanumeric() || chars[q - 1] == '_' || chars[q - 1] == '"' || chars[q - 1] == '`') {
                q -= 1;
            }
            Some(chars[q..s - 1].iter().collect::<String>().trim_matches(['"', '`']).to_string())
        } else {
            None
        };
        let typed_word = changed && (!prefix.is_empty() || qualifier.is_some()) && ci > 0 && {
            let last = chars[ci - 1];
            last.is_alphanumeric() || last == '_' || last == '.'
        };
        if explicit || typed_word {
            let items = build_items(dialect, catalog, text, &prefix, qualifier.as_deref());
            st.completion = (!items.is_empty()).then_some(Completion { items, selected: 0, start: s, end: ci });
        } else if (changed || st.completion.as_ref().is_some_and(|c| c.end != ci)) && !typed_word {
            st.completion = None;
        }
    } else if !out.response.has_focus() {
        st.completion = None;
    }

    // Popup under the cursor.
    if let (Some(c), Some(r)) = (&st.completion, range) {
        let rect = out.galley.pos_from_cursor(r.primary).translate(out.galley_pos.to_vec2());
        let mut clicked = None;
        egui::Area::new(id.with("completion")).order(egui::Order::Foreground).fixed_pos(rect.left_bottom() + egui::vec2(0.0, 2.0)).show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(260.0);
                let start = c.selected.saturating_sub(9);
                for (i, item) in c.items.iter().enumerate().skip(start).take(10) {
                    let sel = i == c.selected;
                    let resp = ui
                        .horizontal(|ui| {
                            let r = ui.selectable_label(sel, RichText::new(&item.label).monospace());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.weak(format!("{} {}", item.detail, item.kind.tag()));
                            });
                            r
                        })
                        .inner;
                    if resp.clicked() {
                        clicked = Some(i);
                    }
                }
                ui.weak(format!("{} matches · ↑↓ Enter Tab · Esc", c.items.len()));
            });
        });
        if let Some(i) = clicked {
            if let Some(c) = st.completion.as_mut() {
                c.selected = i;
            }
            // Accept on the next frame through the same path as Enter.
            st.accept_next = true;
            ui.ctx().request_repaint();
            out.response.request_focus();
        }
    }

    EditorOutput { cursor, selection }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_resolution() {
        let a = aliases(Dialect::Postgres, "select o. from public.orders o join customers as c on c.id = o.cid");
        assert!(a.contains(&("o".into(), "orders".into())));
        assert!(a.contains(&("c".into(), "customers".into())));
    }

    #[test]
    fn completes_columns_after_alias() {
        let mut cat = Catalog::default();
        cat.columns.insert("orders".into(), vec!["id".into(), "total".into()]);
        let items = build_items(Dialect::Postgres, Some(&cat), "select o.t from orders o", "t", Some("o"));
        assert_eq!(items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), vec!["total"]);
    }
}
