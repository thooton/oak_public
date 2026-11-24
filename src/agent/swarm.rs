//! A session's side of a swarm (/swarm, `crate::swarm`): starting one
//! (`start`, for `Command::Swarm`), the SWARM tool's actions (status,
//! models, add, model, kill, title, post, read, wait), posts reaching the
//! members they name (`post`), killing members (`kill`), and Oak's own
//! stall monitor (`after_turn`), which costs no tokens.

use super::*;
use super::subagents::model_and_effort;
use crate::store::Seat;
use crate::swarm::{self as board, COORDINATOR, MONITOR, Member, Post, Swarm};
use crate::tools::{Head, one_line};

/// Starts the message that gives the coordinator its task.
pub const SWARM_TAG: &str = "<swarm>";

/// How long after a member's turn ends the monitor looks for a stall.
const STALL_DELAY: Duration = Duration::from_secs(5);

/// The most members one SWARM add adds.
const ADD_MAX: u64 = 16;

/// Why conversation `c` can't coordinate a swarm, if it can't.
pub(crate) fn refuses(c: &Conversation) -> Option<String> {
    if c.chat {
        return Some("A chat can't run a swarm".into());
    }
    if c.side_chat_of.is_some() {
        return Some("A side chat can't run a swarm".into());
    }
    if c.parent.is_some() {
        return Some("Only a conversation you started can run a swarm".into());
    }
    if c.code_mode {
        return Some("Code Mode can't run a swarm".into());
    }
    match c.provider() {
        api::Provider::Local => Some("A model on your own server can't run a swarm".into()),
        api::Provider::Freebuff | api::Provider::Plugin => Some("This model runs its own agent: it can't coordinate a swarm".into()),
        _ => None,
    }
}

/// /swarm in conversation `c`: it becomes the coordinator of its swarm
/// (made the first time), and the message giving it `task` is returned.
/// A conversation with nothing sent yet has the SWARM kit from its first
/// request (its instructions in the system prompt); one under way loads it
/// with LOAD, as the message says, so what was sent stays as it was.
pub(super) fn start(c: &mut Conversation, task: &str) -> String {
    let again = c.swarm.is_some();
    if !again {
        c.swarm = Some(Seat { swarm: c.id.clone(), name: COORDINATOR.into(), killed: false });
    }
    let sw = board::get(&c.id);
    {
        let mut s = sw.lock().unwrap();
        if s.member(COORDINATOR).is_none() {
            s.roster.members.insert(0, Member { name: COORDINATOR.into(), conv: c.id.clone(), ..Default::default() });
            s.save();
        }
    }
    let fresh = c.messages.is_empty() && c.prefix.pin.is_none();
    if fresh && !c.prefix.kits.iter().any(|k| k == "SWARM") {
        c.prefix.kits.push("SWARM".into());
    }
    let load = if c.prefix.kits.iter().any(|k| k == "SWARM") {
        ""
    } else {
        " First load the SWARM kit (LOAD name=SWARM): it holds your instructions as coordinator and the SWARM tool."
    };
    let what = if again {
        "The user gave your swarm (/swarm) this task: you coordinate it."
    } else {
        "The user started a swarm with /swarm for this task: you are its coordinator."
    };
    format!(
        "{SWARM_TAG}\n{}\n</swarm>\n\n{what}{load} Split the work into pieces that don't collide, add a worker for each (and verifiers), and report to the user as it goes. Don't do the technical work yourself.",
        task.trim()
    )
}

/// How a session names itself in its prompt: a swarm member by its name
/// on the board, another subagent as `agent_label` does.
pub(super) fn self_label(c: &Conversation) -> String {
    match &c.swarm {
        Some(s) if c.parent.is_some() => format!("@{}", s.name),
        _ => agent_label(c.agent_id, &c.title),
    }
}

/// A board post as the members it names get it.
pub(crate) fn board_message(p: &Post) -> String {
    format!("{AGENT_TAG} from=\"@{}\" type=\"board post\" seq=\"{}\">\n{}\n</agent-message>", p.from, p.seq, p.text.trim())
}

/// Posts `text` to swarm `swarm` as `from`; the members it names (and
/// those asleep for any post) get it at once, which wakes them. Returns
/// the post and who it went to.
pub(crate) fn post(swarm: &str, from: &str, text: &str) -> (Post, Vec<String>) {
    let sw = board::get(swarm);
    let (p, to) = {
        let mut s = sw.lock().unwrap();
        let p = s.post(from, text, api::now_ms());
        let to = s.recipients(&p);
        for m in &to {
            if let Some(x) = s.member_mut(&m.name) {
                x.seen = x.seen.max(p.seq);
                x.any = false;
            }
        }
        s.save();
        (p, to)
    };
    let msg = board_message(&p);
    for m in &to {
        if let Some(s) = find_session(&m.conv) {
            s.deliver(msg.clone());
        }
    }
    (p, to.into_iter().map(|m| m.name).collect())
}

/// What a member is doing, as SWARM status and the Swarm view say.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum MemberState {
    /// In a turn (or about to start one on what came in).
    Working,
    /// Asleep on the board.
    Waiting,
    /// Paused by the user.
    Paused,
    /// Its last turn failed (why): a post naming it, a model switch or a
    /// message from the user starts it again.
    Failed(String),
    /// Killed: gone for good.
    Killed,
    /// Its session isn't running.
    Gone,
}

impl MemberState {
    pub(crate) fn word(&self) -> &'static str {
        match self {
            MemberState::Working => "working",
            MemberState::Waiting => "waiting",
            MemberState::Paused | MemberState::Failed(_) | MemberState::Gone => "stopped",
            MemberState::Killed => "killed",
        }
    }

    /// The word, with why for a stopped member.
    pub(crate) fn describe(&self) -> String {
        match self {
            MemberState::Paused => "stopped (paused by the user)".into(),
            MemberState::Failed(e) => format!("stopped (its last turn failed: {e})"),
            MemberState::Gone => "stopped (not running)".into(),
            s => s.word().into(),
        }
    }
}

/// Member `m`'s state, and the model it is on.
pub(crate) fn state_of(m: &Member) -> (MemberState, String) {
    let Some(s) = find_session(&m.conv) else {
        return (if m.killed { MemberState::Killed } else { MemberState::Gone }, String::new());
    };
    let (model, inbox) = {
        let c = s.conv.lock().unwrap();
        (c.model.clone(), !c.inbox.is_empty())
    };
    if m.killed {
        return (MemberState::Killed, model);
    }
    if s.pause_on() {
        return (MemberState::Paused, model);
    }
    let l = s.live.lock().unwrap();
    let state = if l.busy || inbox || l.ready_count() > 0 {
        MemberState::Working
    } else if let Some(e) = &l.error {
        MemberState::Failed(one_line(e).head(160))
    } else {
        MemberState::Waiting
    };
    (state, model)
}

/// Kills the members of swarm `swarm` named `names` (the roster's names):
/// each stops at once and does nothing more. (SWARM kill, or the Swarm
/// view's Remove.)
pub(crate) fn kill(swarm: &str, names: &[String]) {
    let sw = board::get(swarm);
    let convs: Vec<String> = {
        let mut s = sw.lock().unwrap();
        let mut convs = vec![];
        for n in names {
            if let Some(m) = s.member_mut(n).filter(|m| m.name != COORDINATOR) {
                m.killed = true;
                convs.push(m.conv.clone());
            }
        }
        s.save();
        convs
    };
    for conv in convs {
        if let Some(s) = find_session(&conv) {
            s.kill_member();
        }
    }
}

impl Session {
    /// This swarm member was killed: what it is doing stops without a
    /// report, its background tasks are killed, and it does nothing more
    /// (`Worker::killed`). Its conversation stays, to read.
    fn kill_member(&self) {
        {
            let mut c = self.conv.lock().unwrap();
            if let Some(s) = c.swarm.as_mut() {
                s.killed = true;
            }
            c.inbox.clear();
            c.save();
        }
        self.live.lock().unwrap().quiet_stop = true;
        self.tasks.kill_all();
        self.interrupt();
    }

    /// A swarm member the user removed, or killed by its swarm: messages
    /// to it can't be sent.
    pub fn killed_member(&self) -> bool {
        self.conv.lock().unwrap().swarm.as_ref().is_some_and(|s| s.killed)
    }
}

/// A swarm's member's turn ended (or its coordinator's): Oak's monitor
/// looks for a stall a moment later (`check_stall`).
pub(super) fn after_turn(swarm: &str, ctx: &eframe::egui::Context) {
    let (id, ctx) = (swarm.to_string(), ctx.clone());
    ctx.request_repaint();
    std::thread::spawn(move || {
        std::thread::sleep(STALL_DELAY);
        if check_stall(&id).is_some() {
            ctx.request_repaint();
        }
    });
}

/// One monitor at a time, so that two turns ending together post one stall.
static MONITORING: Mutex<()> = Mutex::new(());

/// Oak's stall monitor: when every member of swarm `id` is waiting (none
/// working or paused by the user), the coordinator hasn't said the job is
/// done (`Swarm::done`), and someone has posted since the last stall
/// post, it posts "@coordinator [stall]" as @monitor, naming who waits
/// and whose turn failed. Returns the post's seq.
pub(crate) fn check_stall(id: &str) -> Option<u64> {
    let _one = MONITORING.lock().unwrap_or_else(|e| e.into_inner());
    let sw = board::get(id);
    let (members, last, stall_at, done) = {
        let s = sw.lock().unwrap();
        (s.roster.members.clone(), s.posts.last().cloned(), s.roster.stall_at, s.done())
    };
    let last = last?;
    if done || last.seq <= stall_at {
        return None;
    }
    let live: Vec<&Member> = members.iter().filter(|m| !m.killed).collect();
    if live.len() < 2 {
        return None;
    }
    let (mut waiting, mut failed) = (vec![], vec![]);
    for m in &live {
        match state_of(m).0 {
            MemberState::Working | MemberState::Paused => return None,
            MemberState::Waiting if m.name != COORDINATOR => waiting.push(format!("@{}", m.name)),
            MemberState::Failed(e) => failed.push(format!("@{} (its last turn failed: {e})", m.name)),
            MemberState::Gone => failed.push(format!("@{} (not running)", m.name)),
            _ => {}
        }
    }
    let mut text = "@coordinator [stall] Every member has gone quiet and the job isn't marked done.".to_string();
    if !waiting.is_empty() {
        text += &format!(" Waiting: {}.", waiting.join(", "));
    }
    if !failed.is_empty() {
        text += &format!(" Stopped: {}.", failed.join(", "));
    }
    text += &format!(
        " Last post: #{} by @{}. Hand out the next step by name, switch the model of a member whose turn failed, or post `[done] <summary>` once the job is finished.",
        last.seq, last.from
    );
    let (p, _) = post(id, MONITOR, &text);
    let mut s = sw.lock().unwrap();
    s.roster.stall_at = p.seq;
    s.save();
    Some(p.seq)
}

impl Worker {
    /// This session's seat in its swarm, unless it has none or was killed.
    fn seat(&self) -> Result<Seat, String> {
        match self.conv.lock().unwrap().swarm.clone() {
            Some(s) if !s.killed => Ok(s),
            Some(_) => Err("You were removed from the swarm.".into()),
            None => Err("You aren't in a swarm: SWARM works once the user starts one with /swarm.".into()),
        }
    }

    /// A killed swarm member: it does nothing more.
    pub(super) fn killed(&self) -> bool {
        self.conv.lock().unwrap().swarm.as_ref().is_some_and(|s| s.killed)
    }

    /// SWARM wait asked to sleep on the board, and nothing came in
    /// meanwhile (a post for it, the user's message): the turn ends.
    pub(super) fn parked(&self) -> bool {
        let park = std::mem::take(&mut self.live.lock().unwrap().park);
        park && self.conv.lock().unwrap().inbox.is_empty() && self.live.lock().unwrap().ready_count() == 0
    }

    /// The SWARM tool's actions.
    pub(super) fn swarm_tool(&self, name: &str, input: &Value) -> Result<String, String> {
        let seat = self.seat()?;
        match name {
            "SWARM_STATUS" => Ok(self.swarm_status(&seat)),
            "SWARM_MODELS" => Ok(crate::tools::models_text()),
            "SWARM_ADD" => self.swarm_add(&seat, input),
            "SWARM_MODEL" => self.swarm_model(&seat, input),
            "SWARM_KILL" => self.swarm_kill(&seat, input),
            "SWARM_TITLE" => swarm_title(&seat, input),
            "SWARM_POST" => swarm_post(&seat, input),
            "SWARM_READ" => Ok(swarm_read(&seat, input)),
            "SWARM_WAIT" => self.swarm_wait(&seat, input),
            _ => Err(format!("Unknown tool {name}")),
        }
    }

    /// SWARM status: the title, the board's head and each member.
    fn swarm_status(&self, seat: &Seat) -> String {
        let sw = board::get(&seat.swarm);
        let (title, members, head) = {
            let s = sw.lock().unwrap();
            (s.roster.title.clone(), s.roster.members.clone(), s.head())
        };
        let seen = members.iter().find(|m| m.name == seat.name).map_or(0, |m| m.seen);
        let title = if title.is_empty() { "untitled".to_string() } else { format!("\"{title}\"") };
        let mut out = format!("Swarm {title}. The board's head is #{head}; posts for you have reached you up to #{seen}.\n");
        for m in &members {
            let (state, model) = state_of(m);
            let you = if m.name == seat.name { " (you)" } else { "" };
            let by = if m.added_by.is_empty() || m.added_by == COORDINATOR { String::new() } else { format!(", added by @{}", m.added_by) };
            let model = if model.is_empty() { String::new() } else { format!(", on {model}") };
            out += &format!("@{}{you}: {}{model}{by}\n", m.name, state.describe());
        }
        out.trim_end().to_string()
    }

    /// SWARM add: one member, or a numbered group (`n` copies, or one per
    /// item of `tasks`), each a subagent of the coordinator on the SWARM
    /// kit, whose brief is posted to the board as the adder (and so is its
    /// first message).
    fn swarm_add(&self, seat: &Seat, input: &Value) -> Result<String, String> {
        let base = board::check_name(input["name"].as_str().unwrap_or(""))?;
        let brief = input["brief"].as_str().unwrap_or("").trim().to_string();
        if brief.is_empty() {
            return Err("The brief is empty: it is everything the member gets.".into());
        }
        let tasks: Vec<String> = input["tasks"].as_array().into_iter().flatten().filter_map(|t| t.as_str()).map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect();
        let n = input["n"].as_u64().unwrap_or(1);
        if !(1..=ADD_MAX).contains(&n) || tasks.len() as u64 > ADD_MAX {
            return Err(format!("Add 1 to {ADD_MAX} members at a time."));
        }
        let members: Vec<(String, String)> = if !tasks.is_empty() {
            tasks.iter().enumerate().map(|(i, t)| (format!("{base}-{}", i + 1), format!("{brief}\n\nYour part: {t}"))).collect()
        } else if n > 1 {
            (1..=n).map(|i| (format!("{base}-{i}"), brief.clone())).collect()
        } else {
            vec![(base, brief)]
        };
        let (model, effort) = model_and_effort(input)?;
        let (my_model, my_effort, my_provider) = {
            let c = self.conv.lock().unwrap();
            (c.model.clone(), c.effort.clone(), c.provider())
        };
        let model = model.unwrap_or_else(|| my_model.clone());
        let provider = if model == my_model { my_provider } else { api::Provider::of(&model) };
        if crate::provided::serves(&model) || matches!(provider, api::Provider::Freebuff | api::Provider::Local | api::Provider::Plugin) {
            return Err(format!("{model} can't run a swarm member: pick another (SWARM action=models)."));
        }
        let effort = effort_for(&model, &effort.unwrap_or(my_effort));
        let coord = find_session(&seat.swarm).ok_or("The coordinator isn't running.")?;
        let sw = board::get(&seat.swarm);
        {
            let s = sw.lock().unwrap();
            if let Some((name, _)) = members.iter().find(|(name, _)| s.member(name).is_some()) {
                return Err(format!("@{name} is taken (a killed member's name can't be used again): pick another name."));
            }
        }
        let mut added = vec![];
        for (name, brief) in members {
            let mut conv = {
                let mut c = coord.conv.lock().unwrap();
                c.next_agent_id += 1;
                let k = c.next_agent_id;
                let conv = Conversation {
                    id: format!("{}-agent{k}", c.id),
                    title: format!("@{name}"),
                    titled: true,
                    folder: c.folder.clone(),
                    provider: Some(provider),
                    model: model.clone(),
                    effort: effort.clone(),
                    // Its system prompt is the coordinator's role's.
                    role: c.role.clone(),
                    last_message_at: api::now_ms(),
                    rolling: rolling::State { off: c.rolling.off, ..Default::default() },
                    parent: Some(c.id.clone()),
                    agent_id: k,
                    swarm: Some(Seat { swarm: seat.swarm.clone(), name: name.clone(), killed: false }),
                    ..Default::default()
                };
                c.agents.push(crate::store::AgentRef { id: k, conv: conv.id.clone(), name: format!("@{name}") });
                c.save();
                conv
            };
            // The kit from its start: the worker's instructions are in its
            // system prompt, and the SWARM tool in its list.
            conv.prefix.kits = vec!["SWARM".into()];
            conv.save();
            let id = conv.id.clone();
            {
                let mut s = sw.lock().unwrap();
                s.roster.members.push(Member { name: name.clone(), conv: id.clone(), added_by: seat.name.clone(), ..Default::default() });
                s.save();
            }
            let s = Session::start(conv, self.ctx.clone());
            SPAWNED.lock().unwrap().push((id, s));
            let (p, _) = post(&seat.swarm, &seat.name, &format!("@{name} {brief}"));
            added.push(format!("@{name} (brief #{})", p.seq));
        }
        self.ctx.request_repaint();
        let who = if seat.name == COORDINATOR { "you".to_string() } else { format!("you, @{}", seat.name) };
        Ok(format!(
            "Added {} on {model} at {effort} effort. Each started on its brief at once; it posts to the board, and a post naming {who} wakes you.",
            added.join(", ")
        ))
    }

    /// SWARM model: another model and/or effort for a member (or this one).
    fn swarm_model(&self, seat: &Seat, input: &Value) -> Result<String, String> {
        let target = input["name"].as_str().map(|n| n.trim().trim_start_matches('@').to_lowercase()).filter(|n| !n.is_empty()).unwrap_or_else(|| seat.name.clone());
        let member = board::get(&seat.swarm).lock().unwrap().member(&target).cloned().ok_or(format!("No member @{target}: SWARM action=status lists them."))?;
        if member.killed {
            return Err(format!("@{target} was killed."));
        }
        if seat.name != COORDINATOR && target != seat.name && member.added_by != seat.name {
            return Err(format!("@{target} isn't yours to switch: ask @coordinator."));
        }
        let (model, effort) = model_and_effort(input)?;
        let my_model = self.conv.lock().unwrap().model.clone();
        let model = match model {
            Some(m) => Some(m),
            None if target == seat.name && effort.is_none() => return Err("Give a `model` (SWARM action=models lists them).".into()),
            None if effort.is_some() => None,
            None => Some(my_model),
        };
        if let Some(m) = &model
            && (crate::provided::serves(m) || matches!(api::Provider::of(m), api::Provider::Freebuff | api::Provider::Local))
        {
            return Err(format!("{m} can't run a swarm member: pick another (SWARM action=models)."));
        }
        let s = find_session(&member.conv).ok_or(format!("@{target} isn't running."))?;
        let now = s.conv.lock().unwrap().model.clone();
        if let Some(m) = model.as_ref().filter(|m| **m != now) {
            s.command(Command::Model(api::Provider::of(m), m.clone()));
        }
        let model = model.unwrap_or(now);
        if let Some(e) = &effort {
            s.command(Command::Effort(effort_for(&model, e)));
        }
        // A member whose turn failed (out of quota) takes it up again.
        let failed = {
            let l = s.live.lock().unwrap();
            !l.busy && l.error.is_some()
        };
        if failed && !s.pause_on() {
            s.resume_next();
        }
        let again = if failed { " It takes up its failed turn again." } else { "" };
        Ok(format!("@{target} goes on {model} from its next request; its conversation carries on.{again}"))
    }

    /// SWARM kill: a member (or a numbered group) for good.
    fn swarm_kill(&self, seat: &Seat, input: &Value) -> Result<String, String> {
        let target = input["name"].as_str().unwrap_or("").trim().trim_start_matches('@').to_lowercase();
        if target.is_empty() {
            return Err("Give the member's `name`.".into());
        }
        let named: Vec<Member> = {
            let s = board::get(&seat.swarm);
            let s = s.lock().unwrap();
            s.named(&target).into_iter().filter(|m| !m.killed).cloned().collect()
        };
        if named.is_empty() {
            return Err(format!("No member @{target} to kill: SWARM action=status lists them."));
        }
        for m in &named {
            if m.name == COORDINATOR {
                return Err("The coordinator can't be killed.".into());
            }
            if m.name == seat.name {
                return Err("You can't kill yourself: post your result and end your turn.".into());
            }
            if seat.name != COORDINATOR && m.added_by != seat.name {
                return Err(format!("@{} isn't yours to kill: ask @coordinator.", m.name));
            }
        }
        let names: Vec<String> = named.iter().map(|m| m.name.clone()).collect();
        kill(&seat.swarm, &names);
        let list: Vec<String> = names.iter().map(|n| format!("@{n}")).collect();
        Ok(format!("Killed {}, for good (the names can't be used again). Their helpers, if any, carry on.", list.join(", ")))
    }

    /// SWARM wait: the posts after `since` it would wake for, if any;
    /// else it sleeps on the board (the turn ends once this reply's calls
    /// are done: `parked`).
    fn swarm_wait(&self, seat: &Seat, input: &Value) -> Result<String, String> {
        let mentions_only = input["mentions_only"].as_bool().unwrap_or(true);
        if !self.conv.lock().unwrap().inbox.is_empty() || self.live.lock().unwrap().ready_count() > 0 {
            return Ok("Not waiting: something came in meanwhile, and it follows this result.".into());
        }
        let sw = board::get(&seat.swarm);
        let mut s = sw.lock().unwrap();
        let seen = s.member(&seat.name).map_or(0, |m| m.seen);
        let since = input["since"].as_u64().unwrap_or(seen);
        let missed: Vec<Post> = s.after(since, 50).iter().filter(|p| p.from != seat.name && (!mentions_only || Swarm::names_member(p, &seat.name))).cloned().collect();
        if let Some(last) = missed.last() {
            if let Some(m) = s.member_mut(&seat.name) {
                m.seen = m.seen.max(last.seq);
            }
            s.save();
            let lines: Vec<String> = missed.iter().map(board::post_line).collect();
            return Ok(format!("Not waiting: these came after #{since}:\n{}", lines.join("\n")));
        }
        if let Some(m) = s.member_mut(&seat.name) {
            m.any = !mentions_only;
        }
        s.save();
        drop(s);
        self.set(|l| l.park = true);
        Ok(if mentions_only {
            format!("Asleep on the board: a post naming @{} (or @all) wakes you. Your turn ends here.", seat.name)
        } else {
            "Asleep on the board: any post wakes you. Your turn ends here.".to_string()
        })
    }
}

/// SWARM title: the coordinator names the swarm.
fn swarm_title(seat: &Seat, input: &Value) -> Result<String, String> {
    if seat.name != COORDINATOR {
        return Err("The title is the coordinator's: ask @coordinator.".into());
    }
    let title = one_line(input["title"].as_str().unwrap_or("")).head(80);
    if title.is_empty() {
        return Err("The title is empty.".into());
    }
    let sw = board::get(&seat.swarm);
    let mut s = sw.lock().unwrap();
    s.roster.title = title.clone();
    s.save();
    Ok(format!("The swarm is \"{title}\"."))
}

/// SWARM post.
fn swarm_post(seat: &Seat, input: &Value) -> Result<String, String> {
    let text = input["text"].as_str().unwrap_or("").trim();
    if text.is_empty() {
        return Err("The text is empty.".into());
    }
    let (p, woke) = post(&seat.swarm, &seat.name, text);
    Ok(if woke.is_empty() {
        format!("Posted #{}. It names no member, so it wakes nobody: members read it when something else wakes them.", p.seq)
    } else {
        let woke: Vec<String> = woke.iter().map(|n| format!("@{n}")).collect();
        format!("Posted #{}; it went to {}.", p.seq, woke.join(", "))
    })
}

/// SWARM read: the posts after `since`, or the latest.
fn swarm_read(seat: &Seat, input: &Value) -> String {
    let sw = board::get(&seat.swarm);
    let mut s = sw.lock().unwrap();
    let since = input["since"].as_u64();
    let head = s.head();
    let posts: Vec<Post> = match since {
        Some(n) => s.after(n, 100).to_vec(),
        None => s.recent(20).to_vec(),
    };
    let Some(last) = posts.last().map(|p| p.seq) else {
        return match since {
            Some(n) => format!("No posts after #{n}. The board's head is #{head}."),
            None => "The board is empty.".into(),
        };
    };
    if let Some(m) = s.member_mut(&seat.name) {
        m.seen = m.seen.max(last);
    }
    s.save();
    let mut out: Vec<String> = posts.iter().map(board::post_line).collect();
    if last < head {
        out.push(format!("({} more after #{last}: read on with since={last}.)", head - last));
    } else if since.is_none() && posts.first().is_some_and(|p| p.seq > 1) {
        out.insert(0, format!("(The latest {} of {head} posts; since=0 reads from the start.)", posts.len()));
    }
    out.join("\n")
}
