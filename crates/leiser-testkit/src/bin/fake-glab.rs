//! A fake GitLab CLI: answers `glab api [flags] <path>` from the answers
//! file in `GLAB_CONFIG_DIR` (see `leiser_testkit::FakeGlab`), as `glab`
//! does with `--include`: the status line, the headers, a blank line and
//! the body. A request whose `If-None-Match` names the answer's tag gets
//! `304 Not Modified` and, like `glab`, an exit code of 1. Also handles
//! `glab auth login` for testing host account logins.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use leiser_testkit::{FAKE_GLAB_REQUESTS_FILE, FakeGlab, FakeGlabRequest};

const SENSITIVE_ENV: [&str; 7] = [
    "GITLAB_TOKEN",
    "GITLAB_ACCESS_TOKEN",
    "OAUTH_TOKEN",
    "GITLAB_HOST",
    "GITLAB_URI",
    "GLAB_HOST",
    "CI_JOB_TOKEN",
];

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "fake-glab: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let config_dir =
        PathBuf::from(std::env::var_os("GLAB_CONFIG_DIR").ok_or("no GLAB_CONFIG_DIR")?);
    let leaked_env: Vec<String> = SENSITIVE_ENV
        .iter()
        .filter(|name| std::env::var_os(name).is_some())
        .map(|name| (*name).to_owned())
        .collect();
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("api") => run_api(&config_dir, leaked_env, args),
        Some("auth") => run_auth(&config_dir, leaked_env, args),
        other => Err(format!("unsupported glab subcommand: {other:?}").into()),
    }
}

fn run_auth(
    config_dir: &Path,
    leaked_env: Vec<String>,
    mut args: impl Iterator<Item = String>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if args.next().as_deref() != Some("login") {
        return Err("only `auth login` is faked".into());
    }
    let mut hostname = None;
    let mut fields = BTreeMap::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--hostname" | "-h" => hostname = args.next(),
            "--git-protocol" | "-g" => {
                if let Some(value) = args.next() {
                    fields.insert("git_protocol".to_owned(), value);
                }
            }
            "--api-protocol" | "-p" => {
                if let Some(value) = args.next() {
                    fields.insert("api_protocol".to_owned(), value);
                }
            }
            _ => {}
        }
    }
    log_request(
        config_dir,
        &FakeGlabRequest {
            method: "AUTH_LOGIN".to_owned(),
            path: "auth login".to_owned(),
            hostname: hostname.clone(),
            if_none_match: None,
            fields,
            leaked_env,
            status: 0,
        },
    )?;
    let host = hostname.as_deref().unwrap_or("gitlab.com");
    writeln!(std::io::stdout(), "Logged in to {host} (fake-glab)")?;
    Ok(ExitCode::SUCCESS)
}

fn run_api(
    config_dir: &Path,
    leaked_env: Vec<String>,
    mut args: impl Iterator<Item = String>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut include = false;
    let mut if_none_match = None;
    let mut hostname = None;
    let mut method = "GET".to_owned();
    let mut fields = BTreeMap::new();
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
            "--hostname" => hostname = Some(args.next().ok_or("--hostname without a value")?),
            "--method" | "-X" => {
                method = args
                    .next()
                    .ok_or("--method without a value")?
                    .to_ascii_uppercase();
            }
            "--raw-field" | "-f" | "--field" | "-F" => {
                let raw = args.next().ok_or("--raw-field without a value")?;
                let (key, value) = raw.split_once('=').ok_or("field without '='")?;
                fields.insert(key.to_owned(), value.to_owned());
            }
            _ => path = Some(arg),
        }
    }
    let path = path.ok_or("no request path")?;
    let mut answers = FakeGlab::new(config_dir).answers()?;
    let method_key = format!("{method} {path}");
    let answer = answers
        .remove(&method_key)
        .or_else(|| answers.remove(&path));
    let (status, reason) = match &answer {
        None => (404, "Not Found"),
        Some(answer) if answer.etag.is_some() && answer.etag == if_none_match => {
            (304, "Not Modified")
        }
        Some(_) => (200, "OK"),
    };
    log_request(
        config_dir,
        &FakeGlabRequest {
            method,
            path,
            hostname,
            if_none_match,
            fields,
            leaked_env,
            status,
        },
    )?;
    let mut out = std::io::stdout().lock();
    if include {
        write!(out, "HTTP/2.0 {status} {reason}\r\n")?;
        if let Some(tag) = answer.as_ref().and_then(|answer| answer.etag.as_deref()) {
            write!(out, "Etag: {tag}\r\n")?;
        }
        write!(out, "\r\n")?;
    }
    if status != 200 {
        writeln!(std::io::stderr(), "glab: {reason} (HTTP {status})")?;
        return Ok(ExitCode::from(1));
    }
    match answer.map(|answer| answer.body) {
        Some(serde_json::Value::String(text)) => write!(out, "{text}")?,
        Some(body) => write!(out, "{body}")?,
        None => {}
    }
    Ok(ExitCode::SUCCESS)
}

fn log_request(
    config_dir: &Path,
    request: &FakeGlabRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(config_dir)?;
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config_dir.join(FAKE_GLAB_REQUESTS_FILE))?;
    writeln!(log, "{}", serde_json::to_string(request)?)?;
    Ok(())
}
