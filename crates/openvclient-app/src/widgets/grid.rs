//! Virtualized result grid: only visible rows are laid out, so 100k-row
//! results scroll smoothly.

use std::collections::{HashMap, HashSet};

use egui::{Align, Layout, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};
use openvclient_core::{ResultSet, Value};

use crate::theme;

#[derive(Default)]
pub struct GridState {
    pub selected: Option<(usize, usize)>,
    /// Extra selected rows (shift/cmd-click), by data index.
    pub selected_rows: HashSet<usize>,
    /// Client-side sort (column, ascending). Ignored when `server_sort`.
    pub sort: Option<(usize, bool)>,
    pub filter: String,
    view: Vec<usize>,
    view_key: Option<(usize, Option<(usize, bool)>, String, u64)>,
    pub editing: Option<(usize, usize, String)>,
    pub detail_open: bool,
    /// Data columns kept in place on the left while the rest scroll sideways.
    pub frozen: usize,
    /// Shared vertical scroll offset of the frozen and scrolling parts.
    scroll_y: f32,
}

impl GridState {
    /// Data row indexes in display order (after filter and sort).
    pub fn view(&mut self, rs: &ResultSet, generation: u64, server_sort: bool) -> &[usize] {
        let key = (rs.rows.len(), if server_sort { None } else { self.sort }, self.filter.clone(), generation);
        if self.view_key.as_ref() != Some(&key) {
            let needle = self.filter.to_lowercase();
            let mut v: Vec<usize> = (0..rs.rows.len()).filter(|&i| needle.is_empty() || rs.rows[i].iter().any(|c| c.display().to_lowercase().contains(&needle))).collect();
            if let (Some((c, asc)), false) = (self.sort, server_sort) {
                v.sort_by(|&a, &b| {
                    let o = rs.rows[a][c].cmp_for_sort(&rs.rows[b][c]);
                    if asc { o } else { o.reverse() }
                });
            }
            self.view = v;
            self.view_key = Some(key);
        }
        &self.view
    }

    pub fn selected_rows_sorted(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.selected_rows.iter().copied().collect();
        if let Some((r, _)) = self.selected
            && !v.contains(&r)
        {
            v.push(r);
        }
        v.sort();
        v
    }
}

pub enum GridEvent {
    HeaderClicked(usize),
    CommitEdit {
        row: usize,
        col: usize,
        text: String,
    },
    SetNull {
        row: usize,
        col: usize,
    },
    DeleteRows(Vec<usize>),
    RevertRow(usize),
    CopyRows(Vec<usize>, crate::export::Format),
    /// Keep only rows whose `col` equals (or, with `exclude`, differs from) `value`.
    FilterBy {
        col: usize,
        value: Value,
        exclude: bool,
    },
}

/// Per-row and per-cell decoration supplied by the owner (pending edits).
#[derive(Default)]
pub struct Overlay<'a> {
    pub edited: Option<&'a HashMap<(usize, usize), Option<String>>>,
    pub deleted: Option<&'a HashSet<usize>>,
    pub inserted_from: Option<usize>,
}

pub struct Grid<'a> {
    pub id: egui::Id,
    pub rs: &'a ResultSet,
    pub editable: bool,
    pub server_sort: Option<&'a [(String, openvclient_core::sql::SortDir)]>,
    pub overlay: Overlay<'a>,
    pub generation: u64,
    /// Row numbers start here (table paging offset).
    pub row_offset: u64,
    /// The owner filters on the server, so "Exclude this value" is offered too.
    pub server_filter: bool,
}

/// Grid display options from Settings (set once per frame by the app).
static DISPLAY: std::sync::RwLock<(String, bool)> = std::sync::RwLock::new((String::new(), true));

pub fn set_display(null: &str, short_timestamps: bool) {
    if let Ok(mut d) = DISPLAY.write()
        && (d.0 != null || d.1 != short_timestamps)
    {
        *d = (null.to_string(), short_timestamps);
    }
}

/// "2026-11-06 03:24:26.677559+05:30" → "2026-11-06 03:24:26+05:30".
fn trim_fraction(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let ok = b.len() >= 20 && b[4] == b'-' && b[7] == b'-' && (b[10] == b' ' || b[10] == b'T') && b[13] == b':' && b[16] == b':' && b[19] == b'.';
    if !ok {
        return None;
    }
    let frac_end = s[20..].find(|c: char| !c.is_ascii_digit()).map(|i| 20 + i).unwrap_or(s.len());
    Some(format!("{}{}", &s[..19], &s[frac_end..]))
}

fn cell_text(v: &Value) -> (String, bool) {
    let (null, short) = DISPLAY.read().map(|d| d.clone()).unwrap_or_default();
    match v {
        Value::Null => (if null.is_empty() { "NULL".into() } else { null }, true),
        Value::Text(t) if short => (trim_fraction(t).unwrap_or_else(|| t.clone()), false),
        other => (other.display().into_owned(), false),
    }
}

fn initial_width(rs: &ResultSet, c: usize) -> f32 {
    let mut chars = rs.columns[c].name.len().max(rs.columns[c].type_name.len() / 2 + 2);
    for r in rs.rows.iter().take(50) {
        chars = chars.max(r[c].display().chars().count().min(60));
    }
    (chars as f32 * 7.4 + 24.0).clamp(56.0, 360.0)
}

impl<'a> Grid<'a> {
    /// The grid: optional frozen columns on the left, the rest in a sideways
    /// scroll area; both parts share one vertical scroll position.
    pub fn show(self, ui: &mut Ui, st: &mut GridState) -> Vec<GridEvent> {
        let mut events = Vec::new();
        let rs = self.rs;
        if rs.columns.is_empty() {
            ui.weak("No columns");
            return events;
        }
        let server_sort = self.server_sort.is_some();
        let view: Vec<usize> = st.view(rs, self.generation, server_sort).to_vec();
        let n = rs.columns.len();
        let frozen = st.frozen.min(n.saturating_sub(1));
        let y0 = st.scroll_y;
        ui.horizontal_top(|ui| {
            let mut ys = Vec::new();
            if frozen > 0 {
                let w = 52.0 + (0..frozen).map(|c| initial_width(rs, c)).sum::<f32>() + 8.0;
                ui.allocate_ui_with_layout(egui::vec2(w, ui.available_height()), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(w);
                    ys.push(self.part(ui, st, &view, 0..frozen, true, y0, false, &mut events));
                });
                // A visible edge between the frozen and the scrolling columns.
                let x = ui.cursor().left();
                ui.painter().vline(x, ui.max_rect().y_range(), egui::Stroke::new(2.0, theme::tokens(ui).border));
            }
            // Inside a sideways scroll area the table needs a definite width:
            // the columns' natural width (plus room to widen them), or the view.
            let avail = ui.available_width();
            let natural = if frozen == 0 { 52.0 } else { 0.0 } + (frozen..n).map(|c| initial_width(rs, c)).sum::<f32>() + 16.0;
            let width = if natural > avail { natural + 240.0 } else { avail };
            let height = ui.available_height();
            egui::ScrollArea::horizontal().id_salt(self.id.with("hscroll")).auto_shrink([false, false]).show(ui, |ui| {
                ui.allocate_ui_with_layout(egui::vec2(width, height), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(width);
                    ys.push(self.part(ui, st, &view, frozen..n, frozen == 0, y0, true, &mut events));
                });
            });
            // Whichever part the user scrolled wins; the other follows next frame.
            st.scroll_y = ys.iter().copied().find(|y| (y - y0).abs() > 0.5).unwrap_or(y0);
            if frozen > 0 && (st.scroll_y - y0).abs() > 0.5 {
                ui.ctx().request_repaint();
            }
        });
        events
    }

    #[allow(clippy::too_many_arguments)]
    fn part(&self, ui: &mut Ui, st: &mut GridState, view: &[usize], cols: std::ops::Range<usize>, row_numbers: bool, y: f32, scrollbar: bool, events: &mut Vec<GridEvent>) -> f32 {
        let rs = self.rs;
        let server_sort = self.server_sort.is_some();
        let frozen_part = !scrollbar;
        let dark = ui.visuals().dark_mode;
        let muted = theme::muted(ui);
        // Pending-change tints derive from semantic tokens; the row label also says "new".
        let t = theme::tokens(ui);
        let tint = if dark { 0.22 } else { 0.14 };
        let edit_bg = t.warning.gamma_multiply(tint);
        let del_bg = t.destructive.gamma_multiply(tint);
        let new_bg = t.accent.gamma_multiply(tint);
        let mono = egui::TextStyle::Monospace.resolve(ui.style());
        let row_h = mono.size + 9.0;

        let mut tb = TableBuilder::new(ui)
            .id_salt(self.id.with(("part", cols.start)))
            .striped(true)
            .resizable(!frozen_part)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .vertical_scroll_offset(y)
            .scroll_bar_visibility(if scrollbar { egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded } else { egui::scroll_area::ScrollBarVisibility::AlwaysHidden })
            .cell_layout(Layout::left_to_right(Align::Center));
        if row_numbers {
            tb = tb.column(Column::exact(52.0));
        }
        for c in cols.clone() {
            tb = if frozen_part { tb.column(Column::exact(initial_width(rs, c)).clip(true)) } else { tb.column(Column::initial(initial_width(rs, c)).at_least(36.0).clip(true).resizable(true)) };
        }
        let out = tb
            .header(row_h + 12.0, |mut h| {
                if row_numbers {
                    h.col(|ui| {
                        ui.weak("#");
                    });
                }
                for c in cols.clone() {
                    let col = &rs.columns[c];
                    h.col(|ui| {
                        let arrow = match (self.server_sort, st.sort) {
                            (Some(order), _) => order.iter().find(|(n, _)| n == &col.name).map(|(_, d)| if *d == openvclient_core::sql::SortDir::Asc { " ▲" } else { " ▼" }),
                            (None, Some((sc, asc))) if sc == c => Some(if asc { " ▲" } else { " ▼" }),
                            _ => None,
                        };
                        let resp = ui
                            .vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                ui.label(RichText::new(format!("{}{}", col.name, arrow.unwrap_or(""))).strong());
                                if !col.type_name.is_empty() {
                                    ui.label(RichText::new(&col.type_name).small().color(muted));
                                }
                            })
                            .response
                            .interact(Sense::click());
                        if resp.clicked() {
                            if server_sort {
                                events.push(GridEvent::HeaderClicked(c));
                            } else {
                                st.sort = match st.sort {
                                    Some((sc, true)) if sc == c => Some((c, false)),
                                    Some((sc, false)) if sc == c => None,
                                    _ => Some((c, true)),
                                };
                            }
                        }
                        let resp = resp.on_hover_text("Click to sort · right-click to freeze columns");
                        resp.context_menu(|ui| {
                            if ui.button(format!("Freeze columns up to {}", col.name)).clicked() {
                                st.frozen = c + 1;
                                ui.close();
                            }
                            if st.frozen > 0 && ui.button("Unfreeze columns").clicked() {
                                st.frozen = 0;
                                ui.close();
                            }
                        });
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, view.len(), |mut row| {
                    let vi = row.index();
                    let r = view[vi];
                    let deleted = self.overlay.deleted.is_some_and(|d| d.contains(&r));
                    let inserted = self.overlay.inserted_from.is_some_and(|f| r >= f);
                    let row_selected = st.selected.map(|s| s.0) == Some(r) || st.selected_rows.contains(&r);
                    row.set_selected(row_selected);
                    let rn = row_numbers.then(|| {
                        row.col(|ui| {
                            let label = if inserted { "new".to_string() } else { (self.row_offset + vi as u64 + 1).to_string() };
                            ui.label(RichText::new(label).color(muted).monospace());
                        })
                        .1
                    });
                    if let Some(rn) = rn
                        && rn.clicked()
                    {
                        let m = ui_mods(&rn);
                        if m.command || m.shift {
                            if !st.selected_rows.insert(r) {
                                st.selected_rows.remove(&r);
                            }
                        } else {
                            st.selected_rows.clear();
                            st.selected = Some((r, 0));
                        }
                    }
                    for c in cols.clone() {
                        let editing_here = st.editing.as_ref().is_some_and(|(er, ec, _)| *er == r && *ec == c);
                        let pending = self.overlay.edited.and_then(|e| e.get(&(r, c)));
                        let (_, resp) = row.col(|ui| {
                            let rect = ui.max_rect();
                            if deleted {
                                ui.painter().rect_filled(rect, 0.0, del_bg);
                            } else if inserted {
                                ui.painter().rect_filled(rect, 0.0, new_bg);
                            } else if pending.is_some() {
                                ui.painter().rect_filled(rect, 0.0, edit_bg);
                            }
                            if st.selected == Some((r, c)) {
                                ui.painter().rect_stroke(rect.shrink(1.0), 2.0, egui::Stroke::new(1.5, theme::tokens(ui).ring), egui::StrokeKind::Inside);
                            }
                            if editing_here {
                                let (_, _, buf) = st.editing.as_mut().unwrap();
                                let te = ui.add(egui::TextEdit::singleline(buf).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY));
                                te.request_focus();
                                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                    st.editing = None;
                                } else if te.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                    let (er, ec, text) = st.editing.take().unwrap();
                                    events.push(GridEvent::CommitEdit { row: er, col: ec, text });
                                }
                                return;
                            }
                            let (text, is_null) = match pending {
                                Some(None) => ("NULL".to_string(), true),
                                Some(Some(t)) => (t.clone(), false),
                                None => cell_text(&rs.rows[r][c]),
                            };
                            let mut rt = RichText::new(text).monospace();
                            if is_null {
                                rt = rt.italics().color(muted);
                            }
                            if deleted {
                                rt = rt.strikethrough();
                            }
                            // Untyped columns (SQLite views, expressions) still right-align numbers.
                            let numeric = !is_null && (rs.columns[c].numeric || rs.rows[r][c].is_numeric());
                            if numeric {
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(rt));
                            } else {
                                ui.add(egui::Label::new(rt).truncate());
                            }
                        });
                        if resp.clicked() {
                            st.selected = Some((r, c));
                            if !ui_mods(&resp).command {
                                st.selected_rows.clear();
                            }
                        }
                        if resp.double_clicked() {
                            if self.editable && !deleted {
                                let cur = match pending {
                                    Some(v) => v.clone().unwrap_or_default(),
                                    None => rs.rows[r][c].to_text().unwrap_or_default(),
                                };
                                st.editing = Some((r, c, cur));
                            } else {
                                st.detail_open = true;
                            }
                        }
                        resp.context_menu(|ui| {
                            st.selected = Some((r, c));
                            if ui.button("Copy value").clicked() {
                                let v = pending.cloned().unwrap_or_else(|| rs.rows[r][c].to_text()).unwrap_or_else(|| "NULL".into());
                                ui.ctx().copy_text(v);
                                ui.close();
                            }
                            let rows = {
                                let s = st.selected_rows_sorted();
                                if s.is_empty() { vec![r] } else { s }
                            };
                            ui.menu_button("Copy rows as", |ui| {
                                for f in crate::export::Format::ALL {
                                    if ui.button(f.label()).clicked() {
                                        events.push(GridEvent::CopyRows(rows.clone(), f));
                                        ui.close();
                                    }
                                }
                            });
                            if ui.button("View value…").clicked() {
                                st.detail_open = true;
                                ui.close();
                            }
                            ui.separator();
                            let v = rs.rows[r][c].clone();
                            if ui.button(format!("Filter: {} = this value", rs.columns[c].name)).clicked() {
                                events.push(GridEvent::FilterBy { col: c, value: v.clone(), exclude: false });
                                ui.close();
                            }
                            if self.server_filter && ui.button(format!("Exclude: {} ≠ this value", rs.columns[c].name)).clicked() {
                                events.push(GridEvent::FilterBy { col: c, value: v, exclude: true });
                                ui.close();
                            }
                            if self.editable {
                                ui.separator();
                                if ui.button("Edit cell").clicked() {
                                    st.editing = Some((r, c, rs.rows[r][c].to_text().unwrap_or_default()));
                                    ui.close();
                                }
                                if ui.button("Set NULL").clicked() {
                                    events.push(GridEvent::SetNull { row: r, col: c });
                                    ui.close();
                                }
                                if ui.button(format!("Delete {} row(s)", rows.len())).clicked() {
                                    events.push(GridEvent::DeleteRows(rows.clone()));
                                    ui.close();
                                }
                                if (pending.is_some() || deleted || inserted) && ui.button("Revert row").clicked() {
                                    events.push(GridEvent::RevertRow(r));
                                    ui.close();
                                }
                            }
                        });
                    }
                });
            });
        out.state.clamped_offset().y
    }
}

fn ui_mods(r: &egui::Response) -> egui::Modifiers {
    r.ctx.input(|i| i.modifiers)
}

/// Side window showing the full selected value (long text, JSON, binary).
pub fn detail_window(ctx: &egui::Context, id: egui::Id, rs: &ResultSet, st: &mut GridState) {
    if !st.detail_open {
        return;
    }
    let Some((r, c)) = st.selected else {
        st.detail_open = false;
        return;
    };
    let Some(v) = rs.rows.get(r).and_then(|row| row.get(c)) else {
        st.detail_open = false;
        return;
    };
    let mut open = true;
    egui::Window::new(format!("{} · row {}", rs.columns[c].name, r + 1)).id(id.with("detail")).open(&mut open).default_size([520.0, 360.0]).show(ctx, |ui| {
        let mut text = match v {
            Value::Text(s) => match serde_json::from_str::<serde_json::Value>(s) {
                Ok(j) if s.trim_start().starts_with(['{', '[']) => serde_json::to_string_pretty(&j).unwrap_or_else(|_| s.clone()),
                _ => s.clone(),
            },
            other => other.to_text().unwrap_or_else(|| "NULL".into()),
        };
        ui.horizontal(|ui| {
            ui.weak(format!("{} · {} chars", rs.columns[c].type_name, text.chars().count()));
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(text.clone());
            }
        });
        egui::ScrollArea::both().show(ui, |ui| {
            ui.add(egui::TextEdit::multiline(&mut text).code_editor().desired_width(f32::INFINITY).interactive(true));
        });
    });
    if !open {
        st.detail_open = false;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn trims_fractional_seconds() {
        assert_eq!(super::trim_fraction("2026-11-06 03:24:26.677559+05:30").as_deref(), Some("2026-11-06 03:24:26+05:30"));
        assert_eq!(super::trim_fraction("2026-11-06T03:24:26.5Z").as_deref(), Some("2026-11-06T03:24:26Z"));
        assert_eq!(super::trim_fraction("2026-11-06 03:24:26"), None);
        assert_eq!(super::trim_fraction("hello world, this is text"), None);
    }
}
