//! Oak's own Blender MCP server ("blender"): the tools of the community
//! Blender MCP server that Claude Code and Codex users run
//! (github.com/ahujasid/mcp-for-blender, as models know it: get_scene_info,
//! get_object_info, get_viewport_screenshot, execute_blender_code and the
//! Poly Haven tools), built into Oak. It runs in Oak's process for Oak's
//! conversations (`mcp::Builtin`), and `oak --mcp blender` serves it over
//! stdio to any MCP client (`serve_stdio`).
//!
//! It reaches Blender through an add-on's socket on localhost:9876
//! (`BLENDER_HOST`, `BLENDER_PORT`): Oak's (`assets/blender/oak_mcp.py`,
//! installed by `install_addon`) or the community one, which speak the same
//! protocol, one JSON command and its JSON answer per connection. Every
//! tool is Python sent as `execute_code` (scripts below print their answer
//! as JSON after `MARK`), so both add-ons serve all of them. Poly Haven's
//! files are downloaded here, then imported by a script, so Blender itself
//! needs no network access.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The server's name: its kit and its tools' prefix (`mcp__blender__`).
pub const NAME: &str = "blender";

/// The add-on Oak installs into Blender.
pub const ADDON: &str = include_str!("../../assets/blender/oak_mcp.py");
/// Its module name (the file is `oak_mcp.py`).
pub const ADDON_MODULE: &str = "oak_mcp";

/// What a script prints before its JSON answer.
const MARK: &str = "@@OAK_MCP@@";

/// How long a command may take in Blender (a render, a simulation bake).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30 * 60);

const POLYHAVEN: &str = "https://api.polyhaven.com";
const USER_AGENT: &str = concat!("oak-blender-mcp/", env!("CARGO_PKG_VERSION"));

/// What the model is told with the kit (MCP's `instructions`).
pub const INSTRUCTIONS: &str = "Works in the user's running Blender, live: changes show in their window at once and stay in the open file until they save it (save only when asked). \
Before changing anything, look at the scene with get_scene_info, and take get_viewport_screenshot before and after changes to check the result visually. \
Do the work with execute_blender_code (bpy, bmesh, mathutils) in small steps, printing what you need to know; a failing script returns its traceback. \
For assets, Poly Haven (free, CC0) has HDRIs for environment lighting, textures for materials (download_polyhaven_asset, then set_texture) and generic models; prefer them to modelling everything from primitives, unless a primitive is asked for or nothing suitable exists. \
After importing or creating objects, check each one's world_bounding_box (get_object_info) and fix location, rotation and scale so objects sit where they should and don't clip into each other.";

/// The tools, as MCP lists them.
pub fn tools() -> Value {
    let tool = |name: &str, description: &str, properties: Value, required: &[&str]| {
        json!({"name": name, "description": description, "inputSchema": {"type": "object", "properties": properties, "required": required}})
    };
    json!([
        tool("get_scene_info", "Get detailed information about the current Blender scene: the file, frame range, render settings, collections, and each object's name, type, world location, rotation, scale, dimensions, parent, visibility and materials.",
            json!({"limit": {"type": "integer", "description": "Most objects to list (default 100)"}}), &[]),
        tool("get_object_info", "Get detailed information about a specific object in the Blender scene: transform, world_bounding_box, dimensions, parent and children, collections, materials, modifiers, constraints, mesh statistics, custom properties and animation.",
            json!({"object_name": {"type": "string", "description": "The name of the object to get information about"}}), &["object_name"]),
        tool("get_viewport_screenshot", "Capture a screenshot of the current Blender 3D viewport.",
            json!({"max_size": {"type": "integer", "description": "Maximum size in pixels for the largest dimension (default: 800)"}}), &[]),
        tool("execute_blender_code", "Execute arbitrary Python code in Blender. Make sure to do it step-by-step by breaking it into smaller chunks. Whatever the code prints is returned; an error returns its traceback.",
            json!({"code": {"type": "string", "description": "The Python code to execute"}}), &["code"]),
        tool("get_polyhaven_status", "Check if PolyHaven integration is enabled in Blender. Returns a message indicating whether PolyHaven features are available.", json!({}), &[]),
        tool("get_polyhaven_categories", "Get a list of categories for a specific asset type on Polyhaven.",
            json!({"asset_type": {"type": "string", "enum": ["hdris", "textures", "models", "all"], "description": "The type of asset to get categories for (hdris, textures, models, all)"}}), &[]),
        tool("search_polyhaven_assets", "Search for assets on Polyhaven with optional filtering. Returns a list of matching assets with basic information.",
            json!({"asset_type": {"type": "string", "enum": ["hdris", "textures", "models", "all"], "description": "Type of assets to search for (hdris, textures, models, all)"},
                   "categories": {"type": "string", "description": "Optional comma-separated list of categories to filter by"}}), &[]),
        tool("download_polyhaven_asset", "Download and import a Polyhaven asset into Blender. HDRIs become the world's lighting, textures become a material named after the asset (apply it with set_texture), models are imported into the scene.",
            json!({"asset_id": {"type": "string", "description": "The ID of the asset to download"},
                   "asset_type": {"type": "string", "enum": ["hdris", "textures", "models"], "description": "The type of asset (hdris, textures, models)"},
                   "resolution": {"type": "string", "description": "The resolution to download (e.g., 1k, 2k, 4k)"},
                   "file_format": {"type": "string", "description": "Optional file format (e.g., hdr, exr for HDRIs; jpg, png for textures; gltf, fbx for models)"}}), &["asset_id", "asset_type"]),
        tool("set_texture", "Apply a previously downloaded Polyhaven texture to an object.",
            json!({"object_name": {"type": "string", "description": "Name of the object to apply the texture to"},
                   "texture_id": {"type": "string", "description": "ID of the Polyhaven texture to apply (must be downloaded first)"}}), &["object_name", "texture_id"]),
    ])
}

/// Answers an MCP request: Ok(result) or Err((code, message)).
pub fn handle(method: &str, params: &Value) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(super::PROTOCOL),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "oak-blender", "title": "Blender (Oak)", "version": crate::update::VERSION},
            "instructions": INSTRUCTIONS,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => Ok(call(params["name"].as_str().unwrap_or(""), params.get("arguments").unwrap_or(&Value::Null))),
        "resources/list" => Ok(json!({"resources": []})),
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "prompts/list" => Ok(json!({"prompts": []})),
        _ => Err((-32601, format!("Method not found: {method}"))),
    }
}

/// `oak --mcp blender`: the server over stdio, one JSON-RPC message per
/// line, until stdin closes.
pub fn serve_stdio() -> i32 {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        match std::io::BufRead::read_line(&mut stdin.lock(), &mut line) {
            Ok(0) | Err(_) => return 0,
            Ok(_) => {}
        }
        let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
            if !line.trim().is_empty() {
                let _ = writeln!(out, "{}", json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}}));
                let _ = out.flush();
            }
            continue;
        };
        let (Some(id), Some(method)) = (msg.get("id").filter(|i| !i.is_null()), msg["method"].as_str()) else { continue };
        let reply = match handle(method, &msg["params"]) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        };
        let _ = writeln!(out, "{reply}");
        let _ = out.flush();
    }
}

/// A tool call's MCP result.
fn call(name: &str, args: &Value) -> Value {
    let text = |s: String, error: bool| json!({"content": [{"type": "text", "text": s}], "isError": error});
    let s = |k: &str| args[k].as_str().map(str::trim).filter(|v| !v.is_empty());
    let r = match name {
        "get_scene_info" => script(&with_params(SCENE_INFO, &json!({"limit": args["limit"].as_u64().unwrap_or(100)}))).map(|v| pretty(&v)),
        "get_object_info" => match s("object_name") {
            Some(n) => script(&with_params(OBJECT_INFO, &json!({"name": n}))).map(|v| pretty(&v)),
            None => Err("Give `object_name`.".into()),
        },
        "get_viewport_screenshot" => return screenshot(args["max_size"].as_u64().unwrap_or(800).clamp(64, 1568) as u32),
        "execute_blender_code" => match args["code"].as_str() {
            Some(code) => execute(code).map(|out| format!("Code executed successfully: {out}")),
            None => Err("Give `code`.".into()),
        },
        "get_polyhaven_status" => Ok("PolyHaven integration is enabled and ready to use.".into()),
        "get_polyhaven_categories" => polyhaven_categories(s("asset_type").unwrap_or("hdris")),
        "search_polyhaven_assets" => polyhaven_search(s("asset_type").unwrap_or("all"), s("categories")),
        "download_polyhaven_asset" => match (s("asset_id"), s("asset_type")) {
            (Some(id), Some(kind)) => polyhaven_download(id, kind, s("resolution").unwrap_or("1k"), s("file_format")),
            _ => Err("Give `asset_id` and `asset_type`.".into()),
        },
        "set_texture" => match (s("object_name"), s("texture_id")) {
            (Some(o), Some(t)) => script(&with_params(SET_TEXTURE, &json!({"object": o, "texture": t}))).map(|v| pretty(&v)),
            _ => Err("Give `object_name` and `texture_id`.".into()),
        },
        _ => Err(format!("Unknown tool: {name}")),
    };
    match r {
        Ok(s) => text(s, false),
        Err(e) => text(e, true),
    }
}

/// JSON for the model: indented, but lists of plain values (coordinates,
/// names) on one line, so a scene doesn't take a line per number.
fn pretty(v: &Value) -> String {
    fn go(v: &Value, depth: usize, out: &mut String) {
        let pad = |d: usize| "  ".repeat(d);
        match v {
            Value::Object(m) if !m.is_empty() => {
                out.push_str("{\n");
                for (i, (k, x)) in m.iter().enumerate() {
                    out.push_str(&format!("{}{}: ", pad(depth + 1), Value::String(k.clone())));
                    go(x, depth + 1, out);
                    out.push_str(if i + 1 < m.len() { ",\n" } else { "\n" });
                }
                out.push_str(&pad(depth));
                out.push('}');
            }
            Value::Array(a) if a.iter().any(|x| x.is_object() || x.is_array()) => {
                out.push_str("[\n");
                for (i, x) in a.iter().enumerate() {
                    out.push_str(&pad(depth + 1));
                    go(x, depth + 1, out);
                    out.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
                }
                out.push_str(&pad(depth));
                out.push(']');
            }
            Value::Array(a) => out.push_str(&format!("[{}]", a.iter().map(Value::to_string).collect::<Vec<_>>().join(", "))),
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    go(v, 0, &mut out);
    out
}

/// Where the add-on listens.
fn address() -> (String, u16) {
    let host = std::env::var("BLENDER_HOST").ok().filter(|h| !h.trim().is_empty()).unwrap_or_else(|| "127.0.0.1".into());
    let host = if host == "localhost" { "127.0.0.1".to_string() } else { host };
    let port = std::env::var("BLENDER_PORT").ok().and_then(|p| p.trim().parse().ok()).unwrap_or(9876);
    (host, port)
}

/// Sends one command to the add-on and returns its `result`.
pub fn send(kind: &str, params: Value) -> Result<Value, String> {
    send_within(kind, params, COMMAND_TIMEOUT)
}

/// Whether something listens on the add-on's port (a quick look, for the
/// MCP tab and the live view).
pub fn reachable() -> bool {
    let (host, port) = address();
    (host.as_str(), port).to_socket_addrs().ok().and_then(|mut a| a.next()).is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_millis(300)).is_ok())
}

/// The 3D viewport now, at most `max` pixels on its longer side, for the
/// live view beside a conversation: None when Blender doesn't answer within
/// a few seconds (busy with the model's script) or has no viewport.
pub fn frame(max: u32) -> Option<image::RgbaImage> {
    let r = send_within("execute_code", json!({"code": with_params(SCREENSHOT, &json!({}))}), Duration::from_secs(5)).ok()?;
    let out = r["result"].as_str()?;
    let (_, j) = out.rsplit_once(MARK)?;
    let v: Value = serde_json::from_str(j.trim()).ok()?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(v["png"].as_str()?).ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    let img = if img.width().max(img.height()) > max { img.resize(max, max, image::imageops::FilterType::Triangle) } else { img };
    Some(img.to_rgba8())
}

fn send_within(kind: &str, params: Value, timeout: Duration) -> Result<Value, String> {
    let (host, port) = address();
    let unreachable = |e: String| {
        format!(
            "Couldn't reach Blender on {host}:{port} ({e}). Blender has to be open with the Oak MCP add-on running: Oak installs it from Home > MCP (the Blender row's Install add-on), and Blender's 3D viewport sidebar (N) has an Oak tab that shows it listening. The community Blender MCP add-on works too, once its server is started. Ask the user to open Blender if it isn't."
        )
    };
    let addr = (host.as_str(), port).to_socket_addrs().map_err(|e| unreachable(e.to_string()))?.next().ok_or_else(|| unreachable("no address".into()))?;
    let mut sock = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|e| unreachable(e.to_string()))?;
    sock.set_read_timeout(Some(timeout)).ok();
    sock.write_all(json!({"type": kind, "params": params}).to_string().as_bytes()).map_err(|e| format!("Couldn't send to Blender: {e}"))?;
    let mut buf = vec![];
    let mut chunk = vec![0u8; 1 << 16];
    let reply: Value = loop {
        match sock.read(&mut chunk) {
            Ok(0) => {
                return Err(if buf.is_empty() { "Blender closed the connection without answering.".into() } else { "Blender's answer was cut off.".into() });
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Err(format!("Blender didn't answer within {} minutes (still busy, or a modal dialog is open).", timeout.as_secs() / 60));
            }
            Err(e) => return Err(format!("Lost the connection to Blender: {e}")),
        }
        match serde_json::from_slice::<Value>(&buf) {
            Ok(v) => break v,
            Err(e) if e.is_eof() => continue,
            Err(e) => return Err(format!("Blender sent something that isn't JSON: {e}")),
        }
    };
    if reply["status"] == "error" {
        let message = reply["message"].as_str().unwrap_or("Blender reported an error.");
        // The community add-on's newer versions put the traceback in JSON.
        if let Ok(detail) = serde_json::from_str::<Value>(message)
            && let Some(tb) = detail["traceback"].as_str()
        {
            return Err(format!("{}: {}\n\n{tb}", detail["exception_type"].as_str().unwrap_or("Error"), detail["message"].as_str().unwrap_or("")));
        }
        return Err(message.to_string());
    }
    Ok(reply.get("result").cloned().unwrap_or(Value::Null))
}

/// Runs Python in Blender; what it printed.
pub fn execute(code: &str) -> Result<String, String> {
    let r = send("execute_code", json!({"code": code}))?;
    Ok(match &r["result"] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    })
}

/// Runs one of the scripts below and reads the JSON it printed after MARK.
fn script(code: &str) -> Result<Value, String> {
    let out = execute(code)?;
    let Some((_, json)) = out.rsplit_once(MARK) else { return Err(format!("The script gave no answer. It printed:\n{out}")) };
    let v: Value = serde_json::from_str(json.trim()).map_err(|e| format!("The script's answer isn't JSON ({e})"))?;
    match v["error"].as_str() {
        Some(e) => Err(e.to_string()),
        None => Ok(v),
    }
}

/// A script with its parameters as `P` (a dict), from JSON (a JSON string
/// is a Python string literal too).
fn with_params(code: &str, params: &Value) -> String {
    let literal = serde_json::to_string(&params.to_string()).unwrap_or_default();
    format!("import json as _json\nP = _json.loads({literal})\nMARK = {MARK:?}\n{code}")
}

fn screenshot(max_size: u32) -> Value {
    let shot = || -> Result<Value, String> {
        let v = script(&with_params(SCREENSHOT, &json!({})))?;
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(v["png"].as_str().unwrap_or("")).map_err(|e| format!("The screenshot didn't come through: {e}"))?;
        let img = image::load_from_memory(&bytes).map_err(|e| format!("The screenshot didn't come through: {e}"))?;
        let img = if img.width().max(img.height()) > max_size { img.resize(max_size, max_size, image::imageops::FilterType::Triangle) } else { img };
        let mut png = std::io::Cursor::new(vec![]);
        img.write_to(&mut png, image::ImageFormat::Png).map_err(|e| e.to_string())?;
        let data = base64::engine::general_purpose::STANDARD.encode(png.into_inner());
        Ok(json!({"content": [{"type": "image", "data": data, "mimeType": "image/png"}], "isError": false}))
    };
    shot().unwrap_or_else(|e| json!({"content": [{"type": "text", "text": format!("Screenshot failed: {e}")}], "isError": true}))
}

// ------------------------------------------------------------ Poly Haven

fn http() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder().user_agent(USER_AGENT).timeout(Duration::from_secs(300)).build().map_err(|e| e.to_string())
}

fn get_json(url: &str) -> Result<Value, String> {
    let r = http()?.get(url).send().map_err(|e| format!("Poly Haven didn't answer: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("Poly Haven answered {} for {url}", r.status()));
    }
    r.json().map_err(|e| format!("Poly Haven's answer isn't JSON: {e}"))
}

fn check_type(kind: &str, all: bool) -> Result<(), String> {
    let ok: &[&str] = if all { &["hdris", "textures", "models", "all"] } else { &["hdris", "textures", "models"] };
    if ok.contains(&kind) { Ok(()) } else { Err(format!("Invalid asset type: {kind}. Must be one of: {}", ok.join(", "))) }
}

fn polyhaven_categories(kind: &str) -> Result<String, String> {
    check_type(kind, true)?;
    let v = get_json(&format!("{POLYHAVEN}/categories/{kind}"))?;
    let mut cats: Vec<(&String, u64)> = v.as_object().into_iter().flatten().map(|(k, n)| (k, n.as_u64().unwrap_or(0))).collect();
    cats.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let lines: Vec<String> = cats.iter().map(|(k, n)| format!("- {k}: {n}")).collect();
    Ok(format!("Categories for {kind}:\n{}", lines.join("\n")))
}

fn polyhaven_search(kind: &str, categories: Option<&str>) -> Result<String, String> {
    check_type(kind, true)?;
    let mut url = format!("{POLYHAVEN}/assets?type={kind}");
    if let Some(c) = categories {
        url += &format!("&categories={}", c.split(',').map(str::trim).filter(|c| !c.is_empty()).collect::<Vec<_>>().join(","));
    }
    let v = get_json(&url)?;
    let mut assets: Vec<(&String, &Value)> = v.as_object().into_iter().flatten().collect();
    assets.sort_by_key(|(_, a)| std::cmp::Reverse(a["download_count"].as_u64().unwrap_or(0)));
    let kinds = ["hdris", "textures", "models"];
    let mut out = format!("Found {} assets", assets.len());
    if let Some(c) = categories {
        out += &format!(" in categories: {c}");
    }
    out += &format!("\nShowing {} assets:\n\n", assets.len().min(20));
    for (id, a) in assets.iter().take(20) {
        let ty = a["type"].as_u64().and_then(|t| kinds.get(t as usize)).copied().unwrap_or("unknown");
        let cats: Vec<&str> = a["categories"].as_array().into_iter().flatten().filter_map(|c| c.as_str()).collect();
        out += &format!("- {} (ID: {id})\n  Type: {ty}\n  Categories: {}\n  Downloads: {}\n", a["name"].as_str().unwrap_or(id), cats.join(", "), a["download_count"].as_u64().unwrap_or(0));
        if let Some(d) = a["dimensions"].as_array().filter(|d| d.len() == 2) {
            let m = |v: &Value| (v.as_f64().unwrap_or(0.0) / 10.0).round() / 100.0;
            out += &format!("  Size: {} x {} m\n", m(&d[0]), m(&d[1]));
        }
        out.push('\n');
    }
    Ok(out.trim_end().to_string())
}

/// Where downloads go: a folder per asset and resolution under the
/// system's temporary folder (Blender reads them from there; packing them
/// into the .blend is the user's choice).
fn download_dir(id: &str, res: &str) -> PathBuf {
    let safe = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>();
    std::env::temp_dir().join("oak-polyhaven").join(format!("{}_{}", safe(id), safe(res)))
}

fn download(client: &reqwest::blocking::Client, url: &str, to: &Path) -> Result<(), String> {
    if to.metadata().is_ok_and(|m| m.len() > 0) {
        return Ok(());
    }
    let r = client.get(url).send().map_err(|e| format!("Couldn't download {url}: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("Couldn't download {url}: {}", r.status()));
    }
    let bytes = r.bytes().map_err(|e| format!("Couldn't download {url}: {e}"))?;
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(to, &bytes).map_err(|e| format!("Couldn't save {}: {e}", to.display()))
}

/// The resolutions an asset's files come in, smallest first.
fn resolutions(v: &Value) -> Vec<String> {
    let mut r: Vec<String> = v.as_object().into_iter().flatten().map(|(k, _)| k.clone()).collect();
    r.sort_by_key(|k| k.trim_end_matches('k').parse::<u32>().unwrap_or(u32::MAX));
    r
}

/// A file of `files[group]` at `res` in `fmt`, or why there is none.
fn pick<'a>(files: &'a Value, group: &str, res: &str, fmt: &str) -> Result<&'a Value, String> {
    let g = &files[group];
    if g.is_null() {
        return Err(format!("This asset has no {group} files."));
    }
    let r = &g[res];
    if r.is_null() {
        return Err(format!("No {res} resolution; available: {}", resolutions(g).join(", ")));
    }
    let f = &r[fmt];
    if f["url"].is_null() {
        let fmts: Vec<&String> = r.as_object().into_iter().flatten().map(|(k, _)| k).collect();
        return Err(format!("No {fmt} format at {res}; available: {}", fmts.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
    }
    Ok(f)
}

fn polyhaven_download(id: &str, kind: &str, res: &str, fmt: Option<&str>) -> Result<String, String> {
    check_type(kind, false)?;
    let files = get_json(&format!("{POLYHAVEN}/files/{id}"))?;
    let client = http()?;
    let dir = download_dir(id, res);
    let params = match kind {
        "hdris" => {
            let fmt = fmt.unwrap_or("hdr");
            let f = pick(&files, "hdri", res, fmt)?;
            let path = dir.join(format!("{id}_{res}.{fmt}"));
            download(&client, f["url"].as_str().unwrap_or(""), &path)?;
            json!({"kind": kind, "id": id, "path": path})
        }
        "textures" => {
            let fmt = fmt.unwrap_or("jpg");
            let mut maps = serde_json::Map::new();
            for (map, by_res) in files.as_object().into_iter().flatten() {
                if ["blend", "gltf", "mtlx", "usd"].contains(&map.as_str()) {
                    continue;
                }
                let Some(url) = by_res[res][fmt]["url"].as_str() else { continue };
                let ext = url.rsplit('.').next().unwrap_or(fmt);
                let path = dir.join(format!("{id}_{map}_{res}.{ext}"));
                download(&client, url, &path)?;
                maps.insert(map.clone(), json!(path));
            }
            if maps.is_empty() {
                let any = files.as_object().and_then(|o| o.values().next()).cloned().unwrap_or_default();
                return Err(format!("No {fmt} maps at {res}; resolutions: {}", resolutions(&any).join(", ")));
            }
            json!({"kind": kind, "id": id, "maps": maps})
        }
        _ => {
            let fmt = fmt.unwrap_or("gltf");
            let f = pick(&files, fmt, res, fmt)?;
            let url = f["url"].as_str().unwrap_or("");
            let main = dir.join(url.rsplit('/').next().unwrap_or("model"));
            download(&client, url, &main)?;
            // The files it refers to, at their relative paths.
            for (rel, inc) in f["include"].as_object().into_iter().flatten() {
                if rel.contains("..") {
                    continue;
                }
                download(&client, inc["url"].as_str().unwrap_or(""), &dir.join(rel))?;
            }
            json!({"kind": kind, "id": id, "path": main, "format": fmt})
        }
    };
    let v = script(&with_params(IMPORT, &params))?;
    Ok(v["message"].as_str().map(str::to_string).unwrap_or_else(|| pretty(&v)) + "\n(Poly Haven assets are CC0: free to use, no attribution needed.)")
}

// ------------------------------------------------------- Blender's side

const SCENE_INFO: &str = r#"
import bpy, math
def r(v, n=3): return [round(float(x), n) for x in v]
scene = bpy.context.scene
objs = list(scene.objects)
vl = bpy.context.view_layer
def hidden(o):
    try: return o.hide_get() or o.hide_viewport
    except Exception: return o.hide_viewport
out = {
    "name": scene.name,
    "file": bpy.data.filepath or "(not saved yet)",
    "unsaved_changes": bpy.data.is_dirty,
    "frame": scene.frame_current, "frame_range": [scene.frame_start, scene.frame_end], "fps": scene.render.fps,
    "render_engine": scene.render.engine,
    "resolution": [scene.render.resolution_x, scene.render.resolution_y, scene.render.resolution_percentage],
    "camera": scene.camera.name if scene.camera else None,
    "world": scene.world.name if scene.world else None,
    "unit_system": scene.unit_settings.system, "unit_scale": scene.unit_settings.scale_length,
    "mode": bpy.context.mode,
    "active_object": vl.objects.active.name if vl.objects.active else None,
    "selected_objects": [o.name for o in objs if o.select_get()][:50],
    "object_count": len(objs),
    "materials_count": len(bpy.data.materials),
    "collections": [{"name": c.name, "objects": len(c.objects), "children": [k.name for k in c.children]} for c in bpy.data.collections][:50],
    "objects": [],
}
for o in objs[:P["limit"]]:
    d = {"name": o.name, "type": o.type,
         "location": r(o.matrix_world.translation),
         "rotation_deg": r([math.degrees(a) for a in o.matrix_world.to_euler()], 1),
         "scale": r(o.matrix_world.to_scale()),
         "dimensions": r(o.dimensions)}
    if o.parent: d["parent"] = o.parent.name
    if hidden(o): d["hidden"] = True
    mats = [s.material.name for s in o.material_slots if s.material]
    if mats: d["materials"] = mats
    out["objects"].append(d)
if len(objs) > P["limit"]:
    out["note"] = "Listed %d of %d objects; pass a higher limit, or read the rest with execute_blender_code." % (P["limit"], len(objs))
print(MARK + _json.dumps(out))
"#;

const OBJECT_INFO: &str = r#"
import bpy, math
from mathutils import Vector
def r(v, n=4): return [round(float(x), n) for x in v]
o = bpy.data.objects.get(P["name"])
if o is None:
    near = [x.name for x in bpy.data.objects if P["name"].lower() in x.name.lower()][:10]
    print(MARK + _json.dumps({"error": "Object not found: %s%s" % (P["name"], (". Similar: " + ", ".join(near)) if near else "")}))
else:
    d = {"name": o.name, "type": o.type, "data": o.data.name if o.data else None,
         "location": r(o.location), "rotation_mode": o.rotation_mode,
         "rotation_euler_deg": r([math.degrees(a) for a in o.rotation_euler], 2),
         "scale": r(o.scale), "dimensions": r(o.dimensions),
         "world_location": r(o.matrix_world.translation),
         "visible": o.visible_get(), "hide_render": o.hide_render,
         "parent": o.parent.name if o.parent else None,
         "children": [c.name for c in o.children][:50],
         "collections": [c.name for c in o.users_collection],
         "materials": [s.material.name if s.material else None for s in o.material_slots]}
    if o.bound_box:
        corners = [o.matrix_world @ Vector(c) for c in o.bound_box]
        d["world_bounding_box"] = [r([min(c[i] for c in corners) for i in range(3)]), r([max(c[i] for c in corners) for i in range(3)])]
    if o.modifiers:
        d["modifiers"] = [{"name": m.name, "type": m.type, "show_viewport": m.show_viewport} for m in o.modifiers]
    if o.constraints:
        d["constraints"] = [{"name": c.name, "type": c.type} for c in o.constraints]
    if o.type == "MESH" and o.data:
        me = o.data
        d["mesh"] = {"vertices": len(me.vertices), "edges": len(me.edges), "polygons": len(me.polygons),
                     "triangles": sum(len(p.vertices) - 2 for p in me.polygons),
                     "uv_maps": [u.name for u in me.uv_layers], "shade_smooth": any(p.use_smooth for p in me.polygons[:1000])}
    elif o.type == "LIGHT":
        d["light"] = {"type": o.data.type, "energy": o.data.energy, "color": r(o.data.color, 3)}
    elif o.type == "CAMERA":
        d["camera"] = {"type": o.data.type, "lens": o.data.lens, "clip": [o.data.clip_start, o.data.clip_end]}
    if o.animation_data and o.animation_data.action:
        a = o.animation_data.action
        d["animation"] = {"action": a.name, "frame_range": r(a.frame_range, 1)}
    props = {k: (v if isinstance(v, (int, float, str, bool)) else str(v)) for k, v in o.items() if not k.startswith("_")}
    if props: d["custom_properties"] = props
    print(MARK + _json.dumps(d))
"#;

const SCREENSHOT: &str = r#"
import bpy, os, tempfile, base64
path = os.path.join(tempfile.gettempdir(), "oak_mcp_viewport_%d.png" % os.getpid())
done = None
wm = bpy.context.window_manager
for win in wm.windows:
    for area in win.screen.areas:
        if area.type == "VIEW_3D":
            region = next((g for g in area.regions if g.type == "WINDOW"), None)
            with bpy.context.temp_override(window=win, screen=win.screen, area=area, region=region):
                bpy.ops.screen.screenshot_area(filepath=path)
            done = area
            break
    if done: break
if not done:
    print(MARK + _json.dumps({"error": "No 3D viewport is open in Blender."}))
else:
    with open(path, "rb") as f: data = f.read()
    try: os.remove(path)
    except OSError: pass
    print(MARK + _json.dumps({"png": base64.b64encode(data).decode()}))
"#;

const IMPORT: &str = r#"
import bpy, os
from mathutils import Vector
kind, aid = P["kind"], P["id"]
def bbox(objs):
    pts = [o.matrix_world @ Vector(c) for o in objs if o.type == "MESH" for c in o.bound_box]
    if not pts: return None
    return [[round(min(p[i] for p in pts), 3) for i in range(3)], [round(max(p[i] for p in pts), 3) for i in range(3)]]
if kind == "hdris":
    world = bpy.context.scene.world or bpy.data.worlds.new("World")
    bpy.context.scene.world = world
    world.use_nodes = True
    nt = world.node_tree
    nt.nodes.clear()
    coord = nt.nodes.new("ShaderNodeTexCoord"); coord.location = (-800, 0)
    mapping = nt.nodes.new("ShaderNodeMapping"); mapping.location = (-600, 0)
    env = nt.nodes.new("ShaderNodeTexEnvironment"); env.location = (-400, 0)
    env.image = bpy.data.images.load(P["path"], check_existing=True)
    bg = nt.nodes.new("ShaderNodeBackground"); bg.location = (-150, 0)
    out = nt.nodes.new("ShaderNodeOutputWorld"); out.location = (100, 0)
    nt.links.new(coord.outputs["Generated"], mapping.inputs["Vector"])
    nt.links.new(mapping.outputs["Vector"], env.inputs["Vector"])
    nt.links.new(env.outputs["Color"], bg.inputs["Color"])
    nt.links.new(bg.outputs["Background"], out.inputs["Surface"])
    print(MARK + _json.dumps({"message": "HDRI %s is the world's lighting now (world %s; rotate it with the Mapping node's Z rotation, brighten it with the Background node's Strength)." % (aid, world.name)}))
elif kind == "textures":
    mat = bpy.data.materials.get(aid) or bpy.data.materials.new(aid)
    mat.use_nodes = True
    nt = mat.node_tree
    nt.nodes.clear()
    out = nt.nodes.new("ShaderNodeOutputMaterial"); out.location = (400, 0)
    bsdf = nt.nodes.new("ShaderNodeBsdfPrincipled"); bsdf.location = (100, 0)
    nt.links.new(bsdf.outputs["BSDF"], out.inputs["Surface"])
    coord = nt.nodes.new("ShaderNodeTexCoord"); coord.location = (-1000, 0)
    mapping = nt.nodes.new("ShaderNodeMapping"); mapping.location = (-800, 0)
    nt.links.new(coord.outputs["UV"], mapping.inputs["Vector"])
    used, y = [], 300
    def tex(path, color):
        global y
        n = nt.nodes.new("ShaderNodeTexImage"); n.location = (-500, y); y -= 280
        n.image = bpy.data.images.load(path, check_existing=True)
        if not color: n.image.colorspace_settings.name = "Non-Color"
        nt.links.new(mapping.outputs["Vector"], n.inputs["Vector"])
        return n
    maps = {k.lower(): v for k, v in P["maps"].items()}
    def find(*names):
        # In the order given: nor_gl (OpenGL, as Blender wants) before nor_dx.
        for n in names:
            for k, v in maps.items():
                if k == n or k.startswith(n): return k, v
        return None, None
    k, p = find("diffuse", "diff", "albedo", "color", "col")
    if p: nt.links.new(tex(p, True).outputs["Color"], bsdf.inputs["Base Color"]); used.append(k)
    k, p = find("rough")
    if p: nt.links.new(tex(p, False).outputs["Color"], bsdf.inputs["Roughness"]); used.append(k)
    k, p = find("metal")
    if p: nt.links.new(tex(p, False).outputs["Color"], bsdf.inputs["Metallic"]); used.append(k)
    if "rough" not in " ".join(used) or "metal" not in " ".join(used):
        k, p = find("arm")
        if p:
            n = tex(p, False)
            sep = nt.nodes.new("ShaderNodeSeparateColor"); sep.location = (-200, y + 280)
            nt.links.new(n.outputs["Color"], sep.inputs["Color"])
            if not any(u.startswith("rough") for u in used): nt.links.new(sep.outputs[1], bsdf.inputs["Roughness"])
            if not any(u.startswith("metal") for u in used): nt.links.new(sep.outputs[2], bsdf.inputs["Metallic"])
            used.append(k)
    k, p = find("nor_gl", "normal", "nor")
    if p:
        n = tex(p, False)
        nm = nt.nodes.new("ShaderNodeNormalMap"); nm.location = (-200, y + 280)
        nt.links.new(n.outputs["Color"], nm.inputs["Color"])
        nt.links.new(nm.outputs["Normal"], bsdf.inputs["Normal"]); used.append(k)
    k, p = find("disp", "displacement", "height")
    if p:
        n = tex(p, False)
        d = nt.nodes.new("ShaderNodeDisplacement"); d.location = (100, -400)
        d.inputs["Scale"].default_value = 0.05
        nt.links.new(n.outputs["Color"], d.inputs["Height"])
        nt.links.new(d.outputs["Displacement"], out.inputs["Displacement"]); used.append(k)
    print(MARK + _json.dumps({"message": "Texture %s is material %r now, with maps: %s. Apply it with set_texture (the object needs a UV map)." % (aid, mat.name, ", ".join(used) or "none")}))
else:
    before = set(bpy.data.objects)
    path, fmt = P["path"], P["format"]
    if fmt == "gltf" or path.lower().endswith((".gltf", ".glb")):
        bpy.ops.import_scene.gltf(filepath=path)
    elif fmt == "fbx" or path.lower().endswith(".fbx"):
        bpy.ops.import_scene.fbx(filepath=path)
    elif path.lower().endswith(".blend"):
        with bpy.data.libraries.load(path, link=False) as (src, dst):
            dst.objects = src.objects
        for o in dst.objects:
            if o is not None: bpy.context.scene.collection.objects.link(o)
    else:
        raise RuntimeError("Can't import %s files; use gltf or fbx." % fmt)
    new = [o for o in bpy.data.objects if o not in before]
    print(MARK + _json.dumps({"message": "Imported model %s: %d objects (%s). World bounding box: %s. Check placement and scale against the scene." % (aid, len(new), ", ".join(o.name for o in new[:20]), bbox(new))}))
"#;

const SET_TEXTURE: &str = r#"
import bpy
o = bpy.data.objects.get(P["object"])
mat = bpy.data.materials.get(P["texture"])
if o is None:
    print(MARK + _json.dumps({"error": "Object not found: %s" % P["object"]}))
elif mat is None:
    print(MARK + _json.dumps({"error": "No material %s: download the texture first (download_polyhaven_asset with asset_type textures)." % P["texture"]}))
elif not hasattr(o.data, "materials"):
    print(MARK + _json.dumps({"error": "%s (%s) can't take materials." % (o.name, o.type)}))
else:
    o.data.materials.clear()
    o.data.materials.append(mat)
    note = ""
    if o.type == "MESH" and not o.data.uv_layers:
        note = " It has no UV map, so the texture won't map well: unwrap it (e.g. bpy.ops.uv.smart_project in edit mode) or use a generated mapping."
    print(MARK + _json.dumps({"success": True, "message": "Applied %s to %s.%s" % (mat.name, o.name, note), "material": mat.name, "object": o.name}))
"#;

// ------------------------------------------------------ the add-on itself

/// Blender programs on this machine, newest-looking first: `OAK_BLENDER`,
/// the PATH, and the usual install places.
pub fn find_blender() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = vec![];
    if let Ok(p) = std::env::var("OAK_BLENDER") {
        found.push(PathBuf::from(p));
    }
    let exe = if cfg!(windows) { "blender.exe" } else { "blender" };
    if let Some(path) = std::env::var_os("PATH") {
        found.extend(std::env::split_paths(&path).map(|d| d.join(exe)));
    }
    let mut roots: Vec<PathBuf> = vec![];
    if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
            if let Ok(p) = std::env::var(var) {
                roots.push(PathBuf::from(&p).join("Blender Foundation"));
                roots.push(PathBuf::from(p).join("Steam").join("steamapps").join("common"));
            }
        }
        if let Some(local) = dirs::data_local_dir() {
            roots.push(local.join("Programs").join("Blender Foundation"));
        }
    } else {
        found.extend(["/usr/bin/blender", "/usr/local/bin/blender", "/snap/bin/blender", "/opt/blender/blender"].map(PathBuf::from));
        roots.push(PathBuf::from("/opt"));
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join("Applications"));
            roots.push(home.join(".local/share/Steam/steamapps/common"));
        }
    }
    for root in roots {
        let mut subs: Vec<PathBuf> = std::fs::read_dir(&root).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().starts_with("blender"))).collect();
        subs.sort();
        subs.reverse();
        found.extend(subs.into_iter().map(|d| d.join(exe)));
    }
    let mut out: Vec<PathBuf> = vec![];
    for p in found {
        if p.is_file() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Blender's own settings folders per version (`<config>/<version>`).
fn user_version_dirs() -> Vec<PathBuf> {
    if let Ok(p) = std::env::var("BLENDER_USER_RESOURCES") {
        return vec![PathBuf::from(p)];
    }
    let base = if cfg!(windows) {
        dirs::config_dir().map(|d| d.join("Blender Foundation").join("Blender"))
    } else if cfg!(target_os = "macos") {
        dirs::home_dir().map(|d| d.join("Library/Application Support/Blender"))
    } else {
        dirs::config_dir().map(|d| d.join("blender"))
    };
    let Some(base) = base else { return vec![] };
    let mut v: Vec<PathBuf> = std::fs::read_dir(base).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n.to_string_lossy().chars().next().is_some_and(|c| c.is_ascii_digit()))).collect();
    v.sort();
    v
}

/// The add-on files installed in Blender's settings folders, if any.
pub fn installed_addons() -> Vec<PathBuf> {
    user_version_dirs().into_iter().map(|d| d.join("scripts").join("addons").join(format!("{ADDON_MODULE}.py"))).filter(|p| p.is_file()).collect()
}

/// Whether an installed copy differs from the add-on this Oak brings.
pub fn addon_outdated() -> bool {
    installed_addons().iter().any(|p| std::fs::read_to_string(p).is_ok_and(|s| s.replace("\r\n", "\n") != ADDON.replace("\r\n", "\n")))
}

/// Installs the add-on: with a Blender program found, that Blender copies
/// it into its add-ons and switches it on (in the background, no window);
/// otherwise it goes into every Blender settings folder there is, to be
/// switched on in Preferences. Says what it did.
pub fn install_addon() -> Result<String, String> {
    let src = std::env::temp_dir().join(format!("{ADDON_MODULE}.py"));
    std::fs::write(&src, ADDON).map_err(|e| format!("Couldn't write {}: {e}", src.display()))?;
    let mut tried = vec![];
    for exe in find_blender() {
        // Two runs: Blender looks for add-ons as it starts, so one put into
        // a folder made during the run (a fresh Blender's) would be switched
        // on but not kept in its preferences.
        let copy = format!(
            "import bpy, shutil, os\nd = bpy.utils.user_resource('SCRIPTS', path='addons', create=True)\nshutil.copy({src:?}, os.path.join(d, '{ADDON_MODULE}.py'))\nprint('{MARK}' + d)\n",
            src = src.display().to_string()
        );
        let enable = format!(
            "import bpy, addon_utils\naddon_utils.modules_refresh()\naddon_utils.enable('{ADDON_MODULE}', default_set=True, persistent=True)\nbpy.context.preferences.is_dirty = True\nbpy.ops.wm.save_userpref()\nprint('{MARK}' + str('{ADDON_MODULE}' in bpy.context.preferences.addons))\n"
        );
        let run = |script: &str| -> Result<String, String> {
            let mut cmd = std::process::Command::new(&exe);
            // (Not --factory-startup: it skips the user's preferences, which
            // the save would then replace.)
            cmd.args(["--background", "--python-exit-code", "1", "--python-expr", script]);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            let out = cmd.output().map_err(|e| e.to_string())?;
            let text = String::from_utf8_lossy(&out.stdout);
            match text.split_once(MARK) {
                Some((_, said)) if out.status.success() => Ok(said.lines().next().unwrap_or("").trim().to_string()),
                _ => Err(String::from_utf8_lossy(&out.stderr).lines().chain(text.lines()).rfind(|l| l.contains("Error")).unwrap_or("failed").trim().to_string()),
            }
        };
        match run(&copy).and_then(|dir| run(&enable).map(|on| (dir, on))) {
            Ok((dir, on)) if on == "True" => {
                return Ok(format!("Installed and switched on in {} ({dir}). Restart Blender if it is open; the 3D viewport's sidebar (N) then has an Oak tab.", exe.display()));
            }
            Ok((dir, _)) => tried.push(format!("{}: copied into {dir}, but it didn't stay switched on", exe.display())),
            Err(e) => tried.push(format!("{}: {e}", exe.display())),
        }
    }
    let dirs = user_version_dirs();
    if dirs.is_empty() {
        return Err(if tried.is_empty() {
            "No Blender found on this computer. Install Blender (blender.org), open it once, then install the add-on again; or save the add-on file and install it from Blender's Preferences > Add-ons > Install from Disk.".into()
        } else {
            format!("Blender couldn't install it: {}", tried.join("; "))
        });
    }
    let mut put = vec![];
    for d in &dirs {
        let addons = d.join("scripts").join("addons");
        std::fs::create_dir_all(&addons).map_err(|e| format!("Couldn't make {}: {e}", addons.display()))?;
        let to = addons.join(format!("{ADDON_MODULE}.py"));
        std::fs::write(&to, ADDON).map_err(|e| format!("Couldn't write {}: {e}", to.display()))?;
        put.push(d.file_name().unwrap_or_default().to_string_lossy().into_owned());
    }
    Ok(format!("Copied into Blender {}'s add-ons. Switch it on in Blender: Edit > Preferences > Add-ons, search \"Oak MCP\".", put.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_the_classic_tools() {
        let names: Vec<String> = tools().as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        for n in ["get_scene_info", "get_object_info", "get_viewport_screenshot", "execute_blender_code", "download_polyhaven_asset", "set_texture"] {
            assert!(names.contains(&n.to_string()), "{n}");
        }
        let init = handle("initialize", &json!({"protocolVersion": "2025-03-26"})).unwrap();
        assert_eq!(init["protocolVersion"], "2025-03-26");
        assert!(init["instructions"].as_str().unwrap().contains("get_scene_info"));
        assert_eq!(handle("nope", &json!({})).unwrap_err().0, -32601);
    }

    #[test]
    fn params_reach_python_as_a_dict() {
        let code = with_params("print(P)", &json!({"name": "Cube \"x\"\n\\path", "n": 3}));
        assert!(code.contains("P = _json.loads(\""), "{code}");
        assert!(code.contains("@@OAK_MCP@@"));
    }

    /// With a real Blender: `OAK_BLENDER=path/blender.exe
    /// BLENDER_USER_RESOURCES=scratch cargo test installs_into -- --ignored`.
    #[test]
    #[ignore]
    fn installs_into_a_real_blender() {
        let said = install_addon().unwrap();
        assert!(said.starts_with("Installed and switched on"), "{said}");
        assert_eq!(installed_addons().len(), 1);
        assert!(!addon_outdated());
    }

    /// With a Blender running the add-on: `cargo test with_blender -- --ignored`.
    #[test]
    #[ignore]
    fn with_blender_running() {
        let info = call("get_scene_info", &json!({}));
        assert_eq!(info["isError"], false, "{info}");
        println!("{}", info["content"][0]["text"].as_str().unwrap());
        let r = call("execute_blender_code", &json!({"code": "import bpy\nbpy.ops.mesh.primitive_monkey_add(location=(2, 0, 1))\nprint(bpy.context.active_object.name)"}));
        assert_eq!(r["isError"], false, "{r}");
        println!("{}", r["content"][0]["text"].as_str().unwrap());
        let o = call("get_object_info", &json!({"object_name": "Suzanne"}));
        assert_eq!(o["isError"], false, "{o}");
        println!("{}", o["content"][0]["text"].as_str().unwrap());
        let bad = call("execute_blender_code", &json!({"code": "print('before')\nraise ValueError('boom')"}));
        assert_eq!(bad["isError"], true);
        println!("{}", bad["content"][0]["text"].as_str().unwrap());
        let shot = call("get_viewport_screenshot", &json!({"max_size": 600}));
        assert_eq!(shot["isError"], false, "{shot}");
        assert_eq!(shot["content"][0]["type"], "image");
        let t = std::time::Instant::now();
        let f = frame(720).expect("a frame");
        println!("frame {}x{} in {:?}", f.width(), f.height(), t.elapsed());
        f.save(std::env::temp_dir().join("oak_blender_frame.png")).unwrap();
        let none = call("get_object_info", &json!({"object_name": "suz"}));
        assert_eq!(none["isError"], true);
        println!("{}", none["content"][0]["text"].as_str().unwrap());
    }

    /// With a Blender running the add-on, and the network.
    #[test]
    #[ignore]
    fn polyhaven_with_blender() {
        let text = |r: &Value| r["content"][0]["text"].as_str().unwrap_or("").to_string();
        let r = call("get_polyhaven_categories", &json!({"asset_type": "textures"}));
        assert_eq!(r["isError"], false, "{r}");
        println!("{}", text(&r).lines().take(5).collect::<Vec<_>>().join("\n"));
        let r = call("search_polyhaven_assets", &json!({"asset_type": "textures", "categories": "brick"}));
        assert_eq!(r["isError"], false, "{r}");
        println!("{}", text(&r).lines().take(8).collect::<Vec<_>>().join("\n"));
        let id = text(&r).split("(ID: ").nth(1).unwrap().split(')').next().unwrap().to_string();
        let r = call("download_polyhaven_asset", &json!({"asset_id": id, "asset_type": "textures", "resolution": "1k"}));
        assert_eq!(r["isError"], false, "{r}");
        println!("{}", text(&r));
        let r = call("set_texture", &json!({"object_name": "Cube", "texture_id": id}));
        assert_eq!(r["isError"], false, "{r}");
        println!("{}", text(&r));
        let r = call("download_polyhaven_asset", &json!({"asset_id": "kloppenheim_06_puresky", "asset_type": "hdris", "resolution": "1k"}));
        assert_eq!(r["isError"], false, "{r}");
        println!("{}", text(&r));
        let r = call("download_polyhaven_asset", &json!({"asset_id": "rubber_duck_toy", "asset_type": "models", "resolution": "1k"}));
        assert_eq!(r["isError"], false, "{r}");
        println!("{}", text(&r));
        let r = call("execute_blender_code", &json!({"code": "import bpy\nfor a in bpy.context.screen.areas:\n    if a.type == 'VIEW_3D': a.spaces[0].shading.type = 'MATERIAL'\nbpy.ops.object.select_all(action='DESELECT')"}));
        assert_eq!(r["isError"], false, "{r}");
        frame(1200).unwrap().save(std::env::temp_dir().join("oak_blender_frame2.png")).unwrap();
    }

    #[test]
    fn unreachable_blender_says_what_to_do() {
        // SAFETY: tests in this module alone read BLENDER_PORT.
        unsafe { std::env::set_var("BLENDER_PORT", "1") };
        let r = call("get_scene_info", &json!({}));
        unsafe { std::env::remove_var("BLENDER_PORT") };
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("Couldn't reach Blender"), "{r}");
    }
}
