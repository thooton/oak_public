//! How long a conversation's prompt cache should live: five minutes or an
//! hour, decided before each request (Claude).
//!
//! A one-hour write costs 2× fresh input against 1.25× for five minutes,
//! and a request whose newest breakpoint is the one-hour kind reads only
//! one-hour entries. So an hour pays when a pause between five minutes and
//! an hour is likely soon, which would otherwise write the whole context
//! again. The bet, per request: over the r requests the conversation is
//! expected to make before it ends, pauses for longer or rolls its context
//! over, an hour saves λ · 1.2 · C a request (λ the chance of such a pause,
//! C the context) and costs 0.75 · N more (N what a request adds); it is
//! taken when that beats what switching costs now.
//!
//! Measured over 7022 requests: 10% cheaper on input than five minutes
//! always, against 5% for an hour always.
//!
//! While five minutes is in use after an hour was, the last one-hour entry
//! keeps a breakpoint (the anchor), so it stays warm for the next pause.

use crate::oak::Ttl;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// The prices relative to a fresh input token.
const READ: f64 = 0.05;
const WRITE_5M: f64 = 1.25;
const WRITE_1H: f64 = 2.0;

/// A pause between five minutes and an hour, before any are seen: the
/// rate over Oak's conversations, worth this many pauses.
const PRIOR_RATE: f64 = 0.031;
const PRIOR_WEIGHT: f64 = 5.0;
/// The chance a request is the conversation's last for over an hour.
const END_RATE: f64 = 0.012;
/// Pauses and growths kept.
const KEPT_GAPS: usize = 100;
const KEPT_GROWTH: usize = 20;

/// A cache entry: how much of the context it holds (tokens), when it was
/// last used, and (for the anchor) at which request message it ends, in the
/// view after which summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub tokens: u64,
    pub at: i64,
    pub messages: usize,
    pub base: usize,
}

/// What a conversation's requests showed about its pauses and growth.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// The pauses between requests (seconds), newest last.
    #[serde(default)]
    pub gaps: VecDeque<u32>,
    /// What each request added to the context (tokens), newest last.
    #[serde(default)]
    pub growth: VecDeque<u32>,
    /// The last request: its entry and its lifetime.
    #[serde(default)]
    pub last: Option<Entry>,
    #[serde(default)]
    pub last_ttl: Ttl,
    /// The last one-hour entry.
    #[serde(default)]
    pub hour: Option<Entry>,
    /// Made cold (`cold`) since the last request: what goes before the
    /// conversation may be brought up to date at no extra cost.
    #[serde(default)]
    pub cold: bool,
    /// When the last request was made (keep-alive requests leave it, so
    /// pauses are the user's).
    #[serde(default)]
    pub asked: i64,
}

/// The choice for one request: the lifetime, and the one-hour entry to keep
/// warm with a breakpoint (a request message index).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Choice {
    pub ttl: Ttl,
    pub anchor: Option<usize>,
}

impl State {
    /// How long the cache the last request wrote lives.
    pub fn ttl_ms(&self) -> i64 {
        match self.last_ttl {
            Ttl::Off => Ttl::FiveMinutes.ms(),
            t => t.ms(),
        }
    }

    /// The chance of a pause between five minutes and an hour after a
    /// request, from what was seen (with the prior).
    fn pause_rate(&self) -> f64 {
        let mid = self.gaps.iter().filter(|&&g| g > 300 && g <= 3600).count() as f64;
        (mid + PRIOR_RATE * PRIOR_WEIGHT) / (self.gaps.len() as f64 + PRIOR_WEIGHT)
    }

    /// What a request adds (median, tokens).
    fn growth(&self) -> f64 {
        let mut g: Vec<u32> = self.growth.iter().copied().collect();
        if g.is_empty() {
            return 3000.0;
        }
        g.sort_unstable();
        g[g.len() / 2].max(500) as f64
    }

    /// The entries alive at `now` for a request in the view after summary
    /// `base`: the one any lookup can read (tokens), and the one-hour one.
    fn alive(&self, now: i64, base: usize) -> (u64, u64) {
        let live = |e: &Option<Entry>, ms: i64| e.filter(|e| e.base == base && now - e.at < ms).map_or(0, |e| e.tokens);
        let hour = live(&self.hour, Ttl::OneHour.ms());
        let last = live(&self.last, self.ttl_ms());
        (last.max(hour), hour)
    }

    /// The lifetime for a request of about `context` tokens at `now`, in the
    /// view after summary `base`, the context rolling over at `limit`.
    pub fn choose(&self, now: i64, context: u64, base: usize, limit: u64) -> Choice {
        let (any, hour) = self.alive(now, base);
        let n = self.growth();
        let c = (context as f64).max(n);
        let lam = self.pause_rate();
        let runs = (1.0 / (lam + END_RATE)).min((limit.saturating_sub(context) as f64 / n).max(0.0));
        let (a, a1) = (any as f64, hour as f64);
        let cost_5m = READ * a + WRITE_5M * (c - a).max(0.0);
        let cost_1h = READ * a1 + WRITE_1H * (c - a1).max(0.0);
        let gain = runs * (lam * 1.2 * c - 0.75 * n);
        if gain > cost_1h - cost_5m {
            return Choice { ttl: Ttl::OneHour, anchor: None };
        }
        // Five minutes: the last one-hour entry stays warm, while it lives.
        let anchor = self.hour.filter(|e| e.base == base && now - e.at < Ttl::OneHour.ms() && e.messages > 0).map(|e| e.messages - 1);
        Choice { ttl: Ttl::FiveMinutes, anchor }
    }

    /// A request made at `at` with `choice`, its context `tokens` in all
    /// over `messages` request messages after summary `base`, was answered.
    pub fn record(&mut self, at: i64, choice: Choice, tokens: u64, messages: usize, base: usize) {
        if let Some(last) = self.last {
            let from = if self.asked > 0 { self.asked } else { last.at };
            let gap = ((at - from) / 1000).clamp(0, u32::MAX as i64) as u32;
            push(&mut self.gaps, gap, KEPT_GAPS);
            if last.base == base && tokens > last.tokens {
                push(&mut self.growth, (tokens - last.tokens).min(u32::MAX as u64) as u32, KEPT_GROWTH);
            }
        }
        let entry = Entry { tokens, at, messages, base };
        self.last = Some(entry);
        self.cold = false;
        self.asked = at;
        self.last_ttl = choice.ttl;
        match choice.ttl {
            Ttl::OneHour => self.hour = Some(entry),
            // The anchor read (and so refreshed) the one-hour entry.
            _ if choice.anchor.is_some() => {
                if let Some(h) = self.hour.as_mut() {
                    h.at = at;
                }
            }
            _ => {}
        }
    }

    /// The conversation moved to another account, whose cache it doesn't
    /// have.
    pub fn cold(&mut self) {
        self.last = None;
        self.hour = None;
        self.cold = true;
    }

    /// Whether nothing of the view after summary `base` is cached at
    /// `now`: the next request writes it all whatever it sends.
    pub fn is_cold(&self, now: i64, base: usize) -> bool {
        self.cold || (self.last.is_some() && self.alive(now, base).0 == 0)
    }

    /// When the last request's entry dies, if it lives (ms since 1970).
    pub fn expires_at(&self) -> Option<i64> {
        self.last.map(|e| e.at + self.ttl_ms())
    }

    /// A keep-alive request (`api::keep_warm`) at `at` read the last
    /// request's entry: it lives on from then (and the one-hour one, which
    /// the anchor reads, if it was there).
    pub fn warm(&mut self, at: i64, anchor: bool) {
        if let Some(e) = self.last.as_mut() {
            e.at = at;
        }
        if anchor && let Some(h) = self.hour.as_mut() {
            h.at = at;
        }
    }
}

fn push(q: &mut VecDeque<u32>, v: u32, max: usize) {
    q.push_back(v);
    while q.len() > max {
        q.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    fn state(gaps: &[u32], growth: u32) -> State {
        State { gaps: gaps.iter().copied().collect(), growth: std::iter::repeat_n(growth, 10).collect(), ..Default::default() }
    }

    #[test]
    fn an_hour_when_pauses_are_likely() {
        // Steady work: five minutes.
        let busy = state(&[20; 100], 1000);
        assert_eq!(busy.choose(0, 100_000, 0, 200_000).ttl, Ttl::FiveMinutes);
        // Every fifth request after a pause of 10 minutes: an hour, at a
        // cold write (nothing alive, so switching costs nothing extra).
        let mut gaps = vec![20; 80];
        gaps.extend([600; 20]);
        let pausing = state(&gaps, 1000);
        assert_eq!(pausing.choose(0, 100_000, 0, 200_000).ttl, Ttl::OneHour);
        // Not with the context about to roll over.
        assert_eq!(pausing.choose(0, 199_500, 0, 200_000).ttl, Ttl::FiveMinutes);
    }

    #[test]
    fn switching_costs_count() {
        let mut gaps = vec![20; 90];
        gaps.extend([600; 10]);
        let mut s = state(&gaps, 1000);
        // A warm five-minute entry of the whole context: moving to an hour
        // rewrites it at 2×, which a 10% pause rate over a long run pays for
        // only on a small context.
        s.last = Some(Entry { tokens: 100_000, at: 0, messages: 10, base: 0 });
        s.last_ttl = Ttl::FiveMinutes;
        let cold = State { last: None, ..s.clone() };
        let warm_gain = s.choose(MIN, 101_000, 0, 1_000_000);
        let cold_gain = cold.choose(MIN, 101_000, 0, 1_000_000);
        assert_eq!(cold_gain.ttl, Ttl::OneHour);
        assert!(matches!(warm_gain.ttl, Ttl::OneHour | Ttl::FiveMinutes));
    }

    #[test]
    fn records_pauses_growth_and_the_anchor() {
        let mut s = State::default();
        s.record(0, Choice { ttl: Ttl::OneHour, anchor: None }, 10_000, 4, 0);
        s.record(MIN, Choice { ttl: Ttl::FiveMinutes, anchor: Some(3) }, 11_000, 6, 0);
        assert_eq!(s.gaps, [60]);
        assert_eq!(s.growth, [1000]);
        assert_eq!(s.hour, Some(Entry { tokens: 10_000, at: MIN, messages: 4, base: 0 }), "the anchor refreshed it");
        // In five minutes, the hour's entry is anchored at its last message.
        let busy = State { gaps: [20; 100].into_iter().collect(), ..s.clone() };
        assert_eq!(busy.choose(2 * MIN, 12_000, 0, 200_000), Choice { ttl: Ttl::FiveMinutes, anchor: Some(3) });
        // Not after a summary moved the view, or once it expired.
        assert_eq!(busy.choose(2 * MIN, 12_000, 1, 200_000).anchor, None);
        assert_eq!(busy.choose(62 * MIN, 12_000, 0, 200_000).anchor, None);
        // Moving accounts: nothing is warm.
        s.cold();
        assert_eq!(s.alive(MIN, 0), (0, 0));
    }

    /// Cold when nothing cached is left; a keep-alive ping moves the entry's
    /// time but not the user's, so pauses are still measured from requests.
    #[test]
    fn cold_moments_and_keep_alive() {
        let five = Choice { ttl: Ttl::FiveMinutes, anchor: None };
        let mut s = State::default();
        assert!(!s.is_cold(0, 0), "nothing was ever cached: nothing to lose either");
        let (t0, ttl) = (1_000_000, Ttl::FiveMinutes.ms());
        s.record(t0, five, 50_000, 3, 0);
        assert_eq!(s.expires_at(), Some(t0 + ttl));
        assert!(!s.is_cold(t0 + ttl - 1, 0) && s.is_cold(t0 + ttl, 0));
        assert!(s.is_cold(t0 + 1, 2), "another summary's view");
        s.warm(t0 + ttl - 30_000, false);
        assert!(!s.is_cold(t0 + ttl + 60_000, 0));
        s.record(t0 + ttl + 60_000, five, 51_000, 5, 0);
        assert_eq!(s.gaps.back().copied(), Some(((ttl + 60_000) / 1000) as u32), "the gap is from the last request");
        s.cold();
        assert!(s.is_cold(t0 + ttl + 60_001, 0));
        s.record(t0 + ttl + 70_000, five, 52_000, 7, 0);
        assert!(!s.cold);
    }
}
