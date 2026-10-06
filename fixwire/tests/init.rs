//! `init` with a broken DSN: the app starts, the SDK stays off and says so on
//! stderr. A file of its own: `init` sets the process's hub, and the test runs
//! again in a process of its own to set `FIXWIRE_DSN`.

use std::process::Command;

use fixwire::{Dsn, Level, Options};

const CHILD: &str = "FIXWIRE_TEST_INIT_CHILD";
const WARNING: &str = "fixwire: the DSN must look like https://<key>@<host>: the SDK is off";

#[test]
fn a_broken_dsn_leaves_the_sdk_off_without_a_panic() {
    if std::env::var_os(CHILD).is_some() {
        let env_broken = std::env::var("FIXWIRE_DSN").is_ok_and(|d| d.parse::<Dsn>().is_err());
        // A valid DSN in the environment doesn't stand in for a broken one given.
        let mut dsns = vec![
            Some("ingest.fixwire.io"),
            Some("https://@ingest.fixwire.io"),
        ];
        if env_broken {
            dsns.push(None);
        }
        for dsn in dsns {
            let guard = fixwire::init(Options {
                dsn: dsn.map(Into::into),
                ..Options::default()
            });
            assert!(!guard.client().is_enabled(), "{dsn:?}");
            assert_eq!(guard.client().options().dsn, None);
            assert_eq!(fixwire::capture_message("lost", Level::Info), None);
        }
        return;
    }
    // Said on stderr whether or not debug is on: once per broken DSN.
    for (env_dsn, warnings) in [
        ("https://fw_pk_test@127.0.0.1:9", 2),
        ("ftp://k@ingest.fixwire.io", 3),
    ] {
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "a_broken_dsn_leaves_the_sdk_off_without_a_panic",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("FIXWIRE_DSN", env_dsn)
            .env_remove("FIXWIRE_DEBUG")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        assert_eq!(stderr.matches(WARNING).count(), warnings, "{stderr}");
    }
}
