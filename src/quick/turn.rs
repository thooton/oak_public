//! One Quick turn: the socket, the user's message, and Quick's frames read
//! until the message ends. Quick asks for tools by name; Oak runs each one
//! here, in the conversation's working folder, and answers on the socket, so
//! Quick works on the real project rather than on a copy in its sandbox.

use super::*;
use crate::quick::socket::Socket;

/// How long one turn may run before Oak gives up on it.
const TURN_LIMIT: Duration = Duration::from_secs(3_600);

type Threads = std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, usize>>>;

/// The threads whose turns this Oak is reading, with how many. Quick hands
/// this machine's tool calls to every socket Oak has open to it, whatever
/// thread asked (a call names its thread only in its arguments), so each
/// turn runs and shows only its own thread's calls (`owner`).
static LIVE: Threads = std::sync::LazyLock::new(Default::default);

/// Calls of threads no turn here is reading: the first turn to see one runs
/// it, once.
static CLAIMED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::LazyLock::new(Default::default);

/// Messages the user typed while a Quick turn of theirs ran, by thread. Oak
/// hands each to the turn (`steer`), the turn sends it on its socket as
/// Quick's app does (`steer_message`: `mid_turn`), and Quick says when its
/// agent took it in (`steering_ingested`, naming its `message_id`).
static STEERS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Vec<Steer>>>> = std::sync::LazyLock::new(Default::default);

struct Steer {
    /// Oak's queued message (`agent::Queued::id`); 0 for one it didn't queue.
    id: u64,
    text: String,
    /// The `message_id` it goes with.
    message: String,
    sent: bool,
    taken: bool,
}

/// Hands `text`, the user's queued message `id`, to the Quick turn running
/// on `thread`: it goes on the socket with the turn's next frame.
pub(crate) fn steer(thread: &str, id: u64, text: &str) {
    let message = format!("msg-{}", &hex_uuid().replace('-', "")[..8]);
    let s = Steer { id, text: text.to_string(), message, sent: false, taken: false };
    STEERS.lock().unwrap_or_else(|e| e.into_inner()).entry(thread.to_string()).or_default().push(s);
}

/// The queued messages Quick's agent took in on `thread` in the request that
/// just ended. The thread's steers are done with: one not taken in goes with
/// the next request as a message of its own.
pub(crate) fn take_ingested(thread: &str) -> Vec<u64> {
    let steers = STEERS.lock().unwrap_or_else(|e| e.into_inner()).remove(thread).unwrap_or_default();
    steers.into_iter().filter(|s| s.taken).map(|s| s.id).collect()
}

/// Sends the messages handed to the turn on `thread` that haven't gone yet.
fn send_steers(sock: &mut Socket, thread: &str, say: &dyn Fn(&str, &str) -> Value) -> Result<(), String> {
    let due: Vec<(String, String)> = {
        let mut all = STEERS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(v) = all.get_mut(thread) else { return Ok(()) };
        v.iter_mut().filter(|s| !s.sent).map(|s| {
            s.sent = true;
            (s.text.clone(), s.message.clone())
        }).collect()
    };
    for (text, message) in due {
        if std::env::var("OAK_QUICK_DEBUG").is_ok() {
            eprintln!("STEER sending {message} mid-turn: {}", text.chars().take(80).collect::<String>());
        }
        sock.send(&say(&text, &message))?;
    }
    Ok(())
}

/// Quick's agent took in messages (`steering_ingested`, payload `p`): marks
/// the turn's among them, by message id (or text). True when any were.
fn took_in(thread: &str, p: &Value) -> bool {
    let ids: Vec<&str> = p["message_ids"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let said: Vec<&str> = p["messages"].as_array().into_iter().flatten().filter_map(|m| m["content"].as_str()).map(str::trim).collect();
    let mut all = STEERS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(v) = all.get_mut(thread) else { return false };
    let mut any = false;
    for s in v.iter_mut().filter(|s| s.sent && !s.taken) {
        if ids.contains(&s.message.as_str()) || said.contains(&s.text.trim()) {
            s.taken = true;
            any = true;
        }
    }
    any
}

/// Turns left open for Oak's next request, by thread: Quick took in a
/// message the user typed (`steering_ingested`), so the reply Oak was reading
/// ended there, and Quick's agent carries on in the same message, which the
/// next request (the message in the transcript, after the results) reads on
/// the same socket.
static PARKED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Parked>>> = std::sync::LazyLock::new(Default::default);

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
    let p = PARKED.lock().unwrap_or_else(|e| e.into_inner()).remove(thread)?;
    (p.at.elapsed() < PARK_LIMIT).then_some(p)
}

/// A turn reading its thread, for as long as it lives.
pub(crate) struct Live(String);

impl Live {
    pub(crate) fn new(thread: &str) -> Live {
        *LIVE.lock().unwrap_or_else(|e| e.into_inner()).entry(thread.to_string()).or_default() += 1;
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
    q["_quickwork"]["offload_session_id"].as_str().filter(|s| !s.is_empty()).or_else(|| q["arguments"]["thread_id"].as_str()).unwrap_or("")
}

/// Who runs call `p` (its frame's payload) and shows it, for a turn of
/// `thread`.
pub(crate) fn owner(p: &Value, thread: &str) -> Owner {
    let of = call_thread(p);
    if of.is_empty() || of == thread {
        return Owner::Mine;
    }
    if LIVE.lock().unwrap_or_else(|e| e.into_inner()).contains_key(of) {
        return Owner::Theirs;
    }
    let id = p["tool_call_id"].as_str().unwrap_or("").to_string();
    if CLAIMED.lock().unwrap_or_else(|e| e.into_inner()).insert(id) { Owner::Orphan } else { Owner::Theirs }
}

/// Streams one Oak request through Quick's agent, sending its events to
/// `tx`. Quick keeps the thread's history itself, so only the newest user
/// turn goes; the tools it asks for are run in `body.folder`.
pub fn stream(body: Request, tx: Sender<Sse>) {
    let id = body.model.clone();
    let Some(m) = model(&id) else {
        return drop(tx.send(Sse::Err(format!("Quick doesn't serve {id}."))));
    };
    let folder = body.folder.clone().filter(|f| !f.is_empty()).map(std::path::PathBuf::from).or_else(|| std::env::current_dir().ok()).unwrap_or_default();
    // The thread is the conversation's, so Quick keeps its history there.
    let thread = body.conv.clone().filter(|c| !c.is_empty()).unwrap_or_else(hex_uuid);
    // Quick took in a message typed meanwhile and carries on: this request
    // reads on where the last one stopped, on the same socket. Anything said
    // since that Quick hasn't had goes to it mid-turn too.
    if let Some(p) = take_parked(&thread) {
        if let Some(text) = new_user_text(&body) {
            steer(&thread, 0, &text);
        }
        let Parked { sock, turn, calls, resends, live, folder, outpost, .. } = p;
        return drive(Drive { sock, turn, calls, resends, live, folder, outpost, thread, m, body }, tx);
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
        path = path.replace("&channel=prod", &format!("&replayContext={}&channel=prod", urlencode(&crate::quick::socket::replay_setup())));
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
    if let (Ok(tok), Ok(h)) = (crate::quick::login::access_token(), crate::quick::login::host())
        && let Some(f) = folder.to_str()
    {
        let _ = crate::quick::outpost::register_folder(&tok, &h, f);
    }
    let mut connect = base.clone();
    connect["actor_id"] = json!("");
    if let Some(o) = outpost.as_deref() {
        connect["outpostId"] = json!(o);
    }
    if sock.send(&json!({"type": "session_connect", "payload": connect})).is_err() {
        return drop(tx.send(Sse::Err("Quick closed the socket while connecting.".into())));
    }
    // The app connects to its home session first, federates its sign-in,
    // then joins the thread: Quick binds the machine's outpost to the session
    // that way, and without it the agent is told it has no local device.
    if sock.send(&json!({"type": "federate_status", "payload": {}})).is_err() {
        return drop(tx.send(Sse::Err("Quick closed the socket while federating.".into())));
    }
    let mut home = base.clone();
    home["session_id"] = json!("home");
    home["focus_only"] = json!(true);
    if sock.send(&json!({"type": "session_connect", "payload": home})).is_err() {
        return drop(tx.send(Sse::Err("Quick closed the socket while connecting home.".into())));
    }
    // This connection's workstreams are none (the app sends this before it
    // opens a thread), which is how Quick learns this socket carries none of
    // its own work and may be the courier for the outpost's tools.
    if sock.send(&json!({"type": "workstreams_summary", "payload": {"workstream_ids": []}})).is_err() {
        return drop(tx.send(Sse::Err("Quick closed the socket while summarising workstreams.".into())));
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
    if sock.send(&json!({"type": "session_connect", "payload": focus})).is_err() {
        return drop(tx.send(Sse::Err("Quick closed the socket while joining the thread.".into())));
    }
    // The thread's dock, as the app tells Quick it is: no files open, the
    // sidebar shut. Quick reads a thread with a courier only from a client
    // that says where it is in the thread.
    let dock = json!({
        "type": "dock_state_sync",
        "payload": {"session_id": thread, "files": [], "focused_file": "", "sidebar_open": false, "surfaceType": SURFACE}
    });
    if sock.send(&dock).is_err() {
        return drop(tx.send(Sse::Err("Quick closed the socket while syncing the dock.".into())));
    }
    // The socket naming the outpost is what marks it ONLINE, and only an
    // ONLINE outpost is handed this turn's tools; so the turn waits for Quick
    // to say so before it asks anything.
    if let Some(o) = outpost.as_deref() {
        crate::quick::outpost::await_online(&token, &host, o);
    }
    let user = user_message(&body, &thread, m, outpost.as_deref());
    if let Err(e) = sock.send(&user) {
        return drop(tx.send(Sse::Err(e)));
    }
    let turn = Turn::for_message(&thread, "");
    drive(Drive { sock, turn, calls: Calls::default(), resends: 0, live, folder, outpost, thread, m, body }, tx)
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
}

/// Reads the turn to its end, or until Quick takes in a message the user
/// typed (then the turn is parked for the next request, `PARKED`).
fn drive(d: Drive, tx: Sender<Sse>) {
    let Drive { mut sock, mut turn, mut calls, mut resends, live, folder, outpost, thread, m, body } = d;
    let say = |text: &str, id: &str| steer_message(&body, text, id, &thread, m, outpost.as_deref());
    loop {
        match pump(&mut sock, &folder, &mut turn, &mut calls, resends < RESEND_LIMIT, &say, &tx) {
            // The stream says the turn is over: the reply is whole.
            Ok(Pumped::Done) => return drop(tx.send(Sse::Done)),
            // Quick took in a message the user typed: this reply ends here,
            // and the next request reads on.
            Ok(Pumped::Steered) => {
                turn.resume();
                let p = Parked { sock, turn, calls, resends, live, folder, outpost, at: std::time::Instant::now() };
                PARKED.lock().unwrap_or_else(|e| e.into_inner()).insert(thread.clone(), p);
                return drop(tx.send(Sse::Done));
            }
            // Quick's relay recorded its own error for a call Oak had
            // answered, so Quick's agent read the error, not the result: Oak
            // gives it the real results, and its answer is this turn's too.
            Ok(Pumped::Lost(lost)) => {
                resends += 1;
                eprintln!("Quick's relay replaced {} tool result(s) with its own error; Oak is giving Quick the real ones", lost.len());
                for ev in turn.await_resend() {
                    if tx.send(Sse::Event(ev)).is_err() {
                        return;
                    }
                }
                let mut again = body.clone();
                again.messages = vec![json!({"role": "user", "content": [{"type": "text", "text": resend_text(&lost)}]})];
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

/// Reads Quick's frames until the message ends, running the tools it asks
/// for and answering each on the socket. A message that ends on a result the
/// relay lost comes back as `Pumped::Lost` when `may_resend` (Oak then gives
/// Quick the real results); otherwise it ends the turn as it is.
/// `say` makes the frame for a message the user typed meanwhile, which goes
/// once the turn has begun.
fn pump(sock: &mut Socket, folder: &std::path::Path, turn: &mut Turn, calls: &mut Calls, may_resend: bool, say: &dyn Fn(&str, &str) -> Value, tx: &Sender<Sse>) -> Result<Pumped, String> {
    let started = std::time::Instant::now();
    loop {
        if started.elapsed() > TURN_LIMIT {
            return Err("Quick's turn ran too long; it was given up on.".into());
        }
        let Some(frame) = sock.next() else {
            // The socket closed: a reply that ended is done; one that
            // didn't is an error.
            if !turn.started {
                return Err("Quick closed the connection before the reply began.".into());
            }
            return Ok(Pumped::Done);
        };
        // Messages typed meanwhile go between Quick's frames: Quick takes
        // them in at its agent's next step, so that is soon enough.
        if turn.started {
            send_steers(sock, &turn.thread, say)?;
        }
        let kind = frame["type"].as_str().unwrap_or("").to_string();
        // Another thread's frames, and older messages' of this one, are not
        // this turn's (the socket carries several threads).
        let mine = turn.accepts(&frame);
        if std::env::var("OAK_QUICK_DEBUG").is_ok() {
            eprintln!(
                "FRAME kind={kind:22} thread={:28} msg={:12} accepted={mine}",
                frame["thread_id"].as_str().unwrap_or("-"),
                frame["message_id"].as_str().unwrap_or("-")
            );
            if std::env::var("OAK_QUICK_DEBUG").as_deref() == Ok("2") {
                eprintln!("   {}", frame.to_string().chars().take(600).collect::<String>());
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
        let call = frame["payload"]["tool_call_id"].as_str().unwrap_or("").to_string();
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
        if !repeat && !foreign {
            for ev in turn.feed(&frame) {
                if tx.send(Sse::Event(ev)).is_err() {
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
                let args = if p["arguments"]["params"]["arguments"].is_null() { p["arguments"].clone() } else { p["arguments"]["params"]["arguments"].clone() };
                let key = format!("{name}{args}");
                // Quick asks again for a call Oak has answered when its relay
                // retries the delivery: the same answer goes again, and
                // nothing is run twice. Left unanswered, the relay gives up
                // on the call and records its own error for it.
                let mut finished = vec![];
                let reply = match calls.reply_for(&call, &key) {
                    Some(reply) => reply,
                    None => {
                        let result = run_tool(&name, &args, folder);
                        if std::env::var("OAK_QUICK_DEBUG").is_ok() {
                            eprintln!("TOOL {name} {args} -> {}", result.as_deref().map(|s| s.chars().take(300).collect::<String>()).unwrap_or_default());
                            if let Err(e) = &result {
                                eprintln!("TOOL {name} failed: {e}");
                            }
                        }
                        // The call carries a token for a transport its result
                        // may be offloaded on. Quick's app hands it to its
                        // outpost (`offload_transport_token`) and never sends
                        // it back with the result, so Oak doesn't either.
                        let transport = p["arguments"]["params"]["_quickwork"]["result_transport_token"].as_str().unwrap_or("");
                        let reply = tool_reply(&call, &result, transport);
                        calls.record(&call, &name, &key, &result, reply.clone());
                        if foreign {
                            // Another thread's call: answered, not shown here.
                            calls.mark_foreign(&call);
                        } else {
                            // The call's block ends with what it returned here.
                            finished = turn.finished_call(&call, &result);
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
                sock.send(&reply)?;
                send_steers(sock, &turn.thread, say)?;
                // Quick carries the turn on in a new message once it has the
                // result; that message is this turn's too.
                if !foreign {
                    turn.awaiting_continuation = true;
                }
                for ev in finished {
                    if tx.send(Sse::Event(ev)).is_err() {
                        return Ok(Pumped::Done);
                    }
                }
            }
            "steering_ingested" => {
                let p = &frame["payload"];
                if std::env::var("OAK_QUICK_DEBUG").is_ok() {
                    eprintln!("STEER Quick took in {} (fallback {})", p["message_ids"], p["fallback"]);
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
                let why = frame["payload"]["message"].as_str().or(frame["payload"]["error"].as_str()).unwrap_or("Quick reported an error").to_string();
                return Err(format!("Quick: {why}"));
            }
            _ => {}
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
        self.answered.iter().find(|a| a.call == call && a.key == key).map(|a| a.reply.clone())
    }

    fn record(&mut self, call: &str, name: &str, key: &str, result: &Result<String, String>, reply: Value) {
        let (content, ok) = match result {
            Ok(s) => (s.clone(), true),
            Err(e) => (e.clone(), false),
        };
        self.answered.push(Answer { call: call.into(), name: name.into(), key: key.into(), reply, content, ok, settled: false, foreign: false });
    }

    /// Quick's record of a call's result (a `tool_result` frame). It names
    /// the call by Quick's own id (`tool_id`), which needn't be the id the
    /// call came with, so a result is matched by id when it can be, and an
    /// error that says the relay failed goes with the newest call of its
    /// name. A call Oak answered that Quick records as failed is lost.
    fn saw_server_result(&mut self, p: &Value) {
        let id = ["tool_id", "tool_call_id", "tool_use_id", "id"].iter().find_map(|k| p[*k].as_str()).unwrap_or("");
        let name = p["tool_name"].as_str().or(p["name"].as_str()).unwrap_or("");
        let same_name = |a: &Answer| !a.foreign && (name.is_empty() || a.name == name);
        let error = server_error(p);
        let exact = self.answered.iter().position(|a| !id.is_empty() && a.call == id);
        let relay = error.as_deref().is_some_and(looks_like_relay);
        let at = if exact.is_some() {
            exact
        } else if relay {
            self.answered.iter().rposition(|a| !a.settled && same_name(a)).or_else(|| self.answered.iter().rposition(|a| same_name(a)))
        } else if !name.is_empty() {
            self.answered.iter().position(|a| !a.settled && a.name == name)
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
        self.lost.push(Lost { name: a.name.clone(), call: a.call.clone(), content: a.content.clone(), relay: said });
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
        failed.then(|| p["result"].as_str().map(String::from).unwrap_or_else(|| p["result"].to_string()))
    })
}

/// Whether an error is Quick's relay failing to reach the device (`SNCS
/// outpost invoke error (500)`), rather than a tool's own failure.
fn looks_like_relay(said: &str) -> bool {
    let s = said.to_lowercase();
    ["outpost", "relay", "invoke", "courier", "device"].iter().any(|w| s.contains(w))
}

/// The follow-up that gives Quick's agent the results its relay lost.
fn resend_text(lost: &[Lost]) -> String {
    let mut s = String::from(
        "(From Oak, the device that ran your tools.) Quick's relay replaced the result of the tool call below with its own error, but the call did run on this machine and succeeded. Its real result follows: use it to finish what I asked, without running the call again.\n",
    );
    for l in lost {
        let content: String = l.content.chars().take(40_000).collect();
        let relay: String = l.relay.chars().take(300).collect();
        s.push_str(&format!("\n{} (call {}); the relay said: {relay}\nResult:\n{content}\n", l.name, l.call));
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
        if long { format!("{} {:>12} {name}\n", if m.is_dir() { "d" } else { "-" }, m.len()) } else { format!("{name}\n") }
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
            .filter_map(|e| Some((e.file_name().to_string_lossy().into_owned(), e.metadata().ok()?)))
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
            let read = std::fs::read_dir(&path).map_err(|e| format!("Couldn't list {}: {e}", native(&path)))?;
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
            entries.sort_by(|a, b| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")));
            Ok(json!({
                "success": true,
                "path": format!("/{}", native(&path)),
                "entries": entries,
                "count": entries.len(),
            })
            .to_string())
        }
        "fdfind" | "fd" => {
            let pattern = args["args"].as_str().unwrap_or("").to_string();
            let found = find(folder, &pattern);
            Ok(json!({"success": true, "output": found}).to_string())
        }
        "ripgrep" | "rg" | "grep" => {
            let pat = args["patterns"].as_array().and_then(|a| a.first()).and_then(Value::as_str).unwrap_or("").to_string();
            let found = grep(folder, &pat, args["options"].as_str().unwrap_or(""));
            Ok(json!({"success": true, "output": found}).to_string())
        }
        "bash" | "shell" | "run_command" | "execute_command" => {
            let cmd = args["command"].as_str().or(args["cmd"].as_str()).unwrap_or("").to_string();
            execute(&json!({"command": cmd}), folder)
        }
        // Quick's own coding tool: Python in the conversation's folder, which
        // is how its agent runs shell commands (`subprocess`) too.
        "run_python" | "python" | "run_python_outpost_local" | "run_python_outpost_local_with_write" => run_python(args, folder),
        // The names the outpost declares (`outpost::TOOLS`), so a call made
        // against this machine's registration runs here too.
        "file_matches" => {
            let want = args["path"].as_str().unwrap_or("");
            Ok(json!({"matches": match_paths(folder, want), "outside_allowed_folders": false}).to_string())
        }
        "folder_create" => {
            let p = resolve(folder, args["path"].as_str().unwrap_or(""));
            std::fs::create_dir_all(&p).map(|_| format!("Created {}.", native(&p))).map_err(|e| format!("Couldn't create {}: {e}", native(&p)))
        }
        "file_delete" => {
            let p = resolve(folder, args["path"].as_str().unwrap_or(""));
            if p.is_dir() { std::fs::remove_dir(&p) } else { std::fs::remove_file(&p) }
                .map(|_| format!("Deleted {}.", native(&p)))
                .map_err(|e| format!("Couldn't delete {}: {e}", native(&p)))
        }
        "file_copy" => {
            let (from, to) = (resolve(folder, args["source"].as_str().unwrap_or("")), resolve(folder, args["destination"].as_str().unwrap_or("")));
            std::fs::copy(&from, &to).map(|_| format!("Copied {} to {}.", native(&from), native(&to))).map_err(|e| format!("Couldn't copy: {e}"))
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
            let cmd = args["command"].as_str().or(args["cmd"].as_str()).unwrap_or("").to_string();
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
            // The drive comes back with a leading slash (`/C:/…`).
            without_device.trim_start_matches('/').to_string()
        }
        None => p.to_string(),
    };
    // Quick's own workspace roots are the conversation's folder.
    let bare = real.trim_start_matches("workspace/").trim_end_matches('/');
    let roots = ["artifacts", "agent_files", "attached_files"];
    if bare.is_empty() || roots.contains(&bare) || roots.iter().any(|r| bare.starts_with(&format!("{r}/"))) {
        let tail = bare.split_once('/').map(|(_, t)| t).unwrap_or("");
        return if tail.is_empty() { folder.to_path_buf() } else { folder.join(tail) };
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
    (c.next().is_some_and(|x| x.is_ascii_alphabetic()) && c.next() == Some(':')).then(|| std::path::PathBuf::from(t))
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
        return if path.exists() { vec![native(&path)] } else { vec![] };
    }
    let needle = want.trim_start_matches("./").replace('\\', "/").to_lowercase();
    let mut out = vec![];
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
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

/// Files whose name matches `pattern` (fdfind's `args`, e.g. `-i -H pong`),
/// from the conversation's folder down.
pub(crate) fn find(folder: &std::path::Path, pattern: &str) -> String {
    let needle = pattern.split_whitespace().filter(|w| !w.starts_with('-')).collect::<Vec<_>>().join(" ").to_lowercase();
    let mut out = vec![];
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for e in read.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_lowercase();
            if matches!(name.as_str(), ".git" | "node_modules" | "target" | ".oak") {
                continue;
            }
            if p.is_dir() {
                stack.push(p.clone());
            }
            if needle.is_empty() || name.contains(&needle) {
                out.push(native(&p));
                if out.len() >= 200 {
                    out.sort();
                    return out.join("\n");
                }
            }
        }
    }
    out.sort();
    out.join("\n")
}

/// Lines matching the first of ripgrep's `patterns`, from the conversation's
/// folder down (`options` may say `-i` for case-insensitive).
fn grep(folder: &std::path::Path, pattern: &str, options: &str) -> String {
    if pattern.is_empty() {
        return String::new();
    }
    let insensitive = options.split_whitespace().any(|o| o.starts_with('-') && !o.starts_with("--") && o[1..].contains('i'));
    let needle = if insensitive { pattern.to_lowercase() } else { pattern.to_string() };
    let mut out = vec![];
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for e in read.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if matches!(name.as_str(), ".git" | "node_modules" | "target" | ".oak") {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            // A few megabytes at most: a search of the project, not of data.
            let Ok(meta) = e.metadata() else { continue };
            if meta.len() > 2_000_000 {
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else { continue };
            if bytes.contains(&0) {
                continue; // binary
            }
            let text = String::from_utf8_lossy(&bytes);
            for (n, line) in text.lines().enumerate() {
                let hit = if insensitive { line.to_lowercase().contains(&needle) } else { line.contains(&needle) };
                if hit {
                    out.push(format!("{}:{}: {}", native(&p), n + 1, line.trim_end()));
                    if out.len() >= 200 {
                        return out.join("\n");
                    }
                }
            }
        }
    }
    out.join("\n")
}

/// Runs a shell command in `folder`, with the output clipped for a tool
/// result.
fn execute(input: &Value, folder: &std::path::Path) -> Result<String, String> {    let command = input["command"].as_str().unwrap_or("").trim();
    if command.is_empty() {
        return Err("EXECUTE needs a `command`.".into());
    }
    // Windows' shell has no `ls`, and Quick's agent lists folders with it.
    if cfg!(windows)
        && let Some(listing) = unix_ls(command, folder)
    {
        return listing;
    }
    let timeout = input["timeout"].as_u64().unwrap_or(120).min(crate::tools::EXECUTE_TIMEOUT_MAX.max(1));
    #[cfg(windows)]
    let spawned = {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("cmd")
            .arg("/C")
            .arg(command)
            .current_dir(folder)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .creation_flags(0x0800_0000)
            .spawn()
    };
    #[cfg(not(windows))]
    let spawned: std::io::Result<std::process::Child> = std::process::Command::new("bash").arg("-lc").arg(command).current_dir(folder).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn();
    let mut child = spawned.map_err(|e| format!("Couldn't run `{command}`: {e}"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                return Err(format!("`{command}` ran past its {timeout}s."));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => return Err(format!("`{command}` failed: {e}")),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut text = stdout.into_owned();
    if text.trim().is_empty() {
        text = stderr.into_owned();
    } else if !stderr.trim().is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&stderr);
    }
    if text.trim().is_empty() {
        text = format!("`{command}` finished with {}.", out.status);
    }
    Ok(crate::tools::clip(&text))
}

/// Runs a Python snippet in `folder` (Quick's own coding tool: its agent
/// uses it for shell work too), with the output clipped for a tool result.
fn run_python(args: &Value, folder: &std::path::Path) -> Result<String, String> {
    let code = args["code"].as_str().or(args["script"].as_str()).or(args["source"].as_str()).unwrap_or("");
    if code.trim().is_empty() {
        return Err("run_python needs `code`.".into());
    }
    // The snippet goes beside Oak's scratchpad, so the working folder's own
    // files aren't touched.
    let dir = folder.join(".oak").join("tmp");
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("quick-{}.py", crate::api::now_ms()));
    std::fs::write(&file, code).map_err(|e| format!("Couldn't write the snippet: {e}"))?;
    let mut python = python_command();
    python.arg(&file).current_dir(folder).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = python.spawn().map_err(|e| format!("Couldn't start Python: {e}"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                return Err("The snippet ran past its 120s.".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => return Err(format!("Python failed: {e}")),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.trim().is_empty() {
        if !text.trim().is_empty() {
            text.push_str("\n[stderr]\n");
        }
        text.push_str(&stderr);
    }
    if text.trim().is_empty() {
        text = format!("The snippet finished and printed nothing ({}).", out.status);
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
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
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
        assert_eq!(resolve(folder, "src/main.rs"), std::path::PathBuf::from("C:/work/project/src/main.rs"));
        assert_eq!(resolve(folder, "/C:/work/project/src/main.rs"), std::path::PathBuf::from("C:/work/project/src/main.rs"));
        assert_eq!(resolve(folder, "C:/work/project/src/main.rs"), std::path::PathBuf::from("C:/work/project/src/main.rs"));
        assert_eq!(resolve(folder, "/outposts/DESKTOP/C:/work/project/src/main.rs"), std::path::PathBuf::from("C:/work/project/src/main.rs"));
        assert_eq!(resolve(folder, "/outposts/DESKTOP//C:/work/project/src/main.rs"), std::path::PathBuf::from("C:/work/project/src/main.rs"));
        assert_eq!(resolve(folder, "artifacts/out.txt"), std::path::PathBuf::from("C:/work/project/out.txt"));
        assert_eq!(resolve(folder, "/C:/work/project"), std::path::PathBuf::from("C:/work/project"));
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
        assert_eq!(match_paths(&dir, "src/quick/turn.rs").len(), 1, "a relative path matches by its tail");
        assert_eq!(match_paths(&dir, "turn.rs").len(), 1, "a name matches by name");
        assert!(match_paths(&dir, "nothing-here.txt").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A result goes to Quick as its app sends one: an object, `error` empty
    /// (a tool that failed is a result that says so), and no transport token
    /// (an echoed token is a result Quick's relay looks for elsewhere).
    #[test]
    fn a_tool_result_is_shaped_as_quicks_app_sends_it() {
        let text = tool_reply("c1", &Ok("C:/x/turn.rs: file, 42 bytes.".into()), "0123456789abcdef0123456789abcdef");
        assert_eq!(text["type"], "tool_result");
        assert_eq!(text["payload"]["tool_call_id"], "c1");
        assert!(text["payload"]["result"].is_object(), "a bare string is a result Quick can't read: {text}");
        assert_eq!(text["payload"]["result"]["output"], "C:/x/turn.rs: file, 42 bytes.");
        assert!(text["payload"]["error"].is_null());
        assert!(text["payload"].get("result_transport_token").is_none(), "the token isn't sent back: {text}");
        let listing = tool_reply("c2", &Ok(r#"{"success":true,"entries":[]}"#.into()), "");
        assert_eq!(listing["payload"]["result"]["success"], true);
        let failed = tool_reply("c3", &Err("no such file".into()), "");
        assert_eq!(failed["payload"]["result"]["success"], false);
        assert_eq!(failed["payload"]["result"]["error"], "no such file");
        assert!(failed["payload"]["error"].is_null(), "`error` is the relay's, not the tool's");
    }

    /// Quick's relay records its own error for a call Oak answered: that
    /// answer is lost, and kept to be given back. Quick names the call by its
    /// own id, so the error goes with the newest call of its name.
    #[test]
    fn a_result_the_relay_replaced_is_known_as_lost() {
        let mut calls = Calls::default();
        let ok = Ok(json!({"success": true, "size_bytes": 34711}).to_string());
        calls.record("call-1", "file_type", "file_type{}", &ok, tool_reply("call-1", &ok, ""));
        // Another tool's own failure isn't one of Oak's results lost.
        calls.saw_server_result(&json!({"tool_id": "tooluse_x", "tool_name": "web_search", "error": "rate limited"}));
        assert!(calls.lost.is_empty());
        calls.saw_server_result(&json!({
            "tool_id": "tooluse_abc", "tool_name": "file_type", "status": "error",
            "error": "[{'text': 'Error: RuntimeError - SNCS outpost invoke error (500)'}]"
        }));
        assert_eq!(calls.lost.len(), 1);
        assert_eq!(calls.lost[0].call, "call-1");
        assert!(calls.lost[0].content.contains("34711"));
        let text = resend_text(&calls.lost);
        assert!(text.contains("34711") && text.contains("SNCS outpost invoke error"), "{text}");
        // Given back once: the same error again doesn't lose it twice.
        calls.resent.push("call-1".into());
        calls.lost.clear();
        calls.saw_server_result(&json!({"tool_id": "call-1", "error": "SNCS outpost invoke error (500)"}));
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
        assert!(took_in(thread, &json!({"message_ids": [id], "messages": [{"content": "NO TESTING"}]})));
        assert_eq!(take_ingested(thread), vec![7]);
        assert!(take_ingested(thread).is_empty());
    }

    #[test]
    fn a_call_that_failed_here_is_not_given_back() {
        let mut calls = Calls::default();
        let failed = Err("no such file".to_string());
        calls.record("call-2", "file_type", "k", &failed, tool_reply("call-2", &failed, ""));
        calls.saw_server_result(&json!({"tool_id": "call-2", "error": "SNCS outpost invoke error (500)"}));
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
        assert!(matches!(owner(&call("x1", "oak-test-thread-a"), "oak-test-thread-a"), Owner::Mine));
        assert!(matches!(owner(&call("x2", "oak-test-thread-b"), "oak-test-thread-a"), Owner::Theirs), "a live thread's call is its turn's");
        assert!(matches!(owner(&call("x3", ""), "oak-test-thread-a"), Owner::Mine), "a call naming no thread is the turn's");
        assert!(matches!(owner(&call("x4", "oak-test-thread-gone"), "oak-test-thread-a"), Owner::Orphan));
        assert!(matches!(owner(&call("x4", "oak-test-thread-gone"), "oak-test-thread-b"), Owner::Theirs), "an orphan is run once");
        drop(_b);
        assert!(matches!(owner(&call("x5", "oak-test-thread-b"), "oak-test-thread-a"), Owner::Orphan), "a turn that ended reads its thread no more");
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
        assert!(ls.contains("a.txt") && ls.contains("sub") && !ls.contains(".hidden"), "{ls}");
        let la = unix_ls("ls -la sub a.txt", &dir).unwrap().unwrap();
        assert!(la.contains("a.txt") && la.contains("sub:"), "{la}");
        assert!(unix_ls("ls | sort", &dir).is_none(), "a pipe goes to the shell");
        assert!(unix_ls("ls missing", &dir).unwrap().is_err());
        let listed: Value = serde_json::from_str(&run_tool("folder_list", &json!({"path": native(&dir)}), &dir).unwrap()).unwrap();
        let names: Vec<&str> = listed["entries"].as_array().unwrap().iter().filter_map(|e| e["name"].as_str()).collect();
        assert!(names.contains(&"a.txt") && names.contains(&"sub"), "{listed}");
        assert!(!listed["path"].as_str().unwrap().starts_with("/outposts/"), "paths go in the wire form: {listed}");
        assert!(run_tool("folder_list", &json!({"path": native(&dir.join("missing"))}), &dir).is_err(), "a missing folder isn't an empty one");
        let kind: Value = serde_json::from_str(&run_tool("file_type", &json!({"path": format!("/{}", native(&dir.join("a.txt")))}), &dir).unwrap()).unwrap();
        assert_eq!(kind["size_bytes"], 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
