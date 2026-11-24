//! What keeps a model that's stuck from spending a whole turn on it. The agent loop asks
//! [`Guard::check`] before each call and tells it each result ([`Guard::record`]); the guard
//! refuses a call that can't help, or adds a note to a result the model should think about.
//!
//! The rules look at what calls *did*, not only at what they asked for: running the tests again
//! after an edit is work, running them again with nothing changed in between and getting the same
//! output is a loop. So most rules need the same call to come back with the same result.
//!
//! - **The same call in a row** ([`MAX_SAME_CALL`]): the fourth identical call in a row is refused,
//!   whatever it returned.
//! - **No progress**: a call that returned exactly the same result [`NO_PROGRESS_NOTE`] times in
//!   a row gets a note saying so, and after [`NO_PROGRESS_STOP`] times it isn't run again.
//! - **A cycle**: a few calls made over and over in the same order (period 2 to [`MAX_PERIOD`]),
//!   each returning what it did the time before, are refused when they'd start the
//!   [`CYCLE_ROUNDS`]th round.
//! - **The same failure**: a call that fails with the same error a second time gets a note to try
//!   another way.
//!
//! (Ideas from Hermes Agent's `tool_guardrails.py`, MIT.)
use serde_json::Value;
use std::collections::HashMap;

/// The same call (tool and arguments) this many times in a row runs; the next one is refused.
pub const MAX_SAME_CALL: usize = 3;

/// A call that returned the same result this many times in a row: its result says so.
pub const NO_PROGRESS_NOTE: u32 = 2;

/// ... and after this many, it isn't run again.
pub const NO_PROGRESS_STOP: u32 = 3;

/// Longest run of calls checked for repeating itself.
pub const MAX_PERIOD: usize = 4;

/// Rounds of a cycle (with nothing new in any of its results) after which it's refused.
pub const CYCLE_ROUNDS: usize = 3;

/// One call: its tool and arguments, hashed.
type Key = u64;

fn hash(parts: &[&str]) -> u64 {
    // FNV-1a: stable, and enough to tell calls apart within one turn
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes().chain([0xff]) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

fn key(name: &str, input: &Value) -> Key {
    hash(&[name, &input.to_string()])
}

/// The guard of one turn.
#[derive(Default)]
pub struct Guard {
    /// The last call checked and how many times in a row it came.
    last: Option<Key>,
    same: usize,
    /// The calls that ran, in order, with whether each returned what that call returned the time
    /// before (nothing new).
    ran: Vec<(Key, bool)>,
    /// Per call: a hash of its last result, and how many times in a row before that it came back
    /// the same (0: it was new).
    results: HashMap<Key, (u64, u32)>,
}

impl Guard {
    /// Start over: after a summary the model may need to look at things again.
    pub fn reset(&mut self) {
        *self = Guard::default();
    }

    /// Before a call runs: why it won't (for the model), or None. Decided for all the calls of a
    /// step before any runs, in order.
    pub fn check(&mut self, name: &str, input: &Value) -> Option<String> {
        let k = key(name, input);
        self.same = if self.last == Some(k) { self.same + 1 } else { 1 };
        self.last = Some(k);
        if self.same > MAX_SAME_CALL {
            return Some(format!(
                "Not run: you already made this exact `{name}` call {MAX_SAME_CALL} times in a row. Doing it again won't help: try something different, or tell the user what's in the way."
            ));
        }
        if self.results.get(&k).is_some_and(|(_, n)| *n + 1 >= NO_PROGRESS_STOP) {
            return Some(format!(
                "Not run: this exact `{name}` call returned the same result the last {NO_PROGRESS_STOP} times, and nothing it depends on changed in between. Running it again would return it again: change something first, try another way, or tell the user what's in the way."
            ));
        }
        if let Some(p) = self.cycle(k) {
            return Some(format!(
                "Not run: you've been making the same {p} calls in the same order, round after round, and each returned what it did the round before. Another round would too: step back, use what you already have, or tell the user what's in the way."
            ));
        }
        None
    }

    /// The period of the cycle `next` would carry on into its [`CYCLE_ROUNDS`]th round, if any:
    /// the calls before it repeat with that period, none of them got anything new, and the cycle
    /// has more than one call (one call over and over is [`MAX_SAME_CALL`]'s).
    fn cycle(&self, next: Key) -> Option<usize> {
        (2..=MAX_PERIOD).find(|&p| {
            let need = p * CYCLE_ROUNDS - 1;
            if self.ran.len() < need {
                return false;
            }
            let tail = &self.ran[self.ran.len() - need..];
            let at = |i: usize| if i < need { tail[i].0 } else { next };
            let periodic = (p..=need).all(|i| at(i) == at(i - p));
            // from the second round on, each call's result was what it had been
            let stale = tail[p..].iter().all(|(_, same)| *same);
            let distinct = (1..p).any(|i| at(i) != at(0));
            periodic && stale && distinct
        })
    }

    /// After a call ran: a note to add to its result, or None.
    pub fn record(&mut self, name: &str, input: &Value, content: &str, is_error: bool) -> Option<String> {
        let k = key(name, input);
        let r = hash(&[content, if is_error { "e" } else { "o" }]);
        let repeats = match self.results.get(&k) {
            Some((prev, n)) if *prev == r => n + 1,
            _ => 0,
        };
        self.results.insert(k, (r, repeats));
        self.ran.push((k, repeats > 0));
        if self.ran.len() > 64 {
            self.ran.remove(0);
        }
        if repeats + 1 >= NO_PROGRESS_NOTE && !is_error {
            return Some(format!(
                "[Note: this is what this exact call returned the last time too ({} times in a row). If you're waiting for something to change, change it first; making the same call again won't.]",
                repeats + 1
            ));
        }
        if is_error && repeats >= 1 {
            return Some("[Note: this call failed with this same error before. Don't make it again as it is: read the error, change the call or what it depends on, or try another way.]".into());
        }
        None
    }
}

/// Whether `text` is mostly the same piece over and over (a model stuck repeating itself until the
/// output limit): a line of 20 characters or more, or the text's last 60 characters, coming back
/// so often that the copies make up at least 40% of it. Short texts never are.
pub fn repeating(text: &str) -> bool {
    let len = text.len();
    if len < 1_500 {
        return false;
    }
    let mut lines: HashMap<&str, usize> = HashMap::new();
    for l in text.lines().map(str::trim).filter(|l| l.len() >= 20) {
        *lines.entry(l).or_default() += 1;
    }
    if lines.iter().any(|(l, n)| *n >= 6 && l.len() * n * 10 >= len * 4) {
        return true;
    }
    let t = text.trim_end();
    let mut at = t.len().saturating_sub(60);
    while !t.is_char_boundary(at) {
        at += 1;
    }
    let tail = &t[at..];
    tail.len() >= 40 && {
        let n = t.matches(tail).count();
        n >= 6 && tail.len() * n * 10 >= len * 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_stuck_repeating_itself_is_told_apart_from_long_text() {
        let line = "Let me check the configuration file again to be sure.\n";
        assert!(repeating(&format!("Some start.\n{}", line.repeat(40))));
        // without line breaks
        assert!(repeating(&format!("Intro. {}", "and then the value is read again from the cache ".repeat(50))));
        // long, varied text isn't
        let varied: String = (0..300).map(|i| format!("Step {i}: changed function number {i} in module {}.\n", i * 7)).collect();
        assert!(!repeating(&varied));
        // a table with a repeated separator row isn't either: it's a small part of the text
        let table: String = (0..60).map(|i| format!("| row {i} | value {} | another column here |\n|---|---|---|\n", i * 3)).collect();
        assert!(!repeating(&table));
        assert!(!repeating(&line.repeat(5)));
    }

    fn run(g: &mut Guard, name: &str, input: Value, out: &str) -> Result<Option<String>, String> {
        if let Some(why) = g.check(name, &input) {
            return Err(why);
        }
        Ok(g.record(name, &input, out, false))
    }

    #[test]
    fn the_same_call_in_a_row_is_refused_after_three() {
        let mut g = Guard::default();
        let a = json!({ "path": "a" });
        // results that differ each time: only the row counts
        for n in 0..3 {
            assert!(run(&mut g, "read", a.clone(), &n.to_string()).is_ok());
        }
        assert!(run(&mut g, "read", a.clone(), "3").unwrap_err().contains("3 times in a row"));
        // something else in between starts the count again
        assert!(run(&mut g, "glob", json!({}), "x").is_ok());
        assert!(run(&mut g, "read", a, "4").is_ok());
    }

    #[test]
    fn a_call_that_keeps_returning_the_same_gets_a_note_then_is_refused() {
        let mut g = Guard::default();
        let test = || json!({ "command": "cargo test" });
        let other = |n: i32| json!({ "path": format!("f{n}") });
        // the tests, an edit, the tests again: different output is progress
        assert_eq!(run(&mut g, "shell", test(), "1 failed"), Ok(None));
        assert_eq!(run(&mut g, "edit", other(0), "ok"), Ok(None));
        assert_eq!(run(&mut g, "shell", test(), "2 failed"), Ok(None));
        // the same output again, with other calls in between: a note
        assert_eq!(run(&mut g, "read", other(1), "x"), Ok(None));
        assert!(run(&mut g, "shell", test(), "2 failed").unwrap().unwrap().contains("2 times in a row"));
        assert_eq!(run(&mut g, "read", other(2), "x"), Ok(None));
        assert!(run(&mut g, "shell", test(), "2 failed").unwrap().is_some());
        assert_eq!(run(&mut g, "read", other(3), "x"), Ok(None));
        // and then it's no use
        assert!(run(&mut g, "shell", test(), "2 failed").unwrap_err().contains("same result"));
    }

    #[test]
    fn a_cycle_with_nothing_new_is_refused() {
        let mut g = Guard::default();
        let (a, b) = (json!({ "path": "a" }), json!({ "path": "b" }));
        for _ in 0..2 {
            assert!(run(&mut g, "read", a.clone(), "A").is_ok());
            assert!(run(&mut g, "read", b.clone(), "B").is_ok());
        }
        assert!(run(&mut g, "read", a.clone(), "A").is_ok());
        // the third round would be the same as the two before
        assert!(run(&mut g, "read", b, "B").unwrap_err().contains("same 2 calls in the same order"));

        // with something new in a result, it's work, not a cycle
        let mut g = Guard::default();
        let t = json!({ "command": "test" });
        for n in 0..4 {
            assert!(run(&mut g, "shell", t.clone(), &format!("{n} failed")).is_ok());
            assert!(run(&mut g, "edit", json!({ "path": "x", "new_string": n }), "ok").is_ok());
        }
    }

    #[test]
    fn the_same_failure_twice_gets_a_note() {
        let mut g = Guard::default();
        let i = json!({ "command": "make" });
        assert!(g.check("shell", &i).is_none());
        assert_eq!(g.record("shell", &i, "no rule to make target", true), None);
        assert!(g.check("shell", &i).is_none());
        assert!(g.record("shell", &i, "no rule to make target", true).unwrap().contains("failed with this same error"));
        // another error is something new
        assert!(g.check("glob", &json!({})).is_none());
        assert!(g.check("shell", &i).is_none());
        assert_eq!(g.record("shell", &i, "permission denied", true), None);
    }
}
