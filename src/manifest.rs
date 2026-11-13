//! The plugin manifest gray reads at install and sidecar startup.

/// `/accounts` hands the terminal to the panel; `/usage`
/// prints the same usage data as plain text for a host without a tty, and
/// runs mid-turn instead of queueing behind the running turn.
pub fn manifest() -> serde_json::Value {
    serde_json::json!({
        "name": "connect",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [],
        "hooks": [],
        "capabilities": [],
        "commands": ["/accounts", "/usage"],
        "command_descriptions": {
            "/accounts": "Manage provider logins: add, pin, remove, rotate accounts",
            "/usage": "Show quota used and reset times for each login",
        },
        "completion": ["accounts", "usage"],
        "turn_local": { "/usage": ["*"] },
    })
}
