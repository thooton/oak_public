// Long sleeps in EXECUTE commands. A model waiting for something often
// reaches for `sleep 60; cat log`, which ties the session up for the whole
// minute however soon the thing is ready. EXECUTE refuses such a command
// before it starts and says what to do instead: poll with an until-loop, or
// run the command as a task and wait on it with TASK_WAIT. The check reads
// the command the way bash would: quotes, escapes, comments, `$(...)`,
// backticks, redirections and heredoc bodies are not taken for commands. A
// sleep is refused when it is longer than LIMIT_SECS on its own (anywhere,
// a loop's body too), or when the sleeps outside loops add up to more
// (chained short sleeps). Short sleeps in a loop (`until <check>; do
// sleep 2; done`) are the way to wait, and pass.


/// The longest a command may sleep, in seconds.
pub const LIMIT_SECS: f64 = 5.0;

/// What a command runs, as far as the check needs: one simple command per
/// item, in order.
pub(crate) struct Simple {
    /// The words, quotes removed, after any leading keywords.
    pub(crate) words: Vec<String>,
    /// The same words as written, for the message.
    pub(crate) text: String,
    /// Inside a loop's body (`do ... done`).
    in_loop: bool,
}

/// A sleep found in the command.
struct Sleep {
    /// Index into the simple commands.
    at: usize,
    secs: f64,
}

/// Err with the message for the model when the command sleeps too long.
pub fn check(command: &str) -> Result<(), String> {
    let cmds = simple_commands(command);
    let sleeps: Vec<Sleep> = cmds.iter().enumerate().filter_map(|(at, c)| sleep_secs(&c.words).map(|secs| Sleep { at, secs })).collect();
    let (what, last) = if let Some(s) = sleeps.iter().find(|s| s.secs > LIMIT_SECS) {
        (cmds[s.at].text.clone(), s.at)
    } else {
        let outside: Vec<&Sleep> = sleeps.iter().filter(|s| !cmds[s.at].in_loop).collect();
        let total: f64 = outside.iter().map(|s| s.secs).sum();
        if total <= LIMIT_SECS {
            return Ok(());
        }
        let list: Vec<&str> = outside.iter().map(|s| cmds[s.at].text.as_str()).collect();
        (format!("{} ({} in all)", list.join(", "), fmt_secs(total)), outside.last().unwrap().at)
    };
    let after: Vec<&str> = cmds[last + 1..].iter().filter(|c| !c.text.is_empty()).map(|c| c.text.as_str()).collect();
    let after = crate::tools::ellipsize(&after.join(" "), 300);
    let followed = if after.is_empty() { String::new() } else { format!(" followed by: {after}") };
    Err(format!(
        "Blocked: {what}{followed}. To wait for a condition, use an until-loop (e.g. `until <check>; do sleep 2; done`). \
         To wait for a command you started, run it with TASK action=new (or EXECUTE with timeout 0) and use TASK action=wait. \
         Do not chain shorter sleeps to work around this block."
    ))
}

fn fmt_secs(s: f64) -> String {
    if s.is_infinite() {
        "forever".into()
    } else if s.fract() == 0.0 {
        format!("{s}s")
    } else {
        format!("{s:.1}s")
    }
}

/// How long a simple command sleeps, if it is a `sleep` with arguments
/// that say (`sleep $N` doesn't).
fn sleep_secs(words: &[String]) -> Option<f64> {
    let (name, args) = words.split_first()?;
    if name.rsplit('/').next() != Some("sleep") || args.is_empty() {
        return None;
    }
    args.iter().map(|a| duration(a)).sum()
}

/// One argument of `sleep`: a number with an optional s, m, h or d, or
/// `infinity`.
fn duration(a: &str) -> Option<f64> {
    if a == "infinity" || a == "inf" {
        return Some(f64::INFINITY);
    }
    let mult = match a.chars().last()? {
        's' => 1.0,
        'm' => 60.0,
        'h' => 3600.0,
        'd' => 86400.0,
        _ => 0.0,
    };
    let (num, mult) = if mult > 0.0 { (&a[..a.len() - 1], mult) } else { (a, 1.0) };
    if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    num.parse::<f64>().ok().map(|n| n * mult)
}

enum Tok {
    /// A word: as written, and with quotes removed.
    Word(String, String),
    /// Anything that ends a simple command: `;`, `&`, `|`, `(`, `)`, a newline.
    Sep,
}

/// The command's simple commands, with leading keywords (`do`, `then`,
/// `if`, `{`, ...) and variable assignments taken off, each marked when it
/// is in a loop's body.
pub(crate) fn simple_commands(command: &str) -> Vec<Simple> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut words: Vec<(String, String)> = Vec::new();
    let mut flush = |words: &mut Vec<(String, String)>, depth: &mut usize| {
        let mut w = std::mem::take(words).into_iter().peekable();
        while let Some((_, v)) = w.peek() {
            match v.as_str() {
                "do" => *depth += 1,
                "done" => *depth = depth.saturating_sub(1),
                "then" | "else" | "elif" | "if" | "while" | "until" | "!" | "{" | "}" | "time" | "fi" | "esac" => {}
                v if is_assignment(v) => {}
                _ => break,
            }
            w.next();
        }
        let rest: Vec<(String, String)> = w.collect();
        if !rest.is_empty() {
            out.push(Simple {
                text: rest.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>().join(" "),
                words: rest.into_iter().map(|(_, v)| v).collect(),
                in_loop: *depth > 0,
            });
        }
    };
    for t in tokenize(command) {
        match t {
            Tok::Word(r, v) => words.push((r, v)),
            Tok::Sep => flush(&mut words, &mut depth),
        }
    }
    flush(&mut words, &mut depth);
    out
}

fn is_assignment(w: &str) -> bool {
    let Some((name, _)) = w.split_once('=') else { return false };
    let mut c = name.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_') && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
}

fn tokenize(s: &str) -> Vec<Tok> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\n' => {
                out.push(Tok::Sep);
                i += 1;
                // The bodies of the heredocs this line started.
                for (delim, tabs) in heredocs.drain(..) {
                    while i < c.len() {
                        let start = i;
                        while i < c.len() && c[i] != '\n' {
                            i += 1;
                        }
                        let line: String = c[start..i].iter().collect();
                        i += 1;
                        let line = if tabs { line.trim_start_matches('\t') } else { line.as_str() };
                        if line.trim_end_matches('\r') == delim {
                            break;
                        }
                    }
                }
            }
            ' ' | '\t' | '\r' => i += 1,
            '#' => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '&' if c.get(i + 1) == Some(&'>') => redirect(&c, &mut i, &mut heredocs),
            '<' | '>' => redirect(&c, &mut i, &mut heredocs),
            ';' | '&' | '|' | '(' | ')' => {
                out.push(Tok::Sep);
                i += 1;
            }
            _ => {
                let (raw, val) = word(&c, &mut i);
                // A file descriptor before a redirection (`2>&1`).
                let fd = matches!(c.get(i), Some('<' | '>')) && !raw.is_empty() && raw.chars().all(|x| x.is_ascii_digit());
                if !fd && !raw.is_empty() {
                    out.push(Tok::Word(raw, val));
                }
            }
        }
    }
    out
}

/// Skips a redirection and its target; a heredoc's delimiter is kept for
/// skipping its body at the end of the line.
fn redirect(c: &[char], i: &mut usize, heredocs: &mut Vec<(String, bool)>) {
    let heredoc = c[*i] == '<' && c.get(*i + 1) == Some(&'<') && c.get(*i + 2) != Some(&'<');
    if heredoc {
        *i += 2;
    }
    let tabs = heredoc && c.get(*i) == Some(&'-');
    while *i < c.len() && matches!(c[*i], '<' | '>' | '&' | '|' | '-') {
        *i += 1;
    }
    while *i < c.len() && matches!(c[*i], ' ' | '\t') {
        *i += 1;
    }
    let (_, target) = word(c, i);
    if heredoc && !target.is_empty() {
        heredocs.push((target, tabs));
    }
}

/// One word from `c[*i]`: as written, and with quotes and escapes removed.
fn word(c: &[char], i: &mut usize) -> (String, String) {
    let start = *i;
    let mut val = String::new();
    while *i < c.len() {
        match c[*i] {
            ' ' | '\t' | '\r' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => break,
            '\\' => {
                if let Some(&n) = c.get(*i + 1)
                    && n != '\n'
                {
                    val.push(n);
                }
                *i += 2;
            }
            '\'' => {
                *i += 1;
                while *i < c.len() && c[*i] != '\'' {
                    val.push(c[*i]);
                    *i += 1;
                }
                *i += 1;
            }
            '"' => {
                *i += 1;
                while *i < c.len() && c[*i] != '"' {
                    if c[*i] == '\\' && *i + 1 < c.len() {
                        *i += 1;
                    }
                    val.push(c[*i]);
                    *i += 1;
                }
                *i += 1;
            }
            '$' if c.get(*i + 1) == Some(&'(') => {
                // A command substitution is part of the word, not a command.
                let mut depth = 0;
                while *i < c.len() {
                    match c[*i] {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    val.push(c[*i]);
                    *i += 1;
                    if depth == 0 && c[*i - 1] == ')' {
                        break;
                    }
                }
            }
            '`' => {
                val.push('`');
                *i += 1;
                while *i < c.len() && c[*i] != '`' {
                    val.push(c[*i]);
                    *i += 1;
                }
                if *i < c.len() {
                    val.push('`');
                }
                *i += 1;
            }
            ch => {
                val.push(ch);
                *i += 1;
            }
        }
    }
    *i = (*i).min(c.len());
    (c[start..*i].iter().collect(), val)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(cmd: &str) -> bool {
        check(cmd).is_err()
    }

    #[test]
    fn long_sleeps_are_blocked() {
        let e = check("sleep 60 && cat /tmp/x/baseline.txt\ncat /tmp/x/by4q0v0p4.output").unwrap_err();
        assert_eq!(
            e,
            "Blocked: sleep 60 followed by: cat /tmp/x/baseline.txt cat /tmp/x/by4q0v0p4.output. To wait for a condition, use an until-loop \
             (e.g. `until <check>; do sleep 2; done`). To wait for a command you started, run it with TASK action=new (or EXECUTE with timeout 0) \
             and use TASK action=wait. Do not chain shorter sleeps to work around this block."
        );
        assert!(check("sleep 30").unwrap_err().starts_with("Blocked: sleep 30. To wait"));
        for cmd in [
            "sleep 6",
            "sleep 1m; ls",
            "sleep 0.1h",
            "/usr/bin/sleep 20",
            "make & sleep 30; tail log",
            "cd x && sleep 10 && ls",
            "(sleep 60; cat out)",
            "if true; then sleep 9; fi",
            "sleep infinity",
            "X=1 sleep 30",
            "sleep 60 >/dev/null 2>&1; ls",
            "until [ -f done ]; do sleep 30; done",
            "while sleep 60; do date; done",
        ] {
            assert!(blocked(cmd), "{cmd}");
        }
    }
}
