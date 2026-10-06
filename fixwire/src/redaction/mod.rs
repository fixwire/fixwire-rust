//! On-device redaction: masks secrets and personal data before anything is
//! sent, with the same results as the Fixwire server's redaction
//! (`pkg/redact`), proven by the shared corpus `pkg/redact/testdata/vectors.json`
//! and differential fuzzing against the server's code.
//!
//! Detectors run in a fixed order; a cheap prefilter skips each one on text
//! that cannot match, and validators (Luhn, mod-97, check digits) reject
//! look-alikes so trace ids, hashes and timestamps survive. The server's
//! patterns are RE2, which the `regex` crate follows closely (leftmost-first,
//! linear time); where their meanings differ (`\b`, `\d`, `\s` are ASCII in
//! RE2) the patterns spell out the ASCII forms (see `detectors`).
//!
//! Offsets are byte offsets; repetitions count characters, as the server's
//! count code points.

mod detectors;
mod scanners;
#[cfg(test)]
mod tests;
mod validators;
mod walk;

use std::borrow::Cow;
use std::sync::OnceLock;

use detectors::Detector;
use scanners::Text;

use crate::limits::{REDACT_AHEAD, cut, head};

/// Replaces the value of a sensitive key.
pub(crate) const FILTERED: &str = "[Filtered]";

/// The detectors on by default, in the server's order: all but `ipv4` (in
/// error messages IP addresses are usually servers worth seeing).
pub(crate) const DEFAULT_DETECTORS: [&str; 21] = [
    "private_key",
    "aws_access_key",
    "gcp_api_key",
    "azure_storage_key",
    "github_token",
    "stripe_key",
    "slack_token",
    "slack_webhook",
    "anthropic_key",
    "openai_key",
    "jwt",
    "fixwire_secret_key",
    "url_credentials",
    "http_auth",
    "secret_assignment",
    "email",
    "credit_card",
    "iban",
    "us_ssn",
    "tr_tckn",
    "phone",
];

/// Key fragments whose values are always filtered whole.
pub(crate) const DEFAULT_SENSITIVE_KEYS: [&str; 19] = [
    "password",
    "passwd",
    "pwd",
    "secret",
    "apikey",
    "accesskey",
    "token",
    "credential",
    "privatekey",
    "authorization",
    "cookie",
    "sessionid",
    "csrf",
    "xsrf",
    "cvv",
    "cvc",
    "ssn",
    "creditcard",
    "cardnumber",
];

/// Masks secrets and personal data. Immutable, cheap to share between
/// threads.
pub(crate) struct Redactor {
    detectors: Vec<&'static Detector>,
    keys: Vec<Cow<'static, str>>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field(
                "detectors",
                &self.detectors.iter().map(|d| d.name).collect::<Vec<_>>(),
            )
            .field("sensitive_keys", &self.keys)
            .finish()
    }
}

/// One finding: a byte range and its detector.
struct Found {
    start: usize,
    end: usize,
    name: &'static str,
}

impl Redactor {
    /// A redactor with the default detectors. Sensitive keys, when given,
    /// replace the defaults (an empty list leaves only "auth"), compared like
    /// the server does: lower case, without "-", "_" and spaces.
    pub(crate) fn new(sensitive_keys: Option<&[String]>) -> Redactor {
        Redactor::build(&DEFAULT_DETECTORS, sensitive_keys).expect("the default detectors exist")
    }

    /// The redactor with the default detectors and sensitive keys, built once.
    pub(crate) fn default_shared() -> &'static Redactor {
        static DEFAULT: OnceLock<Redactor> = OnceLock::new();
        DEFAULT.get_or_init(|| Redactor::new(None))
    }

    /// A redactor running the named detectors in the order given (`ipv4`
    /// included), as the server's `Options.Detectors`. Fails on an unknown
    /// name.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn with_detectors(
        names: &[&str],
        sensitive_keys: Option<&[String]>,
    ) -> Result<Redactor, String> {
        Redactor::build(names, sensitive_keys)
    }

    fn build(names: &[&str], sensitive_keys: Option<&[String]>) -> Result<Redactor, String> {
        let detectors = names
            .iter()
            .map(|&name| {
                detectors::named(name).ok_or_else(|| format!("redact: unknown detector {name:?}"))
            })
            .collect::<Result<_, _>>()?;
        let keys = match sensitive_keys {
            None => DEFAULT_SENSITIVE_KEYS
                .iter()
                .map(|&k| Cow::Borrowed(k))
                .collect(),
            Some(keys) => keys.iter().map(|k| Cow::Owned(normalize_key(k))).collect(),
        };
        Ok(Redactor { detectors, keys })
    }

    /// Masks the findings in a string: each becomes `[REDACTED:<detector>]`,
    /// as the server writes it. Returns the masked text (borrowed when there
    /// is nothing to mask) and each finding's detector, leftmost first.
    pub(crate) fn mask<'a>(&self, text: &'a str) -> (Cow<'a, str>, Vec<&'static str>) {
        let found = self.find(text);
        if found.is_empty() {
            return (Cow::Borrowed(text), Vec::new());
        }
        let mut out = String::with_capacity(text.len() + found.len() * 24);
        let mut last = 0;
        for f in &found {
            out.push_str(&text[last..f.start]);
            out.push_str("[REDACTED:");
            out.push_str(f.name);
            out.push(']');
            last = f.end;
        }
        out.push_str(&text[last..]);
        (Cow::Owned(out), found.iter().map(|f| f.name).collect())
    }

    /// Masks `text`, then cuts it to `limit` bytes ending in `...` (see
    /// `limits::cut`). The masking reads the part kept and the next 16 kB, so
    /// a secret the cut goes through is still masked, and no further: a huge
    /// text costs what a short one does.
    pub(crate) fn mask_within<'a>(
        &self,
        text: &'a str,
        limit: usize,
    ) -> (Cow<'a, str>, Vec<&'static str>) {
        let read = head(text, limit.saturating_add(REDACT_AHEAD));
        let more = read.len() < text.len();
        let (masked, found) = self.mask(read);
        let masked = match masked {
            Cow::Owned(m) if m.len() <= limit && !more => Cow::Owned(m),
            Cow::Owned(m) => Cow::Owned(cut(&m, limit, more).into_owned()),
            Cow::Borrowed(m) => cut(m, limit, more),
        };
        (masked, found)
    }

    /// The non-overlapping findings, sorted by start; when two overlap, the
    /// earlier detector wins (and within one detector, the earlier span).
    fn find(&self, text: &str) -> Vec<Found> {
        let text = Text::new(text);
        let mut found: Vec<Found> = Vec::new();
        for d in &self.detectors {
            if !d.may_match(&text) {
                continue;
            }
            let mut added = Vec::new();
            // Spans come leftmost first, so one pass checks them against the
            // findings so far (sorted and disjoint) and the last one added.
            let mut k = 0;
            let mut last_end = 0;
            for (start, end) in d.spans(&text) {
                if start < last_end {
                    continue;
                }
                while k < found.len() && found[k].end <= start {
                    k += 1;
                }
                if k < found.len() && found[k].start < end {
                    continue;
                }
                if d.validate.is_some_and(|v| !v(&text.string()[start..end])) {
                    continue;
                }
                added.push(Found {
                    start,
                    end,
                    name: d.name,
                });
                last_end = end;
            }
            if !added.is_empty() {
                found = merge(found, added);
            }
        }
        found
    }

    /// Whether the value under a key must be filtered whole. Keys that count
    /// model tokens hold no token: gen_ai.usage.input_tokens, max_tokens.
    fn sensitive(&self, key: &str) -> bool {
        let k = normalize_key(key);
        k == "auth"
            || self.keys.iter().any(|frag| {
                k.contains(frag.as_ref()) && (frag.as_ref() != "token" || !token_count(&k))
            })
    }
}

/// Two sorted, disjoint lists of findings as one.
fn merge(a: Vec<Found>, b: Vec<Found>) -> Vec<Found> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let mut b = b.into_iter().peekable();
    for f in a {
        while let Some(g) = b.next_if(|g| g.start < f.start) {
            out.push(g);
        }
        out.push(f);
    }
    out.extend(b);
    out
}

fn token_count(k: &str) -> bool {
    k.ends_with("tokens") || k.contains("tokencount") || k.contains("usage")
}

/// A key as the server compares it: lower case (Go's simple mapping), without
/// "-", "_" and spaces.
fn normalize_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for c in key.chars() {
        match c {
            '-' | '_' | ' ' => {}
            c if c.is_ascii() => out.push(c.to_ascii_lowercase()),
            c => out.push(go_lower(c)),
        }
    }
    out
}

/// A character in lower case as Go's unicode.ToLower maps it, with the
/// server's tables (Go 1.27: Unicode 17.0): the simple mapping, so U+0130 is
/// "i" (Rust's full mapping adds a combining dot). The capitals Unicode 17.0
/// added are mapped here, for Rust releases whose tables are older (1.88 has
/// Unicode 16.0).
fn go_lower(c: char) -> char {
    let added = match c {
        '\u{130}' => return 'i',
        '\u{A7CE}' | '\u{A7D2}' | '\u{A7D4}' => 1,
        '\u{16EA0}'..='\u{16EB8}' => 0x1B,
        _ => 0,
    };
    if added > 0 {
        return char::from_u32(u32::from(c) + added).unwrap_or(c);
    }
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}
