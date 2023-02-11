// This file is part of Moonfire NVR, a security camera network video recorder.
// Copyright (C) 2020 The Moonfire NVR Authors; see AUTHORS and LICENSE.txt.
// SPDX-License-Identifier: GPL-v3.0-or-later WITH GPL-3.0-linking-exception.

//! Subcommand to login a user (without requiring a password).

use base::clock::{self, Clocks};
use bpaf::Bpaf;
use db::auth::SessionFlag;
use failure::{format_err, Error};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;
use std::str::FromStr;

fn parse_perms(perms: String) -> Result<crate::json::Permissions, serde_json::Error> {
    serde_json::from_str(&perms)
}

fn parse_flags(flags: String) -> Result<Vec<SessionFlag>, Error> {
    flags
        .split(',')
        .map(|f| SessionFlag::from_str(f.trim()))
        .collect()
}

#[derive(Bpaf, Debug, PartialEq, Eq)]
pub struct Args {
    /// Directory holding the SQLite3 index database.
    ///
    /// default: `/var/lib/moonfire-nvr/db`.
    #[bpaf(argument("PATH"), fallback_with(crate::default_db_dir))]
    db_dir: PathBuf,

    /// Creates a session with the given permissions, as a JSON object.
    ///
    /// E.g. `{"viewVideo": true}`. See `ref/api.md` for a description of `Permissions`.
    /// If unspecified, uses user's default permissions.
    #[bpaf(argument::<String>("PERMS"), parse(parse_perms), optional)]
    permissions: Option<crate::json::Permissions>,

    /// Restricts this cookie to the given domain.
    #[bpaf(argument("DOMAIN"))]
    domain: Option<String>,

    /// Writes the cookie to a new curl-compatible cookie-jar file.
    ///
    /// `--domain` must be specified. This file can be used later with curl's `--cookie` flag.
    #[bpaf(argument("PATH"))]
    curl_cookie_jar: Option<PathBuf>,

    /// Sets the given db::auth::SessionFlags.
    ///
    /// default: `http-only,secure,same-site,same-site-strict`.
    #[bpaf(
        argument::<String>("FLAGS"),
        fallback_with(|| Ok::<_, std::convert::Infallible>("http-only,secure,same-site,same-site-strict".to_owned())),
        parse(parse_flags),
    )]
    session_flags: Vec<SessionFlag>,

    /// Username to create a session for.
    #[bpaf(positional("USERNAME"))]
    username: String,
}

pub fn run(args: Args) -> Result<i32, Error> {
    let clocks = clock::RealClocks {};
    let (_db_dir, conn) = super::open_conn(&args.db_dir, super::OpenMode::ReadWrite)?;
    let db = std::sync::Arc::new(db::Database::new(clocks, conn, true).unwrap());
    let mut l = db.lock();
    let u = l
        .get_user(&args.username)
        .ok_or_else(|| format_err!("no such user {:?}", &args.username))?;
    let permissions = args
        .permissions
        .map(db::Permissions::from)
        .unwrap_or_else(|| u.permissions.clone());
    let creation = db::auth::Request {
        when_sec: Some(db.clocks().realtime().sec),
        user_agent: None,
        addr: None,
    };
    let mut flags = 0;
    for f in &args.session_flags {
        flags |= *f as i32;
    }
    let uid = u.id;
    let (sid, _) = l.make_session(
        creation,
        uid,
        args.domain.clone().map(String::into_bytes),
        flags,
        permissions,
    )?;
    let mut encoded = [0u8; 64];
    base64::encode_config_slice(sid, base64::STANDARD_NO_PAD, &mut encoded);
    let encoded = std::str::from_utf8(&encoded[..]).expect("base64 is valid UTF-8");

    if let Some(ref p) = args.curl_cookie_jar {
        let d = args
            .domain
            .as_ref()
            .ok_or_else(|| format_err!("--curl-cookie-jar requires --domain"))?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(p)
            .map_err(|e| format_err!("Unable to open {}: {}", p.display(), e))?;
        write!(
            &mut f,
            "# Netscape HTTP Cookie File\n\
            # https://curl.haxx.se/docs/http-cookies.html\n\
            # This file was generated by moonfire-nvr login! Edit at your own risk.\n\n\
            {}\n",
            curl_cookie(encoded, flags, d)
        )?;
        f.sync_all()?;
        println!("Wrote cookie to {}", p.display());
    } else {
        println!("s={encoded}");
    }
    Ok(0)
}

fn curl_cookie(cookie: &str, flags: i32, domain: &str) -> String {
    format!(
        "{httponly}{domain}\t{tailmatch}\t{path}\t{secure}\t{expires}\t{name}\t{cookie}",
        httponly = if (flags & SessionFlag::HttpOnly as i32) != 0 {
            "#HttpOnly_"
        } else {
            ""
        },
        tailmatch = "FALSE",
        path = "/",
        secure = if (flags & SessionFlag::Secure as i32) != 0 {
            "TRUE"
        } else {
            "FALSE"
        },
        expires = "9223372036854775807", // 64-bit CURL_OFF_T_MAX, never expires
        name = "s",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use bpaf::Parser;

    #[test]
    fn parse_args() {
        let args = args()
            .to_options()
            .run_inner(bpaf::Args::from(&[
                "--permissions",
                "{\"viewVideo\": true}",
                "--session-flags",
                "http-only, same-site",
                "slamb",
            ]))
            .unwrap();
        assert_eq!(
            args,
            Args {
                db_dir: crate::default_db_dir().unwrap(),
                domain: None,
                curl_cookie_jar: None,
                permissions: Some(crate::json::Permissions {
                    view_video: true,
                    ..Default::default()
                }),
                session_flags: vec![SessionFlag::HttpOnly, SessionFlag::SameSite],
                username: "slamb".to_owned(),
            }
        );
    }

    #[test]
    fn test_curl_cookie() {
        assert_eq!(
            curl_cookie(
                "o3mx3OntO7GzwwsD54OuyQ4IuipYrwPR2aiULPHSudAa+xIhwWjb+w1TnGRh8Z5Q",
                SessionFlag::HttpOnly as i32,
                "localhost"
            ),
            "#HttpOnly_localhost\tFALSE\t/\tFALSE\t9223372036854775807\ts\t\
                   o3mx3OntO7GzwwsD54OuyQ4IuipYrwPR2aiULPHSudAa+xIhwWjb+w1TnGRh8Z5Q"
        );
    }
}
