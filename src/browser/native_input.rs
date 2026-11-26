//! Native Chromium input shared by BROWSER and plugin background sessions.

use serde_json::{Value, json};

type Call<'a> = dyn Fn(&str, Value) -> Result<Value, String> + 'a;

pub(super) fn focus(call: &Call<'_>, node: u64) -> Result<(), String> {
    call("DOM.focus", json!({"backendNodeId": node}))?;
    Ok(())
}

pub(super) fn select_all(call: &Call<'_>) -> Result<(), String> {
    let modifiers = if cfg!(target_os = "macos") { 4 } else { 2 };
    call(
        "Input.dispatchKeyEvent",
        json!({"type": "rawKeyDown", "key": "a", "code": "KeyA", "windowsVirtualKeyCode": 65, "modifiers": modifiers, "commands": ["selectAll"]}),
    )?;
    call(
        "Input.dispatchKeyEvent",
        json!({"type": "keyUp", "key": "a", "code": "KeyA", "windowsVirtualKeyCode": 65, "modifiers": modifiers}),
    )?;
    Ok(())
}

pub(super) fn insert(call: &Call<'_>, text: &str) -> Result<(), String> {
    call("Input.insertText", json!({"text": text}))?;
    Ok(())
}

pub(super) fn click_buttons(
    call: &Call<'_>,
    x: f64,
    y: f64,
    button: &str,
    clicks: i64,
) -> Result<(), String> {
    let bit = match button {
        "right" => 2,
        "middle" => 4,
        _ => 1,
    };
    for n in 1..=clicks {
        call(
            "Input.dispatchMouseEvent",
            json!({"type": "mousePressed", "x": x, "y": y, "button": button, "buttons": bit, "clickCount": n}),
        )?;
        call(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseReleased", "x": x, "y": y, "button": button, "buttons": 0, "clickCount": n}),
        )?;
    }
    Ok(())
}
