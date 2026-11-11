//! gray-connect: a protocol-1.1 sidecar and CLI plugin for gray.
//!
//! `/connect` (alias `/accounts`) is the provider account-pool panel; it
//! moved here out of gray-account, which is registry login and plugin
//! making. `/usage` prints the same usage cards as plain text.

pub mod accounts;
pub mod manifest;

use std::path::PathBuf;

use serde_json::{Value, json};

/// One NDJSON frame to stdout.
pub fn emit(frame: &Value) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{frame}");
    let _ = out.flush();
}

/// The gray state directory: `GRAY_HOME` when set and non-blank, else the
/// user profile plus `.gray`.
pub fn gray_home() -> anyhow::Result<PathBuf> {
    std::env::var_os("GRAY_HOME")
        .filter(|v| !v.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|p| p.join(".gray")))
        .ok_or_else(|| {
            anyhow::anyhow!("cannot resolve home: set GRAY_HOME or the platform user profile")
        })
}

/// What the NDJSON loop should do after [`handle`] runs.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    /// Write this value as the response `result`, keep serving.
    Result(Value),
    /// Write this value as the response `result`, then exit 0.
    ResultThenExit(Value),
    /// Write a JSON-RPC-style error reply, keep serving.
    MethodNotFound,
    /// Write nothing, exit 0.
    Exit,
}

/// Pure dispatch for one sidecar request. `has_id` distinguishes requests
/// (which get a reply) from notifications (which do not).
pub fn handle(method: &str, params: &Value, has_id: bool) -> Reply {
    match method {
        "plugin/manifest" => Reply::Result(manifest::manifest()),
        "command/run" => Reply::Result(command_run(params)),
        "plugin/shutdown" => {
            if has_id {
                Reply::ResultThenExit(json!({}))
            } else {
                Reply::Exit
            }
        }
        _ => Reply::MethodNotFound,
    }
}

fn command_run(params: &Value) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match name {
        // Both hand the terminal to `gray-connect accounts`; `text` is what
        // a host without a tty shows instead.
        "/connect" | "/accounts" => json!({
            "terminal": ["accounts"],
            "text": accounts::handoff_text(),
        }),
        "/usage" => json!({ "text": accounts::text() }),
        _ => json!({ "text": "" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_claims_connect_and_usage() {
        let m = manifest::manifest();
        assert_eq!(m["name"], "connect");
        assert_eq!(m["protocol"], "1.1");
        assert_eq!(m["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(m["commands"], json!(["/connect", "/accounts", "/usage"]));
        assert_eq!(m["tools"], json!([]));
        assert_eq!(m["hooks"], json!([]));
    }

    #[test]
    fn unknown_command_returns_empty_text() {
        let reply = handle("command/run", &json!({ "name": "/nope" }), true);
        assert_eq!(reply, Reply::Result(json!({ "text": "" })));
    }

    #[test]
    fn shutdown_with_an_id_replies_then_exits() {
        assert_eq!(
            handle("plugin/shutdown", &json!({}), true),
            Reply::ResultThenExit(json!({}))
        );
        assert_eq!(handle("plugin/shutdown", &json!({}), false), Reply::Exit);
    }
}
