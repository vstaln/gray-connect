//! The plugin manifest gray reads at install and sidecar startup.

pub fn manifest() -> serde_json::Value {
    serde_json::json!({
        "name": "connect",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [],
        "hooks": [],
        "capabilities": [],
        "commands": ["/connect"],
        "command_descriptions": {
            "/connect": "Provider logins and usage: add, pin, remove, see quota",
        },
        "completion": ["connect"],
    })
}
