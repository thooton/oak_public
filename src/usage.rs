// Usage tracking: every reply's tokens and cost, with the account's plan
// utilization when it was sent, kept in usage.jsonl in OAK's folder. A
// plugin can add the records other machines made (`oak_sdk::Host`'s
// `usage_*`), which then show with this machine's.
//
// A Claude reply is valued at its API list prices (cache reads, writes,
// input and output as the API prices them) so replies to different models
// add up fairly; an OpenRouter reply at what it cost. What 1% of a plan's
// limit buys moves around a lot, so it is measured over and over: every
// reply's headers carry the utilization (whole percents, from before the
// reply), and the dollars spent between two ticks of it are what that
// percent bought.


use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The account name OpenRouter replies are kept under.
pub const OPENROUTER: &str = "OpenRouter";
/// The usage log's account for the user's own llama.cpp server.
pub const LOCAL: &str = "Local llama.cpp";
/// Between two of an account's replies, a gap this long (ms) may hide use
/// from elsewhere (Claude Code, another OAK): the percent measured across
/// it is left out.
const IDLE_MS: i64 = 30 * 60_000;

pub use oak_sdk::usage::{Rec, insert_sorted, keep_from, merge_records};

/// What a Claude reply's record says besides its tokens.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub conv: String,
    pub miss: String,
    pub missed: u64,
    pub thr: String,
    pub think_dropped: u64,
    pub think_mismatched: u64,
}

/// A model's API list prices, in dollars per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub out: f64,
    pub w5m: f64,
    pub w1h: f64,
    pub read: f64,
}

const fn price(input: f64, out: f64, read: f64) -> Price {
    Price { input, out, w5m: input * 1.25, w1h: input * 2.0, read }
}

/// Claude list prices (global, standard), November 2026:
/// platform.claude.com/docs/en/about-claude/pricing.
const OPUS_5_5: Price = price(4.0, 20.0, 0.20);

/// Model `id`'s list prices, for a request whose prompt (input, cache
/// reads and writes) is `prompt` tokens: Haiku 5.5 charges more past 100k.
/// A model not in the list is priced as the newest Opus.
pub fn price_of(id: &str, prompt: u64) -> Price {
    // Each model's family and version, worked out once: the Usage page
    // prices every reply of 90 days whenever a record comes in.
    // (Replies mostly come in runs of one model: the last is looked at
    // first.)
    thread_local! {
        static LAST: std::cell::RefCell<(String, &'static str, String)> = Default::default();
    }
    LAST.with_borrow_mut(|(last, family, version)| {
        if last != id {
            (*family, *version) = model_version(id);
            id.clone_into(last);
        }
        family_price(family, version, prompt)
    })
}

fn family_price(family: &str, version: &str, prompt: u64) -> Price {
    match (family, version) {
        ("fable" | "mythos", "5.2") => price(8.0, 40.0, 0.20),
        ("fable" | "mythos", "5.1") => price(10.0, 50.0, 0.25),
        ("fable" | "mythos", _) => price(10.0, 50.0, 1.0),
        ("opus", "5.5") => OPUS_5_5,
        ("opus", "5" | "4.8" | "4.7" | "4.6" | "4.5") => price(5.0, 25.0, 0.50),
        ("opus", "4.1" | "4" | "3") => price(15.0, 75.0, 1.50),
        ("sonnet", "5.5") => price(2.0, 10.0, 0.10),
        ("sonnet", "5") => price(2.0, 10.0, 0.20),
        ("sonnet", _) => price(3.0, 15.0, 0.30),
        ("haiku", "5.5") if prompt > 100_000 => price(0.50, 2.50, 0.05),
        ("haiku", "5.5") => price(0.10, 0.50, 0.01),
        ("haiku", "4.5") => price(1.0, 5.0, 0.10),
        ("haiku", "3.5") => price(0.80, 4.0, 0.08),
        ("haiku", _) => price(0.25, 1.25, 0.03),
        _ => OPUS_5_5,
    }
}

/// "claude-opus-5-5" → ("opus", "5.5"); "claude-haiku-5-5-20261001" →
/// ("haiku", "5.5"); "claude-3-5-haiku-20241022" → ("haiku", "3.5").
fn model_version(id: &str) -> (&'static str, String) {
    let parts: Vec<&str> = id.split(['-', '.', '@']).collect();
    let Some(at) = parts.iter().position(|p| ["fable", "mythos", "opus", "sonnet", "haiku"].contains(p)) else { return ("", String::new()) };
    let family = ["fable", "mythos", "opus", "sonnet", "haiku"].into_iter().find(|f| *f == parts[at]).unwrap_or("");
    let short = |p: &&&str| p.len() <= 2 && p.chars().all(|c| c.is_ascii_digit());
    let after: Vec<&str> = parts[at + 1..].iter().take_while(short).copied().collect();
    let version = if after.is_empty() { parts[..at].iter().filter(short).copied().collect::<Vec<_>>().join(".") } else { after.join(".") };
    (family, version)
}

/// What Oak works out from a record (`Rec` is the SDK's, so these are a
/// trait's).
pub trait RecExt {
    fn dollars(&self) -> [f64; 4];
    fn usd_total(&self) -> f64;
    fn claude(&self) -> bool;
    fn of_claude_account(&self) -> bool;
}

impl RecExt for Rec {
    /// What the reply cost (OpenRouter) or would have cost on the API
    /// (Claude), in dollars, by part: cache reads, cache writes, input,
    /// output.
    fn dollars(&self) -> [f64; 4] {
        if self.acct == OPENROUTER {
            // OpenRouter bills the whole reply; it goes under output.
            return [0.0, 0.0, 0.0, self.usd];
        }
        let p = price_of(&self.model, self.input + self.w5m + self.w1h + self.read);
        let m = |n: u64, per: f64| n as f64 * per / 1e6;
        [m(self.read, p.read), m(self.w5m, p.w5m) + m(self.w1h, p.w1h), m(self.input, p.input), m(self.out, p.out)]
    }

    fn usd_total(&self) -> f64 {
        self.dollars().iter().sum()
    }

    /// A reply through a Claude account (not OpenRouter).
    fn claude(&self) -> bool {
        self.ev == "req" && self.acct != OPENROUTER
    }

    /// A reply or a poll of a Claude account.
    fn of_claude_account(&self) -> bool {
        (self.ev == "req" || self.ev == "limits") && !self.acct.is_empty() && self.acct != OPENROUTER
    }
}

// ---------------------------------------------------------------- a reply

/// Adds up one reply's usage from its stream events.
#[derive(Default, Debug)]
pub struct Meter {
    pub input: u64,
    /// Cache writes, all, and by lifetime when the reply says.
    pub write: u64,
    pub w5m: u64,
    pub w1h: u64,
    pub read: u64,
    pub out: u64,
    pub usd: f64,
    pub model: String,
    /// A usage block came: there is something to record.
    pub seen: bool,
}

impl Meter {
    /// Takes the model, usage and cost from a reply's events (the counts
    /// are running totals: the larger one wins).
    pub fn see(&mut self, ev: &crate::oak::Event) {
        use crate::oak::Event;
        match ev {
            Event::Start { model, .. } if !model.is_empty() => self.model = model.clone(),
            Event::Usage(u) => {
                self.seen = true;
                self.input = self.input.max(u.input);
                self.write = self.write.max(u.write);
                self.read = self.read.max(u.cache_read);
                self.out = self.out.max(u.output);
                self.w5m = self.w5m.max(u.write_5m);
                self.w1h = self.w1h.max(u.write_1h);
            }
            Event::Cost(usd) => self.usd = self.usd.max(*usd),
            _ => {}
        }
    }

    /// The cache writes by lifetime: what the reply didn't break down goes
    /// to the lifetime OAK asks for.
    fn writes(&self, one_hour: bool) -> (u64, u64) {
        let rest = self.write.saturating_sub(self.w5m + self.w1h);
        if one_hour { (self.w5m, self.w1h + rest) } else { (self.w5m + rest, self.w1h) }
    }
}

/// The 5-hour and weekly utilization a reply's headers carried.
#[derive(Clone, Copy, Debug, Default)]
pub struct Util {
    pub u5: Option<f32>,
    pub r5: Option<i64>,
    pub u7: Option<f32>,
    pub r7: Option<i64>,
}

impl Util {
    /// The 5-hour and weekly ones among an account's `limits`.
    pub fn of(limits: &[crate::api::Limit]) -> Util {
        let find = |label: &str| limits.iter().find(|l| l.label == label);
        let (five, week) = (find("5h"), find("Week"));
        Util { u5: five.map(|l| l.percent), r5: five.and_then(|l| l.resets_at), u7: week.map(|l| l.percent), r7: week.and_then(|l| l.resets_at) }
    }
}

impl Util {
    /// In whole percents, to tell a change from a repeat.
    fn whole(&self) -> (Option<i32>, Option<i32>) {
        (self.u5.map(|u| u.round() as i32), self.u7.map(|u| u.round() as i32))
    }

    /// `r` with this utilization.
    fn on(self, r: Rec) -> Rec {
        Rec { u5: self.u5, r5: self.r5, u7: self.u7, r7: self.r7, ..r }
    }
}

impl Meter {
    /// The record of this reply (`kind`), its cache writes split as
    /// `writes` does.
    fn rec(&self, kind: String, one_hour_cache: bool) -> Rec {
        let (w5m, w1h) = self.writes(one_hour_cache);
        let (input, read, out, model) = (self.input, self.read, self.out, self.model.clone());
        Rec { t: crate::api::now_ms(), ev: "req".into(), model, kind, input, w5m, w1h, read, out, ..Default::default() }
    }
}

/// Records a reply through Claude account `acct` (its email) on `plan`.
pub fn record_claude(acct: String, plan: String, kind: String, m: &Meter, util: Util, one_hour_cache: bool, info: Info) {
    let Info { conv, miss, missed, thr, think_dropped, think_mismatched } = info;
    push(util.on(Rec { acct, plan, conv, miss, missed, thr, think_dropped, think_mismatched, ..m.rec(kind, one_hour_cache) }));
}

/// Records a reply through OpenRouter.
pub fn record_openrouter(model: &str, kind: String, m: &Meter) {
    let r = m.rec(kind, false);
    let model = if r.model.is_empty() { model.to_string() } else { r.model.clone() };
    push(Rec { acct: OPENROUTER.into(), model, usd: m.usd, ..r });
}

/// Records a request to the user's own llama.cpp server (free: no cost).
pub fn record_local(model: &str, kind: String, m: &Meter) {
    push(Rec {
        t: crate::api::now_ms(),
        ev: "req".into(),
        acct: LOCAL.into(),
        model: model.to_string(),
        kind,
        input: m.input + m.write,
        read: m.read,
        out: m.out,
        ..Default::default()
    });
}

/// Records an account's polled utilization, when it changed since last seen.
pub fn note_limits(acct: String, plan: String, util: Util) {
    if acct.is_empty() || (util.u5.is_none() && util.u7.is_none()) {
        return;
    }
    let same = with(|l| l.last_util.get(&acct) == Some(&util.whole()));
    if !same {
        push(util.on(Rec { t: crate::api::now_ms(), ev: "limits".into(), acct, plan, ..Default::default() }));
    }
}

/// Records the OpenRouter credits left, when they changed.
pub fn note_credits(left: f64) {
    let same = with(|l| l.last_credits.is_some_and(|c| (c - left).abs() < 0.00005));
    if !same {
        push(Rec { t: crate::api::now_ms(), ev: "credits".into(), acct: OPENROUTER.into(), usd: left, ..Default::default() });
    }
}

// ---------------------------------------------------------------- the log

#[derive(Default)]
struct Log {
    recs: Vec<Rec>,
    remote_url: Option<String>,
    remote: Vec<Rec>,
    view_url: Option<String>,
    visible: Vec<Rec>,
    dirty: bool,
    sync_error: Option<String>,
    /// Each account's last utilization (whole percents), to skip polls
    /// that say nothing new.
    last_util: HashMap<String, (Option<i32>, Option<i32>)>,
    last_credits: Option<f64>,
    /// Goes up with every record, so the UI knows to work things out again.
    version: u64,
}

impl Log {
    fn refresh(&mut self, url: Option<String>) {
        if !self.dirty && self.view_url == url {
            // (Sorted by time: what has expired since is at the front.)
            let cut = keep_from();
            let expired = self.visible.partition_point(|r| r.t < cut);
            if expired > 0 {
                self.visible.drain(..expired);
                self.version += 1;
            }
            return;
        }
        self.view_url = url;
        let remote = if self.remote_url == self.view_url && self.view_url.is_some() { &self.remote[..] } else { &[] };
        self.visible = merge_records(&self.recs, remote);
        self.dirty = false;
        self.version += 1;
    }

    fn discard_before_at(&mut self, file: &Path, cutoff: i64) -> Result<(), String> {
        if self.recs.iter().any(|r| r.t < cutoff) {
            let remaining: Vec<_> = self.recs.iter().filter(|r| r.t >= cutoff).cloned().collect();
            save_records(file, &remaining).map_err(|e| format!("Apply usage reset: {e}"))?;
            self.last_util.clear();
            self.last_credits = None;
            for r in &remaining {
                self.take(r);
            }
            self.recs = remaining;
            self.sync_error = None;
            self.dirty = true;
        }
        let before = self.remote.len();
        self.remote.retain(|r| r.t >= cutoff);
        self.dirty |= before != self.remote.len();
        Ok(())
    }

    /// Adds records to what is shown (`visible`) as they come, rather than
    /// merging every record again: those with an id shown already are
    /// left out, as `merge_records` leaves them.
    fn add_visible(&mut self, recs: &[Rec]) {
        if self.dirty {
            return;
        }
        let cut = keep_from();
        let mut added = false;
        for r in recs.iter().filter(|r| r.t >= cut) {
            added |= insert_sorted(&mut self.visible, r);
        }
        if added {
            self.version += 1;
        }
    }

    /// A record made on this machine.
    fn add_local(&mut self, r: Rec) {
        self.take(&r);
        self.add_visible(std::slice::from_ref(&r));
        self.recs.push(r);
    }

    fn take(&mut self, r: &Rec) {
        if r.u5.is_some() || r.u7.is_some() {
            self.last_util.insert(r.acct.clone(), Util { u5: r.u5, u7: r.u7, ..Default::default() }.whole());
        }
        if r.ev == "credits" {
            self.last_credits = Some(r.usd);
        }
    }
}

static LOG: Mutex<Option<Log>> = Mutex::new(None);

fn path() -> PathBuf {
    #[cfg(test)]
    return std::env::temp_dir().join(format!("oak-test-usage-{}.jsonl", std::process::id()));
    #[cfg(not(test))]
    crate::store::dir().join("usage.jsonl")
}

/// The log as read from disk, less records past `KEEP_DAYS` (the file is
/// written again without them).
fn load() -> Log {
    load_from(&path())
}

fn load_from(file: &Path) -> Log {
    let mut log = Log::default();
    let Ok(text) = std::fs::read_to_string(file) else { return log };
    let cut = keep_from();
    let mut dropped = false;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Rec>(line) {
            Ok(mut r) if r.t >= cut => {
                if r.id.is_empty() {
                    r.id = new_id();
                    dropped = true;
                }
                log.take(&r);
                log.recs.push(r);
            }
            _ => dropped = true,
        }
    }
    log.recs.sort_by_key(|r| r.t);
    if dropped && let Err(e) = save_records(file, &log.recs) {
        log.sync_error = Some(format!("Save usage IDs before syncing: {e}"));
    }
    log.dirty = true;
    log.version = 1;
    log
}

fn with<R>(f: impl FnOnce(&mut Log) -> R) -> R {
    let mut g = LOG.lock().unwrap();
    f(g.get_or_insert_with(load))
}

pub(crate) fn push(mut r: Rec) {
    if r.id.is_empty() {
        r.id = new_id();
    }
    with(|l| {
        let saved = (|| -> std::io::Result<()> {
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path())?;
            writeln!(f, "{}", serde_json::to_string(&r).unwrap_or_default())
        })();
        if let Err(e) = saved {
            l.sync_error = Some(format!("Save local usage before syncing: {e}"));
        }
        l.add_local(r);
    });
}

/// Goes up whenever a record is added.
pub fn version() -> u64 {
    let url = shown_source();
    with(|l| {
        l.refresh(url);
        l.version
    })
}

/// Works with every record, oldest first.
pub fn read<R>(f: impl FnOnce(&[Rec]) -> R) -> R {
    let url = shown_source();
    with(|l| {
        l.refresh(url);
        f(&l.visible)
    })
}

fn save_records(file: &Path, recs: &[Rec]) -> std::io::Result<()> {
    let mut out = Vec::new();
    for r in recs {
        serde_json::to_writer(&mut out, r)?;
        out.push(b'\n');
    }
    crate::store::write_private(file, &out)
}

fn new_id() -> String {
    crate::api::hex(&rand::random::<[u8; 16]>())
}

/// Works with this machine's records (only they are uploaded, never
/// events downloaded from peers), without copying them.
pub(crate) fn local_records<R>(f: impl FnOnce(&mut dyn Iterator<Item = &Rec>) -> R) -> Result<R, String> {
    with(|l| {
        if l.sync_error.is_some() {
            save_records(&path(), &l.recs).map_err(|e| format!("Save local usage before syncing: {e}"))?;
            l.sync_error = None;
        }
        let cut = keep_from();
        Ok(f(&mut l.recs.iter().filter(|r| r.t >= cut)))
    })
}

/// Honor a reset of the shared log on disk as well as in memory. A failed write
/// leaves the old log intact and is retried before sync can claim success.
pub(crate) fn discard_before(cutoff: i64) -> Result<(), String> {
    if cutoff <= 0 { return Ok(()); }
    with(|l| l.discard_before_at(&path(), cutoff))
}

pub(crate) fn set_remote(url: String, recs: Vec<Rec>) {
    with(|l| {
        l.remote_url = Some(url);
        l.remote = recs;
        l.dirty = true;
    });
}

/// Records just downloaded from the source `url`, added to those
/// `set_remote` gave. False when it gave another's (or none).
pub(crate) fn add_remote(url: &str, recs: &[Rec]) -> bool {
    with(|l| {
        if l.remote_url.as_deref() != Some(url) {
            return false;
        }
        for r in recs {
            insert_sorted(&mut l.remote, r);
        }
        if l.view_url.as_deref() == Some(url) {
            l.add_visible(recs);
        }
        true
    })
}

/// Which source's shared records show with this machine's, and what the
/// Usage page says about the sharing: a label, and a line of detail.
static SHARED: Mutex<(Option<String>, String, String)> = Mutex::new((None, String::new(), String::new()));

/// A plugin says which source's shared records to show (None: this
/// machine's only), and what to say about it (`oak_sdk::Host::usage_show`).
pub(crate) fn show_shared(source: Option<&str>, label: &str, detail: &str) {
    *SHARED.lock().unwrap() = (source.map(String::from), label.to_string(), detail.to_string());
}

/// The source whose shared records show.
fn shown_source() -> Option<String> {
    SHARED.lock().unwrap().0.clone()
}

/// Where the shown records are from, and a line more about it.
pub fn status() -> (String, String) {
    let g = SHARED.lock().unwrap();
    if g.1.is_empty() {
        return ("This machine".to_string(), "Usage recorded by Oak on this machine.".to_string());
    }
    (g.1.clone(), g.2.clone())
}

// ---------------------------------------------------------------- what it shows

/// What 1% of a limit bought: the API dollars an account's replies spent
/// while its utilization went up by `step` percent, per percent.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub t: i64,
    pub acct: String,
    pub value: f64,
    pub step: i32,
    /// The model most of those dollars went to.
    pub model: String,
}

/// Every measurement of what 1% of the 5-hour (or `week`ly) limit bought,
/// oldest first. Per account: the utilization a reply's headers carry is
/// from before it, so its own units count towards the next tick. A tick is
/// measured only from a tick seen the same way (where in a percent the
/// count started is unknown otherwise): not across a reset, a rise seen
/// only by polling (use from elsewhere), or an idle gap.
pub fn samples(recs: &[Rec], week: bool) -> Vec<Sample> {
    struct St<'a> {
        pct: i32,
        reset: i64,
        spent: f64,
        valid: bool,
        last: i64,
        plan: &'a str,
        /// Dollars by model (a handful).
        models: Vec<(&'a str, f64)>,
    }
    let mut by: BTreeMap<&str, Vec<&Rec>> = BTreeMap::new();
    for r in recs.iter().filter(|r| r.of_claude_account()) {
        by.entry(r.acct.as_str()).or_default().push(r);
    }
    let mut out = vec![];
    for (acct, mut list) in by {
        list.sort_by_key(|r| r.t);
        let mut st: Option<St> = None;
        for r in list {
            let req = r.ev == "req";
            let (u, reset) = if week { (r.u7, r.r7) } else { (r.u5, r.r5) };
            if let Some(u) = u {
                let p = u.round() as i32;
                let reset = reset.unwrap_or(0);
                let fresh = |valid| St { pct: p, reset, spent: 0.0, valid, last: r.t, plan: &r.plan, models: vec![] };
                match &mut st {
                    None => st = Some(fresh(false)),
                    Some(s) => {
                        let moved = reset != 0 && s.reset != 0 && (reset - s.reset).abs() > 10 * 60_000;
                        let idle = req && r.t - s.last > IDLE_MS;
                        // Another plan has other limits: start over.
                        let replanned = !r.plan.is_empty() && !s.plan.is_empty() && r.plan != s.plan;
                        if moved || p < s.pct || replanned || idle {
                            *s = fresh(false); // a new window, or a gap
                        } else if p > s.pct {
                            if s.valid && req && s.spent > 0.0 {
                                let step = p - s.pct;
                                let model = s.models.iter().max_by(|a, b| a.1.total_cmp(&b.1)).map(|m| m.0.to_string()).unwrap_or_default();
                                out.push(Sample { t: r.t, acct: acct.to_string(), value: s.spent / step as f64, step, model });
                            }
                            *s = fresh(req);
                        }
                    }
                }
            }
            if req && let Some(s) = &mut st {
                let usd = r.usd_total();
                s.spent += usd;
                s.last = r.t;
                match s.models.iter_mut().find(|m| m.0 == r.model) {
                    Some(m) => m.1 += usd,
                    None => s.models.push((&r.model, usd)),
                }
            }
        }
    }
    out.sort_by_key(|s| s.t);
    out
}

/// What 1% bought over each measurement and the `n - 1` before it: the
/// dollars they spent over the percents they covered, together. Pooled
/// like this, a reply counted on the wrong side of a tick (the headers only
/// tell of a tick after it happened) lands in the neighbouring measurement
/// and cancels out, and a 2% step weighs twice as much as a 1% one.
pub fn pooled(samples: &[(f64, i32)], n: usize) -> Vec<f64> {
    (0..samples.len())
        .map(|i| {
            let w = &samples[i.saturating_sub(n - 1)..=i];
            let spent: f64 = w.iter().map(|(v, step)| v * *step as f64).sum();
            spent / w.iter().map(|s| s.1 as f64).sum::<f64>().max(1.0)
        })
        .collect()
}

/// Tokens and dollars in one stretch of time.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Bucket {
    pub start: i64,
    pub input: f64,
    pub w5m: f64,
    pub w1h: f64,
    pub read: f64,
    pub out: f64,
    /// Dollars (API prices for Claude, the bill for OpenRouter) by part:
    /// cache reads, cache writes, input, output.
    pub usd: [f64; 4],
    pub replies: u32,
}

impl Bucket {
    pub fn dollars(&self) -> f64 {
        self.usd.iter().sum()
    }

    pub fn tokens(&self) -> f64 {
        self.input + self.w5m + self.w1h + self.read + self.out
    }

    pub fn add(&mut self, r: &Rec) {
        self.input += r.input as f64;
        self.w5m += r.w5m as f64;
        self.w1h += r.w1h as f64;
        self.read += r.read as f64;
        self.out += r.out as f64;
        for (a, b) in self.usd.iter_mut().zip(r.dollars()) {
            *a += b;
        }
        self.replies += 1;
    }
}

/// The replies `keep` takes, added up into `n` buckets of `step` ms from
/// `from`.
pub fn buckets(recs: &[Rec], from: i64, step: i64, n: usize, keep: impl Fn(&Rec) -> bool) -> Vec<Bucket> {
    let mut out: Vec<Bucket> = (0..n).map(|i| Bucket { start: from + i as i64 * step, ..Default::default() }).collect();
    for r in recs.iter().filter(|r| r.ev == "req" && r.t >= from && keep(r)) {
        let i = ((r.t - from) / step) as usize;
        if let Some(b) = out.get_mut(i) {
            b.add(r);
        }
    }
    out
}

/// Everything `keep` takes between `from` and `to`, added up.
pub fn total(recs: &[Rec], from: i64, to: i64, keep: impl Fn(&Rec) -> bool) -> Bucket {
    let mut b = Bucket { start: from, ..Default::default() };
    for r in recs.iter().filter(|r| r.ev == "req" && r.t >= from && r.t < to && keep(r)) {
        b.add(r);
    }
    b
}

/// An account's utilization of the 5-hour (or `week`ly) limit over time:
/// from its replies' headers and from polling.
pub fn utilization(recs: &[Rec], acct: &str, week: bool, from: i64) -> Vec<(i64, f64)> {
    recs.iter()
        .filter(|r| r.acct == acct && r.t >= from && (r.ev == "req" || r.ev == "limits"))
        .filter_map(|r| Some((r.t, (if week { r.u7 } else { r.u5 })? as f64)))
        .collect()
}

/// The OpenRouter credits left over time.
pub fn credits(recs: &[Rec], from: i64) -> Vec<(i64, f64)> {
    recs.iter().filter(|r| r.ev == "credits" && r.t >= from).map(|r| (r.t, r.usd)).collect()
}

/// The Claude accounts in the log, with their plan as last seen, the most
/// recently used first.
pub fn accounts(recs: &[Rec]) -> Vec<(String, String)> {
    let mut seen: Vec<(String, String)> = vec![];
    for r in recs.iter().rev().filter(|r| r.of_claude_account()) {
        if !seen.iter().any(|s| s.0 == r.acct) {
            seen.push((r.acct.clone(), r.plan.clone()));
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(t: i64, u5: f32, input: u64) -> Rec {
        Rec { t, ev: "req".into(), acct: "a@x".into(), model: "m".into(), input, u5: Some(u5), r5: Some(1_000_000_000), ..Default::default() }
    }

    #[test]
    fn legacy_ids_survive_restart_and_remote_stays_separate() {
        let file = std::env::temp_dir().join(format!("oak-usage-migration-{}.jsonl", new_id()));
        let now = crate::api::now_ms();
        let a = req(now, 10.0, 100);
        std::fs::write(&file, format!("{}\n", serde_json::to_string(&a).unwrap())).unwrap();
        let mut log = load_from(&file);
        let id = log.recs[0].id.clone();
        assert!(!id.is_empty());
        assert_eq!(load_from(&file).recs[0].id, id);
        log.remote_url = Some("server-a".into());
        log.remote = vec![log.recs[0].clone(), Rec { id: "peer".into(), ..req(now + 1, 11.0, 200) }];
        log.refresh(Some("server-a".into()));
        assert_eq!(log.visible.len(), 2); // own downloaded record isn't counted twice
        assert_eq!(log.recs.len(), 1); // peer never enters the upload queue
        let version = log.version;
        log.refresh(Some("server-b".into()));
        assert_eq!(log.visible.len(), 1); // switching servers hides the previous server's cache
        assert!(log.version > version);
        log.refresh(None);
        assert_eq!(log.visible.len(), 1);
        std::fs::remove_file(file).unwrap();
    }

    /// Records added one by one (this machine's, and those downloaded,
    /// its own among them) show as merging them all again would.
    #[test]
    fn records_added_as_they_come_show_as_a_full_merge() {
        let now = crate::api::now_ms();
        let rec = |id: &str, t: i64| Rec { id: id.into(), ..req(t, 10.0, 100) };
        let mut log = Log { recs: vec![rec("a", now - 50), rec("b", now - 30)], remote_url: Some("s".into()), ..Default::default() };
        log.remote = merge_records(&[rec("a", now - 50), rec("c", now - 40)], &[]);
        log.dirty = true;
        log.refresh(Some("s".into()));
        let version = log.version;
        log.add_local(rec("d", now - 10));
        // Its own record back as the vault mended it, a peer's
        // from earlier, one with the same time, and one already there.
        let incoming = [Rec { u5: Some(100.0), ..rec("d", now - 10) }, rec("e", now - 100), rec("f", now - 10), rec("c", now - 40)];
        for r in &incoming {
            insert_sorted(&mut log.remote, r);
        }
        log.add_visible(&incoming);
        assert_eq!(log.visible, merge_records(&log.recs, &log.remote));
        assert!(log.version > version);
        log.refresh(Some("s".into()));
        assert_eq!(log.visible, merge_records(&log.recs, &log.remote));
    }

    #[test]
    fn reset_purges_local_disk_and_poll_deduplication() {
        let file = std::env::temp_dir().join(format!("oak-usage-reset-{}.jsonl", new_id()));
        let now = crate::api::now_ms();
        let old = Rec { id: "old".into(), ..req(now - 1000, 10.0, 100) };
        let fresh = Rec { id: "new".into(), ..req(now + 1, 11.0, 200) };
        save_records(&file, &[old.clone(), fresh.clone()]).unwrap();
        let mut log = load_from(&file);
        log.remote = vec![old, fresh.clone()];
        log.discard_before_at(&file, now).unwrap();
        assert_eq!(log.recs, vec![fresh.clone()]);
        assert_eq!(log.remote, vec![fresh.clone()]);
        assert_eq!(load_from(&file).recs, vec![fresh]);
        log.discard_before_at(&file, now + 2).unwrap();
        assert!(log.last_util.is_empty());
        assert!(load_from(&file).recs.is_empty());
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn prices_and_meter() {
        assert_eq!(model_version("claude-opus-5-5"), ("opus", "5.5".into()));
        assert_eq!(model_version("claude-haiku-5-5-20261001"), ("haiku", "5.5".into()));
        assert_eq!(model_version("claude-opus-4-20250514"), ("opus", "4".into()));
        assert_eq!(model_version("claude-3-5-haiku-20241022"), ("haiku", "3.5".into()));
        assert_eq!(price_of("claude-opus-5-5", 0), OPUS_5_5);
        assert_eq!(price_of("claude-sonnet-5-5", 0).read, 0.10);
        assert_eq!(price_of("claude-opus-4-8", 0).w1h, 10.0);
        assert_eq!(price_of("claude-haiku-5-5", 100_001).input, 0.50);
        assert_eq!(price_of("claude-fable-5-2", 0), price(8.0, 40.0, 0.20));
        assert_eq!(price_of("something-new", 0), OPUS_5_5);
        // A million cache reads and a million output tokens on Opus 5.5.
        let r = Rec { ev: "req".into(), acct: "a@x".into(), model: "claude-opus-5-5".into(), read: 1_000_000, out: 1_000_000, ..Default::default() };
        assert_eq!(r.dollars(), [0.20, 0.0, 0.0, 20.0]);
        let o = Rec { ev: "req".into(), acct: OPENROUTER.into(), usd: 0.03, out: 9, ..Default::default() };
        assert_eq!(o.usd_total(), 0.03);
        use crate::oak::{Event, Usage};
        let mut m = Meter::default();
        m.see(&Event::Start { id: "m".into(), model: "claude-opus-5-5".into() });
        m.see(&Event::Usage(Usage { input: 3, write: 100, write_1h: 100, cache_read: 5000, output: 1, ..Default::default() }));
        m.see(&Event::Text { index: 0, text: "x".into() });
        m.see(&Event::Usage(Usage { output: 250, ..Default::default() }));
        assert!(m.seen);
        assert_eq!((m.input, m.read, m.out, m.model.as_str()), (3, 5000, 250, "claude-opus-5-5"));
        assert_eq!(m.writes(false), (0, 100));
        // No breakdown: the writes go to the lifetime OAK asked for.
        let m = Meter { write: 40, ..Default::default() };
        assert_eq!((m.writes(true), m.writes(false)), ((0, 40), (40, 0)));
    }

    #[test]
    fn percent_ticks() {
        let m = 60_000;
        let recs = vec![
            req(0, 10.0, 500),     // start: where in 10% is unknown
            req(m, 10.0, 500),
            req(2 * m, 11.0, 300), // first tick: a boundary, nothing measured yet
            req(3 * m, 11.0, 400),
            req(4 * m, 11.0, 600),
            req(5 * m, 12.0, 100), // 300 + 400 + 600 bought 1%
            req(6 * m, 12.0, 900),
            req(7 * m, 14.0, 50),  // 100 + 900 bought 2%: 500 each
        ];
        let s = samples(&recs, false);
        // In input tokens: the test model is priced as Opus 5.5, $4 a million.
        let tokens = |v: f64| (v * 1e6 / 4.0).round();
        assert_eq!(s.iter().map(|s| (tokens(s.value), s.step)).collect::<Vec<_>>(), vec![(1300.0, 1), (500.0, 2)]);
        assert_eq!(s[0].t, 5 * m);
        // A reset, an idle gap, or a rise seen only by polling start over.
        let mut more = recs.clone();
        more.push(Rec { t: 8 * m, ev: "limits".into(), acct: "a@x".into(), u5: Some(15.0), r5: Some(1_000_000_000), ..Default::default() });
        more.push(req(9 * m, 15.0, 10));
        more.push(req(10 * m, 16.0, 10)); // after a polled rise: a boundary only
        more.push(req(11 * m, 16.0, 70));
        more.push(req(12 * m, 17.0, 10)); // 10 + 70
        more.push(req(12 * m + IDLE_MS + 1, 18.0, 10)); // idle: not measured
        more.push(req(50 * m, 2.0, 10)); // reset
        more.push(req(51 * m, 3.0, 10)); // a boundary
        more.push(req(52 * m, 3.0, 40));
        more.push(req(53 * m, 4.0, 10)); // 10 + 40
        let s = samples(&more, false);
        assert_eq!(s.iter().map(|s| tokens(s.value)).collect::<Vec<_>>(), vec![1300.0, 500.0, 80.0, 50.0]);
        // The week has no utilization in these: no samples.
        assert!(samples(&more, true).is_empty());
    }

    #[test]
    fn plan_change_starts_over() {
        let m = 60_000;
        let on = |t: i64, u5: f32, plan: &str| Rec { plan: plan.into(), ..req(t, u5, 1_000) };
        let recs = vec![on(0, 10.0, "Pro"), on(m, 11.0, "Pro"), on(2 * m, 12.0, "Pro"), on(3 * m, 12.0, "Max 20x"), on(4 * m, 13.0, "Max 20x"), on(5 * m, 14.0, "Max 20x")];
        // 11→12 on Pro, then nothing across the upgrade, then 13→14 on Max.
        assert_eq!(samples(&recs, false).iter().map(|s| s.t).collect::<Vec<_>>(), vec![2 * m, 5 * m]);
    }

    #[test]
    fn buckets_and_smoothing() {
        let recs = vec![req(0, 1.0, 10), req(999, 1.0, 20), req(1000, 1.0, 5), req(5000, 1.0, 7)];
        let b = buckets(&recs, 0, 1000, 3, |_| true);
        assert_eq!(b.iter().map(|b| b.input).collect::<Vec<_>>(), vec![30.0, 5.0, 0.0]);
        assert_eq!(total(&recs, 0, 10_000, |_| true).replies, 4);
        // $3 for 1%, then $12 for 2% ($6 a percent): $15 over 3%, $5 a percent.
        assert_eq!(pooled(&[(3.0, 1), (6.0, 2), (9.0, 1), (0.0, 1)], 2), vec![3.0, 5.0, 7.0, 4.5]);
        let line = serde_json::to_string(&recs[0]).unwrap();
        assert!(!line.contains("w5m") && !line.contains("usd"), "{line}");
        assert_eq!(serde_json::from_str::<Rec>(&line).unwrap(), recs[0]);
    }
}
