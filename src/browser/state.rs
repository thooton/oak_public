//! What the model manages of the browser beyond the page, and what it
//! records (BROWSER's profiles, cookies, set_cookies, delete_cookies,
//! clear_data, emulate, console and network).
//!
//! Profiles: a browser runs on the conversation's own profile unless the
//! model opens a named one (`browser-profiles/<name>` in Oak's folder),
//! which any conversation can open, one at a time, so a login made once
//! serves them all. Cookies are listed, set, deleted, and saved to or
//! loaded from files: JSON (Chromium's cookie objects; Playwright's storage
//! state and browser extensions' exports load too) or Netscape cookies.txt,
//! which curl and wget read. Emulate overrides what the page sees of its
//! surroundings (user agent, locale, timezone, location, color scheme,
//! offline, extra headers, permissions) until reset or closed; a restart
//! applies them again. The network log runs from the start (pages can't
//! tell); the console only from the model's first call for it, since
//! turning on the Runtime domain is something sites that look for
//! automation can notice.

use super::{Instance, OPEN, address, closing};
use crate::cdp::Cdp;
use base64::Engine as _;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// The most of a request's posted data Chromium hands over with it.
pub(super) const POST_MAX: usize = 65_536;
/// Requests kept (the newest).
const NET_MAX: usize = 500;
/// Requests listed at once (the newest that match).
const NET_LISTED: usize = 50;
/// Console lines kept until the model reads them (the newest).
const CONSOLE_MAX: usize = 300;
/// The longest console line.
const LINE_MAX: usize = 2000;
/// The most of a cookie's value a listing shows.
const VALUE_SHOWN: usize = 200;
/// The most of a response body shown (the rest: save it with `path`).
const BODY_SHOWN: usize = 20_000;

fn clip_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect::<String>() + "…" }
}

fn plural(n: usize) -> &'static str {
    crate::plural(n)
}

fn size(n: u64) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{:.1} MB", n as f64 / (1024.0 * 1024.0)),
    }
}

// --------------------------------------------------------------- profiles

fn profiles_root() -> PathBuf {
    crate::store::dir().join("browser-profiles")
}

/// Named profile `name`'s folder (`name` as `profile_name` gives it).
pub(super) fn named_dir(name: &str) -> PathBuf {
    profiles_root().join(name)
}

/// A profile's name, checked and in lower case (so it names one folder
/// on any file system): letters, digits, spaces, `-`, `_` and `.`.
pub(super) fn profile_name(name: &str) -> Result<String, String> {
    let n = name.trim().to_lowercase();
    let reserved = ["con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "lpt1", "lpt2", "lpt3"];
    let ok = !n.is_empty()
        && n.chars().count() <= 64
        && !n.starts_with('.')
        && !n.ends_with('.')
        && n.chars().all(|c| c.is_alphanumeric() || " -_.".contains(c))
        && !reserved.contains(&n.split('.').next().unwrap_or(""));
    if ok {
        Ok(n)
    } else {
        Err(format!("{name:?} can't name a profile: use letters, digits, spaces, -, _ and . (up to 64), not starting or ending with a dot."))
    }
}

/// How long ago `t` was, roughly.
fn ago(t: SystemTime) -> String {
    let s = t.elapsed().map_or(0, |d| d.as_secs());
    match s {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        s => format!("{} day{} ago", s / 86_400, plural((s / 86_400) as usize)),
    }
}

/// BROWSER profiles for conversation `conv`: the named profiles, or with
/// `delete`, deletes one that no browser is on.
pub(crate) fn model_profiles(conv: &str, input: &Value) -> Result<String, String> {
    if let Some(d) = input["delete"].as_str().filter(|d| !d.trim().is_empty()) {
        let name = profile_name(d)?;
        let dir = named_dir(&name);
        if !dir.is_dir() {
            return Err(format!("There is no profile {name:?}."));
        }
        if let Some(i) = OPEN.lock().unwrap().values().find(|i| i.profile == dir) {
            return Err(if i.conv == conv {
                format!("This conversation's browser is open on profile {name:?}: close it first.")
            } else {
                format!("Another conversation's browser is open on profile {name:?}: it can't be deleted until that one closes.")
            });
        }
        for _ in 0..100 {
            if !closing(&dir) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // The browser may hold its files for a moment after it ends.
        let mut err = None;
        for _ in 0..30 {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => return Ok(format!("Deleted profile {name:?}.")),
                Err(_) if !dir.exists() => return Ok(format!("Deleted profile {name:?}.")),
                Err(e) => err = Some(e),
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        return Err(format!("Couldn't delete profile {name:?}: {}", err.map(|e| e.to_string()).unwrap_or_default()));
    }
    let open = OPEN.lock().unwrap();
    let mut out = match open.get(conv).map(|i| i.profile_name.clone()) {
        None => "This conversation's browser is closed.".to_string(),
        Some(None) => "This conversation's browser is open on its own profile.".to_string(),
        Some(Some(n)) => format!("This conversation's browser is open on profile {n:?}."),
    };
    let mut names: Vec<(String, PathBuf)> = std::fs::read_dir(profiles_root())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    names.sort();
    if names.is_empty() {
        out += "\nNo named profiles yet: BROWSER action=open with `profile` makes one, which keeps its logins for every conversation.";
        return Ok(out);
    }
    out += &format!("\n{} named profile{}:", names.len(), plural(names.len()));
    for (name, dir) in names {
        let last = dir.join("oak-last-url");
        let used = std::fs::metadata(&last).or_else(|_| std::fs::metadata(&dir)).and_then(|m| m.modified()).ok();
        let url = std::fs::read_to_string(&last).unwrap_or_default();
        let mut line = format!("\n- {name}");
        if let Some(t) = used {
            line += &format!(" (last used {}", ago(t));
            if !url.trim().is_empty() && url.trim() != "about:blank" {
                line += &format!(", at {}", clip_chars(url.trim(), 120));
            }
            line += ")";
        }
        match open.values().find(|i| i.profile == dir) {
            Some(i) if i.conv == conv => line += ": open here",
            Some(_) => line += ": open in another conversation",
            None => {}
        }
        out += &line;
    }
    Ok(out)
}

// ---------------------------------------------------------------- cookies

/// A cookie as a listing shows it: `name=value (where, until, flags)`.
fn describe_cookie(c: &Value) -> String {
    let value = c["value"].as_str().unwrap_or("");
    let n = value.chars().count();
    let value = if n > VALUE_SHOWN { format!("{}… ({n} characters: save the cookies with `path` for all of it)", clip_chars(value, VALUE_SHOWN)) } else { value.to_string() };
    let mut about = vec![c["domain"].as_str().unwrap_or("").to_string() + c["path"].as_str().unwrap_or("")];
    let expires = c["expires"].as_f64().unwrap_or(-1.0);
    if c["session"] == true || expires <= 0.0 {
        about.push("session".into());
    } else if let Some(t) = chrono::DateTime::from_timestamp(expires as i64, 0) {
        about.push(format!("expires {}", t.format("%Y-%m-%d %H:%M UTC")));
    }
    for (flag, name) in [("httpOnly", "HttpOnly"), ("secure", "Secure")] {
        if c[flag] == true {
            about.push(name.into());
        }
    }
    if let Some(s) = c["sameSite"].as_str() {
        about.push(format!("SameSite={s}"));
    }
    if c["partitionKey"].is_object() || c["partitionKey"].is_string() {
        about.push("partitioned".into());
    }
    format!("{}={value} ({})", c["name"].as_str().unwrap_or(""), about.join(", "))
}

/// Cookies as a Netscape cookies.txt (curl's and wget's format; HttpOnly
/// ones marked as curl does).
fn netscape(cookies: &[Value]) -> String {
    let mut out = String::from("# Netscape HTTP Cookie File\n# Saved by Oak's browser.\n\n");
    for c in cookies {
        let domain = c["domain"].as_str().unwrap_or("");
        let expires = c["expires"].as_f64().filter(|e| *e > 0.0 && c["session"] != true).map_or(0, |e| e as i64);
        let tf = |b: bool| if b { "TRUE" } else { "FALSE" };
        out += &format!(
            "{}{domain}\t{}\t{}\t{}\t{expires}\t{}\t{}\n",
            if c["httpOnly"] == true { "#HttpOnly_" } else { "" },
            tf(domain.starts_with('.')),
            c["path"].as_str().unwrap_or("/"),
            tf(c["secure"] == true),
            c["name"].as_str().unwrap_or(""),
            c["value"].as_str().unwrap_or(""),
        );
    }
    out
}

/// The cookies in a file: JSON (a list of cookies, or an object with one
/// under `cookies`, as Playwright's storage state) or Netscape cookies.txt.
fn parse_cookie_file(text: &str) -> Result<Vec<Value>, String> {
    let t = text.trim_start_matches('\u{feff}').trim();
    if t.starts_with('[') || t.starts_with('{') {
        let v: Value = serde_json::from_str(t).map_err(|e| format!("The file isn't valid JSON: {e}"))?;
        let list = if v.is_array() { v } else { v["cookies"].clone() };
        return list.as_array().cloned().ok_or_else(|| "The JSON has no list of cookies (a list, or an object with one under `cookies`).".to_string());
    }
    let mut out = vec![];
    for line in t.lines() {
        let (line, http_only) = match line.strip_prefix("#HttpOnly_") {
            Some(rest) => (rest, true),
            None => (line, false),
        };
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 7 {
            return Err(format!("Not a cookies.txt line (it takes 7 tab-separated fields): {}", clip_chars(line, 100)));
        }
        let mut c = json!({
            "domain": f[0], "hostOnly": f[1].eq_ignore_ascii_case("false") && !f[0].starts_with('.'),
            "path": f[2], "secure": f[3].eq_ignore_ascii_case("true"), "name": f[5], "value": f[6..].join("\t"), "httpOnly": http_only,
        });
        if let Ok(e) = f[4].trim().parse::<i64>()
            && e > 0
        {
            c["expires"] = json!(e);
        }
        out.push(c);
    }
    Ok(out)
}

/// A cookie to set, in the form Chromium takes, from the model's or a
/// file's. `default_url`: where one with neither `url` nor `domain` goes.
/// `from_file`: a domain without a leading dot is the host's alone (as
/// exports write it), unless `hostOnly` says otherwise.
fn cookie_param(c: &Value, default_url: Option<&str>, from_file: bool) -> Result<Value, String> {
    let name = c["name"].as_str().ok_or("A cookie without a `name`.")?;
    let value = match &c["value"] {
        Value::String(s) => s.clone(),
        Value::Null => return Err(format!("Cookie {name:?} has no `value`.")),
        v => v.to_string(),
    };
    let mut p = json!({"name": name, "value": value});
    let path = c["path"].as_str().filter(|p| !p.is_empty());
    let secure = c["secure"].as_bool();
    match (c["url"].as_str().filter(|u| !u.is_empty()), c["domain"].as_str().filter(|d| !d.is_empty())) {
        (Some(url), _) => p["url"] = json!(address(url, false)),
        (None, Some(domain)) => {
            let host_only = c["hostOnly"].as_bool().unwrap_or(from_file && !domain.starts_with('.'));
            if host_only {
                let scheme = if secure == Some(false) { "http" } else { "https" };
                p["url"] = json!(format!("{scheme}://{}{}", domain.trim_start_matches('.'), path.unwrap_or("/")));
            } else {
                p["domain"] = json!(domain);
            }
        }
        (None, None) => match default_url {
            Some(u) => p["url"] = json!(u),
            None => return Err(format!("Cookie {name:?}: give `url` or `domain` (the page isn't on a website).")),
        },
    }
    if let Some(path) = path {
        p["path"] = json!(path);
    }
    if let Some(s) = secure {
        p["secure"] = json!(s);
    }
    if let Some(h) = c["httpOnly"].as_bool() {
        p["httpOnly"] = json!(h);
    }
    let same = match c["sameSite"].as_str().map(str::to_lowercase).as_deref() {
        Some("strict") => Some("Strict"),
        Some("lax") => Some("Lax"),
        Some("none" | "no_restriction") => Some("None"),
        _ => None,
    };
    if let Some(s) = same {
        p["sameSite"] = json!(s);
    }
    let expires = c["expires"].as_f64().or_else(|| c["expirationDate"].as_f64());
    if let Some(e) = expires.filter(|e| *e > 0.0 && c["session"] != true) {
        p["expires"] = json!(e);
    }
    if c["partitionKey"].is_object() {
        p["partitionKey"] = c["partitionKey"].clone();
    }
    Ok(p)
}

/// Whether cookie `c` belongs to `domain` or one of its subdomains.
fn on_domain(c: &Value, domain: &str) -> bool {
    let d = domain.trim().trim_start_matches('.').to_lowercase();
    let cd = c["domain"].as_str().unwrap_or("").trim_start_matches('.').to_lowercase();
    cd == d || cd.ends_with(&format!(".{d}"))
}

impl Instance {
    fn root_cdp(&self) -> Result<Arc<Cdp>, String> {
        self.root.lock().unwrap().clone().ok_or_else(|| "The browser's own DevTools session isn't up; restart the browser.".into())
    }

    /// The page's address, if it is on a website.
    fn site(&self) -> Option<String> {
        let u = self.view.lock().unwrap().url.clone();
        (u.starts_with("http://") || u.starts_with("https://")).then_some(u)
    }

    /// `url` from the input, else the page's: for calls about a site.
    fn site_url(&self, input: &Value) -> Result<String, String> {
        if let Some(u) = input["url"].as_str().map(str::trim).filter(|u| !u.is_empty()) {
            return Ok(address(u, false));
        }
        self.site().ok_or_else(|| {
            let u = self.view.lock().unwrap().url.clone();
            format!("The page ({}) isn't on a website: give `url`.", if u.is_empty() { "blank" } else { &u })
        })
    }

    fn all_cookies(&self, page: &Cdp) -> Result<Vec<Value>, String> {
        let r = match self.root_cdp().and_then(|r| r.call("Storage.getCookies", json!({})).map_err(|e| e.to_string())) {
            Ok(r) => r,
            Err(_) => page.call("Network.getAllCookies", json!({})).map_err(|e| e.to_string())?,
        };
        Ok(r["cookies"].as_array().cloned().unwrap_or_default())
    }

    fn cookies_for(&self, page: &Cdp, url: &str) -> Result<Vec<Value>, String> {
        let r = page.call("Network.getCookies", json!({"urls": [url]})).map_err(|e| e.to_string())?;
        Ok(r["cookies"].as_array().cloned().unwrap_or_default())
    }

    /// BROWSER cookies: those `url` (default the page's) gets, or all with
    /// `all`; with `path`, saved there instead of shown.
    pub(super) fn cookies(&self, page: &Cdp, input: &Value, folder: &Path) -> Result<String, String> {
        let (cookies, whose) = if input["all"] == true {
            (self.all_cookies(page)?, "every site".to_string())
        } else {
            let url = self.site_url(input)?;
            (self.cookies_for(page, &url)?, url)
        };
        let n = cookies.len();
        if let Some(p) = input["path"].as_str().map(str::trim).filter(|p| !p.is_empty()) {
            let path = crate::tools::resolve(folder, p);
            let json_file = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
            let text = if json_file { serde_json::to_string_pretty(&cookies).unwrap_or_default() } else { netscape(&cookies) };
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
            let format = if json_file { "JSON" } else { "Netscape cookies.txt, which curl -b and wget --load-cookies read" };
            return Ok(format!("Saved {n} cookie{} of {whose} to {} ({format}).", plural(n), path.display()));
        }
        if cookies.is_empty() {
            return Ok(format!("No cookies for {whose}.") + &self.notes());
        }
        let mut out = format!("{n} cookie{} for {whose}:", plural(n));
        for c in &cookies {
            out += &format!("\n{}", describe_cookie(c));
        }
        Ok(crate::tools::clip(&out) + &self.notes())
    }

    /// BROWSER set_cookies: `cookies`, or those in the file at `path`.
    pub(super) fn set_cookies(&self, page: &Cdp, input: &Value, folder: &Path) -> Result<String, String> {
        let (list, from_file) = match (input["cookies"].as_array(), input["path"].as_str().map(str::trim).filter(|p| !p.is_empty())) {
            (Some(list), _) => (list.clone(), None),
            (None, Some(p)) => {
                let path = crate::tools::resolve(folder, p);
                let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                (parse_cookie_file(&text)?, Some(path))
            }
            (None, None) => return Err("Give `cookies`, or `path`: a file of them to load (JSON or cookies.txt).".into()),
        };
        let default_url = self.site();
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let (mut set, mut expired, mut failed) = (0usize, 0usize, vec![]);
        for c in &list {
            let p = match cookie_param(c, default_url.as_deref(), from_file.is_some()) {
                Ok(p) => p,
                Err(e) => {
                    failed.push(e);
                    continue;
                }
            };
            if p["expires"].as_f64().is_some_and(|e| e < now) {
                expired += 1;
                continue;
            }
            match page.call("Network.setCookies", json!({"cookies": [p]})) {
                Ok(_) => set += 1,
                Err(e) => failed.push(format!("{:?}: {e}", p["name"].as_str().unwrap_or(""))),
            }
        }
        let mut out = format!("Set {set} cookie{}", plural(set));
        if let Some(p) = from_file {
            out += &format!(" from {}", p.display());
        }
        out += ".";
        if expired > 0 {
            out += &format!(" Skipped {expired} that had expired.");
        }
        if !failed.is_empty() {
            out += &format!("\n{} couldn't be set:", failed.len());
            for f in failed.iter().take(10) {
                out += &format!("\n- {f}");
            }
            if failed.len() > 10 {
                out += &format!("\n- … and {} more", failed.len() - 10);
            }
        }
        if set > 0 {
            out += "\nReload the page for it to use them.";
        }
        Ok(out + &self.notes())
    }

    /// BROWSER delete_cookies: cookie `name`, or every cookie of `url` or
    /// `domain`, or with `all` every cookie.
    pub(super) fn delete_cookies(&self, page: &Cdp, input: &Value) -> Result<String, String> {
        let s = |k: &str| input[k].as_str().map(str::trim).filter(|s| !s.is_empty());
        let (name, domain, url, all) = (s("name"), s("domain"), s("url"), input["all"] == true);
        let before = self.all_cookies(page)?.len();
        if all && name.is_none() && domain.is_none() && url.is_none() {
            match self.root_cdp().and_then(|r| r.call("Storage.clearCookies", json!({})).map_err(|e| e.to_string())) {
                Ok(_) => {}
                Err(_) => {
                    page.call("Network.clearBrowserCookies", json!({})).map_err(|e| e.to_string())?;
                }
            }
            return Ok(format!("Deleted every cookie ({before}).") + &self.notes());
        }
        if name.is_none() && domain.is_none() && url.is_none() {
            return Err("Give `name` (for `url` or `domain`, else the page's site), `url` or `domain` (all of theirs), or `all` for every cookie.".into());
        }
        let mut targets = match (domain, url) {
            (Some(d), _) => self.all_cookies(page)?.into_iter().filter(|c| on_domain(c, d)).collect::<Vec<_>>(),
            (None, Some(u)) => self.cookies_for(page, &address(u, false))?,
            (None, None) => self.cookies_for(page, &self.site_url(&json!({}))?)?,
        };
        if let Some(n) = name {
            targets.retain(|c| c["name"] == n);
        }
        for c in &targets {
            let mut p = json!({"name": c["name"], "domain": c["domain"], "path": c["path"]});
            if c["partitionKey"].is_object() {
                p["partitionKey"] = c["partitionKey"].clone();
            }
            page.call("Network.deleteCookies", p).map_err(|e| e.to_string())?;
        }
        let gone = before.saturating_sub(self.all_cookies(page)?.len());
        let which = match (name, domain, url) {
            (Some(n), _, _) => format!("named {n:?}"),
            (None, Some(d), _) => format!("of {d}"),
            (None, None, Some(u)) => format!("for {u}"),
            _ => String::new(),
        };
        Ok(format!("Deleted {gone} cookie{} {which}.", plural(gone)) + &self.notes())
    }

    /// BROWSER clear_data: `origin`'s (default the page's) cookies and
    /// storage, or the HTTP cache.
    pub(super) fn clear_data(&self, page: &Cdp, input: &Value) -> Result<String, String> {
        let what = input["what"].as_str().unwrap_or("all");
        if what == "cache" {
            page.call("Network.clearBrowserCache", json!({})).map_err(|e| e.to_string())?;
            return Ok("Cleared the HTTP cache (every site's).".to_string() + &self.notes());
        }
        let types = match what {
            "all" => "all",
            "cookies" => "cookies",
            "storage" => "local_storage,indexeddb,cache_storage,service_workers,file_systems,websql",
            other => return Err(format!("Unknown `what` {other:?}: use all, cookies, storage or cache.")),
        };
        let u = match input["origin"].as_str().map(str::trim).filter(|o| !o.is_empty()) {
            Some(o) => address(o, false),
            None => self.site_url(&json!({}))?,
        };
        let origin = reqwest::Url::parse(&u).map_err(|e| format!("{u}: {e}"))?.origin();
        if !origin.is_tuple() {
            return Err(format!("{u} has no origin to clear."));
        }
        let origin = origin.ascii_serialization();
        page.call("Storage.clearDataForOrigin", json!({"origin": origin, "storageTypes": types})).map_err(|e| e.to_string())?;
        // Session storage is the page's own, not the origin's: cleared in it.
        if what != "cookies" && self.site().and_then(|s| reqwest::Url::parse(&s).ok()).is_some_and(|s| s.origin().ascii_serialization() == origin) {
            let _ = self.eval(page, "sessionStorage.clear()", Duration::from_secs(5));
        }
        let cleared = match what {
            "cookies" => "the cookies",
            "storage" => "the storage (local and session storage, IndexedDB, caches, service workers)",
            _ => "the cookies and storage",
        };
        Ok(format!("Cleared {cleared} of {origin}. The page may still hold what it had read: reload it to start afresh.") + &self.notes())
    }
}

// ---------------------------------------------------------------- emulate

/// The model's overrides (empty or None: Chromium's own).
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Emulation {
    user_agent: String,
    locale: String,
    timezone: String,
    geo: Option<(f64, f64)>,
    color_scheme: String,
    offline: bool,
    headers: Map<String, Value>,
    permissions: Vec<String>,
}

impl Emulation {
    fn describe(&self) -> String {
        let mut parts = vec![];
        if !self.user_agent.is_empty() {
            parts.push(format!("user agent {:?}", self.user_agent));
        }
        if !self.locale.is_empty() {
            parts.push(format!("locale {}", self.locale));
        }
        if !self.timezone.is_empty() {
            parts.push(format!("timezone {}", self.timezone));
        }
        if let Some((lat, lon)) = self.geo {
            parts.push(format!("location {lat}, {lon}"));
        }
        if !self.color_scheme.is_empty() {
            parts.push(format!("{} color scheme", self.color_scheme));
        }
        if self.offline {
            parts.push("offline".into());
        }
        if !self.headers.is_empty() {
            parts.push(format!("extra headers {}", self.headers.keys().cloned().collect::<Vec<_>>().join(", ")));
        }
        if !self.permissions.is_empty() {
            parts.push(format!("permissions granted: {}", self.permissions.join(", ")));
        }
        if parts.is_empty() { "No overrides: the browser's own settings.".into() } else { format!("In effect: {}.", parts.join("; ")) }
    }
}

/// An Accept-Language for `locale`: "fr-FR" → "fr-FR,fr;q=0.9".
fn accept_language(locale: &str) -> String {
    match locale.split_once(['-', '_']) {
        Some((lang, _)) => format!("{locale},{lang};q=0.9"),
        None => locale.to_string(),
    }
}

impl Instance {
    /// Applies overrides `e` to the page, those that differ from `base`
    /// (what is in effect now): the user agent always (it is how the page
    /// passes for a plain Chrome).
    pub(super) fn apply_emulation_from(&self, page: &Cdp, e: &Emulation, base: &Emulation) -> Result<(), String> {
        let call = |m: &str, p: Value| page.call(m, p).map(|_| ()).map_err(|err| err.to_string());
        let ua = if e.user_agent.is_empty() { self.user_agent.lock().unwrap().clone() } else { e.user_agent.clone() };
        if !ua.is_empty() {
            let mut p = json!({"userAgent": ua});
            if !e.locale.is_empty() {
                p["acceptLanguage"] = json!(accept_language(&e.locale));
            }
            call("Emulation.setUserAgentOverride", p)?;
        }
        if e.locale != base.locale {
            let _ = call("Emulation.setLocaleOverride", json!({}));
            if !e.locale.is_empty() {
                call("Emulation.setLocaleOverride", json!({"locale": e.locale})).map_err(|err| format!("locale {:?}: {err}", e.locale))?;
            }
        }
        if e.timezone != base.timezone {
            let r = call("Emulation.setTimezoneOverride", json!({"timezoneId": e.timezone}));
            if !e.timezone.is_empty() {
                r.map_err(|err| format!("timezone {:?}: {err}", e.timezone))?;
            }
        }
        if e.geo != base.geo {
            match e.geo {
                Some((lat, lon)) => call("Emulation.setGeolocationOverride", json!({"latitude": lat, "longitude": lon, "accuracy": 20}))?,
                None => {
                    let _ = call("Emulation.clearGeolocationOverride", json!({}));
                }
            }
        }
        if e.color_scheme != base.color_scheme {
            call("Emulation.setEmulatedMedia", json!({"features": [{"name": "prefers-color-scheme", "value": e.color_scheme}]}))?;
        }
        if e.offline != base.offline {
            call("Network.emulateNetworkConditions", json!({"offline": e.offline, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1}))?;
        }
        if e.headers != base.headers {
            call("Network.setExtraHTTPHeaders", json!({"headers": e.headers}))?;
        }
        let perms = |e: &Emulation| {
            let mut p = e.permissions.clone();
            if e.geo.is_some() && !p.iter().any(|x| x == "geolocation") {
                p.push("geolocation".into());
            }
            p
        };
        if perms(e) != perms(base) {
            let root = self.root_cdp()?;
            let _ = root.call("Browser.resetPermissions", json!({}));
            if !perms(e).is_empty() {
                root.call("Browser.grantPermissions", json!({"permissions": perms(e)})).map_err(|err| format!("permissions: {err}"))?;
            }
        }
        Ok(())
    }

    /// Applies overrides `e` to a page that has none yet.
    pub(super) fn apply_emulation(&self, page: &Cdp, e: &Emulation) -> Result<(), String> {
        self.apply_emulation_from(page, e, &Emulation::default())
    }

    /// BROWSER emulate: changes the overrides (`reset` clears them first),
    /// and says which are in effect.
    pub(super) fn emulate(&self, page: &Cdp, input: &Value) -> Result<String, String> {
        let old = self.emulation.lock().unwrap().clone();
        let mut e = if input["reset"] == true { Emulation::default() } else { old.clone() };
        let s = |k: &str| input[k].as_str().map(|s| s.trim().to_string());
        if let Some(v) = s("user_agent") {
            e.user_agent = v;
        }
        if let Some(v) = s("locale") {
            e.locale = v;
        }
        if let Some(v) = s("timezone") {
            e.timezone = v;
        }
        match (input["latitude"].as_f64(), input["longitude"].as_f64()) {
            (Some(lat), Some(lon)) => {
                if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                    return Err("`latitude` is -90 to 90 and `longitude` -180 to 180.".into());
                }
                e.geo = Some((lat, lon));
            }
            (None, None) => {}
            _ => return Err("Give both `latitude` and `longitude`.".into()),
        }
        if let Some(v) = s("color_scheme") {
            let v = v.to_lowercase();
            if !["", "light", "dark"].contains(&v.as_str()) {
                return Err(format!("Unknown color_scheme {v:?}: use light or dark (\"\" for the browser's own)."));
            }
            e.color_scheme = v;
        }
        if let Some(v) = input["offline"].as_bool() {
            e.offline = v;
        }
        match &input["headers"] {
            Value::Null => {}
            Value::Object(h) => {
                e.headers = h.iter().map(|(k, v)| (k.clone(), json!(v.as_str().map_or_else(|| v.to_string(), String::from)))).collect();
            }
            _ => return Err("`headers` is an object of header names and values ({} for none).".into()),
        }
        if let Some(p) = input["permissions"].as_array() {
            e.permissions = p.iter().filter_map(|x| x.as_str()).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
        }
        // Chromium takes any timezone without a word (and runs on UTC): the
        // page's Intl checks it, and the locale, first.
        if !e.timezone.is_empty() && e.timezone != old.timezone {
            let js = format!("new Intl.DateTimeFormat('en', {{timeZone: {}}}).resolvedOptions().timeZone", json!(e.timezone));
            self.eval(page, &js, Duration::from_secs(5)).map_err(|_| format!("Unknown timezone {:?}: use an IANA name like Europe/Paris or America/New_York.", e.timezone))?;
        }
        if !e.locale.is_empty() && e.locale != old.locale {
            let js = format!("Intl.getCanonicalLocales({})[0]", json!(e.locale));
            self.eval(page, &js, Duration::from_secs(5)).map_err(|_| format!("{:?} isn't a locale: use a tag like fr-FR or ja.", e.locale))?;
        }
        if let Err(err) = self.apply_emulation_from(page, &e, &old) {
            // Back to what was in effect, as far as it goes.
            let _ = self.apply_emulation_from(page, &old, &e);
            return Err(format!("Couldn't apply the overrides ({err}); they are as they were. {}", old.describe()));
        }
        *self.emulation.lock().unwrap() = e.clone();
        let later = if e == old { "" } else { " Requests from now on see them; reload the page for its scripts to start with them." };
        Ok(format!("{}{later}", e.describe()) + &self.notes())
    }
}

// ---------------------------------------------------------------- console

/// The page's console messages and errors, and the browser's own (failed
/// loads, blocked content), until the model reads them.
#[derive(Default)]
pub(super) struct Console {
    lines: VecDeque<String>,
    dropped: usize,
}

/// A logged value, as a console shows it: strings bare, objects by their
/// preview.
fn remote(o: &Value) -> String {
    if let Some(s) = o["value"].as_str() {
        return s.to_string();
    }
    if !o["value"].is_null() {
        return o["value"].to_string();
    }
    if let Some(u) = o["unserializableValue"].as_str() {
        return u.to_string();
    }
    let desc = o["description"].as_str().unwrap_or("");
    if let Some(props) = o["preview"]["properties"].as_array()
        && o["subtype"] != "error"
    {
        let array = o["subtype"] == "array";
        let mut body: Vec<String> = props
            .iter()
            .map(|p| {
                let v = p["value"].as_str().unwrap_or("…");
                let v = if p["type"] == "string" { format!("{v:?}") } else { v.to_string() };
                if array { v } else { format!("{}: {v}", p["name"].as_str().unwrap_or("")) }
            })
            .collect();
        if o["preview"]["overflow"] == true {
            body.push("…".into());
        }
        let body = body.join(", ");
        return if array {
            format!("[{body}]")
        } else if desc == "Object" || desc.is_empty() {
            format!("{{{body}}}")
        } else {
            format!("{desc} {{{body}}}")
        };
    }
    if !desc.is_empty() {
        return desc.to_string();
    }
    match o["type"].as_str() {
        _ if o["subtype"] == "null" => "null".into(),
        Some(t) => t.into(),
        None => String::new(),
    }
}

/// " (url:line)", where a message came from.
fn at(url: &Value, line: &Value) -> String {
    match url.as_str().filter(|u| !u.is_empty()) {
        Some(u) => match line.as_i64() {
            Some(l) => format!(" ({}:{})", clip_chars(u, 150), l + 1),
            None => format!(" ({})", clip_chars(u, 150)),
        },
        None => String::new(),
    }
}

impl Instance {
    /// An event for the console or the network log, if it is one.
    pub(super) fn record(&self, method: &str, p: &Value) {
        if method.starts_with("Network.") {
            self.net.lock().unwrap().event(method, p);
            return;
        }
        let line = match method {
            "Runtime.consoleAPICalled" => {
                let text = p["args"].as_array().into_iter().flatten().map(remote).collect::<Vec<_>>().join(" ");
                let f = &p["stackTrace"]["callFrames"][0];
                format!("[{}] {text}{}", p["type"].as_str().unwrap_or("log"), at(&f["url"], &f["lineNumber"]))
            }
            "Runtime.exceptionThrown" => {
                let d = &p["exceptionDetails"];
                let text = d["exception"]["description"].as_str().or(d["text"].as_str()).unwrap_or("error");
                let text = text.lines().take(6).collect::<Vec<_>>().join("\n    ");
                format!("[uncaught] {text}{}", at(&d["url"], &d["lineNumber"]))
            }
            "Log.entryAdded" => {
                let e = &p["entry"];
                if e["level"] == "verbose" {
                    return;
                }
                format!("[{} {}] {}{}", e["level"].as_str().unwrap_or("info"), e["source"].as_str().unwrap_or("other"), e["text"].as_str().unwrap_or(""), at(&e["url"], &e["lineNumber"]))
            }
            _ => return,
        };
        if let Some(c) = self.console.lock().unwrap().as_mut() {
            if c.lines.len() >= CONSOLE_MAX {
                c.lines.pop_front();
                c.dropped += 1;
            }
            c.lines.push_back(clip_chars(&line, LINE_MAX));
        }
    }

    /// BROWSER console: what was logged since the last call; the first
    /// starts recording (with what the page logged before it).
    pub(super) fn console(&self, page: &Cdp) -> Result<String, String> {
        let first = {
            let mut c = self.console.lock().unwrap();
            let first = c.is_none();
            c.get_or_insert_with(Console::default);
            first
        };
        if first {
            if let Err(e) = page.call("Runtime.enable", json!({})) {
                *self.console.lock().unwrap() = None;
                return Err(e.to_string());
            }
            let _ = page.call("Log.enable", json!({}));
            // What the page logged before comes as events: a moment for them.
            std::thread::sleep(Duration::from_millis(400));
        }
        let (lines, dropped) = {
            let mut g = self.console.lock().unwrap();
            let c = g.as_mut().expect("recording");
            (c.lines.drain(..).collect::<Vec<_>>(), std::mem::take(&mut c.dropped))
        };
        let mut out = String::new();
        if first {
            out += "The console is recorded from now on, until the browser closes.\n";
        }
        if lines.is_empty() {
            out += if first { "Nothing logged." } else { "Nothing logged since the last call." };
        } else {
            out += &format!("{} message{}{}:", lines.len(), plural(lines.len()), if first { "" } else { " since the last call" });
            if dropped > 0 {
                out += &format!(" ({dropped} older one{} dropped)", plural(dropped));
            }
            for l in lines {
                out += &format!("\n{l}");
            }
        }
        Ok(crate::tools::clip(&out) + &self.notes())
    }
}

// ---------------------------------------------------------------- network

/// One request the page made.
#[derive(Clone, Debug, Default)]
pub(super) struct Req {
    n: u64,
    id: String,
    method: String,
    url: String,
    kind: String,
    status: Option<i64>,
    status_text: String,
    mime: String,
    size: Option<u64>,
    error: Option<String>,
    cached: bool,
    req_headers: Value,
    resp_headers: Value,
    post: Option<String>,
    has_post: bool,
}

impl Req {
    fn line(&self) -> String {
        let status = match (&self.error, self.status) {
            (Some(_), _) => "failed".to_string(),
            (None, Some(s)) => s.to_string(),
            (None, None) => "…".to_string(),
        };
        let mut l = format!("#{} {} {status} {}", self.n, self.method, self.kind.to_lowercase());
        if let Some(s) = self.size {
            l += &format!(" {}", size(s));
        }
        l += &format!(" {}", clip_chars(&self.url, 200));
        if self.cached {
            l += " (cached)";
        }
        if let Some(e) = &self.error {
            l += &format!(" ({e})");
        }
        l
    }
}

/// The page's requests, the newest NET_MAX.
#[derive(Default)]
pub(super) struct NetLog {
    next: u64,
    reqs: VecDeque<Req>,
}

impl NetLog {
    fn find(&mut self, id: &str) -> Option<&mut Req> {
        self.reqs.iter_mut().rev().find(|r| r.id == id)
    }

    fn event(&mut self, method: &str, p: &Value) {
        let id = p["requestId"].as_str().unwrap_or("");
        match method {
            "Network.requestWillBeSent" => {
                // A redirect: the request before it ends with its response.
                if p["redirectResponse"].is_object()
                    && let Some(r) = self.find(id)
                {
                    let rr = &p["redirectResponse"];
                    r.status = rr["status"].as_i64();
                    r.status_text = rr["statusText"].as_str().unwrap_or("").to_string();
                    r.resp_headers = rr["headers"].clone();
                }
                let q = &p["request"];
                let url = format!("{}{}", q["url"].as_str().unwrap_or(""), q["urlFragment"].as_str().unwrap_or(""));
                self.next += 1;
                self.reqs.push_back(Req {
                    n: self.next,
                    id: id.to_string(),
                    method: q["method"].as_str().unwrap_or("GET").to_string(),
                    url: clip_chars(&url, 4096),
                    kind: p["type"].as_str().unwrap_or("Other").to_string(),
                    req_headers: q["headers"].clone(),
                    post: q["postData"].as_str().map(String::from),
                    has_post: q["hasPostData"] == true,
                    ..Default::default()
                });
                if self.reqs.len() > NET_MAX {
                    self.reqs.pop_front();
                }
            }
            "Network.requestWillBeSentExtraInfo" => {
                // The headers as sent, cookies included.
                if let Some(r) = self.find(id)
                    && p["headers"].is_object()
                {
                    r.req_headers = p["headers"].clone();
                }
            }
            "Network.responseReceived" => {
                if let Some(r) = self.find(id) {
                    let rs = &p["response"];
                    r.status = rs["status"].as_i64();
                    r.status_text = rs["statusText"].as_str().unwrap_or("").to_string();
                    r.mime = rs["mimeType"].as_str().unwrap_or("").to_string();
                    if !r.resp_headers.is_object() || r.resp_headers.as_object().is_some_and(|h| h.is_empty()) {
                        r.resp_headers = rs["headers"].clone();
                    }
                    r.cached |= rs["fromDiskCache"] == true || rs["fromServiceWorker"] == true || rs["fromPrefetchCache"] == true;
                    if let Some(k) = p["type"].as_str() {
                        r.kind = k.to_string();
                    }
                }
            }
            "Network.responseReceivedExtraInfo" => {
                // The headers as received, Set-Cookie included.
                if let Some(r) = self.find(id)
                    && p["headers"].is_object()
                {
                    r.resp_headers = p["headers"].clone();
                }
            }
            "Network.requestServedFromCache" => {
                if let Some(r) = self.find(id) {
                    r.cached = true;
                }
            }
            "Network.loadingFinished" => {
                if let Some(r) = self.find(id) {
                    r.size = p["encodedDataLength"].as_f64().map(|s| s as u64);
                }
            }
            "Network.loadingFailed" => {
                if let Some(r) = self.find(id) {
                    let mut e = p["errorText"].as_str().unwrap_or("failed").to_string();
                    if p["canceled"] == true {
                        e = "canceled".into();
                    }
                    if let Some(b) = p["blockedReason"].as_str() {
                        e += &format!(", blocked: {b}");
                    }
                    r.error = Some(e);
                }
            }
            _ => {}
        }
    }
}

fn headers_text(h: &Value) -> String {
    match h.as_object() {
        Some(h) if !h.is_empty() => h.iter().map(|(k, v)| format!("\n  {k}: {}", clip_chars(v.as_str().unwrap_or(&v.to_string()), 2000))).collect(),
        _ => "\n  (none)".into(),
    }
}

impl Instance {
    /// BROWSER network: the newest requests (those `filter` matches), or
    /// request `id` in full, its body saved to `path` if given.
    pub(super) fn network(&self, page: &Cdp, input: &Value, folder: &Path) -> Result<String, String> {
        if let Some(n) = input["id"].as_u64() {
            return self.request(page, n, input["path"].as_str().map(str::trim).filter(|p| !p.is_empty()), folder);
        }
        let filter = input["filter"].as_str().unwrap_or("").trim().to_lowercase();
        let log = self.net.lock().unwrap();
        if log.reqs.is_empty() {
            return Ok("No requests since the browser opened.".to_string() + &self.notes());
        }
        let hits: Vec<&Req> = log
            .reqs
            .iter()
            .filter(|r| filter.is_empty() || format!("{} {} {} {}", r.method, r.status.map_or(String::new(), |s| s.to_string()), r.kind, r.url).to_lowercase().contains(&filter))
            .collect();
        let total = hits.len();
        if total == 0 {
            return Ok(format!("No requests match {filter:?} (of {} recorded).", log.reqs.len()));
        }
        let mut out = format!("{total} request{}{}", plural(total), if filter.is_empty() { String::new() } else { format!(" matching {filter:?}") });
        if total > NET_LISTED {
            out += &format!(", the newest {NET_LISTED} shown");
        }
        out += "; `id` shows one in full (headers and body):";
        for r in &hits[total.saturating_sub(NET_LISTED)..] {
            out += &format!("\n{}", r.line());
        }
        drop(log);
        Ok(crate::tools::clip(&out) + &self.notes())
    }

    /// Request `n` in full: headers, posted data, the response's body (or
    /// saved to `path`).
    fn request(&self, page: &Cdp, n: u64, path: Option<&str>, folder: &Path) -> Result<String, String> {
        let r = self.net.lock().unwrap().reqs.iter().find(|r| r.n == n).cloned().ok_or(format!("No request #{n} (the log keeps the newest {NET_MAX})."))?;
        let mut out = format!("#{n} {} {}\nType: {}", r.method, r.url, r.kind);
        match (&r.error, r.status) {
            (Some(e), _) => out += &format!("\nFailed: {e}"),
            (None, Some(s)) => out += &format!("\nStatus: {s} {}{}", r.status_text, if r.cached { " (from cache)" } else { "" }),
            (None, None) => out += "\nNo response yet.",
        }
        if !r.mime.is_empty() {
            out += &format!("\nContent type: {}", r.mime);
        }
        if let Some(s) = r.size {
            out += &format!("\nTransferred: {}", size(s));
        }
        out += &format!("\nRequest headers:{}", headers_text(&r.req_headers));
        if r.has_post || r.post.is_some() {
            let post = match r.post.clone() {
                Some(p) => Some(p),
                None => page.call("Network.getRequestPostData", json!({"requestId": r.id})).ok().and_then(|v| v["postData"].as_str().map(String::from)),
            };
            out += &match post {
                Some(p) => format!("\nPosted ({} characters):\n{}", p.chars().count(), clip_chars(&p, 5000)),
                None => "\nPosted data: not available.".into(),
            };
        }
        if r.status.is_some() {
            out += &format!("\nResponse headers:{}", headers_text(&r.resp_headers));
        }
        if r.status.is_none() || r.error.is_some() {
            return Ok(out);
        }
        let body = page.call("Network.getResponseBody", json!({"requestId": r.id}));
        let bytes = match &body {
            Ok(b) if b["base64Encoded"] == true => super::B64.decode(b["body"].as_str().unwrap_or("")).map_err(|e| e.to_string()),
            Ok(b) => Ok(b["body"].as_str().unwrap_or("").as_bytes().to_vec()),
            Err(e) => Err(e.to_string()),
        };
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                let why = if (300..400).contains(&r.status.unwrap_or(0)) { "a redirect has none".to_string() } else { e };
                if path.is_some() {
                    return Err(format!("{out}\nThe body isn't available ({why}), so nothing was saved."));
                }
                return Ok(format!("{out}\nThe body isn't available ({why})."));
            }
        };
        if let Some(p) = path {
            let file = crate::tools::resolve(folder, p);
            if let Some(dir) = file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            std::fs::write(&file, &bytes).map_err(|e| format!("{}: {e}", file.display()))?;
            return Ok(format!("{out}\nSaved the body ({}) to {}.", size(bytes.len() as u64), file.display()));
        }
        match String::from_utf8(bytes) {
            Ok(text) if text.chars().count() > BODY_SHOWN => Ok(format!(
                "{out}\nBody ({} characters; the first {BODY_SHOWN} shown, save it all with `path`):\n{}",
                text.chars().count(),
                clip_chars(&text, BODY_SHOWN)
            )),
            Ok(text) if text.is_empty() => Ok(format!("{out}\nNo body.")),
            Ok(text) => Ok(format!("{out}\nBody:\n{text}")),
            Err(e) => Ok(format!("{out}\nThe body is binary ({}): save it with `path`.", size(e.into_bytes().len() as u64))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names() {
        assert_eq!(profile_name("  GitHub Work ").unwrap(), "github work");
        assert_eq!(profile_name("a.b-c_d").unwrap(), "a.b-c_d");
        for bad in ["", ".x", "x.", "a/b", "..", "con", "nul.txt", "a\\b", &"x".repeat(65)] {
            assert!(profile_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn cookie_files() {
        let txt = "# Netscape HTTP Cookie File\n\n.example.com\tTRUE\t/\tTRUE\t1999999999\tsid\tabc\n#HttpOnly_www.example.com\tFALSE\t/app\tFALSE\t0\ttok\tx\ty\n";
        let c = parse_cookie_file(txt).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!((c[0]["domain"].as_str(), c[0]["hostOnly"].as_bool(), c[0]["expires"].as_i64()), (Some(".example.com"), Some(false), Some(1999999999)));
        assert_eq!((c[1]["httpOnly"].as_bool(), c[1]["hostOnly"].as_bool(), c[1]["value"].as_str()), (Some(true), Some(true), Some("x\ty")));
        assert!(c[1]["expires"].is_null());
        // Back to cookies.txt the same.
        let cdp = json!([{"name": "sid", "value": "abc", "domain": ".example.com", "path": "/", "expires": 1999999999.5, "secure": true, "httpOnly": false, "session": false}]);
        let out = netscape(cdp.as_array().unwrap());
        assert!(out.contains(".example.com\tTRUE\t/\tTRUE\t1999999999\tsid\tabc\n"), "{out}");
        // Playwright's storage state, and a plain list.
        assert_eq!(parse_cookie_file(r#"{"cookies": [{"name": "a", "value": "1"}], "origins": []}"#).unwrap().len(), 1);
        assert_eq!(parse_cookie_file(r#"[{"name": "a", "value": "1"}]"#).unwrap().len(), 1);
        assert!(parse_cookie_file("{}").is_err());
        assert!(parse_cookie_file("not\ta\tcookie").is_err());
    }

    #[test]
    fn cookie_params() {
        // From a file: a domain without a dot is the host's alone.
        let p = cookie_param(&json!({"name": "a", "value": "1", "domain": "www.x.com", "path": "/p", "secure": true}), None, true).unwrap();
        assert_eq!((p["url"].as_str(), p.get("domain")), (Some("https://www.x.com/p"), None));
        // The model's: a domain is a domain cookie.
        let p = cookie_param(&json!({"name": "a", "value": "1", "domain": "x.com"}), None, false).unwrap();
        assert_eq!(p["domain"], "x.com");
        // An extension's export.
        let p = cookie_param(&json!({"name": "a", "value": "1", "domain": ".x.com", "hostOnly": false, "sameSite": "no_restriction", "expirationDate": 2e9, "session": false}), None, true).unwrap();
        assert_eq!((p["domain"].as_str(), p["sameSite"].as_str(), p["expires"].as_f64()), (Some(".x.com"), Some("None"), Some(2e9)));
        // Neither url nor domain: the page's, else refused.
        assert_eq!(cookie_param(&json!({"name": "a", "value": 5}), Some("https://y.org/"), false).unwrap()["url"], "https://y.org/");
        assert!(cookie_param(&json!({"name": "a", "value": "1"}), None, false).is_err());
        assert!(cookie_param(&json!({"value": "1"}), None, false).is_err());
    }

    #[test]
    fn network_log() {
        let mut log = NetLog::default();
        log.event("Network.requestWillBeSent", &json!({"requestId": "1", "type": "Document", "request": {"url": "https://a.com/", "method": "GET", "headers": {}}}));
        log.event("Network.requestWillBeSent", &json!({"requestId": "1", "type": "Document", "redirectResponse": {"status": 301, "headers": {"Location": "/b"}}, "request": {"url": "https://a.com/b", "method": "GET", "headers": {}}}));
        log.event("Network.responseReceived", &json!({"requestId": "1", "type": "Document", "response": {"status": 200, "statusText": "OK", "mimeType": "text/html", "headers": {"x": "y"}}}));
        log.event("Network.loadingFinished", &json!({"requestId": "1", "encodedDataLength": 2048.0}));
        log.event("Network.requestWillBeSent", &json!({"requestId": "2", "type": "Fetch", "request": {"url": "https://a.com/api", "method": "POST", "headers": {}, "postData": "{}", "hasPostData": true}}));
        log.event("Network.loadingFailed", &json!({"requestId": "2", "errorText": "net::ERR_FAILED"}));
        let lines: Vec<String> = log.reqs.iter().map(Req::line).collect();
        assert_eq!(lines, ["#1 GET 301 document https://a.com/", "#2 GET 200 document 2.0 KB https://a.com/b", "#3 POST failed fetch https://a.com/api (net::ERR_FAILED)"]);
    }

    /// A tiny HTTP server for the real-browser test: `/` sets a cookie and
    /// logs, `/echo` answers with the request's headers, `/api` with the
    /// body posted to it.
    fn serve() -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut s = stream;
                    let mut buf = vec![];
                    let mut b = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        let Ok(n) = s.read(&mut b) else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend(&b[..n]);
                    }
                    let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    let len = head.lines().find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                    while buf.len() < end + len {
                        let Ok(n) = s.read(&mut b) else { return };
                        if n == 0 {
                            break;
                        }
                        buf.extend(&b[..n]);
                    }
                    let body = String::from_utf8_lossy(&buf[end..]).to_string();
                    let path = head.split(' ').nth(1).unwrap_or("/").to_string();
                    let (ctype, extra, out) = match path.as_str() {
                        "/echo" => ("text/plain", "", head.to_lowercase()),
                        "/api" => ("application/json", "", format!("{{\"ok\":true,\"got\":{}}}", serde_json::to_string(&body).unwrap())),
                        "/favicon.ico" => ("text/plain", "", String::new()),
                        _ => ("text/html", "Set-Cookie: srv=1; Path=/\r\n", "<title>Served</title><p>Hi</p><script>console.log('hello from page', {a: 1})</script>".to_string()),
                    };
                    let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len());
                });
            }
        });
        port
    }

    /// Cookies, site data, emulation, the console and the network log, on
    /// a real browser and a local site. Skipped without a browser.
    #[test]
    fn state_on_a_real_browser() {
        use super::super::{close, find_browser, open_with, tool};
        if find_browser().is_none() {
            eprintln!("no browser: skipped");
            return;
        }
        let port = serve();
        let site = format!("http://127.0.0.1:{port}");
        let dir = std::env::temp_dir().join(format!("oak-browser-state-test-{}", std::process::id()));
        let ctx = eframe::egui::Context::default();
        let conv = format!("state-test-{}", std::process::id());
        let profile = dir.join("profile");
        let _inst = open_with(&conv, &ctx, profile.clone(), None, Default::default()).unwrap();
        // Another conversation can't open a browser on the same profile.
        assert!(open_with("someone-else", &ctx, profile.clone(), None, Default::default()).err().unwrap_or_default().contains("in use"));
        let folder = dir.clone();
        let call = |name: &str, input: Value| {
            let r = tool(&conv, name, &input, &folder);
            println!("== {name} {input}\n{}", match &r { Ok(s) => s.clone(), Err(e) => format!("ERR {e}") });
            r
        };
        let eval = |js: &str| call("BROWSER_EVAL", json!({"script": js})).unwrap();
        call("BROWSER_GOTO", json!({"url": format!("{site}/")})).unwrap();
        // Promises the script evaluates to are awaited, as eval says.
        assert_eq!(eval("Promise.resolve(5)"), "5");
        assert_eq!(eval("new Promise(r => setTimeout(() => r('later'), 50))"), "later");
        assert_eq!(eval("await Promise.resolve(6)"), "6");

        // Cookies: the server's, one set, saved, all deleted, loaded back.
        // (Chromium caps a cookie's life at 400 days.)
        let r = call("BROWSER_COOKIES", json!({})).unwrap();
        assert!(r.contains("srv=1 (127.0.0.1/, session"), "{r}");
        let r = call("BROWSER_SET_COOKIES", json!({"cookies": [{"name": "a", "value": "b", "expires": 4e9}]})).unwrap();
        assert!(r.starts_with("Set 1 cookie."), "{r}");
        assert!(eval("document.cookie").contains("a=b"));
        let r = call("BROWSER_COOKIES", json!({"path": "jar.txt"})).unwrap();
        assert!(r.contains("Saved 2 cookies"), "{r}");
        let jar = std::fs::read_to_string(dir.join("jar.txt")).unwrap();
        assert!(jar.contains("127.0.0.1\tFALSE\t/\tFALSE\t0\tsrv\t1\n") && jar.contains("\ta\tb\n") && !jar.contains("\t0\ta\tb"), "{jar}");
        call("BROWSER_COOKIES", json!({"path": "jar.json"})).unwrap();
        let r = call("BROWSER_DELETE_COOKIES", json!({"all": true})).unwrap();
        assert!(r.contains("Deleted every cookie"), "{r}");
        assert_eq!(eval("document.cookie"), "");
        let r = call("BROWSER_SET_COOKIES", json!({"path": "jar.txt"})).unwrap();
        assert!(r.starts_with("Set 2 cookies from"), "{r}");
        assert!(call("BROWSER_COOKIES", json!({"url": site})).unwrap().contains("a=b"));
        let r = call("BROWSER_DELETE_COOKIES", json!({"name": "a"})).unwrap();
        assert!(r.starts_with("Deleted 1 cookie named"), "{r}");
        call("BROWSER_DELETE_COOKIES", json!({"all": true})).unwrap();
        assert!(call("BROWSER_SET_COOKIES", json!({"path": "jar.json"})).unwrap().starts_with("Set 2 cookies"));
        assert!(call("BROWSER_DELETE_COOKIES", json!({})).is_err());

        // Site data.
        eval("localStorage.x = '1'; sessionStorage.y = '2'");
        let r = call("BROWSER_CLEAR_DATA", json!({"what": "storage"})).unwrap();
        assert!(r.contains(&format!("of {site}")), "{r}");
        assert_eq!(eval("String(localStorage.x) + ' ' + String(sessionStorage.y)"), "undefined undefined");
        assert!(eval("document.cookie").contains("srv=1"), "storage only");
        call("BROWSER_CLEAR_DATA", json!({"what": "cookies"})).unwrap();
        assert_eq!(eval("document.cookie"), "");

        // Emulation.
        let r = call("BROWSER_EMULATE", json!({"timezone": "Asia/Tokyo", "locale": "fr-FR", "color_scheme": "dark", "headers": {"X-Test": "yes"}, "latitude": 48.85, "longitude": 2.35})).unwrap();
        assert!(r.contains("timezone Asia/Tokyo") && !r.contains("permissions"), "{r}");
        call("BROWSER_RELOAD", json!({})).unwrap();
        assert_eq!(eval("Intl.DateTimeFormat().resolvedOptions().timeZone"), "Asia/Tokyo");
        assert_eq!(eval("matchMedia('(prefers-color-scheme: dark)').matches"), "true");
        assert_eq!(eval("navigator.language"), "fr-FR");
        let echo = eval("fetch('/echo').then(r => r.text())");
        assert!(echo.contains("x-test: yes") && echo.contains("accept-language: fr-fr"), "{echo}");
        let geo = eval("new Promise((ok, no) => navigator.geolocation.getCurrentPosition(p => ok(p.coords.latitude + ',' + p.coords.longitude), e => no(new Error(e.message)), {timeout: 5000}))");
        assert_eq!(geo, "48.85,2.35");
        assert!(call("BROWSER_EMULATE", json!({"timezone": "Not/AZone"})).unwrap_err().contains("Unknown timezone"));
        assert!(call("BROWSER_EMULATE", json!({"locale": "not a locale!"})).unwrap_err().contains("isn't a locale"));
        // One Chromium refuses: nothing changes.
        let r = call("BROWSER_EMULATE", json!({"timezone": "Europe/Paris", "permissions": ["bogus"]})).unwrap_err();
        assert!(r.contains("they are as they were") && r.contains("timezone Asia/Tokyo"), "{r}");
        assert_eq!(eval("Intl.DateTimeFormat().resolvedOptions().timeZone"), "Asia/Tokyo");
        let r = call("BROWSER_EMULATE", json!({"offline": true})).unwrap();
        assert!(r.contains("offline"), "{r}");
        assert_eq!(eval("fetch('/echo').then(() => 'online', () => 'offline')"), "offline");
        let r = call("BROWSER_EMULATE", json!({"reset": true})).unwrap();
        assert!(r.starts_with("No overrides"), "{r}");
        assert!(!eval("fetch('/echo').then(r => r.text())").contains("x-test"));
        assert_ne!(eval("Intl.DateTimeFormat().resolvedOptions().timeZone"), "Asia/Tokyo");

        // The console: what the page logged before the first call, then on.
        call("BROWSER_RELOAD", json!({})).unwrap();
        let r = call("BROWSER_CONSOLE", json!({})).unwrap();
        assert!(r.contains("[log] hello from page "), "{r}");
        eval("console.log('live', {b: 2}, [1, 'x']); console.error('boom'); setTimeout(() => { throw new Error('kaboom') }, 0); 1");
        std::thread::sleep(Duration::from_millis(300));
        let r = call("BROWSER_CONSOLE", json!({})).unwrap();
        assert!(r.contains("[log] live {b: 2} [1, \"x\"]") && r.contains("[error] boom") && r.contains("[uncaught] Error: kaboom") && !r.contains("hello from page"), "{r}");
        assert!(call("BROWSER_CONSOLE", json!({})).unwrap().contains("Nothing logged since"));

        // The network log.
        eval("fetch('/api', {method: 'POST', body: 'payload'}).then(r => r.text())");
        // (Listed at once, its status once its response came.)
        let start = std::time::Instant::now();
        let r = loop {
            let r = call("BROWSER_NETWORK", json!({"filter": "/api"})).unwrap();
            if r.contains("POST 200 fetch") || start.elapsed() > Duration::from_secs(5) {
                break r;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let line = r.lines().find(|l| l.contains("POST 200 fetch")).unwrap_or_else(|| panic!("{r}"));
        let id: u64 = line.trim_start_matches('#').split(' ').next().unwrap().parse().unwrap();
        let r = call("BROWSER_NETWORK", json!({"id": id})).unwrap();
        assert!(r.contains("Posted (7 characters):\npayload") && r.contains("{\"ok\":true,\"got\":\"payload\"}") && r.contains("Content type: application/json"), "{r}");
        call("BROWSER_NETWORK", json!({"id": id, "path": "body.json"})).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("body.json")).unwrap(), "{\"ok\":true,\"got\":\"payload\"}");
        let r = call("BROWSER_NETWORK", json!({})).unwrap();
        assert!(r.lines().any(|l| l.contains("GET 200 document") && l.ends_with(&format!(" {site}/"))), "{r}");
        assert!(call("BROWSER_NETWORK", json!({"filter": "INTERNET_DISCONNECTED"})).unwrap().contains("No requests match"), "the filter is over method, status, type and URL");
        assert!(call("BROWSER_NETWORK", json!({"filter": "post"})).unwrap().starts_with("1 request matching"));

        close(&conv);
        std::thread::sleep(Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn console_values() {
        assert_eq!(remote(&json!({"type": "string", "value": "hi"})), "hi");
        assert_eq!(remote(&json!({"type": "number", "value": 3})), "3");
        assert_eq!(remote(&json!({"type": "undefined"})), "undefined");
        assert_eq!(remote(&json!({"type": "object", "subtype": "null", "value": null})), "null");
        let obj = json!({"type": "object", "description": "Object", "preview": {"overflow": false, "properties": [{"name": "a", "type": "number", "value": "1"}, {"name": "b", "type": "string", "value": "x"}]}});
        assert_eq!(remote(&obj), "{a: 1, b: \"x\"}");
        let arr = json!({"type": "object", "subtype": "array", "description": "Array(2)", "preview": {"overflow": true, "properties": [{"name": "0", "type": "number", "value": "1"}]}});
        assert_eq!(remote(&arr), "[1, …]");
    }
}
