//! A fake GitHub CLI: answers `gh api [flags] <path>` from the answers file
//! in `GH_CONFIG_DIR` (see `leiser_testkit::FakeGh`), as `gh` does with
//! `--include`: the status line, the headers, a blank line and the body.
//! A request whose `If-None-Match` names the answer's tag gets `304 Not
//! Modified` and, like `gh`, an exit code of 1.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use leiser_testkit::{FAKE_GH_REQUESTS_FILE, FakeGh, FakeGhRequest};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "fake-gh: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let config_dir = PathBuf::from(std::env::var_os("GH_CONFIG_DIR").ok_or("no GH_CONFIG_DIR")?);
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("api") {
        return Err("only `api` is faked".into());
    }
    let mut include = false;
    let mut if_none_match = None;
    let mut path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--include" | "-i" => include = true,
            "--header" | "-H" => {
                let header = args.next().ok_or("--header without a value")?;
                if let Some((name, value)) = header.split_once(':')
                    && name.trim().eq_ignore_ascii_case("if-none-match")
                {
                    if_none_match = Some(value.trim().to_owned());
                }
            }
            "--hostname" | "--method" | "-X" | "--jq" | "-q" | "-f" | "-F" => {
                args.next();
            }
            _ => path = Some(arg),
        }
    }
    let path = path.ok_or("no request path")?;
    let answer = FakeGh::new(&config_dir).answers()?.remove(&path);
    let (status, reason) = match &answer {
        None => (404, "Not Found"),
        Some(answer) if answer.etag.is_some() && answer.etag == if_none_match => {
            (304, "Not Modified")
        }
        Some(_) => (200, "OK"),
    };
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config_dir.join(FAKE_GH_REQUESTS_FILE))?;
    let request = FakeGhRequest {
        path,
        if_none_match,
        status,
    };
    writeln!(log, "{}", serde_json::to_string(&request)?)?;
    let mut out = std::io::stdout().lock();
    if include {
        write!(out, "HTTP/2.0 {status} {reason}\r\n")?;
        if let Some(tag) = answer.as_ref().and_then(|answer| answer.etag.as_deref()) {
            write!(out, "Etag: {tag}\r\n")?;
        }
        write!(out, "\r\n")?;
    }
    if status != 200 {
        writeln!(std::io::stderr(), "gh: {reason} (HTTP {status})")?;
        return Ok(ExitCode::from(1));
    }
    match answer.map(|answer| answer.body) {
        Some(serde_json::Value::String(text)) => write!(out, "{text}")?,
        Some(body) => write!(out, "{body}")?,
        None => {}
    }
    Ok(ExitCode::SUCCESS)
}
