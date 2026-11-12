//! The transcript as a list, the way Zed's list (gpui's `ListState`) works:
//! only the units on screen are laid out each frame, every unit's height is
//! kept once known (an estimate stands in until then), and the scroll
//! position is a unit and an offset into it, so heights changing elsewhere
//! never move what is on screen. Units are measured in the background a few
//! at a time, nearest to the view first, so the scroll bar and the minimap
//! settle on the true size of the conversation within a few frames.
//!
//! Following the end (Zed's FollowMode::Tail): the view stays at the bottom
//! as the transcript grows; scrolling up stops it, coming back down to the
//! end starts it again.
//!
//! A unit is drawn where the known heights say it goes. When its height
//! turns out different, the shapes it painted are moved into place in the
//! same frame (nothing jumps), and another frame is asked for so its
//! widgets answer the pointer where they now are.

use crate::Theme;
use eframe::egui::{self, Align, Color32, Key, Layout, Modifiers, Pos2, Rangef, Rect, Sense, Shape, Stroke, Ui, UiBuilder, Vec2, layers::ShapeIdx, pos2, vec2};
use std::time::{Duration, Instant};

/// A piece of a unit's minimap picture (a run of text, a filled box or a
/// dot), relative to the unit's top left, unscaled.
#[derive(Clone, Copy, Debug)]
pub struct Ink {
    pub rect: Rect,
    pub color: Color32,
}

#[derive(Clone, Default)]
struct Unit {
    /// Its height as last laid out; None until it is, or after it changed.
    h: Option<f32>,
    /// Its height before it changed: the best guess until laid out again.
    hint: Option<f32>,
    sig: u64,
    sketch: Vec<Ink>,
    /// The height the sketch was drawn at, whole, from the unit as it is
    /// now; None while it is from before a change (or there is none yet).
    /// Units are sketched off screen only (never under the pointer, so a
    /// row's hover doesn't light it up, and whole, so a unit taller than
    /// the view isn't sketched only where it was seen).
    sketch_h: Option<f32>,
}

impl Unit {
    /// Its sketch is missing, from before a change, or of another height
    /// (a row in it opened or closed).
    fn needs_sketch(&self) -> bool {
        match (self.sketch_h, self.h) {
            (Some(s), Some(h)) => (s - h).abs() > 0.5,
            (Some(_), None) => true,
            (None, _) => true,
        }
    }
}

/// Dragging the minimap's slider, as VS Code's minimap does it: the view
/// goes where it was when grabbed, plus how far the pointer has moved
/// over the slider's speed then (so measuring units meanwhile doesn't make
/// it jitter under the pointer).
#[derive(Clone, Copy)]
struct MapGrab {
    /// Where the slider was grabbed.
    at: Pos2,
    /// The view's top then.
    pos: f32,
    /// The slider's points per scrolled point then.
    k: f32,
}

/// What the minimap shows this frame: its scale (the column's width to
/// its width), the device pixels per point, where its picture starts (on
/// a pixel), how many whole pixels the picture is scrolled, the slider
/// (on pixels) and its speed (slider points per scrolled point).
struct MapGeo {
    s: f32,
    ppp: f32,
    top: f32,
    left: f32,
    scroll_px: f32,
    slider: Rect,
    k: f32,
}

/// Dragged further than this from the minimap sideways, the view goes
/// back to where it was (as Windows' scroll bars do; VS Code's
/// POINTER_DRAG_RESET_DISTANCE).
const MAP_DRAG_RESET: f32 = 140.0;

/// The scroll bar: thin, and wider under the pointer.
const BAR_W: f32 = 4.0;
pub const BAR_W_HOVER: f32 = 9.0;
/// The minimap's width.
pub const MINIMAP_W: f32 = 96.0;
/// Paging (PageUp, PageDown, a click on the scroll bar's track) moves this
/// much of the view, as Chrome does.
const PAGE: f32 = 0.875;
/// Off-screen layout gets this long a frame.
const MEASURE_BUDGET: Duration = Duration::from_millis(4);
/// Where units are laid out to be measured: far right of anything.
const OFFSCREEN_X: f32 = 100_000.0;

pub struct List {
    units: Vec<Unit>,
    /// The unit at the view's top, and how far down into it the view starts.
    anchor: (usize, f32),
    /// Following the end.
    pub follow: bool,
    width: f32,
    /// The average height of the units measured: unmeasured ones' estimate.
    est: f32,
    /// Scroll still to come (paging and minimap clicks glide there).
    glide: f32,
    /// Its own, for middle-click scrolling (`autoscroll`).
    key: u64,
    /// Dragging the scroll bar: where it was grabbed (below the handle's
    /// top), and the total height then (held while the drag lasts, so the
    /// handle stays under the pointer as units get measured).
    bar_grab: Option<(f32, f32)>,
    /// Dragging the minimap's slider.
    map_grab: Option<MapGrab>,
    /// What the last frame showed: the view's height.
    view_h: f32,
    /// How many units the last frame laid out on screen.
    pub laid: usize,
    /// Frames left in which a click holds the view (what it opens may
    /// only show a frame later).
    hold: u8,
    /// The units the last frame laid out on screen (first and last).
    shown: (usize, usize),
    /// Each unit's top, then the total height, rather than summing the
    /// heights above each time (the scroll bar and the minimap ask several
    /// times a frame; a 3000-turn transcript has 9000 units). Only those
    /// above any height that changed are kept; the rest are worked out
    /// again when next needed, adding the heights in the same order.
    tops: std::cell::RefCell<Vec<f32>>,
    /// A unit's measured height changed since `est` was worked out.
    est_stale: bool,
}

impl Default for List {
    fn default() -> List {
        List {
            units: vec![],
            anchor: (0, 0.0),
            follow: true,
            width: 0.0,
            est: 60.0,
            glide: 0.0,
            key: {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            },
            bar_grab: None,
            map_grab: None,
            view_h: 600.0,
            laid: 0,
            hold: 0,
            shown: (0, 0),
            tops: Default::default(),
            est_stale: true,
        }
    }
}

/// A unit placed this frame: where its top went, its height, and the
/// shapes it painted (indices into the layer's paint list).
struct Placed {
    i: usize,
    top: f32,
    h: f32,
    shapes: (usize, usize),
}

impl List {
    /// Another tab is shown: what was going on here stops.
    pub fn leave(&mut self) {
        self.bar_grab = None;
        self.map_grab = None;
        self.glide = 0.0;
    }

    /// Brings the units in line with the transcript's: `sigs` holds one
    /// per unit, the tail (status, questions, queue) last. Units whose
    /// signature changed are laid out again; a new width lays out all.
    pub fn sync(&mut self, sigs: &[u64], width: f32) {
        let relayout = |u: &mut Unit| {
            if u.h.is_some() {
                u.hint = u.h;
            }
            u.h = None;
            u.sketch_h = None;
        };
        if (width - self.width).abs() > 0.5 {
            self.width = width;
            self.units.iter_mut().for_each(relayout);
            self.heights_changed(0);
        }
        // The units before the first that changed, or came or went, keep
        // their tops.
        let before = self.units.len();
        let mut first = before.min(sigs.len());
        self.units.truncate(sigs.len());
        self.units.resize_with(sigs.len(), Unit::default);
        for (i, (u, &s)) in self.units.iter_mut().zip(sigs).enumerate() {
            if u.sig != s {
                u.sig = s;
                relayout(u);
                first = first.min(i);
            }
        }
        if first < self.units.len() || before != self.units.len() {
            self.heights_changed(first);
        }
        if std::mem::take(&mut self.est_stale) {
            let (sum, n) = self.units.iter().filter_map(|u| u.h).fold((0.0, 0usize), |(s, n), h| (s + h, n + 1));
            if n > 0 && sum / n as f32 != self.est {
                self.est = sum / n as f32;
                self.heights_changed(0);
            }
        }
        let last = self.units.len().saturating_sub(1);
        if self.anchor.0 > last {
            self.anchor = (last, 0.0);
        }
    }

    pub fn height(&self, i: usize) -> f32 {
        let u = &self.units[i];
        u.h.or(u.hint).unwrap_or(self.est)
    }

    /// Unit `i`'s top, from the top of the content.
    pub fn top_of(&self, i: usize) -> f32 {
        let n = self.units.len();
        let mut tops = self.tops.borrow_mut();
        if tops.len() < n + 1 {
            if tops.is_empty() {
                tops.push(0.0);
            }
            let mut y = tops[tops.len() - 1];
            for k in tops.len() - 1..n {
                y += self.height(k);
                tops.push(y);
            }
        }
        tops[i.min(n)]
    }

    /// The height of unit `i` (or of those after it, or the estimate)
    /// changed: the tops below it are worked out again, and the estimate
    /// too (`sync`).
    fn heights_changed(&mut self, i: usize) {
        self.tops.get_mut().truncate(i + 1);
        self.est_stale = true;
    }

    pub fn total(&self) -> f32 {
        self.top_of(self.units.len())
    }

    /// The view's top, from the top of the content.
    pub fn pos(&self) -> f32 {
        self.top_of(self.anchor.0) + self.anchor.1
    }

    fn max_pos(&self, vh: f32) -> f32 {
        (self.total() - vh).max(0.0)
    }

    /// Scrolls to `y` (the view's top, from the content's top). The end
    /// starts following it.
    pub fn set_pos(&mut self, y: f32, vh: f32) {
        let max = self.max_pos(vh);
        let y = y.clamp(0.0, max);
        self.follow = y >= max - 0.5;
        let mut acc = 0.0;
        for i in 0..self.units.len() {
            let h = self.height(i);
            if acc + h > y || i + 1 == self.units.len() {
                self.anchor = (i, (y - acc).max(0.0));
                return;
            }
            acc += h;
        }
        self.anchor = (0, 0.0);
    }

    /// Scrolls by `dy` (positive: towards the end).
    pub fn scroll_by(&mut self, dy: f32, vh: f32) {
        if dy == 0.0 || (self.follow && dy > 0.0) {
            return;
        }
        self.set_pos(self.pos() + dy, vh);
    }

    /// Shows unit `i` near the view's top.
    pub fn jump_to(&mut self, i: usize, vh: f32) {
        self.glide = 0.0;
        self.set_pos(self.top_of(i) - 12.0, vh);
    }

    /// The first unit in view.
    pub fn first_shown(&self) -> usize {
        self.anchor.0
    }

    pub fn jump_to_end(&mut self) {
        self.glide = 0.0;
        self.follow = true;
    }

    /// More than a view's height from the end (the jump-to-the-end bar shows).
    pub fn far_from_end(&self) -> bool {
        !self.follow && self.max_pos(self.view_h) - self.pos() > self.view_h
    }

    /// The wheel, PageUp and PageDown, Ctrl+Home and Ctrl+End, middle-click scrolling and paging
    /// that is still gliding: applied before the frame is laid out, so it
    /// shows them. `view` is where the transcript shows (the wheel scrolls
    /// it anywhere in there).
    pub fn input(&mut self, ui: &mut Ui, t: &Theme, view: Rect) {
        let vh = view.height();
        self.view_h = vh;
        let ctx = ui.ctx().clone();
        // A click in the transcript (opening the summary, a tool row, a
        // notice) keeps what is on screen where it is: following the end
        // would push it up as it grows. The view is held at what the last
        // frame showed, so what opens grows downwards; if nothing grew,
        // the end is followed again (`show`).
        if ui.rect_contains_pointer(view) && ctx.input(|i| i.pointer.primary_pressed() || i.pointer.primary_released()) {
            self.follow = false;
            self.hold = 3;
            ctx.request_repaint();
        }
        // The wheel, as egui smooths it (the speed is the system's).
        if ui.rect_contains_pointer(view) {
            let (dy, raw) = ctx.input_mut(|i| (std::mem::take(&mut i.smooth_scroll_delta.y), i.raw.events.iter().any(|e| matches!(e, egui::Event::MouseWheel { .. }))));
            if raw {
                // A new turn of the wheel ends a glide (its smoothed tail doesn't).
                self.glide = 0.0;
            }
            if dy != 0.0 {
                self.scroll_by(-dy, vh);
            }
        }
        // PageUp and PageDown (no widget uses them): glide a page.
        let (up, down) = ctx.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::PageUp), i.consume_key(Modifiers::NONE, Key::PageDown)));
        if up || down {
            if self.follow && down {
                self.glide = 0.0;
            } else {
                // From where the last glide was going.
                let base = if self.follow { 0.0 } else { self.glide };
                self.glide = base + if up { -PAGE * vh } else { PAGE * vh };
            }
        }
        // Ctrl+Home and Ctrl+End: the start and the end of the transcript,
        // unless a text field has focus (there they move its caret).
        if !ctx.egui_wants_keyboard_input() {
            let (home, end) = ctx.input_mut(|i| (i.consume_key(Modifiers::COMMAND, Key::Home), i.consume_key(Modifiers::COMMAND, Key::End)));
            if home {
                self.follow = false;
                self.glide = -self.shown_pos();
            } else if end {
                self.jump_to_end();
            }
        }
        if self.glide != 0.0 {
            let dt = ctx.input(|i| i.stable_dt).min(0.1);
            // Most of the way in a tenth of a second, like a browser's.
            let step = if self.glide.abs() < 0.5 { self.glide } else { self.glide * (1.0 - (-dt / 0.035).exp()) };
            self.glide -= step;
            let before = self.pos();
            self.scroll_by(step, vh);
            if (self.pos() - before).abs() < 0.01 && step.abs() > 0.01 {
                self.glide = 0.0; // at an end
            }
            ctx.request_repaint();
        }
        self.autoscroll(ui, t, view);
    }

    /// Middle-click scrolling (`autoscroll`).
    fn autoscroll(&mut self, ui: &mut Ui, t: &Theme, view: Rect) {
        let scrollable = self.total() > view.height();
        let Some(dy) = crate::autoscroll::scroll(ui, t, self.key, view, scrollable) else { return };
        self.glide = 0.0;
        if dy != 0.0 {
            self.scroll_by(dy, view.height());
        }
    }

    /// Lays out what shows in `view`: the units in column `col`, each
    /// drawn by `render` (unit 0 first; the last is the tail). Then
    /// measures a few more off screen.
    pub fn show(&mut self, ui: &mut Ui, view: Rect, col: Rangef, render: &mut dyn FnMut(&mut Ui, usize)) {
        let n = self.units.len();
        if n == 0 {
            return;
        }
        let ctx = ui.ctx().clone();
        let off = Rangef::new(col.min + OFFSCREEN_X, col.max + OFFSCREEN_X);
        let warn = ctx.options(|o| o.warn_on_id_clash);
        let twice = self.sketch_shown(ui, view, off, render);
        if twice {
            // Laid out twice this frame, on purpose.
            ctx.options_mut(|o| o.warn_on_id_clash = false);
        }
        let (placed, moved) = self.place(ui, view, col, render);
        self.laid = placed.len();
        if self.hold > 0 {
            self.hold -= 1;
            ctx.request_repaint();
        }
        if let Some(p) = placed.iter().find(|p| p.top + p.h > view.top()).or(placed.last()) {
            self.anchor = (p.i, (view.top() - p.top).max(0.0));
        }
        if moved {
            // Widgets that moved answer the pointer where they are next frame.
            ctx.request_repaint();
        }
        if self.keep_sketches(ui, view, col, &placed) {
            ctx.request_repaint();
        }
        let (lo, hi) = (placed.first().map_or(0, |p| p.i), placed.last().map_or(0, |p| p.i));
        self.shown = (lo, hi);
        if self.measure_more(ui, view, off, render) {
            ctx.request_repaint();
        }
        if twice {
            ctx.options_mut(|o| o.warn_on_id_clash = warn);
        }
    }

    /// Sketches of the units on screen (as the last frame had them), drawn
    /// off screen (in columns `off`) before they are laid out there. Their
    /// widgets are disabled there, so only the ones on screen answer the
    /// pointer (and theirs, laid out last, are the rects egui keeps). True
    /// when any was.
    fn sketch_shown(&mut self, ui: &mut Ui, view: Rect, off: Rangef, render: &mut dyn FnMut(&mut Ui, usize)) -> bool {
        let (lo, hi) = self.shown;
        let mut twice = false;
        for i in lo..=hi.min(self.units.len().saturating_sub(2)) {
            if self.units[i].needs_sketch() {
                self.sketch_off_screen(ui, i, view.top(), off, render, true);
                twice = true;
            }
        }
        twice
    }

    /// Lays out the units that show in `view`: up from the end when
    /// following it, else down from the anchor; then the ones above a gap at
    /// the top. Gives them, and whether any were moved after being laid out.
    fn place(&mut self, ui: &mut Ui, view: Rect, col: Rangef, render: &mut dyn FnMut(&mut Ui, usize)) -> (Vec<Placed>, bool) {
        let n = self.units.len();
        let ctx = ui.ctx().clone();
        let layer = ui.layer_id();
        let mut placed: Vec<Placed> = vec![];
        let mut moved = false;
        if self.follow {
            // From the end up, by the heights known, far enough to fill the view.
            let (mut i, mut y) = (n, view.bottom());
            while i > 0 && y > view.top() {
                i -= 1;
                y -= self.height(i);
            }
            // Shorter than the view: lay it out from the top, where it
            // shows. Shifting it there afterwards would move only the
            // paint, leaving the widgets answering the pointer lower down.
            if i == 0 {
                y = y.min(view.top());
            }
            for k in i..n {
                let p = self.lay(ui, k, y, col, view, render);
                y += p.h;
                placed.push(p);
            }
            let mut d = view.bottom() - y;
            if i == 0 {
                d = d.min(view.top() - placed[0].top);
            }
            moved |= shift(&ctx, layer, &mut placed, d);
        } else {
            let (i, off) = self.anchor;
            let mut y = view.top() - off;
            let mut k = i;
            while k < n && y < view.bottom() {
                let p = self.lay(ui, k, y, col, view, render);
                y += p.h;
                placed.push(p);
                k += 1;
            }
            if k == n && y < view.bottom() + 0.5 && (self.hold == 0 || y < view.bottom() - 0.5) {
                // At the end (nothing grew after a click), or past it
                // (something above shrank): the end goes to the view's
                // bottom, and is followed again.
                moved |= shift(&ctx, layer, &mut placed, (view.bottom() - y).max(0.0));
                self.follow = true;
            }
        }
        // A gap above the first unit: lay out the ones above it.
        while let Some(first) = placed.first()
            && first.top > view.top()
            && first.i > 0
        {
            let (i, top) = (first.i - 1, first.top);
            let guess = top - self.height(i);
            let mut p = vec![self.lay(ui, i, guess, col, view, render)];
            let d = top - (guess + p[0].h);
            moved |= shift(&ctx, layer, &mut p, d);
            placed.insert(0, p.pop().unwrap());
        }
        // Shorter than the view: it starts at the top.
        if let Some(first) = placed.first()
            && first.i == 0
            && first.top > view.top() + 0.5
        {
            let d = view.top() - first.top;
            moved |= shift(&ctx, layer, &mut placed, d);
        }
        (placed, moved)
    }

    /// The tail is sketched where it shows (it holds the text boxes that
    /// take focus, so it is never laid out twice); the other units on
    /// screen that changed are sketched off screen next frame, for which
    /// this is true.
    fn keep_sketches(&mut self, ui: &Ui, view: Rect, col: Rangef, placed: &[Placed]) -> bool {
        let n = self.units.len();
        let (ctx, layer) = (ui.ctx(), ui.layer_id());
        let mut again = false;
        for p in placed {
            if p.i + 1 < n {
                again |= self.units[p.i].needs_sketch();
                continue;
            }
            let whole = p.top >= view.top() - 0.5 && p.top + p.h <= view.bottom() + 0.5;
            let keep = Rangef::new(view.top() - p.top, view.bottom() - p.top);
            let ink = capture(ctx, layer, p.shapes, pos2(col.min, p.top), keep);
            let u = &mut self.units[p.i];
            if whole {
                u.sketch = ink;
            } else {
                u.sketch.retain(|r| (r.rect.bottom() < keep.min || r.rect.top() > keep.max) && r.rect.top() < p.h);
                u.sketch.extend(ink);
            }
            u.sketch_h = u.h;
        }
        again
    }

    /// Off screen (in columns `off`), the units around the ones shown,
    /// nearest to the view first, within the frame's budget. True when some
    /// are left for the next frame.
    fn measure_more(&mut self, ui: &mut Ui, view: Rect, off: Rangef, render: &mut dyn FnMut(&mut Ui, usize)) -> bool {
        let n = self.units.len();
        let (lo, hi) = self.shown;
        let start = Instant::now();
        for d in 1..n {
            for i in [hi.checked_add(d), lo.checked_sub(d)].into_iter().flatten() {
                // (Not the tail: it holds the text boxes that take focus.)
                if i + 1 >= n || (self.units[i].h.is_some() && !self.units[i].needs_sketch()) {
                    continue;
                }
                if start.elapsed() > MEASURE_BUDGET {
                    return true;
                }
                self.sketch_off_screen(ui, i, view.top(), off, render, false);
            }
        }
        false
    }

    /// Lays out unit `i` off screen (at `top`, in columns `off`), keeps its
    /// height and its sketch, and takes back what it painted. `disabled`:
    /// it is also on screen this frame, where its widgets answer.
    fn sketch_off_screen(&mut self, ui: &mut Ui, i: usize, top: f32, off: Rangef, render: &mut dyn FnMut(&mut Ui, usize), disabled: bool) {
        let ctx = ui.ctx().clone();
        let layer = ui.layer_id();
        let clip = Rect::from_x_y_ranges(off, top..=top + 1.0e6);
        SKETCHING.set(true);
        let p = if disabled {
            let mut render = |ui: &mut Ui, i: usize| {
                // Disabled, but drawn as it is on screen.
                ui.style_mut().visuals.disabled_alpha = 1.0;
                ui.disable();
                render(ui, i)
            };
            self.lay(ui, i, top, off, clip, &mut render)
        } else {
            self.lay(ui, i, top, off, clip, render)
        };
        SKETCHING.set(false);
        let ink = capture(&ctx, layer, p.shapes, pos2(off.min, top), Rangef::new(f32::NEG_INFINITY, f32::INFINITY));
        ctx.graphics_mut(|g| {
            let l = g.entry(layer);
            for k in p.shapes.0..p.shapes.1 {
                l.reset_shape(ShapeIdx(k));
            }
        });
        let u = &mut self.units[i];
        u.sketch = ink;
        u.sketch_h = Some(p.h);
    }

    /// Lays out unit `i` with its top at `top`, clipped to `clip`, and keeps its height.
    fn lay(&mut self, ui: &mut Ui, i: usize, top: f32, col: Rangef, clip: Rect, render: &mut dyn FnMut(&mut Ui, usize)) -> Placed {
        let layer = ui.layer_id();
        let s = paint_len(ui.ctx(), layer);
        let rect = Rect::from_x_y_ranges(col, top..=top + 1.0e6);
        // An id of its own, not one from the parent's count of children: a
        // unit sketched off screen first (it changed) would otherwise move
        // the ids of every unit laid out after it, and their widgets would
        // lose the pointer (hover, the cursor) every frame something streams.
        let id = ui.unique_id().with(("transcript-unit", i));
        let mut child = ui.new_child(UiBuilder::new().max_rect(rect).layout(Layout::top_down(Align::Min)).scope_id(id));
        child.set_clip_rect(clip.intersect(ui.clip_rect().union(clip)));
        render(&mut child, i);
        let h = (child.min_rect().bottom() - top).max(0.0) + ui.spacing().item_spacing.y;
        let e = paint_len(ui.ctx(), layer);
        if self.units[i].h != Some(h) {
            self.units[i].h = Some(h);
            self.heights_changed(i);
        }
        Placed { i, top, h, shapes: (s, e) }
    }

    /// The scroll bar's dragging and track clicks, before the frame is laid
    /// out. The handle stays under the pointer while dragged.
    pub fn bar_input(&mut self, ui: &mut Ui, track: Rect) {
        let vh = self.view_h;
        let total = self.total();
        if total <= vh + 0.5 {
            self.bar_grab = None;
            return;
        }
        let resp = ui.interact(track, ui.scope_id().with("transcript-bar"), Sense::click_and_drag());
        let handle = self.handle(track);
        if resp.drag_started() {
            // Held where it was grabbed; grabbed beside it, by its middle.
            let at = ui.input(|i| i.pointer.press_origin()).unwrap_or(handle.center());
            let grab = if handle.y_range().contains(at.y) { at.y - handle.top() } else { handle.height() / 2.0 };
            self.bar_grab = Some((grab, total));
            self.glide = 0.0;
        }
        if resp.dragged()
            && let (Some((grab, frozen)), Some(p)) = (self.bar_grab, resp.interact_pointer_pos())
        {
            let travel = (track.height() - handle.height()).max(1.0);
            let frac = ((p.y - grab - track.top()) / travel).clamp(0.0, 1.0);
            self.set_pos(frac * (frozen - vh).max(0.0), vh);
            ui.ctx().request_repaint();
        } else if !resp.dragged() {
            self.bar_grab = None;
            if resp.clicked()
                && let Some(p) = resp.interact_pointer_pos()
                && !handle.contains(p)
            {
                // A click on the track pages towards it.
                self.glide = if p.y < handle.top() { -PAGE * vh } else { PAGE * vh };
            }
        }
    }

    /// The handle's rect in `track`: under the pointer while dragged.
    fn handle(&self, track: Rect) -> Rect {
        let vh = self.view_h;
        let total = self.bar_grab.map_or(self.total(), |g| g.1).max(vh + 1.0);
        let h = (track.height() * vh / total).max(28.0).min(track.height());
        let travel = (track.height() - h).max(1.0);
        let frac = if self.follow { 1.0 } else { (self.pos() / (total - vh)).clamp(0.0, 1.0) };
        let top = track.top() + travel * frac;
        Rect::from_min_max(pos2(track.left(), top), pos2(track.right(), top + h))
    }

    /// Paints the scroll bar (after the frame is laid out).
    pub fn bar_paint(&self, ui: &mut Ui, t: &Theme, track: Rect) {
        if self.total() <= self.view_h + 0.5 {
            return;
        }
        let mut handle = self.handle(track);
        if let (Some((grab, _)), Some(p)) = (self.bar_grab, ui.input(|i| i.pointer.interact_pos())) {
            let top = (p.y - grab).clamp(track.top(), track.bottom() - handle.height());
            handle = handle.translate(vec2(0.0, top - handle.top()));
        }
        #[cfg(test)]
        ui.ctx().data_mut(|d| d.insert_temp(egui::Id::unique("transcript-bar-handle"), handle));
        let lit = ui.rect_contains_pointer(track) || self.bar_grab.is_some();
        let w = if lit { BAR_W_HOVER } else { BAR_W };
        let draw = Rect::from_min_max(pos2(track.right() - w, handle.top()), pos2(track.right(), handle.bottom()));
        if lit {
            ui.painter().rect_filled(Rect::from_x_y_ranges(draw.x_range(), track.y_range()), 0.0, t.panel);
        }
        ui.painter().rect_filled(draw, 0.0, if self.bar_grab.is_some() { t.ink2 } else if lit { t.ink3 } else { t.ink4 });
    }

    /// Where the view's top is, as the minimap shows it.
    fn shown_pos(&self) -> f32 {
        let max = self.max_pos(self.view_h);
        if self.follow { max } else { self.pos().min(max) }
    }

    /// The minimap's layout in `rect`, as VS Code's (`MinimapLayout`):
    /// taller than its room, the picture scrolls along with the view, so
    /// the slider goes from top to bottom as the view does. The picture
    /// only ever moves by whole device pixels (VS Code's by whole lines),
    /// so what it shows moves rigidly instead of shimmering.
    fn map_geometry(&self, rect: Rect, col_w: f32, ppp: f32) -> MapGeo {
        let snap = |v: f32| (v * ppp).round() / ppp;
        let s = rect.width() / col_w.max(1.0);
        let vh = self.view_h;
        let (m, p, max) = (self.total() * s, rect.height(), self.max_pos(vh));
        let pos = self.shown_pos();
        let over = m > p && max > 0.0;
        let scroll_px = if over { (pos / max * (m - p) * ppp).round() } else { 0.0 };
        // VS Code's computedSliderRatio: the slider's travel over the view's.
        let k = if over { ((p - s * vh) / max).max(1e-6) } else { s };
        let (top, left) = (snap(rect.top()), snap(rect.left()));
        let y = snap(top + pos * s - scroll_px / ppp);
        let h = snap(vh * s).max(1.0 / ppp);
        MapGeo { s, ppp, top, left, scroll_px, slider: Rect::from_min_max(pos2(rect.left(), y), pos2(rect.right(), y + h)), k }
    }

    /// The minimap's clicks and dragging, before the frame is laid out, as
    /// VS Code's: a press on the slider drags it (the view moving with
    /// the pointer at the slider's speed), and a press beside it glides
    /// that part of the transcript to the middle of the view.
    pub fn map_input(&mut self, ui: &mut Ui, rect: Rect, col_w: f32) {
        let vh = self.view_h;
        let resp = ui.interact(rect, ui.scope_id().with("transcript-minimap"), Sense::click_and_drag());
        let g = self.map_geometry(rect, col_w, ui.ctx().pixels_per_point());
        if self.map_grab.is_none()
            && resp.is_pointer_button_down_on()
            && ui.input(|i| i.pointer.primary_pressed())
            && let Some(at) = ui.input(|i| i.pointer.press_origin())
        {
            if self.total() > vh + 0.5 && g.slider.y_range().contains(at.y) {
                self.map_grab = Some(MapGrab { at, pos: self.shown_pos(), k: g.k });
                self.glide = 0.0;
            } else {
                // That line to the middle of the view (VS Code reveals it
                // centred, scrolling smoothly).
                let y = (at.y - g.top + g.scroll_px / g.ppp) / g.s;
                let goal = (y - vh / 2.0).clamp(0.0, self.max_pos(vh));
                self.glide = goal - self.shown_pos();
            }
            ui.ctx().request_repaint();
        }
        if let Some(grab) = self.map_grab {
            if resp.dragged() || resp.is_pointer_button_down_on() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let away = (p.x - grab.at.x).abs().min((p.x - rect.left()).abs()).min((p.x - rect.right()).abs());
                    let y = if cfg!(windows) && away > MAP_DRAG_RESET { grab.pos } else { grab.pos + (p.y - grab.at.y) / grab.k };
                    self.set_pos(y, vh);
                    ui.ctx().request_repaint();
                }
            } else {
                self.map_grab = None;
            }
        }
    }

    /// Paints the minimap in `rect`: every unit's picture, scaled down and
    /// drawn on whole device pixels, the slider over what the view shows
    /// (fading in under the pointer or while dragged, as VS Code's), and
    /// with `cut` (a unit), the summarized part marked.
    pub fn map_paint(&self, ui: &mut Ui, t: &Theme, rect: Rect, col_w: f32, cut: Option<usize>) {
        let g = self.map_geometry(rect, col_w, ui.ctx().pixels_per_point());
        #[cfg(test)]
        ui.ctx().data_mut(|d| d.insert_temp(egui::Id::unique("transcript-map-slider"), (g.slider, g.k)));
        let (ppp, sp) = (g.ppp, g.s * g.ppp);
        let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
        p.vline(rect.left() - 0.5, rect.y_range(), Stroke::new(1.0, t.line.gamma_multiply(0.5)));
        // In device pixels: the room, and a point of the picture's content
        // at `y` (from the content's top) on screen.
        let (w_px, h_px) = ((rect.width() * ppp).floor(), (rect.bottom() - g.top) * ppp);
        let at = |px_x: f32, px_y: f32| pos2(g.left + px_x / ppp, g.top + (px_y - g.scroll_px) / ppp);
        let mut mesh = egui::Mesh::default();
        // From the first unit that reaches the picture's top.
        let n = self.units.len();
        let first = {
            self.top_of(0);
            let tops = self.tops.borrow();
            tops[1..].partition_point(|&bottom| bottom * sp - g.scroll_px < 0.0).min(n)
        };
        let mut top = self.top_of(first);
        for i in first..n {
            let h = self.height(i);
            // The unit's top, in the picture's pixels (not yet rounded:
            // each piece is, from the content's top, so a piece always
            // lands on the same pixels of the picture).
            let uy = top * sp;
            let (y0, y1) = (uy - g.scroll_px, (top + h) * sp - g.scroll_px);
            if y1 >= 0.0 && y0 <= h_px {
                for ink in &self.units[i].sketch {
                    // Nothing past the unit's end (a sketch from before it
                    // shrank must not run into the next).
                    if ink.rect.top() >= h {
                        continue;
                    }
                    let r = Rect::from_min_max(ink.rect.min, pos2(ink.rect.right(), ink.rect.bottom().min(h)));
                    let a = (uy + r.top() * sp).round();
                    let b = a + (r.height() * sp).round().max(1.0);
                    if a - g.scroll_px > h_px || b - g.scroll_px < 0.0 {
                        continue;
                    }
                    let x0 = (r.left() * sp).round().max(0.0);
                    let x1 = (r.right() * sp).round().max(x0 + 1.0).min(w_px);
                    if x1 <= x0 {
                        continue;
                    }
                    mesh.add_colored_rect(Rect::from_min_max(at(x0, a), at(x1, b)), ink.color.gamma_multiply(0.85));
                }
            }
            top += h;
            if y0 > h_px {
                break;
            }
        }
        p.add(Shape::mesh(mesh));
        if let Some(cut) = cut.filter(|&c| c < self.units.len()) {
            // The summarized part tinted, the cut in the accent.
            let y = at(0.0, (self.top_of(cut) * sp).round()).y;
            let above = Rect::from_min_max(rect.min, pos2(rect.right(), y.min(rect.bottom())));
            if above.height() > 0.0 {
                p.rect_filled(above, 0.0, t.accent.gamma_multiply(0.10));
            }
            p.rect_filled(Rect::from_min_max(pos2(rect.left(), y - 1.0), pos2(rect.right(), y + 1.0)), 0.0, t.accent);
        }
        // VS Code's slider colors (dark theme), fading in over a tenth of
        // a second on mouseover, and shown while dragged.
        let hover = ui.rect_contains_pointer(rect);
        let shown = ui.ctx().animate_bool_with_time(ui.scope_id().with("transcript-minimap-slider"), hover || self.map_grab.is_some(), 0.1);
        if self.total() > self.view_h + 0.5 && shown > 0.0 {
            let over = ui.input(|i| i.pointer.hover_pos()).is_some_and(|q| g.slider.contains(q));
            let fill = if self.map_grab.is_some() {
                Color32::from_rgba_unmultiplied(191, 191, 191, 51)
            } else if over {
                Color32::from_rgba_unmultiplied(100, 100, 100, 89)
            } else {
                Color32::from_rgba_unmultiplied(121, 121, 121, 51)
            };
            p.rect_filled(g.slider, 0.0, fill.gamma_multiply(shown));
        }
    }
}

thread_local! {
    static SKETCHING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a unit is being laid out off screen for its height and sketch
/// (`List::sketch_off_screen`): what it paints is taken back once its ink
/// is captured, so painting a part's ink (`Ink` rects, as `capture` reads
/// them) stands in for drawing it.
pub(crate) fn sketching() -> bool {
    SKETCHING.get()
}

/// `f` run as if laying out a unit off screen (`sketching`).
#[cfg(test)]
pub(crate) fn as_sketching<R>(f: impl FnOnce() -> R) -> R {
    SKETCHING.set(true);
    let r = f();
    SKETCHING.set(false);
    r
}

/// How many shapes the layer has so far.
pub(crate) fn paint_len(ctx: &egui::Context, layer: egui::LayerId) -> usize {
    ctx.graphics(|g| g.get(layer).map_or(0, |l| l.next_idx().0))
}

/// Moves what the placed units painted down by `d`. True if they moved.
fn shift(ctx: &egui::Context, layer: egui::LayerId, placed: &mut [Placed], d: f32) -> bool {
    if d.abs() < 0.01 {
        return false;
    }
    ctx.graphics_mut(|g| {
        let l = g.entry(layer);
        for p in placed.iter() {
            for k in p.shapes.0..p.shapes.1 {
                l.mutate_shape(ShapeIdx(k), |c| c.shape.translate(vec2(0.0, d)));
            }
        }
    });
    for p in placed.iter_mut() {
        p.top += d;
    }
    d.abs() > 0.5
}

/// The ink of shapes `range` of the layer, relative to `origin`, within
/// `keep` (a y range relative to `origin`).
pub(crate) fn capture(ctx: &egui::Context, layer: egui::LayerId, range: (usize, usize), origin: Pos2, keep: Rangef) -> Vec<Ink> {
    let mut out = vec![];
    ctx.graphics(|g| {
        if let Some(l) = g.get(layer) {
            for c in l.all_entries().skip(range.0).take(range.1.saturating_sub(range.0)) {
                ink(&c.shape, origin.to_vec2(), &mut out);
            }
        }
    });
    out.retain(|r| r.rect.bottom() >= keep.min && r.rect.top() <= keep.max);
    out
}

fn ink(shape: &Shape, o: Vec2, out: &mut Vec<Ink>) {
    match shape {
        Shape::Vec(v) => v.iter().for_each(|s| ink(s, o, out)),
        Shape::Text(t) => text_ink(t, o, out),
        Shape::Rect(r) if r.fill.a() > 0 => out.push(Ink { rect: r.rect.translate(-o), color: r.fill }),
        Shape::Circle(c) => {
            let color = if c.fill.a() > 0 { c.fill } else { c.stroke.color };
            if color.a() > 0 {
                out.push(Ink { rect: Rect::from_center_size(c.center - o, Vec2::splat(c.radius * 2.0)), color });
            }
        }
        Shape::Mesh(m) if m.texture_id != egui::TextureId::default() => {
            out.push(Ink { rect: m.calc_bounds().translate(-o), color: Color32::from_gray(0x70) });
        }
        _ => {}
    }
}

/// Text as runs of ink, a run per word (and color), like a minimap's.
fn text_ink(t: &egui::epaint::TextShape, o: Vec2, out: &mut Vec<Ink>) {
    let base = t.pos.to_vec2() - o;
    for pr in &t.galley.rows {
        let row = &pr.row;
        let (y0, h) = (base.y + pr.pos.y, row.size.y);
        let push = |out: &mut Vec<Ink>, (a, b, c): (f32, f32, Color32)| {
            out.push(Ink { rect: Rect::from_min_max(pos2(a, y0 + h * 0.22), pos2(b, y0 + h * 0.78)), color: c });
        };
        let mut run: Option<(f32, f32, Color32)> = None;
        for g in &row.glyphs {
            if g.chr.is_whitespace() {
                if let Some(r) = run.take() {
                    push(out, r);
                }
                continue;
            }
            let mut c = t.override_text_color.unwrap_or_else(|| row.visuals.mesh.vertices.get(g.first_vertex as usize).map_or(t.fallback_color, |v| v.color));
            if c == Color32::PLACEHOLDER {
                c = t.fallback_color;
            }
            if t.opacity_factor < 1.0 {
                c = c.gamma_multiply(t.opacity_factor);
            }
            let x0 = base.x + pr.pos.x + g.pos.x;
            let x1 = x0 + g.advance_width;
            match &mut run {
                Some((_, b, rc)) if *rc == c && x0 - *b < 1.0 => *b = x1,
                _ => {
                    if let Some(r) = run.take() {
                        push(out, r);
                    }
                    run = Some((x0, x1, c));
                }
            }
        }
        if let Some(r) = run {
            push(out, r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_hover_test::run_frame;

    /// A sketch is redrawn when it is missing, from before a change, or of
    /// another height than the unit's now (a row in it opened).
    #[test]
    fn sketches_follow_the_units_height() {
        let mut u = Unit { h: Some(40.0), ..Default::default() };
        assert!(u.needs_sketch());
        u.sketch_h = Some(40.0);
        assert!(!u.needs_sketch());
        u.h = Some(120.0);
        assert!(u.needs_sketch());
        let mut list = List::default();
        list.sync(&[1, 2], 300.0);
        list.units[0].h = Some(10.0);
        list.units[0].sketch_h = Some(10.0);
        list.sync(&[3, 2], 300.0);
        assert!(list.units[0].needs_sketch());
    }

    /// Units' tops, kept between changes, are the sums of the heights
    /// above them as units are measured, change and come and go.
    #[test]
    fn tops_are_the_heights_above() {
        let mut list = List::default();
        let sums = |l: &List| (0..=l.units.len()).map(|i| (0..i).map(|k| l.height(k)).fold(0.0, |a, b| a + b)).collect::<Vec<f32>>();
        let tops = |l: &List| (0..=l.units.len()).map(|i| l.top_of(i)).collect::<Vec<f32>>();
        list.sync(&[1, 2, 3, 4], 300.0);
        assert_eq!(tops(&list), sums(&list));
        for (i, h) in [(0, 10.0), (2, 33.5), (3, 7.25)] {
            list.units[i].h = Some(h);
            list.heights_changed(i);
            assert_eq!(tops(&list), sums(&list));
        }
        // A new estimate for the unmeasured, a changed unit, one more, one less.
        for sigs in [&[1, 2, 3, 4][..], &[1, 9, 3, 4], &[1, 9, 3, 4, 5], &[1, 9]] {
            list.sync(sigs, 300.0);
            assert_eq!(tops(&list), sums(&list));
        }
        assert_eq!(list.top_of(99), list.total());
    }

    /// A row under the pointer stays hovered while another unit on screen
    /// keeps changing (a tool's output streaming in): sketching that unit
    /// off screen first must not change the ids of the widgets on screen.
    #[test]
    fn hover_holds_while_a_unit_streams() {
        let ctx = egui::Context::default();
        let t = crate::app_hover_test::warm_theme();
        let mut list = List::default();
        let screen = vec2(800.0, 600.0);
        let row_at = pos2(200.0, 12.0);
        let mut hovered = vec![];
        let mut time = 0.0;
        for frame in 0..12 {
            // Unit 1 grows every other frame; unit 2 is the tail.
            let lines = 1 + frame / 2;
            let mut lit = false;
            run_frame(&ctx, screen, &mut time, 0.05, vec![egui::Event::PointerMoved(row_at)], |ui| {
                list.sync(&[1, lines as u64, 7], 600.0);
                let view = ui.max_rect();
                list.input(ui, &t, view);
                let mut render = |ui: &mut Ui, i: usize| match i {
                    0 => {
                        let (_, _, h) = crate::transcript::row(ui, &t, egui::Id::unique("hover-test-row"), true, true);
                        lit |= h;
                    }
                    1 => {
                        for k in 0..lines {
                            ui.label(format!("line {k}"));
                        }
                    }
                    _ => {
                        ui.label("tail");
                    }
                };
                list.show(ui, view, Rangef::new(0.0, 600.0), &mut render);
            });
            hovered.push(lit);
        }
        // The first frame or two have no widget rects yet.
        assert!(hovered[3..].iter().all(|&h| h), "{hovered:?}");
    }
}
