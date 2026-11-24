//! What `edit` does to a file's text: find each `old_string` and put its `new_string` there.
//!
//! Models often retype text slightly differently from the file; failing the edit for that costs a
//! step (a new `read`, a new try). So each `old_string` is looked for in stages, a later one only
//! when the ones before it found nothing:
//!
//! 1. **Exactly.**
//! 2. **Normalized**: trailing whitespace on each line is ignored, and typographic quotes, dashes
//!    and unusual spaces count as their plain ASCII forms (a curly quote straightened, a trailing
//!    space dropped).
//! 3. **Indentation-flexible**: whole lines only (`old_string` must cover complete lines, a part of
//!    a line never matches here). Lines are compared as in 2 with their leading whitespace ignored
//!    too, but the indentation must be shifted the same way on every line: the difference between
//!    the file's first matched line and `old_string`'s first line, applied to each line of
//!    `old_string`, gives the file's indentation. `new_string` is re-indented by that same
//!    difference before it goes in (see [`shift`]).
//! 4. **Escape drift**: `old_string` with one level of backslash escaping undone (`\\` → `\`,
//!    `\n` → a line break, `\t` → a tab, `\"` → `"`, `\'` → `'`), or added (each `\` doubled),
//!    matched exactly. `new_string` gets the same treatment when it has escapes too (none: it goes
//!    in as written; real line breaks or tabs next to escaped ones that `old_string` wrote escaped:
//!    refused). When both forms are in the file the edit is refused, since which one was meant
//!    can't be told.
//!
//! The rules stay strict otherwise: a match in any stage must be unique (unless `replace_all`), and
//! the parts of the file the edit doesn't cover keep their bytes (see [`Norm::original`]). A CRLF
//! file stays CRLF. Changes found in stages 2-4 count as fuzzy ([`Applied::fuzzy`]).
//!
//! Several changes can be made in one call: each is matched against the file as it was, they may
//! not overlap, and either all of them are made or none.

/// One change: replace `old` with `new` (every occurrence when `all`).
#[derive(Debug, Clone, Copy)]
pub struct Change<'a> {
    pub old: &'a str,
    pub new: &'a str,
    pub all: bool,
}

/// The file's new text, and what it took.
#[derive(Debug, PartialEq)]
pub struct Applied {
    pub text: String,
    /// Places replaced, over all changes.
    pub replacements: usize,
    /// Changes that were found only after normalizing (see the module docs).
    pub fuzzy: usize,
}

/// A character as normalized matching sees it: typographic quotes, dashes and spaces as their
/// plain forms. One character always stays one character, so a position in the normalized text
/// is the same character count into the original line.
fn plain(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        c => c,
    }
}

/// `s` normalized: [`plain`] characters, and each line without its trailing whitespace (a `\r`
/// included, so line endings don't matter either).
fn normalize(s: &str) -> String {
    s.split('\n').map(|l| l.chars().map(plain).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
}

/// A text normalized, with the way back from a position in it to one in the original.
struct Norm<'a> {
    original: &'a str,
    text: String,
    /// Per line: where it starts in `text`, and in `original`.
    lines: Vec<(usize, usize)>,
}

impl<'a> Norm<'a> {
    fn new(original: &'a str) -> Norm<'a> {
        let mut text = String::with_capacity(original.len());
        let mut lines = Vec::new();
        let mut at = 0;
        for (i, line) in original.split('\n').enumerate() {
            if i > 0 {
                text.push('\n');
            }
            lines.push((text.len(), at));
            text.push_str(line.chars().map(plain).collect::<String>().trim_end());
            at += line.len() + 1;
        }
        Norm { original, text, lines }
    }

    /// The byte offset in the original that position `p` of the normalized text stands for. A
    /// position at the end of a normalized line is the end of the original line's visible text,
    /// before its trailing whitespace.
    fn original(&self, p: usize) -> usize {
        let l = self.lines.partition_point(|(n, _)| *n <= p) - 1;
        let (n_start, o_start) = self.lines[l];
        let chars = self.text[n_start..p].chars().count();
        let line = &self.original[o_start..];
        o_start + line.char_indices().nth(chars).map_or(line.len(), |(i, _)| i)
    }
}

/// The file's text with `changes` made, or what's wrong (for the model). `label(i)` names change
/// `i` in messages (`old_string`, or `edits[2]`).
pub fn apply(text: &str, changes: &[Change], label: impl Fn(usize) -> String) -> Result<Applied, String> {
    // Models write \n; a CRLF file stays CRLF.
    let crlf = text.contains("\r\n");
    let to_file = |s: &str, old: &str| if crlf && !old.contains("\r\n") { s.replace('\n', "\r\n") } else { s.to_string() };
    let mut norm: Option<Norm> = None;
    let mut lines: Option<Vec<Line>> = None;
    // (start, end, replacement, change) in the original text
    let mut spans: Vec<(usize, usize, String, usize)> = Vec::new();
    let mut fuzzy = 0;
    for (i, c) in changes.iter().enumerate() {
        let name = label(i);
        if c.old.is_empty() {
            return Err(format!("{name} is empty. Use `write` to create a file, or include the text you're replacing."));
        }
        if c.old == c.new {
            return Err(format!("{name} and its `new_string` are the same: that changes nothing."));
        }
        let (old, new) = (to_file(c.old, c.old), to_file(c.new, c.old));
        let exact: Vec<(usize, usize, String)> = text.match_indices(&old).map(|(s, m)| (s, s + m.len(), new.clone())).collect();
        let found = if !exact.is_empty() {
            exact
        } else if normalize(c.old).trim().is_empty() {
            // only whitespace isn't a thing to look for
            Vec::new()
        } else {
            let n = norm.get_or_insert_with(|| Norm::new(text));
            let mut hits = normalized(n, c.old, &new);
            if hits.is_empty() {
                hits = reindented(lines.get_or_insert_with(|| Line::all(text)), c.old, &new);
            }
            if hits.is_empty() {
                hits = unescaped(text, c, &name, |s| to_file(s, c.old))?;
            }
            if !hits.is_empty() {
                fuzzy += 1;
            }
            hits
        };
        match (found.len(), c.all) {
            (0, _) => return Err(format!("{name} was not found. Read the file again and copy the text exactly, including whitespace.")),
            (1, _) | (_, true) => {}
            (n, false) => return Err(format!("{name} appears {n} times. Add surrounding lines to make it unique, or set `replace_all`.")),
        }
        spans.extend(found.into_iter().map(|(s, e, new)| (s, e, new, i)));
    }
    spans.sort_by_key(|s| (s.0, s.1));
    for w in spans.windows(2) {
        if w[1].0 < w[0].1 {
            let (a, b) = (w[0].3.min(w[1].3), w[0].3.max(w[1].3));
            return Err(if a == b {
                format!("{} matches overlapping text.", label(a))
            } else {
                format!("{} and {} change overlapping text: merge them into one edit.", label(a), label(b))
            });
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (s, e, new, _) in &spans {
        out.push_str(&text[at..*s]);
        out.push_str(new);
        at = *e;
    }
    out.push_str(&text[at..]);
    Ok(Applied { text: out, replacements: spans.len(), fuzzy })
}

/// Stage 2: `old` found in the normalized text `n`.
fn normalized(n: &Norm, old: &str, new: &str) -> Vec<(usize, usize, String)> {
    let wanted = normalize(old);
    // `old_string` ending in whitespace (often the next line's indentation) matches only up to
    // the end of a line: otherwise its whitespace, gone after normalizing, would let it end in the
    // middle of a word or before indentation it doesn't replace
    let open_end = old.split('\n').next_back().is_some_and(|l| l.ends_with(|c: char| plain(c).is_whitespace()));
    n.text
        .match_indices(&wanted)
        .filter(|(s, m)| !open_end || n.text[s + m.len()..].starts_with('\n') || s + m.len() == n.text.len())
        .map(|(s, m)| (n.original(s), n.original(s + m.len()), new.to_string()))
        .collect()
}

/// A line's leading spaces and tabs.
fn indent(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

/// What a line is in stage 3: [`plain`] characters, without whitespace at either end.
fn bare(line: &str) -> String {
    line.chars().map(plain).collect::<String>().trim().to_string()
}

/// `line` moved by the indentation difference between `from` (`old_string`'s first line) and `to`
/// (the file's): where `to` is `from` and more, the more is added (blank lines stay as they are);
/// where `from` is `to` and more, the more is taken off the start when it's there; otherwise
/// (tabs against spaces) a line that starts with `from` starts with `to` instead.
fn shift(line: &str, from: &str, to: &str) -> String {
    if let Some(more) = to.strip_prefix(from) {
        if line.trim().is_empty() { line.to_string() } else { format!("{more}{line}") }
    } else if let Some(less) = from.strip_prefix(to) {
        line.strip_prefix(less).unwrap_or(line).to_string()
    } else {
        line.strip_prefix(from).map_or_else(|| line.to_string(), |rest| format!("{to}{rest}"))
    }
}

/// A line of the file, for stage 3.
struct Line<'a> {
    /// Byte offsets of its start, and of its end before the `\r\n` or `\n`.
    start: usize,
    end: usize,
    /// Where the next line starts (the end of the text for the last one).
    next: usize,
    indent: &'a str,
    bare: String,
}

impl<'a> Line<'a> {
    fn all(text: &'a str) -> Vec<Line<'a>> {
        let mut at = 0;
        text.split('\n')
            .map(|l| {
                let start = at;
                at = (at + l.len() + 1).min(text.len());
                Line { start, end: start + l.strip_suffix('\r').unwrap_or(l).len(), next: at, indent: indent(l), bare: bare(l) }
            })
            .collect()
    }
}

/// Stage 3: `old` as whole lines of the file whose indentation is shifted the same way on every
/// line, with `new` shifted to match.
fn reindented(file: &[Line], old: &str, new: &str) -> Vec<(usize, usize, String)> {
    let mut want: Vec<&str> = old.split('\n').collect();
    // a final line break: the match takes the last line's line break too
    let through_break = old.ends_with('\n');
    if through_break {
        want.pop();
    }
    let bares: Vec<String> = want.iter().map(|l| bare(l)).collect();
    let Some(first) = bares.iter().position(|b| !b.is_empty()) else { return Vec::new() };
    if want.len() > file.len() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for at in 0..=file.len() - want.len() {
        let here = &file[at..at + want.len()];
        if !here.iter().zip(&bares).all(|(f, b)| f.bare == *b) {
            continue;
        }
        let (from, to) = (indent(want[first]), here[first].indent);
        let same_shift = want.iter().zip(here).zip(&bares).all(|((w, f), b)| b.is_empty() || indent(&shift(w, from, to)) == f.indent);
        if !same_shift {
            continue;
        }
        let last = &here[want.len() - 1];
        let end = if through_break { last.next } else { last.end };
        let new = new.split('\n').map(|l| shift(l, from, to)).collect::<Vec<_>>().join("\n");
        hits.push((here[0].start, end, new));
    }
    hits
}

/// `s` with one level of backslash escaping undone, and which escapes it had (the character after
/// each backslash); `None` when it has none.
fn unescape(s: &str) -> Option<(String, Vec<char>)> {
    let mut out = String::with_capacity(s.len());
    let mut kinds = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let to = match (c, chars.peek()) {
            ('\\', Some(&k @ ('\\' | '"' | '\''))) => Some((k, k)),
            ('\\', Some('n')) => Some(('n', '\n')),
            ('\\', Some('t')) => Some(('t', '\t')),
            _ => None,
        };
        match to {
            Some((k, to)) => {
                chars.next();
                out.push(to);
                if !kinds.contains(&k) {
                    kinds.push(k);
                }
            }
            None => out.push(c),
        }
    }
    (!kinds.is_empty()).then_some((out, kinds))
}

/// `s` with one level of backslash escaping added (each backslash doubled), and which escapes it
/// had (the character after each backslash); `None` when it has no backslash.
fn escape(s: &str) -> Option<(String, Vec<char>)> {
    let mut kinds = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let k = chars.next().unwrap_or('\\');
            if !kinds.contains(&k) {
                kinds.push(k);
            }
        }
    }
    (!kinds.is_empty()).then(|| (s.replace('\\', "\\\\"), kinds))
}

/// Stage 4: `old_string` with its escaping read one level differently, found exactly. `to_file`
/// makes a text's line breaks the file's. An error when both readings are in the file, or when
/// `new_string` mixes real line breaks or tabs with escaped ones that `old_string` wrote escaped
/// (so which of them are meant as written can't be told).
fn unescaped(text: &str, c: &Change, name: &str, to_file: impl Fn(&str) -> String) -> Result<Vec<(usize, usize, String)>, String> {
    type Way = fn(&str) -> Option<(String, Vec<char>)>;
    // (escaping undone, how for messages, the way, where it's found, the escapes old_string had)
    let mut found = Vec::new();
    for (undo, how, way) in [(true, "with one level of backslash escaping removed", unescape as Way), (false, "with its backslashes doubled", escape)] {
        if let Some((old, kinds)) = way(c.old) {
            let old = to_file(&old);
            let hits: Vec<(usize, usize)> = text.match_indices(&old).map(|(s, m)| (s, s + m.len())).collect();
            if !hits.is_empty() {
                found.push((undo, how, way, hits, kinds));
            }
        }
    }
    if found.len() > 1 {
        return Err(format!(
            "{name} was not found as written, and the file has it both {} and {}: which was meant can't be told. Copy the text exactly as it is in the file, backslashes included.",
            found[0].1, found[1].1
        ));
    }
    let Some((undo, how, way, hits, kinds)) = found.pop() else { return Ok(Vec::new()) };
    let new = match way(c.new) {
        None => c.new.to_string(),
        Some((new, new_kinds)) => {
            // `old_string` wrote line breaks (or tabs) as `\n` (`\t`); a `new_string` with both
            // real ones and escaped ones may mean the escaped ones as they are
            let mixed = undo && [('n', '\n'), ('t', '\t')].into_iter().any(|(k, real)| kinds.contains(&k) && !c.old.contains(real) && new_kinds.contains(&k) && c.new.contains(real));
            if mixed {
                return Err(format!(
                    "{name} was found only {how}, but its `new_string` has both real line breaks or tabs and escaped ones, so which are meant as written can't be told. Copy the text exactly as it is in the file, backslashes included."
                ));
            }
            new
        }
    };
    let new = to_file(&new);
    // read the same way, `new_string` can turn out to be what's there already
    if hits.iter().all(|&(s, e)| text[s..e] == new) {
        return Err(format!("{name} was found only {how}, and its `new_string` read the same way is the same text: that changes nothing."));
    }
    Ok(hits.into_iter().map(|(s, e)| (s, e, new.clone())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str, old: &str, new: &str) -> Result<Applied, String> {
        apply(text, &[Change { old, new, all: false }], |_| "`old_string`".into())
    }

    #[test]
    fn exact_first() {
        let a = one("let a = 1;\nlet b = 2;\n", "let b = 2;", "let b = 3;").unwrap();
        assert_eq!(a, Applied { text: "let a = 1;\nlet b = 3;\n".into(), replacements: 1, fuzzy: 0 });
        assert!(one("x\nx\n", "x", "y").unwrap_err().contains("appears 2 times"));
        assert!(one("x\n", "nope", "y").unwrap_err().contains("not found"));
        assert!(one("x\n", "", "y").unwrap_err().contains("empty"));
        assert!(one("x\n", "x", "x").unwrap_err().contains("the same"));
    }

    #[test]
    fn typographic_characters_and_trailing_whitespace_are_forgiven() {
        // the file has curly quotes, an en dash and trailing spaces; the model typed plain ones
        let file = "fn main() {\n    println!(\u{201C}it\u{2019}s 1\u{2013}2\u{201D});   \n    done();\n}\n";
        let a = one(file, "    println!(\"it's 1-2\");\n    done();", "    println!(\"ok\");\n    done();").unwrap();
        assert_eq!(a.text, "fn main() {\n    println!(\"ok\");\n    done();\n}\n");
        assert_eq!(a.fuzzy, 1);
        // the model added trailing whitespace the file doesn't have, and a non-breaking space
        let a = one("a = 1\nb = 2\n", "a\u{00A0}= 1  \nb = 2", "a = 9\nb = 2").unwrap();
        assert_eq!(a.text, "a = 9\nb = 2\n");
        // still unique, or refused
        assert!(one("x = \u{201C}a\u{201D}\nx = \"a\"  \n", "x = \"a\"", "y").is_ok(), "exact wins over normalized");
        assert!(one("x = \u{201C}a\u{201D} \ny\nx = \u{201C}a\u{201D}\n", "x = \"a\"\n", "z\n").unwrap_err().contains("appears 2 times"));
        // only whitespace isn't a thing to look for
        assert!(one("a\n  \nb\n", "  \t", "x").unwrap_err().contains("not found"));
    }

    #[test]
    fn what_the_edit_doesnt_cover_keeps_its_bytes() {
        // a match that starts mid-line: the start of the line stays as it was, curly quotes and all
        let file = "say(\u{201C}hi\u{201D}); call(\u{2018}x\u{2019});  \nnext\n";
        let a = one(file, "call('x');\nnext", "call('y');\nnext").unwrap();
        assert_eq!(a.text, "say(\u{201C}hi\u{201D}); call('y');\nnext\n");
        // a match that ends mid-line keeps that line's rest, trailing spaces included
        let a = one("one \u{2014} two   \nthree\n", "one - two", "1 - 2").unwrap();
        assert_eq!(a.text, "1 - 2   \nthree\n");
    }

    #[test]
    fn whitespace_at_the_end_of_old_string_isnt_dropped_into_a_match() {
        // the next line's indentation at the end: not a match that ends before that indentation
        assert!(one("a = \u{201C}x\u{201D}\n    b\n", "a = \"x\"\n    ", "a = 'y'\n    ").unwrap_err().contains("not found"));
        // nor a match that ends in the middle of a word
        assert!(one("ab\n", "a ", "Z").unwrap_err().contains("not found"));
        // at the end of a line it still matches (the file has no trailing space there)
        assert_eq!(one("a = \u{201C}x\u{201D}\nb\n", "a = \"x\"  ", "a = 1").unwrap().text, "a = 1\nb\n");
    }

    #[test]
    fn crlf_files_stay_crlf() {
        let file = "a\r\nb  \r\nc\r\n";
        // exact (the \n become \r\n)
        assert_eq!(one(file, "a\nb  ", "A\nB").unwrap().text, "A\r\nB\r\nc\r\n");
        // normalized (the trailing spaces and the \r don't matter)
        let a = one(file, "b\nc", "B\nC").unwrap();
        assert_eq!((a.text.as_str(), a.fuzzy), ("a\r\nB\r\nC\r\n", 1));
    }

    #[test]
    fn several_changes_at_once_or_none() {
        let file = "alpha\nbeta\ngamma\ndelta\n";
        let label = |i: usize| format!("edits[{i}]");
        let a = apply(file, &[Change { old: "gamma", new: "GAMMA", all: false }, Change { old: "alpha", new: "ALPHA", all: false }], label).unwrap();
        assert_eq!((a.text.as_str(), a.replacements), ("ALPHA\nbeta\nGAMMA\ndelta\n", 2));
        // each is matched against the file as it was, not after the change before it
        assert!(apply(file, &[Change { old: "beta", new: "BETA", all: false }, Change { old: "BETA", new: "b", all: false }], label).unwrap_err().contains("edits[1] was not found"));
        let e = apply(file, &[Change { old: "beta\ngamma", new: "x", all: false }, Change { old: "gamma\ndelta", new: "y", all: false }], label).unwrap_err();
        assert!(e.contains("edits[0] and edits[1]") && e.contains("merge"), "{e}");
        let a = apply("x x x\n", &[Change { old: "x", new: "y", all: true }], label).unwrap();
        assert_eq!((a.text.as_str(), a.replacements), ("y y y\n", 3));
    }

    #[test]
    fn indentation_shifted_by_spaces_is_forgiven_and_new_string_reindented() {
        // Python: the model dropped the class's 4 spaces
        let file = "class A:\n    def f(self):\n        if x:\n            return 1\n        return 2\n";
        let a = one(file, "def f(self):\n    if x:\n        return 1\n", "def f(self):\n    if y:\n        return 3\n\n    log()\n").unwrap();
        assert_eq!(a.text, "class A:\n    def f(self):\n        if y:\n            return 3\n\n        log()\n        return 2\n");
        assert_eq!((a.replacements, a.fuzzy), (1, 1));
        // YAML, and the old_string's last line without its line break: that line's break stays
        let file = "spec:\n    containers:\n        - name: a\n          image: x\nnext: 1\n";
        let a = one(file, "containers:\n    - name: a\n      image: x", "containers:\n    - name: a\n      image: y\n    - name: b\n      image: z").unwrap();
        assert_eq!(a.text, "spec:\n    containers:\n        - name: a\n          image: y\n        - name: b\n          image: z\nnext: 1\n");
        // the model indented more than the file: the extra comes off new_string too
        let a = one("def g():\n    return 1\nx = 2\n", "        def g():\n            return 1", "        def g():\n            return 2\n        # done").unwrap();
        assert_eq!(a.text, "def g():\n    return 2\n# done\nx = 2\n");
    }

    #[test]
    fn indentation_shifted_by_a_tab_is_forgiven() {
        let file = "func f() {\n\tif x {\n\t\treturn\n\t}\n}\n";
        let a = one(file, "if x {\n\treturn\n}", "if y {\n\treturn nil\n}").unwrap();
        assert_eq!((a.text.as_str(), a.fuzzy), ("func f() {\n\tif y {\n\t\treturn nil\n\t}\n}\n", 1));
        // tabs in the file, 4 spaces from the model: one level swapped for the other
        let a = one("\tif x {\n\t}\n", "    if x {\n    }", "    if y {\n    }").unwrap();
        assert_eq!(a.text, "\tif y {\n\t}\n");
        // a CRLF file stays CRLF
        let a = one("x:\r\n    if a:\r\n        b\r\n", "if a:\n    b\n", "if a:\n    c\n").unwrap();
        assert_eq!(a.text, "x:\r\n    if a:\r\n        c\r\n");
    }

    #[test]
    fn indentation_flexible_matches_are_strict_otherwise() {
        // two places with the same lines at different depths: refused
        let file = "a:\n  x: 1\n  y: 2\nb:\n    x: 1\n    y: 2\n";
        assert!(one(file, "x: 1\ny: 2", "x: 3\ny: 2").unwrap_err().contains("appears 2 times"));
        // with replace_all each place is re-indented to fit
        let a = apply(file, &[Change { old: "x: 1\ny: 2", new: "x: 3\ny: 2", all: true }], |_| "`old_string`".into()).unwrap();
        assert_eq!((a.text.as_str(), a.replacements), ("a:\n  x: 3\n  y: 2\nb:\n    x: 3\n    y: 2\n", 2));
        // the indentation must move the same way on every line (no flattening a block)
        assert!(one("def f():\n    return 1\n", "def f():\nreturn 1", "x").unwrap_err().contains("not found"));
        // whole lines only: a first or last line that's part of the file's line doesn't match here
        let file = "    let x = foo(1);\n    bar();\n";
        assert!(one(file, "x = foo(1);\nbar();", "x = 2;\nbar();").unwrap_err().contains("not found"));
        assert!(one(file, "let x = foo(1);\nbar(", "let x = 2;\nbaz(").unwrap_err().contains("not found"));
        // what's around the match keeps its bytes
        let a = one("keep  \n  a\n  b\nkeep\u{2019}\n", "a\nb", "c").unwrap();
        assert_eq!(a.text, "keep  \n  c\nkeep\u{2019}\n");
    }

    #[test]
    fn doubled_escapes_are_forgiven() {
        // a Rust string literal with \n in the file; the model doubled the backslash
        let file = "fn main() {\n    println!(\"a\\nb\");\n}\n";
        let a = one(file, "println!(\"a\\\\nb\");", "println!(\"a\\\\nc\");").unwrap();
        assert_eq!((a.text.as_str(), a.fuzzy), ("fn main() {\n    println!(\"a\\nc\");\n}\n", 1));
        // the file has real line breaks; the model sent \n (and new_string has \n too)
        let a = one("first\nsecond\nthird\n", "first\\nsecond", "1\\n2\\n2b").unwrap();
        assert_eq!(a.text, "1\n2\n2b\nthird\n");
        // new_string without escapes goes in as written
        let a = one("first\nsecond\nthird\n", "first\\nsecond", "only").unwrap();
        assert_eq!(a.text, "only\nthird\n");
        // ...and in a CRLF file the line breaks it makes are CRLF
        let a = one("first\r\nsecond\r\n", "first\\nsecond", "1\\n2").unwrap();
        assert_eq!(a.text, "1\r\n2\r\n");
        // JSON with \" in it, sent escaped once more
        let file = "{\"msg\": \"say \\\"hi\\\"\"}\n";
        let a = one(file, "{\\\"msg\\\": \\\"say \\\\\\\"hi\\\\\\\"\\\"}", "{\\\"msg\\\": \\\"say \\\\\\\"bye\\\\\\\"\\\"}").unwrap();
        assert_eq!(a.text, "{\"msg\": \"say \\\"bye\\\"\"}\n");
    }

    #[test]
    fn escapes_missing_a_level_are_forgiven() {
        // the file has \\n (a Rust literal for a backslash and an n); the model sent \n
        let file = "let sep = \"\\\\n\";\n";
        let a = one(file, "let sep = \"\\n\";", "let sep = \"\\t\";").unwrap();
        assert_eq!((a.text.as_str(), a.fuzzy), ("let sep = \"\\\\t\";\n", 1));
    }

    #[test]
    fn escape_drift_that_changes_nothing_is_refused() {
        // `a\\nb'` and `a\\nb\'` differ, but both read as the file's `a\nb'`: nothing would change
        let e = one("x = a\\nb';\n", "a\\\\nb'", "a\\\\nb\\'").unwrap_err();
        assert!(e.contains("changes nothing"), "{e}");
    }

    #[test]
    fn escape_drift_that_cant_be_told_is_refused() {
        // the file has the text both with a real line break and with \\n: which one?
        let file = "x = \"a\n\"\ny = \"a\\\\n\"\n";
        let e = one(file, "\"a\\n\"", "\"b\"").unwrap_err();
        assert!(e.contains("both") && e.contains("removed") && e.contains("doubled"), "{e}");
        // still unique, or refused
        assert!(one("a\nb\na\nb\n", "a\\nb", "c").unwrap_err().contains("appears 2 times"));
        // new_string with both real and escaped line breaks, where old_string wrote them escaped
        let e = one("first\nsecond\n", "first\\nsecond", "one\ntwo\\nthree").unwrap_err();
        assert!(e.contains("real line breaks"), "{e}");
    }
}
