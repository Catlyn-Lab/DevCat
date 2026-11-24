//! A SQLite file on another computer, opened over SSH (a SQLite connection with its SSH settings on).
//!
//! SQLite is a library next to its file, not a server, so there's no port to tunnel to. DevCat logs in
//! with its own SSH client (`ssh::connect_session`, the same as JDBC over SSH) and:
//!
//! - **live**, when the host has `sqlite3` 3.26 or newer: runs one long-lived `sqlite3` there on the
//!   file and sends it the statements on its stdin, reading the results from its stdout in
//!   `.mode quote` (exact integers, BLOBs as `X'…'`, text told apart from numbers). The host's SQLite
//!   does the locking, so writing is as safe as with any other program on that host. One process is one
//!   session, like the local driver's single connection: temp tables and transactions carry over.
//! - **snapshot**, when it has no usable `sqlite3`: downloads the file (and its `-wal`) into a folder of
//!   its own under DevCat's data folder and opens that copy **read-only** with the local driver.
//!   Nothing is ever written back: an app on the host may be writing to the file. Reconnecting takes a
//!   fresh copy.
//!
//! What a user or an agent types must never reach `sqlite3` as anything but SQL: a line starting with
//! `.` is a shell command there (`.shell`, `.system`, `.output`, …), and the shell adds functions that
//! reach other files (`readfile`, `writefile`, `edit`, `fsdir`, `zipfile`, `load_extension`) next to
//! `ATTACH` and `VACUUM INTO`. So every statement is checked ([`check_statement`]) and refused if it has
//! such a line or word, `-safe` is used where the host's `sqlite3` has it (3.37+) and `-init /dev/null`
//! skips the user's `~/.sqliterc`. A read-only connection (and the read-only twin agents' reads use)
//! starts `sqlite3 -readonly`, so the host's SQLite refuses writes whatever the words say.
//!
//! What's lost against a local file: column types of a query's result aren't known (the grid shows the
//! values' own types), and duplicate column names of a query DevCat cuts short get SQLite's `:1` suffix.

use super::sql_common as sc;
use super::sqlite::{self as lite, Rows};
use super::*;
use crate::error::{AppError, AppResult};
use crate::models::{ConnectionConfig, ConnectionKind, SshConfig};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};

const D: Dialect = Dialect::Sqlite;
const DEFAULT_MAX_ROWS: usize = 1000;
const JS_MAX_SAFE_INT: i64 = 9_007_199_254_740_991;
/// Oldest `sqlite3` that runs live: `pragma_table_xinfo` (the structure queries) came in 3.26.
const MIN_LIVE: (u32, u32) = (3, 26);
/// `-safe` came in 3.37.
const MIN_SAFE: (u32, u32) = (3, 37);
/// A copy bigger than this isn't downloaded.
const MAX_SNAPSHOT: u64 = 1 << 30;
/// A copy taken while an app writes can be torn: tries before giving up.
const SNAPSHOT_TRIES: usize = 3;

type Session = super::ssh::SshSession;

/// Open a SQLite connection whose SSH settings are on.
pub async fn connect(cfg: &ConnectionConfig) -> AppResult<DynDriver> {
    let ssh = cfg.ssh.clone().filter(|s| s.enabled).ok_or_else(|| AppError::msg("SSH settings missing"))?;
    if cfg.url.as_deref().is_some_and(|u| !u.trim().is_empty()) {
        return Err(AppError::msg("SQLite over SSH takes the file's path on the host, not a URL"));
    }
    let path = cfg
        .file_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| AppError::msg("No SQLite database file: type its path on the SSH host"))?
        .to_string();
    if path == ":memory:" {
        return Err(AppError::msg("An in-memory database can't be opened over SSH"));
    }
    let host = ssh.host.trim().to_string();
    let session = Arc::new(super::ssh::connect_session(&ssh).await?);
    let probe = probe(&session, &path).await.map_err(|e| AppError::msg(format!("{host}: {e}")))?;
    if !probe.posix {
        return Err(AppError::msg(format!(
            "SQLite over SSH needs a Linux, macOS or BSD host; {host} doesn't look like one (no POSIX shell)"
        )));
    }
    let create = cfg.options.get("create").is_some_and(|v| v == "true") && !cfg.read_only;
    match probe.kind {
        FileKind::Dir => return Err(AppError::msg(format!("{path} on {host} is a folder, not a database file"))),
        FileKind::Missing if !create => {
            return Err(AppError::msg(format!(
                "No file {path} on {host}. Check the path, or tick \"Create the file if it doesn't exist\" to make a new database there."
            )))
        }
        _ => {}
    }
    match probe.sqlite.clone().filter(|v| v.at_least(MIN_LIVE)) {
        Some(v) => Ok(Arc::new(LiveDriver::start(session, ssh, host, path, v, cfg.read_only, create).await?)),
        None => {
            if probe.kind == FileKind::Missing {
                return Err(AppError::msg(format!(
                    "No file {path} on {host}, and DevCat can only make one with sqlite3 3.26 or newer there"
                )));
            }
            let reason = match &probe.sqlite {
                Some(v) => format!("its sqlite3 is {}, older than 3.26", v.text),
                None => "it has no sqlite3".to_string(),
            };
            Ok(Arc::new(SnapshotDriver::take(&session, host, path, &probe, reason).await?))
        }
    }
}

// -------------------------------------------------------------------------------------------------
// The host

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    File,
    Dir,
    Missing,
}

#[derive(Debug, Clone, PartialEq)]
struct Version {
    text: String,
    major: u32,
    minor: u32,
}

impl Version {
    /// From `sqlite3 -version`: `3.45.3 2024-04-15 13:34:05 …`.
    fn parse(s: &str) -> Option<Version> {
        let text = s.split_whitespace().next()?.to_string();
        let mut it = text.split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next()?.parse().ok()?;
        Some(Version { text, major, minor })
    }
    fn at_least(&self, (major, minor): (u32, u32)) -> bool {
        (self.major, self.minor) >= (major, minor)
    }
}

#[derive(Debug)]
struct Probe {
    posix: bool,
    kind: FileKind,
    wal: bool,
    size: Option<u64>,
    sqlite: Option<Version>,
}

/// The path as a POSIX shell word: `~/…` as the host's home (quoted, it wouldn't expand), and a
/// relative path (from the login folder) kept from looking like an option.
fn path_word(p: &str) -> String {
    if p == "~" {
        return "\"$HOME\"".into();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return format!("\"$HOME\"/{}", super::jdbc::sh_quote(rest));
    }
    if p.starts_with('/') {
        super::jdbc::sh_quote(p)
    } else {
        super::jdbc::sh_quote(&format!("./{p}"))
    }
}

/// A command for the host: `sh` whatever the login shell is, as a login shell for the PATH its
/// profile sets (`sqlite3` in `/usr/local/bin`, Homebrew…).
fn sh(script: &str) -> String {
    format!("exec sh -lc {}", super::jdbc::sh_quote(script))
}

fn probe_script(path: &str) -> String {
    format!(
        "p={}; echo \"os=$(uname -s 2>/dev/null)\"; \
         if [ -d \"$p\" ]; then echo kind=dir; elif [ -e \"$p\" ]; then echo kind=file; else echo kind=none; fi; \
         if [ -f \"$p-wal\" ] && [ -s \"$p-wal\" ]; then echo wal=yes; fi; \
         if [ -f \"$p\" ]; then echo \"size=$(wc -c < \"$p\" | tr -d ' ')\"; fi; \
         v=$(sqlite3 -version 2>/dev/null) && echo \"sqlite=$v\"; true",
        path_word(path)
    )
}

fn parse_probe(out: &str) -> Probe {
    let mut p = Probe { posix: false, kind: FileKind::Missing, wal: false, size: None, sqlite: None };
    for line in out.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "os" => p.posix = !v.trim().is_empty(),
            "kind" => {
                p.kind = match v.trim() {
                    "dir" => FileKind::Dir,
                    "file" => FileKind::File,
                    _ => FileKind::Missing,
                }
            }
            "wal" => p.wal = v.trim() == "yes",
            "size" => p.size = v.trim().parse().ok(),
            "sqlite" => p.sqlite = Version::parse(v),
            _ => {}
        }
    }
    p
}

async fn probe(session: &Session, path: &str) -> AppResult<Probe> {
    let (_, out, _) = super::jdbc::ssh_run(session, &sh(&probe_script(path)), None).await?;
    Ok(parse_probe(&out))
}

// -------------------------------------------------------------------------------------------------
// What may be sent

/// Functions and statements the `sqlite3` shell has that reach other files on the host (or load
/// code): never sent, whatever the connection's access (`-safe` on 3.37+ refuses them too, but by
/// ending the process).
const FILE_WORDS: &[&str] = &[
    "attach",
    "load_extension",
    "readfile",
    "writefile",
    "edit",
    "fsdir",
    "zipfile",
    "zipfile_cds",
    "sqlar_compress",
    "sqlar_uncompress",
    "fts3_tokenizer",
    "lsmode",
];

/// The words of a statement with string literals and comments taken out (quoted identifiers
/// stay: `"readfile"(…)` calls the function too), lower-cased.
fn words(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.is_empty() {
            out.push(std::mem::take(cur).to_ascii_lowercase());
        }
    };
    while i < b.len() {
        let c = b[i];
        if c == b'\'' {
            flush(&mut cur, &mut out);
            i += 1;
            while i < b.len() {
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
        } else if c == b'-' && b.get(i + 1) == Some(&b'-') {
            flush(&mut cur, &mut out);
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            flush(&mut cur, &mut out);
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80 {
            cur.push(c as char);
            i += 1;
        } else {
            flush(&mut cur, &mut out);
            i += 1;
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// Refuse a statement that `sqlite3` would read as more than SQL, or that reaches other files.
pub(super) fn check_statement(sql: &str) -> Result<(), String> {
    if sql.lines().any(|l| l.trim_start().starts_with('.')) {
        return Err("a line that starts with \".\" isn't sent to sqlite3 on the host (it would be a shell command there). \
                    Put that text on the line before, or build it with char(10)"
            .into());
    }
    if sql.contains('\0') {
        return Err("the statement contains a NUL character".into());
    }
    let w = words(sql);
    if let Some(bad) = w.iter().find(|x| FILE_WORDS.contains(&x.as_str())) {
        return Err(format!("{bad} isn't allowed over SSH: it reaches other files on the host"));
    }
    if w.first().is_some_and(|f| f == "vacuum") && w.iter().any(|x| x == "into") {
        return Err("VACUUM INTO isn't allowed over SSH: it writes another file on the host".into());
    }
    Ok(())
}

/// `sql` as `sqlite3` must get it: complete (ending with `;` outside strings and comments), or an
/// error for an unterminated string or comment (whatever came after it would be read into it).
fn terminated(sql: &str) -> Result<String, String> {
    if complete(sql) {
        return Ok(sql.to_string());
    }
    let t = format!("{sql}\n;");
    if complete(&t) {
        Ok(t)
    } else {
        Err("the statement isn't complete (an unterminated string, quoted name or comment?)".into())
    }
}

fn complete(sql: &str) -> bool {
    let Ok(c) = std::ffi::CString::new(sql) else { return false };
    // SAFETY: a valid NUL-terminated string; sqlite3_complete only reads it.
    unsafe { libsqlite3_sys::sqlite3_complete(c.as_ptr()) != 0 }
}

// -------------------------------------------------------------------------------------------------
// Values in, values out

/// A text value as SQL that has no line breaks or other control characters in it (they become
/// `char(n)`), so nothing in it can start a line `sqlite3` reads as a command.
pub(super) fn text_literal(s: &str) -> String {
    if !s.chars().any(|c| c.is_control() && c != '\t') {
        return format!("'{}'", s.replace('\'', "''"));
    }
    let mut parts: Vec<String> = Vec::new();
    let mut run = String::new();
    for c in s.chars() {
        if c.is_control() && c != '\t' {
            if !run.is_empty() {
                parts.push(format!("'{}'", std::mem::take(&mut run).replace('\'', "''")));
            }
            parts.push(format!("char({})", c as u32));
        } else {
            run.push(c);
        }
    }
    if !run.is_empty() {
        parts.push(format!("'{}'", run.replace('\'', "''")));
    }
    format!("({})", parts.join("||"))
}

fn literal(v: &Value, blob: bool) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "1".into() } else { "0".into() },
        Value::Number(n) => n.to_string(),
        Value::String(s) if blob => match sc::parse_hex_binary(s) {
            Some(bytes) => format!("X'{}'", hex::encode(bytes)),
            None => text_literal(s),
        },
        Value::String(s) => text_literal(s),
        other => text_literal(&other.to_string()),
    }
}

/// Put `params` into `sql` for `?NNN` and `?` (outside strings, quoted names and comments).
fn bind(sql: &str, params: &[Value], blob: &dyn Fn(usize) -> bool) -> String {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut i = 0;
    let mut next = 0usize;
    let mut start = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'?' => {
                out.push_str(&sql[start..i]);
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                let n = if j > i + 1 {
                    sql[i + 1..j].parse::<usize>().unwrap_or(1).saturating_sub(1)
                } else {
                    next
                };
                next = n + 1;
                out.push_str(&params.get(n).map(|v| literal(v, blob(n))).unwrap_or_else(|| "NULL".into()));
                i = j;
                start = j;
            }
            _ => i += 1,
        }
    }
    out.push_str(&sql[start.min(sql.len())..]);
    out
}

/// One `.mode quote` value: `NULL`, a number, `'text'`, `X'hex'`, `unistr('…')` (3.50+, for
/// control characters) or `replace('…','\n',char(10))` (older versions, for line breaks).
struct QuoteParser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> QuoteParser<'a> {
    fn eat(&mut self, lit: &str) -> bool {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn string(&mut self) -> Option<String> {
        if !self.eat("'") {
            return None;
        }
        let mut buf = Vec::new();
        loop {
            let c = *self.s.get(self.i)?;
            self.i += 1;
            if c == b'\'' {
                if self.s.get(self.i) == Some(&b'\'') {
                    buf.push(b'\'');
                    self.i += 1;
                    continue;
                }
                return Some(String::from_utf8_lossy(&buf).into_owned());
            }
            buf.push(c);
        }
    }

    fn text(&mut self) -> Option<String> {
        if self.eat("unistr(") {
            let s = self.string()?;
            if !self.eat(")") {
                return None;
            }
            return Some(unistr(&s));
        }
        if self.eat("replace(") {
            let inner = self.text()?;
            if !self.eat(",") {
                return None;
            }
            let token = self.string()?;
            if !self.eat(",char(") {
                return None;
            }
            let start = self.i;
            while self.s.get(self.i).is_some_and(u8::is_ascii_digit) {
                self.i += 1;
            }
            let n: u32 = std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()?;
            if !self.eat("))") {
                return None;
            }
            return Some(inner.replace(&token, &char::from_u32(n)?.to_string()));
        }
        self.string()
    }

    fn value(&mut self) -> Option<Value> {
        let rest = &self.s[self.i..];
        if self.eat("NULL") {
            return Some(Value::Null);
        }
        if rest.starts_with(b"X'") || rest.starts_with(b"x'") {
            self.i += 1;
            let h = self.string()?;
            return Some(sc::binary_to_value(&hex::decode(h).ok()?));
        }
        if rest.first().is_some_and(|c| *c == b'\'') || rest.starts_with(b"unistr(") || rest.starts_with(b"replace(") {
            return self.text().map(Value::String);
        }
        // a number
        let start = self.i;
        while self.s.get(self.i).is_some_and(|c| !matches!(c, b',' | b'\n')) {
            self.i += 1;
        }
        let tok = std::str::from_utf8(&self.s[start..self.i]).ok()?.trim();
        number(tok)
    }

    /// A record: values separated by `,`, ending at a line break (or the end).
    fn record(&mut self) -> Option<Vec<Value>> {
        let mut vals = Vec::new();
        loop {
            vals.push(self.value()?);
            match self.s.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'\n') => {
                    self.i += 1;
                    return Some(vals);
                }
                None => return Some(vals),
                _ => return None,
            }
        }
    }
}

fn number(tok: &str) -> Option<Value> {
    match tok {
        "Inf" | "9.0e+999" => return Some(Value::String("inf".into())),
        "-Inf" | "-9.0e+999" => return Some(Value::String("-inf".into())),
        "NaN" => return Some(Value::String("NaN".into())),
        _ => {}
    }
    if !tok.contains(['.', 'e', 'E']) {
        return Some(match tok.parse::<i64>() {
            Ok(n) if n.abs() <= JS_MAX_SAFE_INT => Value::from(n),
            Ok(n) => Value::String(n.to_string()),
            Err(_) if !tok.is_empty() && tok.trim_start_matches('-').bytes().all(|c| c.is_ascii_digit()) => Value::String(tok.into()),
            Err(_) => return None,
        });
    }
    let f: f64 = tok.parse().ok()?;
    Some(serde_json::Number::from_f64(f).map(Value::Number).unwrap_or_else(|| Value::String(f.to_string())))
}

/// `unistr()`'s escapes: `\\`, `\uXXXX`, `\+XXXXXX`, `\UXXXXXXXX`.
fn unistr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let n = match it.peek() {
            Some('\\') => {
                it.next();
                out.push('\\');
                continue;
            }
            Some('u') => 4,
            Some('+') => 6,
            Some('U') => 8,
            _ => {
                out.push('\\');
                continue;
            }
        };
        it.next();
        let hex: String = (0..n).filter_map(|_| it.next()).collect();
        match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
            Some(ch) => out.push(ch),
            None => {
                out.push('\\');
                out.push_str(&hex);
            }
        }
    }
    out
}

/// What `sqlite3` printed for one statement: a header and rows, or an error.
#[derive(Debug, PartialEq)]
enum Output {
    Rows { columns: Vec<String>, rows: Vec<Vec<Value>> },
    Error(String),
}

fn is_error_line(s: &[u8]) -> bool {
    [&b"Parse error"[..], b"Runtime error", b"Error:", b"Error near", b"Error: near"].iter().any(|p| s.starts_with(p))
        || (s.starts_with(b"line ") && s.windows(2).any(|w| w == b": "))
}

/// `Parse error near line 7: no such table: x` → `no such table: x` (the line is the session's).
fn clean_error(s: &str) -> String {
    let s = s.trim();
    let first = s.lines().next().unwrap_or("");
    let rest: String = s.lines().skip(1).collect::<Vec<_>>().join("\n");
    let msg = match first.find("line ") {
        Some(at) => {
            let after = &first[at + 5..];
            let digits = after.bytes().take_while(u8::is_ascii_digit).count();
            match after[digits..].strip_prefix(": ") {
                Some(m) if digits > 0 => m.to_string(),
                _ => first.trim_start_matches("Error: ").to_string(),
            }
        }
        None => first.trim_start_matches("Error: ").to_string(),
    };
    if rest.trim().is_empty() {
        msg
    } else {
        format!("{msg}\n{rest}")
    }
}

fn parse_output(text: &[u8]) -> Output {
    let mut p = QuoteParser { s: text, i: 0 };
    let mut records: Vec<Vec<Value>> = Vec::new();
    while p.i < text.len() {
        if is_error_line(&text[p.i..]) {
            return Output::Error(clean_error(&String::from_utf8_lossy(&text[p.i..])));
        }
        let at = p.i;
        match p.record() {
            Some(r) => records.push(r),
            None => {
                // not quote-mode output: an error printed in the middle of the rows, or something else
                let all = String::from_utf8_lossy(text);
                let from = all.lines().position(|l| is_error_line(l.as_bytes()));
                return Output::Error(match from {
                    Some(n) => clean_error(&all.lines().skip(n).collect::<Vec<_>>().join("\n")),
                    None => format!("unexpected output from sqlite3: {}", String::from_utf8_lossy(&text[at..]).chars().take(300).collect::<String>()),
                });
            }
        }
    }
    let mut it = records.into_iter();
    let columns = it.next().map(|h| h.iter().map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).collect()).unwrap_or_default();
    Output::Rows { columns, rows: it.collect() }
}

// -------------------------------------------------------------------------------------------------
// Live: `sqlite3` on the host

/// One `sqlite3` process on the host, behind its exec channel.
struct Cli {
    write: russh::ChannelWriteHalf<russh::client::Msg>,
    out: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    mark: String,
    _pump: tokio::task::JoinHandle<()>,
}

impl Cli {
    async fn send(&self, text: &str) -> AppResult<()> {
        self.write.data(text.as_bytes()).await.map_err(|e| AppError::msg(format!("SSH: {e}")))
    }

    /// The output up to the line `<mark><tag>`; `Err` with what came if the process ended first.
    async fn read_to(&mut self, tag: &str) -> Result<Vec<u8>, String> {
        let line = format!("{}{tag}\n", self.mark);
        let mut from = 0;
        loop {
            let found = if self.buf.starts_with(line.as_bytes()) {
                Some(0)
            } else {
                let nl = format!("\n{line}");
                self.buf[from..].windows(nl.len()).position(|w| w == nl.as_bytes()).map(|p| from + p + 1)
            };
            if let Some(at) = found {
                let mut section: Vec<u8> = self.buf.drain(..at + line.len()).collect();
                section.truncate(at);
                if section.last() == Some(&b'\n') {
                    section.pop();
                }
                return Ok(section);
            }
            from = self.buf.len().saturating_sub(line.len() + 1);
            match self.out.recv().await {
                Some(chunk) => self.buf.extend_from_slice(&chunk),
                None => return Err(String::from_utf8_lossy(&std::mem::take(&mut self.buf)).trim().to_string()),
            }
        }
    }

    /// Run one complete, checked statement: its output and the number of rows it changed.
    async fn run(&mut self, sql: &str) -> Result<(Output, u64), String> {
        let payload = format!("{sql}\n.print {m}R\nSELECT changes();\n.print {m}E\n", m = self.mark);
        self.send(&payload).await.map_err(|e| e.to_string())?;
        let out = parse_output(&self.read_to("R").await?);
        let changes = match parse_output(&self.read_to("E").await?) {
            Output::Rows { rows, .. } => rows.first().and_then(|r| r.first()).and_then(Value::as_u64).unwrap_or(0),
            Output::Error(_) => 0,
        };
        Ok((out, changes))
    }
}

/// The process ended (the session dropped, `-safe` refused something, it was cancelled).
fn ended(host: &str, said: &str) -> AppError {
    let said = said.trim();
    if said.is_empty() {
        AppError::msg(format!("sqlite3 on {host} stopped. The next query starts it again (temp tables and an open transaction are gone)."))
    } else {
        AppError::msg(format!(
            "sqlite3 on {host} stopped: {}\nThe next query starts it again (temp tables and an open transaction are gone).",
            clean_error(said)
        ))
    }
}

/// Runs internal catalog queries (no user text: params go in as literals).
struct CliRows<'a> {
    cli: &'a mut Cli,
    host: &'a str,
    dead: bool,
}

#[async_trait]
impl Rows for CliRows<'_> {
    async fn rows(&mut self, sql: &str, params: &[Value]) -> AppResult<Vec<Vec<Value>>> {
        let sql = bind(sql, params, &|_| false);
        check_statement(&sql).map_err(AppError::msg)?;
        let sql = terminated(&sql).map_err(AppError::msg)?;
        match self.cli.run(&sql).await {
            Ok((Output::Rows { rows, .. }, _)) => Ok(rows),
            Ok((Output::Error(e), _)) => Err(AppError::msg(e)),
            Err(said) => {
                self.dead = true;
                Err(ended(self.host, &said))
            }
        }
    }
}

pub struct LiveDriver {
    host: String,
    path: String,
    version: Version,
    read_only: bool,
    safe: bool,
    ssh: SshConfig,
    session: Mutex<Arc<Session>>,
    cli: Mutex<Option<Cli>>,
    pid: parking_lot::Mutex<Option<u32>>,
    cancelled: AtomicBool,
}

impl LiveDriver {
    async fn start(session: Arc<Session>, ssh: SshConfig, host: String, path: String, version: Version, read_only: bool, create: bool) -> AppResult<Self> {
        let safe = version.at_least(MIN_SAFE);
        let d = LiveDriver {
            host,
            path,
            version,
            read_only,
            safe,
            ssh,
            session: Mutex::new(session),
            cli: Mutex::new(None),
            pid: parking_lot::Mutex::new(None),
            cancelled: AtomicBool::new(false),
        };
        let mut g = d.cli.lock().await;
        let cli = d.spawn().await?;
        *g = Some(cli);
        // Opening is lazy; make sure the file is really a database (and, with "create", make it one).
        let mut r = CliRows { cli: g.as_mut().expect("just set"), host: &d.host, dead: false };
        let check = r.rows("SELECT count(*) FROM sqlite_master", &[]).await;
        if let Err(e) = check {
            return Err(AppError::msg(format!("{} on {}: {e}", d.path, d.host)));
        }
        let _ = create; // sqlite3 made the file (empty, a valid database) when it opened it
        drop(g);
        Ok(d)
    }

    fn command(&self) -> String {
        let flags = format!("{}{}", if self.read_only { " -readonly" } else { "" }, if self.safe { " -safe" } else { "" });
        sh(&format!("echo \"DEVCAT-PID $$\"; exec sqlite3 -init /dev/null -batch{flags} {} 2>&1", path_word(&self.path)))
    }

    /// Start `sqlite3` on the host (logging in again if the session dropped).
    async fn spawn(&self) -> AppResult<Cli> {
        let mut session = self.session.lock().await;
        let ch = match session.channel_open_session().await {
            Ok(ch) => ch,
            Err(_) => {
                *session = Arc::new(super::ssh::connect_session(&self.ssh).await?);
                session.channel_open_session().await.map_err(|e| AppError::msg(format!("SSH: {e}")))?
            }
        };
        ch.exec(true, self.command()).await.map_err(|e| AppError::msg(format!("SSH: {e}")))?;
        let (mut read, write) = ch.split();
        let (tx, out) = mpsc::channel::<Vec<u8>>(256);
        let pump = tokio::spawn(async move {
            use russh::ChannelMsg;
            while let Some(msg) = read.wait().await {
                let data = match msg {
                    ChannelMsg::Data { data } => data.to_vec(),
                    ChannelMsg::ExtendedData { data, .. } => data.to_vec(),
                    ChannelMsg::Eof | ChannelMsg::Close => break,
                    _ => continue,
                };
                if tx.send(data).await.is_err() {
                    break;
                }
            }
        });
        let mark = format!("@@devcat-{}:", uuid::Uuid::new_v4().simple());
        let mut cli = Cli { write, out, buf: Vec::new(), mark, _pump: pump };
        cli.send(&format!(".mode quote\n.headers on\n.timeout 5000\n.print {}S\n", cli.mark)).await?;
        let hello = cli.read_to("S").await.map_err(|said| {
            let lower = said.to_ascii_lowercase();
            if lower.contains("not found") || lower.contains("no such file") {
                AppError::msg(format!("sqlite3 wasn't found on {} for this login: {said}", self.host))
            } else {
                ended(&self.host, &said)
            }
        })?;
        let hello = String::from_utf8_lossy(&hello);
        let mut lines = hello.lines();
        let pid = lines.next().and_then(|l| l.trim().strip_prefix("DEVCAT-PID ")).and_then(|p| p.trim().parse().ok());
        *self.pid.lock() = pid;
        let rest: Vec<&str> = lines.filter(|l| !l.trim().is_empty()).collect();
        if !rest.is_empty() {
            return Err(AppError::msg(format!("sqlite3 on {}: {}", self.host, clean_error(&rest.join("\n")))));
        }
        Ok(cli)
    }

    /// The running `sqlite3`, started if needed (after a cancel or a stop).
    async fn lock(&self) -> AppResult<tokio::sync::MutexGuard<'_, Option<Cli>>> {
        let mut g = self.cli.lock().await;
        self.cancelled.store(false, Ordering::SeqCst);
        if g.is_none() {
            *g = Some(self.spawn().await?);
        }
        Ok(g)
    }

    /// Run a catalog read with the shared code in `sqlite.rs`.
    async fn with_rows<T>(&self, f: impl for<'r, 's> FnOnce(&'r mut CliRows<'s>) -> futures::future::BoxFuture<'r, AppResult<T>>) -> AppResult<T> {
        let mut g = self.lock().await?;
        let mut r = CliRows { cli: g.as_mut().expect("started"), host: &self.host, dead: false };
        let out = f(&mut r).await;
        let dead = r.dead;
        if dead {
            *g = None;
        }
        out
    }

    /// One statement from the user, with at most `max_rows` rows.
    async fn statement(&self, cli: &mut Cli, sql: &str, max_rows: usize) -> Result<QueryResult, StmtError> {
        let started = Instant::now();
        check_statement(sql).map_err(StmtError::Sql)?;
        let body = sql.trim().trim_end_matches(';').trim_end();
        let first = sc::may_return_rows(D, sql).then(|| words(sql).into_iter().next()).flatten().unwrap_or_default();
        let wrappable = sc::can_stop_early(D, sql) && matches!(first.as_str(), "select" | "with" | "values");
        let mut truncated = false;
        // a query that has no side effects is cut short on the host
        let mut ran = None;
        if wrappable {
            let wrapped = terminated(&format!("SELECT * FROM (\n{body}\n) LIMIT {}", max_rows.saturating_add(1))).map_err(StmtError::Sql)?;
            match cli.run(&wrapped).await {
                Ok(r @ (Output::Rows { .. }, _)) => ran = Some(r),
                // something the wrapping doesn't fit: run it as it is (it has no side effects)
                Ok(_) => {}
                Err(said) => return Err(StmtError::Ended(said)),
            }
        }
        let (out, changes) = match ran {
            Some(r) => r,
            None => cli.run(&terminated(sql).map_err(StmtError::Sql)?).await.map_err(StmtError::Ended)?,
        };
        let (mut columns, mut rows) = match out {
            Output::Rows { columns, rows } => (columns, rows),
            Output::Error(e) => return Err(StmtError::Sql(e)),
        };
        if rows.len() > max_rows {
            rows.truncate(max_rows);
            truncated = true;
        }
        // no rows: no header either; ask for the columns with a row of NULLs
        if columns.is_empty() && wrappable {
            let probe = terminated(&format!("SELECT * FROM (SELECT NULL AS devcat_) LEFT JOIN (\n{body}\n) ON 1 LIMIT 1")).map_err(StmtError::Sql)?;
            if let (Output::Rows { columns: c, .. }, _) = cli.run(&probe).await.map_err(StmtError::Ended)? {
                columns = c.into_iter().skip(1).collect();
            }
        }
        let has_result_set = !columns.is_empty() || (sc::may_return_rows(D, sql) && !rows.is_empty());
        Ok(QueryResult {
            statement: sql.to_string(),
            columns: columns.into_iter().map(|name| ColumnInfo { name, type_name: String::new() }).collect(),
            rows,
            rows_affected: if has_result_set { None } else { Some(changes) },
            elapsed_ms: started.elapsed().as_millis() as u64,
            truncated,
            message: None,
        })
    }

    fn cancelled_error(&self) -> String {
        format!("Query cancelled. sqlite3 on {} was stopped; the next query starts it again (temp tables and an open transaction are gone).", self.host)
    }
}

enum StmtError {
    Sql(String),
    Ended(String),
}

#[async_trait]
impl Driver for LiveDriver {
    fn kind(&self) -> ConnectionKind {
        ConnectionKind::Sqlite
    }
    fn dialect(&self) -> Dialect {
        D
    }

    async fn server_version(&self) -> AppResult<String> {
        let v = self.with_rows(|r| Box::pin(lite::server_version(r))).await?;
        let v = if v.is_empty() { self.version.text.clone() } else { v };
        Ok(format!("SQLite {v} on {} (sqlite3 over SSH{})", self.host, if self.read_only { ", read-only" } else { "" }))
    }

    async fn children(&self, path: &[String]) -> AppResult<Vec<TreeNode>> {
        let path = path.to_vec();
        self.with_rows(move |r| Box::pin(async move { lite::children(r, &path).await })).await
    }

    async fn list_databases(&self) -> AppResult<Vec<String>> {
        self.with_rows(|r| Box::pin(lite::list_databases(r))).await
    }

    async fn execute(&self, script: &str, opts: &ExecOptions) -> AppResult<Vec<QueryResult>> {
        let stmts = sc::split_script(D, script);
        if stmts.is_empty() {
            return Ok(vec![]);
        }
        if self.read_only {
            if let Some(i) = stmts.iter().position(|s| !sc::is_read_only_statement(D, &s.sql)) {
                return Err(sc::read_only_error(i + 1));
            }
        }
        for (i, st) in stmts.iter().enumerate() {
            if let Err(e) = check_statement(&st.sql) {
                return Err(sc::statement_error(i + 1, stmts.len(), st.line(script), &st.sql, e));
            }
        }
        let max_rows = opts.max_rows.filter(|m| *m > 0).unwrap_or(DEFAULT_MAX_ROWS);
        let mut g = self.lock().await?;
        let mut out = Vec::with_capacity(stmts.len());
        for (i, st) in stmts.iter().enumerate() {
            let cli = g.as_mut().expect("started");
            match self.statement(cli, &st.sql, max_rows).await {
                Ok(r) => out.push(r),
                Err(StmtError::Sql(e)) => return Err(sc::statement_error(i + 1, stmts.len(), st.line(script), &st.sql, e)),
                Err(StmtError::Ended(said)) => {
                    *g = None;
                    let msg = if self.cancelled.swap(false, Ordering::SeqCst) { self.cancelled_error() } else { ended(&self.host, &said).to_string() };
                    return Err(sc::statement_error(i + 1, stmts.len(), st.line(script), &st.sql, msg));
                }
            }
        }
        Ok(out)
    }

    async fn table_data(&self, req: &TableDataRequest) -> AppResult<TableDataPage> {
        let db = lite::db_of(req.table.database.as_deref().or(req.table.schema.as_deref())).to_string();
        let table = lite::tref(&db, &req.table.name);
        let filter = req.filter.as_deref().map(str::trim).filter(|f| !f.is_empty());
        if let Some(f) = filter {
            if sc::split_statements(D, f).len() > 1 {
                return Err(AppError::msg("The filter must be a single SQL expression"));
            }
            check_statement(f).map_err(AppError::msg)?;
            if self.read_only && !sc::is_read_only_statement(D, &format!("SELECT 1 WHERE {f}")) {
                return Err(sc::read_only_error(1));
            }
        }
        let mut g = self.lock().await?;
        let mut r = CliRows { cli: g.as_mut().expect("started"), host: &self.host, dead: false };
        let pk = lite::primary_key(&mut r, &db, &req.table.name).await;
        let pk = match pk {
            Ok(pk) => pk,
            Err(e) => {
                if r.dead {
                    *g = None;
                }
                return Err(e);
            }
        };
        let sql = sc::select_page_sql(D, &table, filter, &req.sort, &pk, req.limit.max(1), req.offset);
        let cli = g.as_mut().expect("started");
        let result = match self.statement(cli, &sql, usize::MAX).await {
            Ok(r) => r,
            Err(StmtError::Sql(e)) => return Err(AppError::msg(e)),
            Err(StmtError::Ended(said)) => {
                *g = None;
                return Err(ended(&self.host, &said));
            }
        };
        let mut r = CliRows { cli: g.as_mut().expect("started"), host: &self.host, dead: false };
        let total = r.rows(&sc::count_sql(D, &table, filter), &[]).await;
        if r.dead {
            *g = None;
        }
        let total = total?.first().and_then(|r| r.first()).and_then(Value::as_i64).unwrap_or(0);
        Ok(TableDataPage { result, total: Some(total.max(0) as u64), primary_key: pk })
    }

    async fn table_structure(&self, table: &TableRef) -> AppResult<TableStructure> {
        let table = table.clone();
        self.with_rows(move |r| Box::pin(async move { lite::table_structure(r, &table).await })).await
    }

    async fn apply_changes(&self, table: &TableRef, changes: &[RowChange]) -> AppResult<u64> {
        if self.read_only {
            return Err(AppError::msg("This connection is read-only"));
        }
        if changes.is_empty() {
            return Ok(0);
        }
        let db = lite::db_of(table.database.as_deref().or(table.schema.as_deref())).to_string();
        let t = lite::tref(&db, &table.name);
        let mut g = self.lock().await?;
        let mut r = CliRows { cli: g.as_mut().expect("started"), host: &self.host, dead: false };
        let types = match lite::column_types(&mut r, &db, &table.name).await {
            Ok(t) => t,
            Err(e) => {
                if r.dead {
                    *g = None;
                }
                return Err(e);
            }
        };
        let mut stmts = Vec::with_capacity(changes.len());
        for change in changes {
            let stmt = sc::build_change(D, &t, change, &|_, p| p)?;
            let blob = |i: usize| stmt.param_columns.get(i).and_then(|c| types.get(c)).is_some_and(|t| t.contains("BLOB"));
            let sql = bind(&stmt.sql, &stmt.params, &blob);
            check_statement(&sql).map_err(AppError::msg)?;
            stmts.push((terminated(&sql).map_err(AppError::msg)?, stmt.expect_one, stmt.sql.clone()));
        }
        let cli = g.as_mut().expect("started");
        let result: Result<u64, AppError> = async {
            match cli.run("BEGIN;").await {
                Ok((Output::Error(e), _)) => return Err(AppError::msg(e)),
                Err(said) => return Err(ended(&self.host, &said)),
                _ => {}
            }
            let mut total = 0u64;
            for (n, (sql, expect_one, shown)) in stmts.iter().enumerate() {
                let (out, affected) = cli.run(sql).await.map_err(|said| ended(&self.host, &said))?;
                if let Output::Error(e) = out {
                    let _ = cli.run("ROLLBACK;").await;
                    return Err(AppError::msg(format!("Change {}: {e}", n + 1)));
                }
                if *expect_one && affected != 1 {
                    let _ = cli.run("ROLLBACK;").await;
                    return Err(sc::row_count_error(n + 1, shown, affected));
                }
                total += affected;
            }
            match cli.run("COMMIT;").await {
                Ok((Output::Error(e), _)) => {
                    let _ = cli.run("ROLLBACK;").await;
                    Err(AppError::msg(e))
                }
                Err(said) => Err(ended(&self.host, &said)),
                _ => Ok(total),
            }
        }
        .await;
        if result.as_ref().is_err_and(|e| e.to_string().contains(" stopped")) {
            *g = None;
        }
        result
    }

    async fn object_ddl(&self, table: &TableRef, kind: &NodeKind) -> AppResult<String> {
        let (table, kind) = (table.clone(), kind.clone());
        self.with_rows(move |r| Box::pin(async move { lite::object_ddl(r, &table, &kind).await })).await
    }

    async fn schema_snapshot(&self, database: Option<&str>, schema: Option<&str>) -> AppResult<SchemaSnapshot> {
        let (database, schema) = (database.map(String::from), schema.map(String::from));
        self.with_rows(move |r| Box::pin(async move { lite::schema_snapshot(r, database.as_deref(), schema.as_deref()).await })).await
    }

    /// Stops `sqlite3` on the host (it has no way to interrupt one statement from outside); the
    /// next call starts it again.
    async fn cancel(&self) -> AppResult<()> {
        let pid = *self.pid.lock();
        let Some(pid) = pid else { return Ok(()) };
        self.cancelled.store(true, Ordering::SeqCst);
        let session = self.session.lock().await.clone();
        super::jdbc::ssh_run(&session, &format!("kill {pid} 2>/dev/null; true"), None).await?;
        Ok(())
    }

    async fn close(&self) {
        let pid = self.pid.lock().take();
        if let Some(pid) = pid {
            let session = self.session.lock().await.clone();
            let _ = tokio::time::timeout(Duration::from_secs(5), super::jdbc::ssh_run(&session, &format!("kill {pid} 2>/dev/null; true"), None)).await;
        }
        if let Some(cli) = self.cli.lock().await.take() {
            let _ = cli.write.eof().await;
            let _ = cli.write.close().await;
        }
    }
}

// -------------------------------------------------------------------------------------------------
// Snapshot: a read-only copy

/// Where copies are kept: `<data>/sqlite-ssh/<random>/`, one folder per open connection.
fn snapshots_dir() -> AppResult<PathBuf> {
    Ok(super::jdbc::data_dir()?.join("sqlite-ssh"))
}

/// Folders left by a DevCat that didn't close them (a crash), after a week.
fn sweep(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > Duration::from_secs(7 * 86400));
        if old {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Copy `path` on the host to `dest`, streaming.
async fn download(session: &Session, path: &str, dest: &Path) -> AppResult<u64> {
    use russh::ChannelMsg;
    use tokio::io::AsyncWriteExt;
    let mut ch = session.channel_open_session().await.map_err(|e| AppError::msg(format!("SSH: {e}")))?;
    ch.exec(true, sh(&format!("exec cat {}", path_word(path)))).await.map_err(|e| AppError::msg(format!("SSH: {e}")))?;
    let mut file = tokio::fs::File::create(dest).await?;
    let (mut n, mut err, mut status) = (0u64, Vec::new(), None);
    while let Some(msg) = ch.wait().await {
        match msg {
            ChannelMsg::Data { data } => {
                n += data.len() as u64;
                file.write_all(&data).await?;
            }
            ChannelMsg::ExtendedData { data, .. } => err.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            ChannelMsg::Eof | ChannelMsg::Close => {}
            _ => {}
        }
    }
    file.flush().await?;
    if status != Some(0) {
        return Err(AppError::msg(format!("couldn't read {path}: {}", String::from_utf8_lossy(&err).trim())));
    }
    Ok(n)
}

/// Whether the copy is a whole database; folds its WAL in, so it opens read-only on its own.
async fn settle(file: &Path) -> AppResult<bool> {
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{ConnectOptions, Connection};
    let mut conn = SqliteConnectOptions::new().filename(file).connect().await?;
    let check: Vec<String> = sqlx::query_scalar("PRAGMA quick_check").fetch_all(&mut conn).await.unwrap_or_default();
    let ok = check.first().is_some_and(|s| s == "ok");
    if ok {
        let _ = sqlx::query("PRAGMA journal_mode = DELETE").execute(&mut conn).await;
    }
    let _ = conn.close().await;
    Ok(ok)
}

pub struct SnapshotDriver {
    inner: lite::SqliteDriver,
    dir: PathBuf,
    host: String,
    path: String,
    reason: String,
    taken: chrono::DateTime<chrono::Local>,
}

impl SnapshotDriver {
    async fn take(session: &Session, host: String, path: String, probe: &Probe, reason: String) -> AppResult<Self> {
        if let Some(size) = probe.size.filter(|s| *s > MAX_SNAPSHOT) {
            return Err(AppError::msg(format!(
                "{host} can't open {path} itself ({reason}), and at {} MB it's too big to copy here. Install sqlite3 3.26 or newer on {host} to open it there.",
                size >> 20
            )));
        }
        let base = snapshots_dir()?;
        std::fs::create_dir_all(&base)?;
        sweep(&base);
        let dir = base.join(uuid::Uuid::new_v4().simple().to_string());
        std::fs::create_dir_all(&dir)?;
        let file = dir.join("copy.sqlite");
        let result: AppResult<()> = async {
            for attempt in 1..=SNAPSHOT_TRIES {
                let _ = std::fs::remove_file(dir.join("copy.sqlite-wal"));
                download(session, &path, &file).await.map_err(|e| AppError::msg(format!("{host}: {e}")))?;
                if probe.wal {
                    let _ = download(session, &format!("{path}-wal"), &dir.join("copy.sqlite-wal")).await;
                }
                match settle(&file).await {
                    Ok(true) => return Ok(()),
                    Ok(false) if attempt < SNAPSHOT_TRIES => tokio::time::sleep(Duration::from_millis(300)).await,
                    Ok(false) => {
                        return Err(AppError::msg(format!(
                            "The copy of {path} from {host} isn't a whole database ({SNAPSHOT_TRIES} tries): something is writing to it, or it isn't a SQLite file."
                        )))
                    }
                    Err(e) => return Err(AppError::msg(format!("{path} from {host} isn't a SQLite database: {e}"))),
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        let local = ConnectionConfig {
            id: String::new(),
            name: String::new(),
            kind: ConnectionKind::Sqlite,
            host: String::new(),
            port: None,
            user: String::new(),
            password: None,
            database: None,
            file_path: Some(file.to_string_lossy().into_owned()),
            url: None,
            ssl_mode: None,
            ssh: None,
            color: None,
            group: None,
            project_id: None,
            options: HashMap::new(),
            read_only: true,
            mcp_access: None,
        };
        let inner = match lite::SqliteDriver::connect(&local).await {
            Ok(d) => d,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        Ok(SnapshotDriver { inner, dir, host, path, reason, taken: chrono::Local::now() })
    }

    fn read_only_error(&self) -> AppError {
        AppError::msg(format!(
            "This is a read-only copy of {} from {} ({}), so it can't be changed. Install sqlite3 3.26 or newer on {} to work on the file itself.",
            self.path, self.host, self.reason, self.host
        ))
    }
}

impl Drop for SnapshotDriver {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[async_trait]
impl Driver for SnapshotDriver {
    fn kind(&self) -> ConnectionKind {
        ConnectionKind::Sqlite
    }
    fn dialect(&self) -> Dialect {
        D
    }
    async fn server_version(&self) -> AppResult<String> {
        let v = self.inner.server_version().await?;
        Ok(format!(
            "{v}: read-only copy of {} from {}, taken {} ({}). Reconnect for a fresh copy.",
            self.path,
            self.host,
            self.taken.format("%Y-%m-%d %H:%M"),
            self.reason
        ))
    }
    async fn children(&self, path: &[String]) -> AppResult<Vec<TreeNode>> {
        let mut nodes = self.inner.children(path).await?;
        if path.is_empty() {
            for n in &mut nodes {
                if n.label == "main" {
                    n.detail = Some(format!("{}:{} (copy, {})", self.host, self.path, self.taken.format("%H:%M")));
                }
            }
        }
        Ok(nodes)
    }
    async fn list_databases(&self) -> AppResult<Vec<String>> {
        self.inner.list_databases().await
    }
    async fn execute(&self, script: &str, opts: &ExecOptions) -> AppResult<Vec<QueryResult>> {
        if sc::split_script(D, script).iter().any(|s| !sc::is_read_only_statement(D, &s.sql)) {
            return Err(self.read_only_error());
        }
        self.inner.execute(script, opts).await
    }
    async fn table_data(&self, req: &TableDataRequest) -> AppResult<TableDataPage> {
        self.inner.table_data(req).await
    }
    async fn table_structure(&self, table: &TableRef) -> AppResult<TableStructure> {
        self.inner.table_structure(table).await
    }
    async fn apply_changes(&self, _table: &TableRef, _changes: &[RowChange]) -> AppResult<u64> {
        Err(self.read_only_error())
    }
    async fn object_ddl(&self, table: &TableRef, kind: &NodeKind) -> AppResult<String> {
        self.inner.object_ddl(table, kind).await
    }
    async fn schema_snapshot(&self, database: Option<&str>, schema: Option<&str>) -> AppResult<SchemaSnapshot> {
        self.inner.schema_snapshot(database, schema).await
    }
    async fn cancel(&self) -> AppResult<()> {
        self.inner.cancel().await
    }
    async fn close(&self) {
        self.inner.close().await;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn quote_output() {
        let text = b"'x','y','z','w','n','big','r','u','v'\n'a\nb','it''s',9.9900000000000002131,X'00ff',NULL,9223372036854775807,1.0,unistr('\\u0001a\\u001b\\\\'),replace('c\\nd','\\n',char(10))\n";
        let Output::Rows { columns, rows } = parse_output(text) else { panic!() };
        assert_eq!(columns, vec!["x", "y", "z", "w", "n", "big", "r", "u", "v"]);
        assert_eq!(
            rows,
            vec![vec![json!("a\nb"), json!("it's"), json!(9.99), json!("\\x00ff"), json!(null), json!("9223372036854775807"), json!(1.0), json!("\u{1}a\u{1b}\\"), json!("c\nd")]]
        );
        assert_eq!(parse_output(b""), Output::Rows { columns: vec![], rows: vec![] });
        // nested replace (old versions: \r and \n)
        let Output::Rows { rows, .. } = parse_output(b"'a'\nreplace(replace('x\\ry\\nz','\\r',char(13)),'\\n',char(10))\n") else { panic!() };
        assert_eq!(rows[0][0], json!("x\ry\nz"));
        assert_eq!(parse_output(b"'a'\n-5,Inf\n"), Output::Rows { columns: vec!["a".into()], rows: vec![vec![json!(-5), json!("inf")]] });
    }

    #[test]
    fn errors() {
        assert_eq!(parse_output(b"Parse error near line 4: no such table: nope\n"), Output::Error("no such table: nope".into()));
        assert_eq!(parse_output(b"Error: near line 6: no such table: nope\n"), Output::Error("no such table: nope".into()));
        assert_eq!(parse_output(b"Runtime error near line 9: integer overflow\n"), Output::Error("integer overflow".into()));
        // rows, then an error
        assert_eq!(parse_output(b"'a'\n1\nRuntime error near line 3: boom\n"), Output::Error("boom".into()));
        // a value that merely starts a line with "Error:" is a value
        let Output::Rows { rows, .. } = parse_output(b"'a'\n'x\nError: y'\n") else { panic!() };
        assert_eq!(rows[0][0], json!("x\nError: y"));
    }

    #[test]
    fn statements_checked() {
        assert!(check_statement("select 1").is_ok());
        assert!(check_statement("select 'a.b', \"x\".y from t").is_ok());
        assert!(check_statement(".shell id").is_err());
        assert!(check_statement("select 1\n  .shell id").is_err());
        assert!(check_statement("-- c\n.system id").is_err());
        assert!(check_statement("select 'a\n.shell id'").is_err());
        assert!(check_statement("ATTACH '/etc/x' AS x").is_err());
        assert!(check_statement("select readfile('/etc/passwd')").is_err());
        assert!(check_statement("select \"writefile\"('/tmp/x', 'y')").is_err());
        assert!(check_statement("select * from fsdir('/')").is_err());
        assert!(check_statement("vacuum into '/tmp/x.db'").is_err());
        assert!(check_statement("select 'readfile' as name").is_ok());
        assert!(check_statement("select 1 -- readfile").is_ok());
        assert!(check_statement("select 1 as attached_at").is_ok());
    }

    #[test]
    fn termination() {
        assert_eq!(terminated("select 1;").unwrap(), "select 1;");
        assert_eq!(terminated("select 1").unwrap(), "select 1\n;");
        assert_eq!(terminated("select 1 -- c").unwrap(), "select 1 -- c\n;");
        assert!(terminated("select 'abc").is_err());
        assert!(terminated("select 1 /* x").is_err());
        let trig = "CREATE TRIGGER t AFTER INSERT ON a BEGIN SELECT 1; END";
        assert!(complete(&terminated(trig).unwrap()));
    }

    #[test]
    fn binding() {
        let sql = bind("SELECT a FROM t WHERE x = ?1 AND y = ?2 OR z = ?1 AND '?' = '?'", &[json!("it's\n.shell"), json!(5)], &|_| false);
        assert_eq!(sql, "SELECT a FROM t WHERE x = ('it''s'||char(10)||'.shell') AND y = 5 OR z = ('it''s'||char(10)||'.shell') AND '?' = '?'");
        assert!(check_statement(&sql).is_ok());
        let sql = bind("UPDATE t SET b = ?, c = ? WHERE id = ?", &[json!("\\x0102"), json!(null), json!(3)], &|i| i == 0);
        assert_eq!(sql, "UPDATE t SET b = X'0102', c = NULL WHERE id = 3");
    }

    #[test]
    fn paths_and_probe() {
        assert_eq!(path_word("/var/db/app.db"), "'/var/db/app.db'");
        assert_eq!(path_word("~/app.db"), "\"$HOME\"/'app.db'");
        assert_eq!(path_word("-x.db"), "'./-x.db'");
        assert_eq!(path_word("it's.db"), "'./it'\\''s.db'");
        let p = parse_probe("os=Linux\nkind=file\nwal=yes\nsize=4096\nsqlite=3.45.3 2024-04-15 13:34:05 abc (64-bit)\n");
        assert!(p.posix && p.wal && p.kind == FileKind::File && p.size == Some(4096));
        let v = p.sqlite.unwrap();
        assert!(v.at_least(MIN_LIVE) && v.at_least(MIN_SAFE));
        let p = parse_probe("os=\nkind=none\n");
        assert!(!p.posix && p.kind == FileKind::Missing && p.sqlite.is_none());
        assert!(!Version::parse("3.22.0 2018").unwrap().at_least(MIN_LIVE));
    }

    /// Against a real host. A throwaway sshd that lets a key in, e.g. a container with openssh and
    /// sqlite3. Env: `DEVCAT_TEST_SQLITE_SSH_PORT`, `DEVCAT_TEST_SQLITE_SSH_KEY`,
    /// `DEVCAT_TEST_SQLITE_SSH_USER` (and `DEVCAT_TEST_SQLITE_SSH_MODE`: `live` or `snapshot`, what the
    /// host should give).
    #[tokio::test]
    async fn over_ssh() {
        let (Ok(port), Ok(key), Ok(user)) =
            (std::env::var("DEVCAT_TEST_SQLITE_SSH_PORT"), std::env::var("DEVCAT_TEST_SQLITE_SSH_KEY"), std::env::var("DEVCAT_TEST_SQLITE_SSH_USER"))
        else {
            eprintln!("skipping: set DEVCAT_TEST_SQLITE_SSH_PORT, _KEY and _USER");
            return;
        };
        let live = std::env::var("DEVCAT_TEST_SQLITE_SSH_MODE").map(|m| m != "snapshot").unwrap_or(true);
        crate::db::ssh::init(std::env::temp_dir().join(format!("devcat-sqlite-ssh-{}", uuid::Uuid::new_v4())));
        crate::db::jdbc::init(std::env::temp_dir().join(format!("devcat-sqlite-ssh-data-{}", uuid::Uuid::new_v4())));
        let name = format!("devcat-test-{}.db", uuid::Uuid::new_v4().simple());
        let cfg = |path: &str, create: bool, read_only: bool| ConnectionConfig {
            id: "t".into(),
            name: "t".into(),
            kind: ConnectionKind::Sqlite,
            host: String::new(),
            port: None,
            user: String::new(),
            password: None,
            database: None,
            file_path: Some(path.into()),
            url: None,
            ssl_mode: None,
            ssh: Some(SshConfig {
                enabled: true,
                host: "127.0.0.1".into(),
                port: port.parse().unwrap(),
                user: user.clone(),
                auth: "key".into(),
                key_path: Some(key.clone()),
                password: None,
            }),
            color: None,
            group: None,
            project_id: None,
            options: if create { [("create".to_string(), "true".to_string())].into_iter().collect() } else { HashMap::new() },
            read_only,
            mcp_access: None,
        };
        let o = || ExecOptions { database: None, schema: None, max_rows: None };
        let path = format!("~/{name}");
        // a missing file isn't made without "create"
        let e = connect(&cfg(&path, false, false)).await.err().unwrap().to_string();
        assert!(e.contains("No file"), "{e}");
        if !live {
            // the snapshot host has no sqlite3: the test put a database there as `seed.db`
            let d = connect(&cfg("~/seed.db", false, false)).await.unwrap();
            let v = d.server_version().await.unwrap();
            assert!(v.contains("read-only copy"), "{v}");
            let r = d.execute("select name from authors order by id", &o()).await.unwrap();
            assert_eq!(r[0].rows[0][0], json!("Ann"));
            let e = d.execute("delete from authors", &o()).await.unwrap_err().to_string();
            assert!(e.contains("read-only copy"), "{e}");
            d.close().await;
            return;
        }
        let d = connect(&cfg(&path, true, false)).await.unwrap();
        let v = d.server_version().await.unwrap();
        assert!(v.starts_with("SQLite 3.") && v.contains("over SSH"), "{v}");
        d.execute(
            "CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL, born DATE, pic BLOB);
             INSERT INTO authors (name, born, pic) VALUES ('Ann', '1970-01-02', X'00ff'), ('B''ob
.shell id', NULL, NULL);
             CREATE VIEW v AS SELECT id FROM authors;",
            &o(),
        )
        .await
        .err()
        .map(|e| assert!(e.to_string().contains("isn't sent"), "{e}"))
        .expect("a line starting with . is refused");
        d.execute(
            "CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL, born DATE, pic BLOB, big INTEGER);
             INSERT INTO authors (name, born, pic, big) VALUES ('Ann', '1970-01-02', X'00ff', 9223372036854775807), ('B''ob' || char(10) || '.x', NULL, NULL, 1);
             CREATE VIEW v AS SELECT id FROM authors;",
            &o(),
        )
        .await
        .unwrap();
        let r = d.execute("select * from authors order by id; select 1 where 0; select 2 as two", &o()).await.unwrap();
        assert_eq!(r[0].columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["id", "name", "born", "pic", "big"]);
        assert_eq!(r[0].rows[0], vec![json!(1), json!("Ann"), json!("1970-01-02"), json!("\\x00ff"), json!("9223372036854775807")]);
        assert_eq!(r[0].rows[1][1], json!("B'ob\n.x"));
        assert_eq!(r[1].columns.len(), 1);
        assert!(r[1].rows.is_empty());
        assert_eq!(r[2].rows[0][0], json!(2));
        let r = d.execute("update authors set born = '2000-01-01' where id = 2", &o()).await.unwrap();
        assert_eq!(r[0].rows_affected, Some(1));
        let e = d.execute("select 1;\nselect * from nope", &o()).await.unwrap_err().to_string();
        assert!(e.contains("statement 2 of 2") && e.contains("no such table"), "{e}");
        let e = d.execute("select readfile('/etc/passwd')", &o()).await.unwrap_err().to_string();
        assert!(e.contains("other files"), "{e}");
        // truncation on the host
        let r = d
            .execute("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 5000000) SELECT x FROM c", &ExecOptions { database: None, schema: None, max_rows: Some(100) })
            .await
            .unwrap();
        assert!(r[0].truncated && r[0].rows.len() == 100);
        // temp tables and transactions carry over
        d.execute("create temp table t(a); begin; insert into t values (1)", &o()).await.unwrap();
        d.execute("rollback", &o()).await.unwrap();
        assert_eq!(d.execute("select count(*) from t", &o()).await.unwrap()[0].rows[0][0], json!(0));
        // tree, structure, ddl, data, edits
        let tables = d.children(&["main".into(), "tables".into()]).await.unwrap();
        assert_eq!(tables[0].label, "authors");
        let authors = TableRef { database: Some("main".into()), schema: None, name: "authors".into() };
        let st = d.table_structure(&authors).await.unwrap();
        assert!(st.columns[0].is_primary_key && st.columns[0].auto_increment);
        assert!(d.object_ddl(&authors, &NodeKind::Table).await.unwrap().starts_with("CREATE TABLE authors"));
        assert_eq!(d.schema_snapshot(None, None).await.unwrap().views.len(), 1);
        let page = d.table_data(&TableDataRequest { table: authors.clone(), offset: 0, limit: 1, filter: None, sort: vec![] }).await.unwrap();
        assert_eq!(page.total, Some(2));
        assert_eq!(page.primary_key, vec!["id"]);
        assert_eq!(page.result.rows.len(), 1);
        let mut ins = serde_json::Map::new();
        ins.insert("name".into(), json!("Cid\n.shell id"));
        ins.insert("pic".into(), json!("\\x0102"));
        assert_eq!(d.apply_changes(&authors, &[RowChange::Insert { values: ins }]).await.unwrap(), 1);
        let r = d.execute("select name, pic from authors where id = 3", &o()).await.unwrap();
        assert_eq!(r[0].rows[0], vec![json!("Cid\n.shell id"), json!("\\x0102")]);
        let key = |id: i64| -> serde_json::Map<String, Value> { [("id".to_string(), json!(id))].into_iter().collect() };
        let e = d.apply_changes(&authors, &[RowChange::Delete { key: key(3) }, RowChange::Delete { key: key(99) }]).await.unwrap_err().to_string();
        assert!(e.contains("affected 0 rows"), "{e}");
        assert_eq!(d.execute("select count(*) from authors", &o()).await.unwrap()[0].rows[0][0], json!(3), "rolled back");
        // cancel a long statement: the process is stopped and starts again
        let d = Arc::new(d);
        let d2 = d.clone();
        let h = tokio::spawn(async move { d2.execute("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c", &o()).await });
        tokio::time::sleep(Duration::from_millis(800)).await;
        d.cancel().await.unwrap();
        let res = tokio::time::timeout(Duration::from_secs(10), h).await.expect("cancel took too long").unwrap();
        assert!(res.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(d.execute("select count(*) from authors", &o()).await.unwrap()[0].rows[0][0], json!(3));
        d.close().await;
        // read-only: refused by DevCat and by the host's sqlite3
        let ro = connect(&cfg(&path, false, true)).await.unwrap();
        assert!(ro.execute("delete from authors", &o()).await.is_err());
        assert_eq!(ro.execute("select count(*) from authors", &o()).await.unwrap()[0].rows[0][0], json!(3));
        ro.close().await;
        // a folder isn't a database
        let e = connect(&cfg("/tmp", false, false)).await.err().unwrap().to_string();
        assert!(e.contains("folder"), "{e}");
    }
}
