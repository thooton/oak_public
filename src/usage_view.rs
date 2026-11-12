// The Usage page, and its preview on Home under the accounts: charts of
// what usage.rs keeps. One y-axis per plot, 2px lines over a faint area
// wash, end dots ringed in the surface colour, a legend whenever there are
// two series or more, the focus account in the accent with the others grey,
// and a crosshair whose tooltip lists every series.

use crate::usage::RecExt;
use crate::time::{hour_min, local_offset_ms, month_day};
use crate::{Theme, api, openrouter, usage};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Id, Pos2, Rect, RichText, Sense, Stroke, Ui, UiBuilder, pos2, vec2};

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

/// Cache reads, cache writes, input, output: in this order everywhere.
const CATEGORY: [Color32; 4] =
    [Color32::from_rgb(0x6a, 0xa8, 0x4f), Color32::from_rgb(0x39, 0x87, 0xe5), Color32::from_rgb(0xc9, 0x85, 0x00), Color32::from_rgb(0x90, 0x85, 0xe9)];
const CATEGORY_NAMES: [&str; 4] = ["cache read", "cache write", "input", "output"];

fn categories() -> Vec<(&'static str, Color32)> {
    CATEGORY_NAMES.iter().copied().zip(CATEGORY).collect()
}

/// A legend entry per category, for the column charts.
fn legend(parts: &[(&str, Color32)]) -> Vec<(String, Color32, bool)> {
    parts.iter().map(|(n, c)| (n.to_string(), *c, true)).collect()
}

const SMALL: f32 = 11.5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Range {
    Day,
    Week,
    Month,
}

impl Range {
    const ALL: [Range; 3] = [Range::Day, Range::Week, Range::Month];

    fn label(self) -> &'static str {
        ["24h", "7d", "30d"][self as usize]
    }

    /// Buckets in a column chart, and how long each is.
    fn n(self) -> usize {
        [24, 7, 30][self as usize]
    }

    fn step(self) -> i64 {
        if self == Range::Day { HOUR } else { DAY }
    }

    /// The first bucket's start, in whole local hours or days, so the last
    /// bucket is the current one.
    fn from(self, now: i64) -> i64 {
        let (off, step) = (local_offset_ms(), self.step());
        (now + off).div_euclid(step) * step - off - (self.n() as i64 - 1) * step
    }

    fn before(self) -> &'static str {
        ["vs the 24h before", "vs the 7 days before", "vs the 30 days before"][self as usize]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Units,
    Cost,
    Tokens,
    Limits,
    OpenRouter,
}

impl Tab {
    const ALL: [Tab; 5] = [Tab::Units, Tab::Cost, Tab::Tokens, Tab::Limits, Tab::OpenRouter];

    fn label(self) -> &'static str {
        ["$ per 1%", "API cost", "Tokens", "Limits", "OpenRouter"][self as usize]
    }
}

/// What the page shows, and what was worked out for it last.
pub struct View {
    pub range: Range,
    pub tab: Tab,
    /// The account the page is about (its email); None: all of them.
    pub acct: Option<String>,
    data: Option<(DataKey, Data)>,
    preview: Option<(u64, String, Preview)>,
}

impl Default for View {
    fn default() -> View {
        View { range: Range::Week, tab: Tab::Units, acct: None, data: None, preview: None }
    }
}

/// When to work the data out again: a new record, other filters, another
/// minute, or another current account.
#[derive(Clone, PartialEq)]
struct DataKey {
    version: u64,
    range: Range,
    acct: Option<String>,
    minute: i64,
    current: String,
}

/// One account's measurements of what 1% bought: each one, and the last
/// three pooled (their dollars over their percents).
#[derive(Default)]
struct Measured {
    raw: Vec<(i64, f64)>,
    steps: Vec<i32>,
    mean: Vec<(i64, f64)>,
    models: Vec<String>,
}

impl Measured {
    fn of(samples: &[usage::Sample], acct: &str) -> Measured {
        let mine: Vec<&usage::Sample> = samples.iter().filter(|s| s.acct == acct).collect();
        let values: Vec<(f64, i32)> = mine.iter().map(|s| (s.value, s.step)).collect();
        let mean = usage::pooled(&values, 3);
        Measured {
            raw: mine.iter().map(|s| (s.t, s.value)).collect(),
            steps: mine.iter().map(|s| s.step).collect(),
            mean: mine.iter().zip(mean).map(|(s, m)| (s.t, m)).collect(),
            models: mine.iter().map(|s| s.model.clone()).collect(),
        }
    }

    fn last(&self) -> Option<f64> {
        self.mean.last().map(|p| p.1)
    }

    /// The mean as it stood at `t`.
    fn at(&self, t: i64) -> Option<f64> {
        self.mean.iter().rev().find(|p| p.0 <= t).map(|p| p.1)
    }
}

struct Data {
    now: i64,
    from: i64,
    /// Every account in the log or signed in: (email, plan).
    accounts: Vec<(String, String)>,
    /// The account in the accent: the one picked, or the current one.
    focus: Option<String>,
    five: Vec<(String, Measured)>,
    week: Vec<(String, Measured)>,
    /// Claude replies (the picked account's) per bucket, and in all.
    claude: Vec<usage::Bucket>,
    claude_total: usage::Bucket,
    claude_before: usage::Bucket,
    /// Every reply the filter takes (OpenRouter's too, for all accounts).
    tokens: Vec<usage::Bucket>,
    tokens_total: usage::Bucket,
    by_model: Vec<(String, bool, usage::Bucket)>,
    openrouter: Vec<usage::Bucket>,
    openrouter_total: usage::Bucket,
    util5: Vec<(String, Vec<(i64, f64)>)>,
    util7: Vec<(String, Vec<(i64, f64)>)>,
    credits: Vec<(i64, f64)>,
}

fn work_out(range: Range, acct: &Option<String>, current: &str) -> Data {
    let now = api::now_ms();
    let from = range.from(now);
    let span = now - from;
    usage::read(|recs| {
        let mut accounts = usage::accounts(recs);
        for a in api::accounts() {
            if !a.email.is_empty() && !a.api_key && !accounts.iter().any(|x| x.0 == a.email) {
                accounts.push((a.email.clone(), a.plan.clone()));
            }
        }
        let focus = acct.clone().or_else(|| (!current.is_empty()).then(|| current.to_string())).or_else(|| accounts.first().map(|a| a.0.clone()));
        let (s5, s7) = (usage::samples(recs, false), usage::samples(recs, true));
        let measured = |s: &[usage::Sample]| accounts.iter().map(|(a, _)| (a.clone(), Measured::of(s, a))).filter(|m| !m.1.raw.is_empty()).collect::<Vec<_>>();
        let picked = |r: &usage::Rec| acct.as_ref().is_none_or(|a| &r.acct == a);
        let claude = |r: &usage::Rec| r.claude() && picked(r);
        let mut models: Vec<(String, bool, usage::Bucket)> = vec![];
        for r in recs.iter().filter(|r| r.ev == "req" && r.t >= from && picked(r)) {
            let or = r.acct == usage::OPENROUTER;
            let at = match models.iter().position(|m| m.0 == r.model && m.1 == or) {
                Some(i) => i,
                None => {
                    models.push((r.model.clone(), or, usage::Bucket::default()));
                    models.len() - 1
                }
            };
            models[at].2.add(r);
        }
        models.sort_by(|a, b| b.2.dollars().total_cmp(&a.2.dollars()));
        let util = |week: bool| {
            accounts.iter().map(|(a, _)| (a.clone(), usage::utilization(recs, a, week, from))).filter(|u| !u.1.is_empty()).collect::<Vec<_>>()
        };
        Data {
            now,
            from,
            focus,
            five: measured(&s5),
            week: measured(&s7),
            claude: usage::buckets(recs, from, range.step(), range.n(), claude),
            claude_total: usage::total(recs, from, now + 1, claude),
            claude_before: usage::total(recs, from - span, from, claude),
            tokens: usage::buckets(recs, from, range.step(), range.n(), picked),
            tokens_total: usage::total(recs, from, now + 1, picked),
            by_model: models,
            openrouter: usage::buckets(recs, from, range.step(), range.n(), |r| r.acct == usage::OPENROUTER),
            openrouter_total: usage::total(recs, from, now + 1, |r| r.acct == usage::OPENROUTER),
            util5: util(false),
            util7: util(true),
            credits: usage::credits(recs, from - 30 * DAY),
            accounts,
        }
    })
}

/// What Home's preview shows: the current account's last measurements.
#[derive(Default)]
struct Preview {
    name: String,
    mean: Vec<f64>,
    last: Option<f64>,
    day_ago: Option<f64>,
    count: usize,
}

fn current_email() -> String {
    api::accounts().into_iter().find(|a| a.current).map(|a| a.email).unwrap_or_default()
}

/// How the page names an account: its name, or the email before the @.
fn display_name(email: &str) -> String {
    if email == usage::OPENROUTER {
        return email.into();
    }
    match api::accounts().into_iter().find(|a| a.email == email) {
        Some(a) if !a.name.is_empty() => a.name,
        _ => email.split('@').next().unwrap_or(email).to_string(),
    }
}

// ---------------------------------------------------------------- numbers

/// 812, 12.9k, 41.2k, 286k, 4.82M.
pub fn compact(v: f64) -> String {
    let a = v.abs();
    let s = if a < 1_000.0 {
        format!("{a:.0}")
    } else if a < 100_000.0 {
        format!("{:.1}k", a / 1_000.0)
    } else if a < 1e6 {
        format!("{:.0}k", a / 1_000.0)
    } else if a < 1e8 {
        format!("{:.2}M", a / 1e6)
    } else if a < 1e9 {
        format!("{:.0}M", a / 1e6)
    } else {
        format!("{:.2}B", a / 1e9)
    };
    // "40.0k" → "40k": trailing zeros go, before the suffix.
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, suffix) = s.split_at(split);
    let num = if num.contains('.') { num.trim_end_matches('0').trim_end_matches('.') } else { num };
    let s = format!("{num}{suffix}");
    if v < 0.0 { format!("−{s}") } else { s }
}

/// "$0", "$0.005", "$0.50", "$12.34", "$184", "$1.2k": dollars, with as
/// many digits as the size needs.
pub fn usd(v: f64) -> String {
    let a = v.abs();
    let s = if a == 0.0 {
        "$0".into()
    } else if a < 0.01 {
        // Sub-cent, as on an axis: at most four places, no trailing zeros.
        let s = format!("${a:.4}");
        s.trim_end_matches('0').to_string()
    } else if a < 100.0 && (a.fract() > 1e-9 || a < 1.0) {
        format!("${a:.2}")
    } else if a < 1000.0 {
        format!("${a:.0}")
    } else {
        format!("${}", compact(a))
    };
    if v < 0.0 { format!("−{s}") } else { s }
}

fn percent(v: f64) -> String {
    format!("{v:.0}%")
}

/// "+12%", "−25%": the change from `then` to `now`.
fn change(now: f64, then: f64) -> Option<String> {
    if then <= 0.0 {
        return None;
    }
    let p = (now / then - 1.0) * 100.0;
    Some(if p >= 0.5 { format!("+{p:.0}%") } else if p <= -0.5 { format!("−{:.0}%", -p) } else { "±0%".into() })
}

/// Clean steps for an axis that has to reach `max`: (step, top), three steps.
fn nice(max: f64) -> (f64, f64) {
    if max <= 0.0 || !max.is_finite() {
        return (1.0, 3.0);
    }
    let raw = max / 3.0;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(10.0 * mag);
    (step, step * 3.0)
}

fn when(ms: i64) -> String {
    format!("{}, {}", month_day(ms), hour_min(ms))
}

// ---------------------------------------------------------------- marks

fn font(size: f32) -> FontId {
    FontId::proportional(size)
}

/// A line through `pts`, with the area under it washed in its colour.
fn area(p: &egui::Painter, pts: &[Pos2], base: f32, color: Color32) {
    if pts.len() < 2 {
        return;
    }
    let wash = color.gamma_multiply(0.10);
    let mut m = egui::Mesh::default();
    for w in pts.windows(2) {
        let i = m.vertices.len() as u32;
        m.colored_vertex(w[0], wash);
        m.colored_vertex(w[1], wash);
        m.colored_vertex(pos2(w[1].x, base), wash);
        m.colored_vertex(pos2(w[0].x, base), wash);
        m.add_triangle(i, i + 1, i + 2);
        m.add_triangle(i, i + 2, i + 3);
    }
    p.add(egui::Shape::mesh(m));
}

/// A dot ringed in the surface colour, so it reads where it crosses a line.
fn end_dot(p: &egui::Painter, t: &Theme, at: Pos2, color: Color32) {
    p.circle_filled(at, 6.0, t.panel);
    p.circle_filled(at, 4.0, color);
}

/// Points as steps: level until the next value (utilization holds between
/// readings).
fn stepped(pts: &[Pos2]) -> Vec<Pos2> {
    let mut out = vec![];
    for (i, q) in pts.iter().enumerate() {
        if i > 0 {
            out.push(pos2(q.x, out.last().map_or(q.y, |l: &Pos2| l.y)));
        }
        out.push(*q);
    }
    out
}

/// A short stroke of a series' colour: the key beside its name.
fn key_line(ui: &mut Ui, color: Color32, square: bool) {
    let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    if square {
        ui.painter().rect_filled(Rect::from_center_size(r.center(), vec2(8.0, 8.0)), 1.0, color);
    } else {
        ui.painter().line_segment([r.left_center(), r.right_center()], Stroke::new(2.0, color));
    }
}

/// A chart's title on the left, and its legend after it.
fn title_row(p: &egui::Painter, t: &Theme, rect: Rect, title: &str, legend: &[(String, Color32, bool)]) -> Rect {
    let r = p.text(pos2(rect.left(), rect.center().y), Align2::LEFT_CENTER, title, font(SMALL), t.ink2);
    let mut x = r.right() + 16.0;
    for (name, color, square) in legend {
        let c = pos2(x + 5.0, rect.center().y);
        if *square {
            p.rect_filled(Rect::from_center_size(c, vec2(8.0, 8.0)), 1.0, *color);
        } else {
            p.line_segment([c - vec2(5.0, 0.0), c + vec2(5.0, 0.0)], Stroke::new(2.0, *color));
        }
        let r = p.text(pos2(x + 16.0, rect.center().y), Align2::LEFT_CENTER, name, font(SMALL), t.ink3);
        x = r.right() + 14.0;
    }
    r
}

// ---------------------------------------------------------------- charts

/// The plot inside `rect`, with room for the y labels on the left, the x
/// labels below and an end label on the right; its y grid drawn.
fn plot_area(p: &egui::Painter, t: &Theme, rect: Rect, top: f64, step: f64, fmt: &dyn Fn(f64) -> String, right: f32) -> Rect {
    let plot = Rect::from_min_max(pos2(rect.left() + 46.0, rect.top() + 6.0), pos2(rect.right() - right, rect.bottom() - 18.0));
    let mut v = 0.0;
    while v <= top + step * 0.01 {
        let y = (plot.bottom() - (v / top) as f32 * plot.height()).round() + 0.5;
        let color = if v == 0.0 { t.line } else { crate::theme::mix(t.panel, t.ink, 0.07) };
        p.hline(plot.x_range(), y, Stroke::new(1.0, color));
        p.text(pos2(plot.left() - 8.0, y), Align2::RIGHT_CENTER, fmt(v), font(11.0), t.ink3);
        v += step;
    }
    plot
}

/// Time labels under a line chart from `x0` to `x1`.
fn time_labels(p: &egui::Painter, t: &Theme, plot: Rect, x0: i64, x1: i64, range: Range) {
    let x = |ms: i64| plot.left() + (ms - x0) as f32 / (x1 - x0).max(1) as f32 * plot.width();
    let (every, label): (i64, fn(i64) -> String) = match range {
        Range::Day => (6 * HOUR, hour_min),
        Range::Week => (DAY, month_day),
        Range::Month => (5 * DAY, month_day),
    };
    let off = local_offset_ms();
    let mut at = (x0 + off).div_euclid(every) * every - off;
    if at < x0 {
        at += every;
    }
    while at <= x1 {
        let px = x(at);
        if px > plot.left() + 12.0 && px < plot.right() - 12.0 {
            p.text(pos2(px, plot.bottom() + 4.0), Align2::CENTER_TOP, label(at), font(11.0), t.ink3);
        }
        at += every;
    }
}

/// One series in a line chart.
struct Line {
    name: String,
    color: Color32,
    /// The line.
    pts: Vec<(i64, f64)>,
    /// Single measurements, as faint dots under it.
    dots: Vec<(i64, f64)>,
    /// In the accent, with its area, dots and end label; else grey, quiet.
    focus: bool,
    /// What the tooltip adds under the value at a point.
    notes: Vec<String>,
}

struct LineSpec<'a> {
    id: &'a str,
    x0: i64,
    x1: i64,
    range: Range,
    /// The axis tops out at this when given (percent: 100).
    top: Option<f64>,
    fmt: &'a dyn Fn(f64) -> String,
    steps: bool,
    empty: &'a str,
}

fn line_chart(ui: &mut Ui, t: &Theme, rect: Rect, lines: &[Line], s: &LineSpec) {
    let p = ui.painter_at(rect.expand(8.0));
    let visible = |pts: &[(i64, f64)]| pts.iter().filter(|q| q.0 >= s.x0 && q.0 <= s.x1).map(|q| q.1).fold(0.0f64, f64::max);
    let max = lines.iter().map(|l| visible(&l.pts).max(visible(&l.dots))).fold(0.0, f64::max);
    let (step, top) = match s.top {
        Some(top) => (top / 4.0, top),
        None => nice(max * 1.08),
    };
    let in_range = |pts: &[(i64, f64)]| pts.iter().any(|q| q.0 <= s.x1) && pts.iter().any(|q| q.0 >= s.x0) || pts.iter().any(|q| q.0 < s.x0) && s.steps;
    let empty = !lines.iter().any(|l| in_range(&l.pts));
    // An empty chart has no scale to show.
    let plot = plot_area(&p, t, rect, top, step, if empty { &|_| String::new() } else { s.fmt }, 44.0);
    time_labels(&p, t, plot, s.x0, s.x1, s.range);
    if empty {
        p.text(plot.center(), Align2::CENTER_CENTER, s.empty, font(12.0), t.ink3);
        return;
    }
    let x = |ms: i64| plot.left() + (ms - s.x0) as f32 / (s.x1 - s.x0).max(1) as f32 * plot.width();
    let y = |v: f64| plot.bottom() - (v / top).clamp(0.0, 1.05) as f32 * plot.height();
    // The point just before the range carries the line in from the edge.
    let shown = |pts: &[(i64, f64)]| -> Vec<Pos2> {
        let first = pts.iter().position(|q| q.0 >= s.x0).unwrap_or(pts.len());
        pts[first.saturating_sub(1)..].iter().filter(|q| q.0 <= s.x1).map(|q| pos2(x(q.0.max(s.x0)), y(q.1))).collect()
    };
    let clip = p.with_clip_rect(Rect::from_min_max(pos2(plot.left(), plot.top() - 6.0), pos2(plot.right() + 6.0, plot.bottom())));
    for l in lines.iter().filter(|l| !l.focus) {
        let mut pts = shown(&l.pts);
        if s.steps {
            pts = stepped(&pts);
        }
        clip.add(egui::Shape::line(pts, Stroke::new(1.5, l.color)));
    }
    let mut end_labels = vec![];
    for l in lines.iter().filter(|l| l.focus) {
        let mut pts = shown(&l.pts);
        if s.steps {
            if let Some(last) = pts.last().copied() {
                pts.push(pos2(x(s.x1), last.y)); // holds until now
            }
            pts = stepped(&pts);
        }
        area(&clip, &pts, plot.bottom(), l.color);
        for d in l.dots.iter().filter(|d| d.0 >= s.x0) {
            clip.circle_filled(pos2(x(d.0), y(d.1)), 1.5, l.color.gamma_multiply(0.35));
        }
        clip.add(egui::Shape::line(pts.clone(), Stroke::new(2.0, l.color)));
        if let (Some(last), Some(v)) = (pts.last(), l.pts.iter().rev().find(|q| q.0 <= s.x1)) {
            end_labels.push((*last, l.color, (s.fmt)(v.1)));
        }
    }
    for (at, color, text) in end_labels {
        end_dot(&p, t, at, color);
        p.text(at + vec2(9.0, 0.0), Align2::LEFT_CENTER, text, font(11.0), t.ink);
    }
    // The crosshair: snaps to the nearest point of the series in focus (or
    // any), and the tooltip lists every series there.
    let resp = ui.interact(plot, Id::unique(("usage-line", s.id)), Sense::hover());
    let Some(at) = resp.hover_pos() else { return };
    let tx = s.x0 + ((at.x - plot.left()) / plot.width() * (s.x1 - s.x0) as f32) as i64;
    let lead = lines.iter().find(|l| l.focus).filter(|l| !l.pts.is_empty()).or(lines.iter().find(|l| !l.pts.is_empty()));
    let Some(lead) = lead else { return };
    let near = |pts: &[(i64, f64)]| pts.iter().enumerate().filter(|q| q.1.0 >= s.x0 - DAY).min_by_key(|q| (q.1.0 - tx).abs()).map(|(i, q)| (i, *q));
    let Some((_, (st, _))) = near(&lead.pts) else { return };
    p.vline(x(st.max(s.x0)), plot.y_range(), Stroke::new(1.0, crate::theme::mix(t.panel, t.ink, 0.35)));
    let mut rows = vec![];
    for l in lines {
        // Each series' value at that time: its latest point up to it.
        let v = l.pts.iter().enumerate().rev().find(|q| q.1.0 <= st);
        if let Some((i, (pt, v))) = v {
            if l.focus {
                end_dot(&p, t, pos2(x((*pt).max(s.x0)), y(*v)), l.color);
            }
            rows.push((l.name.clone(), l.color, *v, l.notes.get(i).cloned()));
        }
    }
    resp.on_hover_ui_at_pointer(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(RichText::new(when(st)).size(SMALL).color(t.ink));
        for (name, color, v, note) in rows {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                key_line(ui, color, false);
                ui.label(RichText::new((s.fmt)(v)).size(SMALL).color(t.ink));
                ui.label(RichText::new(name).size(SMALL).color(t.ink3));
                if let Some(n) = note.filter(|n| !n.is_empty()) {
                    ui.label(RichText::new(n).size(SMALL).color(t.ink3));
                }
            });
        }
    });
}

/// Stacked columns: one per bucket, its `parts` from the baseline up, with
/// a 2px gap between parts; the last column's total on its cap; a tooltip
/// per column.
#[allow(clippy::too_many_arguments)]
fn column_chart(ui: &mut Ui, t: &Theme, rect: Rect, id: &str, buckets: &[(i64, Vec<f64>)], parts: &[(&str, Color32)], fmt: &dyn Fn(f64) -> String, range: Range) {
    let p = ui.painter_at(rect.expand(8.0));
    let max = buckets.iter().map(|b| b.1.iter().sum::<f64>()).fold(0.0, f64::max);
    let (step, top) = nice(max * 1.08);
    let plot = plot_area(&p, t, rect, top, step, if max > 0.0 { fmt } else { &|_| String::new() }, 8.0);
    if max <= 0.0 {
        p.text(plot.center(), Align2::CENTER_CENTER, "Nothing in this range", font(12.0), t.ink3);
    }
    let slot = plot.width() / buckets.len().max(1) as f32;
    let bw = (slot * 0.62).clamp(2.0, 24.0);
    let y = |v: f64| plot.bottom() - (v / top) as f32 * plot.height();
    let every = match range {
        Range::Day => 3,
        Range::Week => 1,
        Range::Month => 5,
    };
    let hover = ui.interact(plot, Id::unique(("usage-columns", id)), Sense::hover()).hover_pos();
    let mut tip = None;
    for (i, (start, seg)) in buckets.iter().enumerate() {
        let cx = plot.left() + slot * (i as f32 + 0.5);
        let col = Rect::from_min_max(pos2(cx - slot / 2.0, plot.top()), pos2(cx + slot / 2.0, plot.bottom()));
        if hover.is_some_and(|h| col.contains(h)) {
            p.rect_filled(col, 0.0, crate::theme::mix(t.panel, t.ink, 0.04));
            tip = Some(i);
        }
        let last_part = seg.iter().rposition(|v| *v > 0.0);
        let mut acc = 0.0;
        for (j, v) in seg.iter().enumerate() {
            if *v <= 0.0 {
                continue;
            }
            let (y0, y1) = (y(acc), y(acc + v));
            let gap = if acc > 0.0 { 2.0 } else { 0.0 };
            let r = Rect::from_min_max(pos2(cx - bw / 2.0, y1), pos2(cx + bw / 2.0, (y0 - gap).max(y1 + 0.5)));
            let round = if Some(j) == last_part { CornerRadius { nw: 3, ne: 3, sw: 0, se: 0 } } else { CornerRadius::ZERO };
            p.rect_filled(r, round, parts[j % parts.len()].1);
            acc += v;
        }
        if i == buckets.len() - 1 && acc > 0.0 {
            p.text(pos2(cx, y(acc) - 4.0), Align2::CENTER_BOTTOM, fmt(acc), font(11.0), t.ink);
        }
        let label_at = if range == Range::Day { i.is_multiple_of(every) } else { (buckets.len() - 1 - i).is_multiple_of(every) };
        if label_at {
            let text = if range == Range::Day { hour_min(*start) } else { month_day(*start) };
            p.text(pos2(cx, plot.bottom() + 4.0), Align2::CENTER_TOP, text, font(11.0), t.ink3);
        }
    }
    let Some(i) = tip else { return };
    let (start, seg) = &buckets[i];
    let total: f64 = seg.iter().sum();
    let head = if range == Range::Day { format!("{}, {}–{}", month_day(*start), hour_min(*start), hour_min(start + HOUR)) } else { month_day(*start) };
    let resp = ui.interact(plot, Id::unique(("usage-columns", id)), Sense::hover());
    resp.on_hover_ui_at_pointer(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(RichText::new(head).size(SMALL).color(t.ink3));
        ui.label(RichText::new(fmt(total)).size(13.0).color(t.ink));
        if seg.len() > 1 {
            for (j, v) in seg.iter().enumerate().rev() {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    key_line(ui, parts[j % parts.len()].1, true);
                    ui.label(RichText::new(fmt(*v)).size(SMALL).color(t.ink));
                    ui.label(RichText::new(parts[j].0).size(SMALL).color(t.ink3));
                });
            }
        }
    });
}

/// A small trend, mapped from `rect`'s left to right across `values`.
enum Trend {
    /// Grey, its last stretch in the accent.
    Plain,
    /// The accent throughout, washed, with an end dot.
    Accent,
}

fn trend_pts(values: &[f64], rect: Rect, lo: f64, span: f64) -> Vec<Pos2> {
    values
        .iter()
        .enumerate()
        .map(|(i, v)| pos2(rect.left() + rect.width() * i as f32 / (values.len() - 1) as f32, rect.bottom() - ((v - lo) / span) as f32 * rect.height()))
        .collect()
}

fn sparkline(p: &egui::Painter, t: &Theme, rect: Rect, values: &[f64], trend: Trend) {
    if values.len() < 2 || values.iter().all(|v| *v == 0.0) {
        return;
    }
    let (lo, hi) = values.iter().fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
    let pts = match trend {
        Trend::Plain => trend_pts(values, rect, lo, (hi - lo).max(hi.abs() * 0.05).max(1e-9)),
        Trend::Accent => trend_pts(values, rect, lo * 0.9, (hi * 1.05 - lo * 0.9).max(1e-9)),
    };
    let n = pts.len();
    match trend {
        Trend::Plain => {
            p.add(egui::Shape::line(pts[..n - 1].to_vec(), Stroke::new(1.5, t.ink4)));
            p.add(egui::Shape::line(pts[n - 2..].to_vec(), Stroke::new(2.0, t.accent)));
            p.circle_filled(pts[n - 1], 2.5, t.accent);
        }
        Trend::Accent => {
            // The preview's chart rect sits 9px above the card's bottom, and
            // the wash dips 1px below it.
            area(p, &pts, rect.bottom() + 8.0, t.accent);
            p.add(egui::Shape::line(pts.to_vec(), Stroke::new(2.0, t.accent)));
            end_dot(p, t, pts[n - 1], t.accent);
        }
    }
}

/// One of the figures across the top: label, value, how it changed (or
/// what else to know), and a sparkline.
fn tile(p: &egui::Painter, t: &Theme, rect: Rect, label: &str, value: &str, note: (&str, &str), trend: &[f64]) {
    let x = rect.left() + 16.0;
    p.text(pos2(x, rect.top() + 10.0), Align2::LEFT_TOP, label, font(SMALL), t.ink3);
    p.text(pos2(x, rect.top() + 25.0), Align2::LEFT_TOP, value, font(22.0), t.ink);
    let r = p.text(pos2(x, rect.top() + 54.0), Align2::LEFT_TOP, note.0, font(SMALL), t.ink2);
    p.text(pos2(r.right() + if note.0.is_empty() { 0.0 } else { 4.0 }, rect.top() + 54.0), Align2::LEFT_TOP, note.1, font(SMALL), t.ink3);
    sparkline(p, t, Rect::from_min_max(pos2(x, rect.bottom() - 24.0), pos2(rect.right() - 16.0, rect.bottom() - 8.0)), trend, Trend::Plain);
}

// ---------------------------------------------------------------- Home

/// The preview's numbers, worked out again when the records changed.
fn work_out_preview(view: &mut View) {
    let current = current_email();
    let version = usage::version();
    if view.preview.as_ref().is_none_or(|p| p.0 != version || p.1 != current) {
        let pv = usage::read(|recs| {
            let samples = usage::samples(recs, false);
            // The current account, or the one measured last.
            let acct = if samples.iter().any(|s| s.acct == current) { current.clone() } else { samples.last().map(|s| s.acct.clone()).unwrap_or(current.clone()) };
            let m = Measured::of(&samples, &acct);
            let now = api::now_ms();
            Preview {
                name: display_name(&acct),
                mean: m.mean.iter().rev().take(24).rev().map(|p| p.1).collect(),
                last: m.last(),
                day_ago: m.mean.iter().rev().find(|p| p.0 <= now - DAY).map(|p| p.1),
                count: m.raw.len(),
            }
        });
        view.preview = Some((version, current, pv));
    }
}

/// The preview under Home's accounts: what 1% of the current account's
/// 5-hour limit buys, its trend, and a button to open the Usage page.
/// True when clicked.
pub fn preview(ui: &mut Ui, t: &Theme, rect: Rect, view: &mut View) -> bool {
    work_out_preview(view);
    let pv = &view.preview.as_ref().unwrap().2;    let resp = ui.interact(rect, Id::unique("usage-preview"), Sense::click());
    let lit = resp.hovered();
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 0.0, t.panel);
    p.rect_stroke(rect, 0.0, Stroke::new(1.0, if lit { t.ink3 } else { t.line }), egui::StrokeKind::Inside);
    let x = rect.left() + 10.0;
    p.text(pos2(x, rect.top() + 8.0), Align2::LEFT_TOP, "API $ per 1% of 5h", font(SMALL), t.ink3);
    // The expand mark: two corners pointing out.
    let e = Rect::from_center_size(pos2(rect.right() - 15.0, rect.top() + 15.0), vec2(10.0, 10.0));
    let c = if lit { t.ink } else { t.ink3 };
    let s = Stroke::new(1.3, c);
    p.line_segment([e.right_top(), e.right_top() + vec2(-4.0, 0.0)], s);
    p.line_segment([e.right_top(), e.right_top() + vec2(0.0, 4.0)], s);
    p.line_segment([e.right_top(), e.center() + vec2(1.0, -1.0)], s);
    p.line_segment([e.left_bottom(), e.left_bottom() + vec2(4.0, 0.0)], s);
    p.line_segment([e.left_bottom(), e.left_bottom() + vec2(0.0, -4.0)], s);
    p.line_segment([e.left_bottom(), e.center() + vec2(-1.0, 1.0)], s);
    match pv.last {
        Some(v) => {
            let r = p.text(pos2(x, rect.top() + 23.0), Align2::LEFT_TOP, usd(v), font(20.0), t.ink);
            let note = match pv.day_ago.and_then(|d| change(v, d)) {
                Some(c) => format!("{c} vs a day ago"),
                None => format!("{} measured", pv.count),
            };
            p.text(pos2(r.right() + 7.0, r.bottom() - 3.0), Align2::LEFT_BOTTOM, note, font(SMALL), t.ink3);
            sparkline(&p, t, Rect::from_min_max(pos2(x, rect.top() + 54.0), pos2(rect.right() - 12.0, rect.bottom() - 9.0)), &pv.mean, Trend::Accent);
        }
        None => {
            p.text(pos2(x, rect.top() + 23.0), Align2::LEFT_TOP, "—", font(20.0), t.ink3);
            let msg = "Measured as replies move the limit\nby whole percents";
            p.text(pos2(x, rect.top() + 56.0), Align2::LEFT_TOP, msg, font(SMALL), t.ink3);
        }
    }
    let tip = match pv.last {
        Some(_) => format!(
            "{}: API-equivalent cost per 1% of the 5-hour limit; last 3 measurements. Open Usage.",
            pv.name
        ),
        None => "API-equivalent cost per 1% of the 5-hour limit. Open Usage.".into(),
    };
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked()
}

// ---------------------------------------------------------------- the page

/// The Usage page, in the Settings page's frame. True when it is closed.
pub fn page(ui: &mut Ui, t: &Theme, view: &mut View) -> bool {
    let (card, head, closed) = crate::widgets::page_card(ui, t, 1000.0, Some(t.panel), "usage-close");
    let p = ui.painter().clone();
    p.text(pos2(head.left() + 16.0, head.center().y), Align2::LEFT_CENTER, "Usage", font(13.0), t.ink);
    let (source, detail) = usage::status();
    let source_rect = p.text(pos2(head.right() - 48.0, head.center().y), Align2::RIGHT_CENTER, source, font(11.5), t.ink3);
    ui.interact(source_rect, Id::unique("usage-source"), Sense::hover()).on_hover_text(detail);

    // The data, worked out again when something changed.
    let current = current_email();
    let key = DataKey { version: usage::version(), range: view.range, acct: view.acct.clone(), minute: api::now_ms() / 60_000, current: current.clone() };
    if view.data.as_ref().is_none_or(|d| d.0 != key) {
        view.data = Some((key.clone(), work_out(view.range, &view.acct, &current)));
    }

    // Filters: one row above everything they scope.
    let row = Rect::from_min_max(pos2(card.left() + 16.0, head.bottom()), pos2(card.right() - 16.0, head.bottom() + 44.0));
    filters(ui, &p, t, view, row);
    p.hline(card.x_range(), row.bottom() - 0.5, Stroke::new(1.0, t.line));

    let Some((_, d)) = &view.data else { return closed };
    let focus_name = d.focus.as_deref().map(display_name).unwrap_or_default();
    // With all accounts picked, the spending adds up over them, but what 1%
    // buys is one account's (limits differ by plan): say whose.
    if view.acct.is_none()
        && let Some(f) = &d.focus
    {
        let plan = d.accounts.iter().find(|a| &a.0 == f).map(|a| a.1.as_str()).unwrap_or("");
        let who = if plan.is_empty() { focus_name.clone() } else { format!("{focus_name}, {plan}") };
        let current = if *f == current { ", the current account" } else { "" };
        let text = format!("$ per 1%: {who}{current}");
        p.text(pos2(row.right(), row.center().y), Align2::RIGHT_CENTER, text, font(SMALL), t.ink3);
    }

    let tiles = Rect::from_min_max(pos2(card.left(), row.bottom()), pos2(card.right(), row.bottom() + 96.0));
    figures(ui, &p, t, tiles, d, view.range, &focus_name);
    p.hline(card.x_range(), tiles.bottom() - 0.5, Stroke::new(1.0, t.line));

    // The chart picker.
    let tabs = Rect::from_min_max(pos2(card.left(), tiles.bottom()), pos2(card.right(), tiles.bottom() + 34.0));
    let mut tx = tabs.left();
    for tab in Tab::ALL {
        let w = crate::widgets::text_width(ui, tab.label(), &font(12.5)) + 32.0;
        let r = Rect::from_min_size(pos2(tx, tabs.top()), vec2(w, tabs.height()));
        let resp = ui.interact(r, Id::unique(("usage-tab", tab.label())), Sense::click());
        let on = view.tab == tab;
        p.text(r.center(), Align2::CENTER_CENTER, tab.label(), font(12.5), if on || resp.hovered() { t.ink } else { t.ink3 });
        if on {
            p.hline(r.x_range(), r.bottom() - 1.5, Stroke::new(2.0, t.accent));
        }
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            view.tab = tab;
        }
        tx = r.right();
    }
    p.hline(card.x_range(), tabs.bottom() - 0.5, Stroke::new(1.0, t.line));

    let area = Rect::from_min_max(pos2(card.left() + 16.0, tabs.bottom() + 12.0), pos2(card.right() - 16.0, card.bottom() - 12.0));
    charts(ui, t, area, view.tab, view.range, d);
    closed
}

/// The filters in `row`: the range's buttons and the account picker.
fn filters(ui: &mut Ui, p: &egui::Painter, t: &Theme, view: &mut View, row: Rect) {
    let mut bx = row.left();
    for r in Range::ALL {
        let b = Rect::from_min_size(pos2(bx, row.center().y - 12.0), vec2(42.0, 24.0));
        let resp = ui.interact(b, Id::unique(("usage-range", r.label())), Sense::click());
        let on = view.range == r;
        let pp = ui.painter();
        pp.rect_filled(b, 0.0, if on { t.panel2 } else { t.panel });
        pp.rect_stroke(b, 0.0, Stroke::new(1.0, if on || resp.hovered() { t.ink3 } else { t.line }), egui::StrokeKind::Inside);
        pp.text(b.center(), Align2::CENTER_CENTER, r.label(), font(12.0), if on { t.ink } else { t.ink3 });
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            view.range = r;
        }
        bx = b.right() - 1.0;
    }
    let accounts = view.data.as_ref().map(|d| d.1.accounts.clone()).unwrap_or_default();
    let label_at = pos2(bx + 20.0, row.center().y);
    let lr = p.text(label_at, Align2::LEFT_CENTER, "Account", font(12.0), t.ink3);
    let combo = Rect::from_min_size(pos2(lr.right() + 10.0, row.center().y - 12.0), vec2(260.0, 24.0));
    ui.scope_builder(UiBuilder::new().max_rect(combo), |ui| {
        let name = |a: &(String, String)| if a.1.is_empty() { display_name(&a.0) } else { format!("{}, {}", display_name(&a.0), a.1) };
        let selected = match &view.acct {
            None => "All accounts".to_string(),
            Some(e) => accounts.iter().find(|a| &a.0 == e).map(name).unwrap_or(display_name(e)),
        };
        egui::ComboBox::from_id_salt("usage-account").selected_text(RichText::new(selected).size(12.0)).width(240.0).show_ui(ui, |ui| {
            ui.selectable_value(&mut view.acct, None, "All accounts");
            for a in &accounts {
                ui.selectable_value(&mut view.acct, Some(a.0.clone()), name(a));
            }
        });
    });
}

/// The figures, five tiles across `tiles`: $ per 1% of 5h and of the week
/// (for `focus_name`), Claude at API prices, tokens and OpenRouter.
fn figures(ui: &Ui, p: &egui::Painter, t: &Theme, tiles: Rect, d: &Data, range: Range, focus_name: &str) {
    let tw = tiles.width() / 5.0;
    let cell = |i: usize| Rect::from_min_size(pos2(tiles.left() + tw * i as f32, tiles.top()), vec2(tw, tiles.height()));
    for i in 1..5 {
        p.vline(tiles.left() + tw * i as f32, tiles.y_range(), Stroke::new(1.0, t.line));
    }
    let measured = |m: Option<&Measured>, ago: i64, what: &str| -> (String, String, String, Vec<f64>) {
        match m.and_then(|m| Some((m, m.last()?))) {
            Some((m, v)) => {
                let (c, note) = match m.at(d.now - ago).and_then(|then| change(v, then)) {
                    Some(c) => (c, format!("vs {what} ago")),
                    None => (String::new(), format!("{} measured", m.raw.len())),
                };
                (usd(v), c, note, m.mean.iter().rev().take(12).rev().map(|p| p.1).collect())
            }
            None => ("—".into(), String::new(), "not measured yet".into(), vec![]),
        }
    };
    let five = d.focus.as_ref().and_then(|f| d.five.iter().find(|m| &m.0 == f)).map(|m| &m.1);
    let week = d.focus.as_ref().and_then(|f| d.week.iter().find(|m| &m.0 == f)).map(|m| &m.1);
    let (v, c, n, s) = measured(five, DAY, "a day");
    tile(p, t, cell(0), "$ per 1% of 5h", &v, (&c, &n), &s);
    let (v, c, n, s) = measured(week, 7 * DAY, "a week");
    tile(p, t, cell(1), "$ per 1% of week", &v, (&c, &n), &s);
    let spent = d.claude_total.dollars();
    let c = change(spent, d.claude_before.dollars()).unwrap_or_default();
    let n = if c.is_empty() { format!("{} replies", d.claude_total.replies) } else { range.before().to_string() };
    tile(p, t, cell(2), "Claude at API prices", &usd(spent), (&c, &n), &d.claude.iter().map(|b| b.dollars()).collect::<Vec<_>>());
    let tk = d.tokens_total.tokens();
    let share = if tk > 0.0 { format!("{:.0}%", d.tokens_total.read / tk * 100.0) } else { String::new() };
    tile(p, t, cell(3), "Tokens", &compact(tk), (&share, if tk > 0.0 { "cache reads" } else { "none in this range" }), &d.tokens.iter().map(|b| b.tokens()).collect::<Vec<_>>());
    let or = openrouter::account();
    let left = or.as_ref().and_then(|a| a.left).map(openrouter::dollars);
    tile(
        p,
        t,
        cell(4),
        "OpenRouter",
        &usd(d.openrouter_total.dollars()),
        match &left {
            Some(l) => (l.as_str(), "credits left"),
            None if or.is_some() => ("", "credits loading"),
            None => ("", "no key"),
        },
        &d.openrouter.iter().map(|b| b.dollars()).collect::<Vec<_>>(),
    );
    // Hover help on the two measured tiles.
    let how = "API-equivalent cost divided by utilization change, pooled over the last 3 measurements.\n\
               Combines reporting OAK machines. Missing reports and use outside OAK lower the estimate.\n\
               Excludes polling-only samples, resets, and idle gaps over 30 minutes.";
    for i in 0..2 {
        ui.interact(cell(i), Id::unique(("usage-tile", i)), Sense::hover()).on_hover_text(format!("{}\n\n{how}", if focus_name.is_empty() { "No account" } else { focus_name }));
    }
}

/// Lines for each account's series, named with the account's plan (limits
/// differ by plan): the focus in the accent, the rest grey.
/// An account's series: the account, its line's points, its dots and their notes.
type AccountSeries = (String, Vec<(i64, f64)>, Vec<(i64, f64)>, Vec<String>);

fn account_lines(t: &Theme, focus: &Option<String>, plans: &[(String, String)], series: Vec<AccountSeries>) -> Vec<Line> {
    let mut lines: Vec<Line> = series
        .into_iter()
        .map(|(acct, pts, dots, notes)| {
            let on = focus.as_ref() == Some(&acct);
            let name = match plans.iter().find(|p| p.0 == acct) {
                Some((_, plan)) if !plan.is_empty() => format!("{}, {plan}", display_name(&acct)),
                _ => display_name(&acct),
            };
            Line { name, color: if on { t.accent } else { t.ink4 }, pts, dots: if on { dots } else { vec![] }, focus: on, notes }
        })
        .collect();
    // The focus last in the tooltip's order but drawn over the rest.
    lines.sort_by_key(|l| !l.focus);
    lines
}

fn legend_for(t: &Theme, lines: &[Line]) -> Vec<(String, Color32, bool)> {
    let mut out = vec![];
    if let Some(f) = lines.iter().find(|l| l.focus) {
        out.push((f.name.clone(), t.accent, false));
    }
    if lines.iter().any(|l| !l.focus) {
        out.push((if lines.len() > 2 || out.is_empty() { "other accounts".into() } else { lines.iter().find(|l| !l.focus).unwrap().name.clone() }, t.ink4, false));
    }
    if out.len() < 2 { vec![] } else { out }
}

/// What 1% bought in each `bin` ms from `from`, at the bin's middle: the
/// dollars of its measurements over the percents they covered, pooled (so
/// the ticks' timing errors cancel); and how many went into it.
fn binned(pts: &[(i64, f64)], steps: &[i32], models: &[String], from: i64, bin: i64) -> (Vec<(i64, f64)>, Vec<String>) {
    // Each bin: its start, the dollars, how many measurements, their models, the percents.
    type Bin<'a> = (i64, f64, usize, std::collections::HashMap<&'a str, usize>, f64);
    let mut bins: Vec<Bin> = vec![];
    for (i, (t, v)) in pts.iter().enumerate().filter(|q| q.1.0 >= from - bin) {
        let start = from + (t - from).div_euclid(bin) * bin;
        if bins.last().is_none_or(|b| b.0 != start) {
            bins.push((start, 0.0, 0, Default::default(), 0.0));
        }
        let b = bins.last_mut().unwrap();
        let step = steps.get(i).copied().unwrap_or(1).max(1) as f64;
        b.1 += v * step;
        b.4 += step;
        b.2 += 1;
        *b.3.entry(models.get(i).map_or("", |m| m.as_str())).or_default() += 1;
    }
    let notes = bins
        .iter()
        .map(|b| {
            let model = b.3.iter().max_by_key(|m| m.1).map(|m| model_short(m.0)).unwrap_or_default();
            format!("{} measured{}", b.2, if model.is_empty() { String::new() } else { format!(", mostly {model}") })
        })
        .collect();
    (bins.iter().map(|b| (b.0 + bin / 2, b.1 / b.4)).collect(), notes)
}

fn charts(ui: &mut Ui, t: &Theme, area: Rect, tab: Tab, range: Range, d: &Data) {
    let p = ui.painter().clone();
    let (x0, x1) = (d.from, d.now);
    let half = |i: usize| {
        let h = (area.height() - 8.0) / 2.0;
        let top = area.top() + i as f32 * (h + 8.0);
        (Rect::from_min_max(pos2(area.left(), top), pos2(area.right(), top + 18.0)), Rect::from_min_max(pos2(area.left(), top + 20.0), pos2(area.right(), top + h)))
    };
    let whole = (Rect::from_min_max(area.min, pos2(area.right(), area.top() + 18.0)), Rect::from_min_max(pos2(area.left(), area.top() + 20.0), area.max));
    let k: &dyn Fn(f64) -> String = &compact;
    match tab {
        Tab::Units => {
            // The line: the mean per bin; the dots: every measurement.
            let (bin, per) = match range {
                Range::Day => (HOUR, "hour"),
                Range::Week => (4 * HOUR, "4 hours"),
                Range::Month => (12 * HOUR, "12 hours"),
            };
            for (i, (title, set, empty)) in [
                ("Per 1% of the 5-hour limit", &d.five, "No 5-hour ticks measured in this range"),
                ("Per 1% of the weekly limit", &d.week, "No weekly ticks measured in this range: they come seldom"),
            ]
            .into_iter()
            .enumerate()
            {
                let (head, plot) = half(i);
                let lines = account_lines(
                    t,
                    &d.focus,
                    &d.accounts,
                    set.iter()
                        .map(|(a, m)| {
                            let (pts, notes) = binned(&m.raw, &m.steps, &m.models, x0, bin);
                            (a.clone(), pts, m.raw.clone(), notes)
                        })
                        .collect(),
                );
                title_row(&p, t, head, title, &legend_for(t, &lines));
                if i == 0 {
                    let note = format!("dots: single measurements, line: pooled per {per}");
                    p.text(pos2(head.right(), head.center().y), Align2::RIGHT_CENTER, note, font(SMALL), t.ink3);
                }
                line_chart(ui, t, plot, &lines, &LineSpec { id: title, x0, x1, range, top: None, fmt: &usd, steps: false, empty });
            }
        }
        Tab::Cost => {
            title_row(&p, t, whole.0, "Claude replies at API prices, by what they paid for", &legend(&categories()));
            let buckets: Vec<(i64, Vec<f64>)> = d.claude.iter().map(|b| (b.start, b.usd.to_vec())).collect();
            column_chart(ui, t, whole.1, "cost", &buckets, &categories(), &usd, range);
        }
        Tab::Tokens => {
            let table_h = (22.0 + 20.0 * d.by_model.len().min(6) as f32).min(area.height() * 0.4);
            let chart = Rect::from_min_max(whole.1.min, pos2(area.right(), area.bottom() - table_h - 12.0));
            title_row(&p, t, whole.0, "Tokens, by kind", &legend(&categories()));
            let buckets: Vec<(i64, Vec<f64>)> = d.tokens.iter().map(|b| (b.start, vec![b.read, b.w5m + b.w1h, b.input, b.out])).collect();
            column_chart(ui, t, chart, "tokens", &buckets, &categories(), k, range);
            model_table(&p, t, Rect::from_min_max(pos2(area.left(), chart.bottom() + 12.0), area.max), &d.by_model);
        }
        Tab::Limits => {
            for (i, (title, set)) in [("5-hour limit used", &d.util5), ("Weekly limit used", &d.util7)].into_iter().enumerate() {
                let (head, plot) = half(i);
                let lines = account_lines(t, &d.focus, &d.accounts, set.iter().map(|(a, pts)| (a.clone(), pts.clone(), vec![], vec![])).collect());
                title_row(&p, t, head, title, &legend_for(t, &lines));
                line_chart(ui, t, plot, &lines, &LineSpec { id: title, x0, x1, range, top: Some(100.0), fmt: &percent, steps: true, empty: "No readings in this range" });
            }
        }
        Tab::OpenRouter => {
            let (head, plot) = half(0);
            title_row(&p, t, head, if range == Range::Day { "Spent per hour" } else { "Spent per day" }, &[]);
            let buckets: Vec<(i64, Vec<f64>)> = d.openrouter.iter().map(|b| (b.start, vec![b.dollars()])).collect();
            column_chart(ui, t, plot, "openrouter", &buckets, &[("spent", t.accent)], &usd, range);
            let (head, plot) = half(1);
            title_row(&p, t, head, "Credits left", &[]);
            let line = Line { name: "credits left".into(), color: t.accent, pts: d.credits.clone(), dots: vec![], focus: true, notes: vec![] };
            line_chart(ui, t, plot, &[line], &LineSpec { id: "credits", x0, x1, range, top: None, fmt: &usd, steps: true, empty: "No credit readings yet" });
        }
    }
}

/// "Opus 5.5" for "claude-opus-5-5"; other ids as they are, less the lab.
fn model_short(id: &str) -> String {
    if let Some(rest) = id.strip_prefix("claude-") {
        let mut parts = rest.split('-');
        let family = parts.next().unwrap_or("");
        let version: Vec<&str> = parts.take_while(|p| p.len() <= 2).collect();
        let mut f = family.chars();
        let family = f.next().map(|c| c.to_uppercase().collect::<String>() + f.as_str()).unwrap_or_default();
        return if version.is_empty() { family } else { format!("{family} {}", version.join(".")) };
    }
    id.rsplit('/').next().unwrap_or(id).to_string()
}

/// Tokens and cost per model in the range.
fn model_table(p: &egui::Painter, t: &Theme, rect: Rect, rows: &[(String, bool, usage::Bucket)]) {
    let cols = [rect.left(), rect.left() + rect.width() * 0.42, rect.left() + rect.width() * 0.58, rect.left() + rect.width() * 0.79, rect.right()];
    let head_y = rect.top() + 8.0;
    p.text(pos2(cols[0], head_y), Align2::LEFT_CENTER, "Model", font(SMALL), t.ink3);
    for (i, h) in ["Replies", "Tokens", "Cost"].iter().enumerate() {
        p.text(pos2(cols[i + 2], head_y), Align2::RIGHT_CENTER, *h, font(SMALL), t.ink3);
    }
    p.hline(rect.x_range(), rect.top() + 18.5, Stroke::new(1.0, t.line));
    if rows.is_empty() {
        p.text(pos2(cols[0], rect.top() + 32.0), Align2::LEFT_CENTER, "No replies in this range", font(12.0), t.ink3);
    }
    for (i, (model, or, b)) in rows.iter().take(6).enumerate() {
        let y = rect.top() + 32.0 + i as f32 * 20.0;
        if y > rect.bottom() {
            break;
        }
        let name = if *or { format!("{} (OpenRouter)", model_short(model)) } else { model_short(model) };
        p.text(pos2(cols[0], y), Align2::LEFT_CENTER, name, font(12.0), t.ink);
        p.text(pos2(cols[2], y), Align2::RIGHT_CENTER, b.replies.to_string(), font(12.0), t.ink2);
        p.text(pos2(cols[3], y), Align2::RIGHT_CENTER, compact(b.tokens()), font(12.0), t.ink2);
        p.text(pos2(cols[4], y), Align2::RIGHT_CENTER, usd(b.dollars()), font(12.0), t.ink2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(compact(812.0), "812");
        assert_eq!(compact(41_230.0), "41.2k");
        assert_eq!(compact(40_000.0), "40k");
        assert_eq!(compact(286_400.0), "286k");
        assert_eq!(compact(4_820_000.0), "4.82M");
        assert_eq!(nice(95.0), (50.0, 150.0));
        assert_eq!(nice(7.4e6), (2.5e6, 7.5e6));
        assert_eq!(change(75.0, 100.0).as_deref(), Some("−25%"));
        assert_eq!([usd(0.0), usd(0.5), usd(0.005), usd(12.345), usd(184.2), usd(1240.0)], ["$0", "$0.50", "$0.005", "$12.35", "$184", "$1.2k"]);
        assert_eq!(model_short("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_short("claude-haiku-5-5-20261001"), "Haiku 5.5");
        assert_eq!(model_short("deepseek/deepseek-v4.1-flash-1017"), "deepseek-v4.1-flash-1017");
        // Buckets end with the current hour or day.
        let now = 1_793_300_000_000;
        for r in Range::ALL {
            let from = r.from(now);
            assert!(from + r.n() as i64 * r.step() > now && from + (r.n() as i64 - 1) * r.step() <= now, "{r:?}");
        }
    }
}
