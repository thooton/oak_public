//! Private Chromium for native plugin forms. Observe accessibility labels;
//! deliver input through CDP, without executing script in the page.

use super::{Cdp, Instance, OPEN, Status, close, egui, native_input, open_isolated};
use oak_sdk::browser::{Address, Command, Node, Screen};
use serde_json::{Value, json};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

const PREFIX: &str = ":background-browser:";
const CLOSED: &str = "The background browser is closed.";
const CHANGED: &str = "The browser page changed before input could be sent.";
const ACTION_FAILED: &str = "Chromium couldn't complete the requested browser action.";

pub(crate) struct Lease {
    pub(crate) owner: String,
    handle: String,
}

impl Lease {
    pub(crate) fn new(ctx: &egui::Context, owner: String, url: String) -> Self {
        let handle = open_isolated(ctx, url, PREFIX).conv.clone();
        Self { owner, handle }
    }

    pub(crate) fn handle(&self) -> &str {
        &self.handle
    }

    /// The running browser, for the page that owns it to watch.
    pub(crate) fn instance(&self) -> Option<std::sync::Arc<Instance>> {
        OPEN.lock().unwrap().get(&self.handle).filter(|inst| inst.is_private()).cloned()
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        close(&self.handle);
    }
}

pub(crate) fn command(handle: &str, command: Command) -> Result<String, String> {
    if !handle.starts_with(PREFIX) {
        return Err(CLOSED.into());
    }
    if matches!(&command, Command::Close) {
        close(handle);
        return Ok(String::new());
    }
    let inst = OPEN
        .lock()
        .unwrap()
        .get(handle)
        .filter(|inst| inst.is_private())
        .cloned()
        .ok_or(CLOSED)?;
    // Only this browser's worker should send input. Never record its commands.
    let status = inst.view.lock().unwrap().status.clone();
    let page =
        match status {
            Status::Starting if matches!(&command, Command::Observe) => {
                return encode(&Screen {
                    address: Address {
                        origin: String::new(),
                        path: String::new(),
                        document: String::new(),
                    },
                    nodes: Vec::new(),
                });
            }
            Status::Ready => inst.page().ok_or(CLOSED)?,
            Status::Failed(_) | Status::Quit(_) => return Err(
                "Chromium stopped or couldn't start. Check that a supported browser is installed."
                    .into(),
            ),
            Status::Starting => return Err(ACTION_FAILED.into()),
        };
    if inst.view.lock().unwrap().dialog.is_some() {
        return Err(
            "The website opened a browser dialog. Continue in your default browser.".into(),
        );
    }
    match command {
        Command::Observe => observe(&inst, &page).and_then(|s| encode(&s)),
        Command::Fill { screen, node, text } => {
            fill(&inst, &page, &screen, node, &text).map(|()| String::new())
        }
        Command::Click { screen, node } => {
            click(&inst, &page, &screen, node).map(|()| String::new())
        }
        Command::Navigate { url } => navigate(&inst, &page, &url).map(|()| String::new()),
        Command::Close => unreachable!(),
    }
}

/// Loads `url` in the session's page, as the address bar would: HTTPS
/// without credentials only, like the URL a session is opened on.
fn navigate(inst: &Instance, page: &Cdp, url: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(url)
        .ok()
        .filter(|u| u.scheme() == "https" && u.has_host() && u.username().is_empty() && u.password().is_none())
        .ok_or("A background browser only goes to HTTPS URLs without credentials.")?;
    let r = call(inst, page, "Page.navigate", json!({"url": url.as_str()}))?;
    match r["errorText"].as_str().filter(|e| !e.is_empty()) {
        Some(e) => Err(format!("Chromium couldn't load the page ({e}).")),
        None => Ok(()),
    }
}

fn encode(screen: &Screen) -> Result<String, String> {
    serde_json::to_string(screen).map_err(|_| ACTION_FAILED.into())
}

fn call(inst: &Instance, page: &Cdp, method: &str, params: Value) -> Result<Value, String> {
    if inst.is_closed() {
        return Err(CLOSED.into());
    }
    page.call_until(method, params, Duration::from_secs(4), &|| {
        inst.is_closed().then(|| CLOSED.into())
    })
    .map_err(|_| {
        if inst.is_closed() {
            CLOSED
        } else {
            ACTION_FAILED
        }
        .into()
    })
}

fn address(inst: &Instance, page: &Cdp) -> Result<Address, String> {
    let tree = call(inst, page, "Page.getFrameTree", json!({}))?;
    let frame = &tree["frameTree"]["frame"];
    let url =
        reqwest::Url::parse(frame["url"].as_str().unwrap_or("")).map_err(|_| ACTION_FAILED)?;
    // Include same-document route changes in an opaque identity, not in the
    // observed URL: query strings and fragments can have authentication state.
    let mut key = DefaultHasher::new();
    frame["loaderId"].as_str().unwrap_or("").hash(&mut key);
    url.as_str().hash(&mut key);
    frame["urlFragment"].as_str().hash(&mut key);
    Ok(Address {
        origin: url.origin().ascii_serialization(),
        path: url.path().to_string(),
        document: format!("{:016x}", key.finish()),
    })
}

fn require_document(inst: &Instance, page: &Cdp, want: &Address) -> Result<(), String> {
    let current = address(inst, page)?;
    if !current.origin.starts_with("https://") || current.document.is_empty() || &current != want {
        return Err(CHANGED.into());
    }
    Ok(())
}

fn prop<'a>(node: &'a Value, name: &str) -> Option<&'a Value> {
    node["properties"]
        .as_array()?
        .iter()
        .find(|p| p["name"].as_str() == Some(name))
        .map(|p| &p["value"]["value"])
}

fn observe(inst: &Instance, page: &Cdp) -> Result<Screen, String> {
    let before = address(inst, page)?;
    if before.origin == "null" {
        return Ok(Screen {
            address: before,
            nodes: Vec::new(),
        });
    }
    let tree = call(inst, page, "Accessibility.getFullAXTree", json!({}))?;
    let mut nodes: Vec<Node> = tree["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| {
            if node["ignored"] == true || prop(node, "hidden") == Some(&Value::Bool(true)) {
                return None;
            }
            let role = node["role"]["value"].as_str()?;
            if !matches!(
                role,
                "button"
                    | "link"
                    | "heading"
                    | "textbox"
                    | "spinbutton"
                    | "alert"
                    | "dialog"
                    | "StaticText"
                    | "Iframe"
            ) {
                return None;
            }
            let label: String = node["name"]["value"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(240)
                .collect();
            let id = node["backendDOMNodeId"].as_u64().unwrap_or(0);
            Some(Node {
                id,
                role: role.to_string(),
                label,
                disabled: prop(node, "disabled") == Some(&Value::Bool(true)),
                invalid: prop(node, "invalid")
                    .is_some_and(|v| v == &Value::Bool(true) || v.as_str() == Some("true")),
                busy: prop(node, "busy") == Some(&Value::Bool(true)),
                input_type: None,
                autocomplete: None,
                test_id: None,
            })
        })
        .take(500)
        .collect();
    for n in nodes
        .iter_mut()
        .filter(|n| n.id != 0 && matches!(n.role.as_str(), "textbox" | "spinbutton" | "button"))
        .take(40)
    {
        let Ok(dom) = call(
            inst,
            page,
            "DOM.describeNode",
            json!({"backendNodeId": n.id}),
        ) else {
            continue;
        };
        let Some(attrs) = dom["node"]["attributes"].as_array() else {
            continue;
        };
        for pair in attrs.chunks_exact(2) {
            let val = || {
                pair[1]
                    .as_str()
                    .map(|s| s.chars().take(120).collect::<String>())
            };
            match pair[0].as_str() {
                Some("type") => n.input_type = val(),
                Some("autocomplete") => n.autocomplete = val(),
                Some("data-testid" | "data-id") if n.test_id.is_none() => n.test_id = val(),
                Some("readonly") => n.disabled = true,
                _ => {}
            }
        }
    }
    if address(inst, page)? != before {
        return Err(CHANGED.into());
    }
    Ok(Screen {
        address: before,
        nodes,
    })
}

fn fill(
    inst: &Instance,
    page: &Cdp,
    screen: &Address,
    node: u64,
    text: &str,
) -> Result<(), String> {
    require_document(inst, page, screen)?;
    let native = |method: &str, params| call(inst, page, method, params);
    native_input::focus(&native, node)?;
    // Make sure Chromium focused this exact node, not another field or an iframe.
    let target = call(
        inst,
        page,
        "Accessibility.getPartialAXTree",
        json!({"backendNodeId": node, "fetchRelatives": false}),
    )?;
    if !target["nodes"].as_array().into_iter().flatten().any(|n| {
        n["backendDOMNodeId"].as_u64() == Some(node)
            && matches!(n["role"]["value"].as_str(), Some("textbox" | "spinbutton"))
            && prop(n, "focused") == Some(&Value::Bool(true))
    }) {
        return Err("The AWS form field did not take focus.".into());
    }
    native_input::select_all(&native)?;
    require_document(inst, page, screen)?;
    native_input::insert(&native, text)
}

fn click(inst: &Instance, page: &Cdp, screen: &Address, node: u64) -> Result<(), String> {
    require_document(inst, page, screen)?;
    let target = json!({"backendNodeId": node});
    call(inst, page, "DOM.scrollIntoViewIfNeeded", target.clone())?;
    let bounds = call(inst, page, "DOM.getContentQuads", target)?;
    let quad = bounds["quads"]
        .as_array()
        .and_then(|q| q.first())
        .and_then(Value::as_array)
        .filter(|q| q.len() == 8)
        .ok_or(ACTION_FAILED)?;
    let (w, h) = inst.viewport_css();
    let center = |axis: usize, limit: u32| -> Result<f64, String> {
        let coords: Vec<_> = quad
            .iter()
            .skip(axis)
            .step_by(2)
            .filter_map(Value::as_f64)
            .collect();
        let min = coords
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min)
            .max(0.0);
        let max = coords
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
            .min(f64::from(limit));
        (coords.len() == 4 && min < max)
            .then_some((min + max) / 2.0)
            .ok_or(ACTION_FAILED.into())
    };
    let (x, y) = (center(0, w)?, center(1, h)?);
    require_document(inst, page, screen)?;
    let native = |method: &str, params| call(inst, page, method, params);
    native(
        "Input.dispatchMouseEvent",
        json!({"type": "mouseMoved", "x": x, "y": y, "button": "none", "buttons": 0}),
    )?;
    native_input::click_buttons(&native, x, y, "left", 1)
}
