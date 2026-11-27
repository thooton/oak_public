//! Which account a conversation's requests go through, for every provider
//! with more than one.
//!
//! A conversation keeps its account (its seat) for as long as it can: the
//! prompt cache lives on the account, so moving a conversation whose cache
//! is warm writes it all again. It moves at a usage limit, or, when its cache
//! has gone cold anyway, when its account runs low and another has much
//! more left. A conversation without a seat takes the account with the most
//! left after what the conversations already on it will use, so parallel
//! sessions start on separate accounts and the accounts fill evenly.

use crate::providers::Kind;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

/// An account a request could go through, as rotation weighs it.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// Its id within its provider.
    pub id: String,
    /// How much it has left: percent of its tightest limit for a
    /// subscription, dollars for credits; equal for every account of a free
    /// provider.
    pub left: f64,
    /// It can make requests now.
    pub usable: bool,
    /// When its tightest limit resets (ms since 1970); `i64::MAX` when it
    /// has none. Between equals, the one that resets first goes first: what
    /// it has left would be lost at the reset.
    pub resets_at: i64,
    /// Another account is used only when none of these can make requests
    /// (logins on this machine, once the vault has accounts).
    pub preferred: bool,
}

/// A seat: the account a conversation is on, and when its cache was last
/// written or read.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Seat {
    account: String,
    used_at: i64,
    /// How long its cache lives after `used_at` (ms).
    ttl_ms: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct Seats {
    /// By provider id, then conversation id.
    by: HashMap<String, HashMap<String, Seat>>,
}

static SEATS: Mutex<Option<Seats>> = Mutex::new(None);

fn path() -> std::path::PathBuf {
    crate::store::dir().join("seats.json")
}

fn with<T>(f: impl FnOnce(&mut Seats) -> T) -> T {
    let mut s = SEATS.lock().unwrap();
    let seats = s.get_or_insert_with(|| if cfg!(test) { Seats::default() } else { crate::store::read_json(&path()).unwrap_or_default() });
    f(seats)
}

fn save(s: &Seats) {
    if !cfg!(test) {
        let _ = crate::store::write_json(&path(), s);
    }
}

/// An account below this much left (percent) moves its conversations, when
/// their cache is cold, to one with `MOVE_GAIN` more.
const LOW: f64 = 15.0;
const MOVE_GAIN: f64 = 20.0;

/// What one conversation on an account is expected to use of it before
/// the next pick (percent), for spreading conversations out.
const LOAD: f64 = 8.0;

/// What an active conversation of the same family (`family`: a swarm's
/// lead and agents, a session and its subagents) on an account weighs
/// against it, on top of `LOAD`. A family's conversations run at the same
/// moments (a swarm's agents wake on one board post), and several sockets
/// opened at once on one Quick account get dropped mid-reply, so they go to
/// separate accounts unless the others have far less left.
const SIBLING: f64 = 50.0;

/// A conversation counts as active on its account for this long after its
/// last request, whatever its cache's lifetime.
const ACTIVE_MS: i64 = 10 * 60 * 1000;

/// The family conversation `conv` belongs to: its id without a `#...` part
/// (a request made on its behalf, such as its summary) or a trailing
/// `-agent<N>` (a subagent's or swarm agent's), so a swarm's lead and its
/// agents share one.
pub(crate) fn family(conv: &str) -> &str {
    let conv = conv.split('#').next().unwrap_or(conv);
    match conv.rfind("-agent") {
        Some(i) if i > 0 && conv[i + 6..].chars().all(|c| c.is_ascii_digit()) && conv.len() > i + 6 => &conv[..i],
        _ => conv,
    }
}

/// The account conversation `conv` goes through now: its seat's, or a new
/// one (taken as its seat). None when no account can make requests. `fixed`
/// (rotation off): a conversation with a seat keeps it even when it can't
/// make requests (the request waits for its limit to reset).
pub fn pick(kind: Kind, conv: &str, cands: &[Candidate], now: i64, fixed: bool) -> Option<String> {
    with(|s| {
        let seats = s.by.entry(kind.id().to_string()).or_default();
        let picked = choose(seats, conv, cands, now, fixed)?;
        if !conv.is_empty() && seats.get(conv).is_none_or(|seat| seat.account != picked) {
            // It counts on its account from now: a conversation picking
            // before this one's first reply is back goes elsewhere.
            seats.insert(conv.to_string(), Seat { account: picked.clone(), used_at: now, ttl_ms: 0 });
            save(s);
        }
        Some(picked)
    })
}

fn choose(seats: &HashMap<String, Seat>, conv: &str, cands: &[Candidate], now: i64, fixed: bool) -> Option<String> {
    let seat = seats.get(conv).filter(|_| !conv.is_empty());
    let current = seat.and_then(|seat| cands.iter().find(|c| c.id == seat.account));
    if let (Some(c), Some(seat)) = (current, seat) {
        if fixed {
            return Some(c.id.clone());
        }
        // A preferred account came back while it is on a fallback: it
        // moves, as soon as its cache is cold.
        let warm = now - seat.used_at < seat.ttl_ms;
        if c.usable && (warm || !wants_move(c, cands)) {
            return Some(c.id.clone());
        }
    }
    best(seats, conv, cands, now)
}

/// Whether a cold conversation should leave account `c`: it runs low and
/// another has much more left, or it is a fallback and a preferred account
/// can make requests.
fn wants_move(c: &Candidate, cands: &[Candidate]) -> bool {
    let usable = || cands.iter().filter(|o| o.usable && o.id != c.id);
    (!c.preferred && usable().any(|o| o.preferred)) || (c.left < LOW && usable().any(|o| o.left >= c.left + MOVE_GAIN))
}

/// The account a conversation without a usable seat takes: the most left
/// after the other active conversations' expected use, its own family's
/// weighing far more (`SIBLING`), a preferred one when any can make
/// requests; between equals, the one that resets first.
fn best(seats: &HashMap<String, Seat>, conv: &str, cands: &[Candidate], now: i64) -> Option<String> {
    let preferred = cands.iter().any(|c| c.usable && c.preferred);
    let kin = family(conv);
    // (Other active conversations on account `id`; only its family's, when `kin_only`.)
    let active = |id: &str, kin_only: bool| {
        seats
            .iter()
            .filter(|(k, s)| k.as_str() != conv && s.account == id && now - s.used_at < ACTIVE_MS.max(s.ttl_ms))
            .filter(|(k, _)| !kin_only || family(k) == kin)
            .count() as f64
    };
    let load = |id: &str| active(id, false);
    let siblings = |id: &str| active(id, true);
    let score = |c: &Candidate| c.left - LOAD * load(&c.id) - SIBLING * siblings(&c.id);
    cands
        .iter()
        .filter(|c| c.usable && (c.preferred || !preferred))
        .max_by(|a, b| score(a).total_cmp(&score(b)).then(b.resets_at.cmp(&a.resets_at)))
        .map(|c| c.id.clone())
}

/// Weighs accounts without reserving a seat, e.g. while a handoff is being written.
pub(crate) fn preview(kind: Kind, conv: &str, cands: &[Candidate], now: i64) -> Option<String> {
    with(|s| best(s.by.entry(kind.id().to_string()).or_default(), conv, cands, now))
}

/// A request of `conv` went through `account`: its cache there lives
/// `ttl_ms` from now.
pub fn used(kind: Kind, conv: &str, account: &str, ttl_ms: i64, now: i64) {
    if conv.is_empty() {
        return;
    }
    with(|s| {
        let seats = s.by.entry(kind.id().to_string()).or_default();
        let moved = seats.get(conv).is_none_or(|seat| seat.account != account);
        seats.insert(conv.to_string(), Seat { account: account.to_string(), used_at: now, ttl_ms });
        if moved {
            save(s);
        }
    })
}

/// The account `conv` is on, if any.
pub fn seat(kind: Kind, conv: &str) -> Option<String> {
    with(|s| s.by.get(kind.id()).and_then(|m| m.get(conv)).map(|seat| seat.account.clone()))
}

/// The conversations active on each account of `kind`, by account id.
pub fn active(kind: Kind, now: i64) -> HashMap<String, usize> {
    with(|s| {
        let mut out = HashMap::new();
        for seat in s.by.get(kind.id()).into_iter().flat_map(|m| m.values()) {
            if now - seat.used_at < ACTIVE_MS.max(seat.ttl_ms) {
                *out.entry(seat.account.clone()).or_default() += 1;
            }
        }
        out
    })
}

/// How many other conversations of `conv`'s family (`family`) are active on
/// `account`, besides `conv` and requests made on its behalf.
pub(crate) fn kin_on(kind: Kind, conv: &str, account: &str, now: i64) -> usize {
    let kin = family(conv);
    with(|s| {
        s.by.get(kind.id()).map_or(0, |m| {
            m.iter()
                .filter(|(k, seat)| {
                    k.split('#').next() != Some(conv)
                        && family(k) == kin
                        && seat.account == account
                        && now - seat.used_at < ACTIVE_MS.max(seat.ttl_ms)
                })
                .count()
        })
    })
}

/// Forgets the seats on an account that was removed.
pub fn forget_account(kind: Kind, account: &str) {
    with(|s| {
        if let Some(m) = s.by.get_mut(kind.id()) {
            m.retain(|_, seat| seat.account != account);
        }
        save(s);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, left: f64) -> Candidate {
        Candidate { id: id.into(), left, usable: true, resets_at: i64::MAX, preferred: true }
    }

    fn seat(account: &str, used_at: i64, ttl_ms: i64) -> Seat {
        Seat { account: account.into(), used_at, ttl_ms }
    }

    const MIN: i64 = 60_000;

    #[test]
    fn a_warm_conversation_stays_put() {
        let mut seats = HashMap::new();
        seats.insert("c".to_string(), seat("a", 0, 5 * MIN));
        let cands = [cand("a", 5.0), cand("b", 90.0)];
        // Warm: stays, even low.
        assert_eq!(choose(&seats, "c", &cands, 4 * MIN, false).as_deref(), Some("a"));
        // Cold and low with a much fuller one: moves.
        assert_eq!(choose(&seats, "c", &cands, 6 * MIN, false).as_deref(), Some("b"));
        // Cold but not low: stays.
        let cands = [cand("a", 40.0), cand("b", 90.0)];
        assert_eq!(choose(&seats, "c", &cands, 6 * MIN, false).as_deref(), Some("a"));
        // Used up: moves, warm or not; with rotation off it stays.
        let cands = [Candidate { usable: false, ..cand("a", 0.0) }, cand("b", 90.0)];
        assert_eq!(choose(&seats, "c", &cands, MIN, false).as_deref(), Some("b"));
        assert_eq!(choose(&seats, "c", &cands, MIN, true).as_deref(), Some("a"));
    }

    #[test]
    fn conversations_picking_together_spread_out() {
        const T: i64 = 1_800_000_000_000; // a real clock: an unused seat (0) is long cold
        let cands = [cand("rot-p", 50.0), cand("rot-q", 50.0)];
        let first = pick(Kind::OpenRouter, "rot-1", &cands, T, false).unwrap();
        let second = pick(Kind::OpenRouter, "rot-2", &cands, T + 1, false).unwrap();
        assert_ne!(first, second, "both on {first} before either's reply came back");
        // The new seat isn't warm: it doesn't hold a conversation on a used-up account.
        let cands = [Candidate { usable: false, ..cand(&first, 0.0) }, cand(&second, 50.0)];
        assert_eq!(pick(Kind::OpenRouter, "rot-1", &cands, T + 2, false), Some(second));
    }

    #[test]
    fn a_family_is_its_id_without_an_agent_or_request_suffix() {
        assert_eq!(family("fork-1-ab"), "fork-1-ab");
        assert_eq!(family("fork-1-ab-agent3"), "fork-1-ab");
        assert_eq!(family("fork-1-ab-agent12#summary"), "fork-1-ab");
        assert_eq!(family("1795-agent"), "1795-agent", "no number: not an agent");
        assert_eq!(family("my-agentx"), "my-agentx");
        assert_eq!(family("1795756505455"), "1795756505455");
    }

    /// A swarm's agents go to separate accounts even when one has much more
    /// left, and share one only when nothing else is close.
    #[test]
    fn a_swarm_spreads_over_accounts() {
        let mut seats = HashMap::new();
        seats.insert("fork-s".to_string(), seat("a", 0, 0));
        seats.insert("fork-s-agent1".to_string(), seat("b", 0, 0));
        seats.insert("other".to_string(), seat("c", 0, 0));
        let cands = [cand("a", 95.0), cand("b", 90.0), cand("c", 60.0), cand("d", 55.0)];
        // Without family, "a" would win (95 - 8 = 87).
        assert_eq!(best(&seats, "fork-s-agent2", &cands, MIN).as_deref(), Some("d"));
        // An unrelated conversation still takes the fullest.
        assert_eq!(best(&seats, "new", &cands, MIN).as_deref(), Some("a"));
        // Its summary avoids the account it and its siblings are on.
        assert_eq!(best(&seats, "fork-s#summary", &cands, MIN).as_deref(), Some("d"));
        // Every other account nearly empty: it shares.
        let cands = [cand("a", 95.0), cand("b", 90.0), cand("c", 5.0), cand("d", 5.0)];
        assert_eq!(best(&seats, "fork-s-agent2", &cands, MIN).as_deref(), Some("a"));
    }

    #[test]
    fn new_conversations_spread_out() {
        let mut seats = HashMap::new();
        let cands = [cand("a", 60.0), cand("b", 55.0)];
        assert_eq!(best(&seats, "x", &cands, 0).as_deref(), Some("a"));
        seats.insert("x".to_string(), seat("a", 0, 5 * MIN));
        // "a" has an active conversation: the next goes to "b".
        assert_eq!(best(&seats, "y", &cands, MIN).as_deref(), Some("b"));
        // Equal: the one that resets first.
        let cands = [Candidate { resets_at: 9, ..cand("a", 50.0) }, Candidate { resets_at: 5, ..cand("b", 50.0) }];
        assert_eq!(best(&HashMap::new(), "z", &cands, 0).as_deref(), Some("b"));
        // Preferred accounts first.
        let cands = [Candidate { preferred: false, ..cand("a", 100.0) }, cand("b", 10.0)];
        assert_eq!(best(&HashMap::new(), "z", &cands, 0).as_deref(), Some("b"));
    }
}
