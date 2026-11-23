//! One Quick turn: the socket, the user's message, and Quick's frames read
//! until the message ends. Quick asks for tools by name; Oak runs each one
//! here, in the conversation's working folder, and answers on the socket, so
//! Quick works on the real project rather than on a copy in its sandbox.

use super::*;
use crate::quick::socket::{PollFrame, Socket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// How long one turn may run before Oak gives up on it.
const TURN_LIMIT: Duration = Duration::from_secs(3_600);

type Threads = std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, usize>>>;

/// The threads whose turns this Oak is reading, with how many. Quick hands
/// this machine's tool calls to every socket Oak has open to it, whatever
/// thread asked (a call names its thread only in its arguments), so each
/// turn runs and shows only its own thread's calls (`owner`).
static LIVE: Threads = std::sync::LazyLock::new(Default::default);

/// The threads whose conversation's own agent is reading the turn's reply,
/// with how many, so the Oak calls a snippet makes go to it to be run
/// (`run_oak_alive`): while a `Runner` for the thread lives.
static RUNNERS: Threads = std::sync::LazyLock::new(Default::default);

/// The Oak calls a turn handed its conversation's agent and waits for, by
/// call id: what the agent returns for one goes there (`ran`).
static WAITERS: std::sync::LazyLock<
    Mutex<std::collections::HashMap<String, std::sync::mpsc::Sender<Value>>>,
> = std::sync::LazyLock::new(Default::default);

/// The conversation's own agent reads `thread`'s turn, and runs the Oak
/// calls of Quick's snippets in it, while this lives.
pub(crate) struct Runner(String);

impl Runner {
    pub(crate) fn new(thread: &str) -> Runner {
        *RUNNERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(thread.to_string())
            .or_default() += 1;
        Runner(thread.to_string())
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let mut r = RUNNERS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = r.get_mut(&self.0) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                r.remove(&self.0);
            }
        }
    }
}

fn has_runner(thread: &str) -> bool {
    RUNNERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(thread)
}

/// Oak's agent ran a call (`r`, its `tool_result` block): a turn waiting for
/// it (`run_oak_alive`) gets it. Every call any conversation runs comes
/// here; only those a turn waits for are looked at.
pub(crate) fn ran(r: &Value) {
    let Some(id) = r["call_id"].as_str() else {
        return;
    };
    let waiter = WAITERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id);
    if let Some(tx) = waiter {
        let _ = tx.send(r.clone());
    }
}

/// Waits, as a turn does, for what Oak's agent returns for call `id`.
#[cfg(test)]
pub(crate) fn wait_for_result(id: &str) -> std::sync::mpsc::Receiver<Value> {
    let (tx, rx) = std::sync::mpsc::channel();
    WAITERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_string(), tx);
    rx
}

/// Calls of threads no turn here is reading: the first turn to see one runs
/// it, once.
static CLAIMED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

/// Assistant `message_id`s already seen on each thread, so a new request after
/// an interrupt or pause ignores any trailing frames of an older message.
static SEEN_MESSAGES: std::sync::LazyLock<
    Mutex<std::collections::HashMap<String, std::collections::HashSet<String>>>,
> = std::sync::LazyLock::new(Default::default);

fn note_seen_message(thread: &str, msg: &str) {
    if !thread.is_empty() && !msg.is_empty() {
        SEEN_MESSAGES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(thread.to_string())
            .or_default()
            .insert(msg.to_string());
    }
}

fn seen_messages(thread: &str) -> std::collections::HashSet<String> {
    SEEN_MESSAGES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(thread)
        .cloned()
        .unwrap_or_default()
}

/// Why a running Quick turn is being stopped early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopKind {
    Interrupt,
    Pause,
}

/// Control state shared between Oak's worker and the background thread running
/// [`stream`] for a conversation's Quick thread.
#[derive(Default)]
pub(crate) struct TurnCtl {
    abort: Mutex<Option<StopKind>>,
    pause_queued: AtomicBool,
    done: (Mutex<bool>, Condvar),
}

impl TurnCtl {
    pub(crate) fn aborted(&self) -> Option<StopKind> {
        *self.abort.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn pause_requested(&self) -> bool {
        self.pause_queued.load(Ordering::SeqCst)
    }

    pub(crate) fn set_abort(&self, kind: StopKind) {
        let mut a = self.abort.lock().unwrap_or_else(|e| e.into_inner());
        if a.is_none() || kind == StopKind::Interrupt {
            *a = Some(kind);
        }
    }

    pub(crate) fn wait_done(&self, timeout: Duration) {
        let (lock, cvar) = &self.done;
        let mut done = lock.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = std::time::Instant::now() + timeout;
        while !*done {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let (next, _) = cvar
                .wait_timeout(done, left)
                .unwrap_or_else(|e| e.into_inner());
            done = next;
        }
    }
}

static CTLS: std::sync::LazyLock<Mutex<std::collections::HashMap<String, Arc<TurnCtl>>>> =
    std::sync::LazyLock::new(Default::default);

thread_local! {
    static CURRENT_CTL: std::cell::RefCell<Option<Arc<TurnCtl>>> = const { std::cell::RefCell::new(None) };
}

fn current_abort() -> Option<StopKind> {
    CURRENT_CTL.with(|c| c.borrow().as_ref().and_then(|ctl| ctl.aborted()))
}

struct CtlGuard {
    thread: String,
    ctl: Arc<TurnCtl>,
}

impl CtlGuard {
    fn register(thread: &str) -> (Arc<TurnCtl>, CtlGuard) {
        let prev = CTLS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(thread)
            .cloned();
        if let Some(old) = prev {
            old.set_abort(StopKind::Interrupt);
            old.wait_done(Duration::from_secs(3));
        }
        let ctl = Arc::new(TurnCtl::default());
        CTLS.lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(thread.to_string(), ctl.clone());
        (
            ctl.clone(),
            CtlGuard {
                thread: thread.to_string(),
                ctl,
            },
        )
    }
}

impl Drop for CtlGuard {
    fn drop(&mut self) {
        let (lock, cvar) = &self.ctl.done;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cvar.notify_all();
        let mut all = CTLS.lock().unwrap_or_else(|e| e.into_inner());
        if all
            .get(&self.thread)
            .is_some_and(|c| Arc::ptr_eq(c, &self.ctl))
        {
            all.remove(&self.thread);
        }
    }
}

/// Tells the active Quick turn on `thread` whether a normal pause
/// (`Session::request_pause`) is waiting to take hold.
pub(crate) fn set_pause(thread: &str, on: bool) {
    if let Some(ctl) = CTLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(thread)
        .cloned()
    {
        ctl.pause_queued.store(on, Ordering::SeqCst);
    }
}

/// Immediately aborts the active (or parked) Quick turn on `thread`: kills any
/// running tool child process, cancels pending steers, and sends
/// `stop_generation` on the socket.
pub(crate) fn abort(thread: &str, kind: StopKind) {
    if let Some(ctl) = CTLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(thread)
        .cloned()
    {
        ctl.set_abort(kind);
    }
    drop_parked(thread, kind == StopKind::Pause);
}

/// Waits briefly for the active Quick turn on `thread` to finish cleaning up.
pub(crate) fn wait_stopped(thread: &str) {
    if let Some(ctl) = CTLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(thread)
        .cloned()
    {
        ctl.wait_done(Duration::from_secs(3));
    }
}

/// Closes any parked turn on `thread`, telling Quick to stop generating on it.
pub(crate) fn drop_parked(thread: &str, pause: bool) {
    if let Some(mut p) = PARKED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(thread)
    {
        stop_traffic(&mut p.sock, thread, &p.turn.message, pause);
    }
}

/// Messages the user typed while a Quick turn of theirs ran, by thread. Oak
/// hands each to the turn (`steer`), the turn sends it on its socket as
/// Quick's app does (`steer_message`: `mid_turn`), and Quick says when its
/// agent took it in (`steering_ingested`, naming its `message_id`).
static STEERS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, Vec<Steer>>>,
> = std::sync::LazyLock::new(Default::default);

struct Steer {
    /// Oak's queued message (`agent::Queued::id`); 0 for one it didn't queue.
    id: u64,
    text: String,
    /// The `message_id` it goes with.
    message: String,
    sent: bool,
    taken: bool,
    canceled: bool,
}

/// Hands `text`, the user's queued message `id`, to the Quick turn running
/// on `thread`: it goes on the socket with the turn's next frame.
pub(crate) fn steer(thread: &str, id: u64, text: &str) {
    let message = format!("msg-{}", &hex_uuid().replace('-', "")[..8]);
    let s = Steer {
        id,
        text: text.to_string(),
        message,
        sent: false,
        taken: false,
        canceled: false,
    };
    STEERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(thread.to_string())
        .or_default()
        .push(s);
}

/// Cancels a queued message `id` that was handed to Quick's running turn
/// before Quick ingested it (when the user removes or edits it in the queue).
/// True if it wasn't taken in yet.
pub(crate) fn cancel_steer(id: u64) -> bool {
    let mut all = STEERS.lock().unwrap_or_else(|e| e.into_inner());
    for v in all.values_mut() {
        if let Some(pos) = v.iter().position(|s| s.id == id) {
            if v[pos].taken {
                return false;
            }
            if !v[pos].sent {
                v.remove(pos);
            } else {
                v[pos].canceled = true;
            }
            return true;
        }
    }
    true
}

/// The queued messages Quick's agent took in on `thread` in the request that
/// just ended. The thread's steers are done with: one not taken in goes with
/// the next request as a message of its own.
pub(crate) fn take_ingested(thread: &str) -> Vec<u64> {
    let steers = STEERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(thread)
        .unwrap_or_default();
    steers
        .into_iter()
        .filter(|s| s.taken && !s.canceled)
        .map(|s| s.id)
        .collect()
}

/// Sends the messages handed to the turn on `thread` that haven't gone yet,
/// and cancels any the user removed or held for editing meanwhile.
fn send_steers(
    sock: &mut Socket,
    thread: &str,
    say: &dyn Fn(&str, &str) -> Value,
) -> Result<(), String> {
    let (due, canceled): (Vec<(String, String)>, Vec<String>) = {
        let mut all = STEERS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(v) = all.get_mut(thread) else {
            return Ok(());
        };
        let mut due = vec![];
        let mut canceled = vec![];
        v.retain_mut(|s| {
            if s.canceled {
                if s.sent && !s.taken {
                    canceled.push(s.message.clone());
                }
                return false;
            }
            if !s.sent {
                s.sent = true;
                due.push((s.text.clone(), s.message.clone()));
            }
            true
        });
        (due, canceled)
    };
    if !canceled.is_empty() {
        sock.send(&json!({"type": "steering_cancel", "payload": {"thread_id": thread, "message_ids": canceled}}))?;
    }
    for (text, message) in due {
        if std::env::var("OAK_QUICK_DEBUG").is_ok() {
            eprintln!(
                "STEER sending {message} mid-turn: {}",
                text.chars().take(80).collect::<String>()
            );
        }
        sock.send(&say(&text, &message))?;
    }
    Ok(())
}

/// Quick's agent took in messages (`steering_ingested`, payload `p`): marks
/// the turn's among them, by message id (or text). True when any were.
fn took_in(thread: &str, p: &Value) -> bool {
    let ids: Vec<&str> = p["message_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let said: Vec<&str> = p["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["content"].as_str())
        .map(str::trim)
        .collect();
    let mut all = STEERS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(v) = all.get_mut(thread) else {
        return false;
    };
    let mut any = false;
    for s in v.iter_mut().filter(|s| s.sent && !s.taken && !s.canceled) {
        let want = s.text.trim();
        if ids.contains(&s.message.as_str()) || said.iter().any(|m| *m == want || m.ends_with(want))
        {
            s.taken = true;
            any = true;
        }
    }
    any
}

/// Cancels any un-ingested steers sent on `sock`, sends `stop_generation` to
/// Quick, and waits briefly for `stream_ended` so Quick's backend has stopped
/// the turn before the next request opens on `thread`.
fn stop_traffic(sock: &mut Socket, thread: &str, message: &str, pause: bool) {
    let sent_steers: Vec<String> = {
        let mut all = STEERS.lock().unwrap_or_else(|e| e.into_inner());
        let mut ids = vec![];
        if let Some(v) = all.get_mut(thread) {
            for s in v.iter_mut() {
                if s.sent && !s.taken {
                    ids.push(s.message.clone());
                }
                s.taken = false;
            }
        }
        ids
    };
    if !sent_steers.is_empty() {
        let _ = sock.send(&json!({
            "type": "steering_cancel",
            "payload": {"thread_id": thread, "message_ids": sent_steers}
        }));
    }
    let mut payload = json!({"thread_id": thread});
    if !message.is_empty() {
        payload["message_id"] = json!(message);
        note_seen_message(thread, message);
    }
    if pause {
        payload["explicit_pause"] = json!(true);
    }
    if sock
        .send(&json!({"type": "stop_generation", "payload": payload}))
        .is_ok()
    {
        let until = std::time::Instant::now() + Duration::from_millis(350);
        for f in sock.wait_for_stop(until) {
            if let Some(m) = f["message_id"]
                .as_str()
                .or(f["payload"]["message_id"].as_str())
            {
                note_seen_message(thread, m);
            }
        }
    }
}

/// Turns left open for Oak's next request, by thread: Quick took in a
/// message the user typed (`steering_ingested`), so the reply Oak was reading
/// ended there, and Quick's agent carries on in the same message, which the
/// next request (the message in the transcript, after the results) reads on
/// the same socket.
static PARKED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Parked>>> =
    std::sync::LazyLock::new(Default::default);

/// How long a parked turn waits for Oak's next request. Oak asks again at
/// once; one that doesn't come (an interrupt, a pause) leaves it to close.
const PARK_LIMIT: Duration = Duration::from_secs(60);

struct Parked {
    sock: Socket,
    turn: Turn,
    calls: Calls,
    resends: usize,
    live: Live,
    folder: std::path::PathBuf,
    outpost: Option<String>,
    at: std::time::Instant,
}

/// The turn parked on `thread`, if it is fresh.
fn take_parked(thread: &str) -> Option<Parked> {
    let p = PARKED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(thread)?;
    (p.at.elapsed() < PARK_LIMIT).then_some(p)
}

/// A turn reading its thread, for as long as it lives.
pub(crate) struct Live(String);

impl Live {
    pub(crate) fn new(thread: &str) -> Live {
        *LIVE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(thread.to_string())
            .or_default() += 1;
        Live(thread.to_string())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = live.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                live.remove(&self.0);
            }
        }
    }
}

/// Whose a tool call is to run and show.
pub(crate) enum Owner {
    /// This turn's thread's (or a call that names no thread).
    Mine,
    /// Another turn's here, which runs it and shows it.
    Theirs,
    /// A thread's no turn here is reading: this turn runs it, once, and
    /// shows it nowhere.
    Orphan,
}

/// The thread a tool call is for, as Quick names it in the call.
pub(crate) fn call_thread(p: &Value) -> &str {
    let q = &p["arguments"]["params"];
    q["_quickwork"]["offload_session_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| q["arguments"]["thread_id"].as_str())
        .unwrap_or("")
}

/// Who runs call `p` (its frame's payload) and shows it, for a turn of
/// `thread`.
pub(crate) fn owner(p: &Value, thread: &str) -> Owner {
    let of = call_thread(p);
    if of.is_empty() || of == thread {
        return Owner::Mine;
    }
    if LIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(of)
    {
        return Owner::Theirs;
    }
    let id = p["tool_call_id"].as_str().unwrap_or("").to_string();
    if CLAIMED.lock().unwrap_or_else(|e| e.into_inner()).insert(id) {
        Owner::Orphan
    } else {
        Owner::Theirs
    }
}

/// Streams one Oak request through Quick's agent, sending its events to
/// `tx`. Quick keeps the thread's history itself, so only the newest user
/// turn goes; the tools it asks for are run in `body.folder`.
pub fn stream(body: Request, tx: Sender<Sse>) {
    let id = body.model.clone();
    let Some(m) = model(&id) else {
        return drop(tx.send(Sse::Err(format!("Quick doesn't serve {id}."))));
    };
    let folder = body
        .folder
        .clone()
        .filter(|f| !f.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    // The thread is the conversation's, so Quick keeps its history there.
    let thread = body
        .conv
        .clone()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(hex_uuid);
    let (ctl, _ctl_guard) = CtlGuard::register(&thread);
    // Quick took in a message typed meanwhile and carries on: this request
    // reads on where the last one stopped, on the same socket. Anything said
    // since that Quick hasn't had goes to it mid-turn too.
    if let Some(p) = take_parked(&thread) {
        if let Some(text) = new_user_text(&body) {
            steer(&thread, 0, &text);
        }
        let Parked {
            sock,
            turn,
            calls,
            resends,
            live,
            folder,
            outpost,
            ..
        } = p;
        return drive(
            Drive {
                sock,
                turn,
                calls,
                resends,
                live,
                folder,
                outpost,
                thread,
                m,
                body,
                ctl,
            },
            tx,
        );
    }
    // This thread's calls are this turn's; another thread's are its turn's.
    let live = Live::new(&thread);
    let token = match login::access_token() {
        Ok(t) => t,
        Err(e) => return drop(tx.send(Sse::Err(e))),
    };
    let host = match login::host() {
        Ok(h) => h,
        Err(e) => return drop(tx.send(Sse::Err(e))),
    };
    // This client is this machine's outpost, and it says so where the app
    // does: in the socket's own query, not only in the frames (Quick marks an
    // outpost ONLINE for the socket that names it, which is what makes it
    // eligible to run tools).
    let outpost = outpost();
    let mut path = format!(
        "/streaming/quickwork-chat-bidirectional-stream?surfaceType={SURFACE}&token={}&workingSessionId={thread}&channel=prod",
        urlencode(&token)
    );
    if let Some(o) = outpost.as_deref() {
        path = format!(
            "/streaming/quickwork-chat-bidirectional-stream?surfaceType={SURFACE}&token={}&outpostId={o}&workingSessionId={thread}&channel=prod",
            urlencode(&token)
        );
    }
    // Quick numbers its frames and may send them out of order, twice or not
    // at all; a socket that sets up its delivery (`replayContext`, as the app
    // does) can ask for the missing ones again, and the socket puts them in
    // order (`socket::Replay`). `OAK_QUICK_REPLAY=off` leaves it out.
    if std::env::var("OAK_QUICK_REPLAY").as_deref() != Ok("off") {
        path = path.replace(
            "&channel=prod",
            &format!(
                "&replayContext={}&channel=prod",
                urlencode(&crate::quick::socket::replay_setup())
            ),
        );
    }
    let mut sock = match Socket::connect(&host, &path) {
        Ok(s) => s,
        Err(e) => return drop(tx.send(Sse::Err(e))),
    };
    let base = json!({
        "surfaceType": SURFACE,
        "appVersion": APP_VERSION,
        "acp_first_text": 1,
        "user_notification": 1,
        "subtask_progress": 1,
        "external_agent_sign_in": 1,
        "recents_promotion": 1,
    });
    // This machine registers itself as Quick's outpost, so Quick routes the
    // tools it asks for here rather than running them in its own sandbox.
    // (Failure is not fatal: a turn still works, without local files.)
    let _ = crate::quick::outpost::register();
    // Quick routes a folder's tools to the outpost that registered it, so the
    // conversation's folder is registered against this machine.
    if let (Ok(tok), Ok(h)) = (
        crate::quick::login::access_token(),
        crate::quick::login::host(),
    ) && let Some(f) = folder.to_str()
    {
        let _ = crate::quick::outpost::register_folder(&tok, &h, f);
    }
    let mut connect = base.clone();
    connect["actor_id"] = json!("");
    if let Some(o) = outpost.as_deref() {
        connect["outpostId"] = json!(o);
    }
    if sock
        .send(&json!({"type": "session_connect", "payload": connect}))
        .is_err()
    {
        return drop(tx.send(Sse::Err("Quick closed the socket while connecting.".into())));
    }
    // The app connects to its home session first, federates its sign-in,
    // then joins the thread: Quick binds the machine's outpost to the session
    // that way, and without it the agent is told it has no local device.
    if sock
        .send(&json!({"type": "federate_status", "payload": {}}))
        .is_err()
    {
        return drop(tx.send(Sse::Err("Quick closed the socket while federating.".into())));
    }
    let mut home = base.clone();
    home["session_id"] = json!("home");
    home["focus_only"] = json!(true);
    if sock
        .send(&json!({"type": "session_connect", "payload": home}))
        .is_err()
    {
        return drop(tx.send(Sse::Err(
            "Quick closed the socket while connecting home.".into(),
        )));
    }
    // This connection's workstreams are none (the app sends this before it
    // opens a thread), which is how Quick learns this socket carries none of
    // its own work and may be the courier for the outpost's tools.
    if sock
        .send(&json!({"type": "workstreams_summary", "payload": {"workstream_ids": []}}))
        .is_err()
    {
        return drop(tx.send(Sse::Err(
            "Quick closed the socket while summarising workstreams.".into(),
        )));
    }
    let mut focus = base.clone();
    focus["session_id"] = json!(thread);
    focus["focus_only"] = json!(true);
    // Quick routes a tool call to the machine that owns an outpost; without
    // one it runs the tool in its own sandbox, which has none of the user's
    // files. The outpost is the one this machine registered (quick.json's
    // `outpost`, or OAK_QUICK_OUTPOST for a stand-in).
    if let Some(o) = outpost.as_deref() {
        focus["outpostId"] = json!(o);
    }
    if sock
        .send(&json!({"type": "session_connect", "payload": focus}))
        .is_err()
    {
        return drop(tx.send(Sse::Err(
            "Quick closed the socket while joining the thread.".into(),
        )));
    }
    // The thread's dock, as the app tells Quick it is: no files open, the
    // sidebar shut. Quick reads a thread with a courier only from a client
    // that says where it is in the thread.
    let dock = json!({
        "type": "dock_state_sync",
        "payload": {"session_id": thread, "files": [], "focused_file": "", "sidebar_open": false, "surfaceType": SURFACE}
    });
    if sock.send(&dock).is_err() {
        return drop(tx.send(Sse::Err(
            "Quick closed the socket while syncing the dock.".into(),
        )));
    }
    // The socket naming the outpost is what marks it ONLINE, and only an
    // ONLINE outpost is handed this turn's tools; so the turn waits for Quick
    // to say so before it asks anything.
    if let Some(o) = outpost.as_deref()
        && ctl.aborted().is_none()
    {
        crate::quick::outpost::await_online(&token, &host, o);
    }
    for f in sock.drain_pending() {
        if let Some(mid) = f["message_id"]
            .as_str()
            .or(f["payload"]["message_id"].as_str())
        {
            note_seen_message(&thread, mid);
        }
    }
    if ctl.aborted().is_some() || ctl.pause_requested() {
        return drop(tx.send(Sse::Done));
    }
    let mut user = user_message(&body, &thread, m, outpost.as_deref());
    // Quick's message takes text only: the images the user attached go to
    // the thread's workspace, and the message says where.
    let attached = attached_images(&body, &thread);
    if !attached.is_empty() {
        let said = user["payload"]["text"].as_str().unwrap_or("").to_string();
        user["payload"]["text"] = json!(format!("{said}\n\n{}", attached.join("\n")));
    }
    if let Err(e) = sock.send(&user) {
        return drop(tx.send(Sse::Err(e)));
    }
    let mut turn = Turn::for_message(&thread, "");
    turn.ignore_messages = seen_messages(&thread);
    drive(
        Drive {
            sock,
            turn,
            calls: Calls::default(),
            resends: 0,
            live,
            folder,
            outpost,
            thread,
            m,
            body,
            ctl,
        },
        tx,
    )
}

/// A turn Oak reads: the socket, what it has read, and what it needs to
/// speak to Quick on it.
struct Drive {
    sock: Socket,
    turn: Turn,
    calls: Calls,
    resends: usize,
    live: Live,
    folder: std::path::PathBuf,
    outpost: Option<String>,
    thread: String,
    m: &'static Model,
    body: Request,
    ctl: Arc<TurnCtl>,
}

/// Reads the turn to its end, or until Quick takes in a message the user
/// typed (then the turn is parked for the next request, `PARKED`).
fn drive(d: Drive, tx: Sender<Sse>) {
    let Drive {
        mut sock,
        mut turn,
        mut calls,
        mut resends,
        live,
        folder,
        outpost,
        thread,
        m,
        body,
        ctl,
    } = d;
    let say = |text: &str, id: &str| steer_message(&body, text, id, &thread, m, outpost.as_deref());
    turn.oak_tools = oak_tools(&body);
    loop {
        match pump(
            &mut sock,
            &folder,
            &mut turn,
            &mut calls,
            resends < RESEND_LIMIT,
            &say,
            &tx,
            &ctl,
        ) {
            // The stream says the turn is over: the reply is whole.
            Ok(Pumped::Done) => return drop(tx.send(Sse::Done)),
            // Quick took in a message the user typed: this reply ends here,
            // and the next request reads on.
            Ok(Pumped::Steered) => {
                if let Some(kind) = ctl.aborted() {
                    stop_traffic(
                        &mut sock,
                        &turn.thread,
                        &turn.message,
                        kind == StopKind::Pause,
                    );
                    return drop(tx.send(Sse::Done));
                }
                if ctl.pause_requested() {
                    stop_traffic(&mut sock, &turn.thread, &turn.message, true);
                    return drop(tx.send(Sse::Done));
                }
                turn.resume();
                let p = Parked {
                    sock,
                    turn,
                    calls,
                    resends,
                    live,
                    folder,
                    outpost,
                    at: std::time::Instant::now(),
                };
                PARKED
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(thread.clone(), p);
                return drop(tx.send(Sse::Done));
            }
            // Quick's relay recorded its own error for a call Oak had
            // answered, so Quick's agent read the error, not the result: Oak
            // gives it the real results, and its answer is this turn's too.
            Ok(Pumped::Lost(lost)) => {
                resends += 1;
                eprintln!(
                    "Quick's relay replaced {} tool result(s) with its own error; Oak is giving Quick the real ones",
                    lost.len()
                );
                for ev in turn.await_resend() {
                    if tx.send(Sse::Event(ev)).is_err() {
                        return;
                    }
                }
                let mut again = body.clone();
                again.messages = vec![
                    json!({"role": "user", "content": [{"type": "text", "text": resend_text(&lost)}]}),
                ];
                again.instruction = None;
                let user = user_message(&again, &thread, m, outpost.as_deref());
                if let Err(e) = sock.send(&user) {
                    return drop(tx.send(Sse::Err(e)));
                }
            }
            Err(e) => return drop(tx.send(Sse::Err(e))),
        }
    }
}

/// How many times one turn gives Quick results its relay lost.
const RESEND_LIMIT: usize = 2;

/// How a read of Quick's frames ended.
pub(crate) enum Pumped {
    /// The message ended: the reply is whole.
    Done,
    /// The message ended on results Oak gave that Quick's relay replaced
    /// with its own error, so Quick's agent read the error instead.
    Lost(Vec<Lost>),
    /// Quick's agent took in a message the user typed meanwhile: the reply
    /// so far is whole, and Quick's message goes on.
    Steered,
}

fn finish_stop(
    sock: &mut Socket,
    turn: &mut Turn,
    tx: &Sender<Sse>,
    reason: &str,
    pause: bool,
) -> Result<Pumped, String> {
    stop_traffic(sock, &turn.thread, &turn.message, pause);
    if !turn.blocks().is_empty() {
        for ev in turn.close(reason) {
            let _ = tx.send(Sse::Event(ev));
        }
    }
    Ok(Pumped::Done)
}

/// Reads Quick's frames until the message ends, running the tools it asks
/// for and answering each on the socket. A message that ends on a result the
/// relay lost comes back as `Pumped::Lost` when `may_resend` (Oak then gives
/// Quick the real results); otherwise it ends the turn as it is.
/// `say` makes the frame for a message the user typed meanwhile, which goes
/// once the turn has begun.
#[allow(clippy::too_many_arguments)]
fn pump(
    sock: &mut Socket,
    folder: &std::path::Path,
    turn: &mut Turn,
    calls: &mut Calls,
    may_resend: bool,
    say: &dyn Fn(&str, &str) -> Value,
    tx: &Sender<Sse>,
    ctl: &Arc<TurnCtl>,
) -> Result<Pumped, String> {
    let started = std::time::Instant::now();
    loop {
        if started.elapsed() > TURN_LIMIT {
            return Err("Quick's turn ran too long; it was given up on.".into());
        }
        if let Some(kind) = ctl.aborted() {
            let reason = match kind {
                StopKind::Interrupt => crate::agent::CUT_SHORT,
                StopKind::Pause => stop::TOOL_CALLS,
            };
            return finish_stop(sock, turn, tx, reason, kind == StopKind::Pause);
        }
        if ctl.pause_requested() {
            return finish_stop(sock, turn, tx, stop::TOOL_CALLS, true);
        }
        if turn.started {
            send_steers(sock, &turn.thread, say)?;
        }
        let frame = match sock.poll_next(Duration::from_millis(100)) {
            PollFrame::Frame(f) => f,
            PollFrame::Idle => continue,
            PollFrame::Closed => {
                // The socket closed: a reply that ended is done; one that
                // didn't is an error.
                if !turn.started && ctl.aborted().is_none() && !ctl.pause_requested() {
                    return Err("Quick closed the connection before the reply began.".into());
                }
                return Ok(Pumped::Done);
            }
        };
        // Messages typed meanwhile go between Quick's frames: Quick takes
        // them in at its agent's next step, so that is soon enough.
        if turn.started && ctl.aborted().is_none() && !ctl.pause_requested() {
            send_steers(sock, &turn.thread, say)?;
        }
        let kind = frame["type"].as_str().unwrap_or("").to_string();
        // Another thread's frames, and older messages' of this one, are not
        // this turn's (the socket carries several threads).
        let mine = turn.accepts(&frame);
        if mine && !turn.message.is_empty() {
            note_seen_message(&turn.thread, &turn.message);
        }
        if std::env::var("OAK_QUICK_DEBUG").is_ok() {
            eprintln!(
                "FRAME kind={kind:22} thread={:28} msg={:12} accepted={mine}",
                frame["thread_id"].as_str().unwrap_or("-"),
                frame["message_id"].as_str().unwrap_or("-")
            );
            if std::env::var("OAK_QUICK_DEBUG").as_deref() == Ok("2") {
                eprintln!(
                    "   {}",
                    frame.to_string().chars().take(600).collect::<String>()
                );
            }
        }
        if !mine {
            continue;
        }
        // Quick's own record of a call's result. When its relay fails to
        // reach the device it records its own error for the call (`SNCS
        // outpost invoke error (500)`), and that can be what its agent reads
        // even though Oak answered.
        if kind == "tool_result" {
            calls.saw_server_result(&frame["payload"]);
        }
        if kind == "message_complete" && may_resend && !calls.lost.is_empty() {
            let lost = std::mem::take(&mut calls.lost);
            calls.resent.extend(lost.iter().map(|l| l.call.clone()));
            return Ok(Pumped::Lost(lost));
        }
        let call = frame["payload"]["tool_call_id"]
            .as_str()
            .unwrap_or("")
            .to_string();
        // A call asked for again is in the transcript already.
        let repeat = kind == "tool_call" && calls.knows(&call);
        // Quick hands every socket here each of this machine's calls: another
        // thread's is that thread's turn's to run and show, and one of a
        // thread no turn reads is run once and shown nowhere.
        let foreign = if kind != "tool_call" || call.is_empty() {
            false
        } else if repeat {
            calls.is_foreign(&call)
        } else {
            match owner(&frame["payload"], &turn.thread) {
                Owner::Mine => false,
                Owner::Theirs => continue,
                Owner::Orphan => true,
            }
        };
        if kind == "tool_call" && !repeat && !foreign {
            if let Some(ab) = ctl.aborted() {
                let reason = match ab {
                    StopKind::Interrupt => crate::agent::CUT_SHORT,
                    StopKind::Pause => stop::TOOL_CALLS,
                };
                return finish_stop(sock, turn, tx, reason, ab == StopKind::Pause);
            }
            if ctl.pause_requested() {
                return finish_stop(sock, turn, tx, stop::TOOL_CALLS, true);
            }
        }
        if !repeat && !foreign {
            for ev in turn.feed(&frame) {
                if tx.send(Sse::Event(ev)).is_err() {
                    stop_traffic(sock, &turn.thread, &turn.message, false);
                    return Ok(Pumped::Done);
                }
            }
        }
        match kind.as_str() {
            "tool_call" => {
                if call.is_empty() {
                    continue;
                }
                let p = &frame["payload"];
                let name = p["tool_name"].as_str().unwrap_or("").to_string();
                let args = if p["arguments"]["params"]["arguments"].is_null() {
                    p["arguments"].clone()
                } else {
                    p["arguments"]["params"]["arguments"].clone()
                };
                let key = format!("{name}{args}");
                // Quick asks again for a call Oak has answered when its relay
                // retries the delivery: the same answer goes again, and
                // nothing is run twice. Left unanswered, the relay gives up
                // on the call and records its own error for it.
                let mut finished = vec![];
                let reply = match calls.reply_for(&call, &key) {
                    Some(reply) => reply,
                    None => {
                        let (result, shown) =
                            run_call(sock, ctl, tx, turn, &call, &name, &args, folder, foreign)?;
                        if std::env::var("OAK_QUICK_DEBUG").is_ok() {
                            eprintln!(
                                "TOOL {name} {args} -> {}",
                                result
                                    .as_deref()
                                    .map(|s| s.chars().take(300).collect::<String>())
                                    .unwrap_or_default()
                            );
                            if let Err(e) = &result {
                                eprintln!("TOOL {name} failed: {e}");
                            }
                        }
                        // The call carries a token for a transport its result
                        // may be offloaded on. Quick's app hands it to its
                        // outpost (`offload_transport_token`) and never sends
                        // it back with the result, so Oak doesn't either.
                        let transport =
                            p["arguments"]["params"]["_quickwork"]["result_transport_token"]
                                .as_str()
                                .unwrap_or("");
                        let reply = tool_reply(&call, &result, transport);
                        calls.record(&call, &name, &key, &result, reply.clone());
                        if foreign {
                            // Another thread's call: answered, not shown here.
                            calls.mark_foreign(&call);
                        } else {
                            // The call's block ends with what it returned here
                            // (a snippet's Oak calls show their own results).
                            finished = turn.finished_call(&call, shown.as_ref().unwrap_or(&result));
                        }
                        reply
                    }
                };
                // A probe switch: Quick's own bookkeeping for the call may not
                // be ready the instant its frame is written, and a result that
                // arrives first would be dropped.
                if let Ok(ms) = std::env::var("OAK_QUICK_TOOL_DELAY_MS")
                    && let Ok(n) = ms.parse::<u64>()
                {
                    std::thread::sleep(Duration::from_millis(n));
                }
                let _ = sock.send(&reply);
                for ev in finished {
                    if tx.send(Sse::Event(ev)).is_err() {
                        stop_traffic(sock, &turn.thread, &turn.message, false);
                        return Ok(Pumped::Done);
                    }
                }
                if let Some(ab) = ctl.aborted() {
                    let reason = match ab {
                        StopKind::Interrupt => crate::agent::CUT_SHORT,
                        StopKind::Pause => stop::TOOL_CALLS,
                    };
                    return finish_stop(sock, turn, tx, reason, ab == StopKind::Pause);
                }
                if ctl.pause_requested() {
                    return finish_stop(sock, turn, tx, stop::TOOL_CALLS, true);
                }
                send_steers(sock, &turn.thread, say)?;
                // Quick carries the turn on in a new message once it has the
                // result; that message is this turn's too.
                if !foreign {
                    turn.awaiting_continuation = true;
                }
            }
            "steering_ingested" => {
                let p = &frame["payload"];
                if std::env::var("OAK_QUICK_DEBUG").is_ok() {
                    eprintln!(
                        "STEER Quick took in {} (fallback {})",
                        p["message_ids"], p["fallback"]
                    );
                }
                if took_in(&turn.thread, p) {
                    for ev in turn.close(stop::STEERED) {
                        if tx.send(Sse::Event(ev)).is_err() {
                            return Ok(Pumped::Done);
                        }
                    }
                    return Ok(Pumped::Steered);
                }
            }
            "message_complete" => {
                // Quick's agent is done (it has run every turn it wanted):
                // Oak's own loop stops here.
                return Ok(Pumped::Done);
            }
            "error" => {
                let why = frame["payload"]["message"]
                    .as_str()
                    .or(frame["payload"]["error"].as_str())
                    .unwrap_or("Quick reported an error")
                    .to_string();
                return Err(format!("Quick: {why}"));
            }
            _ => {}
        }
    }
}

/// A snippet's code, under whichever name its call gives it.
fn snippet_code(args: &Value) -> &str {
    args["code"]
        .as_str()
        .or(args["script"].as_str())
        .or(args["source"].as_str())
        .unwrap_or("")
}

/// What a tool returned, or why it failed.
type ToolResult = Result<String, String>;

/// Runs one of Quick's calls: a snippet's Oak calls through the
/// conversation's own agent while it reads this turn (`run_oak_alive`), and
/// any other call here (`run_tool_alive`; a snippet too, when no agent reads
/// the turn: `run_oak_direct`). With it, what the transcript shows as the
/// call's result when that isn't the result itself (the Oak calls are
/// blocks of their own, with their own results).
#[allow(clippy::too_many_arguments)]
fn run_call(
    sock: &mut Socket,
    ctl: &Arc<TurnCtl>,
    tx: &Sender<Sse>,
    turn: &mut Turn,
    call: &str,
    name: &str,
    args: &Value,
    folder: &std::path::Path,
    foreign: bool,
) -> Result<(ToolResult, Option<ToolResult>), String> {
    if is_snippet(name) && !foreign && has_runner(&turn.thread) {
        let oak = match crate::quick::calls::snippet(snippet_code(args), &turn.oak_tools) {
            Ok(oak) => oak,
            Err(e) => return Ok((Err(e), None)),
        };
        let result = run_oak_alive(sock, ctl, tx, turn, call, &oak)?;
        let names: Vec<&str> = oak.iter().map(|c| c.name.as_str()).collect();
        let shown = format!(
            "Ran {} through Oak ({} below).",
            names.join(", "),
            if oak.len() == 1 { "its result is" } else { "their results are" }
        );
        return Ok((result, Some(Ok(shown))));
    }
    Ok((run_tool_alive(sock, ctl, name, args, folder)?, None))
}

/// Runs a snippet's Oak calls (`oak`) through the conversation's own agent,
/// which runs them as it runs any model's calls: each is a block of the
/// reply (`Turn::oak_call`), so Oak shows, runs and tracks it as its own,
/// all at once (those on a file one writes in order). The turn waits for
/// their results (`ran`), keeping the socket read meanwhile, and Quick gets
/// them all in one answer (`calls::answer`).
fn run_oak_alive(
    sock: &mut Socket,
    ctl: &Arc<TurnCtl>,
    tx: &Sender<Sse>,
    turn: &mut Turn,
    call: &str,
    oak: &[crate::quick::calls::OakCall],
) -> Result<Result<String, String>, String> {
    let (rtx, rrx) = std::sync::mpsc::channel::<Value>();
    // Ids any provider takes, should the conversation move to another one.
    let base: String = call
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let ids: Vec<String> = (0..oak.len()).map(|k| format!("{base}_oak{k}")).collect();
    {
        let mut w = WAITERS.lock().unwrap_or_else(|e| e.into_inner());
        for id in &ids {
            w.insert(id.clone(), rtx.clone());
        }
    }
    drop(rtx);
    let forget = || {
        let mut w = WAITERS.lock().unwrap_or_else(|e| e.into_inner());
        for id in &ids {
            w.remove(id);
        }
    };
    for (id, c) in ids.iter().zip(oak) {
        for ev in turn.oak_call(id, &c.name, &c.input) {
            if tx.send(Sse::Event(ev)).is_err() {
                forget();
                return Ok(Err("Oak stopped reading the turn before the calls ran.".into()));
            }
        }
    }
    let mut got: std::collections::HashMap<String, Value> = Default::default();
    let mut aborted_at: Option<std::time::Instant> = None;
    let mut why = "the turn ended";
    while got.len() < ids.len() {
        match rrx.recv_timeout(Duration::from_millis(20)) {
            Ok(r) => {
                if let Some(id) = r["call_id"].as_str() {
                    got.insert(id.to_string(), r.clone());
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(kind) = ctl.aborted() {
                    let since = *aborted_at.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed() >= Duration::from_millis(300) {
                        why = match kind {
                            StopKind::Interrupt => "the user interrupted the turn",
                            StopKind::Pause => "the user paused the session (ultrapause)",
                        };
                        break;
                    }
                    let _ = sock.keep_alive(Duration::from_millis(20));
                    continue;
                }
                // The agent stopped reading the turn: nothing more will run.
                if !has_runner(&turn.thread) {
                    why = "Oak's agent stopped reading the turn";
                    break;
                }
                if !sock.keep_alive(Duration::from_millis(100)) {
                    forget();
                    return Err("Quick closed the socket while Oak's calls ran.".into());
                }
            }
        }
    }
    forget();
    let thread = turn.thread.clone();
    let results: Vec<Result<String, String>> = ids
        .iter()
        .map(|id| match got.get(id) {
            Some(r) => crate::quick::calls::result_text(&images_delivered(r, &thread)),
            None => Err(format!("It didn't finish: {why}.")),
        })
        .collect();
    Ok(crate::quick::calls::answer(oak, &results))
}

/// An image block's bytes and media type.
fn image_bytes(b: &Value) -> Option<(Vec<u8>, String)> {
    let data = crate::blobs::data_of(b)?;
    let bytes =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data.as_bytes()).ok()?;
    let media = b["media_type"].as_str().unwrap_or("image/png").to_string();
    Some((bytes, media))
}

/// The name an image goes to Quick's workspace by: its file's, or one made
/// for it (the `k`th of a message).
fn image_name(b: &Value, media: &str, k: usize) -> String {
    if let Some(n) = b["_oak_file"].as_str().filter(|n| !n.trim().is_empty()) {
        return n.to_string();
    }
    let ext = match media {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    };
    format!("oak-image-{}-{k}.{ext}", crate::api::now_ms())
}

/// Where a file Oak put in Quick's workspace is, as Quick's agent opens it:
/// the path its file tools take (its sandbox's own, `/outposts/backend/…`),
/// and the workspace's.
fn workspace_place(delivered: &Value) -> String {
    format!(
        "/outposts/backend{} (`{}`)",
        delivered["path"].as_str().unwrap_or(""),
        delivered["workspace_path"].as_str().unwrap_or("")
    )
}

/// A call's `tool_result` block with its images put in thread `thread`'s
/// workspace (`outpost::upload`), each replaced by a note of where it is:
/// Quick's agent can't be handed an image in a result, but it can open one
/// in its workspace with its image tool.
fn images_delivered(r: &Value, thread: &str) -> Value {
    let Some(blocks) = r["content"].as_array() else {
        return r.clone();
    };
    if !blocks.iter().any(|b| b["type"] == "image") {
        return r.clone();
    }
    let mut k = 0;
    let content: Vec<Value> = blocks
        .iter()
        .map(|b| {
            if b["type"] != "image" {
                return b.clone();
            }
            k += 1;
            let Some((bytes, media)) = image_bytes(b) else {
                return b.clone();
            };
            let name = image_name(b, &media, k);
            let text = match crate::quick::outpost::upload(&bytes, &name, &media, thread) {
                Ok(v) => format!(
                    "[An image ({name}): Oak shows it to the user, and put it in your workspace at {}. To see it, open that path with your image-reading tool (file_read_image).]",
                    workspace_place(&v)
                ),
                Err(e) => format!("[An image ({name}): Oak shows it to the user, but couldn't put it in your workspace: {e}]"),
            };
            json!({"type": "text", "text": text})
        })
        .collect();
    let mut out = r.clone();
    out["content"] = Value::Array(content);
    out
}

/// The images the user attached since Quick last replied (the user turns
/// after the newest assistant message, as `new_user_text` reads them), each
/// put in thread `thread`'s workspace: a line for each, saying where, for the
/// message that goes to Quick. None for a request with an instruction of its
/// own (a summary).
fn attached_images(r: &Request, thread: &str) -> Vec<String> {
    if r.instruction.as_deref().is_some_and(|i| !i.trim().is_empty()) {
        return vec![];
    }
    let mut turns: Vec<Vec<Value>> = vec![];
    for m in r.messages.iter().rev() {
        if m["role"] == "assistant" {
            break;
        }
        if m["role"] != "user" {
            continue;
        }
        let images: Vec<Value> = m["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "image")
            .cloned()
            .collect();
        turns.push(images);
    }
    turns.reverse();
    let mut lines = vec![];
    for (k, b) in turns.into_iter().flatten().enumerate() {
        let Some((bytes, media)) = image_bytes(&b) else {
            continue;
        };
        let name = image_name(&b, &media, k + 1);
        lines.push(match crate::quick::outpost::upload(&bytes, &name, &media, thread) {
            Ok(v) => format!(
                "[The user attached an image ({name}). Oak put it in your workspace at {}: open that path with your image-reading tool (file_read_image) to see it.]",
                workspace_place(&v)
            ),
            Err(e) => format!("[The user attached an image ({name}), but Oak couldn't put it in your workspace: {e}]"),
        });
    }
    lines
}

/// A snippet's Oak calls run here, for a turn no conversation's agent reads
/// (another thread's call, a test): those Oak runs without a session (READ,
/// WRITE, EDIT, EXECUTE, SEARCH and FETCH), all at once, each file's in
/// order.
fn run_oak_direct(args: &Value, folder: &std::path::Path) -> Result<String, String> {
    let tools = crate::tools::definitions_with_kits(false, false, false, &[]);
    let oak = crate::quick::calls::snippet(snippet_code(args), &tools)?;
    let ctl = CURRENT_CTL.with(|c| c.borrow().clone());
    // The calls on one file go one after another, in order; the rest at once.
    let mut groups: Vec<(Option<String>, Vec<usize>)> = vec![];
    for (k, c) in oak.iter().enumerate() {
        let path = c.input["path"]
            .as_str()
            .map(|p| resolve(folder, p).to_string_lossy().to_lowercase());
        match groups
            .iter_mut()
            .find(|g| path.is_some() && g.0 == path)
        {
            Some(g) => g.1.push(k),
            None => groups.push((path, vec![k])),
        }
    }
    let results: Vec<Mutex<Option<Result<String, String>>>> =
        (0..oak.len()).map(|_| Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for (_, ks) in &groups {
            let (ctl, oak, results) = (ctl.clone(), &oak, &results);
            s.spawn(move || {
                CURRENT_CTL.with(|cell| *cell.borrow_mut() = ctl);
                for &k in ks {
                    let c = &oak[k];
                    let r = match c.name.as_str() {
                        "READ" | "WRITE" | "EDIT" | "SEARCH" | "FETCH" | "EXECUTE" => {
                            run_tool(&c.name, &c.input, folder)
                        }
                        other => Err(format!(
                            "{other} runs only in the conversation's own turn in Oak; here READ, WRITE, EDIT, EXECUTE, SEARCH and FETCH run."
                        )),
                    };
                    *results[k].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                }
            });
        }
    });
    let results: Vec<Result<String, String>> = results
        .into_iter()
        .map(|m| {
            m.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .unwrap_or_else(|| Err("It didn't run.".into()))
        })
        .collect();
    crate::quick::calls::answer(&oak, &results)
}

/// Runs a tool call on a thread of its own and keeps the socket read while
/// it runs. A socket nobody reads for the length of a long tool (a build, a
/// snippet near its limit) stops answering Quick: its relay gives up on the
/// call, and then the socket closes under the turn, which ended the chat.
/// What comes meanwhile is held, in order, for the turn to read after (a call
/// the relay asks for again among it is answered from `Calls`).
fn run_tool_alive(
    sock: &mut Socket,
    ctl: &Arc<TurnCtl>,
    name: &str,
    args: &Value,
    folder: &std::path::Path,
) -> Result<Result<String, String>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let (n, a, f, c) = (
        name.to_string(),
        args.clone(),
        folder.to_path_buf(),
        ctl.clone(),
    );
    std::thread::Builder::new()
        .name(format!("quick-tool-{name}"))
        .spawn(move || {
            CURRENT_CTL.with(|cell| *cell.borrow_mut() = Some(c));
            let res = run_tool(&n, &a, &f);
            CURRENT_CTL.with(|cell| *cell.borrow_mut() = None);
            let _ = tx.send(res);
        })
        .map_err(|e| format!("Couldn't start {name}: {e}"))?;
    let mut aborted_at: Option<std::time::Instant> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(result) => return Ok(result),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Ok(Err(format!("{name} stopped without an answer.")));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(kind) = ctl.aborted() {
                    let since = *aborted_at.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed() >= Duration::from_millis(300) {
                        let why = match kind {
                            StopKind::Interrupt => "the user interrupted the turn",
                            StopKind::Pause => "the user paused the session (ultrapause)",
                        };
                        return Ok(Err(format!("{name} was stopped because {why}.")));
                    }
                    let _ = sock.keep_alive(Duration::from_millis(20));
                    continue;
                }
                if !sock.keep_alive(Duration::from_millis(100)) {
                    return Err(format!("Quick closed the socket while {name} ran."));
                }
            }
        }
    }
}

/// The `tool_result` frame for a call, shaped as Quick's app sends it: the
/// result is always an object (its outpost answers in JSON, and a bare
/// string is a result Quick's relay can't read), and `error` stays empty. A
/// tool that ran and failed is a result (`success: false`); Quick's app sets
/// `error` only when its outpost couldn't be reached at all, and Quick reports
/// that as the relay failing. `OAK_QUICK_TOOL_RESULT=string` sends the bare
/// text and `=token` sends the transport token back, as Oak once did (probe
/// switches).
pub(crate) fn tool_reply(call: &str, result: &Result<String, String>, transport: &str) -> Value {
    let mode = std::env::var("OAK_QUICK_TOOL_RESULT").unwrap_or_default();
    let body = match result {
        Ok(s) if mode == "string" => json!(s),
        Ok(s) => match serde_json::from_str::<Value>(s) {
            Ok(v @ Value::Object(_)) => v,
            _ => json!({"success": true, "output": s}),
        },
        Err(e) => json!({"success": false, "error": e}),
    };
    let mut payload = json!({"tool_call_id": call, "result": body, "error": Value::Null});
    if mode == "token" && !transport.is_empty() {
        payload["result_transport_token"] = json!(transport);
    }
    json!({"type": "tool_result", "payload": payload})
}

/// The calls Oak answered this turn, and what Quick's relay made of them.
#[derive(Default)]
pub(crate) struct Calls {
    answered: Vec<Answer>,
    /// Answers Quick's relay replaced with its own error, not given back yet.
    lost: Vec<Lost>,
    /// The calls given back already, so each is given back once.
    resent: Vec<String>,
}

struct Answer {
    call: String,
    name: String,
    /// The call's name and input, so a repeat is known as the same call.
    key: String,
    /// The frame Oak answered with.
    reply: Value,
    /// What the tool returned (or why it failed).
    content: String,
    ok: bool,
    /// Quick has recorded a result for it.
    settled: bool,
    /// Another thread's call, answered for it and shown nowhere.
    foreign: bool,
}

/// A result Oak gave that Quick's relay replaced with its own error.
pub(crate) struct Lost {
    pub name: String,
    pub call: String,
    /// What the tool returned on this machine.
    pub content: String,
    /// What the relay said instead.
    pub relay: String,
}

impl Calls {
    fn knows(&self, call: &str) -> bool {
        !call.is_empty() && self.answered.iter().any(|a| a.call == call)
    }

    fn is_foreign(&self, call: &str) -> bool {
        self.answered.iter().any(|a| a.call == call && a.foreign)
    }

    fn mark_foreign(&mut self, call: &str) {
        for a in self.answered.iter_mut().filter(|a| a.call == call) {
            a.foreign = true;
        }
    }

    /// The answer Oak gave this call, when it is the same call asked again.
    fn reply_for(&self, call: &str, key: &str) -> Option<Value> {
        self.answered
            .iter()
            .find(|a| a.call == call && a.key == key)
            .map(|a| a.reply.clone())
    }

    fn record(
        &mut self,
        call: &str,
        name: &str,
        key: &str,
        result: &Result<String, String>,
        reply: Value,
    ) {
        let (content, ok) = match result {
            Ok(s) => (s.clone(), true),
            Err(e) => (e.clone(), false),
        };
        self.answered.push(Answer {
            call: call.into(),
            name: name.into(),
            key: key.into(),
            reply,
            content,
            ok,
            settled: false,
            foreign: false,
        });
    }

    /// Quick's record of a call's result (a `tool_result` frame). It names
    /// the call by Quick's own id (`tool_id`), which needn't be the id the
    /// call came with, so a result is matched by id when it can be, and an
    /// error that says the relay failed goes with the newest call of its
    /// name. A call Oak answered that Quick records as failed is lost.
    fn saw_server_result(&mut self, p: &Value) {
        let id = ["tool_id", "tool_call_id", "tool_use_id", "id"]
            .iter()
            .find_map(|k| p[*k].as_str())
            .unwrap_or("");
        let name = p["tool_name"].as_str().or(p["name"].as_str()).unwrap_or("");
        let same_name = |a: &Answer| !a.foreign && (name.is_empty() || a.name == name);
        let error = server_error(p);
        let exact = self
            .answered
            .iter()
            .position(|a| !id.is_empty() && a.call == id);
        let relay = error.as_deref().is_some_and(looks_like_relay);
        let at = if exact.is_some() {
            exact
        } else if relay {
            self.answered
                .iter()
                .rposition(|a| !a.settled && same_name(a))
                .or_else(|| self.answered.iter().rposition(|a| same_name(a)))
        } else if !name.is_empty() {
            self.answered
                .iter()
                .position(|a| !a.settled && a.name == name)
        } else {
            None
        };
        let Some(i) = at else { return };
        self.answered[i].settled = true;
        let Some(said) = error else { return };
        // An error about another call (Quick's own tools fail too) isn't one
        // of Oak's results lost.
        if exact.is_none() && !relay {
            return;
        }
        let a = &self.answered[i];
        if !a.ok || self.resent.contains(&a.call) || self.lost.iter().any(|l| l.call == a.call) {
            return;
        }
        self.lost.push(Lost {
            name: a.name.clone(),
            call: a.call.clone(),
            content: a.content.clone(),
            relay: said,
        });
    }
}

/// The error a `tool_result` of Quick's records, if it records one.
fn server_error(p: &Value) -> Option<String> {
    let said = match &p["error"] {
        Value::Null | Value::Bool(false) => None,
        Value::String(s) if s.trim().is_empty() => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    };
    said.or_else(|| {
        let failed = p["status"].as_str() == Some("error") || p["is_error"].as_bool() == Some(true);
        failed.then(|| {
            p["result"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| p["result"].to_string())
        })
    })
}

/// Whether an error is Quick's relay failing to reach the device (`SNCS
/// outpost invoke error (500)`), rather than a tool's own failure.
fn looks_like_relay(said: &str) -> bool {
    let s = said.to_lowercase();
    ["outpost", "relay", "invoke", "courier", "device"]
        .iter()
        .any(|w| s.contains(w))
}

/// The follow-up that gives Quick's agent the results its relay lost.
fn resend_text(lost: &[Lost]) -> String {
    let mut s = String::from(
        "(From Oak, the device that ran your tools.) Quick's relay replaced the result of the tool call below with its own error, but the call did run on this machine and succeeded. Its real result follows: use it to finish what I asked, without running the call again.\n",
    );
    for l in lost {
        let content: String = l.content.chars().take(40_000).collect();
        let relay: String = l.relay.chars().take(300).collect();
        s.push_str(&format!(
            "\n{} (call {}); the relay said: {relay}\nResult:\n{content}\n",
            l.name, l.call
        ));
    }
    s
}

/// `ls`, as a Unix shell runs it, for Windows, whose `cmd` has none: Quick's
/// agent lists folders that way. None when the command isn't a plain `ls`
/// (a pipe or a redirect goes to the shell as it is).
pub(crate) fn unix_ls(command: &str, folder: &std::path::Path) -> Option<Result<String, String>> {
    let mut words = command.split_whitespace();
    if words.next()? != "ls" || command.contains(['|', '&', '>', '<', ';', '`', '$']) {
        return None;
    }
    let (mut all, mut long) = (false, false);
    let mut paths = vec![];
    for w in words {
        match w.strip_prefix('-') {
            Some(flags) if !flags.starts_with('-') => {
                all |= flags.contains('a') || flags.contains('A');
                long |= flags.contains('l');
            }
            Some(_) => {}
            None => paths.push(w.trim_matches(|c: char| c == '"' || c == '\'').to_string()),
        }
    }
    if paths.is_empty() {
        paths.push(String::new());
    }
    let line = |name: &str, m: &std::fs::Metadata| {
        if long {
            format!(
                "{} {:>12} {name}\n",
                if m.is_dir() { "d" } else { "-" },
                m.len()
            )
        } else {
            format!("{name}\n")
        }
    };
    let mut out = String::new();
    for (n, want) in paths.iter().enumerate() {
        let dir = resolve(folder, want);
        let meta = match std::fs::metadata(&dir) {
            Ok(m) => m,
            Err(e) => return Some(Err(format!("ls: cannot access '{want}': {e}"))),
        };
        if paths.len() > 1 {
            if n > 0 {
                out.push('\n');
            }
            out.push_str(&format!("{want}:\n"));
        }
        if !meta.is_dir() {
            out.push_str(&line(want, &meta));
            continue;
        }
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => return Some(Err(format!("ls: cannot open directory '{want}': {e}"))),
        };
        let mut entries: Vec<(String, std::fs::Metadata)> = read
            .flatten()
            .filter_map(|e| {
                Some((
                    e.file_name().to_string_lossy().into_owned(),
                    e.metadata().ok()?,
                ))
            })
            .filter(|(name, _)| all || !name.starts_with('.'))
            .collect();
        entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        for (name, m) in &entries {
            out.push_str(&line(name, m));
        }
    }
    if out.is_empty() {
        out = "(empty)".into();
    }
    Some(Ok(crate::tools::clip(&out)))
}

/// Runs one of Quick's tool calls in `folder`. Quick names its tools as its
/// own sandbox does; Oak's equivalents take Oak's inputs.
fn run_tool(name: &str, args: &Value, folder: &std::path::Path) -> Result<String, String> {
    match name {
        // Oak's own names (when Quick's agent uses the ones it was given).
        "READ" => crate::tools::read(folder, args),
        "WRITE" => crate::tools::write(folder, args),
        "EDIT" => crate::tools::edit(folder, args),
        "SEARCH" => crate::tools::search(args),
        "FETCH" => crate::web::fetch(args),
        "EXECUTE" => execute(args, folder),
        // Quick's sandbox's names.
        "file_read" | "read_file" => {
            let input = json!({"path": args["path"].as_str().unwrap_or("")});
            crate::tools::read(folder, &input)
        }
        "file_write" | "write_file" => {
            let input = json!({"path": args["path"].as_str().unwrap_or(""), "content": args["content"].as_str().unwrap_or("")});
            crate::tools::write(folder, &input)
        }
        "file_edit" => {
            let input = json!({
                "path": args["path"].as_str().unwrap_or(""),
                "old": args["old_string"].as_str().or(args["old"].as_str()).unwrap_or(""),
                "new": args["new_string"].as_str().or(args["new"].as_str()).unwrap_or(""),
            });
            crate::tools::edit(folder, &input)
        }
        "folder_list" | "list_dir" | "list_directory" => {
            let path = resolve(folder, args["path"].as_str().unwrap_or(""));
            // Asking with no path is how Quick learns which folders it may
            // work in (`allowed_folders`); it names them this way, in its own
            // sandbox's vocabulary, and every later path comes back in it.
            if args["path"].as_str().unwrap_or("").trim().is_empty() {
                // The app answers with the real paths (`/C:/…`), which is what
                // its agent then asks to list.
                return Ok(json!({
                    "allowed_folders": [format!("/{}", native(folder))],
                })
                .to_string());
            }
            let mut entries = vec![];
            let read = std::fs::read_dir(&path)
                .map_err(|e| format!("Couldn't list {}: {e}", native(&path)))?;
            {
                for e in read.flatten().take(500) {
                    let p = e.path();
                    let kind = if p.is_dir() { "directory" } else { "file" };
                    entries.push(json!({
                        "name": e.file_name().to_string_lossy(),
                        "type": kind,
                        // The wire form Quick's allowed folders carry (`/C:/…`):
                        // Quick puts its own device prefix on it. A made-up
                        // device name names an outpost that doesn't exist,
                        // and the next call on such a path fails.
                        "path": format!("/{}", native(&p)),
                    }));
                }
            }
            entries.sort_by(|a, b| {
                a["name"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["name"].as_str().unwrap_or(""))
            });
            Ok(json!({
                "success": true,
                "path": format!("/{}", native(&path)),
                "entries": entries,
                "count": entries.len(),
            })
            .to_string())
        }
        "fdfind" | "fd" => fdfind(args, folder),
        "ripgrep" | "rg" | "grep" => ripgrep(args, folder),
        "bash" | "shell" | "run_command" | "execute_command" => {
            let cmd = args["command"]
                .as_str()
                .or(args["cmd"].as_str())
                .unwrap_or("")
                .to_string();
            execute(&json!({"command": cmd}), folder)
        }
        // This machine's Python runs Oak's tools (`crate::quick::calls`):
        // here when no conversation's agent reads the turn (`run_call`).
        n if is_snippet(n) => run_oak_direct(args, folder),
        // Quick's own coding tool by its other names: Python in the
        // conversation's folder.
        "run_python" | "python" => run_python(args, folder),
        // The names the outpost declares (`outpost::TOOLS`), so a call made
        // against this machine's registration runs here too.
        "file_matches" => {
            let want = args["path"].as_str().unwrap_or("");
            Ok(
                json!({"matches": match_paths(folder, want), "outside_allowed_folders": false})
                    .to_string(),
            )
        }
        "folder_create" => {
            let p = resolve(folder, args["path"].as_str().unwrap_or(""));
            std::fs::create_dir_all(&p)
                .map(|_| format!("Created {}.", native(&p)))
                .map_err(|e| format!("Couldn't create {}: {e}", native(&p)))
        }
        "file_delete" => {
            let p = resolve(folder, args["path"].as_str().unwrap_or(""));
            if p.is_dir() {
                std::fs::remove_dir(&p)
            } else {
                std::fs::remove_file(&p)
            }
            .map(|_| format!("Deleted {}.", native(&p)))
            .map_err(|e| format!("Couldn't delete {}: {e}", native(&p)))
        }
        "file_copy" => {
            let (from, to) = (
                resolve(folder, args["source"].as_str().unwrap_or("")),
                resolve(folder, args["destination"].as_str().unwrap_or("")),
            );
            std::fs::copy(&from, &to)
                .map(|_| format!("Copied {} to {}.", native(&from), native(&to)))
                .map_err(|e| format!("Couldn't copy: {e}"))
        }
        // How Quick's agent sees a file on this machine (an image, to look
        // at it): put in the thread's workspace, with its path there.
        "file_upload" => {
            let p = resolve(folder, args["path"].as_str().unwrap_or(""));
            let session = args["session_id"].as_str().unwrap_or("");
            crate::quick::outpost::upload_file(&p, session).map(|v| v.to_string())
        }
        "file_type" => {
            let p = resolve(folder, args["path"].as_str().unwrap_or(""));
            match std::fs::metadata(&p) {
                Ok(m) => Ok(json!({
                    "success": true,
                    "path": native(&p),
                    "type": if m.is_dir() { "directory" } else { "file" },
                    "size_bytes": m.len(),
                    "description": format!("{}: {}, {} bytes.", native(&p), if m.is_dir() { "directory" } else { "file" }, m.len()),
                })
                .to_string()),
                Err(e) => Err(format!("{}: {e}", native(&p))),
            }
        }
        // Quick's sandbox's other tools (its shell, its browser). Its agent
        // reaches for these when it works locally, so they run here too.
        "run_shell" => {
            let cmd = args["command"]
                .as_str()
                .or(args["cmd"].as_str())
                .unwrap_or("")
                .to_string();
            execute(&json!({ "command": cmd }), folder)
        }
        other => Err(format!(
            "Quick asked for {other}, which Oak doesn't run (Oak has no such tool). Oak's own tools are READ, WRITE, EDIT, EXECUTE, SEARCH and FETCH; run shell commands with EXECUTE or run_python."
        )),
    }
}

/// The folder a path Quick sent names. Quick works in its outpost's
/// vocabulary (`/outposts/<device>/C:/Users/…`, and `artifacts/`,
/// `agent_files/` beside it), which doesn't exist on this machine: a path
/// under an outpost is the real one, and the workspace's own roots are the
/// conversation's folder. Anything else is resolved against it.
fn resolve(folder: &std::path::Path, path: &str) -> std::path::PathBuf {
    let p = path.trim();
    if p.is_empty() {
        return folder.to_path_buf();
    }
    // `/outposts/<device>/C:/Users/sussy/Documents/oak_public` → `C:/Users/…`.
    let after = p.strip_prefix("/outposts/");
    let real = match after {
        Some(rest) => {
            let without_device = rest.split_once('/').map(|(_, tail)| tail).unwrap_or(rest);
            // The drive comes back with a leading slash (`/C:/…`); a Unix
            // device's path keeps its root (`/outposts/dev/tmp/x` is `/tmp/x`).
            let t = without_device.trim_start_matches('/');
            if cfg!(windows) || drive_path(t).is_some() {
                t.to_string()
            } else {
                format!("/{t}")
            }
        }
        None => p.to_string(),
    };
    // Quick's own workspace roots are the conversation's folder.
    let bare = real.trim_start_matches("workspace/").trim_end_matches('/');
    let roots = ["artifacts", "agent_files", "attached_files"];
    if bare.is_empty()
        || roots.contains(&bare)
        || roots.iter().any(|r| bare.starts_with(&format!("{r}/")))
    {
        let tail = bare.split_once('/').map(|(_, t)| t).unwrap_or("");
        return if tail.is_empty() {
            folder.to_path_buf()
        } else {
            folder.join(tail)
        };
    }
    // A real path as Quick sends it back: `C:/…` or with the leading slash
    // its allowed folders carry, `/C:/…`.
    if let Some(p) = drive_path(&real) {
        return p;
    }
    let path = std::path::PathBuf::from(&real);
    // A path with a root and no drive is absolute too; anything else is the
    // conversation's folder's.
    if path.is_absolute() {
        path
    } else {
        folder.join(path)
    }
}

/// A Windows path either way it is written: `C:/Users/…` or `/C:/Users/…`.
/// None when it doesn't name a drive.
fn drive_path(s: &str) -> Option<std::path::PathBuf> {
    let t = s.trim_start_matches('/');
    let mut c = t.chars();
    (c.next().is_some_and(|x| x.is_ascii_alphabetic()) && c.next() == Some(':'))
        .then(|| std::path::PathBuf::from(t))
}

/// A path as Quick's outpost names it (`C:/Users/…`), for the paths it is
/// sent back.
fn native(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// What `file_matches` answers with: a path Quick sends whole (`/C:/Users/…`)
/// is checked as it is; a name or a relative path is matched against the
/// conversation's folder, by file name or by the tail of its path.
pub(crate) fn match_paths(folder: &std::path::Path, want: &str) -> Vec<String> {
    let want = want.trim();
    if want.is_empty() {
        return vec![];
    }
    if let Some(p) = drive_path(want) {
        return if p.exists() { vec![native(&p)] } else { vec![] };
    }
    let path = std::path::PathBuf::from(want);
    if path.is_absolute() {
        return if path.exists() {
            vec![native(&path)]
        } else {
            vec![]
        };
    }
    let needle = want
        .trim_start_matches("./")
        .replace('\\', "/")
        .to_lowercase();
    let mut out = vec![];
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in read.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_lowercase();
            if matches!(name.as_str(), ".git" | "node_modules" | "target" | ".oak") {
                continue;
            }
            if p.is_dir() {
                stack.push(p.clone());
            }
            if name.contains(&needle) || native(&p).to_lowercase().ends_with(&needle) {
                out.push(native(&p));
                if out.len() >= 200 {
                    out.sort();
                    return out;
                }
            }
        }
    }
    out.sort();
    out
}

/// Strings a call gives as a list or as one.
fn strings_of(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        Value::String(s) => vec![s.clone()],
        _ => vec![],
    }
}

/// The paths a search names (`paths`, or `path`), on this machine; the
/// conversation's folder when it names none.
fn search_paths(args: &Value, folder: &std::path::Path) -> Vec<std::path::PathBuf> {
    let named: Vec<std::path::PathBuf> = strings_of(&args["paths"])
        .into_iter()
        .chain(strings_of(&args["path"]))
        .filter(|p| !p.trim().is_empty())
        .map(|p| resolve(folder, &p))
        .collect();
    if named.is_empty() {
        vec![folder.to_path_buf()]
    } else {
        named
    }
}

/// The ripgrep Oak bundles (`toolbin`), else the one on PATH.
fn rg_program() -> std::path::PathBuf {
    crate::toolbin::dir()
        .map(|d| d.join(if cfg!(windows) { "rg.exe" } else { "rg" }))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| "rg".into())
}

/// Runs Oak's ripgrep with `argv` in `folder`, `input` on its stdin, for
/// `timeout` seconds at most.
fn run_rg(
    argv: &[std::ffi::OsString],
    folder: &std::path::Path,
    input: Option<Vec<u8>>,
    timeout: u64,
) -> Result<Ran, String> {
    use std::process::Stdio;
    let mut cmd = no_console(std::process::Command::new(rg_program()));
    cmd.args(argv)
        .current_dir(folder)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Couldn't start ripgrep: {e}"))?;
    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin.write_all(&bytes);
        });
    }
    wait_for(child, Duration::from_secs(timeout)).map_err(|e| format!("ripgrep failed: {e}"))
}

/// Quick's `ripgrep`, run by Oak's ripgrep: every one of its `patterns` (a
/// regex each, as rg reads them), its `options` (rg's own flags), in the
/// `paths` it names. It once went to a walk of Oak's own that took the first
/// pattern as plain text and searched only the conversation's folder, so a
/// regex (`a|b`) or another folder found nothing, and the agent went on to
/// search with Python.
fn ripgrep(args: &Value, folder: &std::path::Path) -> Result<String, String> {
    let patterns: Vec<String> = strings_of(&args["patterns"])
        .into_iter()
        .chain(strings_of(&args["pattern"]))
        .filter(|p| !p.is_empty())
        .collect();
    if patterns.is_empty() {
        return Err("ripgrep needs `patterns`.".into());
    }
    let flags = crate::freebuff_tools::parse_flags(args["options"].as_str().unwrap_or(""))
        .ok_or("ripgrep's `options` leave a quote open.")?;
    if let Some(no) = crate::freebuff_tools::flag_refusal(&flags) {
        return Err(no);
    }
    let mut argv: Vec<std::ffi::OsString> = [
        "--no-config",
        "--color=never",
        "--line-number",
        "--with-filename",
        "--no-heading",
        "--max-columns=400",
        "--max-columns-preview",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    if cfg!(windows) {
        argv.push("--path-separator=/".into());
    }
    for p in &patterns {
        argv.push("-e".into());
        argv.push(p.into());
    }
    let ended = flags.iter().any(|f| f == "--");
    argv.extend(flags.into_iter().map(Into::into));
    if !ended {
        argv.push("--".into());
    }
    argv.extend(search_paths(args, folder).into_iter().map(Into::into));
    let timeout = timeout_of(args, 60);
    let ran = run_rg(&argv, folder, None, timeout)?;
    let Some(status) = ran.status else {
        return Err(stopped("The search", timeout, &ran.stdout));
    };
    match status.code() {
        // 1 is rg's "no match": an empty output, as Quick's own answers.
        Some(0 | 1) => Ok(json!({"success": true, "output": crate::tools::clip(&ran.stdout)}).to_string()),
        // Some files couldn't be read; what was found still stands.
        _ if !ran.stdout.trim().is_empty() => Ok(json!({"success": true, "output": crate::tools::clip(&ran.stdout), "errors": crate::tools::clip(ran.stderr.trim())}).to_string()),
        _ => Err(format!("ripgrep failed ({status}): {}", ran.stderr.trim())),
    }
}

/// The most entries `fdfind` lists.
const FIND_MAX: usize = 2_000;

/// Quick's `fdfind`, as fd reads its `args`: a regex matched against each
/// name (smart case), the paths after it, and fd's usual flags (`-e`, `-t`,
/// `-g`, `-H`, `-I`, `-u`, `-d`, `-p`, `-F`, `-E`, `-s`, `-i`). Oak's
/// ripgrep lists the files (`rg --files`, which keeps to .gitignore as fd
/// does) and matches the names; the folders are those the files are in.
fn fdfind(args: &Value, folder: &std::path::Path) -> Result<String, String> {
    let given = crate::freebuff_tools::parse_flags(args["args"].as_str().unwrap_or(""))
        .ok_or("fdfind's `args` leave a quote open.")?;
    // `-HI` is `-H -I`.
    let words: Vec<String> = given
        .into_iter()
        .flat_map(|w| {
            let short = w.len() > 2
                && w.starts_with('-')
                && !w.starts_with("--")
                && w[1..].chars().all(|c| "HIuisFgpaL".contains(c));
            if short {
                w[1..].chars().map(|c| format!("-{c}")).collect()
            } else {
                vec![w]
            }
        })
        .collect();
    let mut list: Vec<std::ffi::OsString> = vec!["--no-config".into(), "--files".into()];
    if cfg!(windows) {
        list.push("--path-separator=/".into());
    }
    let (mut pattern, mut roots, mut exts, mut kinds) = (None::<String>, vec![], vec![], vec![]);
    let (mut glob, mut full, mut fixed, mut case, mut only_positional) =
        (false, false, false, None::<bool>, false);
    let mut it = words.into_iter();
    while let Some(w) = it.next() {
        if only_positional || !w.starts_with('-') || w == "-" {
            if pattern.is_none() {
                pattern = Some(w);
            } else {
                roots.push(resolve(folder, &w));
            }
            continue;
        }
        let (name, inline) = match w.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (w.clone(), None),
        };
        let value = |it: &mut std::vec::IntoIter<String>| {
            inline.clone().or_else(|| it.next()).unwrap_or_default()
        };
        match name.as_str() {
            "--" => only_positional = true,
            "-H" | "--hidden" => list.push("--hidden".into()),
            "-I" | "--no-ignore" => list.push("--no-ignore".into()),
            "-u" | "--unrestricted" => list.extend(["--hidden".into(), "--no-ignore".into()]),
            "-s" | "--case-sensitive" => case = Some(true),
            "-i" | "--ignore-case" => case = Some(false),
            "-g" | "--glob" => glob = true,
            "-p" | "--full-path" => full = true,
            "-F" | "--fixed-strings" => fixed = true,
            "-e" | "--extension" => {
                exts.push(value(&mut it).trim_start_matches('.').to_lowercase())
            }
            "-t" | "--type" => kinds.push(value(&mut it).to_lowercase()),
            "-d" | "--max-depth" => list.extend(["--max-depth".into(), value(&mut it).into()]),
            "-E" | "--exclude" => {
                list.extend(["--glob".into(), format!("!{}", value(&mut it)).into()])
            }
            _ => {}
        }
    }
    let pattern = pattern.unwrap_or_default();
    if roots.is_empty() {
        roots = search_paths(args, folder);
    } else {
        roots.extend(
            strings_of(&args["paths"])
                .iter()
                .filter(|p| !p.trim().is_empty())
                .map(|p| resolve(folder, p)),
        );
    }
    if glob && !pattern.is_empty() {
        list.extend([
            if case == Some(false) {
                "--iglob"
            } else {
                "--glob"
            }
            .into(),
            pattern.clone().into(),
        ]);
    }
    list.push("--".into());
    list.extend(roots.iter().map(|r| r.clone().into_os_string()));
    let timeout = timeout_of(args, 60);
    let ran = run_rg(&list, folder, None, timeout)?;
    let Some(status) = ran.status else {
        return Err(stopped("The search", timeout, &ran.stdout));
    };
    if !status.success() && ran.stdout.trim().is_empty() && status.code() != Some(1) {
        return Err(format!(
            "Listing files failed ({status}): {}",
            ran.stderr.trim()
        ));
    }
    let files: Vec<String> = ran
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.replace('\\', "/"))
        .collect();
    let wants = |k: char| {
        kinds.is_empty()
            || kinds
                .iter()
                .any(|t| t.starts_with(k) || (k == 'f' && t.starts_with('x')))
    };
    let mut entries: Vec<String> = vec![];
    if wants('f') {
        entries.extend(files.iter().cloned());
    }
    if wants('d') && !glob {
        let tops: Vec<String> = roots
            .iter()
            .map(|r| native(r).trim_end_matches('/').to_lowercase())
            .collect();
        let mut dirs = std::collections::BTreeSet::new();
        for f in &files {
            let mut p = f.as_str();
            while let Some((parent, _)) = p.rsplit_once('/') {
                let low = parent.to_lowercase();
                if tops.contains(&low)
                    || !tops.iter().any(|t| low.starts_with(&format!("{t}/")))
                    || !dirs.insert(parent.to_string())
                {
                    break;
                }
                p = parent;
            }
        }
        entries.extend(dirs);
    }
    let base = |e: &str| e.rsplit('/').next().unwrap_or(e).to_string();
    if !exts.is_empty() {
        entries.retain(|e| {
            base(e)
                .rsplit_once('.')
                .is_some_and(|(_, x)| exts.contains(&x.to_lowercase()))
        });
    }
    if !glob && !pattern.is_empty() {
        // The names go through ripgrep, one a line, so the pattern is matched
        // with the regex fd itself uses; a match's line number is its entry.
        let names: String = entries
            .iter()
            .map(|e| if full { e.clone() } else { base(e) })
            .collect::<Vec<_>>()
            .join("\n");
        let mut argv: Vec<std::ffi::OsString> = vec![
            "--no-config".into(),
            "--color=never".into(),
            "--line-number".into(),
            "--no-filename".into(),
        ];
        argv.push(match case {
            Some(true) => "--case-sensitive".into(),
            Some(false) => "--ignore-case".into(),
            None => "--smart-case".into(),
        });
        if fixed {
            argv.push("--fixed-strings".into());
        }
        argv.extend(["-e".into(), pattern.into(), "-".into()]);
        let ran = run_rg(&argv, folder, Some(names.into_bytes()), timeout)?;
        if ran.status.and_then(|s| s.code()) == Some(2) {
            return Err(format!("fdfind's pattern: {}", ran.stderr.trim()));
        }
        let hit: std::collections::BTreeSet<usize> = ran
            .stdout
            .lines()
            .filter_map(|l| l.split(':').next()?.parse::<usize>().ok())
            .collect();
        entries = entries
            .into_iter()
            .enumerate()
            .filter(|(i, _)| hit.contains(&(i + 1)))
            .map(|(_, e)| e)
            .collect();
    }
    entries.sort();
    entries.dedup();
    let more = entries.len().saturating_sub(FIND_MAX);
    entries.truncate(FIND_MAX);
    let mut out = entries.join("\n");
    if more > 0 {
        out.push_str(&format!("\n[{more} more not listed: narrow the search]"));
    }
    Ok(json!({"success": true, "output": out}).to_string())
}

/// The time limit a call asks for (`timeout`, in seconds, a number or a
/// string), else `default`; one second at least, `EXECUTE_TIMEOUT_MAX` at
/// most.
fn timeout_of(args: &Value, default: u64) -> u64 {
    let t = &args["timeout"];
    let asked = t
        .as_u64()
        .or_else(|| t.as_f64().filter(|f| *f > 0.0).map(|f| f.ceil() as u64))
        .or_else(|| t.as_str().and_then(|s| s.trim().parse().ok()));
    asked
        .unwrap_or(default)
        .clamp(1, crate::tools::EXECUTE_TIMEOUT_MAX.max(1))
}

/// What a tool says of a program it stopped at its time limit (or when the
/// user interrupted or ultrapaused the turn): what the program printed by
/// then, and how to give it longer.
fn stopped(what: &str, timeout: u64, printed: &str) -> String {
    if let Some(kind) = current_abort() {
        let why = match kind {
            StopKind::Interrupt => "the user interrupted the turn",
            StopKind::Pause => "the user paused the session (ultrapause)",
        };
        let mut s = format!("{what} was stopped because {why}.");
        if !printed.trim().is_empty() {
            s.push_str(&format!(
                " What it printed by then:\n{}",
                crate::tools::clip(printed)
            ));
        }
        return s;
    }
    let mut s = format!("{what} ran past its {timeout}s and was stopped.");
    if !printed.trim().is_empty() {
        s.push_str(&format!(
            " What it printed by then:\n{}\n",
            crate::tools::clip(printed)
        ));
    }
    s.push_str(&format!(" Give it a longer `timeout` (up to {}s), or start long work in the background and check on it.", crate::tools::EXECUTE_TIMEOUT_MAX));
    s
}

/// What a program a tool started printed, and how it ended: `status` is None
/// when it ran past its time and was stopped.
struct Ran {
    status: Option<std::process::ExitStatus>,
    stdout: String,
    stderr: String,
}

/// The most of a program's output held while it runs (a tool result is
/// clipped far shorter).
const OUTPUT_CAP: usize = 8 * 1024 * 1024;

/// Waits on `child` for `timeout`, reading its output as it comes: a program
/// whose pipe fills while nobody reads it stops until somebody does, and sat
/// out its whole time limit that way. One that runs past `timeout` (or while
/// the turn is aborted) is stopped, with what it started, and what it printed
/// by then is kept.
fn wait_for(mut child: std::process::Child, timeout: Duration) -> Result<Ran, String> {
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if std::time::Instant::now() >= deadline || current_abort().is_some() => {
                kill_tree(&mut child);
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                kill_tree(&mut child);
                return Err(e.to_string());
            }
        }
    };
    // A program it started and left running may hold the pipes open: the
    // output is taken for a moment more, not until they close.
    let grace = std::time::Instant::now()
        + if current_abort().is_some() {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(2)
        };
    Ok(Ran {
        status,
        stdout: gather(&out, grace),
        stderr: gather(&err, grace),
    })
}

/// Reads `pipe` on a thread of its own, handing on what it reads.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    rx
}

/// What `drain` read, until its pipe closed or `until`.
fn gather(rx: &std::sync::mpsc::Receiver<Vec<u8>>, until: std::time::Instant) -> String {
    let mut bytes = vec![];
    while let Ok(b) = rx.recv_timeout(until.saturating_duration_since(std::time::Instant::now())) {
        if bytes.len() < OUTPUT_CAP {
            bytes.extend(b);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Stops `child` and, on Windows, what it started (`taskkill /T`): a
/// snippet's own subprocesses would otherwise run on and hold its pipes.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        use std::process::Stdio;
        let _ = no_console(std::process::Command::new("taskkill"))
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Runs a shell command in `folder`, with the output clipped for a tool
/// result.
fn execute(input: &Value, folder: &std::path::Path) -> Result<String, String> {
    use std::process::Stdio;
    let command = input["command"].as_str().unwrap_or("").trim();
    if command.is_empty() {
        return Err("EXECUTE needs a `command`.".into());
    }
    // Windows' shell has no `ls`, and Quick's agent lists folders with it.
    if cfg!(windows)
        && let Some(listing) = unix_ls(command, folder)
    {
        return listing;
    }
    let timeout = timeout_of(input, 120);
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(command);
        no_console(c)
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = std::process::Command::new("bash");
        c.arg("-lc").arg(command);
        c
    };
    cmd.current_dir(folder)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd
        .spawn()
        .map_err(|e| format!("Couldn't run `{command}`: {e}"))?;
    let ran = wait_for(child, Duration::from_secs(timeout))
        .map_err(|e| format!("`{command}` failed: {e}"))?;
    let mut text = ran.stdout;
    if text.trim().is_empty() {
        text = ran.stderr;
    } else if !ran.stderr.trim().is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&ran.stderr);
    }
    let Some(status) = ran.status else {
        return Err(stopped(&format!("`{command}`"), timeout, &text));
    };
    if text.trim().is_empty() {
        text = format!("`{command}` finished with {status}.");
    }
    Ok(crate::tools::clip(&text))
}

/// Runs a Python snippet in `folder` (Quick's own coding tool: its agent
/// uses it for shell work too), with the output clipped for a tool result.
/// It runs as long as the call asks (`timeout`), two minutes when it doesn't
/// say.
fn run_python(args: &Value, folder: &std::path::Path) -> Result<String, String> {
    use std::process::Stdio;
    let code = args["code"]
        .as_str()
        .or(args["script"].as_str())
        .or(args["source"].as_str())
        .unwrap_or("");
    if code.trim().is_empty() {
        return Err("run_python needs `code`.".into());
    }
    let timeout = timeout_of(args, 120);
    // The snippet goes beside Oak's scratchpad, so the working folder's own
    // files aren't touched.
    let dir = folder.join(".oak").join("tmp");
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("quick-{}.py", crate::api::now_ms()));
    std::fs::write(&file, code).map_err(|e| format!("Couldn't write the snippet: {e}"))?;
    let mut python = python_command();
    python
        .arg(&file)
        .current_dir(folder)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = python
        .spawn()
        .map_err(|e| format!("Couldn't start Python: {e}"))?;
    let ran =
        wait_for(child, Duration::from_secs(timeout)).map_err(|e| format!("Python failed: {e}"))?;
    let mut text = ran.stdout;
    if !ran.stderr.trim().is_empty() {
        if !text.trim().is_empty() {
            text.push_str("\n[stderr]\n");
        }
        text.push_str(&ran.stderr);
    }
    let Some(status) = ran.status else {
        return Err(stopped("The snippet", timeout, &text));
    };
    if text.trim().is_empty() {
        text = format!("The snippet finished and printed nothing ({status}).");
    }
    Ok(crate::tools::clip(&text))
}

/// A program started for a tool, with no console window: Oak is a windowed
/// program, so a console program it starts would otherwise get a console of
/// its own, shown on the user's desktop. The program still has a (hidden)
/// console, which what it starts in turn shares.
fn no_console(mut cmd: std::process::Command) -> std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Python: the environment's, or the usual launcher on Windows (looked for
/// once, not before every snippet), started with no console window.
fn python_command() -> std::process::Command {
    if let Ok(p) = std::env::var("OAK_TEST_PYTHON")
        && !p.trim().is_empty()
    {
        return no_console(std::process::Command::new(p));
    }
    #[cfg(windows)]
    const CANDIDATES: [&str; 3] = ["python", "py", "python3"];
    #[cfg(not(windows))]
    const CANDIDATES: [&str; 2] = ["python3", "python"];
    static FOUND: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    let found = FOUND.get_or_init(|| {
        CANDIDATES
            .into_iter()
            .find(|c| {
                no_console(std::process::Command::new(c))
                    .arg("--version")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
            })
            .unwrap_or(CANDIDATES[0])
    });
    no_console(std::process::Command::new(found))
}

/// The outpost Quick routes this machine's tool calls to: the one saved at
/// sign-in, or `OAK_QUICK_OUTPOST` (the desktop app registers one per
/// machine; Quick's sandbox is used when there is none).
fn outpost() -> Option<String> {
    if let Ok(o) = std::env::var("OAK_QUICK_OUTPOST") {
        let o = o.trim().to_string();
        if !o.is_empty() {
            return Some(o);
        }
    }
    let saved = crate::quick::login::saved_outpost();
    (!saved.is_empty()).then_some(saved)
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Quick sends a path back in its own form; each of them names the file on
    /// this machine, so a tool that stats one doesn't fail on the drive's
    /// doubled name (`C:/C:/…`).
    #[test]
    fn a_path_quick_sends_resolves_to_this_machine() {
        let folder = std::path::Path::new("C:/work/project");
        assert_eq!(
            resolve(folder, "src/main.rs"),
            std::path::PathBuf::from("C:/work/project/src/main.rs")
        );
        assert_eq!(
            resolve(folder, "/C:/work/project/src/main.rs"),
            std::path::PathBuf::from("C:/work/project/src/main.rs")
        );
        assert_eq!(
            resolve(folder, "C:/work/project/src/main.rs"),
            std::path::PathBuf::from("C:/work/project/src/main.rs")
        );
        assert_eq!(
            resolve(folder, "/outposts/DESKTOP/C:/work/project/src/main.rs"),
            std::path::PathBuf::from("C:/work/project/src/main.rs")
        );
        assert_eq!(
            resolve(folder, "/outposts/DESKTOP//C:/work/project/src/main.rs"),
            std::path::PathBuf::from("C:/work/project/src/main.rs")
        );
        assert_eq!(
            resolve(folder, "artifacts/out.txt"),
            std::path::PathBuf::from("C:/work/project/out.txt")
        );
        assert_eq!(
            resolve(folder, "/C:/work/project"),
            std::path::PathBuf::from("C:/work/project")
        );
    }

    /// `file_matches` answers a path it is given whole, and finds a relative
    /// one by name and by path's tail.
    #[test]
    fn file_matches_finds_what_is_there() {
        let dir = std::env::temp_dir().join(format!("oak-quick-matches-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/quick")).unwrap();
        std::fs::write(dir.join("src/quick/turn.rs"), b"x").unwrap();
        std::fs::write(dir.join("readme.md"), b"x").unwrap();
        let whole = format!("/{}", native(&dir.join("src/quick/turn.rs")));
        assert_eq!(match_paths(&dir, &whole).len(), 1);
        assert_eq!(
            match_paths(&dir, "src/quick/turn.rs").len(),
            1,
            "a relative path matches by its tail"
        );
        assert_eq!(
            match_paths(&dir, "turn.rs").len(),
            1,
            "a name matches by name"
        );
        assert!(match_paths(&dir, "nothing-here.txt").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A result goes to Quick as its app sends one: an object, `error` empty
    /// (a tool that failed is a result that says so), and no transport token
    /// (an echoed token is a result Quick's relay looks for elsewhere).
    #[test]
    fn a_tool_result_is_shaped_as_quicks_app_sends_it() {
        let text = tool_reply(
            "c1",
            &Ok("C:/x/turn.rs: file, 42 bytes.".into()),
            "0123456789abcdef0123456789abcdef",
        );
        assert_eq!(text["type"], "tool_result");
        assert_eq!(text["payload"]["tool_call_id"], "c1");
        assert!(
            text["payload"]["result"].is_object(),
            "a bare string is a result Quick can't read: {text}"
        );
        assert_eq!(
            text["payload"]["result"]["output"],
            "C:/x/turn.rs: file, 42 bytes."
        );
        assert!(text["payload"]["error"].is_null());
        assert!(
            text["payload"].get("result_transport_token").is_none(),
            "the token isn't sent back: {text}"
        );
        let listing = tool_reply("c2", &Ok(r#"{"success":true,"entries":[]}"#.into()), "");
        assert_eq!(listing["payload"]["result"]["success"], true);
        let failed = tool_reply("c3", &Err("no such file".into()), "");
        assert_eq!(failed["payload"]["result"]["success"], false);
        assert_eq!(failed["payload"]["result"]["error"], "no such file");
        assert!(
            failed["payload"]["error"].is_null(),
            "`error` is the relay's, not the tool's"
        );
    }

    /// Quick's relay records its own error for a call Oak answered: that
    /// answer is lost, and kept to be given back. Quick names the call by its
    /// own id, so the error goes with the newest call of its name.
    #[test]
    fn a_result_the_relay_replaced_is_known_as_lost() {
        let mut calls = Calls::default();
        let ok = Ok(json!({"success": true, "size_bytes": 34711}).to_string());
        calls.record(
            "call-1",
            "file_type",
            "file_type{}",
            &ok,
            tool_reply("call-1", &ok, ""),
        );
        // Another tool's own failure isn't one of Oak's results lost.
        calls.saw_server_result(
            &json!({"tool_id": "tooluse_x", "tool_name": "web_search", "error": "rate limited"}),
        );
        assert!(calls.lost.is_empty());
        calls.saw_server_result(&json!({
            "tool_id": "tooluse_abc", "tool_name": "file_type", "status": "error",
            "error": "[{'text': 'Error: RuntimeError - SNCS outpost invoke error (500)'}]"
        }));
        assert_eq!(calls.lost.len(), 1);
        assert_eq!(calls.lost[0].call, "call-1");
        assert!(calls.lost[0].content.contains("34711"));
        let text = resend_text(&calls.lost);
        assert!(
            text.contains("34711") && text.contains("SNCS outpost invoke error"),
            "{text}"
        );
        // Given back once: the same error again doesn't lose it twice.
        calls.resent.push("call-1".into());
        calls.lost.clear();
        calls.saw_server_result(
            &json!({"tool_id": "call-1", "error": "SNCS outpost invoke error (500)"}),
        );
        assert!(calls.lost.is_empty());
    }

    /// A call that failed here was answered as a failure: the relay's error
    /// for it hides nothing, so nothing is given back.
    #[test]
    fn a_message_quick_took_in_is_known_by_its_id() {
        let thread = "steer-test-thread";
        steer(thread, 7, "NO TESTING");
        steer(thread, 8, "and later");
        let id = STEERS.lock().unwrap()[thread][0].message.clone();
        // Not sent yet: Quick can't have taken it in.
        assert!(!took_in(thread, &json!({"message_ids": [id]})));
        for s in STEERS.lock().unwrap().get_mut(thread).unwrap() {
            s.sent = true;
        }
        assert!(took_in(
            thread,
            &json!({"message_ids": [id], "messages": [{"content": "NO TESTING"}]})
        ));
        assert_eq!(take_ingested(thread), vec![7]);
        assert!(take_ingested(thread).is_empty());
    }

    #[test]
    fn a_call_that_failed_here_is_not_given_back() {
        let mut calls = Calls::default();
        let failed = Err("no such file".to_string());
        calls.record(
            "call-2",
            "file_type",
            "k",
            &failed,
            tool_reply("call-2", &failed, ""),
        );
        calls.saw_server_result(
            &json!({"tool_id": "call-2", "error": "SNCS outpost invoke error (500)"}),
        );
        assert!(calls.lost.is_empty());
        // A call asked again is answered with the same reply, not run again.
        assert!(calls.reply_for("call-2", "k").is_some());
        assert!(calls.reply_for("call-2", "other input").is_none());
    }

    /// Quick hands every socket each call: a live thread's call is its own
    /// turn's, and one of a thread no turn reads is answered once.
    #[test]
    fn another_threads_calls_are_left_to_its_turn() {
        let call = |id: &str, of: &str| {
            json!({"tool_call_id": id, "tool_name": "run_python_outpost_local",
                "arguments": {"params": {"arguments": {"code": "print(1)"}, "_quickwork": {"offload_session_id": of}}}})
        };
        let _a = Live::new("oak-test-thread-a");
        let _b = Live::new("oak-test-thread-b");
        assert!(matches!(
            owner(&call("x1", "oak-test-thread-a"), "oak-test-thread-a"),
            Owner::Mine
        ));
        assert!(
            matches!(
                owner(&call("x2", "oak-test-thread-b"), "oak-test-thread-a"),
                Owner::Theirs
            ),
            "a live thread's call is its turn's"
        );
        assert!(
            matches!(owner(&call("x3", ""), "oak-test-thread-a"), Owner::Mine),
            "a call naming no thread is the turn's"
        );
        assert!(matches!(
            owner(&call("x4", "oak-test-thread-gone"), "oak-test-thread-a"),
            Owner::Orphan
        ));
        assert!(
            matches!(
                owner(&call("x4", "oak-test-thread-gone"), "oak-test-thread-b"),
                Owner::Theirs
            ),
            "an orphan is run once"
        );
        drop(_b);
        assert!(
            matches!(
                owner(&call("x5", "oak-test-thread-b"), "oak-test-thread-a"),
                Owner::Orphan
            ),
            "a turn that ended reads its thread no more"
        );
    }

    /// A snippet runs with no console window on the user's desktop, and so
    /// does what it starts.
    #[cfg(windows)]
    #[test]
    fn a_snippet_opens_no_console_window() {
        let dir = std::env::temp_dir().join(format!("oak-quick-console-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let code = "import ctypes, subprocess, sys\nk, u = ctypes.windll.kernel32, ctypes.windll.user32\nw = k.GetConsoleWindow()\nprint('shown' if w and u.IsWindowVisible(w) else 'hidden')\nprint(subprocess.run([sys.executable, '-c', 'print(42)'], capture_output=True, text=True).stdout.strip())";
        let out = run_python(&json!({"code": code}), &dir);
        let _ = std::fs::remove_dir_all(&dir);
        let out = match out {
            Ok(o) => o,
            Err(e) if e.starts_with("Couldn't start Python") => {
                eprintln!("no Python here: {e}");
                return;
            }
            Err(e) => panic!("{e}"),
        };
        assert!(out.contains("hidden") && !out.contains("shown"), "{out}");
        assert!(out.contains("42"), "what a snippet starts runs too: {out}");
    }

    /// `ls` and `folder_list` list a real folder, and `file_type` gives the
    /// size as a number.
    #[test]
    fn a_real_folder_is_listed() {
        let dir = std::env::temp_dir().join(format!("oak-quick-ls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join(".hidden"), b"x").unwrap();
        let ls = unix_ls("ls", &dir).unwrap().unwrap();
        assert!(
            ls.contains("a.txt") && ls.contains("sub") && !ls.contains(".hidden"),
            "{ls}"
        );
        let la = unix_ls("ls -la sub a.txt", &dir).unwrap().unwrap();
        assert!(la.contains("a.txt") && la.contains("sub:"), "{la}");
        assert!(
            unix_ls("ls | sort", &dir).is_none(),
            "a pipe goes to the shell"
        );
        assert!(unix_ls("ls missing", &dir).unwrap().is_err());
        let listed: Value = serde_json::from_str(
            &run_tool("folder_list", &json!({"path": native(&dir)}), &dir).unwrap(),
        )
        .unwrap();
        let names: Vec<&str> = listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["name"].as_str())
            .collect();
        assert!(
            names.contains(&"a.txt") && names.contains(&"sub"),
            "{listed}"
        );
        assert!(
            !listed["path"].as_str().unwrap().starts_with("/outposts/"),
            "paths go in the wire form: {listed}"
        );
        assert!(
            run_tool(
                "folder_list",
                &json!({"path": native(&dir.join("missing"))}),
                &dir
            )
            .is_err(),
            "a missing folder isn't an empty one"
        );
        let kind: Value = serde_json::from_str(
            &run_tool(
                "file_type",
                &json!({"path": format!("/{}", native(&dir.join("a.txt")))}),
                &dir,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(kind["size_bytes"], 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A snippet keeps to the time its call asks for and says what it
    /// printed before it was stopped, even when what it started holds its
    /// pipes; one that prints a lot isn't held up on its pipe.
    #[test]
    fn a_snippet_keeps_to_its_time_and_its_output() {
        assert_eq!(timeout_of(&json!({}), 120), 120);
        assert_eq!(timeout_of(&json!({"timeout": 5}), 120), 5);
        assert_eq!(timeout_of(&json!({"timeout": "7"}), 120), 7);
        assert_eq!(timeout_of(&json!({"timeout": 0}), 120), 1);
        assert_eq!(
            timeout_of(&json!({"timeout": 1_000_000}), 120),
            crate::tools::EXECUTE_TIMEOUT_MAX
        );
        let dir = std::env::temp_dir().join(format!("oak-quick-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // (Printed first, and with time to spare: Python can take seconds to
        // start on a busy machine, CI's included.)
        let code = "import subprocess, sys, time\nprint('started', flush=True)\nsubprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])\ntime.sleep(60)";
        let start = std::time::Instant::now();
        let slow = run_python(&json!({"code": code, "timeout": 6}), &dir);
        let took = start.elapsed();
        match slow {
            Err(e) if e.starts_with("Couldn't start Python") => {
                eprintln!("no Python here: {e}");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            Err(e) => assert!(
                e.contains("ran past its 6s") && e.contains("started"),
                "{e}"
            ),
            Ok(o) => panic!("a snippet past its time isn't a result: {o}"),
        }
        assert!(
            took < Duration::from_secs(25),
            "stopped at its time, not when its pipes closed: {took:?}"
        );
        let start = std::time::Instant::now();
        let big = run_python(&json!({"code": "import sys\nsys.stdout.write('x' * 3_000_000)\nprint('done')", "timeout": 60}), &dir).unwrap();
        assert!(
            big.contains("xxxx") && big.contains("done"),
            "{}",
            &big[..big.len().min(200)]
        );
        assert!(start.elapsed() < Duration::from_secs(30));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Quick's `ripgrep` is ripgrep: every pattern a regex, rg's options, in
    /// the paths the call names. Its `fdfind` reads fd's arguments.
    #[test]
    fn search_tools_run_oaks_ripgrep() {
        let dir = std::env::temp_dir().join(format!("oak-quick-rg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn timeout() {}\nlet deadline = 1;\n").unwrap();
        std::fs::write(dir.join("other/b.txt"), "TIMEOUT here\n").unwrap();
        let src = native(&dir.join("src"));
        let out = |r: Result<String, String>| -> String {
            let v: Value = serde_json::from_str(&r.unwrap()).unwrap();
            v["output"].as_str().unwrap_or("").to_string()
        };
        let first = ripgrep(
            &json!({"patterns": [r"time(out)?\(", "deadline"], "paths": [format!("/outposts/DESKTOP/{src}")]}),
            &dir,
        );
        if let Err(e) = &first
            && e.starts_with("Couldn't start ripgrep")
        {
            eprintln!("no ripgrep here: {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let found = out(first);
        assert!(
            found.contains("a.rs:1:") && found.contains("a.rs:2:") && !found.contains("b.txt"),
            "{found}"
        );
        assert_eq!(
            out(ripgrep(
                &json!({"patterns": ["nothing-like-this"], "paths": [src]}),
                &dir
            )),
            ""
        );
        let both = out(ripgrep(
            &json!({"patterns": ["timeout"], "options": "-i -l"}),
            &dir,
        ));
        assert!(both.contains("a.rs") && both.contains("b.txt"), "{both}");
        assert!(ripgrep(&json!({"patterns": ["x"], "options": "--pre=cat"}), &dir).is_err());
        let rs = out(fdfind(
            &json!({"args": "-e rs", "paths": [native(&dir)]}),
            &dir,
        ));
        assert!(rs.contains("a.rs") && !rs.contains("b.txt"), "{rs}");
        let b = out(fdfind(&json!({"args": r"^b\."}), &dir));
        assert!(b.ends_with("other/b.txt") && !b.contains("a.rs"), "{b}");
        let dirs = out(fdfind(&json!({"args": "-t d oth"}), &dir));
        assert!(dirs.ends_with("/other") && !dirs.contains("a.rs"), "{dirs}");
        let txt = out(fdfind(&json!({"args": "-g '*.txt'"}), &dir));
        assert!(txt.ends_with("b.txt") && !txt.contains("a.rs"), "{txt}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_running_tool_stops_at_once_on_interrupt_or_ultrapause() {
        let dir = std::env::temp_dir().join(format!("oak-quick-abort-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (kind, expected) in [
            (StopKind::Interrupt, "the user interrupted the turn"),
            (StopKind::Pause, "the user paused the session (ultrapause)"),
        ] {
            let ctl = Arc::new(TurnCtl::default());
            let ctl2 = ctl.clone();
            // Stopped once the snippet has printed (Python can take seconds
            // to start on a busy machine), and timed from the stop.
            let marker = dir.join("printed");
            let _ = std::fs::remove_file(&marker);
            let seen = marker.clone();
            let stopped_at = std::thread::spawn(move || {
                let wait = std::time::Instant::now();
                while !seen.exists() && wait.elapsed() < Duration::from_secs(60) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                ctl2.set_abort(kind);
                std::time::Instant::now()
            });
            CURRENT_CTL.with(|c| *c.borrow_mut() = Some(ctl));
            let res = run_python(
                &json!({"code": "import time, pathlib\nprint('before', flush=True)\npathlib.Path('printed').write_text('1')\ntime.sleep(60)", "timeout": 90}),
                &dir,
            );
            CURRENT_CTL.with(|c| *c.borrow_mut() = None);
            let took = stopped_at.join().unwrap().elapsed();
            match res {
                Err(e) if e.starts_with("Couldn't start Python") => {
                    let _ = std::fs::remove_dir_all(&dir);
                    return;
                }
                Err(e) => {
                    assert!(e.contains(expected) && e.contains("before"), "{e}");
                    assert!(took < Duration::from_secs(5), "stopped promptly: {took:?}");
                }
                Ok(o) => panic!("expected abort error, got: {o}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With no conversation's agent reading the turn, a snippet's Oak calls
    /// run here, at once, a file's in order, and Quick gets each result.
    #[test]
    fn a_snippet_runs_oaks_calls_here_when_no_agent_reads_the_turn() {
        let dir = std::env::temp_dir().join(format!("oak-quick-snippet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let code = "WRITE(path='a.txt', content='one\\ntwo\\n')\nREAD(path='a.txt')\nREAD('missing.txt')";
        let out = run_tool("run_python_outpost_local", &json!({"code": code}), &dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(out.starts_with("[1/3] WRITE(path='a.txt', content='one\\ntwo\\n')\n"), "{out}");
        let read = out.split("[2/3] READ(path='a.txt')\n").nth(1).unwrap_or("");
        assert!(read.contains("one") && read.contains("two"), "the read follows the write: {out}");
        assert!(out.contains("[3/3] READ('missing.txt')\nError: "), "{out}");
        let bad = run_tool("run_python_outpost_local", &json!({"code": "import os"}), &dir).unwrap_err();
        assert!(bad.contains("Oak tool calls only"), "{bad}");
    }

    /// A device's path is the path on the device: a drive's on Windows, and
    /// one from the root on Linux.
    #[test]
    fn a_device_path_is_the_real_path() {
        let folder = std::path::Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
        assert_eq!(resolve(folder, "/outposts/DESKTOP/C:/Users/x"), std::path::PathBuf::from("C:/Users/x"));
        assert_eq!(resolve(folder, "/outposts/DESKTOP//C:/Users/x"), std::path::PathBuf::from("C:/Users/x"));
        assert_eq!(resolve(folder, "artifacts/a.txt"), folder.join("a.txt"));
        assert_eq!(resolve(folder, "src/a.rs"), folder.join("src/a.rs"));
        if !cfg!(windows) {
            assert_eq!(resolve(folder, "/outposts/box/tmp/x/src"), std::path::PathBuf::from("/tmp/x/src"));
            assert_eq!(resolve(folder, "/outposts/box//tmp/x"), std::path::PathBuf::from("/tmp/x"));
        }
    }

    /// What Oak's agent returns for a call a turn waits on goes to that turn.
    #[test]
    fn a_result_goes_to_the_turn_waiting_for_it() {
        let (tx, rx) = std::sync::mpsc::channel();
        WAITERS.lock().unwrap().insert("w1_oak0".into(), tx);
        ran(&json!({"type": "tool_result", "call_id": "other", "content": "x"}));
        ran(&json!({"type": "tool_result", "call_id": "w1_oak0", "content": "done"}));
        assert_eq!(rx.try_recv().unwrap()["content"], "done");
        assert!(rx.try_recv().is_err());
        assert!(!WAITERS.lock().unwrap().contains_key("w1_oak0"));
        let r = Runner::new("oak-test-runner");
        let r2 = Runner::new("oak-test-runner");
        drop(r);
        assert!(has_runner("oak-test-runner"));
        drop(r2);
        assert!(!has_runner("oak-test-runner"));
    }

    #[test]
    fn a_steered_message_can_be_canceled_before_ingest() {
        let thread = "steer-cancel-thread";
        steer(thread, 101, "cancel me before send");
        assert!(cancel_steer(101));
        assert!(STEERS.lock().unwrap()[thread].is_empty());

        steer(thread, 102, "cancel me after send");
        STEERS.lock().unwrap().get_mut(thread).unwrap()[0].sent = true;
        assert!(cancel_steer(102));
        assert!(STEERS.lock().unwrap()[thread][0].canceled);
        assert!(take_ingested(thread).is_empty());
    }

    #[test]
    fn a_new_turn_ignores_messages_from_an_interrupted_one() {
        let mut t = Turn::for_message("th-ignore", "");
        t.ignore_messages.insert("old-msg-1".into());
        let old = json!({"type": "message_start", "payload": {"role": "assistant"}, "thread_id": "th-ignore", "message_id": "old-msg-1"});
        assert!(!t.accepts(&old));
        let fresh = json!({"type": "message_start", "payload": {"role": "assistant"}, "thread_id": "th-ignore", "message_id": "new-msg-2"});
        assert!(t.accepts(&fresh));
    }
}
