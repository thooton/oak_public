//! Syntax highlighting for the code in READ, WRITE, EDIT and EXECUTE calls
//! (for files whose name doesn't tell, and command output, only when the
//! text is clearly code: `detect`): a small tokenizer that knows each
//! language's keywords, comments and strings, which is enough to color code by kind (keywords, strings, numbers,
//! comments, types, function names). EDIT calls show as a diff: removed
//! lines on red, added lines on green.

use crate::Theme;
use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{Color32, FontId};
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Keyword,
    Str,
    Number,
    Comment,
    Type,
    Func,
}

/// What the tokenizer needs to know about a language.
#[derive(Clone, Copy, Debug)]
pub struct Syntax {
    keywords: &'static [&'static str],
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    /// The characters that open (and close) a string.
    quotes: &'static str,
    /// Strings may run over several lines (else a string ends at the line's end).
    multiline_strings: bool,
    /// Python's """ and ''' strings.
    triple_quotes: bool,
    /// Rust: a ' starts a char literal only as 'x' or '\n' (else it is a lifetime).
    rust_chars: bool,
    /// Capitalized names are types (not in shell scripts or config files).
    types: bool,
    /// `$` may be part of a name (JavaScript, shell variables, PHP).
    dollar: bool,
}

const BASE: Syntax = Syntax {
    keywords: &[],
    line_comments: &[],
    block_comment: None,
    quotes: "\"'",
    multiline_strings: false,
    triple_quotes: false,
    rust_chars: false,
    types: true,
    dollar: false,
};

const RUST: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in",
    "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true",
    "type", "unsafe", "use", "where", "while", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize",
    "f32", "f64", "bool", "char", "str",
];
const C: &[&str] = &[
    "auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else", "enum", "extern", "float", "for", "goto", "if",
    "inline", "int", "long", "register", "return", "short", "signed", "sizeof", "static", "struct", "switch", "typedef", "union",
    "unsigned", "void", "volatile", "while", "bool", "true", "false", "NULL", "nullptr", "class", "namespace", "template", "typename",
    "public", "private", "protected", "virtual", "override", "new", "delete", "this", "using", "try", "catch", "throw", "constexpr",
    "#include", "#define", "#if", "#ifdef", "#ifndef", "#endif", "#else", "#pragma",
];
const JS: &[&str] = &[
    "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete", "do", "else", "export",
    "extends", "false", "finally", "for", "from", "function", "if", "import", "in", "instanceof", "let", "new", "null", "of", "return",
    "static", "super", "switch", "this", "throw", "true", "try", "typeof", "undefined", "var", "void", "while", "yield", "interface",
    "type", "enum", "implements", "private", "public", "protected", "readonly", "as", "keyof", "string", "number", "boolean", "any",
];
const PY: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
    "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try",
    "while", "with", "yield", "self", "print",
];
const GO: &[&str] = &[
    "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough", "for", "func", "go", "goto", "if", "import",
    "interface", "map", "package", "range", "return", "select", "struct", "switch", "type", "var", "nil", "true", "false", "string",
    "int", "int64", "int32", "uint", "uint64", "byte", "rune", "bool", "error", "float64", "any",
];
const JAVA: &[&str] = &[
    "abstract", "boolean", "break", "byte", "case", "catch", "char", "class", "const", "continue", "default", "do", "double", "else",
    "enum", "extends", "final", "finally", "float", "for", "if", "implements", "import", "instanceof", "int", "interface", "long",
    "native", "new", "package", "private", "protected", "public", "return", "short", "static", "super", "switch", "synchronized",
    "this", "throw", "throws", "try", "void", "volatile", "while", "true", "false", "null", "var", "val", "fun", "when", "object",
    "namespace", "using", "string", "bool", "async", "await", "override", "virtual", "readonly", "get", "set",
];
const SH: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "in", "function", "return", "local",
    "export", "readonly", "declare", "set", "unset", "shift", "exit", "echo", "cd", "source", "trap", "true", "false",
];
const CONFIG: &[&str] = &["true", "false", "null", "yes", "no", "on", "off"];
const SQL: &[&str] = &[
    "select", "from", "where", "insert", "into", "values", "update", "set", "delete", "create", "table", "drop", "alter", "index", "join",
    "left", "right", "inner", "outer", "on", "and", "or", "not", "null", "as", "order", "by", "group", "having", "limit", "primary",
    "key", "foreign", "references", "SELECT", "FROM", "WHERE", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE",
    "TABLE", "DROP", "ALTER", "INDEX", "JOIN", "LEFT", "INNER", "ON", "AND", "OR", "NOT", "NULL", "AS", "ORDER", "BY", "GROUP",
    "LIMIT", "PRIMARY", "KEY",
];
const LUA: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in", "local", "nil", "not", "or", "repeat",
    "return", "then", "true", "until", "while",
];
const RUBY: &[&str] = &[
    "alias", "and", "begin", "break", "case", "class", "def", "do", "else", "elsif", "end", "ensure", "false", "for", "if", "in",
    "module", "next", "nil", "not", "or", "redo", "rescue", "retry", "return", "self", "super", "then", "true", "unless", "until",
    "when", "while", "yield", "require", "attr_accessor",
];
const CSS: &[&str] = &["important", "media", "import", "from", "to", "inherit", "none", "auto"];

/// A file's name, lowercase, and its extension (None without a dot).
fn name_ext(path: &str) -> (String, Option<String>) {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path).to_ascii_lowercase();
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_string());
    (name, ext)
}

/// The language of a file, from its name (its extension, or a name
/// without one such as "Makefile"); None for prose and unknown files.
pub fn for_path(path: &str) -> Option<Syntax> {
    let (name, ext) = name_ext(path);
    let ext = ext.as_deref().unwrap_or(&name);
    let c_like = Syntax { line_comments: &["//"], block_comment: Some(("/*", "*/")), ..BASE };
    let hash = Syntax { line_comments: &["#"], ..BASE };
    Some(match ext {
        "rs" => Syntax { keywords: RUST, multiline_strings: true, rust_chars: true, quotes: "\"'", ..c_like },
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "m" | "mm" => Syntax { keywords: C, ..c_like },
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" | "vue" | "svelte" => {
            Syntax { keywords: JS, quotes: "\"'`", multiline_strings: true, dollar: true, ..c_like }
        }
        "json" | "jsonc" | "json5" => Syntax { keywords: CONFIG, types: false, ..c_like },
        "go" => Syntax { keywords: GO, quotes: "\"'`", ..c_like },
        "java" | "kt" | "kts" | "cs" | "scala" | "swift" | "dart" | "groovy" | "gradle" => Syntax { keywords: JAVA, ..c_like },
        "php" => Syntax { keywords: JAVA, dollar: true, line_comments: &["//", "#"], ..c_like },
        "py" | "pyw" | "pyi" => Syntax { keywords: PY, triple_quotes: true, ..hash },
        "rb" | "rake" | "gemspec" => Syntax { keywords: RUBY, ..hash },
        "sh" | "bash" | "zsh" | "fish" | "bashrc" | "profile" => Syntax { keywords: SH, types: false, dollar: true, multiline_strings: true, ..hash },
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "env" | "properties" | "gitignore" | "gitattributes" | "dockerignore" => {
            Syntax { keywords: CONFIG, types: false, ..hash }
        }
        "dockerfile" | "makefile" | "mk" | "cmake" | "r" | "pl" | "ps1" | "nix" => Syntax { types: false, dollar: true, ..hash },
        "sql" => Syntax { keywords: SQL, types: false, line_comments: &["--"], block_comment: Some(("/*", "*/")), ..BASE },
        "lua" => Syntax { keywords: LUA, line_comments: &["--"], ..BASE },
        "hs" | "elm" => Syntax { line_comments: &["--"], block_comment: Some(("{-", "-}")), ..BASE },
        "css" | "scss" | "less" => Syntax { keywords: CSS, types: false, ..c_like },
        "html" | "htm" | "xml" | "svg" | "xaml" | "csproj" | "plist" => {
            Syntax { types: false, block_comment: Some(("<!--", "-->")), multiline_strings: true, ..BASE }
        }
        "bat" | "cmd" => Syntax { types: false, line_comments: &["::", "rem ", "REM "], ..BASE },
        _ => return None,
    })
}

/// The language of a markdown code fence's info word (```rust), for the
/// replies Oak shows: the names people write that aren't file extensions,
/// then the extension's own.
pub fn for_fence(lang: &str) -> Option<Syntax> {
    let lang = lang.trim().to_ascii_lowercase();
    let ext = match lang.as_str() {
        "c++" | "cplusplus" | "cpp" | "h++" => "cpp",
        "c#" | "csharp" | "csx" => "cs",
        "objective-c" | "objc" => "m",
        "bash" | "shell" | "sh" | "zsh" | "console" | "shell-session" | "terminal" => "sh",
        "python" | "python3" | "py3" => "py",
        "javascript" | "js" | "node" => "js",
        "typescript" | "ts" => "ts",
        "rust" | "rs" => "rs",
        "golang" | "go" => "go",
        "json" | "jsonc" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "html" | "htm" | "xml" | "svg" | "xhtml" => "html",
        "css" | "scss" | "less" => "css",
        "sql" | "postgres" | "mysql" | "sqlite" => "sql",
        "markdown" | "md" | "text" | "txt" | "plaintext" | "plain" => return None,
        "diff" | "patch" => return None,
        other => other,
    };
    for_path(&format!("x.{ext}"))
}

fn ident_start(c: char, s: &Syntax) -> bool {
    c.is_alphabetic() || c == '_' || (s.dollar && c == '$')
}

fn ident_char(c: char, s: &Syntax) -> bool {
    c.is_alphanumeric() || c == '_' || (s.dollar && c == '$')
}

/// The text cut into runs by kind, covering all of it.
pub fn spans(text: &str, s: &Syntax) -> Vec<(Range<usize>, Kind)> {
    let mut out: Vec<(Range<usize>, Kind)> = vec![];
    let mut i = 0;
    // The last name seen: a name after `fn` (or `def`, ...) is a function's.
    let mut prev_word = "";
    while i < text.len() {
        let c = text[i..].chars().next().unwrap();
        let (end, kind) = if let Some(end) = comment_end(text, i, s) {
            (end, Kind::Comment)
        } else if let Some(end) = string_end(text, i, c, s) {
            (end, Kind::Str)
        } else if c.is_ascii_digit() {
            (number_end(text.as_bytes(), i), Kind::Number)
        } else if ident_start(c, s) || (c == '#' && s.keywords.iter().any(|k| k.starts_with('#'))) {
            let (end, kind) = name(text, i, c, s, prev_word);
            prev_word = &text[i..end];
            (end, kind)
        } else {
            if !c.is_whitespace() {
                prev_word = "";
            }
            (i + c.len_utf8(), Kind::Plain)
        };
        if end > i {
            match out.last_mut() {
                Some((last, lk)) if *lk == kind && last.end == i => last.end = end,
                _ => out.push((i..end, kind)),
            }
        }
        i = end;
    }
    out
}

/// The end of the comment starting at `i`, if one does: to the line's end,
/// or past the block's close.
fn comment_end(text: &str, i: usize, s: &Syntax) -> Option<usize> {
    let b = text.as_bytes();
    if let Some(lc) = s.line_comments.iter().find(|lc| text[i..].starts_with(*lc)) {
        // "#" only starts a comment at a word's start (not "a#b", "$#").
        let ok = !(*lc == "#" && i > 0 && !b[i - 1].is_ascii_whitespace() && !b"([{;,".contains(&b[i - 1]));
        if ok {
            return Some(text[i..].find('\n').map_or(text.len(), |n| i + n));
        }
    }
    let (open, close) = s.block_comment.filter(|(open, _)| text[i..].starts_with(open))?;
    Some(text[i + open.len()..].find(close).map_or(text.len(), |n| i + open.len() + n + close.len()))
}

/// The end of the string starting at `i` with `c`, if one does (a Rust
/// lifetime's quote doesn't start one).
fn string_end(text: &str, i: usize, c: char, s: &Syntax) -> Option<usize> {
    if s.triple_quotes && (text[i..].starts_with("\"\"\"") || text[i..].starts_with("'''")) {
        let q = &text[i..i + 3];
        return Some(text[i + 3..].find(q).map_or(text.len(), |n| i + 3 + n + 3));
    }
    if !s.quotes.contains(c) {
        return None;
    }
    let lifetime = s.rust_chars && c == '\'' && {
        let rest: Vec<char> = text[i + 1..].chars().take(3).collect();
        !(rest.get(1) == Some(&'\'') || rest.first() == Some(&'\\'))
    };
    if lifetime {
        return None;
    }
    let multiline = s.multiline_strings || c == '`';
    let b = text.as_bytes();
    let mut j = i + 1;
    while j < text.len() {
        match b[j] {
            b'\\' => j += 2,
            b'\n' if !multiline => return Some(j),
            d if d == c as u8 => return Some(j + 1),
            _ => j += 1,
        }
    }
    Some(text.len())
}

/// The end of the number starting at `i`.
fn number_end(b: &[u8], i: usize) -> usize {
    let mut j = i;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || (b[j] == b'.' && b.get(j + 1).is_some_and(u8::is_ascii_digit))) {
        j += 1;
    }
    j
}

/// The end and kind of the name starting at `i` with `c`: a keyword, a
/// type, a function's (declared after `prev_word`, a call, or a Rust
/// macro) or plain.
fn name(text: &str, i: usize, c: char, s: &Syntax, prev_word: &str) -> (usize, Kind) {
    let mut j = i + c.len_utf8();
    while let Some(d) = text[j..].chars().next().filter(|&d| ident_char(d, s)) {
        j += d.len_utf8();
    }
    let word = &text[i..j];
    let next = text[j..].trim_start_matches([' ', '\t']).chars().next();
    let declared = matches!(prev_word, "fn" | "def" | "func" | "function");
    let kind = if s.keywords.contains(&word) {
        Kind::Keyword
    } else if declared || next == Some('(') || (s.rust_chars && next == Some('!') && !text[j..].starts_with("!=")) {
        Kind::Func
    } else if s.types && word.chars().next().is_some_and(char::is_uppercase) && word.len() > 1 {
        Kind::Type
    } else {
        Kind::Plain
    };
    (j, kind)
}

/// Shell commands (EXECUTE's).
pub fn shell() -> Syntax {
    for_path("x.sh").unwrap()
}

/// Files that are prose or data, never guessed to be code.
fn prose(path: &str) -> bool {
    matches!(name_ext(path).1.as_deref().unwrap_or(""), "md" | "markdown" | "txt" | "rst" | "log" | "csv" | "tsv" | "adoc" | "org" | "tex")
}

/// The language of a file: from its name, or for a name that doesn't
/// tell (and isn't prose), from its text when that is clearly code.
pub fn for_file(path: &str, text: &str) -> Option<Syntax> {
    for_path(path).or_else(|| if prose(path) { None } else { detect(text) })
}

/// The language of `text` when it is clearly code: a shebang, a JSON
/// document, markup, or several lines that only code in one language has.
/// None for prose, logs and anything unsure.
pub fn detect(text: &str) -> Option<Syntax> {
    let t = text.trim_start();
    if let Some(first) = t.lines().next().filter(|l| l.starts_with("#!")) {
        let lang = ["python", "node", "deno", "sh"].into_iter().find(|l| first.contains(l))?;
        return for_path(match lang {
            "python" => "x.py",
            "node" | "deno" => "x.js",
            _ => "x.sh",
        });
    }
    if (t.starts_with('{') || t.starts_with('[')) && t.len() < 2_000_000 && serde_json::from_str::<serde_json::Value>(t).is_ok() && t.contains('"') {
        return for_path("x.json");
    }
    if t.starts_with("<?xml") || t.starts_with("<!DOCTYPE") || t.starts_with("<!doctype") || t.starts_with("<html") || t.starts_with("<svg") {
        return for_path("x.html");
    }
    // Lines that read as one language's code, by how they start.
    let langs: [(&str, &[&str]); 7] = [
        ("x.rs", &["fn ", "pub fn ", "pub struct ", "impl ", "use ", "let ", "let mut ", "mod ", "#[derive", "pub(crate) ", "match ", "}"]),
        ("x.py", &["def ", "import ", "from ", "class ", "elif ", "return ", "self.", "if __name__", "@"]),
        ("x.js", &["function ", "const ", "let ", "export ", "import ", "return ", "async function", "module.exports", "});", "}"]),
        ("x.c", &["#include", "#define", "int ", "void ", "static ", "struct ", "return ", "}", "typedef "]),
        ("x.go", &["package ", "func ", "import ", "type ", "var ", "return ", "}", "if err != nil"]),
        ("x.java", &["public ", "private ", "class ", "import ", "package ", "return ", "}", "@Override"]),
        ("x.sh", &["if [", "then", "fi", "for ", "do", "done", "export ", "echo ", "set -e", "local "]),
    ];
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).take(400).collect();
    if lines.len() < 3 {
        return None;
    }
    // Code has its share of these; prose and logs mostly don't.
    let punct = lines.iter().filter(|l| l.ends_with([';', '{', '}', ')', ':', ',', ']']) || l.starts_with(['}', ')', ']'])).count();
    if punct * 3 < lines.len() {
        return None;
    }
    // Distinctive starts score; a shared one (`}`, `return `) only helps.
    let score = |starts: &[&str]| lines.iter().filter(|l| starts.iter().any(|s| l.starts_with(s))).count();
    let (best, n) = langs.iter().map(|(p, s)| (*p, score(s))).max_by_key(|x| x.1)?;
    let any = |f: &dyn Fn(&str) -> bool| lines.iter().any(|l| f(l));
    let starts = |starts: &[&str]| any(&|l| starts.iter().any(|s| l.starts_with(s)));
    let distinct = |p: &str| match p {
        "x.rs" => starts(&["fn ", "pub fn ", "impl ", "let mut ", "#[derive", "use "]),
        "x.py" => starts(&["def ", "elif ", "self.", "if __name__"]) || any(&|l| l.starts_with("import ") && !l.contains(';')),
        "x.js" => starts(&["function ", "const ", "export ", "module.exports"]) || any(&|l| l.contains("=>")),
        "x.c" => starts(&["#include", "#define", "typedef "]),
        "x.go" => starts(&["package ", "func "]),
        "x.java" => starts(&["public ", "private "]),
        "x.sh" => any(&|l| matches!(l, "fi" | "done" | "esac")),
        _ => false,
    };
    (n >= 3 && n * 5 >= lines.len() && distinct(best)).then(|| for_path(best)).flatten()
}

/// Appends `text`, a line at a time, colored by kind; `line_start` adds
/// what goes before each line (its newline included) and is told its index.
fn append_code(job: &mut LayoutJob, text: &str, syntax: Option<&Syntax>, font: &FontId, pal: &Palette, mut line_start: impl FnMut(&mut LayoutJob, usize)) {
    let runs = syntax.map_or_else(|| vec![(0..text.len(), Kind::Plain)], |s| spans(text, s));
    let mut runs = runs.into_iter().peekable();
    let mut start = 0;
    for (i, line) in text.split('\n').enumerate() {
        line_start(job, i);
        let end = start + line.len();
        // The runs overlapping this line, cut to it.
        while let Some((r, k)) = runs.peek().cloned() {
            let (a, b) = (r.start.max(start), r.end.min(end));
            if a < b {
                job.append(&text[a..b], 0.0, fmt(font, pal.color(k)));
            }
            if r.end <= end + 1 {
                runs.next();
            } else {
                break;
            }
        }
        start = end + 1;
    }
}

fn fmt(font: &FontId, color: Color32) -> TextFormat {
    TextFormat { font_id: font.clone(), color, ..Default::default() }
}

/// Code with a line number before each line (READ's output): `gutter`
/// holds the numbers, `text` the code, a line for each.
pub fn numbered_job(gutter: &[&str], text: &str, syntax: Option<&Syntax>, font: &FontId, pal: &Palette, gutter_color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let width = gutter.iter().map(|g| g.len()).max().unwrap_or(0);
    append_code(&mut job, text, syntax, font, pal, |job, i| {
        if i > 0 {
            job.append("\n", 0.0, fmt(font, pal.plain));
        }
        let g = gutter.get(i).copied().unwrap_or("");
        job.append(&format!("{g:>width$}  "), 0.0, fmt(font, gutter_color));
    });
    job
}

/// The colors code is drawn in.
pub struct Palette {
    pub plain: Color32,
    pub keyword: Color32,
    pub string: Color32,
    pub number: Color32,
    pub comment: Color32,
    pub typ: Color32,
    pub func: Color32,
    /// The "-" and "+" of a diff.
    pub del: Color32,
    pub add: Color32,
}

impl Palette {
    /// Oak's colors for code: the same for tool rows and the replies'
    /// code blocks.
    pub fn for_theme(t: &Theme) -> Palette {
        Palette {
            plain: t.ink,
            keyword: crate::theme::hex(0xc792ea),
            string: crate::theme::hex(0xa5c97a),
            number: crate::theme::hex(0xe0a46b),
            comment: t.ink3,
            typ: crate::theme::hex(0xe8c879),
            func: crate::theme::hex(0x7fb4f0),
            del: crate::theme::hex(0xe5534b),
            add: crate::theme::hex(0x57ab5a),
        }
    }

    fn color(&self, k: Kind) -> Color32 {
        match k {
            Kind::Plain => self.plain,
            Kind::Keyword => self.keyword,
            Kind::Str => self.string,
            Kind::Number => self.number,
            Kind::Comment => self.comment,
            Kind::Type => self.typ,
            Kind::Func => self.func,
        }
    }
}

/// A part of a code view: plain code (`None`), or removed (`Some(false)`)
/// or added (`Some(true)`) lines of a diff.
pub type Part<'a> = (Option<bool>, &'a str);

/// The parts as one text: each part's lines, diff lines after a "- " or
/// "+ ", colored by kind. Returns the job and each line's part kind, for
/// the line backgrounds.
pub fn job(parts: &[Part], syntax: Option<&Syntax>, font: &FontId, pal: &Palette) -> (LayoutJob, Vec<Option<bool>>) {
    let mut job = LayoutJob::default();
    let mut kinds = vec![];
    for (sign, text) in parts {
        // Each part highlighted as a whole, so strings and comments that
        // span lines keep their color.
        let text = text.strip_suffix('\n').unwrap_or(text);
        append_code(&mut job, text, syntax, font, pal, |job, _| {
            if !kinds.is_empty() {
                job.append("\n", 0.0, fmt(font, pal.plain));
            }
            kinds.push(*sign);
            match sign {
                Some(false) => job.append("- ", 0.0, fmt(font, pal.del)),
                Some(true) => job.append("+ ", 0.0, fmt(font, pal.add)),
                None => {}
            }
        });
    }
    (job, kinds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pal() -> Palette {
        Palette {
            plain: Color32::WHITE,
            keyword: Color32::RED,
            string: Color32::GREEN,
            number: Color32::BLUE,
            comment: Color32::GRAY,
            typ: Color32::YELLOW,
            func: Color32::BROWN,
            del: Color32::DARK_RED,
            add: Color32::DARK_GREEN,
        }
    }

    fn kinds<'a>(text: &'a str, path: &str) -> Vec<(&'a str, Kind)> {
        let s = for_path(path).unwrap();
        spans(text, &s).into_iter().filter(|(_, k)| *k != Kind::Plain).map(|(r, k)| (&text[r], k)).collect()
    }

    #[test]
    fn rust() {
        let k = kinds("pub fn main<'a>() { let s = \"hi // no\"; // done\n println!(\"{}\", 42); Foo::new('x') }", "src/main.rs");
        assert_eq!(
            k,
            vec![
                ("pub", Kind::Keyword),
                ("fn", Kind::Keyword),
                ("main", Kind::Func),
                ("let", Kind::Keyword),
                ("\"hi // no\"", Kind::Str),
                ("// done", Kind::Comment),
                ("println", Kind::Func),
                ("\"{}\"", Kind::Str),
                ("42", Kind::Number),
                ("Foo", Kind::Type),
                ("new", Kind::Func),
                ("'x'", Kind::Str),
            ]
        );
    }

    #[test]
    fn python_and_shell() {
        let k = kinds("def f(x):\n    \"\"\"doc\nmore\"\"\"\n    return x # note", "a.py");
        assert_eq!(k, vec![("def", Kind::Keyword), ("f", Kind::Func), ("\"\"\"doc\nmore\"\"\"", Kind::Str), ("return", Kind::Keyword), ("# note", Kind::Comment)]);
        let k = kinds("echo \"$HOME\" # hi\nls a#b", "x.sh");
        assert_eq!(k, vec![("echo", Kind::Keyword), ("\"$HOME\"", Kind::Str), ("# hi", Kind::Comment)]);
        assert!(for_path("README.md").is_none());
    }

    #[test]
    fn detects_only_clear_code() {
        let rust = "use std::io;\n\nfn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";
        assert!(detect(rust).is_some_and(|s| s.rust_chars));
        assert!(detect("#!/usr/bin/env python3\nprint('hi')\n").is_some_and(|s| s.triple_quotes));
        assert!(detect("{\"a\": [1, 2]}").is_some());
        let prose = "The build finished.\nAll tests passed, with 3 ignored.\nNothing else to do here.\nSee the log for details.";
        assert!(detect(prose).is_none());
        let log = "   Compiling oak v0.1.0\n    Finished `dev` profile in 15s\n     Running target/debug/oak\nerror: something";
        assert!(detect(log).is_none());
        assert!(for_file("notes.md", rust).is_none(), "prose files aren't guessed");
        assert!(for_file("Makefile.custom", rust).is_some());
    }

    #[test]
    fn numbered() {
        let pal = pal();
        let s = for_path("a.rs").unwrap();
        let job = numbered_job(&["9", "10"], "let a = 1;\nfn f() {}", Some(&s), &FontId::monospace(12.0), &pal, Color32::DARK_GRAY);
        assert_eq!(job.text, " 9  let a = 1;\n10  fn f() {}");
        let kw = job.sections.iter().find(|sec| &job.text[sec.byte_range.start.0..sec.byte_range.end.0] == "fn").unwrap();
        assert_eq!(kw.format.color, Color32::RED);
    }

    #[test]
    fn fences_name_their_language() {
        assert!(for_fence("rust").is_some_and(|s| s.rust_chars));
        assert!(for_fence("Rust").is_some_and(|s| s.rust_chars), "case doesn't matter");
        assert!(for_fence("python").is_some_and(|s| s.triple_quotes));
        assert!(for_fence("bash").is_some_and(|s| s.dollar), "sh's $ names");
        assert!(for_fence("c++").is_some());
        assert!(for_fence("csharp").is_some());
        // Prose, unnamed blocks and languages Oak doesn't know stay plain.
        assert!(for_fence("").is_none());
        assert!(for_fence("text").is_none());
        assert!(for_fence("diff").is_none());
        assert!(for_fence("klingon").is_none());
    }

    #[test]
    fn diff_job() {
        let pal = pal();
        let s = for_path("a.rs").unwrap();
        let (job, kinds) = job(&[(Some(false), "let a = 1;\n/* x\ny */"), (Some(true), "let a = 2;\n")], Some(&s), &FontId::monospace(12.0), &pal);
        assert_eq!(job.text, "- let a = 1;\n- /* x\n- y */\n+ let a = 2;");
        assert_eq!(kinds, vec![Some(false), Some(false), Some(false), Some(true)]);
        // The comment's second line keeps the comment color.
        let y = job.sections.iter().find(|sec| &job.text[sec.byte_range.start.0..sec.byte_range.end.0] == "y */").unwrap();
        assert_eq!(y.format.color, Color32::GRAY);
    }
}
