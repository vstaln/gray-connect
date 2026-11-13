//! gray-connect CLI entry point.
//!
//! `manifest`, `accounts`, `usage`, `help` and `version` run as ordinary
//! shell commands; no arguments starts the NDJSON sidecar protocol over
//! stdio.

use std::io::IsTerminal;

use gray_connect::{Reply, accounts, handle, manifest};
use serde::Deserialize;
use serde_json::{Value, json};

const USAGE: &str = "gray-connect — provider account pools and usage

usage: gray-connect <command> [args]
       (the same commands run as `gray connect <command>` in the gray CLI)

commands:
  accounts          the /connect panel (pin, add, remove, strategy)
  usage             usage cards for every provider pool, as plain text
  manifest          print the plugin manifest (used by gray at install)
  version           print the version
  help              show this text

with no arguments on a pipe, gray-connect runs the NDJSON sidecar protocol
on stdio (that is how gray talks to it).";

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("manifest") => {
            println!("{}", serde_json::to_string(&manifest::manifest())?);
            Ok(())
        }
        Some("accounts" | "connect") => accounts::run(),
        Some("usage") => {
            println!("{}", accounts::text());
            Ok(())
        }
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some("-V" | "--version" | "version") => {
            println!("gray-connect {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command '{other}'\n\n{USAGE}");
            std::process::exit(2);
        }
        // A TTY stdin can never be the host (piped stdin is the spawn
        // signal), so bare `gray connect` in a terminal means "show me what
        // this is" — print help instead of waiting on sidecar frames.
        None if std::io::stdin().is_terminal() => {
            println!("{USAGE}");
            Ok(())
        }
        None => sidecar(),
    }
}

/// NDJSON sidecar: one JSON request per stdin line, one reply per stdout
/// line. Commands run in order — the panel owns the terminal while it is
/// open, so a second command cannot interleave with it.
fn sidecar() -> anyhow::Result<()> {
    use std::io::BufRead as _;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(_) => continue,
        };
        let has_id = request.id.is_some();
        if !has_id && request.method != "plugin/shutdown" {
            continue;
        }
        let id = request.id.clone().unwrap_or(Value::Null);
        match handle(&request.method, &request.params, has_id) {
            Reply::Result(result) => gray_connect::emit(&json!({ "id": id, "result": result })),
            Reply::ResultThenExit(result) => {
                gray_connect::emit(&json!({ "id": id, "result": result }));
                return Ok(());
            }
            Reply::MethodNotFound => gray_connect::emit(&json!({
                "id": id,
                "error": { "code": -32601, "message": "method not found" },
            })),
            Reply::Exit => return Ok(()),
        }
    }
    Ok(())
}
