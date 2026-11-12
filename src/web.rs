// FETCH and SCREENSHOT: web pages for the model without a browser add-on.
// FETCH turns a page's HTML into readable text (or with `render`, lets
// headless Chrome run the page first); SCREENSHOT has Chrome draw it.
// Each run gets a fresh profile, so the user's browser is never touched.

use crate::tools::{Head, one_line};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How much text one FETCH returns; `start` reads on.
pub const PAGE_CHARS: usize = 30_000;

const AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36";

pub fn fetch(input: &Value) -> Result<String, String> {
    let url = input["url"].as_str().unwrap_or("").trim();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("The url must start with http:// or https://".into());
    }
    let raw = input["raw"].as_bool().unwrap_or(false);
    let start = input["start"].as_u64().unwrap_or(0) as usize;
    let (body, final_url, kind) = if input["render"].as_bool().unwrap_or(false) {
        let wait = input["wait_ms"].as_u64().unwrap_or(2000).min(20_000);
        let out = chrome(&["--dump-dom".into(), format!("--virtual-time-budget={wait}")], url, None)?;
        (String::from_utf8_lossy(&out).into_owned(), url.to_string(), "text/html".to_string())
    } else {
        // One client for every FETCH: one soon after another from the same
        // site reuses its connection instead of a new TLS handshake.
        static CLIENT: std::sync::LazyLock<Result<reqwest::blocking::Client, String>> =
            std::sync::LazyLock::new(|| reqwest::blocking::Client::builder().timeout(Duration::from_secs(30)).user_agent(AGENT).build().map_err(|e| e.to_string()));
        let client = CLIENT.clone()?;
        let resp = client.get(url).send().map_err(|e| format!("Couldn't fetch {url}: {e}"))?;
        let status = resp.status();
        let final_url = resp.url().to_string();
        let kind = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let bytes = resp.bytes().map_err(|e| format!("Couldn't read {url}: {e}"))?;
        if !status.is_success() {
            let text = String::from_utf8_lossy(&bytes);
            return Err(format!("HTTP {status} from {final_url}\n{}", page_text(&text, &final_url).head(2000)));
        }
        if bytes[..bytes.len().min(4096)].contains(&0) || kind.starts_with("image/") || kind.contains("pdf") || kind.contains("octet-stream") {
            return Err(format!(
                "{final_url} is not text ({}, {} bytes). Download it with curl in EXECUTE (READ shows image files).",
                if kind.is_empty() { "unknown type" } else { &kind },
                bytes.len()
            ));
        }
        (String::from_utf8_lossy(&bytes).into_owned(), final_url, kind)
    };
    let html = kind.contains("html") || (kind.is_empty() && body.trim_start().starts_with('<'));
    let text = if html && !raw { page_text(&body, &final_url) } else { body };
    let head = if final_url != url { format!("URL: {final_url} (redirected)\n") } else { String::new() };
    Ok(head + &window(&text, start))
}

/// `PAGE_CHARS` of `text` from character `start`, saying how to read on.
fn window(text: &str, start: usize) -> String {
    let total = text.chars().count();
    if start >= total && total > 0 {
        return format!("[start {start} is past the end: the text is {total} characters]");
    }
    let part: String = text.chars().skip(start).take(PAGE_CHARS).collect();
    let end = start + part.chars().count();
    let mut out = String::new();
    if start > 0 {
        out += &format!("[characters {start}-{end} of {total}]\n");
    }
    out += &part;
    if end < total {
        out += &format!("\n\n[{} more characters; call FETCH again with start={end}]", total - end);
    }
    out
}

/// The readable text of an HTML page: title, headings marked with #, list
/// items with -, links as "text (url)", table cells split by |; scripts,
/// styles and the rest of the markup left out.
pub fn page_text(html: &str, base: &str) -> String {
    let mut r = Reader { base, out: String::new(), title: String::new(), skip: None, in_title: false, pre: 0, links: vec![] };
    let mut rest = html;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            r.text(rest);
            break;
        };
        r.text(&rest[..lt]);
        rest = &rest[lt..];
        if rest.starts_with("<!--") {
            rest = rest.find("-->").map_or("", |i| &rest[i + 3..]);
            continue;
        }
        let Some(gt) = tag_end(rest) else { break };
        r.tag(&rest[1..gt]);
        rest = &rest[gt + 1..];
    }
    let body = tidy(&r.out);
    let title = one_line(&r.title);
    if title.is_empty() { body } else { format!("Title: {title}\n\n{body}") }
}

/// What `page_text` has read of a page so far.
struct Reader<'a> {
    /// The page's address, for its links.
    base: &'a str,
    out: String,
    title: String,
    /// Inside <script> and the like: its name.
    skip: Option<String>,
    in_title: bool,
    /// How many <pre>s (or <textarea>s) it is inside.
    pre: u32,
    /// Open links: where to show, and where their text started.
    links: Vec<(Option<String>, usize)>,
}

impl Reader<'_> {
    /// Text between tags.
    fn text(&mut self, s: &str) {
        text(&mut self.out, s, self.pre > 0, self.skip.is_some(), self.in_title, &mut self.title);
    }

    /// A tag (between its < and >).
    fn tag(&mut self, tag: &str) {
        let closing = tag.starts_with('/');
        let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
        if let Some(s) = &self.skip {
            if closing && name == *s {
                self.skip = None;
            }
            return;
        }
        match name.as_str() {
            "script" | "style" | "noscript" | "svg" | "template" | "iframe" | "canvas" | "object" if !closing && !tag.ends_with('/') => {
                self.skip = Some(name)
            }
            "title" => self.in_title = !closing,
            "pre" | "textarea" => {
                self.pre = if closing { self.pre.saturating_sub(1) } else { self.pre + 1 };
                newline(&mut self.out, 1);
            }
            "br" => self.out.push('\n'),
            "hr" => {
                newline(&mut self.out, 1);
                self.out.push_str("---\n");
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                newline(&mut self.out, 2);
                if !closing {
                    self.out.push_str(&"#".repeat((name.as_bytes()[1] - b'0') as usize));
                    self.out.push(' ');
                }
            }
            "li" | "dt" if !closing => {
                newline(&mut self.out, 1);
                self.out.push_str("- ");
            }
            "td" | "th" if !closing => {
                if !self.out.ends_with('\n') && !self.out.is_empty() {
                    self.out.push_str(" | ");
                }
            }
            "a" if !closing => self.links.push((attr(tag, "href").and_then(|h| link(&h, self.base)), self.out.len())),
            "a" => {
                // Only links with text: an icon's link would just be noise.
                if let Some((Some(href), at)) = self.links.pop()
                    && !self.out[at.min(self.out.len())..].trim().is_empty()
                {
                    while self.out.ends_with(' ') {
                        self.out.pop();
                    }
                    self.out.push_str(&format!(" ({href})"));
                }
            }
            // Pieces pages set side by side with CSS: keep words apart.
            "span" | "button" | "time" | "label" | "input" | "select" | "option" => {
                if !self.out.is_empty() && !self.out.ends_with([' ', '\n']) {
                    self.out.push(' ');
                }
            }
            "img" => {
                if let Some(alt) = attr(tag, "alt").filter(|a| !a.trim().is_empty()) {
                    self.out.push_str(&format!("[image: {}]", alt.trim()));
                }
            }
            "p" | "div" | "section" | "article" | "header" | "footer" | "nav" | "main" | "aside" | "ul" | "ol" | "dl" | "dd" | "tr"
            | "table" | "blockquote" | "form" | "figure" | "figcaption" | "details" | "summary" | "body" => {
                newline(&mut self.out, if matches!(name.as_str(), "p" | "table" | "section" | "article") { 2 } else { 1 })
            }
            _ => {}
        }
    }
}

/// `text` with its lines trimmed and at most one blank line in a row.
fn tidy(text: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in text.lines() {
        let line = line.trim_end();
        blank = if line.trim().is_empty() { blank + 1 } else { 0 };
        if blank <= 1 {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

/// Where a tag ends, skipping a `>` inside a quoted attribute.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices().skip(1) {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

fn text(out: &mut String, s: &str, pre: bool, skip: bool, in_title: bool, title: &mut String) {
    if skip || s.is_empty() {
        return;
    }
    let s = decode(s);
    if in_title {
        title.push_str(&s);
        return;
    }
    if pre {
        out.push_str(&s);
        return;
    }
    let collapsed = one_line(&s);
    if !collapsed.is_empty() && s.starts_with(char::is_whitespace) && !out.ends_with([' ', '\n']) && !out.is_empty() {
        out.push(' ');
    }
    out.push_str(&collapsed);
    if s.ends_with(char::is_whitespace) {
        out.push(' ');
    }
}

/// Ends the current line, with `n - 1` blank lines after it.
fn newline(out: &mut String, n: usize) {
    while out.ends_with(' ') {
        out.pop();
    }
    if out.is_empty() {
        return;
    }
    let have = out.chars().rev().take_while(|&c| c == '\n').count();
    for _ in have..n {
        out.push('\n');
    }
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let before = lower[..at].chars().last();
        if !before.is_some_and(char::is_whitespace) {
            continue;
        }
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let value = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or(""),
            _ => rest.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or(""),
        };
        return Some(decode(value));
    }
    None
}

/// A link worth showing, made absolute against the page's URL.
fn link(href: &str, base: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") || href.starts_with("data:") {
        return None;
    }
    if href.contains("://") || href.starts_with("mailto:") {
        return Some(href.to_string());
    }
    let scheme_end = base.find("://")? + 3;
    let origin_end = base[scheme_end..].find('/').map_or(base.len(), |i| scheme_end + i);
    if let Some(rest) = href.strip_prefix("//") {
        return Some(format!("{}{rest}", &base[..scheme_end]));
    }
    if href.starts_with('/') {
        return Some(format!("{}{href}", &base[..origin_end]));
    }
    let path = base.split(['?', '#']).next().unwrap_or(base);
    let dir = if path.len() > origin_end { &path[..path.rfind('/').unwrap_or(path.len()) + 1] } else { "" };
    Some(if dir.is_empty() { format!("{}/{href}", &base[..origin_end]) } else { format!("{dir}{href}") })
}

/// Decodes HTML character references.
fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest[1..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '#')).map(|j| j + 1);
        let (name, after) = match end {
            Some(e) if rest[e..].starts_with(';') => (&rest[1..e], &rest[e + 1..]),
            _ => {
                out.push('&');
                rest = &rest[1..];
                continue;
            }
        };
        let ch = match name {
            "amp" | "lt" | "gt" | "quot" | "apos" | "nbsp" => Some(match name { "amp" => '&', "lt" => '<', "gt" => '>', "quot" => '"', "apos" => '\'', _ => ' ' }),
            "mdash" | "ndash" | "hellip" => Some(match name { "mdash" => '—', "ndash" => '–', _ => '…' }),
            "lsquo" | "rsquo" | "ldquo" | "rdquo" => Some(match name { "lsquo" => '‘', "rsquo" => '’', "ldquo" => '“', _ => '”' }),
            "middot" | "copy" | "reg" | "trade" | "times" | "bull" => Some(match name { "middot" => '·', "copy" => '©', "reg" => '®', "trade" => '™', "times" => '×', _ => '•' }),
            "rarr" | "larr" => Some(if name == "rarr" { '→' } else { '←' }),
            n if n.starts_with("#x") || n.starts_with("#X") => u32::from_str_radix(&n[2..], 16).ok().and_then(char::from_u32),
            n if n.starts_with('#') => n[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = after;
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ------------------------------------------------------------ headless Chrome

/// Chrome, or another Chromium browser (they take the same switches).
#[cfg(windows)]
pub(crate) fn browser() -> Option<PathBuf> {
    let mut places = vec![];
    for subs in [[r"Google\Chrome\Application\chrome.exe", r"Chromium\Application\chrome.exe"], [r"BraveSoftware\Brave-Browser\Application\brave.exe", r"Microsoft\Edge\Application\msedge.exe"]] {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(root) = std::env::var_os(var).map(PathBuf::from) {
                places.extend(subs.iter().map(|sub| root.join(sub)));
            }
        }
    }
    places.into_iter().find(|p| p.is_file()).or_else(playwright)
}

/// Chrome, or another Chromium browser, on PATH (on Arch: google-chrome
/// from the AUR, chromium, brave-bin, microsoft-edge-stable-bin).
#[cfg(not(windows))]
pub(crate) fn browser() -> Option<PathBuf> {
    const NAMES: [&str; 9] = ["google-chrome-stable", "google-chrome", "chromium", "chromium-browser", "brave",
        "brave-browser", "microsoft-edge-stable", "microsoft-edge",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"];
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    NAMES.iter().find_map(|n| {
        let n = Path::new(n);
        if n.is_absolute() { Some(n.to_path_buf()).filter(|p| p.is_file()) } else { dirs.iter().map(|d| d.join(n)).find(|p| p.is_file()) }
    })
    .or_else(playwright)
}

/// The Chromium Playwright downloads (`npx playwright install chromium`),
/// the newest, when no browser is installed: in ms-playwright under the
/// cache folder (LOCALAPPDATA on Windows).
fn playwright() -> Option<PathBuf> {
    let root = if cfg!(windows) { dirs::data_local_dir()? } else { dirs::cache_dir()? }.join("ms-playwright");
    let mut builds: Vec<PathBuf> = std::fs::read_dir(&root).ok()?.flatten().map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("chromium-")))
        .collect();
    builds.sort_by_key(|p| std::cmp::Reverse(p.file_name().and_then(|n| n.to_str()).and_then(|n| n[9..].parse::<u32>().ok()).unwrap_or(0)));
    let exes = [r"chrome-win64\chrome.exe", r"chrome-win\chrome.exe", "chrome-linux64/chrome", "chrome-linux/chrome", "chrome-mac/Chromium.app/Contents/MacOS/Chromium"];
    builds.iter().flat_map(|b| exes.iter().map(move |e| b.join(e))).find(|p| p.is_file())
}

/// Runs headless Chrome on `url` with a throwaway profile and returns what
/// it printed. `shot` is the screenshot file it should write, if any.
fn chrome(extra: &[String], url: &str, shot: Option<&Path>) -> Result<Vec<u8>, String> {
    let exe = browser().ok_or("No Chrome found (looked for Chrome, Chromium, Brave and Edge).")?;
    let profile = std::env::temp_dir().join(format!("oak-chrome-{}-{}", std::process::id(), crate::api::now_ms()));
    let mut cmd = Command::new(&exe);
    cmd.args([
        "--headless", "--disable-gpu", "--no-first-run", "--no-default-browser-check",
        "--disable-extensions", "--disable-sync", "--disable-background-networking", "--disable-component-update",
        "--disable-features=Translate,OptimizationHints,MediaRouter", "--renderer-process-limit=1",
        "--mute-audio", "--hide-scrollbars",
    ])
    .arg(format!("--user-data-dir={}", profile.display()))
    .args(extra)
    .arg(url)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| format!("Couldn't start {}: {e}", exe.display()))?;
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = vec![];
        let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
        buf
    });
    let t = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? { break Some(s); }
        if t.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = reader.join().unwrap_or_default();
    // Chrome may still hold the profile for a moment after exiting.
    for _ in 0..20 {
        if std::fs::remove_dir_all(&profile).is_ok() || !profile.exists() { break; }
        std::thread::sleep(Duration::from_millis(100));
    }
    let name = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    match status {
        None => Err(format!("{name} took more than 60 s on {url}; gave up.")),
        Some(_) if shot.is_some_and(|p| !p.is_file()) => Err(format!("{name} didn't write a screenshot of {url}.")),
        Some(_) if shot.is_none() && out.is_empty() => Err(format!("{name} returned nothing for {url}.")),
        Some(_) => Ok(out),
    }
}

/// A screenshot of a web page, as content blocks for the model (and saved
/// to `save`, if given).
pub fn screenshot(input: &Value, save: Option<PathBuf>) -> Result<Vec<Value>, String> {
    let url = input["url"].as_str().unwrap_or("").trim();
    let file_url = url.starts_with("file:///");
    if !url.starts_with("http://") && !url.starts_with("https://") && !file_url {
        return Err("The url must start with http://, https:// or file:///".into());
    }
    let w = input["width"].as_u64().unwrap_or(1280).clamp(320, 3840);
    let h = input["height"].as_u64().unwrap_or(800).clamp(200, 8000);
    let wait = input["wait_ms"].as_u64().unwrap_or(2000).min(20_000);
    let file = std::env::temp_dir().join(format!("oak-shot-{}-{}.png", std::process::id(), crate::api::now_ms()));
    let args = [format!("--window-size={w},{h}"), format!("--virtual-time-budget={wait}"), format!("--screenshot={}", file.display())];
    chrome(&args, url, Some(&file))?;
    let result = crate::images::read_for_model(&file);
    if let Some(dest) = save {
        if let Some(dir) = dest.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::copy(&file, &dest).map_err(|e| format!("Couldn't save {}: {e}", dest.display()))?;
    }
    let _ = std::fs::remove_file(&file);
    let mut blocks = result?;
    let saved = input["path"].as_str().map(|p| format!(", saved to {p}")).unwrap_or_default();
    blocks[0] = serde_json::json!({"type": "text", "text": format!("Screenshot of {url} at {w}x{h}{saved}")});
    Ok(blocks)
}

/// Percent-encodes `s` for a URL's path or query: RFC 3986's unreserved
/// characters stay as they are.
pub(crate) fn percent(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text() {
        let html = r##"<!DOCTYPE html><html><head><title>Oak &amp; Co</title><style>p{color:red}</style>
            <script>var x = "<p>not text</p>";</script></head><body>
            <nav><a href="/">Home</a> <a href="docs/intro.html">Docs</a> <a href="#top">Top</a></nav>
            <h1>Hello,   world</h1><p>Some <b>bold</b> text&nbsp;and a <a href='https://x.org/a?b=1'>link</a>.</p>
            <!-- a comment <p>hidden</p> -->
            <ul><li>one</li><li>two &#8212; &#x41;</li></ul>
            <table><tr><th>k</th><th>v</th></tr><tr><td>a</td><td>1</td></tr></table>
            <pre>  keep
    this</pre><img src=x.png alt="A tree"><svg><text>drawn</text></svg><noscript>Enable JS</noscript>
            <p>5 &lt; 6 &unknown; AT&T</p></body></html>"##;
        let t = page_text(html, "https://oak.dev/guide/start.html?x=1");
        println!("{t}");
        assert!(t.starts_with("Title: Oak & Co\n\n"));
        assert!(t.contains("Home (https://oak.dev/) Docs (https://oak.dev/guide/docs/intro.html) Top\n"));
        assert!(t.contains("\n# Hello, world\n"));
        assert!(t.contains("Some bold text and a link (https://x.org/a?b=1)."));
        assert!(t.contains("- one\n- two — A"));
        assert!(t.contains("k | v\na | 1"));
        assert!(t.contains("  keep\n    this"));
        assert!(t.contains("[image: A tree]"));
        assert!(t.contains("5 < 6 &unknown; AT&T"));
        for gone in ["color:red", "not text", "hidden", "drawn", "Enable JS", "<"] {
            assert!(!t.replace("5 < 6", "").contains(gone), "{gone:?} left in");
        }
    }
}
