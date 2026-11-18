// Pictures in conversations, kept as files of their own: `images` in Oak's
// folder (`store::dir`), each named after the SHA-256 of its bytes with its
// type's extension, so a picture is stored once however many conversations
// hold it. A conversation's image block names its file (`blob`) and its
// size in pixels (`_oak_size`) in place of the base64 data (`externalize`);
// the data goes back in only for what is sent (`inline`, as a request goes
// out: `api::stream`). Transcript files and the messages Oak holds are then
// text, and a picture is read from disk when it is drawn or sent.
//
// Only data that reads back exactly as it was is moved out (standard base64
// with padding, as Oak writes it), so a request sends byte for byte what it
// sent before and the prompt cache still matches. Files are never removed:
// a revert or another conversation (a side chat's fork) may still name them.

use base64::Engine;
use serde_json::{Value, json};
use std::path::PathBuf;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// Where the pictures are kept. Under test, a folder of the test run's own
/// (as `store::conversations_dir`).
pub fn dir() -> PathBuf {
    #[cfg(test)]
    let d = std::env::temp_dir().join(format!("oak-test-images-{}", std::process::id()));
    #[cfg(not(test))]
    let d = crate::store::dir().join("images");
    static MADE: std::sync::Once = std::sync::Once::new();
    MADE.call_once(|| {
        let _ = std::fs::create_dir_all(&d);
    });
    d
}

fn extension(media_type: &str) -> Option<&'static str> {
    Some(match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return None,
    })
}

/// The file a block's `blob` names: a hash and an extension, nothing that
/// could reach outside the folder.
pub fn path(name: &str) -> Option<PathBuf> {
    let (hash, ext) = name.split_once('.')?;
    let ok = hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()) && matches!(ext, "png" | "jpg" | "gif" | "webp");
    ok.then(|| dir().join(name))
}

/// Whether `b` is an image block that still holds its data.
fn inline_image(b: &Value) -> bool {
    b["type"] == "image" && b["data"].is_string()
}

/// Whether message `m` has an image that still holds its data (in a tool
/// result too).
pub fn has_inline(m: &Value) -> bool {
    m["content"].as_array().into_iter().flatten().any(|b| inline_image(b) || b["content"].as_array().is_some_and(|c| c.iter().any(inline_image)))
}

/// Moves the data of the images in message `m` (and in its tool results)
/// to their files. True when any moved.
pub fn externalize(m: &mut Value) -> bool {
    let mut moved = false;
    for b in m.get_mut("content").and_then(Value::as_array_mut).into_iter().flatten() {
        moved |= externalize_block(b);
        for inner in b.get_mut("content").and_then(Value::as_array_mut).into_iter().flatten() {
            moved |= externalize_block(inner);
        }
    }
    moved
}

/// Moves an image block's data to its file (`externalize`). Data that
/// doesn't read back exactly, of a type without a file extension here, or
/// that can't be written stays where it is.
fn externalize_block(b: &mut Value) -> bool {
    if !inline_image(b) {
        return false;
    }
    let Some(o) = b.as_object_mut() else { return false };
    let data = o.get("data").and_then(Value::as_str).unwrap_or("");
    let Some(ext) = o.get("media_type").and_then(Value::as_str).and_then(extension) else { return false };
    let Ok(bytes) = B64.decode(data) else { return false };
    if bytes.is_empty() || B64.encode(&bytes) != data {
        return false;
    }
    let name = format!("{}.{ext}", crate::api::hex(&<sha2::Sha256 as sha2::Digest>::digest(&bytes)[..]));
    let file = dir().join(&name);
    // (Named after what it holds: one already there is this picture.)
    if std::fs::metadata(&file).map_or(true, |m| m.len() != bytes.len() as u64) && crate::store::write_atomic_unsynced(&file, &bytes).is_err() {
        return false;
    }
    let size = image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format().ok().and_then(|r| r.into_dimensions().ok());
    o.remove("data");
    o.insert("blob".into(), json!(name));
    if let Some((w, h)) = size {
        o.insert("_oak_size".into(), json!([w, h]));
    }
    true
}

/// The bytes of the file an image block names.
fn read(name: &str) -> Option<Vec<u8>> {
    std::fs::read(path(name)?).ok()
}

/// What stands in for a picture whose file is gone.
const GONE: &str = "[A picture that is no longer on disk]";

/// Puts the data of each image in `messages` (a request's) back from its
/// file; a picture whose file is gone goes as a line saying so.
pub fn inline(messages: &mut [Value]) {
    for m in messages {
        for b in m.get_mut("content").and_then(Value::as_array_mut).into_iter().flatten() {
            inline_block(b);
            for inner in b.get_mut("content").and_then(Value::as_array_mut).into_iter().flatten() {
                inline_block(inner);
            }
        }
    }
}

/// Whether any image in `messages` names a file instead of holding data.
pub fn any_file(messages: &[Value]) -> bool {
    let named = |b: &Value| b["type"] == "image" && b["blob"].is_string();
    messages.iter().flat_map(|m| m["content"].as_array().into_iter().flatten()).any(|b| named(b) || b["content"].as_array().is_some_and(|c| c.iter().any(named)))
}

fn inline_block(b: &mut Value) {
    if b["type"] != "image" || b["data"].is_string() {
        return;
    }
    // Neither data nor a file's name (written by something that stripped
    // it): sending it would get the whole request refused (HTTP 400), so
    // it goes as a line too.
    let Some(name) = b["blob"].as_str() else {
        *b = json!({"type": "text", "text": GONE});
        return;
    };
    match read(name) {
        Some(bytes) => {
            let o = b.as_object_mut().unwrap();
            o.remove("blob");
            o.remove("_oak_size");
            o.insert("data".into(), json!(B64.encode(bytes)));
        }
        None => *b = json!({"type": "text", "text": GONE}),
    }
}

/// An image block's bytes: its data, or its file's.
pub fn bytes_of(b: &Value) -> Option<Vec<u8>> {
    match b["data"].as_str() {
        Some(d) => B64.decode(d).ok(),
        None => read(b["blob"].as_str()?),
    }
}

/// An image block's data as base64: its own, or its file's.
pub fn data_of(b: &Value) -> Option<String> {
    match b["data"].as_str() {
        Some(d) => Some(d.to_string()),
        None => read(b["blob"].as_str()?).map(|bytes| B64.encode(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(5, 3, |x, y| image::Rgba([x as u8 * 40, y as u8 * 60, 7, 255])));
        let mut out = std::io::Cursor::new(vec![]);
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn images_move_to_files_and_back_exactly() {
        let data = B64.encode(png());
        let original = json!({"role": "user", "content": [
            {"type": "image", "media_type": "image/png", "data": data, "_oak_at": 5},
            {"type": "tool_result", "call_id": "c", "content": [{"type": "text", "text": "shot"}, {"type": "image", "media_type": "image/png", "data": data}]},
        ]});
        let mut m = original.clone();
        assert!(has_inline(&m));
        assert!(externalize(&mut m));
        assert!(!has_inline(&m) && !externalize(&mut m));
        let b = &m["content"][0];
        assert!(b["data"].is_null() && path(b["blob"].as_str().unwrap()).unwrap().exists());
        assert_eq!(b["_oak_size"], json!([5, 3]));
        assert_eq!(b["_oak_at"], 5);
        assert_eq!(m["content"][1]["content"][1]["blob"], b["blob"], "the same picture, one file");
        assert_eq!(bytes_of(b).unwrap(), png());
        assert_eq!(data_of(b).unwrap(), data);
        let mut sent = vec![m];
        inline(&mut sent);
        assert_eq!(sent[0], original);
    }

    #[test]
    fn what_wouldnt_read_back_stays() {
        // Not canonical base64 (a line break), and a type with no file.
        let png = B64.encode(png());
        let odd = format!("{}\n{}", &png[..8], &png[8..]);
        let mut m = json!({"role": "user", "content": [
            {"type": "image", "media_type": "image/png", "data": odd},
            {"type": "image", "media_type": "image/bmp", "data": png},
        ]});
        let before = m.clone();
        assert!(!externalize(&mut m));
        assert_eq!(m, before);
        // A file that is gone goes as a line.
        let mut gone = vec![json!({"role": "user", "content": [{"type": "image", "media_type": "image/png", "blob": format!("{}.png", "0".repeat(64))}]})];
        inline(&mut gone);
        assert_eq!(gone[0]["content"][0], json!({"type": "text", "text": GONE}));
        // Nor data nor a file (in a tool result too): a line, never a null.
        let mut bare = vec![json!({"role": "user", "content": [
            {"type": "image", "media_type": "image/png"},
            {"type": "tool_result", "call_id": "c", "content": [{"type": "text", "text": "shot"}, {"type": "image", "media_type": "image/png", "data": null}]},
        ]})];
        inline(&mut bare);
        assert_eq!(bare[0]["content"][0], json!({"type": "text", "text": GONE}));
        assert_eq!(bare[0]["content"][1]["content"][1], json!({"type": "text", "text": GONE}));
        assert!(path("../x.png").is_none() && path(&format!("{}.exe", "a".repeat(64))).is_none());
    }
}
