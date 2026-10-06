//! A cron job reporting to Fixwire: each run is a check-in to its monitor
//! (created from the first one), each account a scope of its own; a failed
//! account is reported and the run carries on, then ends as an error with a
//! summary warning. The run is a trace.
//!
//! ```sh
//! FIXWIRE_DSN=https://<key>@<host> cargo run --bin nightly-report
//! ```

use std::error::Error;
use std::fmt;
use std::process::ExitCode;

use fixwire::{Breadcrumb, Level, MonitorConfig, MonitorSchedule, Options, User};

const ACCOUNTS: [(&str, &str); 3] = [
    ("acct_1", "ada@example.com"),
    ("acct_2", "grace@example.com"),
    ("acct_3", "alan@example.com"),
];

/// A report that could not be built.
#[derive(Debug)]
struct ReportFailed {
    account: &'static str,
    cause: std::io::Error,
}

impl fmt::Display for ReportFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "building the report of {}", self.account)
    }
}

impl Error for ReportFailed {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.cause)
    }
}

#[derive(Debug)]
struct RunFailed(usize);

impl fmt::Display for RunFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} of {} reports failed", self.0, ACCOUNTS.len())
    }
}

impl Error for RunFailed {}

fn main() -> ExitCode {
    // FIXWIRE_DSN in the environment; the guard sends what is left when main returns.
    let _fixwire = fixwire::init(Options {
        release: Some(std::env::var("RELEASE").unwrap_or_else(|_| "nightly-report@1.0.0".into())),
        traces_sample_rate: 1.0,
        ..Options::default()
    });
    // The first check-in creates the monitor: 3:00 every night in Berlin, 5 minutes late at most, 30
    // minutes long at most.
    let monitor = MonitorConfig {
        schedule: MonitorSchedule::Crontab("0 3 * * *".into()),
        checkin_margin: Some(5),
        max_runtime: Some(30),
        timezone: Some("Europe/Berlin".into()),
    };
    match fixwire::with_monitor("nightly-report", Some(monitor), run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// Builds every account's report; a failure doesn't stop the others.
fn run() -> Result<(), RunFailed> {
    fixwire::trace("nightly-report", "task", |_| {
        let mut failed = 0;
        for (account, email) in ACCOUNTS {
            fixwire::with_scope(
                |s| {
                    s.set_tag("account", account);
                    s.set_user(Some(User {
                        id: Some(account.into()),
                        email: Some(email.into()),
                        ..User::default()
                    }));
                },
                || match build_report(account) {
                    Ok(rows) => fixwire::add_breadcrumb(Breadcrumb::new(
                        "report",
                        format!("{account}: {rows} rows"),
                    )),
                    Err(e) => {
                        fixwire::capture_error(&e);
                        failed += 1;
                    }
                },
            );
        }
        if failed > 0 {
            let summary = RunFailed(failed);
            fixwire::capture_message(summary.to_string(), Level::Warning);
            return Err(summary);
        }
        Ok(())
    })
}

fn build_report(account: &'static str) -> Result<usize, ReportFailed> {
    fixwire::trace(format!("report {account}"), "report.build", |_| {
        if account == "acct_2" {
            // The data warehouse lost this account's partition.
            return Err(ReportFailed {
                account,
                cause: std::io::Error::other("partition 2026-10 not found"),
            });
        }
        Ok(42)
    })
}
