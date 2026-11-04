//! Network work in a short-lived child process.
//!
//! Measured on the release build: one check maps winhttp.dll plus ~19 more DLLs (webio,
//! schannel, ncrypt, crypt32, dnsapi, iphlpapi, nsi, ...) and leaves a threadpool wait
//! thread and ~1 MB private behind for good. Unloading winhttp alone cannot undo that and
//! risks unmapping code a registered callback still uses. So the tray process never does
//! HTTP itself: it starts `holdmyram.exe --update-worker`, sends one JSON request line on
//! stdin, reads progress and one result line from stdout, and the child exits. Windows
//! frees every thread, handle and DLL with it.
//!
//! The child computes the API URL and URL policy itself (`active_api`), so a request can
//! never widen the allow-list; `fetch` re-checks every URL in the record anyway.
//!
//! Protocol (stdout, one line each): `P <done> <total|->` progress, then `R <json reply>`.

use super::{active_api, check, download_and_stage, now_unix, CheckRecord, Http, Staged, UpdateError, Version};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::PathBuf;

pub const ARG: &str = "--update-worker";
const MAX_REQUEST: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Check { current: String },
    Download { record: CheckRecord, current: String, updates_dir: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Reply {
    Checked { record: CheckRecord },
    Staged { staged: Staged },
    Failed { code: String, msg: String },
}

/// One parsed stdout line of the worker.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Progress(u64, Option<u64>),
    Done(Reply),
    Other,
}

pub fn parse_line(line: &str) -> Line {
    let line = line.trim_end();
    if let Some(rest) = line.strip_prefix("P ") {
        let mut it = rest.split(' ');
        if let (Some(d), Some(t)) = (it.next().and_then(|d| d.parse().ok()), it.next()) {
            return Line::Progress(d, t.parse().ok());
        }
    } else if let Some(json) = line.strip_prefix("R ") {
        if let Ok(r) = serde_json::from_str(json) {
            return Line::Done(r);
        }
    }
    Line::Other
}

/// The child's side: read one request, do it with `net`, write progress and the reply.
/// Returns the process exit code.
pub fn run(input: &mut dyn BufRead, out: &mut dyn Write, net: &dyn Http) -> i32 {
    let reply = |out: &mut dyn Write, r: &Reply| {
        let _ = writeln!(out, "R {}", serde_json::to_string(r).unwrap_or_default());
        let _ = out.flush();
    };
    let mut line = String::new();
    let read = std::io::Read::take(&mut *input, MAX_REQUEST as u64).read_line(&mut line);
    let req: Request = match read.ok().and_then(|_| serde_json::from_str(line.trim()).ok()) {
        Some(r) => r,
        None => {
            reply(out, &Reply::Failed { code: "internal".into(), msg: "update worker: bad request".into() });
            return 2;
        }
    };
    let (api, policy) = active_api();
    let parse = |v: &str| Version::parse(v).ok_or_else(|| UpdateError::new("internal", format!("bad version {v:?}")));
    let r = match req {
        Request::Check { current } => parse(&current).map(|cur| Reply::Checked { record: check(net, &api, policy, &cur, now_unix()) }),
        Request::Download { record, current, updates_dir } => parse(&current).and_then(|cur| {
            let mut progress = |d: u64, t: Option<u64>| {
                let t = t.map(|t| t.to_string()).unwrap_or_else(|| "-".into());
                let _ = writeln!(out, "P {d} {t}");
            };
            download_and_stage(net, policy, &record, &cur, &updates_dir, &mut progress).map(|staged| Reply::Staged { staged })
        }),
    };
    let r = r.unwrap_or_else(|e| Reply::Failed { code: e.code.to_string(), msg: e.msg });
    reply(out, &r);
    0
}

/// The request as the one line the worker reads.
pub fn request_line(req: &Request) -> String {
    serde_json::to_string(req).unwrap_or_default()
}
