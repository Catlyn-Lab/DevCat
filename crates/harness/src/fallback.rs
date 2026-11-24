//! A second model for when the first one's provider fails: [`Fallback`] takes each step with the
//! primary model, and when its provider is down, out of quota or refuses the key (after its own
//! retries, see [`crate::llm`]), takes that step with the backup instead. The primary then sits
//! out for a while ([`FIRST_BENCH`], doubling up to [`MAX_BENCH`] while it keeps failing) so each
//! step doesn't wait for it to fail again, and gets the next step after that.
//!
//! What it never falls back for: a conversation too long for the window (summarizing fixes that,
//! and the backup's window may well be smaller), a stop, and a request the provider read and
//! refused (400: the backup would refuse it too). Switching costs the provider's prompt cache.
//! (After Hermes Agent's `fallback_providers`, MIT.)
use crate::agent::Model;
use crate::llm::{Delta, Reply, Request, is_overflow};
use crate::{Error, Result};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a primary that just failed sits out.
pub const FIRST_BENCH: Duration = Duration::from_secs(60);
/// The longest it sits out.
pub const MAX_BENCH: Duration = Duration::from_secs(60 * 60);

pub struct Fallback {
    primary: Arc<dyn Model>,
    backup: Arc<dyn Model>,
    /// Until when the primary sits out, and for how long the next time it fails.
    bench: Mutex<(Option<Instant>, Duration)>,
}

impl Fallback {
    pub fn new(primary: Arc<dyn Model>, backup: Arc<dyn Model>) -> Fallback {
        Fallback { primary, backup, bench: Mutex::new((None, FIRST_BENCH)) }
    }

    fn benched(&self) -> bool {
        self.bench.lock().unwrap_or_else(|e| e.into_inner()).0.is_some_and(|until| Instant::now() < until)
    }
}

/// Whether the backup could take a step that failed with `e`: the provider is down or overloaded
/// (5xx, 429, 529 after the retries), out of credit (402), refuses the key or the model (401, 403,
/// 404), or couldn't be reached at all.
pub fn worth_falling_back(e: &Error) -> bool {
    match e {
        Error::Io(_) => true,
        Error::Provider(_) if is_overflow(e) => false,
        Error::Provider(m) => m.get(..3).and_then(|c| c.parse::<u16>().ok()).is_some_and(|c| matches!(c, 401 | 402 | 403 | 404 | 408 | 429 | 529) || c >= 500),
        _ => false,
    }
}

#[async_trait]
impl Model for Fallback {
    /// The model taking the steps now.
    fn name(&self) -> String {
        if self.benched() { self.backup.name() } else { self.primary.name() }
    }
    fn max_output(&self) -> u64 {
        if self.benched() { self.backup.max_output() } else { self.primary.max_output() }
    }
    async fn step(&self, req: &Request<'_>, on: &mut (dyn FnMut(Delta) + Send)) -> Result<Reply> {
        if !self.benched() {
            match self.primary.step(req, on).await {
                Ok(r) => {
                    *self.bench.lock().unwrap_or_else(|e| e.into_inner()) = (None, FIRST_BENCH);
                    return Ok(r);
                }
                Err(e) if worth_falling_back(&e) => {
                    {
                        let mut b = self.bench.lock().unwrap_or_else(|e| e.into_inner());
                        let wait = b.1;
                        *b = (Some(Instant::now() + wait), (wait * 2).min(MAX_BENCH));
                    }
                    // what the primary streamed before it failed is void: the step comes again
                    let why: String = e.to_string().chars().take(200).collect();
                    on(Delta::Retry { attempt: 1, of: 1, reason: format!("{why}; {} takes over for now", self.backup.name()) });
                }
                Err(e) => return Err(e),
            }
        }
        self.backup.step(req, on).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Block, Usage};

    /// Answers with its name, or fails with `fail` while it's set.
    struct M(&'static str, Mutex<Option<String>>, Mutex<u32>);

    #[async_trait]
    impl Model for M {
        fn name(&self) -> String {
            self.0.into()
        }
        async fn step(&self, _: &Request<'_>, _: &mut (dyn FnMut(Delta) + Send)) -> Result<Reply> {
            *self.2.lock().unwrap() += 1;
            match self.1.lock().unwrap().clone() {
                Some(m) if m == "io" => Err(Error::Io("connection refused".into())),
                Some(m) => Err(Error::Provider(m)),
                None => Ok(Reply { content: vec![Block::Text { text: self.0.into() }], usage: Usage::default(), truncated: false }),
            }
        }
    }

    fn m(name: &'static str, fail: Option<&str>) -> Arc<M> {
        Arc::new(M(name, Mutex::new(fail.map(String::from)), Mutex::new(0)))
    }

    async fn ask(f: &Fallback) -> (Result<String>, Vec<String>) {
        let req = Request { system: "", messages: &[], tools: &[], cache_key: None };
        let mut said = Vec::new();
        let r = f.step(&req, &mut |d| if let Delta::Retry { reason, .. } = d { said.push(reason) }).await;
        (r.map(|r| if let Block::Text { text } = &r.content[0] { text.clone() } else { String::new() }), said)
    }

    #[tokio::test]
    async fn the_backup_takes_over_while_the_primary_fails() {
        let (a, b) = (m("a", Some("503 overloaded")), m("b", None));
        let f = Fallback::new(a.clone(), b.clone());
        let (r, said) = ask(&f).await;
        assert_eq!(r.unwrap(), "b");
        assert!(said[0].contains("503 overloaded") && said[0].contains("b takes over"), "{said:?}");
        // the primary sits out: the next step goes straight to the backup
        assert_eq!(f.name(), "b");
        assert_eq!(ask(&f).await.0.unwrap(), "b");
        assert_eq!(*a.2.lock().unwrap(), 1);
        // once its time is up, it's asked again
        f.bench.lock().unwrap().0 = Some(Instant::now());
        *a.1.lock().unwrap() = None;
        assert_eq!(ask(&f).await.0.unwrap(), "a");
        assert_eq!(f.bench.lock().unwrap().1, FIRST_BENCH);
    }

    #[tokio::test]
    async fn some_failures_are_not_the_providers() {
        for fail in ["400 invalid tool schema", "400 prompt is too long: 250000 tokens > 200000 maximum"] {
            let f = Fallback::new(m("a", Some(fail)), m("b", None));
            assert!(ask(&f).await.0.is_err(), "{fail}");
        }
        for fail in ["401 invalid x-api-key", "429 rate limited", "402 insufficient credits", "io"] {
            let f = Fallback::new(m("a", Some(fail)), m("b", None));
            assert_eq!(ask(&f).await.0.unwrap(), "b", "{fail}");
        }
        assert!(!worth_falling_back(&Error::Cancelled));
    }

    #[tokio::test]
    async fn a_primary_that_keeps_failing_sits_out_longer() {
        let f = Fallback::new(m("a", Some("500 boom")), m("b", None));
        for n in 0..10 {
            f.bench.lock().unwrap().0 = None;
            ask(&f).await.0.unwrap();
            let wait = f.bench.lock().unwrap().1;
            assert_eq!(wait, (FIRST_BENCH * 2u32.pow(n + 1)).min(MAX_BENCH));
        }
        // when both fail, the backup's error is the step's
        let f = Fallback::new(m("a", Some("500 boom")), m("b", Some("503 also down")));
        assert!(ask(&f).await.0.unwrap_err().to_string().contains("also down"));
    }
}
