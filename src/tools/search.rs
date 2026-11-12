//! SEARCH: the web the way a search engine shows it. A ranked list of
//! pages, each with its title, link and a short summary, and no page text:
//! the model FETCHes the pages worth reading, as a person clicks results.
//!
//! The providers are opencode's keyless ones (Firecrawl, Parallel, Exa,
//! Tavily), each brought to the same {url, title, summary, published}
//! shape. One answers; when it fails or finds nothing, the next is tried.
//! OAK_SEARCH_PROVIDER puts one first; FIRECRAWL_API_KEY, PARALLEL_API_KEY,
//! EXA_API_KEY and TAVILY_API_KEY are used when set.

use crate::tools::Head;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::Duration;
use super::one_line;

/// Results returned when the call doesn't say (opencode asks for 8).
pub const DEFAULT_RESULTS: usize = 10;
pub const MAX_RESULTS: usize = 20;
/// A result's summary, at most: a search engine's snippet.
const SUMMARY_CHARS: usize = 240;
/// One provider's request, at most.
const TIMEOUT: Duration = Duration::from_secs(20);
/// The line closing a result list, which also tells the model what next.
pub const FOOTER: &str = "Summaries only, not the pages: FETCH a URL to read it.";

/// One search result, whichever provider found it.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub url: String,
    pub title: String,
    pub summary: String,
    /// YYYY-MM-DD when the provider knows it.
    pub published: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Provider {
    Firecrawl,
    Parallel,
    Exa,
    Tavily,
}

const PROVIDERS: [Provider; 4] = [Provider::Firecrawl, Provider::Parallel, Provider::Exa, Provider::Tavily];

impl Provider {
    fn name(self) -> &'static str {
        match self {
            Provider::Firecrawl => "Firecrawl",
            Provider::Parallel => "Parallel",
            Provider::Exa => "Exa",
            Provider::Tavily => "Tavily",
        }
    }

    fn key(self) -> Option<String> {
        let var = match self {
            Provider::Firecrawl => "FIRECRAWL_API_KEY",
            Provider::Parallel => "PARALLEL_API_KEY",
            Provider::Exa => "EXA_API_KEY",
            Provider::Tavily => "TAVILY_API_KEY",
        };
        std::env::var(var).ok().filter(|k| !k.trim().is_empty())
    }

    /// Up to `limit` results for each query, best first.
    fn search(self, objective: &str, queries: &[String], limit: usize) -> Result<Vec<Vec<Hit>>, String> {
        match self {
            // Parallel takes every query in one request and ranks against
            // the objective itself.
            Provider::Parallel => parallel(objective, queries).map(|h| vec![h]),
            _ => {
                let runs: Vec<Result<Vec<Hit>, String>> = std::thread::scope(|s| {
                    let jobs: Vec<_> = queries
                        .iter()
                        .map(|q| {
                            s.spawn(move || match self {
                                Provider::Firecrawl => firecrawl(q, limit),
                                Provider::Exa => exa(q, objective, limit),
                                _ => tavily(q, limit),
                            })
                        })
                        .collect();
                    jobs.into_iter().map(|j| j.join().unwrap_or_else(|_| Err("search thread panicked".into()))).collect()
                });
                // Some queries answering is enough; the first error otherwise.
                if runs.iter().all(Result::is_err) {
                    return Err(runs.into_iter().find_map(Result::err).unwrap_or_default());
                }
                Ok(runs.into_iter().filter_map(Result::ok).collect())
            }
        }
    }
}

/// The providers in the order they are tried: OAK_SEARCH_PROVIDER's
/// first, then those with a key, then the rest.
fn order() -> Vec<Provider> {
    let chosen = std::env::var("OAK_SEARCH_PROVIDER").unwrap_or_default().to_lowercase();
    let mut list = PROVIDERS.to_vec();
    list.sort_by_key(|p| (p.name().to_lowercase() != chosen.trim(), p.key().is_none()));
    list
}

pub fn search(input: &Value) -> Result<String, String> {
    let objective = input["objective"].as_str().unwrap_or("").trim().to_string();
    let mut queries: Vec<String> = input["queries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|q| q.as_str())
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty())
        .take(5)
        .collect();
    if queries.is_empty() {
        if objective.is_empty() {
            return Err("Give at least one query.".into());
        }
        queries.push(objective.clone());
    }
    let objective = if objective.is_empty() { queries.join("; ") } else { objective };
    let limit = input["max_results"].as_u64().map_or(DEFAULT_RESULTS, |n| (n as usize).clamp(1, MAX_RESULTS));

    let mut failures = vec![];
    for p in order() {
        match p.search(&objective, &queries, limit) {
            Ok(runs) => {
                let hits = merge(runs, limit);
                if !hits.is_empty() {
                    return Ok(format_results(&objective, p.name(), &hits));
                }
                failures.push(format!("{}: no results", p.name()));
            }
            Err(e) => failures.push(format!("{}: {e}", p.name())),
        }
    }
    if failures.iter().all(|f| f.ends_with(": no results")) {
        return Ok("No search results found. Try different queries.".into());
    }
    Err(format!("Search failed ({})", failures.join("; ")))
}

/// The list the model reads (and the transcript shows as links):
///
/// ```text
/// 1. Title
///    https://example.com/page
///    2024-05-01, A sentence or two about the page.
/// ```
pub fn format_results(objective: &str, provider: &str, hits: &[Hit]) -> String {
    let mut out = format!("{} result{} for \"{}\" (via {provider}):\n", hits.len(), crate::plural(hits.len()), one_line(objective));
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n   {}\n", i + 1, one_line(&h.title), h.url));
        let line = match (&h.published, h.summary.is_empty()) {
            (Some(d), false) => format!("{d}, {}", h.summary),
            (Some(d), true) => d.clone(),
            (None, _) => h.summary.clone(),
        };
        if !line.is_empty() {
            out.push_str(&format!("   {line}\n"));
        }
    }
    out.push('\n');
    out.push_str(FOOTER);
    out
}

/// A result list read back (for the transcript): (title, url, summary).
pub fn parse_results(text: &str) -> Option<Vec<(String, String, String)>> {
    if !text.trim_end().ends_with(FOOTER) {
        return None;
    }
    let mut hits = vec![];
    for block in text.split("\n\n").skip(1) {
        let mut lines = block.lines();
        let Some((n, title)) = lines.next().and_then(|l| l.split_once(". ")) else { continue };
        if n.parse::<usize>().is_err() {
            continue;
        }
        let Some(url) = lines.next().map(str::trim).filter(|u| u.starts_with("http")) else { continue };
        let summary = lines.map(str::trim).collect::<Vec<_>>().join(" ");
        hits.push((title.to_string(), url.to_string(), summary));
    }
    Some(hits)
}

/// Each query's results taken in turns (every query's first, then every
/// second, …), each page once, at most `limit`.
fn merge(runs: Vec<Vec<Hit>>, limit: usize) -> Vec<Hit> {
    let mut seen = HashSet::new();
    let mut out = vec![];
    let longest = runs.iter().map(Vec::len).max().unwrap_or(0);
    for rank in 0..longest {
        for run in &runs {
            let Some(h) = run.get(rank) else { continue };
            if out.len() < limit && seen.insert(page_key(&h.url)) {
                out.push(h.clone());
            }
        }
    }
    out
}

/// A URL as one page: no scheme, `www.`, fragment, trailing slash or
/// tracking parameters, so the same page found twice counts once.
fn page_key(url: &str) -> String {
    let url = url.split('#').next().unwrap_or(url);
    let url = url.split_once("://").map_or(url, |(_, rest)| rest);
    let url = url.strip_prefix("www.").unwrap_or(url);
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let noise = |k: &str| k.starts_with("utm_") || ["fbclid", "gclid", "ref", "ref_src", "search", "srsltid"].contains(&k);
    let kept: Vec<&str> = query.split('&').filter(|p| !p.is_empty() && !noise(p.split('=').next().unwrap_or(p))).collect();
    let path = path.trim_end_matches('/').to_lowercase();
    if kept.is_empty() { path } else { format!("{path}?{}", kept.join("&")) }
}

// ------------------------------------------------------------- summaries

/// A short summary of a result from whatever text its provider gave
/// (a search snippet, or excerpts of the page): its prose, without
/// markdown, navigation or code, cut at a word near SUMMARY_CHARS.
pub fn summarize(text: &str, title: &str) -> String {
    let mut lines = vec![];
    let mut fenced = false;
    for raw in text.lines() {
        let l = raw.trim();
        if l.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let l = plain(l);
        if l.is_empty() || l == "..." || l == "…" || l.eq_ignore_ascii_case(title.trim()) {
            continue;
        }
        lines.push(l);
    }
    // Prose first: sentences, or long runs of words. Menus, headings and
    // code fragments only when there is nothing else.
    let prose: Vec<&String> = lines.iter().filter(|l| is_prose(l)).collect();
    let picked: Vec<&String> = if prose.is_empty() { lines.iter().collect() } else { prose };
    let mut out = String::new();
    for l in picked {
        if out.len() >= SUMMARY_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(l);
    }
    cut(&one_line(&out), SUMMARY_CHARS)
}

fn is_prose(l: &str) -> bool {
    let words = l.split_whitespace().count();
    let letters = l.chars().filter(|c| c.is_alphabetic() || c.is_whitespace()).count();
    let wordy = letters * 10 >= l.chars().count() * 8;
    wordy && (words >= 12 || (words >= 5 && l.ends_with(['.', '!', '?', ':'])))
}

/// A line of markdown as plain text.
fn plain(l: &str) -> String {
    let l = l.trim_start_matches(['#', '>', '-', '*', '+', '|', ' ']).trim_end_matches(['|', ' ']);
    // Code comments' markers (`///`, `//!`), as in source files' excerpts.
    let l = l.strip_prefix("//").map_or(l, |c| c.trim_start_matches(['/', '!', ' ']));
    let mut out = String::with_capacity(l.len());
    let mut rest = l;
    while let Some(i) = rest.find(['[', '!', '\\', '`', '*', '_']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        // ![alt](src): dropped. [text](url): the text. [...]: dropped.
        if let Some(t) = tail.strip_prefix("![").or_else(|| tail.strip_prefix('['))
            && let Some(close) = t.find(']')
        {
            let label = &t[..close];
            let after = &t[close + 1..];
            let after = match after.strip_prefix('(').and_then(|a| a.find(')').map(|j| &a[j + 1..])) {
                Some(a) => a,
                None => after,
            };
            if !tail.starts_with('!') && label != "..." && label != "…" {
                out.push_str(&plain(label));
            }
            rest = after;
            continue;
        }
        match tail.chars().next().unwrap() {
            // \_ and the like: the character itself.
            '\\' => {
                let mut cs = tail[1..].chars();
                if let Some(c) = cs.next() {
                    out.push(c);
                }
                rest = cs.as_str();
            }
            // Emphasis and code marks; `_` only around words, not in names.
            '_' if !out.ends_with(|c: char| c.is_whitespace()) && !out.is_empty() => {
                out.push('_');
                rest = &tail[1..];
            }
            '`' | '*' | '_' => rest = &tail[1..],
            c => {
                out.push(c);
                rest = &tail[c.len_utf8()..];
            }
        }
    }
    out.push_str(rest);
    one_line(&out)
}

/// `s` at most `max` bytes, cut after a word, with "…" when cut.
fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head = &s[..s.floor_char_boundary(max)];
    let head = head.rfind(' ').filter(|&i| i > max / 2).map_or(head, |i| &head[..i]);
    format!("{}…", head.trim_end_matches([',', ';', ':', ' ', '-', '–', '(']))
}

/// "2024-05-01" from the dates providers give.
fn day(s: &str) -> Option<String> {
    let s = s.trim();
    let b = s.as_bytes();
    let iso = b.len() >= 10 && b[4] == b'-' && b[7] == b'-' && s[..4].chars().chain(s[5..7].chars()).chain(s[8..10].chars()).all(|c| c.is_ascii_digit());
    iso.then(|| s[..10].to_string())
}

fn hit(url: &str, title: Option<&str>, text: &str, published: Option<&str>) -> Option<Hit> {
    let url = url.trim();
    if !url.starts_with("http") {
        return None;
    }
    let title = title.map(one_line).filter(|t| !t.is_empty() && t != "N/A").unwrap_or_else(|| url.to_string());
    Some(Hit { url: url.to_string(), summary: summarize(text, &title), title, published: published.and_then(day) })
}

// ------------------------------------------------------------- providers

/// One client for every search: a clone shares its connection pool, so a
/// search soon after another to the same provider skips the new
/// connection and TLS handshake (about 300 ms of each).
fn client() -> reqwest::blocking::Client {
    static CLIENT: std::sync::LazyLock<reqwest::blocking::Client> =
        std::sync::LazyLock::new(|| reqwest::blocking::Client::builder().timeout(TIMEOUT).user_agent("oak/0.1").build().unwrap_or_default());
    CLIENT.clone()
}

fn http_error(status: reqwest::StatusCode) -> String {
    match status.as_u16() {
        429 => "rate limited (HTTP 429)".into(),
        401 | 403 => format!("not authorized (HTTP {})", status.as_u16()),
        _ => format!("HTTP {status}"),
    }
}

/// One stateless MCP tools/call, as opencode makes it: the call's result.
fn mcp(url: &str, tool: &str, args: Value, key: Option<String>) -> Result<Value, String> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}});
    let mut req = client().post(url).header("Accept", "application/json, text/event-stream").json(&body);
    if let Some(key) = key {
        req = req.bearer_auth(key);
    }
    let resp = req.send().map_err(|e| if e.is_timeout() { "timed out".into() } else { e.to_string() })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(http_error(status));
    }
    let text = resp.text().map_err(|e| e.to_string())?;
    let msg = mcp_message(&text).ok_or("unreadable answer")?;
    if let Some(e) = msg.get("error") {
        return Err(e["message"].as_str().unwrap_or("unknown error").to_string());
    }
    let result = msg["result"].clone();
    if result["isError"].as_bool() == Some(true) {
        return Err(one_line(&mcp_text(&result)).head(300));
    }
    Ok(result)
}

/// The JSON-RPC answer in a body that is plain JSON or an SSE stream.
fn mcp_message(text: &str) -> Option<Value> {
    std::iter::once(text.trim())
        .chain(text.lines().filter_map(|l| l.strip_prefix("data:")))
        .filter_map(|p| serde_json::from_str::<Value>(p.trim()).ok())
        .find(|v| v.get("result").is_some() || v.get("error").is_some())
}

fn mcp_text(result: &Value) -> String {
    result["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n")
}

/// Firecrawl: a search engine's own results and snippets.
fn firecrawl(query: &str, limit: usize) -> Result<Vec<Hit>, String> {
    let result = mcp("https://mcp.firecrawl.dev/v2/mcp", "firecrawl_search", json!({"query": query, "limit": limit}), Provider::Firecrawl.key())?;
    parse_firecrawl(&mcp_text(&result))
}

fn parse_firecrawl(text: &str) -> Result<Vec<Hit>, String> {
    let v: Value = serde_json::from_str(text.trim()).map_err(|_| format!("unreadable answer: {}", text.head(200)))?;
    if v["success"].as_bool() == Some(false) {
        return Err(v["error"].as_str().unwrap_or("search failed").to_string());
    }
    let web = v["data"]["web"].as_array().or_else(|| v["data"].as_array()).or_else(|| v["web"].as_array());
    Ok(web
        .into_iter()
        .flatten()
        .filter_map(|r| hit(r["url"].as_str()?, r["title"].as_str(), r["description"].as_str().unwrap_or(""), r["date"].as_str()))
        .collect())
}

/// Parallel: page excerpts, which become summaries.
fn parallel(objective: &str, queries: &[String]) -> Result<Vec<Hit>, String> {
    let args = json!({"objective": objective, "search_queries": queries});
    let result = mcp("https://search.parallel.ai/mcp", "web_search", args, Provider::Parallel.key())?;
    Ok(parse_parallel(&result))
}

fn parse_parallel(result: &Value) -> Vec<Hit> {
    result["structuredContent"]["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let excerpts: Vec<&str> = r["excerpts"].as_array().into_iter().flatten().filter_map(|e| e.as_str()).collect();
            hit(r["url"].as_str()?, r["title"].as_str(), &excerpts.join("\n"), r["publish_date"].as_str())
        })
        .collect()
}

/// Exa: highlights of each page, which become summaries.
fn exa(query: &str, objective: &str, limit: usize) -> Result<Vec<Hit>, String> {
    let mut url = "https://mcp.exa.ai/mcp".to_string();
    if let Some(key) = Provider::Exa.key() {
        url.push_str(&format!("?exaApiKey={}", key.trim()));
    }
    let args = json!({"query": query, "objective": objective, "numResults": limit});
    Ok(parse_exa(&mcp_text(&mcp(&url, "web_search_exa", args, None)?)))
}

/// Exa's text: blocks of `Title:`, `URL:`, `Published:` and the page's
/// `Highlights:` (or `Text:`), split by `---` lines.
fn parse_exa(text: &str) -> Vec<Hit> {
    text.split("\n---\n")
        .filter_map(|block| {
            let field = |name: &str| block.lines().find_map(|l| l.strip_prefix(name)).map(str::trim);
            let body = ["\nHighlights:", "\nText:", "\nSummary:"].iter().find_map(|m| block.split_once(m)).map_or("", |(_, b)| b);
            hit(field("URL:")?, field("Title:"), body, field("Published:"))
        })
        .collect()
}

/// Tavily: page chunks, which become summaries.
fn tavily(query: &str, limit: usize) -> Result<Vec<Hit>, String> {
    let mut req = client()
        .post("https://api.tavily.com/search")
        .header("X-Client-Name", "oak")
        .json(&json!({"query": query, "search_depth": "basic", "max_results": limit}));
    req = match Provider::Tavily.key() {
        Some(key) => req.bearer_auth(key),
        None => req.header("X-Tavily-Access-Mode", "keyless"),
    };
    let resp = req.send().map_err(|e| if e.is_timeout() { "timed out".into() } else { e.to_string() })?;
    if !resp.status().is_success() {
        return Err(http_error(resp.status()));
    }
    let v: Value = resp.json().map_err(|e| e.to_string())?;
    Ok(v["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| hit(r["url"].as_str()?, r["title"].as_str(), &r["content"].as_str().unwrap_or("").replace("[...]", "\n"), r["published_date"].as_str()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(url: &str) -> Hit {
        Hit { url: url.into(), title: url.into(), summary: String::new(), published: None }
    }

    #[test]
    fn summaries_are_short_prose() {
        // Parallel's excerpts: navigation, headings and code around prose.
        let ex = "Skip to main content ## ClientBuilder\nreqwest :: blocking\n\nStruct Client Builder Copy item path\n\nSource\npub struct ClientBuilder {  /* private fields */  }\nAvailable on non-WebAssembly and crate feature blocking only. Expand description\nA [`ClientBuilder`](struct.ClientBuilder.html) can be used to create a `Client` with custom configuration.\n\n```\nlet c = 1;\n```\n";
        let s = summarize(ex, "ClientBuilder in reqwest::blocking");
        assert_eq!(s, "A ClientBuilder can be used to create a Client with custom configuration.");
        // Long text is cut at a word.
        let long = "This sentence keeps going and going with plenty of words in it. ".repeat(20);
        let s = summarize(&long, "");
        assert!(s.len() <= SUMMARY_CHARS + 3 && s.ends_with('…') && !s.contains("  "), "{s}");
        // Escapes, emphasis and images; names with underscores stay.
        assert_eq!(plain("**Bold** use connect\\_timeout and snake_case ![img](x.png) [...] [docs](https://x)"), "Bold use connect_timeout and snake_case docs");
        // Nothing but a menu: the menu, rather than nothing.
        assert_eq!(summarize("Home\nDocs\nBlog", "t"), "Home Docs Blog");
    }

    #[test]
    fn merge_takes_turns_and_drops_repeats() {
        let a = vec![h("https://a.com/1"), h("https://www.a.com/1/#top"), h("https://a.com/2")];
        let b = vec![h("https://b.com/1?utm_source=x"), h("https://b.com/1"), h("https://youtube.com/watch?v=1"), h("https://youtube.com/watch?v=2")];
        let got: Vec<String> = merge(vec![a, b], 4).into_iter().map(|h| h.url).collect();
        assert_eq!(got, ["https://a.com/1", "https://b.com/1?utm_source=x", "https://a.com/2", "https://youtube.com/watch?v=1"]);
    }

    #[test]
    fn providers_parse() {
        let fc = r#"{"success":true,"data":{"web":[{"url":"https://docs.rs/x","title":"X - Rust","description":"Sets a timeout for the whole request.","position":1}]}}"#;
        let got = parse_firecrawl(fc).unwrap();
        assert_eq!(got, vec![Hit { url: "https://docs.rs/x".into(), title: "X - Rust".into(), summary: "Sets a timeout for the whole request.".into(), published: None }]);

        let exa = "Title: Page One\nURL: https://one.example/a\nPublished: 2024-03-05T10:00:00.000Z\nAuthor: N/A\nHighlights:\nThe first page explains how the timeout applies to every request made.\n\n---\n\nTitle: N/A\nURL: https://two.example/\nPublished: N/A\nHighlights:\nShort.";
        let got = parse_exa(exa);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].published.as_deref(), Some("2024-03-05"));
        assert_eq!(got[0].summary, "The first page explains how the timeout applies to every request made.");
        assert_eq!((got[1].title.as_str(), got[1].published.as_deref()), ("https://two.example/", None));

        let par = json!({"structuredContent": {"results": [{"url": "https://p.example", "title": "P", "publish_date": null, "excerpts": ["## P\nA page about the thing, with enough words to read as a sentence."]}]}});
        assert_eq!(parse_parallel(&par)[0].summary, "A page about the thing, with enough words to read as a sentence.");
    }

    /// Each provider, for real: `cargo test live_search -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_search() {
        let queries = vec!["reqwest blocking client timeout".to_string(), "reqwest ClientBuilder timeout".to_string()];
        for p in PROVIDERS {
            match p.search("How to set a timeout on a reqwest blocking client", &queries, DEFAULT_RESULTS) {
                Ok(runs) => {
                    let text = format_results("reqwest blocking timeout", p.name(), &merge(runs, DEFAULT_RESULTS));
                    println!("{text}\n[{} chars]\n", text.len());
                    assert!(text.len() < 6_000);
                }
                Err(e) => println!("{}: {e}\n", p.name()),
            }
        }
    }

    #[test]
    fn results_round_trip() {
        let hits = vec![
            Hit { url: "https://a.example/x".into(), title: "A\ntitle".into(), summary: "About A.".into(), published: Some("2024-01-02".into()) },
            Hit { url: "https://b.example".into(), title: "B".into(), summary: String::new(), published: None },
        ];
        let text = format_results("find things", "Firecrawl", &hits);
        assert!(text.starts_with("2 results for \"find things\" (via Firecrawl):\n\n1. A title\n   https://a.example/x\n   2024-01-02, About A.\n\n2. B\n"), "{text}");
        assert_eq!(
            parse_results(&text).unwrap(),
            [("A title".to_string(), "https://a.example/x".to_string(), "2024-01-02, About A.".to_string()), ("B".into(), "https://b.example".into(), String::new())]
        );
        assert!(parse_results("No search results found.").is_none());
    }
}
