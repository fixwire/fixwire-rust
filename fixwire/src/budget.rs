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
}

impl Bucket {
    fn full(tokens: f64, now: Instant) -> Bucket {
        Bucket {
            tokens,
            updated: now,
            suppressed: 0,
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

/// No slot: the end of the list.
const NONE: usize = usize::MAX;

/// An issue's bucket, linked by index to the issues seen just before and
/// after it.
struct Slot {
    issue: u64,
    bucket: Bucket,
    newer: usize,
    older: usize,
}

/// The issues remembered, in the order they were last seen: a lookup, a
/// move to the front and forgetting the least recently seen take constant
/// time.
struct Issues {
    slots: Vec<Slot>,
    index: HashMap<u64, usize>,
    newest: usize,
    oldest: usize,
}

impl Issues {
    fn new() -> Issues {
        Issues {
            slots: Vec::new(),
            index: HashMap::new(),
            newest: NONE,
            oldest: NONE,
        }
    }

    /// The issue's bucket, now the most recently seen; a new issue past
    /// `MAX_ISSUES` takes the least recently seen one's slot.
    fn seen(&mut self, issue: u64, new: Bucket) -> &mut Bucket {
        let at = if let Some(&at) = self.index.get(&issue) {
            self.unlink(at);
            at
        } else if self.slots.len() < MAX_ISSUES {
            self.slots.push(Slot {
                issue,
                bucket: new,
                newer: NONE,
                older: NONE,
            });
            self.index.insert(issue, self.slots.len() - 1);
            self.slots.len() - 1
        } else {
            let at = self.oldest;
            self.unlink(at);
            let slot = &mut self.slots[at];
            self.index.remove(&slot.issue);
            slot.issue = issue;
            slot.bucket = new;
            self.index.insert(issue, at);
            at
        };
        self.slots[at].older = self.newest;
        match self.newest {
            NONE => self.oldest = at,
            newest => self.slots[newest].newer = at,
        }
        self.newest = at;
        &mut self.slots[at].bucket
    }

    /// Takes the slot out of the list.
    fn unlink(&mut self, at: usize) {
        let Slot { newer, older, .. } = self.slots[at];
        match newer {
            NONE => self.newest = older,
            newer => self.slots[newer].older = older,
        }
        match older {
            NONE => self.oldest = newer,
            older => self.slots[older].newer = newer,
        }
        self.slots[at].newer = NONE;
        self.slots[at].older = NONE;
    }
}

pub(crate) struct Budget {
    opts: ErrorBudget,
    state: Mutex<(Issues, Bucket)>,
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
            state: Mutex::new((Issues::new(), all)),
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
        let bucket = issues.seen(issue, Bucket::full(burst, now));
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
    fn the_least_recently_seen_issue_is_forgotten_first() {
        // One event an issue: a remembered issue holds the next back, a
        // forgotten one starts afresh.
        let b = Budget::new(ErrorBudget {
            per_issue_burst: 1,
            per_minute: 1e9,
            ..ErrorBudget::default()
        });
        let t0 = Instant::now();
        let mut tick = 0;
        let mut allow = |issue: u64| {
            tick += 1;
            b.allow(issue, t0 + Duration::from_micros(tick))
        };
        let max = MAX_ISSUES as u64;
        for issue in 0..max {
            assert_eq!(allow(issue), Some(0));
        }
        for issue in 0..max {
            assert_eq!(allow(issue), None, "issue {issue} remembered");
        }
        // Seen again, 0 leaves 1 the least recently seen: the 1,025th issue
        // forgets it, and only it.
        assert_eq!(allow(0), None);
        assert_eq!(allow(max), Some(0));
        for issue in (0..=max).filter(|&i| i != 1) {
            assert_eq!(allow(issue), None, "issue {issue} remembered");
        }
        assert_eq!(allow(1), Some(0), "forgotten: a fresh budget");
        // That forgot 0, the least recently seen since; 0 forgets 2.
        assert_eq!(allow(0), Some(0));
        assert_eq!(allow(2), Some(0));
        assert_eq!(allow(max), None);
    }

    #[test]
    fn new_issues_take_constant_time() {
        // Past the issues remembered, each new one forgets the least
        // recently seen without a scan.
        let time = |issues: u64| {
            let b = Budget::new(ErrorBudget::default());
            let now = Instant::now();
            let start = Instant::now();
            for issue in 0..issues {
                b.allow(issue, now);
            }
            start.elapsed()
        };
        // The best of runs taken in turn, at least 5 and up to 20 for a quiet
        // moment: a busy machine slows both alike.
        let (mut once, mut twice) = (Duration::MAX, Duration::MAX);
        for round in 1..=20 {
            if round > 5 && twice < once * 3 {
                break;
            }
            once = once.min(time(100_000));
            twice = twice.min(time(200_000));
        }
        assert!(
            once < Duration::from_millis(500),
            "100,000 issues took {once:?}"
        );
        assert!(
            twice < once * 3,
            "200,000 issues took {twice:?}, 100,000 {once:?}"
        );
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
