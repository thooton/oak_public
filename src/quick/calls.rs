//! Oak's own tools through Quick's device Python. Quick's agent runs code on
//! this machine with `run_python_outpost_local` (`run_python_on_device` to it);
//! here that tool takes Oak tool calls instead of Python: one or more calls
//! written as Python calls, a line each, with literal arguments only
//! (`READ(path="src/main.rs")`). They are read here (`snippet`), the
//! conversation's own agent runs them as it runs any model's calls, all at
//! once (`turn::run_oak_alive`), and Quick gets each one's result, in order
//! (`answer`). Nothing else of Python runs: no names, expressions, imports or
//! statements, so a snippet is exactly the calls it shows.

use serde_json::{Map, Number, Value};

/// One Oak call of a snippet: the tool as Oak runs it, its input, and the
/// snippet's text of it (what Quick's answer names it by).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OakCall {
    pub name: String,
    pub input: Value,
    pub source: String,
}

/// What a snippet that isn't Oak calls is told, after what was wrong.
pub(crate) const HOW: &str = "This device's Python tool runs Oak tool calls only, not Python: one or more lines like READ(path=\"src/main.rs\") or EXECUTE(command=\"cargo test\", summary=\"Run the tests\"), with literal arguments (strings, numbers, True/False/None, lists and dicts), all run at the same time. Shell commands go in EXECUTE.";

/// The tools Quick can't call through a snippet: ASK waits for the reply's
/// end, which a turn waiting on its result never reaches.
const NOT_HERE: [&str; 2] = ["ASK", "REQUEST_ASK"];

/// Whether `name` is a tool Quick may be offered and call here.
pub(crate) fn offered(name: &str) -> bool {
    !NOT_HERE.contains(&name)
}

/// A snippet's calls, each named as Oak runs it (matched to `tools`, Oak's
/// definitions, in any case), its positional arguments given the names of
/// the tool's required fields in order. Fails, with what was wrong and how a
/// snippet looks, before anything runs.
pub(crate) fn snippet(code: &str, tools: &[Value]) -> Result<Vec<OakCall>, String> {
    let raw = parse(code).map_err(|e| format!("{e}\n\n{HOW}"))?;
    if raw.is_empty() {
        return Err(format!("The snippet makes no calls.\n\n{HOW}"));
    }
    raw.into_iter().map(|c| resolve(c, tools)).collect()
}

/// A call as written: its name, positional and keyword arguments, and text.
#[derive(Debug)]
struct Written {
    name: String,
    positional: Vec<Value>,
    keywords: Vec<(String, Value)>,
    source: String,
}

fn resolve(c: Written, tools: &[Value]) -> Result<OakCall, String> {
    let def = tools.iter().find(|d| d["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(&c.name)));
    let name = match def {
        Some(d) => d["name"].as_str().unwrap_or(&c.name).to_string(),
        None => match crate::tools::tool_names().into_iter().find(|n| n.eq_ignore_ascii_case(&c.name)) {
            Some(n) => n,
            // A kit's or a protocol's tool loaded since (Oak names them in
            // capitals, MCP's tools `mcp__…`): Oak's agent says if it isn't.
            None if c.name.starts_with("mcp__") || c.name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') => c.name.clone(),
            None => {
                let names: Vec<&str> = tools.iter().filter_map(|d| d["name"].as_str()).filter(|n| offered(n)).collect();
                return Err(format!("`{}` isn't one of Oak's tools. They are: {}.\n\n{HOW}", c.name, names.join(", ")));
            }
        },
    };
    if !offered(&name) {
        return Err(format!("{name} isn't available through this tool: ask the user in your reply instead."));
    }
    let mut input = Map::new();
    if !c.positional.is_empty() {
        let order: Vec<String> = def
            .and_then(|d| d["input_schema"]["required"].as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if c.positional.len() > order.len() {
            let takes = if order.is_empty() { "no positional arguments".to_string() } else { format!("at most {} positional ({})", order.len(), order.join(", ")) };
            return Err(format!("{name} takes {takes}: give the others by name, as in {name}(path=\"...\")."));
        }
        for (k, v) in order.into_iter().zip(c.positional) {
            input.insert(k, v);
        }
    }
    for (k, v) in c.keywords {
        if input.contains_key(&k) {
            return Err(format!("{name} got `{k}` twice."));
        }
        input.insert(k, v);
    }
    Ok(OakCall { name, input: Value::Object(input), source: c.source })
}

/// What a call's `tool_result` block says, as text for Quick (an image it
/// can't be handed is named instead); Err when the call failed.
pub(crate) fn result_text(r: &Value) -> Result<String, String> {
    let text = match &r["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().unwrap_or("").to_string(),
                Some("image") => "[An image: Oak shows it to the user, but it can't be passed to you through this tool.]".to_string(),
                _ => b.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    if r["is_error"].as_bool() == Some(true) { Err(text) } else { Ok(text) }
}

/// Quick's answer for a snippet: a lone call's result as it is; several
/// calls' each under the call's own line, in order (a failed one marked so).
pub(crate) fn answer(calls: &[OakCall], results: &[Result<String, String>]) -> Result<String, String> {
    if let ([_], [only]) = (calls, results) {
        return only.clone();
    }
    let n = calls.len();
    let mut out = String::new();
    for (k, (c, r)) in calls.iter().zip(results).enumerate() {
        if k > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&format!("[{}/{n}] {}\n", k + 1, headline(&c.source)));
        match r {
            Ok(s) => out.push_str(s),
            Err(e) => {
                out.push_str("Error: ");
                out.push_str(e);
            }
        }
    }
    Ok(out)
}

/// A call's text on one line, short: what an answer names it by.
pub(crate) fn headline(source: &str) -> String {
    let line = source.lines().next().unwrap_or("").trim_end();
    let more = source.lines().nth(1).is_some();
    let mut s: String = line.chars().take(160).collect();
    if more || line.chars().count() > 160 {
        s.push_str(" …");
    }
    s
}

// ---------------------------------------------------------------- reading

/// A snippet's calls as written, or where and why it isn't calls.
fn parse(code: &str) -> Result<Vec<Written>, String> {
    let code = code.replace("\r\n", "\n");
    let mut p = Parser { src: &code, chars: code.char_indices().collect(), i: 0 };
    let mut out = vec![];
    loop {
        p.skip(true);
        while p.peek() == Some(';') {
            p.bump();
            p.skip(true);
        }
        if p.peek().is_none() {
            return Ok(out);
        }
        let start = p.pos();
        let Some(name) = p.ident() else {
            return Err(p.err("Expected an Oak tool call, like READ(path=\"...\")"));
        };
        p.skip(false);
        if p.peek() != Some('(') {
            return Err(p.err(&format!("Expected `(` after `{name}`: only calls of Oak's tools run here")));
        }
        p.bump();
        let (positional, keywords) = p.args()?;
        let source = code[start..p.pos()].to_string();
        p.skip(false);
        match p.peek() {
            None | Some('\n') | Some(';') => {}
            Some(_) => return Err(p.err("Expected the end of the line after the call (one call per line)")),
        }
        out.push(Written { name, positional, keywords, source });
    }
}

struct Parser<'a> {
    src: &'a str,
    chars: Vec<(usize, char)>,
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).map(|c| c.1)
    }

    fn peek_at(&self, k: usize) -> Option<char> {
        self.chars.get(self.i + k).map(|c| c.1)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.i += 1;
        }
        c
    }

    /// The byte offset of the next character.
    fn pos(&self) -> usize {
        self.chars.get(self.i).map_or(self.src.len(), |c| c.0)
    }

    /// `msg`, with the line and column it is about.
    fn err(&self, msg: &str) -> String {
        let at = self.pos();
        let before = &self.src[..at];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        format!("Line {line}, column {col}: {msg}.")
    }

    /// Skips spaces, comments and line continuations, and new lines too
    /// when `lines` (inside brackets, and between calls).
    fn skip(&mut self, lines: bool) {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\x0c' | '\r') => {
                    self.bump();
                }
                Some('\n') if lines => {
                    self.bump();
                }
                Some('\\') if self.peek_at(1) == Some('\n') => {
                    self.i += 2;
                }
                Some('#') => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.bump();
                    }
                }
                _ => return,
            }
        }
    }

    fn ident(&mut self) -> Option<String> {
        let c = self.peek()?;
        if !(c.is_ascii_alphabetic() || c == '_') {
            return None;
        }
        let mut s = String::new();
        while let Some(c) = self.peek().filter(|c| c.is_ascii_alphanumeric() || *c == '_') {
            s.push(c);
            self.bump();
        }
        Some(s)
    }

    /// A call's arguments, after its `(`, through its `)`.
    #[allow(clippy::type_complexity)]
    fn args(&mut self) -> Result<(Vec<Value>, Vec<(String, Value)>), String> {
        let (mut positional, mut keywords) = (vec![], vec![]);
        loop {
            self.skip(true);
            match self.peek() {
                Some(')') => {
                    self.bump();
                    return Ok((positional, keywords));
                }
                None => return Err(self.err("The call isn't closed: `)` is missing")),
                _ => {}
            }
            let save = self.i;
            let mut keyword = None;
            if !self.string_ahead()
                && let Some(id) = self.ident()
            {
                self.skip(true);
                if self.peek() == Some('=') && self.peek_at(1) != Some('=') {
                    self.bump();
                    keyword = Some(id);
                } else {
                    self.i = save;
                }
            }
            let value = self.value()?;
            match keyword {
                Some(k) => {
                    if keywords.iter().any(|(n, _)| *n == k) {
                        return Err(self.err(&format!("`{k}` is given twice")));
                    }
                    keywords.push((k, value));
                }
                None if !keywords.is_empty() => return Err(self.err("A positional argument can't follow a keyword argument")),
                None => positional.push(value),
            }
            self.skip(true);
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(')') => {}
                None => return Err(self.err("The call isn't closed: `)` is missing")),
                Some(_) => return Err(self.err("Expected `,` or `)` after the argument")),
            }
        }
    }

    /// One literal argument.
    fn value(&mut self) -> Result<Value, String> {
        self.skip(true);
        if self.string_ahead() {
            return self.strings().map(Value::String);
        }
        match self.peek() {
            None => Err(self.err("Expected a value")),
            Some(c) if c.is_ascii_digit() || (c == '.' && self.peek_at(1).is_some_and(|d| d.is_ascii_digit())) => self.number(false),
            Some(s @ ('-' | '+')) => {
                self.bump();
                self.skip(false);
                match self.peek() {
                    Some(c) if c.is_ascii_digit() || c == '.' => self.number(s == '-'),
                    _ => Err(self.err("Expected a number after the sign")),
                }
            }
            Some('[') => {
                self.bump();
                Ok(Value::Array(self.items(']')?.0))
            }
            Some('(') => {
                self.bump();
                let (mut items, comma) = self.items(')')?;
                // `(x)` is x; `(x,)` and `(x, y)` are tuples (lists here).
                Ok(if items.len() == 1 && !comma { items.pop().unwrap() } else { Value::Array(items) })
            }
            Some('{') => {
                self.bump();
                self.dict()
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let id = self.ident().unwrap_or_default();
                match id.as_str() {
                    "True" | "true" => Ok(Value::Bool(true)),
                    "False" | "false" => Ok(Value::Bool(false)),
                    "None" | "null" => Ok(Value::Null),
                    _ if matches!(self.peek(), Some('"' | '\'')) => Err(self.err(&format!("`{id}` strings aren't supported: write the text out in a plain or raw string"))),
                    _ => Err(self.err(&format!("`{id}` isn't a literal: arguments take only strings, numbers, True/False/None, lists and dicts"))),
                }
            }
            Some(c) => Err(self.err(&format!("Unexpected `{c}`: arguments take only strings, numbers, True/False/None, lists and dicts"))),
        }
    }

    /// Values up to `close`, comma-separated; and whether a comma followed
    /// the last.
    fn items(&mut self, close: char) -> Result<(Vec<Value>, bool), String> {
        let mut items = vec![];
        let mut comma = false;
        loop {
            self.skip(true);
            if self.peek() == Some(close) {
                self.bump();
                return Ok((items, comma));
            }
            items.push(self.value()?);
            comma = false;
            self.skip(true);
            match self.peek() {
                Some(',') => {
                    self.bump();
                    comma = true;
                }
                Some(c) if c == close => {}
                None => return Err(self.err(&format!("`{close}` is missing"))),
                Some(_) => return Err(self.err(&format!("Expected `,` or `{close}`"))),
            }
        }
    }

    /// A dict's pairs after its `{`, through its `}`: string keys only (it
    /// is a JSON object).
    fn dict(&mut self) -> Result<Value, String> {
        let mut out = Map::new();
        loop {
            self.skip(true);
            if self.peek() == Some('}') {
                self.bump();
                return Ok(Value::Object(out));
            }
            if !self.string_ahead() {
                return Err(self.err("A dict's keys must be strings"));
            }
            let key = self.strings()?;
            self.skip(true);
            if self.peek() != Some(':') {
                return Err(self.err("Expected `:` after the dict's key"));
            }
            self.bump();
            let v = self.value()?;
            out.insert(key, v);
            self.skip(true);
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some('}') => {}
                None => return Err(self.err("`}` is missing")),
                Some(_) => return Err(self.err("Expected `,` or `}`")),
            }
        }
    }

    /// The prefix of a string literal starting here (its length, and
    /// whether it is raw), if one does.
    fn string_prefix(&self) -> Option<(usize, bool)> {
        let quote = |k: usize| matches!(self.peek_at(k), Some('"' | '\''));
        match self.peek()? {
            '"' | '\'' => Some((0, false)),
            'r' | 'R' if quote(1) => Some((1, true)),
            'u' | 'U' if quote(1) => Some((1, false)),
            _ => None,
        }
    }

    fn string_ahead(&self) -> bool {
        self.string_prefix().is_some()
    }

    /// One string literal or several side by side (joined, as Python joins
    /// them).
    fn strings(&mut self) -> Result<String, String> {
        let mut out = self.string()?;
        loop {
            let save = self.i;
            self.skip(true);
            if self.string_ahead() {
                out.push_str(&self.string()?);
            } else {
                self.i = save;
                return Ok(out);
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        let (prefix, raw) = self.string_prefix().ok_or_else(|| self.err("Expected a string"))?;
        self.i += prefix;
        let q = self.bump().unwrap_or('"');
        let triple = self.peek() == Some(q) && self.peek_at(1) == Some(q);
        if triple {
            self.i += 2;
        }
        let mut out = String::new();
        loop {
            let Some(c) = self.bump() else {
                return Err(self.err("A string isn't closed"));
            };
            if c == q {
                if !triple {
                    return Ok(out);
                }
                if self.peek() == Some(q) && self.peek_at(1) == Some(q) {
                    self.i += 2;
                    return Ok(out);
                }
                out.push(c);
                continue;
            }
            if c == '\n' && !triple {
                return Err(self.err("A string runs past the end of its line: use a triple-quoted string (\"\"\"...\"\"\") or \\n"));
            }
            if c != '\\' {
                out.push(c);
                continue;
            }
            let Some(e) = self.bump() else {
                return Err(self.err("A string isn't closed"));
            };
            if raw {
                // A raw string keeps its backslashes (and a quote after one
                // doesn't end it).
                out.push('\\');
                out.push(e);
                continue;
            }
            match e {
                '\n' => {}
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '\\' => out.push('\\'),
                '\'' => out.push('\''),
                '"' => out.push('"'),
                'a' => out.push('\x07'),
                'b' => out.push('\x08'),
                'f' => out.push('\x0c'),
                'v' => out.push('\x0b'),
                '0'..='7' => {
                    let mut n = e.to_digit(8).unwrap_or(0);
                    for _ in 0..2 {
                        match self.peek().and_then(|d| d.to_digit(8)) {
                            Some(d) => {
                                n = n * 8 + d;
                                self.bump();
                            }
                            None => break,
                        }
                    }
                    out.push(char::from_u32(n).unwrap_or('\u{fffd}'));
                }
                'x' | 'u' | 'U' => {
                    let len = match e {
                        'x' => 2,
                        'u' => 4,
                        _ => 8,
                    };
                    let mut n = 0u32;
                    for _ in 0..len {
                        match self.bump().and_then(|d| d.to_digit(16)) {
                            Some(d) => n = n * 16 + d,
                            None => return Err(self.err(&format!("`\\{e}` takes {len} hex digits"))),
                        }
                    }
                    match char::from_u32(n) {
                        Some(ch) => out.push(ch),
                        None => return Err(self.err("That escape isn't a character")),
                    }
                }
                'N' => return Err(self.err("`\\N{...}` escapes aren't supported: write the character itself")),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
        }
    }

    /// A number (negative when `neg`): an integer where it is one, else a
    /// float.
    fn number(&mut self, neg: bool) -> Result<Value, String> {
        let mut s = String::new();
        let radix = if self.peek() == Some('0') {
            match self.peek_at(1) {
                Some('x' | 'X') => 16,
                Some('o' | 'O') => 8,
                Some('b' | 'B') => 2,
                _ => 10,
            }
        } else {
            10
        };
        if radix != 10 {
            self.i += 2;
            while let Some(c) = self.peek().filter(|c| c.is_ascii_alphanumeric() || *c == '_') {
                if c != '_' {
                    s.push(c);
                }
                self.bump();
            }
            let n = i64::from_str_radix(&s, radix).map_err(|_| self.err("That number doesn't read"))?;
            return Ok(Value::Number(Number::from(if neg { -n } else { n })));
        }
        let mut float = false;
        while let Some(c) = self.peek() {
            match c {
                '0'..='9' => s.push(c),
                '_' => {}
                '.' if !float => {
                    float = true;
                    s.push(c);
                }
                'e' | 'E' if !s.contains('e') => {
                    float = true;
                    s.push('e');
                    if let Some(sign @ ('+' | '-')) = self.peek_at(1) {
                        self.bump();
                        s.push(sign);
                    }
                }
                _ => break,
            }
            self.bump();
        }
        if self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            return Err(self.err("That number doesn't read"));
        }
        if neg {
            s.insert(0, '-');
        }
        if !float && let Ok(n) = s.parse::<i64>() {
            return Ok(Value::Number(Number::from(n)));
        }
        s.parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| self.err("That number doesn't read"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn defs() -> Vec<Value> {
        vec![
            json!({"name": "READ", "description": "", "input_schema": {"type": "object", "properties": {"path": {}, "offset": {}}, "required": ["path"]}}),
            json!({"name": "WRITE", "description": "", "input_schema": {"type": "object", "properties": {"path": {}, "content": {}}, "required": ["path", "content"]}}),
            json!({"name": "EXECUTE", "description": "", "input_schema": {"type": "object", "properties": {"command": {}}, "required": ["command"]}}),
            json!({"name": "TASK", "description": "", "input_schema": {"type": "object", "properties": {"action": {}}, "required": ["action"]}}),
            json!({"name": "ASK", "description": "", "input_schema": {"type": "object", "properties": {}, "required": []}}),
        ]
    }

    #[test]
    fn calls_read_as_oaks_tools_with_their_inputs() {
        let code = "READ(path='src/a.rs', offset=10)\r\nwrite(\"b.txt\", content=\"\"\"line 1\nit's \"quoted\"\n\"\"\")  # a comment\n\nTASK('wait', id=3, timeout=1.5e2); EXECUTE(command=r'rg \\bfoo\\b -n', env={'A': [1, -2, True, None, 0x1F]})\n";
        let calls = snippet(code, &defs()).unwrap();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0].name, "READ");
        assert_eq!(calls[0].input, json!({"path": "src/a.rs", "offset": 10}));
        assert_eq!(calls[1].name, "WRITE");
        assert_eq!(calls[1].input, json!({"path": "b.txt", "content": "line 1\nit's \"quoted\"\n"}));
        assert_eq!(calls[2].input, json!({"action": "wait", "id": 3, "timeout": 150.0}));
        assert_eq!(calls[3].input, json!({"command": "rg \\bfoo\\b -n", "env": {"A": [1, -2, true, null, 31]}}));
        assert_eq!(calls[0].source, "READ(path='src/a.rs', offset=10)");
    }

    #[test]
    fn strings_read_as_python_reads_them() {
        let one = |s: &str| snippet(&format!("READ(path={s})"), &defs()).unwrap()[0].input["path"].clone();
        assert_eq!(one(r#""a\tb\n\\\x41\u00e9\101""#), json!("a\tb\n\\AéA"));
        assert_eq!(one("'a' \"b\"\n            'c'"), json!("abc"));
        assert_eq!(one(r#"r"C:\new\x""#), json!(r"C:\new\x"));
        assert_eq!(one(r#"r'it\'s'"#), json!(r"it\'s"));
        assert_eq!(one("'''x'y''z'''"), json!("x'y''z"));
        assert_eq!(one("'a\\\nb'"), json!("ab"));
        assert_eq!(one(r#"'\q'"#), json!("\\q"));
        assert_eq!(one("('only')"), json!("only"));
        assert_eq!(one("'naïve ✓'"), json!("naïve ✓"));
        assert_eq!(snippet("READ(path=('a', 'b',))", &defs()).unwrap()[0].input["path"], json!(["a", "b"]));
    }

    #[test]
    fn anything_but_calls_is_refused_before_running() {
        for bad in [
            "import os",
            "x = READ(path='a')",
            "READ(path=open('a'))",
            "READ(path=f'{x}')",
            "READ(path='a') READ(path='b')",
            "READ(path='a'",
            "READ(path='a\nb')",
            "READ(path='a', path='b')",
            "READ(offset=1, 'a')",
            "print('hi')",
            "READ('a', 'b')",
            "EXECUTE(command='ls', 1 + 2)",
            "",
            "# just a comment",
        ] {
            assert!(snippet(bad, &defs()).is_err(), "{bad:?} should be refused");
        }
        let e = snippet("READ(path='a'\n", &defs()).unwrap_err();
        assert!(e.starts_with("Line 2"), "{e}");
        assert!(e.contains(HOW), "{e}");
        assert!(snippet("print('hi')", &defs()).unwrap_err().contains("`print` isn't one of Oak's tools. They are: READ, WRITE, EXECUTE, TASK."));
        assert!(snippet("ASK(questions=[])", &defs()).unwrap_err().contains("ASK isn't available"));
    }

    #[test]
    fn a_tool_loaded_since_passes_through_by_its_name() {
        let calls = snippet("BROWSER(action='open')\nmcp__blender__run(code='x')\nnote(action='new')", &defs()).unwrap();
        assert_eq!(calls[0].name, "BROWSER");
        assert_eq!(calls[1].name, "mcp__blender__run");
        assert_eq!(calls[2].name, "NOTE");
    }

    #[test]
    fn an_answer_has_each_calls_result_in_order() {
        let calls = snippet("READ(path='a')\nEXECUTE(command='false')", &defs()).unwrap();
        let a = answer(&calls, &[Ok("one".into()), Err("exit 1".into())]).unwrap();
        assert_eq!(a, "[1/2] READ(path='a')\none\n\n[2/2] EXECUTE(command='false')\nError: exit 1");
        assert_eq!(answer(&calls[..1], &[Err("no".into())]), Err("no".into()));
        let r = json!({"type": "tool_result", "call_id": "x", "content": [{"type": "text", "text": "t"}, {"type": "image"}], "is_error": false});
        assert!(result_text(&r).unwrap().starts_with("t\n[An image"));
        assert_eq!(result_text(&json!({"content": "bad", "is_error": true})), Err("bad".into()));
    }
}
