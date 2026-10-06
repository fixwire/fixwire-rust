//! Client budgets: a crash loop costs a few events and a count, not the
//! quota. Each issue (a cheap fingerprint of the event) may send a burst,
//! then so many a minute, within a budget for all of them; occurrences held
//! back are counted and ride on the issue's next event
//! (`fixwire.suppressed`), so issue counts stay right. The server's grouping
//! is the real one; the fingerprint only drives the budgets.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use regex::Regex;

use crate::hub::lock;
use crate::options::ErrorBudget;
use crate::types::Event;

/// The issues remembered (the least recently seen go first), and the in-app
/// frames that name one.
const MAX_ISSUES: usize = 1024;
const TOP_FRAMES: usize = 5;
/// The bytes of a message read for its fingerprint: finding every match can
/// take time quadratic in the text, and the start tells issues apart.
const MAX_MESSAGE: usize = 1024;

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
    suppressed: u64,
    seen: Instant,
}

impl Bucket {
    fn full(tokens: f64, now: Instant) -> Bucket {
        Bucket {
            tokens,
            updated: now,
            suppressed: 0,
            seen: now,
        }
    }

    /// Refills at `per_minute` up to `burst`, and takes a token.
    fn take(&mut self, burst: f64, per_minute: f64, now: Instant) -> bool {
        let minutes = now.saturating_duration_since(self.updated).as_secs_f64() / 60.0;
        self.tokens = burst.min(self.tokens + minutes * per_minute);
        self.updated = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub(crate) struct Budget {
    opts: ErrorBudget,
    state: Mutex<(HashMap<u64, Bucket>, Bucket)>,
}

impl Budget {
    pub(crate) fn new(opts: ErrorBudget) -> Budget {
        let opts = ErrorBudget {
            per_issue_burst: if opts.per_issue_burst == 0 {
                10
            } else {
                opts.per_issue_burst
            },
            per_issue_per_minute: if opts.per_issue_per_minute > 0.0 {
                opts.per_issue_per_minute
            } else {
                1.0
            },
            per_minute: if opts.per_minute > 0.0 {
                opts.per_minute
            } else {
                600.0
            },
            disabled: opts.disabled,
        };
        let all = Bucket::full(opts.per_minute, Instant::now());
        Budget {
            opts,
            state: Mutex::new((HashMap::new(), all)),
        }
    }

    /// Whether an event of the issue may be sent, and the occurrences held
    /// back since the last one sent.
    pub(crate) fn allow(&self, issue: u64, now: Instant) -> Option<u64> {
        if self.opts.disabled {
            return Some(0);
        }
        let burst = f64::from(self.opts.per_issue_burst);
        let mut guard = lock(&self.state);
        let (issues, all) = &mut *guard;
        if !issues.contains_key(&issue)
            && issues.len() >= MAX_ISSUES
            && let Some(oldest) = issues.iter().min_by_key(|(_, b)| b.seen).map(|(k, _)| *k)
        {
            issues.remove(&oldest);
        }
        let bucket = issues
            .entry(issue)
            .or_insert_with(|| Bucket::full(burst, now));
        bucket.seen = now;
        if bucket.take(burst, self.opts.per_issue_per_minute, now)
            && all.take(self.opts.per_minute, self.opts.per_minute, now)
        {
            Some(std::mem::take(&mut bucket.suppressed))
        } else {
            bucket.suppressed += 1;
            None
        }
    }
}

/// The parts of a message that change between occurrences: hex, UUIDs, long
/// hex ids, numbers and emails.
static VARIABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b0x[0-9a-fA-F]+\b|\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b|\b[0-9a-fA-F]{16,}\b|[0-9]+(?:\.[0-9]+)?|\S+@\S+\.\w+",
    )
    .expect("a valid pattern")
});

/// The event's fingerprint for the budgets: its exception types and top
/// in-app frames (or its message without the parts that vary), and its
/// custom fingerprint.
pub(crate) fn issue_of(e: &Event) -> u64 {
    let mut parts: Vec<String> = Vec::new();
    match e.exceptions.first() {
        Some(outer) => {
            parts.extend(e.exceptions.iter().map(|x| x.ty.clone()));
            let app: Vec<_> = outer.frames.iter().filter(|f| f.in_app).collect();
            let frames: Vec<_> = if app.is_empty() {
                outer.frames.iter().collect()
            } else {
                app
            };
            let top = frames.len().saturating_sub(TOP_FRAMES);
            parts.extend(
                frames[top..]
                    .iter()
                    .map(|f| format!("{}|{}", f.module.as_deref().unwrap_or_default(), f.function)),
            );
            if outer.frames.is_empty() {
                parts.push(
                    VARIABLE
                        .replace_all(head(&outer.message), "<*>")
                        .into_owned(),
                );
            }
        }
        None => parts.push(
            VARIABLE
                .replace_all(head(e.message.as_deref().unwrap_or_default()), "<*>")
                .into_owned(),
        ),
    }
    if !e.fingerprint.is_empty() {
        parts.push(e.fingerprint.join("\x1f"));
    }
    fnv1a(parts.join("\x1e").as_bytes())
}

/// The first `MAX_MESSAGE` bytes of a message, cut on a character boundary.
fn head(message: &str) -> &str {
    crate::limits::head(message, MAX_MESSAGE)
}

/// FNV-1a, 64 bits: cheap, and stable across runs (unlike `DefaultHasher`).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::types::Exception;

    #[test]
    fn a_crash_loop_costs_a_burst_and_a_count() {
        let b = Budget::new(ErrorBudget::default());
        let t0 = Instant::now();
        let sent = (0..50).filter(|_| b.allow(1, t0).is_some()).count();
        assert_eq!(sent, 10);
        // A minute later one more goes, carrying the 40 held back.
        assert_eq!(b.allow(1, t0 + Duration::from_secs(61)), Some(40));
        assert_eq!(b.allow(2, t0), Some(0), "another issue has its own budget");
    }

    #[test]
    fn fingerprints_ignore_what_varies() {
        let msg = |m: &str| Event {
            message: Some(m.into()),
            ..Event::default()
        };
        assert_eq!(
            issue_of(&msg("order 17 failed for ada@example.com")),
            issue_of(&msg("order 4521 failed for bo@example.org"))
        );
        assert_ne!(
            issue_of(&msg("order 17 failed")),
            issue_of(&msg("payment 17 failed"))
        );
        let err = |ty: &str| Event {
            exceptions: vec![Exception {
                ty: ty.into(),
                ..Exception::default()
            }],
            ..Event::default()
        };
        assert_ne!(issue_of(&err("ParseIntError")), issue_of(&err("Declined")));
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn hostile_messages_take_linear_time() {
        // Each match makes the regex scan to the end of the text: quadratic without a bound.
        for message in ["1g".repeat(100_000), "1\u{e9}".repeat(70_000)] {
            let start = Instant::now();
            let id = issue_of(&Event {
                message: Some(message.clone()),
                ..Event::default()
            });
            assert!(start.elapsed() < Duration::from_secs(1));
            let longer = Event {
                message: Some(message + "tail"),
                ..Event::default()
            };
            assert_eq!(issue_of(&longer), id, "the start names the issue");
        }
        assert_eq!(head("\u{e9}".repeat(600).as_str()).len(), 1024);
    }
}
