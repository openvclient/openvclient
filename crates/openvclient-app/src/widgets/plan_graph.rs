//! Graphical execution plan in the style of SQL Server Management Studio:
//! the root operator sits on the left, its inputs flow in from the right,
//! each box shows the operator's own share of the total cost, and arrow
//! thickness follows the number of rows passed between operators.

use egui::{Align2, FontId, Rect, Sense, Stroke, Vec2, pos2, vec2};
use egui_material_icons::icons::*;
use openvclient_core::explain::PlanNode;

use crate::theme::{self, ico};
use egui::RichText;

const W: f32 = 196.0;
const H: f32 = 78.0;
const GAP_X: f32 = 76.0;
const GAP_Y: f32 = 18.0;
const PAD: f32 = 16.0;

/// Which node is selected, as a path of child indexes from the root.
#[derive(Default, Clone)]
pub struct PlanView {
    pub selected: Option<Vec<usize>>,
    /// Zoom factor; `None` means fit the whole plan into view.
    pub zoom: Option<f32>,
}

struct Placed<'a> {
    node: &'a PlanNode,
    path: Vec<usize>,
    rect: Rect,
    parent: Option<usize>,
}

fn place<'a>(n: &'a PlanNode, depth: usize, path: Vec<usize>, parent: Option<usize>, next_row: &mut usize, out: &mut Vec<Placed<'a>>) {
    // A parent lines up with its first input, like SSMS.
    let row = *next_row;
    let idx = out.len();
    out.push(Placed { node: n, path: path.clone(), rect: Rect::NOTHING, parent });
    if n.children.is_empty() {
        *next_row += 1;
    }
    for (i, c) in n.children.iter().enumerate() {
        let mut p = path.clone();
        p.push(i);
        place(c, depth + 1, p, Some(idx), next_row, out);
    }
    let x = PAD + depth as f32 * (W + GAP_X);
    let y = PAD + row as f32 * (H + GAP_Y);
    out[idx].rect = Rect::from_min_size(pos2(x, y), vec2(W, H));
}

/// The node's own cost: its total minus what its inputs already cost.
fn own_cost(n: &PlanNode) -> Option<f64> {
    let total = n.total_cost?;
    let inputs: f64 = n.children.iter().filter_map(|c| c.total_cost).sum();
    Some((total - inputs).max(0.0))
}

fn rows(n: &PlanNode) -> Option<f64> {
    n.actual_rows.or(n.plan_rows)
}

fn icon(title: &str) -> String {
    String::from(icon_of(title))
}

fn icon_of(title: &str) -> egui_material_icons::MaterialIcon {
    let t = title.to_lowercase();
    let has = |k: &str| t.contains(k);
    if has("seek") || has("index") && !has("scan") {
        ICON_KEY
    } else if has("index") {
        ICON_MANAGE_SEARCH
    } else if has("scan") || has("table") {
        ICON_TABLE_ROWS
    } else if has("join") || has("nested loop") || has("merge") {
        ICON_JOIN_INNER
    } else if has("sort") {
        ICON_SORT
    } else if has("aggregate") || has("group") || has("count") || has("sum") {
        ICON_FUNCTIONS
    } else if has("hash") {
        ICON_TAG
    } else if has("filter") || has("where") {
        ICON_FILTER_ALT
    } else if has("limit") || has("top") {
        ICON_VIEW_LIST
    } else if has("insert") || has("update") || has("delete") || has("modify") {
        ICON_EDIT
    } else if has("parallel") || has("gather") || has("exchange") {
        ICON_CALL_SPLIT
    } else {
        ICON_ACCOUNT_TREE
    }
}

fn fmt_num(v: f64) -> String {
    if v >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if v >= 1e4 {
        format!("{:.1}K", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

fn node_at<'a>(root: &'a PlanNode, path: &[usize]) -> Option<&'a PlanNode> {
    path.iter().try_fold(root, |n, i| n.children.get(*i))
}

impl PlanView {
    pub fn show(&mut self, ui: &mut egui::Ui, root: &PlanNode) {
        let t = theme::tokens(ui);
        let mut placed = Vec::new();
        let mut next_row = 0;
        place(root, 0, Vec::new(), None, &mut next_row, &mut placed);
        let total = root.total_cost.filter(|c| *c > 0.0);
        let max_ms = root.max_actual_ms();
        let size = placed.iter().fold(Vec2::ZERO, |s, p| s.max(p.rect.max.to_vec2())) + Vec2::splat(PAD);
        let selected_path = self.selected.clone();

        let props_h = if selected_path.is_some() { 170.0 } else { 0.0 };
        // Zoom bar: fit the whole plan (default), or step in and out.
        let fit = (ui.available_width() / size.x).min((ui.available_height() - props_h - 40.0).max(120.0) / size.y).clamp(0.35, 1.0);
        let z = self.zoom.unwrap_or(fit);
        ui.horizontal(|ui| {
            if ui.selectable_label(self.zoom.is_none(), ico(ICON_FIT_SCREEN, "Fit")).on_hover_text("Fit the whole plan in view").clicked() {
                self.zoom = None;
            }
            if ui.small_button(String::from(ICON_ZOOM_OUT)).on_hover_text("Zoom out").clicked() {
                self.zoom = Some((z - 0.15).max(0.35));
            }
            ui.label(RichText::new(format!("{:.0}%", z * 100.0)).color(t.muted_fg));
            if ui.small_button(String::from(ICON_ZOOM_IN)).on_hover_text("Zoom in").clicked() {
                self.zoom = Some((z + 0.15).min(2.0));
            }
            if ui.small_button("100%").on_hover_text("Actual size").clicked() {
                self.zoom = Some(1.0);
            }
        });
        let graph_h = (ui.available_height() - props_h).max(160.0);
        egui::ScrollArea::both().id_salt("plan_graph").max_height(graph_h).auto_shrink([false, false]).show(ui, |ui| {
            let (area, _) = ui.allocate_exact_size((size * z).max(vec2(ui.available_width(), 0.0)), Sense::hover());
            // Layout is computed at 100%; scale every rect, offset and font by `z`.
            let sc = |r: Rect| Rect::from_min_size(area.min + r.min.to_vec2() * z, r.size() * z);
            let f = |px: f32| FontId::proportional(px * z);
            let painter = ui.painter_at(area);

            // Arrows first so boxes sit on top. Data flows child -> parent (right to left).
            for p in &placed {
                let Some(pi) = p.parent else { continue };
                let parent = sc(placed[pi].rect);
                let child = sc(p.rect);
                let r = rows(p.node).unwrap_or(1.0);
                let width = (1.0 + (r + 1.0).log10() as f32 * 1.4).clamp(1.0, 9.0) * z.max(0.5);
                let stroke = Stroke::new(width, t.muted_fg.gamma_multiply(0.7));
                let start = pos2(child.left(), child.center().y);
                let end = pos2(parent.right(), parent.top() + 24.0 * z);
                let mid_x = (start.x + end.x) / 2.0;
                painter.line(vec![start, pos2(mid_x, start.y), pos2(mid_x, end.y), pos2(end.x + 6.0 * z, end.y)], stroke);
                // Arrow head into the parent.
                let tip = end;
                painter.add(egui::Shape::convex_polygon(
                    vec![tip, pos2(tip.x + 8.0 + width, tip.y - 4.0 - width / 2.0), pos2(tip.x + 8.0 + width, tip.y + 4.0 + width / 2.0)],
                    t.muted_fg.gamma_multiply(0.8),
                    Stroke::NONE,
                ));
                if let Some(rv) = rows(p.node) {
                    painter.text(pos2((mid_x + start.x) / 2.0, start.y - width / 2.0 - 2.0), Align2::CENTER_BOTTOM, fmt_num(rv), f(10.5), t.muted_fg);
                }
            }

            for p in &placed {
                let rect = sc(p.rect);
                let resp = ui.interact(rect, ui.scope_id().with(("plan_node", &p.path)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                let is_sel = selected_path.as_deref() == Some(p.path.as_slice());
                let share = match (own_cost(p.node), total) {
                    (Some(c), Some(tot)) => Some((c / tot * 100.0).clamp(0.0, 100.0)),
                    _ => None,
                };
                let hot = share.unwrap_or(0.0);
                let accent = if hot >= 50.0 {
                    t.destructive
                } else if hot >= 20.0 {
                    t.warning
                } else {
                    t.accent
                };
                let fill = if is_sel {
                    t.selection
                } else if resp.hovered() {
                    t.muted
                } else {
                    t.card
                };
                painter.rect(rect, 8.0 * z, fill, Stroke::new(if is_sel { 2.0 } else { 1.0 }, if is_sel { t.ring } else { t.border }), egui::StrokeKind::Inside);
                // Cost heat strip on the left edge.
                painter.rect_filled(Rect::from_min_size(rect.min, vec2(4.0 * z, rect.height())), egui::CornerRadius { nw: 8, sw: 8, ne: 0, se: 0 }, accent);

                painter.text(pos2(rect.left() + 20.0 * z, rect.top() + 20.0 * z), Align2::CENTER_CENTER, icon(&p.node.title), f(18.0), accent);
                let title_clip = Rect::from_min_max(pos2(rect.left() + 34.0 * z, rect.top() + 6.0 * z), pos2(rect.right() - 8.0 * z, rect.top() + 46.0 * z));
                let job = egui::text::LayoutJob {
                    wrap: egui::text::TextWrapping { max_width: title_clip.width(), max_rows: 2, break_anywhere: false, overflow_character: Some('…') },
                    ..egui::text::LayoutJob::simple(p.node.title.clone(), f(12.5), t.fg, title_clip.width())
                };
                let g = ui.fonts_mut(|f| f.layout_job(job));
                painter.with_clip_rect(title_clip).galley(title_clip.min, g, t.fg);

                let mut line = Vec::new();
                if let Some(s) = share {
                    line.push(format!("Cost: {s:.0}%"));
                }
                if let Some(ms) = p.node.actual_ms {
                    line.push(format!("{ms:.2} ms"));
                }
                if let Some(r) = rows(p.node) {
                    line.push(format!("{} rows", fmt_num(r)));
                }
                let lc = if hot >= 20.0 { accent } else { t.muted_fg };
                painter.with_clip_rect(rect.shrink(4.0 * z)).text(pos2(rect.left() + 12.0 * z, rect.bottom() - 14.0 * z), Align2::LEFT_CENTER, line.join(" · "), f(11.5), lc);
                if max_ms > 0.0
                    && let Some(ms) = p.node.actual_ms
                {
                    let frac = (ms / max_ms) as f32;
                    let bar = Rect::from_min_size(pos2(rect.left() + 12.0 * z, rect.bottom() - 5.0 * z), vec2((rect.width() - 24.0 * z) * frac.clamp(0.02, 1.0), 2.0));
                    painter.rect_filled(bar, 1.0, accent);
                }
                if resp.clicked() {
                    self.selected = if is_sel { None } else { Some(p.path.clone()) };
                }
                resp.on_hover_ui(|ui| tooltip(ui, p.node, share));
            }
        });

        if let Some(path) = &self.selected
            && let Some(n) = node_at(root, path)
        {
            ui.separator();
            egui::ScrollArea::vertical().id_salt("plan_props").max_height(props_h - 12.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(icon(&n.title)).color(t.accent));
                    ui.label(egui::RichText::new(&n.title).strong());
                });
                props_grid(ui, n, own_cost(n).zip(total).map(|(c, tot)| c / tot * 100.0));
            });
        }
    }
}

fn props_grid(ui: &mut egui::Ui, n: &PlanNode, share: Option<f64>) {
    let t = theme::tokens(ui);
    egui::Grid::new(ui.scope_id().with("props")).num_columns(2).spacing([16.0, 4.0]).striped(true).show(ui, |ui| {
        let mut row = |k: &str, v: String| {
            ui.label(egui::RichText::new(k).color(t.muted_fg));
            ui.label(egui::RichText::new(v).monospace());
            ui.end_row();
        };
        if let Some(s) = share {
            row("Operator cost", format!("{s:.1}% of total"));
        }
        if let Some(c) = n.startup_cost {
            row("Startup cost", format!("{c:.4}"));
        }
        if let Some(c) = n.total_cost {
            row("Subtree cost", format!("{c:.4}"));
        }
        if let Some(r) = n.plan_rows {
            row("Estimated rows", format!("{r:.0}"));
        }
        if let Some(r) = n.actual_rows {
            row("Actual rows", format!("{r:.0}"));
        }
        if let Some(ms) = n.actual_ms {
            row("Actual time", format!("{ms:.3} ms"));
        }
        if let Some(l) = n.loops {
            row("Executions", format!("{l:.0}"));
        }
        for (k, v) in &n.props {
            row(k, v.clone());
        }
    });
}

fn tooltip(ui: &mut egui::Ui, n: &PlanNode, share: Option<f64>) {
    ui.set_max_width(420.0);
    ui.label(egui::RichText::new(&n.title).strong());
    ui.add_space(4.0);
    props_grid(ui, n, share);
}
