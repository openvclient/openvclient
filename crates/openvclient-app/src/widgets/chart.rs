//! A quick chart of a result set: pick a label column and a numeric column,
//! shown as bars or a line. Meant for a glance at the shape of the data, not
//! for reports, so it draws at most 500 points.

use egui::{Align2, FontId, Pos2, Rect, RichText, Sense, Stroke, pos2, vec2};
use openvclient_core::{ResultSet, Value};

use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Bar,
    Line,
}

#[derive(Default)]
pub struct ChartState {
    pub x: Option<usize>,
    pub y: Option<usize>,
    pub kind: Kind,
}

const MAX_POINTS: usize = 500;

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::UInt(u) => Some(*u as f64),
        Value::Float(f) => Some(*f),
        Value::Text(t) => t.trim().parse().ok(),
        _ => None,
    }
}

/// Columns whose sampled values are all numbers.
pub fn numeric_columns(rs: &ResultSet) -> Vec<usize> {
    (0..rs.columns.len())
        .filter(|&c| {
            rs.columns[c].numeric || (!rs.rows.is_empty() && rs.rows.iter().take(50).all(|r| r[c].is_null() || number(&r[c]).is_some()) && rs.rows.iter().take(50).any(|r| number(&r[c]).is_some()))
        })
        .collect()
}

fn short(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 {
        format!("{:.1}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if a >= 1e4 {
        format!("{:.1}K", v / 1e3)
    } else if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

pub fn show(ui: &mut egui::Ui, rs: &ResultSet, rows: &[usize], st: &mut ChartState) {
    let t = theme::tokens(ui);
    // Choices from an earlier result may not fit this one.
    let n_cols = rs.columns.len();
    if st.x.is_some_and(|x| x != usize::MAX && x >= n_cols) {
        st.x = None;
    }
    let nums = numeric_columns(rs);
    if st.y.is_some_and(|y| y >= n_cols || !nums.contains(&y)) {
        st.y = None;
    }
    if nums.is_empty() {
        crate::views::unsupported(ui, "A chart needs at least one numeric column.");
        return;
    }
    // Sensible defaults: first non-numeric column as labels, first numeric as values.
    let y = *st.y.get_or_insert(*nums.iter().find(|c| Some(**c) != st.x).unwrap_or(&nums[0]));
    let x = st.x.unwrap_or_else(|| (0..rs.columns.len()).find(|c| !nums.contains(c)).unwrap_or(usize::MAX));
    ui.horizontal(|ui| {
        ui.label(RichText::new("Labels").color(t.muted_fg));
        let xl = if x == usize::MAX { "row number".to_string() } else { rs.columns[x].name.clone() };
        egui::ComboBox::from_id_salt("chart_x").selected_text(xl).show_ui(ui, |ui| {
            if ui.selectable_label(x == usize::MAX, "row number").clicked() {
                st.x = Some(usize::MAX);
            }
            for c in 0..rs.columns.len() {
                if ui.selectable_label(x == c, &rs.columns[c].name).clicked() {
                    st.x = Some(c);
                }
            }
        });
        ui.label(RichText::new("Values").color(t.muted_fg));
        egui::ComboBox::from_id_salt("chart_y").selected_text(&rs.columns[y].name).show_ui(ui, |ui| {
            for &c in &nums {
                if ui.selectable_label(y == c, &rs.columns[c].name).clicked() {
                    st.y = Some(c);
                }
            }
        });
        ui.separator();
        ui.selectable_value(&mut st.kind, Kind::Bar, "Bars");
        ui.selectable_value(&mut st.kind, Kind::Line, "Line");
        if rows.len() > MAX_POINTS {
            ui.label(RichText::new(format!("first {MAX_POINTS} of {} rows", rows.len())).small().color(t.warning));
        }
    });
    let pts: Vec<(String, f64)> = rows
        .iter()
        .take(MAX_POINTS)
        .filter_map(|&r| {
            let v = number(&rs.rows[r][y])?;
            let label = if x == usize::MAX { (r + 1).to_string() } else { rs.rows[r][x].to_text().unwrap_or_else(|| "NULL".into()) };
            Some((label, v))
        })
        .collect();
    if pts.is_empty() {
        crate::views::unsupported(ui, "No numeric values to chart.");
        return;
    }
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ui.available_height().max(180.0)), Sense::hover());
    let p = ui.painter_at(rect);
    let (lo, hi) = pts.iter().fold((0.0_f64, f64::MIN), |(lo, hi), (_, v)| (lo.min(*v), hi.max(*v)));
    let hi = if hi <= lo { lo + 1.0 } else { hi };
    let plot = Rect::from_min_max(pos2(rect.left() + 56.0, rect.top() + 10.0), pos2(rect.right() - 12.0, rect.bottom() - 34.0));
    let ys = |v: f64| plot.bottom() - ((v - lo) / (hi - lo)) as f32 * plot.height();
    // Grid lines and value labels.
    for k in 0..=4 {
        let v = lo + (hi - lo) * k as f64 / 4.0;
        let yy = ys(v);
        p.hline(plot.x_range(), yy, Stroke::new(1.0, t.border.gamma_multiply(0.5)));
        p.text(pos2(plot.left() - 6.0, yy), Align2::RIGHT_CENTER, short(v), FontId::proportional(11.0), t.muted_fg);
    }
    let n = pts.len();
    let step = plot.width() / n as f32;
    let label_every = ((n as f32 * 70.0 / plot.width()).ceil() as usize).max(1);
    let hover = resp.hover_pos();
    let mut tip: Option<(String, f64)> = None;
    let mut line: Vec<Pos2> = Vec::with_capacity(n);
    for (i, (label, v)) in pts.iter().enumerate() {
        let cx = plot.left() + step * (i as f32 + 0.5);
        let top = ys(*v);
        let zero = ys(0.0_f64.clamp(lo, hi));
        let hot = hover.is_some_and(|h| (h.x - cx).abs() <= step / 2.0 && plot.y_range().contains(h.y));
        match st.kind {
            Kind::Bar => {
                let w = (step * 0.7).clamp(1.0, 48.0);
                let r = Rect::from_min_max(pos2(cx - w / 2.0, top.min(zero)), pos2(cx + w / 2.0, top.max(zero)));
                p.rect_filled(r, 2.0, if hot { t.ring } else { t.accent.gamma_multiply(0.85) });
            }
            Kind::Line => {
                line.push(pos2(cx, top));
                if hot {
                    p.circle_filled(pos2(cx, top), 4.0, t.ring);
                }
            }
        }
        if i % label_every == 0 {
            let l: String = label.chars().take(12).collect();
            p.text(pos2(cx, plot.bottom() + 6.0), Align2::CENTER_TOP, l, FontId::proportional(11.0), t.muted_fg);
        }
        if hot {
            tip = Some((label.clone(), *v));
        }
    }
    if st.kind == Kind::Line && line.len() > 1 {
        p.add(egui::Shape::line(line, Stroke::new(2.0, t.accent)));
    }
    p.line_segment([plot.left_bottom(), plot.right_bottom()], Stroke::new(1.0, t.border));
    if let Some((l, v)) = tip {
        resp.on_hover_text(format!("{l}\n{}: {v}", rs.columns[y].name));
    }
}
