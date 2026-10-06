//! The DSN: `{scheme}://{key}@{host}[:{port}][/{path}]`.

use std::fmt;
use std::str::FromStr;

/// Where the SDK sends, and with which key.
#[derive(Clone, PartialEq, Eq)]
pub struct Dsn {
    key: String,
    base_url: String,
}

/// A DSN without a scheme, a host or a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidDsn;

impl fmt::Display for InvalidDsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("fixwire: the DSN must look like https://<key>@<host>")
    }
}

impl std::error::Error for InvalidDsn {}

impl Dsn {
    /// The project's publishable key.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The DSN without the key: the endpoints are relative to it.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The address of an endpoint (`/v1/logs`, …).
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

impl FromStr for Dsn {
    type Err = InvalidDsn;

    fn from_str(s: &str) -> Result<Dsn, InvalidDsn> {
        let s = s.trim();
        let (scheme, rest) = s.split_once("://").ok_or(InvalidDsn)?;
        if scheme != "https" && scheme != "http" {
            return Err(InvalidDsn);
        }
        let (authority, path) = match rest.find(['/', '?', '#']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let (userinfo, host) = authority.rsplit_once('@').ok_or(InvalidDsn)?;
        let key = userinfo.split(':').next().unwrap_or_default();
        if key.is_empty() || host.is_empty() {
            return Err(InvalidDsn);
        }
        let path = path
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .trim_end_matches('/');
        Ok(Dsn {
            key: key.to_owned(),
            base_url: format!("{scheme}://{host}{path}"),
        })
    }
}

impl fmt::Debug for Dsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The key stays out of logs.
        f.debug_struct("Dsn")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_dsns() {
        let d: Dsn = "https://fw_pk_live_abc@ingest.eu.fixwire.io"
            .parse()
            .unwrap();
        assert_eq!(
            (d.key(), d.base_url()),
            ("fw_pk_live_abc", "https://ingest.eu.fixwire.io")
        );
        let d: Dsn = "http://k@localhost:8080/fixwire/".parse().unwrap();
        assert_eq!(d.url("/v1/logs"), "http://localhost:8080/fixwire/v1/logs");
        for bad in [
            "",
            "ingest.fixwire.io",
            "https://ingest.fixwire.io",
            "ftp://k@host",
            "https://@host",
            "https://k@",
        ] {
            assert_eq!(bad.parse::<Dsn>(), Err(InvalidDsn), "{bad}");
        }
    }
}
