//! SEARCH's images: pictures from the web for the model, e.g. screenshots
//! of an app's UI to work from.
//!
//! SEARCH with `images` returns one picture, a contact sheet laid out
//! like Google Images in the Oak theme's colors: the results' thumbnails
//! in columns, and under each its number (large), its title and the start
//! of its link; beside the sheet, one line of text for each (title, size,
//! site). That is a dozen images for the tokens of one, without their
//! long links. The numbers are ids: SEARCH with `image` (an id, or any
//! image's URL) gives one image in full, with its link and its page's,
//! and saves the file in the scratchpad so it can be shown to the user or
//! opened again.
//!
//! The ids count up through the conversation (two searches in one reply
//! never share one) and are kept with their links in `index.json` in the
//! scratchpad's `images` folder, so they outlast a restart.
//!
//! The sources answer plain HTTP requests, no browser: Google Images
//! (through Serper, only with SERPER_API_KEY), then Brave's image search,
//! whose results are read from its page (fetched with curl, see `brave`).

use super::search::{Provider, client, http_error, serper_call};
use super::{Head, one_line};
use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use image::{DynamicImage, Rgba, RgbaImage, imageops::FilterType};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// Images on a sheet when the call doesn't say, and at most (some six
/// rows: a taller sheet would be scaled down, its thumbnails too small
/// to tell).
pub const DEFAULT_IMAGES: usize = 12;
pub const MAX_IMAGES: usize = 24;
/// The last page that can be asked for.
const MAX_PAGE: usize = 5;
/// Results asked for beyond a sheet's, for the thumbnails that don't load.
const SPARE: usize = 6;
/// The ids the index keeps: the oldest go first.
const KEPT: usize = 600;

/// A thumbnail, and an image seen in full, at most (bytes) and how long
/// each may take.
const THUMB_BYTES: usize = 3_000_000;
const THUMB_TIME: Duration = Duration::from_secs(8);
const FULL_BYTES: usize = 25_000_000;
const FULL_TIME: Duration = Duration::from_secs(25);

const AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36";

/// One image a search found.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Found {
    pub title: String,
    /// The image itself, where it is published.
    pub image: String,
    /// A small copy, from the search engine.
    pub thumb: String,
    /// The page it is on.
    pub page: String,
    /// That page's site.
    pub source: String,
    pub width: u32,
    pub height: u32,
}

impl Found {
    fn json(&self, id: u64, query: &str) -> Value {
        json!({"id": id, "query": query, "title": self.title, "image": self.image, "thumb": self.thumb, "page": self.page,
               "source": self.source, "width": self.width, "height": self.height})
    }

    fn from_json(v: &Value) -> Found {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let n = |k: &str| v[k].as_u64().unwrap_or(0) as u32;
        Found { title: s("title"), image: s("image"), thumb: s("thumb"), page: s("page"), source: s("source"), width: n("width"), height: n("height") }
    }
}

/// Whether a SEARCH call is for images: a search for them (`images`),
/// or one to see in full (`image`).
pub fn wanted(input: &Value) -> bool {
    input["images"].as_bool() == Some(true) || chosen(input).is_some()
}

/// The `image` a call names: an id or a link, as text.
fn chosen(input: &Value) -> Option<String> {
    let image = match &input["image"] {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    (!image.is_empty()).then_some(image)
}

/// A SEARCH call for images (`wanted`): the result's blocks (text, then
/// the picture). `dir` is the conversation's folder for images.
pub fn images(input: &Value, dir: &Path) -> Result<Vec<Value>, String> {
    match chosen(input) {
        Some(image) => view(&image, dir),
        None => search(input, dir),
    }
}

/// How many images a search's result says it found, when `text` is one's.
pub fn count(text: &str) -> Option<usize> {
    let (n, rest) = text.split_once(' ')?;
    (rest.starts_with("image for \"") || rest.starts_with("images for \"")).then(|| n.parse().ok()).flatten()
}

// ---------------------------------------------------------------- search

fn search(input: &Value, dir: &Path) -> Result<Vec<Value>, String> {
    let objective = one_line(input["objective"].as_str().unwrap_or(""));
    let mut queries: Vec<String> = input["queries"].as_array().into_iter().flatten().filter_map(|q| q.as_str()).map(one_line).filter(|q| !q.is_empty()).take(5).collect();
    if queries.is_empty() {
        if objective.is_empty() {
            return Err("Give at least one query.".into());
        }
        queries.push(objective.clone());
    }
    // What the result says the images are for.
    let query = if objective.is_empty() { queries.join(", ") } else { objective };
    let limit = input["max_results"].as_u64().map_or(DEFAULT_IMAGES, |n| (n as usize).clamp(1, MAX_IMAGES));
    let page = input["page"].as_u64().map_or(1, |n| (n as usize).clamp(1, MAX_PAGE));
    let skip = (page - 1) * limit;

    let (found, via) = find_all(&queries, skip + limit + SPARE)?;
    let rest: Vec<Found> = found.into_iter().skip(skip).take(limit + SPARE).collect();
    if rest.is_empty() {
        return Ok(vec![json!({"type": "text", "text": if page > 1 { format!("No more images: page {page} is past the last. Try another query.") } else { "No images found. Try another query.".to_string() }})]);
    }
    // More may follow when the source had some left over.
    let more = (rest.len() > limit && page < MAX_PAGE).then_some(page + 1);
    let shown: Vec<(Found, DynamicImage)> = thumbnails(rest).into_iter().take(limit).collect();
    if shown.is_empty() {
        return Err(format!("{via} found images, but none of their thumbnails could be loaded. Try again."));
    }
    let first = remember(dir, &query, shown.iter().map(|(f, _)| f))?;
    let tiles: Vec<Tile> = shown
        .iter()
        .enumerate()
        .map(|(i, (f, picture))| Tile { id: first + i as u64, title: &f.title, link: short(if f.page.is_empty() { &f.image } else { &f.page }), picture })
        .collect();
    let sheet = crate::images::encode_jpeg(&DynamicImage::ImageRgba8(sheet(&tiles)?))?;

    let mut text = format!("{} image{} for \"{query}\" (via {via}), each with its number under it on the sheet:\n\n", shown.len(), crate::plural(shown.len()));
    for (i, (f, _)) in shown.iter().enumerate() {
        text.push_str(&listing(first + i as u64, f));
        text.push('\n');
    }
    text.push_str("\nThumbnails only. SEARCH with `image` set to one's number shows that image in full, with its links.");
    if let Some(page) = more {
        text.push_str(&format!(" For more images, search again with page={page}."));
    }
    Ok(vec![json!({"type": "text", "text": text}), sheet.block()])
}

/// One image's line beside the sheet: "14. Title (2516x1881, site.com)".
fn listing(id: u64, f: &Found) -> String {
    let size = if f.width > 0 && f.height > 0 { format!("{}x{}", f.width, f.height) } else { String::new() };
    let about: Vec<&str> = [size.as_str(), f.source.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
    let title = if f.title.is_empty() { "(untitled)".to_string() } else { f.title.head(90) };
    if about.is_empty() { format!("{id}. {title}") } else { format!("{id}. {title} ({})", about.join(", ")) }
}

/// The images for each of `queries`, which are searched at once: they
/// take turns, each image once (as SEARCH merges its pages), with the
/// source's name.
fn find_all(queries: &[String], want: usize) -> Result<(Vec<Found>, &'static str), String> {
    let runs: Vec<Result<(Vec<Found>, &'static str), String>> = std::thread::scope(|s| {
        let jobs: Vec<_> = queries.iter().map(|q| s.spawn(move || find(q, want))).collect();
        jobs.into_iter().map(|j| j.join().unwrap_or_else(|_| Err("Image search failed".into()))).collect()
    });
    let via = match runs.iter().find_map(|r| r.as_ref().ok()) {
        Some((_, via)) => *via,
        None => return Err(runs.into_iter().find_map(Result::err).unwrap_or_default()),
    };
    Ok((merge(runs.into_iter().filter_map(Result::ok).map(|(found, _)| found).collect()), via))
}

/// The lists' images in turn (each list's first, then each one's second,
/// and so on), without those already taken.
fn merge(lists: Vec<Vec<Found>>) -> Vec<Found> {
    let mut out: Vec<Found> = vec![];
    for i in 0..lists.iter().map(Vec::len).max().unwrap_or(0) {
        for f in lists.iter().filter_map(|list| list.get(i)) {
            if !out.iter().any(|o| o.image == f.image) {
                out.push(f.clone());
            }
        }
    }
    out
}

/// Up to `want` images for `query` from the first source that has any,
/// and that source's name.
fn find(query: &str, want: usize) -> Result<(Vec<Found>, &'static str), String> {
    let mut failures = vec![];
    if let Some(key) = Provider::Serper.key() {
        match serper_call("images", json!({"q": query, "num": want.min(100)}), &key).map(|v| parse_serper(&v)) {
            Ok(found) if !found.is_empty() => return Ok((found, "Google")),
            Ok(_) => failures.push("Google: no results".to_string()),
            Err(e) => failures.push(format!("Google: {e}")),
        }
    }
    match brave(query) {
        Ok(found) if !found.is_empty() => return Ok((found, "Brave")),
        Ok(_) => failures.push("Brave: no results".to_string()),
        Err(e) => failures.push(format!("Brave: {e}")),
    }
    if failures.iter().all(|f| f.ends_with(": no results")) {
        return Ok((vec![], "Brave"));
    }
    Err(format!("Image search failed ({})", failures.join("; ")))
}

fn parse_serper(v: &Value) -> Vec<Found> {
    let s = |r: &Value, k: &str| r[k].as_str().unwrap_or("").to_string();
    let n = |r: &Value, k: &str| r[k].as_u64().unwrap_or(0) as u32;
    v["images"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["imageUrl"].as_str().is_some_and(|u| u.starts_with("http")))
        .map(|r| {
            let image = s(r, "imageUrl");
            let thumb = Some(s(r, "thumbnailUrl")).filter(|t| t.starts_with("http")).unwrap_or_else(|| image.clone());
            let source = Some(s(r, "domain")).filter(|d| !d.is_empty()).unwrap_or_else(|| site(&s(r, "link")));
            Found { title: one_line(&s(r, "title")), image, thumb, page: s(r, "link"), source, width: n(r, "imageWidth"), height: n(r, "imageHeight") }
        })
        .collect()
}

/// Brave's image search page, which carries its results (some 150 of
/// them) as a JavaScript object. Brave turns away Oak's own HTTP client
/// (by its TLS handshake, whatever the headers say) and answers curl, so
/// the page is fetched with the `curl` program: Windows has one, Git for
/// Windows another, and so has nearly every Linux.
fn brave(query: &str) -> Result<Vec<Found>, String> {
    let url = format!("https://search.brave.com/images?q={}", crate::web::percent(query));
    let (status, html) = curl(&url)?;
    let html = String::from_utf8_lossy(&html);
    let found = parse_brave(&html);
    if status == 429 || (found.is_empty() && html.contains("<title>Captcha")) {
        return Err("rate limited".into());
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    Ok(found)
}

/// GETs `url` with the `curl` program: the status and the body.
fn curl(url: &str) -> Result<(u16, Vec<u8>), String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sS", "-L", "--compressed", "--max-time", "20", "-A", AGENT, "-H", "Accept-Language: en-US,en;q=0.9", "-w", "\n%{http_code}", url])
        .stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().map_err(|e| format!("couldn't run curl: {e}"))?;
    if !out.status.success() {
        let why = one_line(&String::from_utf8_lossy(&out.stderr)).head(160);
        return Err(if why.is_empty() { format!("curl failed ({})", out.status) } else { why });
    }
    // The status is the last line (`-w`), after the body.
    let cut = out.stdout.iter().rposition(|&b| b == b'\n').ok_or("curl returned nothing")?;
    let status = std::str::from_utf8(&out.stdout[cut + 1..]).ok().and_then(|s| s.trim().parse().ok()).ok_or("curl returned no status")?;
    Ok((status, out.stdout[..cut].to_vec()))
}

/// The results in Brave's page. Each is an object literal whose fields
/// come in one order:
/// `{title:"…",url:"<page>",…,source:"<site>",thumbnail:{src:"…",…},properties:{url:"<image>",…,height:N,width:N,…}`.
fn parse_brave(html: &str) -> Vec<Found> {
    const START: &str = "{title:\"";
    const IMAGE: &str = "properties:{url:\"";
    let mut out = vec![];
    let mut rest = html;
    while let Some(at) = rest.find(IMAGE) {
        let (before, after) = (&rest[..at], &rest[at + IMAGE.len()..]);
        rest = after;
        // The object this belongs to starts at the last title before it.
        let Some(start) = before.rfind(START) else { continue };
        let object = &before[start + START.len()..];
        let Some((title, tail)) = js_string(object) else { continue };
        let field = |name: &str| tail.find(name).and_then(|i| js_string(&tail[i + name.len()..])).map(|(s, _)| s);
        let (Some(page), Some(thumb), Some((image, props))) = (tail.strip_prefix(",url:\"").and_then(js_string).map(|(s, _)| s), field("thumbnail:{src:\""), js_string(after)) else {
            continue;
        };
        if !image.starts_with("http") || !thumb.starts_with("http") {
            continue;
        }
        let props = &props[..props.find('}').unwrap_or(props.len())];
        let number = |name: &str| props.find(name).map(|i| props[i + name.len()..].chars().take_while(char::is_ascii_digit).collect::<String>()).and_then(|d| d.parse().ok()).unwrap_or(0);
        let source = field("source:\"").filter(|s| !s.is_empty()).unwrap_or_else(|| site(&page));
        out.push(Found { title: one_line(&title), image, thumb, page, source, width: number("width:"), height: number("height:") });
    }
    out
}

/// A JavaScript string's value, read from just after its opening quote,
/// and what follows its closing quote.
fn js_string(s: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, &s[i + 1..])),
            '\\' => match chars.next()?.1 {
                'n' | 'r' | 't' => out.push(' '),
                'u' => {
                    let hex: String = chars.by_ref().take(4).map(|(_, c)| c).collect();
                    out.push(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32).unwrap_or('\u{fffd}'));
                }
                'x' => {
                    let hex: String = chars.by_ref().take(2).map(|(_, c)| c).collect();
                    out.push(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32).unwrap_or('\u{fffd}'));
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// A URL's site: its host without `www.`.
fn site(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or("").trim_start_matches("www.").to_string()
}

// ------------------------------------------------------------- downloads

/// The bytes at `url`, at most `max` of them, or why not. `referer`: the
/// page an image is on, which some sites want to see.
fn download(url: &str, referer: Option<&str>, max: usize, time: Duration) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut req = client().get(url).timeout(time).header("User-Agent", AGENT).header("Accept", "image/avif,image/webp,image/png,image/jpeg,image/*,*/*;q=0.8");
    if let Some(r) = referer.filter(|r| r.starts_with("http")) {
        req = req.header("Referer", r);
    }
    let resp = req.send().map_err(|e| if e.is_timeout() { "timed out".to_string() } else { one_line(&e.to_string()).head(160) })?;
    if !resp.status().is_success() {
        return Err(http_error(resp.status()));
    }
    let mut bytes = vec![];
    resp.take(max as u64 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() > max {
        return Err(format!("larger than {} MB", max / 1_000_000));
    }
    Ok(bytes)
}

/// The images whose thumbnails load, in their order, each with its
/// thumbnail decoded; all are fetched at once.
fn thumbnails(found: Vec<Found>) -> Vec<(Found, DynamicImage)> {
    let loaded: Vec<Option<DynamicImage>> = std::thread::scope(|s| {
        let jobs: Vec<_> = found
            .iter()
            .map(|f| s.spawn(move || download(&f.thumb, None, THUMB_BYTES, THUMB_TIME).ok().and_then(|b| image::load_from_memory(&b).ok())))
            .collect();
        jobs.into_iter().map(|j| j.join().ok().flatten()).collect()
    });
    found.into_iter().zip(loaded).filter_map(|(f, img)| Some((f, img?))).collect()
}

// ------------------------------------------------------------- the sheet

/// The sheet is laid out as Google Images lays its results out: columns
/// of one width, each thumbnail in the shortest at its own height, its
/// corners rounded, its caption under it. `COLUMN` is a column's width,
/// `GAP` what is between two columns and under a caption, `MARGIN` what
/// is around it all.
const COLUMNS: usize = 4;
const COLUMN: u32 = 306;
const GAP: u32 = 10;
const MARGIN: u32 = 12;
const RADIUS: f32 = 12.0;
/// A caption's height: under the thumbnail its id, large, and beside the
/// id two lines, the image's title and the start of its link.
const CAPTION: u32 = 46;
/// A tile's width to its height, at least and at most: a taller or a
/// longer image is shown whole inside such a tile.
const ASPECT: (f32, f32) = (0.55, 3.2);

/// The captions' sizes, and their baselines from the thumbnail's bottom
/// (`ALONE`: a title or a link with no other line).
const ID_SIZE: f32 = 34.0;
const TITLE_SIZE: f32 = 15.0;
const LINK_SIZE: f32 = 14.0;
const ID_BASE: f32 = 37.0;
const TITLE_BASE: f32 = 21.0;
const LINK_BASE: f32 = 39.0;
const ALONE_BASE: f32 = 30.0;

/// The sheet's colors, which are the Oak theme's, so that it sits in the
/// transcript as a part of it: the page behind, the ids in oak green.
struct Colors {
    ground: [u8; 3],
    /// Behind an image that doesn't fill its tile.
    tile: [u8; 3],
    /// The line around a thumbnail, which keeps a dark one off the page.
    edge: [u8; 3],
    id: [u8; 3],
    title: [u8; 3],
    link: [u8; 3],
}

fn colors() -> Colors {
    let t = oak_sdk::theme::Theme::oak();
    let rgb = |c: eframe::egui::Color32| [c.r(), c.g(), c.b()];
    Colors { ground: rgb(t.bg), tile: rgb(t.panel2), edge: rgb(t.ink4), id: rgb(t.accent), title: rgb(t.ink), link: rgb(t.ink3) }
}

/// One image on the sheet.
struct Tile<'a> {
    id: u64,
    title: &'a str,
    /// Its page's link (or its own), as `short` gives it.
    link: String,
    picture: &'a DynamicImage,
}

/// One thumbnail's place on the sheet.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Place {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// Where thumbnails of these proportions (width to height) go, and the
/// sheet's width and height: each in the column that is shortest so far
/// (the leftmost of those), as wide as the column.
fn layout(aspects: &[f32]) -> (Vec<Place>, u32, u32) {
    let columns = aspects.len().clamp(1, COLUMNS);
    let mut ends = vec![MARGIN; columns];
    let places = aspects
        .iter()
        .map(|a| {
            let column = (0..columns).min_by_key(|c| ends[*c]).unwrap_or(0);
            let h = (COLUMN as f32 / a.clamp(ASPECT.0, ASPECT.1)).round() as u32;
            let place = Place { x: MARGIN + column as u32 * (COLUMN + GAP), y: ends[column], w: COLUMN, h };
            ends[column] += h + CAPTION + GAP;
            place
        })
        .collect();
    let width = 2 * MARGIN + columns as u32 * (COLUMN + GAP) - GAP;
    let height = ends.iter().max().map_or(MARGIN, |end| end - GAP) + MARGIN;
    (places, width, height)
}

/// `top` over `under`, as much of it as `cover` (0 to 1).
fn blend(under: [u8; 3], top: [u8; 3], cover: f32) -> [u8; 3] {
    std::array::from_fn(|k| (under[k] as f32 + (top[k] as f32 - under[k] as f32) * cover).round() as u8)
}

/// Draws `picture` at `place` with rounded corners and a line around
/// it. One too tall or too long for its tile is whole in the middle.
fn thumbnail(out: &mut RgbaImage, place: Place, picture: &DynamicImage, colors: &Colors) {
    let Place { x, y, w, h } = place;
    let [r, g, b] = colors.tile;
    let mut tile = RgbaImage::from_pixel(w, h, Rgba([r, g, b, 255]));
    let aspect = picture.width().max(1) as f32 / picture.height().max(1) as f32;
    let small = if (ASPECT.0..=ASPECT.1).contains(&aspect) { picture.resize_exact(w, h, FilterType::Triangle) } else { picture.resize(w, h, FilterType::Triangle) }.to_rgba8();
    let (dx, dy) = ((w - small.width().min(w)) / 2, (h - small.height().min(h)) / 2);
    image::imageops::overlay(&mut tile, &small, dx as i64, dy as i64);

    let (half_w, half_h) = (w as f32 / 2.0, h as f32 / 2.0);
    let radius = RADIUS.min(half_w).min(half_h);
    for ty in 0..h {
        for tx in 0..w {
            // How far the pixel's middle is outside the rounded outline.
            let qx = (tx as f32 + 0.5 - half_w).abs() - (half_w - radius);
            let qy = (ty as f32 + 0.5 - half_h).abs() - (half_h - radius);
            let out_by = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius;
            let pixel = tile.get_pixel(tx, ty);
            let mut color = [pixel[0], pixel[1], pixel[2]];
            if out_by > -1.5 {
                color = blend(color, colors.edge, (out_by + 1.5).clamp(0.0, 1.0));
                color = blend(colors.ground, color, (0.5 - out_by).clamp(0.0, 1.0));
            }
            if x + tx < out.width() && y + ty < out.height() {
                out.put_pixel(x + tx, y + ty, Rgba([color[0], color[1], color[2], 255]));
            }
        }
    }
}

/// How wide `text` is in `font` at `size`.
fn width(font: &FontRef, size: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size));
    let (mut pen, mut before) = (0.0, None);
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        pen += before.map_or(0.0, |b| scaled.kern(b, id)) + scaled.h_advance(id);
        before = Some(id);
    }
    pen
}

/// Draws `text` from `x` on the line whose baseline is `baseline`;
/// returns where it ends.
fn write(img: &mut RgbaImage, font: &FontRef, size: f32, x: f32, baseline: f32, text: &str, ink: [u8; 3]) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size));
    let (mut pen, mut before) = (x, None);
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        pen += before.map_or(0.0, |b| scaled.kern(b, id));
        if let Some(outline) = scaled.outline_glyph(id.with_scale_and_position(size, point(pen, baseline))) {
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, cover| {
                let (px, py) = (bounds.min.x as i64 + gx as i64, bounds.min.y as i64 + gy as i64);
                if px >= 0 && py >= 0 && px < img.width() as i64 && py < img.height() as i64 {
                    let pixel = img.get_pixel_mut(px as u32, py as u32);
                    let [r, g, b] = blend([pixel[0], pixel[1], pixel[2]], ink, cover.clamp(0.0, 1.0));
                    *pixel = Rgba([r, g, b, 255]);
                }
            });
        }
        pen += scaled.h_advance(id);
        before = Some(id);
    }
    pen
}

/// The start of `text` that fits in `room` pixels, with an ellipsis for
/// what was cut.
fn fit(font: &FontRef, size: f32, text: &str, room: f32) -> String {
    if width(font, size, text) <= room {
        return text.to_string();
    }
    let mut kept = String::new();
    for c in text.chars() {
        if width(font, size, &format!("{kept}{c}…")) > room {
            break;
        }
        kept.push(c);
    }
    if kept.is_empty() { kept } else { format!("{}…", kept.trim_end()) }
}

/// A link as a caption shows it: no scheme, `www.` or query.
fn short(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.trim_start_matches("www.").split(['?', '#']).next().unwrap_or("").trim_end_matches('/').to_string()
}

/// The contact sheet: the pictures in columns (`layout`), and under each
/// one its id with its title and the start of its link beside it.
fn sheet(tiles: &[Tile]) -> Result<RgbaImage, String> {
    let font = |bytes| FontRef::try_from_slice(bytes).map_err(|_| "Oak's font couldn't be read".to_string());
    let (bold, regular) = (font(crate::markdown::PLEX_BOLD)?, font(crate::markdown::PLEX_REGULAR)?);
    let colors = colors();
    let aspects: Vec<f32> = tiles.iter().map(|t| t.picture.width().max(1) as f32 / t.picture.height().max(1) as f32).collect();
    let (places, w, h) = layout(&aspects);
    let [r, g, b] = colors.ground;
    let mut out = RgbaImage::from_pixel(w, h, Rgba([r, g, b, 255]));
    for (tile, p) in tiles.iter().zip(places) {
        thumbnail(&mut out, p, tile.picture, &colors);
        let (left, top, right) = (p.x as f32 + 1.0, (p.y + p.h) as f32, (p.x + p.w) as f32 - 2.0);
        let text = write(&mut out, &bold, ID_SIZE, left, top + ID_BASE, &tile.id.to_string(), colors.id) + 9.0;
        let room = right - text;
        match (tile.title.is_empty(), tile.link.is_empty()) {
            (false, false) => {
                write(&mut out, &regular, TITLE_SIZE, text, top + TITLE_BASE, &fit(&regular, TITLE_SIZE, tile.title, room), colors.title);
                write(&mut out, &regular, LINK_SIZE, text, top + LINK_BASE, &fit(&regular, LINK_SIZE, &tile.link, room), colors.link);
            }
            (false, true) => drop(write(&mut out, &regular, TITLE_SIZE, text, top + ALONE_BASE, &fit(&regular, TITLE_SIZE, tile.title, room), colors.title)),
            (true, _) => drop(write(&mut out, &regular, LINK_SIZE, text, top + ALONE_BASE, &fit(&regular, LINK_SIZE, &tile.link, room), colors.link)),
        }
    }
    Ok(out)
}

// -------------------------------------------------------------- the index

/// One writer of an index at a time: searches run side by side.
static INDEX: Mutex<()> = Mutex::new(());

fn index_path(dir: &Path) -> PathBuf {
    dir.join("index.json")
}

fn read_index(dir: &Path) -> Vec<Value> {
    std::fs::read(index_path(dir)).ok().and_then(|b| serde_json::from_slice::<Vec<Value>>(&b).ok()).unwrap_or_default()
}

/// Gives `found` the next ids and keeps them in `dir`'s index; returns
/// the first id.
fn remember<'a>(dir: &Path, query: &str, found: impl Iterator<Item = &'a Found>) -> Result<u64, String> {
    let _one = INDEX.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't make {}: {e}", dir.display()))?;
    let mut index = read_index(dir);
    let first = index.iter().filter_map(|v| v["id"].as_u64()).max().unwrap_or(0) + 1;
    index.extend(found.enumerate().map(|(i, f)| f.json(first + i as u64, query)));
    let old = index.len().saturating_sub(KEPT);
    index.drain(..old);
    let text = serde_json::to_string(&index).map_err(|e| e.to_string())?;
    std::fs::write(index_path(dir), text).map_err(|e| format!("Couldn't write {}: {e}", index_path(dir).display()))?;
    Ok(first)
}

fn recall(dir: &Path, id: u64) -> Option<Found> {
    let _one = INDEX.lock().unwrap_or_else(|e| e.into_inner());
    read_index(dir).iter().find(|v| v["id"].as_u64() == Some(id)).map(Found::from_json)
}

// ------------------------------------------------------------------ view

/// `image`: an image's number on a sheet, or an image's link.
fn view(image: &str, dir: &Path) -> Result<Vec<Value>, String> {
    let (found, id) = match image.parse::<u64>() {
        Ok(id) => (recall(dir, id).ok_or(format!("No image {id}: the numbers are those on the sheet of a SEARCH with `images`."))?, Some(id)),
        Err(_) if image.starts_with("http://") || image.starts_with("https://") => (Found { image: image.to_string(), ..Found::default() }, None),
        Err(_) => return Err("`image` is the number of an image on a sheet, or an image's link starting with http:// or https://".into()),
    };
    // The image where it is published; when its site won't give it (or
    // it is a kind Oak can't read), the search engine's small copy.
    let full = download(&found.image, Some(&found.page), FULL_BYTES, FULL_TIME).and_then(|b| crate::images::from_bytes(&b).map(|img| (b, img)));
    let (bytes, img, note) = match full {
        Ok((b, img)) => (b, img, String::new()),
        Err(e) if !found.thumb.is_empty() && found.thumb != found.image => {
            let b = download(&found.thumb, None, THUMB_BYTES, THUMB_TIME).map_err(|t| format!("Couldn't load {} ({e}), nor its thumbnail ({t}).", found.image))?;
            let img = crate::images::from_bytes(&b).map_err(|t| format!("Couldn't load {} ({e}), nor its thumbnail ({t}).", found.image))?;
            (b, img, format!("\nThe full image couldn't be loaded ({e}): this is the search's thumbnail."))
        }
        Err(e) => return Err(format!("Couldn't load {}: {e}", found.image)),
    };
    let saved = save(dir, id, &found, &bytes);

    let mut text = match id {
        Some(id) => format!("Image {id}: {}\n", if found.title.is_empty() { "(untitled)" } else { &found.title }),
        None => String::new(),
    };
    text.push_str(&format!("Image: {}\n", found.image));
    if !found.page.is_empty() {
        text.push_str(&format!("Page: {}\n", found.page));
    }
    let (w, h) = image::load_from_memory(&bytes).map(|i| (i.width(), i.height())).unwrap_or((img.width, img.height));
    text.push_str(&if (w, h) == (img.width, img.height) { format!("{w}x{h}") } else { format!("{w}x{h} (shown at {}x{})", img.width, img.height) });
    match saved {
        Ok(path) => text.push_str(&format!(", saved to {}", path.display())),
        Err(e) => text.push_str(&format!(" (not saved: {e})")),
    }
    text.push_str(&note);
    Ok(vec![json!({"type": "text", "text": text}), img.block()])
}

/// Writes a viewed image's file into `dir`, named after its id and
/// title (or its link's last part); returns where.
fn save(dir: &Path, id: Option<u64>, found: &Found, bytes: &[u8]) -> Result<PathBuf, String> {
    let ext = match image::guess_format(bytes) {
        Ok(image::ImageFormat::Jpeg) => "jpg",
        Ok(image::ImageFormat::Gif) => "gif",
        Ok(image::ImageFormat::WebP) => "webp",
        _ => "png",
    };
    let last = found.image.split(['?', '#']).next().unwrap_or("").trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let stem = last.rsplit_once('.').map_or(last, |(s, _)| s);
    let name = slug(if found.title.is_empty() { stem } else { &found.title });
    let name = match id {
        Some(id) => format!("{id}-{name}.{ext}"),
        None => format!("{name}.{ext}"),
    };
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(name);
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    Ok(path)
}

/// A file name's stem from `text`: its letters and digits, lower case,
/// with dashes between the words, at most 40 characters.
fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if out.len() >= 40 {
            break;
        }
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-');
    if out.is_empty() { "image".to_string() } else { out.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oak-images-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn brave_results_parse() {
        let html = r#"<script>x={results:[{title:"VS Code \"Settings\" editor",url:"https:\u002F\u002Fcode.visualstudio.com\u002Fdocs\u002Fsettings",is_source_local:false,description:"",source:"code.visualstudio.com",thumbnail:{src:"https://imgs.search.brave.com/abc/rs:fit:500:0:1:0/g:ce/aHR0",alt:null,height:237,width:500,original:"https://code.visualstudio.com/a.png",bo_serp_visible:true},properties:{url:"https://code.visualstudio.com/a.png",resized:"https://imgs.search.brave.com/r",placeholder:"https://imgs.search.brave.com/p",height:1072,width:2266,format:null},meta_url:{scheme:"https",path:"x"},confidence:"high"},{title:"Second",url:"https://www.example.org/page",source:"",thumbnail:{src:"https://imgs.search.brave.com/def",height:10,width:20},properties:{url:"https://example.org/b.jpg",height:600,width:800}},{title:"No picture",url:"https://x.example"}]}</script>"#;
        let got = parse_brave(html);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(
            got[0],
            Found {
                title: "VS Code \"Settings\" editor".into(),
                image: "https://code.visualstudio.com/a.png".into(),
                thumb: "https://imgs.search.brave.com/abc/rs:fit:500:0:1:0/g:ce/aHR0".into(),
                page: "https://code.visualstudio.com/docs/settings".into(),
                source: "code.visualstudio.com".into(),
                width: 2266,
                height: 1072,
            }
        );
        // No source given: the page's site.
        assert_eq!((got[1].source.as_str(), got[1].width, got[1].height), ("example.org", 800, 600));
        assert!(parse_brave("<html><title>Captcha - Brave Search</title></html>").is_empty());
    }

    #[test]
    fn serper_images_parse() {
        let v = json!({"images": [{"title": "Apple Inc. - Wikipedia", "imageUrl": "https://upload.wikimedia.org/a.png", "imageWidth": 814, "imageHeight": 1000,
            "thumbnailUrl": "https://encrypted-tbn0.gstatic.com/images?q=x", "source": "Wikipedia", "domain": "en.wikipedia.org", "link": "https://en.wikipedia.org/wiki/Apple_Inc.", "position": 1}]});
        let got = parse_serper(&v);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].thumb.as_str(), got[0].source.as_str(), got[0].width, got[0].height), ("https://encrypted-tbn0.gstatic.com/images?q=x", "en.wikipedia.org", 814, 1000));
        assert_eq!(listing(7, &got[0]), "7. Apple Inc. - Wikipedia (814x1000, en.wikipedia.org)");
    }

    #[test]
    fn thumbnails_go_to_the_shortest_column() {
        // Twelve 16:10 screenshots: four columns, three in each, in reading order.
        let (places, w, h) = layout(&[1.6; 12]);
        let pitch = 191 + CAPTION + GAP;
        assert_eq!((places.len(), w, h), (12, 2 * MARGIN + 4 * COLUMN + 3 * GAP, 2 * MARGIN + 3 * pitch - GAP));
        assert_eq!(places[0], Place { x: MARGIN, y: MARGIN, w: COLUMN, h: 191 });
        assert_eq!(places[3], Place { x: MARGIN + 3 * (COLUMN + GAP), y: MARGIN, w: COLUMN, h: 191 });
        assert_eq!(places[4], Place { x: MARGIN, y: MARGIN + pitch, w: COLUMN, h: 191 });

        // Each at its own height; the next goes under the shortest column
        // (the leftmost of them), not under the first.
        let (places, _, h) = layout(&[1.0, 2.0, 2.0, 2.0, 1.0, 0.2, 6.0]);
        assert_eq!((places[0].h, places[1].h), (306, 153));
        assert_eq!(places[4], Place { x: MARGIN + COLUMN + GAP, y: MARGIN + 153 + CAPTION + GAP, w: COLUMN, h: 306 });
        // The very tall and the very long keep to the limits.
        assert_eq!((places[5].x, places[5].h, places[6].h), (MARGIN + 2 * (COLUMN + GAP), 556, 96));
        assert_eq!(h, MARGIN + 153 + CAPTION + GAP + 556 + CAPTION + MARGIN);

        // A lone picture: one column.
        assert_eq!(layout(&[1.6]), (vec![Place { x: MARGIN, y: MARGIN, w: COLUMN, h: 191 }], 2 * MARGIN + COLUMN, 2 * MARGIN + 191 + CAPTION));
    }

    #[test]
    fn the_sheet_has_each_id_title_and_link_under_its_picture() {
        let c = colors();
        let px = |rgb: [u8; 3]| Rgba([rgb[0], rgb[1], rgb[2], 255]);
        let (red_ink, blue_ink) = (Rgba([200, 0, 0, 255]), Rgba([0, 0, 200, 255]));
        let red = DynamicImage::ImageRgba8(RgbaImage::from_pixel(600, 300, red_ink));
        let tall = DynamicImage::ImageRgba8(RgbaImage::from_pixel(100, 400, blue_ink));
        let long = "code.visualstudio.com/docs/getstarted/personalize-vscode/and/a/long/way/further/down/the/site";
        let s = sheet(&[
            Tile { id: 95, title: "A title that goes on and on, far past the end of its thumbnail", link: long.to_string(), picture: &red },
            Tile { id: 96, title: "", link: "x.example".to_string(), picture: &tall },
        ])
        .unwrap();
        assert_eq!((s.width(), s.height()), (2 * MARGIN + 2 * COLUMN + GAP, 2 * MARGIN + 556 + CAPTION));
        // The Oak theme's page around; the first picture with its corners
        // rounded off and a line around it.
        assert_eq!((*s.get_pixel(0, 0), *s.get_pixel(MARGIN, MARGIN)), (px(c.ground), px(c.ground)));
        assert_eq!(*s.get_pixel(MARGIN, MARGIN + 76), px(c.edge));
        assert_eq!((*s.get_pixel(MARGIN + 2, MARGIN + 76), *s.get_pixel(MARGIN + 153, MARGIN + 76)), (red_ink, red_ink));
        // The tall one whole, in the middle of the tallest tile there is.
        let x = MARGIN + COLUMN + GAP;
        assert_eq!((*s.get_pixel(x + 20, 300), *s.get_pixel(x + 153, 300)), (px(c.tile), blue_ink));
        // The captions: the id in the accent, writing beside it, cut before the next column.
        let written = |x: std::ops::Range<u32>, y: u32| x.flat_map(|x| (y..y + CAPTION).map(move |y| (x, y))).filter(|&(x, y)| s.get_pixel(x, y)[1] > 60).count();
        let under = MARGIN + 153;
        assert!(written(MARGIN..MARGIN + 44, under) > 150 && written(MARGIN + 50..MARGIN + COLUMN, under) > 500, "{} {}", written(MARGIN..MARGIN + 44, under), written(MARGIN + 50..MARGIN + COLUMN, under));
        assert_eq!(written(MARGIN + COLUMN - 1..x, under), 0);
        assert!((MARGIN..MARGIN + 44).any(|x| (under..under + CAPTION).any(|y| *s.get_pixel(x, y) == px(c.id))));
        assert!(written(x..x + 44, MARGIN + 556) > 150 && written(x + 50..x + COLUMN, MARGIN + 556) > 60);

        let regular = FontRef::try_from_slice(crate::markdown::PLEX_REGULAR).unwrap();
        let cut = fit(&regular, LINK_SIZE, long, 200.0);
        assert!(cut.starts_with("code.visualstudio.com/") && cut.ends_with('…') && width(&regular, LINK_SIZE, &cut) <= 200.0, "{cut}");
        assert_eq!(fit(&regular, LINK_SIZE, "x.example", 200.0), "x.example");
        assert_eq!(short("https://www.learn.microsoft.com/en-us/visualstudio/ide/?view=vs-2022#top"), "learn.microsoft.com/en-us/visualstudio/ide");
    }

    #[test]
    fn ids_count_up_and_are_kept() {
        let d = dir("ids");
        let f = |n: &str| Found { title: n.into(), image: format!("https://x.example/{n}.png"), thumb: "https://t.example/t".into(), page: "https://x.example/".into(), source: "x.example".into(), width: 3, height: 2 };
        let (a, b, c) = (f("a"), f("b"), f("c"));
        assert_eq!(remember(&d, "first", [&a, &b].into_iter()).unwrap(), 1);
        assert_eq!(remember(&d, "second", [&c].into_iter()).unwrap(), 3);
        assert_eq!(recall(&d, 2), Some(b));
        assert_eq!(recall(&d, 3), Some(c));
        assert_eq!(recall(&d, 4), None);
        // Asking for one that isn't there says what ids are.
        let e = images(&json!({"image": 9}), &d).unwrap_err();
        assert!(e.starts_with("No image 9"), "{e}");
        assert_eq!(images(&json!({"image": " 9 "}), &d).unwrap_err(), e);
        assert!(images(&json!({"image": "file:///x.png"}), &d).unwrap_err().starts_with("`image` is the number"));
        // Which SEARCH calls are for images.
        assert!(wanted(&json!({"images": true, "queries": ["x"]})) && wanted(&json!({"image": 3})) && wanted(&json!({"image": "https://x.example/a.png"})));
        assert!(!wanted(&json!({"queries": ["x"]})) && !wanted(&json!({"images": false, "image": ""})));
        assert_eq!(images(&json!({"images": true}), &d).unwrap_err(), "Give at least one query.");
        // Several queries' images take turns, each once.
        let lists = vec![vec![f("a"), f("b")], vec![f("b"), f("c"), f("d")]];
        assert_eq!(merge(lists).iter().map(|f| f.title.as_str()).collect::<Vec<_>>(), ["a", "b", "c", "d"]);
        assert_eq!((count("12 images for \"x\" (via Brave), each"), count("1 image for \"x\""), count("3 results for \"x\" (via Exa):")), (Some(12), Some(1), None));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn saved_files_are_named_after_the_image() {
        assert_eq!(slug("Visual Studio Code - Settings (UI)!"), "visual-studio-code-settings-ui");
        assert_eq!(slug("日本語"), "image");
        let d = dir("save");
        let png = {
            let mut out = std::io::Cursor::new(vec![]);
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 4, Rgba([1, 2, 3, 255]))).write_to(&mut out, image::ImageFormat::Png).unwrap();
            out.into_inner()
        };
        let titled = Found { title: "A Shot".into(), image: "https://x.example/img/shot.webp?x=1".into(), ..Found::default() };
        assert_eq!(save(&d, Some(12), &titled, &png).unwrap(), d.join("12-a-shot.png"));
        let bare = Found { image: "https://x.example/img/Main_Window.final.jpeg?x=1".into(), ..Found::default() };
        assert_eq!(save(&d, None, &bare, &png).unwrap(), d.join("main-window-final.png"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// For real: `cargo test live_images_search -- --ignored --nocapture`
    /// (OAK_TEST_QUERY sets the query; the sheet and the first image are
    /// left in the temp folder it prints).
    #[test]
    #[ignore]
    fn live_images_search() {
        let d = dir("live");
        let query = std::env::var("OAK_TEST_QUERY").unwrap_or_else(|_| "visual studio code settings editor screenshot".into());
        let t = std::time::Instant::now();
        let blocks = images(&json!({"images": true, "queries": [query]}), &d).unwrap();
        println!("{}\n[{} ms]", blocks[0]["text"].as_str().unwrap(), t.elapsed().as_millis());
        let sheet = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, blocks[1]["data"].as_str().unwrap()).unwrap();
        std::fs::write(d.join("sheet.jpg"), &sheet).unwrap();
        println!("sheet: {} ({} bytes)", d.join("sheet.jpg").display(), sheet.len());
        let blocks = images(&json!({"images": true, "objective": "what the settings look like", "queries": [query, "vscode settings json editor"], "page": 2, "max_results": 4}), &d).unwrap();
        println!("{}", blocks[0]["text"].as_str().unwrap());
        let t = std::time::Instant::now();
        let blocks = images(&json!({"image": "1"}), &d).unwrap();
        println!("{}\n[{} ms]", blocks[0]["text"].as_str().unwrap(), t.elapsed().as_millis());
        assert_eq!(blocks[1]["type"], "image");
    }
}
