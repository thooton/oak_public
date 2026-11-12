//! A transcript in its tab: a list (`vlist`) in the panel, its scroll bar at
//! the right edge and the minimap beside it (when there is room), and the bar
//! to jump back to the end when far up. A conversation's (`chat_view`), and a
//! cloud session's or an Oak agent's (`read_only_view`), are drawn the same
//! way.

use super::*;
use std::ops::Range;

/// Where a transcript's parts go in `view`.
pub(crate) struct Frame {
    pub view: Rect,
    /// The scroll bar's track.
    pub track: Rect,
    /// The column, lined up with the message box, clear of the minimap.
    pub col: Rangef,
    /// The minimap, when there is room for it.
    pub map: Option<Rect>,
}

impl Frame {
    pub fn new(view: Rect) -> Frame {
        let track = Rect::from_min_max(egui::pos2(view.right() - vlist::BAR_W_HOVER - 2.0, view.top() + 2.0), egui::pos2(view.right() - 2.0, view.bottom() - 2.0));
        let mut col = chat_column(view.x_range());
        let map_on = view.width() >= 640.0;
        let limit = track.left() - if map_on { vlist::MINIMAP_W + 16.0 } else { 4.0 };
        if col.max > limit {
            col = Rangef::new(col.min.min((limit - 100.0).max(view.left())), limit);
        }
        let map = map_on.then(|| Rect::from_min_max(egui::pos2(track.left() - 6.0 - vlist::MINIMAP_W, view.top()), egui::pos2(track.left() - 6.0, view.bottom())));
        Frame { view, track, col, map }
    }

    /// Shows the list (already `sync`ed): its `units` of `items`, then `tail`
    /// at the end, with the scroll bar, the minimap (`cut` marks where the
    /// summary open covers it) and the jump bar. `cleared`: the unit before
    /// which old tool output is cleared, marked in the transcript only.
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &self,
        ui: &mut Ui,
        t: &Theme,
        list: &mut vlist::List,
        items: &[Item],
        units: &[Range<usize>],
        results: &transcript::Results,
        revert: Option<&[Value]>,
        mut tail: impl FnMut(&mut Ui),
        cut: impl FnOnce(&Ui) -> Option<usize>,
        cleared: Option<usize>,
    ) {
        let Frame { view, track, col, map } = *self;
        list.input(ui, t, view);
        list.bar_input(ui, track);
        if let Some(map) = map {
            list.map_input(ui, map, col.span());
        }
        let cut = cut(ui);
        // Where the line marking the summarized part ends: at the minimap
        // (its cut line goes on from there), else at the scroll bar.
        let mark_end = map.map_or(track.left(), |m| m.left()) - 4.0;
        let mut render = |ui: &mut Ui, i: usize| {
            if i == 0 {
                ui.add_space(20.0);
            }
            crate::find::begin_unit(i);
            let top = ui.cursor().top();
            match units.get(i) {
                Some(u) => transcript_unit(ui, t, items, u.clone(), results, revert),
                None => {
                    tail(ui);
                    ui.add_space(20.0);
                }
            }
            if cut == Some(i) && i > 0 {
                mark_line(ui, t.accent, "above context is summarized", top, col, mark_end);
            } else if cleared == Some(i) && i > 0 {
                mark_line(ui, CLEARED_BLUE, "above tool output is cleared", top, col, mark_end);
            }
        };
        list.show(ui, view, col, &mut render);
        list.bar_paint(ui, t, track);
        if let Some(map) = map {
            list.map_paint(ui, t, map, col.span(), cut);
        }
        if list.far_from_end() && jump_bar(ui, t, Rect::from_x_y_ranges(col, view.bottom() - 40.0..=view.bottom() - 10.0)) {
            list.jump_to_end();
        }
    }
}

/// A transcript nothing can be reverted from (a cloud session's, an Oak
/// agent's), with `tail` at its end (its state hashed in `tail_sig`).
pub(crate) fn read_only_view(ui: &mut Ui, t: &Theme, conv: &store::Conversation, live: &Live, list: &mut vlist::List, tail_sig: u64, tail: impl FnMut(&mut Ui)) {
    let frame = Frame::new(ui.max_rect());
    let (mut items, _, results) = items_at(conv, live);
    for x in &mut items {
        if let Item::UserText(_, r, ..) | Item::Image(_, r) | Item::File(_, r) = x {
            *r = None;
        }
    }
    let units = units(&items);
    let mut sigs: Vec<u64> = units.iter().map(|u| unit_sig(&items[u.clone()], &results)).collect();
    sigs.push(tail_sig);
    list.sync(&sigs, frame.col.span());
    crate::transcript::for_conversation(&conv.id, || frame.show(ui, t, list, &items, &units, &results, None, tail, |_| None, None));
}

/// A conversation's transcript. Returns what was done to the queue.
pub(crate) fn chat_view(ui: &mut Ui, t: &Theme, id: &str, s: &Session, list: &mut vlist::List, editing: &mut Option<Editing>, agents_pausing: bool) -> Option<QueueAction> {
    let frame = Frame::new(ui.max_rect());
    let conv = s.conv.lock().unwrap();
    let mut live = s.live.lock().unwrap();
    live.drawn = ui.ctx().cumulative_pass_nr();
    live.drawn_viewport = Some(ui.ctx().viewport_id());
    // (Out while the items borrow the rest; back before the lock goes.)
    let mut ask = live.ask.take();
    let mut action = None;
    // The user's answer to a REQUEST_ASK, applied once the locks are let go.
    let mut ask_reply = None;
    // The conversation's items, kept from an earlier frame while it is
    // unchanged (`Built`), unless calls ran while their reply still
    // streams in: their results are the reply's.
    let mut built = None;
    let mut fresh = None;
    {
        let (items, msgs, results, units, mut sigs) = if let Some(early) = &live.early {
            let (items, msgs, mut results) = items_at(&conv, &live);
            crate::transcript::add_results(&mut results, early);
            let units = units(&items);
            let sigs: Vec<u64> = units.iter().map(|u| unit_sig(&items[u.clone()], &results)).collect();
            let (items, msgs, results) = fresh.insert((items, msgs, results));
            (&items[..], &msgs[..], &*results, units, sigs)
        } else {
            built.insert(Built::take(id, &conv)).frame(&conv, &live)
        };
        let mut tail = tail_sig(&conv, &live, editing) ^ agents_pausing as u64;
        if let Some(a) = &ask {
            tail ^= a.questions.len() as u64 + 1;
        }
        // Find (Ctrl+F): in the messages, as shown.
        let texts = |u: usize| {
            items[units[u].clone()]
                .iter()
                .filter_map(|x| match x {
                    Item::UserText(s, ..) => Some(crate::find::Text::Plain(s)),
                    Item::Text(s, _, _, _, None, _) => Some(crate::find::Text::Markdown(s)),
                    Item::Text(s, _, _, _, Some(r), _) => Some(crate::find::Text::Markdown(transcript::shown_text(s, r))),
                    _ => None,
                })
                .collect()
        };
        let find = crate::find::begin(ui, id, &sigs, texts, list.first_shown());
        sigs.push(tail);
        list.sync(&sigs, frame.col.span());
        // The unit a message starts (the first one at or after it).
        let unit_of = |msg: usize| units.iter().position(|u| msgs[u.start] >= msg).unwrap_or(units.len());
        if let Some(msg) = ui.data_mut(|d| d.remove_temp::<usize>(Id::unique(JUMP_REQUEST))) {
            list.jump_to(unit_of(msg), frame.view.height());
        }
        // The end: what OAK is doing, then its questions and the queue.
        let tail = |ui: &mut Ui| {
            transcript_status(ui, t, &conv, &live, agents_pausing);
            if let Some(a) = ask.as_mut()
                && ask_view(ui, t, a)
            {
                a.sent = true;
            }
            if !conv.ask_request.is_empty() {
                ask_reply = ask_request_view(ui, t, &conv.ask_request).or(ask_reply);
            }
            action = queue_view(ui, t, id, &live.queued, editing);
        };
        // The summarized part, always marked: the newest summary's cut,
        // regardless of the banner's open state.
        let cut = |_: &Ui| items.iter().rev().find_map(|i| if let Item::Summary { cut, .. } = i { Some(unit_of(*cut)) } else { None });
        // Where old tool output stops being cleared (by a summary or at a
        // cold moment), when any is.
        let cleared = results.values().any(|r| r.cleared).then(|| unit_of(conv.rolling.cleared_to())).filter(|&u| u < units.len());
        // File links in replies are relative to the working folder.
        crate::markdown::set_link_base(ui.ctx(), Some(&conv.folder));
        crate::transcript::for_conversation(id, || frame.show(ui, t, list, items, &units, results, Some(&conv.messages), tail, cut, cleared));
        crate::markdown::set_link_base(ui.ctx(), None);
        let right = frame.map.map_or(frame.track.left(), |m| m.left()) - 4.0;
        crate::find::end(ui, t, id, find, list, frame.view, right);
    }
    drop(fresh);
    if let Some(b) = built {
        b.put_back(id, ui.ctx().cumulative_pass_nr());
    }
    live.ask = ask;
    drop(live);
    drop(conv);
    if let Some(accept) = ask_reply {
        s.answer_ask_request(accept);
    }
    action
}

#[cfg(test)]
thread_local! {
    /// How many times this thread worked out a conversation's items
    /// (`Built::new`): the frame benchmark and tests see whether they were kept.
    pub(crate) static BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many conversations' items `Built` keeps.
const BUILT_KEPT: usize = 8;

thread_local! {
    static BUILT: std::cell::RefCell<HashMap<String, Built>> = Default::default();
}

/// A conversation's items (`conv_items`) as worked out from one revision of
/// it (`store::Shared`), kept for its next frames: a long transcript is
/// walked when it changes rather than every frame (in a 3000-turn one that
/// was most of the frame). What changes without the conversation changing
/// is worked out again each frame: the reply streaming in and the turn end
/// before it, rewrites under way and commands running (`volatile`).
struct Built {
    rev: u64,
    /// The pass it was last drawn in: the one drawn longest ago goes first.
    used: u64,
    /// Borrowed from the conversation, as it was at `rev` and still is
    /// while its revision is `rev`. Between frames they are only the
    /// conversation's (`len` of them); during one, the reply streaming in
    /// follows.
    items: Vec<Item<'static>>,
    msgs: Vec<usize>,
    results: transcript::Results<'static>,
    len: usize,
    /// Where the units worked out each frame start: at the unit of the last
    /// item that isn't a mark, as whether it ends a turn depends on what
    /// follows it.
    tail: usize,
    /// The units before `tail`, and their signatures.
    units: Vec<Range<usize>>,
    sigs: Vec<u64>,
    /// Of those units, the ones that show what isn't in the conversation: a
    /// rewrite (Unslop), or a command with no result yet whose output grows.
    volatile: Vec<usize>,
}

impl Built {
    /// The items kept for `conv` when it hasn't changed since, else new ones.
    fn take(id: &str, conv: &store::SharedGuard) -> Built {
        let rev = conv.rev();
        BUILT.with(|m| m.borrow_mut().remove(id)).filter(|b| b.rev == rev).unwrap_or_else(|| Built::new(conv, rev))
    }

    fn new(conv: &Conversation, rev: u64) -> Built {
        #[cfg(test)]
        BUILDS.set(BUILDS.get() + 1);
        let (mut items, msgs, results) = transcript::conv_items(conv);
        transcript::mark_turn_ends(&mut items, 0, false);
        let len = items.len();
        let all = units(&items);
        let last = items.iter().rposition(|x| !matches!(x, Item::Mark(_)));
        let kept = last.and_then(|l| all.iter().position(|u| u.contains(&l))).unwrap_or(0);
        let tail = all.get(kept).map_or(len, |u| u.start);
        let units = all[..kept].to_vec();
        let sigs = units.iter().map(|u| unit_sig(&items[u.clone()], &results)).collect();
        let volatile = (0..units.len()).filter(|&u| items[units[u].clone()].iter().any(|x| transcript::volatile(x, &results))).collect();
        // SAFETY: only the lifetime changes. The items are used again only
        // while the conversation's revision is still `rev` (`take`), when it
        // is unchanged (nothing can change it without a new revision) and in
        // the same place (in the `Arc` its revision belongs to), so they are
        // as good as borrowed from it then (`frame`).
        let (items, results) = unsafe { (std::mem::transmute::<Vec<Item<'_>>, Vec<Item<'static>>>(items), std::mem::transmute::<transcript::Results<'_>, transcript::Results<'static>>(results)) };
        Built { rev, used: 0, items, msgs, results, len, tail, units, sigs, volatile }
    }

    /// This frame's items (with `live`'s reply streaming in), each one's
    /// message, the tool results, the units and their signatures.
    #[allow(clippy::type_complexity)]
    fn frame<'f>(&'f mut self, conv: &'f store::SharedGuard, live: &'f Live) -> (&'f [Item<'f>], &'f [usize], &'f transcript::Results<'f>, Vec<Range<usize>>, Vec<u64>) {
        // The conversation the items were worked out from, unchanged.
        assert_eq!(conv.rev(), self.rev);
        let conv: &'f Conversation = conv;
        // SAFETY: `conv` is the conversation at `self.rev`, so the items
        // and results borrow it as it is (see `new`). The reply's items,
        // borrowed from `live`, are taken out (`put_back`, or the next
        // `frame`) before anything reads them again, and only dropped
        // otherwise.
        let items = unsafe { std::mem::transmute::<&mut Vec<Item<'static>>, &'f mut Vec<Item<'f>>>(&mut self.items) };
        let results = unsafe { std::mem::transmute::<&transcript::Results<'static>, &'f transcript::Results<'f>>(&self.results) };
        items.truncate(self.len);
        self.msgs.truncate(self.len);
        // Rewrites as they stand now: in the units that have them, and in
        // those worked out each frame (the latest reply's, where they are).
        for &u in &self.volatile {
            for x in &mut items[self.units[u].clone()] {
                transcript::refresh(x, conv);
            }
        }
        for x in &mut items[self.tail..] {
            transcript::refresh(x, conv);
        }
        transcript::push_live(items, &mut self.msgs, conv, live);
        transcript::mark_turn_ends(items, self.tail, live.busy);
        let mut units = Vec::with_capacity(self.units.len() + 8);
        units.extend_from_slice(&self.units);
        units.extend(transcript::units(&items[self.tail..]).into_iter().map(|u| u.start + self.tail..u.end + self.tail));
        let mut sigs = Vec::with_capacity(units.len() + 1);
        sigs.extend_from_slice(&self.sigs);
        for &u in &self.volatile {
            sigs[u] = unit_sig(&items[units[u].clone()], results);
        }
        sigs.extend(units[self.units.len()..].iter().map(|u| unit_sig(&items[u.clone()], results)));
        (items, &self.msgs, results, units, sigs)
    }

    /// Kept for the next frame, drawn in pass `pass`, without the reply.
    fn put_back(mut self, id: &str, pass: u64) {
        self.items.truncate(self.len);
        self.msgs.truncate(self.len);
        self.used = pass;
        BUILT.with(|m| {
            let mut m = m.borrow_mut();
            m.insert(id.to_string(), self);
            if m.len() > BUILT_KEPT
                && let Some(oldest) = m.iter().min_by_key(|(_, b)| b.used).map(|(k, _)| k.clone())
            {
                m.remove(&oldest);
            }
        });
    }
}

/// The line marking where old tool output stops being cleared.
const CLEARED_BLUE: Color32 = Color32::from_rgb(0x6f, 0xa8, 0xe8);

/// A line where a part of the transcript ends (`top`, the top of the first
/// unit after it): where the summarized part ends (in the accent), or the
/// part whose old tool output is cleared (in blue). From the column's right
/// edge to `end`, with what it means over it.
fn mark_line(ui: &Ui, color: Color32, label: &str, top: f32, col: Rangef, end: f32) {
    let y = top - ui.spacing().item_spacing.y / 2.0;
    let font = egui::FontId::proportional(11.0);
    let painter = ui.painter();
    let galley = painter.layout_no_wrap(label.into(), font, color);
    // The words over the line where there is room for them right of the
    // column, else running back over its edge.
    let x0 = col.max + 8.0;
    let text_x = (end - galley.size().x).min(x0);
    let start = text_x.min(x0) - 4.0;
    painter.line_segment([egui::pos2(start, y), egui::pos2(end, y)], Stroke::new(1.0, color));
    painter.circle_filled(egui::pos2(start, y), 2.0, color);
    painter.galley(egui::pos2(text_x, y - 2.0 - galley.size().y), galley, color);
}

/// Far up the transcript: a bar over its bottom that goes back to the end.
fn jump_bar(ui: &mut Ui, t: &Theme, rect: Rect) -> bool {
    let rect = Rect::from_center_size(rect.center(), vec2(rect.width().min(220.0), rect.height()));
    let resp = ui.interact(rect, ui.scope_id().with("transcript-jump-bar"), Sense::click());
    let lit = resp.hovered();
    let p = ui.painter();
    p.rect_filled(rect.translate(vec2(0.0, 2.0)), 4.0, Color32::from_black_alpha(60));
    p.rect_filled(rect, 4.0, if lit { t.panel2 } else { t.panel });
    p.rect_stroke(rect, 4.0, Stroke::new(1.0, if lit { t.accent } else { t.line }), egui::StrokeKind::Inside);
    let color = if lit { t.ink } else { t.ink2 };
    let font = FontId::proportional(12.5);
    let g = p.layout_no_wrap("Jump to the end".into(), font, color);
    let w = g.size().x + 18.0;
    let x = rect.center().x - w / 2.0;
    // An arrow down: a stem and a head.
    let a = egui::pos2(x + 4.0, rect.center().y);
    let s = Stroke::new(1.5, if lit { t.accent } else { color });
    p.line_segment([a + vec2(0.0, -5.0), a + vec2(0.0, 5.0)], s);
    p.line_segment([a + vec2(-3.5, 1.5), a + vec2(0.0, 5.0)], s);
    p.line_segment([a + vec2(3.5, 1.5), a + vec2(0.0, 5.0)], s);
    p.galley(egui::pos2(x + 18.0, rect.center().y - g.size().y / 2.0), g, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Back to the latest").clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a frame shows, worked out from scratch (`items_at`).
    fn fresh(conv: &Conversation, live: &Live) -> (Vec<Range<usize>>, Vec<u64>, Vec<usize>) {
        let (items, msgs, results) = items_at(conv, live);
        let units = units(&items);
        let sigs = units.iter().map(|u| unit_sig(&items[u.clone()], &results)).collect();
        (units, sigs, msgs)
    }

    /// The kept items give a frame what working them out again would, with
    /// a reply streaming in or not, busy or not, marks after the last
    /// message, and after the conversation changes.
    #[test]
    fn kept_items_match_fresh_ones() {
        let mut conv: Conversation = serde_json::from_value(crate::frame_bench::long_conversation("kept", 6)).unwrap();
        conv.marks.push(store::Mark { at: conv.messages.len(), kind: store::MarkKind::GoalContinued, text: "x".into(), time: 0, done: 0, sent: String::new() });
        let shared = store::Shared::new(conv);
        let id = "kept-items-test";
        let live = |busy: bool, blocks: Vec<Value>| {
            let mut l = Live::default();
            l.busy = busy;
            l.blocks = blocks;
            l
        };
        let lives = [
            live(false, vec![]),
            live(true, vec![]),
            live(true, vec![serde_json::json!({"type": "text", "text": "partly"})]),
            live(true, vec![serde_json::json!({"type": "reasoning", "text": "hm"}), serde_json::json!({"type": "tool_call", "id": "c9", "name": "READ", "input": {}})]),
        ];
        let builds = || BUILDS.get();
        for change in 0..4 {
            match change {
                // The reply's rewrite streams in, a few words a frame.
                0 => {
                    let mut c = shared.lock().unwrap();
                    c.messages.push(serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "Earlier.", unslop::KEY: {"id": "kept-early"}}]}));
                    c.messages.push(serde_json::json!({"role": "user", "content": [{"type": "text", "text": "Go on"}]}));
                    c.messages.push(serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "Latest.", unslop::KEY: {"id": "kept-latest"}}]}));
                }
                // A reply ending in text (whether it ends the turn depends
                // on what streams in after it), then a call and its result
                // with a message.
                1 => shared.lock().unwrap().messages.push(serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "Done."}]})),
                2 => {
                    let mut c = shared.lock().unwrap();
                    c.messages.push(serde_json::json!({"role": "assistant", "content": [{"type": "tool_call", "id": "late", "name": "EXECUTE", "input": {"command": "ls"}}]}));
                    c.messages.push(serde_json::json!({"role": "user", "content": [{"type": "tool_result", "call_id": "late", "content": "a\nb"}, {"type": "text", "text": "Thanks"}]}));
                }
                _ => {
                    shared.lock().unwrap().messages.truncate(4);
                }
            }
            let before = builds();
            for (i, live) in lives.iter().enumerate() {
                unslop::set_job_text("kept-early", &"early ".repeat(i));
                unslop::set_job_text("kept-latest", &"latest ".repeat(i));
                let conv = shared.lock().unwrap();
                let (units, sigs, msgs) = fresh(&conv, live);
                let mut b = Built::take(id, &conv);
                let (items, kept_msgs, _, kept_units, kept_sigs) = b.frame(&conv, live);
                assert_eq!((&kept_units, &kept_sigs, kept_msgs), (&units, &sigs, &msgs[..]), "change {change}");
                assert_eq!(items.len(), msgs.len());
                b.put_back(id, 0);
            }
            // Worked out once for each revision, then kept.
            assert_eq!(builds() - before, 1, "change {change}");
        }
        // Reading the conversation keeps its revision; a change makes a new one.
        let rev = shared.lock().unwrap().rev();
        let _ = shared.lock().unwrap().messages.len();
        assert_eq!(shared.lock().unwrap().rev(), rev);
        shared.lock().unwrap().title.push('!');
        assert_ne!(shared.lock().unwrap().rev(), rev);
    }
}
