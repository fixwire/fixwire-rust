//! The detectors of the Fixwire server's redaction, in its order, with the
//! same patterns, prefilters, validators and scanners.
//!
//! The server's regular expressions are RE2 (Go's regexp), which the `regex`
//! crate follows closely: both are leftmost-first and linear-time, count code
//! points in repetitions and fold case simply, so `(?i)` matches the Kelvin
//! sign for "k" and the long s for "s" (in letter classes too) and never "ss"
//! for the sharp s. The server's patterns are therefore kept, with one kind of
//! change: RE2's `\b`, `\d` and `\s` are ASCII, the `regex` crate's Unicode. So
//! the word boundary is spelled `(?-u:\b)` (which also keeps the lazy DFA on
//! non-ASCII text), digits `[0-9]` and whitespace `[\t\n\f\r ]` (RE2's `\s`
//! has no vertical tab, unlike `(?-u:\s)`), also inside negated classes;
//! `[\s\S]` is `(?s:.)`. `(?i)` needs the crate's `unicode-case` feature (on
//! by default).

use std::sync::OnceLock;

use regex::Regex;

use super::scanners::{self, Span, Text};
use super::validators;

/// The name of the detector that is off by default.
pub(super) const IPV4: &str = "ipv4";

/// Finds one kind of sensitive value. A cheap literal prefilter skips the
/// pattern on text that cannot match, and a validator rejects look-alikes.
pub(super) struct Detector {
    pub(super) name: &'static str,
    /// Substrings one of which must appear (in any case, unless
    /// `case_sensitive`); none means always run.
    prefilter: &'static [&'static str],
    case_sensitive: bool,
    find: Find,
    /// Rejects a matched text.
    pub(super) validate: Option<fn(&str) -> bool>,
    /// A cheaper prefilter than literals, where there are none.
    may: Option<fn(&str) -> bool>,
}

enum Find {
    /// A regular expression (compiled on first use) whose `group` (0: the
    /// whole match) is masked.
    Pattern {
        source: &'static str,
        group: usize,
        re: OnceLock<Regex>,
    },
    /// A hand-written scanner returning the spans.
    Scan(fn(&Text<'_>) -> Vec<Span>),
}

impl Detector {
    /// Whether the prefilters let the text through to the pattern or scanner.
    pub(super) fn may_match(&self, text: &Text<'_>) -> bool {
        if !self.prefilter.is_empty() {
            let hay = if self.case_sensitive {
                text.string()
            } else {
                text.lower()
            };
            if !self.prefilter.iter().any(|p| hay.contains(p)) {
                return false;
            }
        }
        self.may.is_none_or(|may| may(text.string()))
    }

    /// The candidate spans, leftmost first, as byte offsets.
    pub(super) fn spans(&self, text: &Text<'_>) -> Vec<Span> {
        let (source, group, re) = match &self.find {
            Find::Scan(scan) => return scan(text),
            Find::Pattern { source, group, re } => (source, *group, re),
        };
        let re = re.get_or_init(|| Regex::new(source).expect("a valid detector pattern"));
        if group == 0 {
            return re
                .find_iter(text.string())
                .map(|m| (m.start(), m.end()))
                .collect();
        }
        re.captures_iter(text.string())
            .filter_map(|c| c.get(group).or_else(|| c.get(0)))
            .map(|m| (m.start(), m.end()))
            .collect()
    }
}

const fn pattern(source: &'static str, group: usize) -> Find {
    Find::Pattern {
        source,
        group,
        re: OnceLock::new(),
    }
}

/// A detector whose prefilter matches case-sensitively (`exact`) or in any
/// case.
const fn detector(
    name: &'static str,
    prefilter: &'static [&'static str],
    exact: bool,
    find: Find,
) -> Detector {
    Detector {
        name,
        prefilter,
        case_sensitive: exact,
        find,
        validate: None,
        may: None,
    }
}

impl Detector {
    const fn validated(mut self, validate: fn(&str) -> bool) -> Detector {
        self.validate = Some(validate);
        self
    }

    const fn gated(mut self, may: fn(&str) -> bool) -> Detector {
        self.may = Some(may);
        self
    }
}

const EXACT: bool = true;
const ANY_CASE: bool = false;

/// Every detector, in the server's order (findings that overlap go to the
/// earlier one).
pub(super) static REGISTRY: [Detector; 22] = [
    detector(
        "private_key",
        &["PRIVATE KEY-----"],
        EXACT,
        pattern(
            r"-----BEGIN (?:[A-Z ]+ )?PRIVATE KEY-----(?s:.)*?-----END (?:[A-Z ]+ )?PRIVATE KEY-----",
            0,
        ),
    ),
    detector(
        "aws_access_key",
        &["AKIA", "ASIA", "ABIA", "ACCA"],
        EXACT,
        pattern(r"(?-u:\b)(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}(?-u:\b)", 0),
    ),
    detector(
        "gcp_api_key",
        &["AIza"],
        EXACT,
        pattern(r"(?-u:\b)AIza[0-9A-Za-z_\-]{35}", 0),
    ),
    detector(
        "azure_storage_key",
        &["accountkey="],
        ANY_CASE,
        pattern(r"(?i)AccountKey=([A-Za-z0-9+/]{86}==)", 1),
    ),
    detector(
        "github_token",
        &["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"],
        EXACT,
        pattern(
            r"(?-u:\b)(?:gh[pousr]_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{60,255})(?-u:\b)",
            0,
        ),
    ),
    detector(
        "stripe_key",
        &["sk_live_", "sk_test_", "rk_live_", "rk_test_", "whsec_"],
        EXACT,
        pattern(
            r"(?-u:\b)(?:(?:sk|rk)_(?:live|test)_[0-9A-Za-z]{16,247}|whsec_[A-Za-z0-9+/=]{24,})",
            0,
        ),
    ),
    detector(
        "slack_token",
        &["xox"],
        EXACT,
        pattern(r"(?-u:\b)xox[abposr]-[0-9A-Za-z-]{10,250}(?-u:\b)", 0),
    ),
    detector(
        "slack_webhook",
        &["hooks.slack.com/services/"],
        EXACT,
        pattern(
            r"https://hooks\.slack\.com/services/T[A-Z0-9]+/B[A-Z0-9]+/[A-Za-z0-9]+",
            0,
        ),
    ),
    detector(
        "anthropic_key",
        &["sk-ant-"],
        EXACT,
        pattern(r"(?-u:\b)sk-ant-(?:api|admin)[0-9]{2}-[A-Za-z0-9_\-]{80,}", 0),
    ),
    detector(
        "openai_key",
        &["sk-"],
        EXACT,
        pattern(
            r"(?-u:\b)sk-(?:(?:proj|svcacct|admin)-[A-Za-z0-9_\-]{40,}|[A-Za-z0-9]{20}T3BlbkFJ[A-Za-z0-9]{20})",
            0,
        ),
    ),
    detector(
        "jwt",
        &["eyJ"],
        EXACT,
        pattern(
            r"(?-u:\b)eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            0,
        ),
    ),
    detector(
        "fixwire_secret_key",
        &["_sk_live_", "_sk_test_"],
        EXACT,
        pattern(r"(?-u:\b)[a-z]{2,4}_sk_(?:live|test)_[0-9A-Za-z]{38}(?-u:\b)", 0),
    ),
    // The password in scheme://user:password@host (the user stays).
    detector(
        "url_credentials",
        &["://"],
        EXACT,
        pattern(
            r"(?-u:\b)[A-Za-z][A-Za-z0-9+.\-]*://[^\t\n\f\r /?#@:]*:([^\t\n\f\r /?#@]+)@",
            1,
        ),
    )
    .validated(validators::unmasked),
    // Bearer and Basic credentials outside a header (messages, breadcrumbs).
    detector(
        "http_auth",
        &["bearer", "basic"],
        ANY_CASE,
        pattern(
            r"(?i)(?-u:\b)(?:bearer|basic)[\t\n\f\r ]+([A-Za-z0-9._~+/\-]{12,}=*)",
            1,
        ),
    )
    .validated(validators::credential_like),
    // A value given to a secret's name, in text, config and URLs. The name may
    // end a longer one (access_token, client_secret, csrfToken, PHPSESSID,
    // X-Amz-Signature); an OAuth code counts in a query or fragment only.
    detector(
        "secret_assignment",
        &[
            "pass",
            "pwd",
            "secret",
            "key",
            "token",
            "credential",
            "sess",
            "sig",
            "code",
        ],
        ANY_CASE,
        pattern(
            r#"(?i)(?:password|passwd|pwd|secret(?:[_-]?key)?|private[_-]?key|token|api[_-]?key|access[_-]?key|credentials?|sess(?:ion)?[_-]?id|sig(?:nature)?|[?&#]code)["']?[\t\n\f\r ]*[:=][\t\n\f\r ]*["']?([^\t\n\f\r "',;&]{6,})"#,
            1,
        ),
    )
    .validated(validators::unmasked),
    detector("email", &["@"], EXACT, Find::Scan(scanners::email_spans)),
    detector("credit_card", &[], ANY_CASE, Find::Scan(scanners::card_spans)),
    detector(
        "iban",
        &[],
        ANY_CASE,
        pattern(
            r"(?-u:\b)[A-Z]{2}[0-9]{2}(?: ?[A-Z0-9]{4}){2,7}(?: ?[A-Z0-9]{1,3})?(?-u:\b)",
            0,
        ),
    )
    .gated(scanners::may_hold_iban)
    .validated(validators::iban),
    detector("us_ssn", &["-"], EXACT, Find::Scan(scanners::ssn_spans)),
    detector("tr_tckn", &[], ANY_CASE, Find::Scan(scanners::tckn_spans)),
    detector(
        "phone",
        &["+"],
        EXACT,
        pattern(r"\+[0-9](?:[ .\-()]?[0-9]){7,14}(?-u:\b)", 0),
    )
    .validated(validators::phone),
    detector(
        IPV4,
        &["."],
        EXACT,
        pattern(
            r"(?-u:\b)(?:(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])(?-u:\b)",
            0,
        ),
    ),
];

/// The detector with this name.
pub(super) fn named(name: &str) -> Option<&'static Detector> {
    REGISTRY.iter().find(|d| d.name == name)
}
