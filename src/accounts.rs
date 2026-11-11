//! `/connect` (alias `/accounts`) — provider account pools panel.
//!
//! Moved out of gray core (`repl/accounts_panel.rs`) and then out of
//! gray-account: this plugin owns the whole surface. Two credential-
//! ownership models show up here:
//!
//! - **Plugin-owned pools**: a `*-sub` plugin that answers
//!   `<cli> account list --json` (the account contract) is listed with its
//!   accounts, and mutations run the same CLI: `use`/`remove`/`strategy`
//!   plus `add` (a TUI suspend — the provider's own login needs the real
//!   tty).
//! - **Vault pools**: `auth.json` v2 plugin entries
//!   (`plugin:<lock>:<provider>:<method>`) hold labeled credential
//!   envelopes the host serves. Pin, unpin, strategy, and remove mutate
//!   the file directly under the `auth.json.lock` protocol.
//!
//! Every row it shows and every mutation it runs goes through the same
//! CLI verbs or file fields a user can touch by hand.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde_json::{Value, json};

use crate::gray_home;

/// How long a probe may run before the panel moves on without it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
const USAGE_TIMEOUT: Duration = Duration::from_secs(12);

// ---------- vault (auth.json) ----------

/// Exclusive `auth.json.lock` guard — same protocol core uses: create
/// 0600, `try_lock` with a 5s deadline.
struct VaultLock {
    _file: std::fs::File,
}

fn auth_path() -> anyhow::Result<PathBuf> {
    Ok(gray_home()?.join("auth.json"))
}

fn vault_lock(path: &Path) -> anyhow::Result<VaultLock> {
    let lock_path = path.with_extension("json.lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&lock_path)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(VaultLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                anyhow::bail!("auth store lock timeout")
            }
            Err(std::fs::TryLockError::Error(e)) => {
                anyhow::bail!("auth store lock unavailable: {e}")
            }
        }
    }
}

fn vault_load(path: &Path) -> Value {
    let body = std::fs::read_to_string(path).unwrap_or_default();
    serde_json::from_str(&body).unwrap_or_else(|_| json!({}))
}

/// Atomic write: sibling tmp file, 0600, rename over the target.
fn vault_save(path: &Path, store: &Value) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(store)?;
    {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write as _;
        options.open(&tmp)?.write_all(&body)?;
    }
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// `plugin:<lock>:<provider>:<method>` refs → (auth_ref, lock, provider).
fn vault_refs(path: &Path) -> Vec<(String, String, String)> {
    vault_load(path)
        .as_object()
        .map(|m| m.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|k| k.starts_with("plugin:"))
        .filter_map(|k| {
            let mut parts = k.splitn(4, ':');
            let _ = parts.next();
            let lock = parts.next()?.to_string();
            let provider = parts.next()?.to_string();
            Some((k, lock, provider))
        })
        .collect()
}

fn pool<'a>(store: &'a Value, auth_ref: &str) -> Option<&'a Value> {
    store.get(auth_ref).filter(|v| v.get("accounts").is_some())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct VaultRow {
    label: String,
    email: Option<String>,
    cooling: bool,
    cooldown_until: u64,
    pinned: bool,
}

fn vault_rows(path: &Path, auth_ref: &str) -> Vec<VaultRow> {
    let store = vault_load(path);
    let now = now_secs();
    let Some(pool) = pool(&store, auth_ref) else {
        return Vec::new();
    };
    let selected = pool["selected"].as_str();
    pool["accounts"]
        .as_array()
        .map(|accounts| {
            accounts
                .iter()
                .filter_map(|a| {
                    let label = a["label"].as_str()?.to_string();
                    let until = a["cooldown_until"].as_u64().unwrap_or(0);
                    Some(VaultRow {
                        pinned: selected == Some(label.as_str()),
                        email: a["email"].as_str().map(str::to_string),
                        cooling: until > now,
                        cooldown_until: until,
                        label,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `(strategy, selected)` for a pool header; default fill_first.
fn vault_meta(path: &Path, auth_ref: &str) -> Option<(String, Option<String>)> {
    let store = vault_load(path);
    let pool = pool(&store, auth_ref)?;
    let strategy = pool["strategy"].as_str().unwrap_or("fill_first");
    let strategy = if strategy.is_empty() {
        "fill_first".to_string()
    } else {
        strategy.to_string()
    };
    Some((strategy, pool["selected"].as_str().map(str::to_string)))
}

fn vault_mutate<T>(
    path: &Path,
    auth_ref: &str,
    f: impl FnOnce(&mut Value) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let _lock = vault_lock(path)?;
    let mut store = vault_load(path);
    let pool = store
        .get_mut(auth_ref)
        .filter(|v| v.get("accounts").is_some())
        .with_context(|| format!("no plugin credential pool for {auth_ref}"))?;
    let out = f(pool)?;
    vault_save(path, &store)?;
    Ok(out)
}

fn vault_select(path: &Path, auth_ref: &str, label: Option<&str>) -> anyhow::Result<()> {
    vault_mutate(path, auth_ref, |pool| {
        match label {
            Some(label) => {
                let exists = pool["accounts"]
                    .as_array()
                    .is_some_and(|aa| aa.iter().any(|a| a["label"].as_str() == Some(label)));
                anyhow::ensure!(exists, "no account {label:?} in {auth_ref}");
                pool["selected"] = json!(label);
            }
            None => pool["selected"] = Value::Null,
        }
        Ok(())
    })
}

fn vault_strategy(path: &Path, auth_ref: &str, strategy: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(strategy, "fill_first" | "least_used"),
        "strategy must be fill_first or least_used"
    );
    vault_mutate(path, auth_ref, |pool| {
        pool["strategy"] = json!(strategy);
        Ok(())
    })
}

fn vault_remove(path: &Path, auth_ref: &str, label: &str) -> anyhow::Result<()> {
    vault_mutate(path, auth_ref, |pool| {
        let accounts = pool["accounts"]
            .as_array_mut()
            .context("pool has no accounts array")?;
        let before = accounts.len();
        accounts.retain(|a| a["label"].as_str() != Some(label));
        anyhow::ensure!(accounts.len() < before, "no account {label:?}");
        if pool["selected"].as_str() == Some(label) {
            pool["selected"] = Value::Null;
        }
        Ok(())
    })
}

// ---------- provider probing (lock.json + manifests + account verbs) ----------

struct Provider {
    /// Lock name (the `plugin:` auth_ref component).
    plugin: String,
    /// First provider id the manifest declares, for display.
    provider: String,
    argv: Option<Vec<String>>,
    doc: Option<Value>,
    probe_err: Option<String>,
}

fn lock_plugins(home: &Path) -> Vec<(String, Option<Vec<String>>)> {
    let path = home.join("plugins/lock.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(lock) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let mut out: Vec<(String, Option<Vec<String>>)> = Vec::new();
    let Some(map) = lock["plugins"].as_object() else {
        return out;
    };
    for (name, entry) in map {
        let enabled = entry["enabled"].as_bool().unwrap_or(true);
        if !enabled {
            continue;
        }
        let argv = entry["cli_argv"]
            .as_array()
            .or_else(|| entry["argv"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .filter(|a: &Vec<String>| !a.is_empty());
        out.push((name.clone(), argv));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The first provider id a plugin's cached manifest declares.
fn manifest_provider(home: &Path, plugin: &str) -> Option<String> {
    let text =
        std::fs::read_to_string(home.join("plugins").join(format!("{plugin}-manifest.json")))
            .ok()?;
    let m: Value = serde_json::from_str(&text).ok()?;
    m["providers"]
        .as_array()?
        .first()?
        .get("id")?
        .as_str()
        .map(str::to_string)
}

/// `<cli> account list --json` with a hard timeout; `Err` for nonzero
/// exits (a plugin without the verb) or timeouts.
fn probe_accounts(argv: &[String]) -> Result<Value, String> {
    run_json(argv, &["account", "list", "--json"], PROBE_TIMEOUT)
}

fn run_json(argv: &[String], verb: &[&str], timeout: Duration) -> Result<Value, String> {
    let (prog, base) = argv.split_first().ok_or("empty argv")?;
    let mut child = std::process::Command::new(prog)
        .args(base)
        .args(verb)
        .env(
            "GRAY_BIN",
            std::env::var_os("GRAY_EXE").unwrap_or_else(|| "gray".into()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(15)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("probe timed out".into());
            }
            Err(e) => return Err(format!("wait: {e}")),
        }
    }
    let out = child.wait_with_output().map_err(|e| format!("wait: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(err
            .lines()
            .next()
            .unwrap_or("account probe failed")
            .to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("bad account json: {e}"))
}

/// A mutation verb through the plugin CLI; `Ok(stdout)` on success.
fn account_cmd(argv: &[String], args: &[&str]) -> Result<String, String> {
    let (prog, base) = argv.split_first().ok_or("empty argv")?;
    let out = std::process::Command::new(prog)
        .args(base)
        .args(args)
        .env(
            "GRAY_BIN",
            std::env::var_os("GRAY_EXE").unwrap_or_else(|| "gray".into()),
        )
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(err
            .lines()
            .next()
            .unwrap_or("account command failed")
            .to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ---------- panel model ----------

struct UsageEntry {
    label: Option<String>,
    card: Option<Value>,
    error: Option<String>,
}

/// One account's usage card, as one explicit state.
enum Fetch {
    /// A fetch thread is in flight; the row shows `…`.
    Pending,
    Loaded(Value),
    Failed(String),
    /// This provider has no `account usage` verb (or lives in the vault).
    Unsupported,
}

/// A row a mutation can target: a plugin-pool account behind the CLI, or
/// an entry in the `auth.json` vault pool.
#[derive(Clone, PartialEq, Eq)]
enum Target {
    Cli { provider: usize, label: String },
    Vault { auth_ref: String, label: String },
}

/// Where the panel is. One enum, matched exhaustively — a prompt, a
/// confirmation, a detail page and a suspended terminal cannot overlap.
enum Mode {
    List,
    Adding {
        provider: usize,
        buf: String,
    },
    ConfirmRemove {
        target: Target,
    },
    Detail {
        plugin: String,
        label: Option<String>,
    },
    /// The TUI is closed and `<cli> account add <label>` owns the tty.
    Running {
        argv: Vec<String>,
        label: String,
    },
}

fn fetch_usage(argv: &[String]) -> Result<Vec<UsageEntry>, String> {
    let doc = run_json(argv, &["account", "usage", "--json"], USAGE_TIMEOUT)?;
    let list = doc["accounts"]
        .as_array()
        .ok_or("usage reply has no accounts list")?;
    Ok(list
        .iter()
        .map(|a| UsageEntry {
            label: a["label"].as_str().map(str::to_string),
            card: a["usage"].is_object().then(|| a["usage"].clone()),
            error: a["error"].as_str().map(str::to_string),
        })
        .collect())
}

fn order_key(p: &Provider) -> (u8, String) {
    let rank = match (p.plugin.as_str(), p.provider.as_str()) {
        ("claude-sub", _) | (_, "claude") => 0,
        ("codex-sub", _) | (_, "codex") => 1,
        _ => 2,
    };
    (rank, p.plugin.clone())
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn iso_epoch(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once(['T', ' '])?;
    let mut ymd = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (ymd.next()??, ymd.next()??, ymd.next()??);
    let cut = rest.find(['Z', '+', '-', '.']).unwrap_or(rest.len());
    let (clock, zone) = rest.split_at(cut);
    let mut hms = clock.split(':').map(|p| p.parse::<i64>().ok());
    let (hh, mi) = (hms.next()??, hms.next()??);
    let ss = hms.next().unwrap_or(Some(0))?;
    let zone = zone.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match zone.as_bytes().first().copied() {
        None | Some(b'Z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let (oh, om) = zone[1..].split_once(':')?;
            let secs = oh.parse::<i64>().ok()? * 3_600 + om.parse::<i64>().ok()? * 60;
            if sign == b'+' { secs } else { -secs }
        }
        _ => return None,
    };
    Some(days_from_civil(y, m, d) * 86_400 + hh * 3_600 + mi * 60 + ss - offset)
}

fn countdown(resets_at: &str, now: i64) -> Option<String> {
    let left = iso_epoch(resets_at)? - now;
    Some(if left >= 86_400 {
        format!("{}d", left / 86_400)
    } else if left >= 3_600 {
        format!("{}h{:02}", left / 3_600, left % 3_600 / 60)
    } else {
        format!("{}m", left.max(0) / 60)
    })
}

fn bar(pct: f64) -> String {
    let filled = (pct.clamp(0.0, 100.0) * 5.0 / 100.0).ceil() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(5 - filled))
}

fn window_cell(w: &Value, now: i64) -> Option<String> {
    let label = w["label"].as_str().or_else(|| w["id"].as_str())?;
    let pct = w["used_percent"].as_f64()?;
    let reset = w["resets_at"]
        .as_str()
        .and_then(|at| countdown(at, now))
        .map(|c| format!(" ↻{c}"))
        .unwrap_or_default();
    Some(format!("{label} {} {pct:.0}%{reset}", bar(pct)))
}

fn window_detail(w: &Value, now: i64) -> Option<String> {
    let label = w["label"].as_str().or_else(|| w["id"].as_str())?;
    let pct = w["used_percent"].as_f64()?;
    let reset = match w["resets_at"].as_str().and_then(|at| countdown(at, now)) {
        Some(c) => format!("resets in {c}"),
        None => "reset time unknown".to_string(),
    };
    Some(format!("{label:<5} {}  {pct:>3.0}%   {reset}", bar(pct)))
}

enum Row {
    Header { provider: usize },
    Account { provider: usize, label: String },
    Ambient { provider: usize },
    AddHint { provider: usize },
    Vault { auth_ref: String, label: String },
    Plain { provider: usize },
}

struct Panel {
    providers: Vec<Provider>,
    rows: Vec<Row>,
    sel: usize,
    status: Option<String>,
    mode: Mode,
    /// Per account (`None` = the provider's ambient login).
    usage: HashMap<(String, Option<String>), Fetch>,
    usage_gen: u64,
    usage_tx: Sender<(u64, String, Result<Vec<UsageEntry>, String>)>,
    usage_rx: Receiver<(u64, String, Result<Vec<UsageEntry>, String>)>,
}

impl Panel {
    fn load() -> Self {
        let (usage_tx, usage_rx) = std::sync::mpsc::channel();
        let mut p = Panel {
            providers: Vec::new(),
            rows: Vec::new(),
            sel: 0,
            status: None,
            mode: Mode::List,
            usage: HashMap::new(),
            usage_gen: 0,
            usage_tx,
            usage_rx,
        };
        p.reload();
        p
    }

    fn reload(&mut self) {
        let home = gray_home().unwrap_or_default();
        let auth = auth_path().unwrap_or_default();
        let mut providers: Vec<Provider> = Vec::new();
        for (name, argv) in lock_plugins(&home) {
            let Some(provider) = manifest_provider(&home, &name) else {
                continue;
            };
            let (doc, probe_err) = match &argv {
                Some(argv) => match probe_accounts(argv) {
                    Ok(v) => (Some(v), None),
                    Err(e) => (None, Some(e)),
                },
                None => (None, None),
            };
            providers.push(Provider {
                plugin: name,
                provider,
                argv,
                doc,
                probe_err,
            });
        }
        // Vault pools for providers without an answering plugin CLI get
        // their own group: the pool lives in auth.json.
        let served: std::collections::BTreeSet<String> = providers
            .iter()
            .filter(|p| p.doc.is_some())
            .map(|p| p.plugin.clone())
            .collect();
        for (auth_ref, lock, provider_id) in vault_refs(&auth) {
            if served.contains(&lock) {
                continue;
            }
            providers.push(Provider {
                plugin: lock,
                provider: provider_id,
                argv: None,
                doc: Some(json!({"_vault": auth_ref})),
                probe_err: None,
            });
        }
        providers.sort_by_key(order_key);
        self.providers = providers;
        self.rebuild_rows();
        if self.sel >= self.rows.len() {
            self.sel = self.rows.len().saturating_sub(1);
        }
    }

    fn rebuild_rows(&mut self) {
        let auth = auth_path().unwrap_or_default();
        self.rows.clear();
        for (i, p) in self.providers.iter().enumerate() {
            self.rows.push(Row::Header { provider: i });
            if let Some(doc) = &p.doc {
                if let Some(auth_ref) = doc.get("_vault").and_then(Value::as_str) {
                    for r in vault_rows(&auth, auth_ref) {
                        self.rows.push(Row::Vault {
                            auth_ref: auth_ref.to_string(),
                            label: r.label.clone(),
                        });
                    }
                } else {
                    match doc["accounts"].as_array() {
                        Some(list) if !list.is_empty() => {
                            for a in list {
                                if let Some(label) = a["label"].as_str() {
                                    self.rows.push(Row::Account {
                                        provider: i,
                                        label: label.to_string(),
                                    });
                                }
                            }
                        }
                        _ => self.rows.push(Row::Ambient { provider: i }),
                    }
                    if p.argv.is_some() {
                        self.rows.push(Row::AddHint { provider: i });
                    }
                }
            } else {
                self.rows.push(Row::Plain { provider: i });
            }
        }
    }

    fn start_usage(&mut self) {
        self.usage_gen += 1;
        self.usage.clear();
        for p in &self.providers {
            let Some(argv) = p.argv.clone() else {
                continue;
            };
            // Every key this provider can answer, so a row never has to
            // guess whether a fetch is in flight.
            let mut labels: Vec<Option<String>> = p
                .doc
                .as_ref()
                .and_then(|d| d["accounts"].as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|a| a["label"].as_str().map(str::to_string))
                        .map(Some)
                        .collect()
                })
                .unwrap_or_default();
            labels.push(None);
            for label in labels {
                self.usage.insert((p.plugin.clone(), label), Fetch::Pending);
            }
            let (tx, generation, plugin) =
                (self.usage_tx.clone(), self.usage_gen, p.plugin.clone());
            std::thread::spawn(move || {
                let _ = tx.send((generation, plugin, fetch_usage(&argv)));
            });
        }
    }

    fn accept(
        &mut self,
        (generation, plugin, fetched): (u64, String, Result<Vec<UsageEntry>, String>),
    ) {
        if generation != self.usage_gen {
            return;
        }
        let keys: Vec<(String, Option<String>)> = self
            .usage
            .keys()
            .filter(|(owner, _)| *owner == plugin)
            .cloned()
            .collect();
        let entries = match fetched {
            Ok(entries) => entries,
            Err(e) => {
                for key in keys {
                    self.usage.insert(key, Fetch::Failed(e.clone()));
                }
                return;
            }
        };
        for key in keys {
            let state = match entries.iter().find(|e| e.label == key.1) {
                Some(entry) => match (&entry.card, &entry.error) {
                    (Some(card), _) => Fetch::Loaded(card.clone()),
                    (None, Some(error)) => Fetch::Failed(error.clone()),
                    (None, None) => Fetch::Unsupported,
                },
                None => Fetch::Unsupported,
            };
            self.usage.insert(key, state);
        }
    }

    fn pump_usage(&mut self) {
        while let Ok(msg) = self.usage_rx.try_recv() {
            self.accept(msg);
        }
    }

    fn await_usage(&mut self, budget: Duration) {
        let deadline = Instant::now() + budget;
        while self.usage.values().any(|u| matches!(u, Fetch::Pending)) {
            match self
                .usage_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(msg) => self.accept(msg),
                Err(_) => break,
            }
        }
    }

    fn refresh(&mut self) {
        self.reload();
        self.start_usage();
    }

    fn usage_cell(&self, plugin: &str, label: Option<&str>) -> String {
        let key = (plugin.to_string(), label.map(str::to_string));
        let cells = match self.usage.get(&key) {
            Some(Fetch::Pending) => return "  …".to_string(),
            Some(Fetch::Loaded(card)) => {
                let now = now_secs() as i64;
                card["windows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(2)
                    .filter_map(|w| window_cell(w, now))
                    .collect::<Vec<_>>()
                    .join("  ")
            }
            _ => String::new(),
        };
        if cells.is_empty() {
            cells
        } else {
            format!("  {cells}")
        }
    }

    /// The provider answered `account usage` for its ambient login with an
    /// error — no login, no quota.
    fn not_connected(&self, provider: usize) -> bool {
        let key = (self.providers[provider].plugin.clone(), None);
        matches!(self.usage.get(&key), Some(Fetch::Failed(_)))
    }

    /// What a `p`/`c`/`d` applies to right now.
    fn target(&self) -> Option<Target> {
        match self.selected_row()? {
            Row::Account { provider, label } => Some(Target::Cli {
                provider: *provider,
                label: label.clone(),
            }),
            Row::Vault { auth_ref, label } => Some(Target::Vault {
                auth_ref: auth_ref.clone(),
                label: label.clone(),
            }),
            _ => None,
        }
    }

    fn open_detail(&mut self) {
        let next = match self.selected_row() {
            Some(Row::Account { provider, label }) => Some((*provider, Some(label.clone()))),
            Some(Row::Ambient { provider }) => Some((*provider, None)),
            _ => None,
        };
        if let Some((provider, label)) = next {
            self.mode = Mode::Detail {
                plugin: self.providers[provider].plugin.clone(),
                label,
            };
        }
    }

    fn detail_lines(&self, plugin: &str, label: Option<&str>) -> Vec<String> {
        let now = now_secs() as i64;
        let Some(p) = self.providers.iter().find(|p| p.plugin == plugin) else {
            return Vec::new();
        };
        let who = label.unwrap_or("ambient login");
        let mut out = vec![format!("{who}   {}  ({})", p.plugin, p.provider)];
        let key = (plugin.to_string(), label.map(str::to_string));
        match self.usage.get(&key) {
            Some(Fetch::Pending) | None => out.push("loading usage…".to_string()),
            Some(Fetch::Unsupported) => out.push("no usage from this plugin".to_string()),
            Some(Fetch::Failed(e)) => out.push(format!("error: {e}")),
            Some(Fetch::Loaded(card)) => {
                if let Some(plan) = card["plan"].as_str() {
                    out.push(format!("plan {plan}"));
                }
                for w in card["windows"].as_array().into_iter().flatten() {
                    out.extend(window_detail(w, now));
                }
                if let Some(note) = card["note"].as_str() {
                    out.push(note.to_string());
                }
            }
        }
        out
    }

    fn do_pin(&mut self) {
        let pinned = match self.selected_row() {
            Some(Row::Account { provider, label }) => self.providers[*provider]
                .doc
                .as_ref()
                .and_then(|d| d["accounts"].as_array())
                .is_some_and(|list| {
                    list.iter().any(|a| {
                        a["label"].as_str() == Some(label.as_str())
                            && a["pinned"].as_bool() == Some(true)
                    })
                }),
            Some(Row::Vault { auth_ref, label }) => {
                vault_rows(&auth_path().unwrap_or_default(), auth_ref)
                    .iter()
                    .any(|r| r.label == *label && r.pinned)
            }
            _ => false,
        };
        self.do_use(pinned);
    }

    fn row_text(&self, row: &Row) -> (String, bool) {
        let auth = auth_path().unwrap_or_default();
        match row {
            Row::Header { provider } => {
                let p = &self.providers[*provider];
                let strategy = p
                    .doc
                    .as_ref()
                    .and_then(|d| {
                        if d.get("_vault").is_some() {
                            let auth_ref = d["_vault"].as_str().unwrap_or_default();
                            vault_meta(&auth, auth_ref).map(|(st, _)| st)
                        } else {
                            d["strategy"].as_str().map(str::to_string)
                        }
                    })
                    .unwrap_or_default();
                let mut line = format!("{}  ({})", p.plugin, p.provider);
                if !strategy.is_empty() {
                    line.push_str(&format!("   strategy {strategy}"));
                }
                if let Some(e) = &p.probe_err {
                    line.push_str(&format!("   — {e}"));
                }
                (line, true)
            }
            Row::Account { provider, label } => {
                let p = &self.providers[*provider];
                let a = p.doc.as_ref().and_then(|d| {
                    d["accounts"]
                        .as_array()?
                        .iter()
                        .find(|x| x["label"].as_str() == Some(label))
                });
                let Some(a) = a else {
                    return (format!("    {label}"), false);
                };
                let pin = if a["pinned"].as_bool() == Some(true) {
                    "●"
                } else {
                    " "
                };
                let who = a["email"].as_str().unwrap_or("identity pending");
                let org = a["org"]
                    .as_str()
                    .map(|o| format!(" · {o}"))
                    .unwrap_or_default();
                let usage = self.usage_cell(&p.plugin, Some(label.as_str()));
                let mut line = format!("  {pin} {label:<12} {who}{org}{usage}");
                if a["cooling"].as_bool() == Some(true) {
                    let left = a["cooldown_until"]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_sub(now_secs());
                    line.push_str(&format!("   cooling {}m{:02}s", left / 60, left % 60));
                }
                if let Some(e) = a["last_error"].as_str() {
                    line.push_str(&format!("   ({e})"));
                }
                (line, false)
            }
            Row::Ambient { provider } => {
                if self.not_connected(*provider) {
                    ("    not connected — a to log in".to_string(), false)
                } else {
                    let plugin = &self.providers[*provider].plugin;
                    (
                        format!(
                            "    ambient login serves (no accounts yet){}",
                            self.usage_cell(plugin, None)
                        ),
                        false,
                    )
                }
            }
            Row::AddHint { .. } => ("    + add account".to_string(), false),
            Row::Vault { auth_ref, label } => {
                let detail = vault_rows(&auth, auth_ref)
                    .into_iter()
                    .find(|r| r.label == *label);
                let (pin, who, state) = match &detail {
                    Some(r) => (
                        if r.pinned { "●" } else { " " },
                        r.email.clone().unwrap_or_else(|| "credential".into()),
                        if r.cooling {
                            let left = r.cooldown_until.saturating_sub(now_secs());
                            format!("cooling {}m{:02}s", left / 60, left % 60)
                        } else {
                            "ok".to_string()
                        },
                    ),
                    None => (" ", "credential".to_string(), "?".to_string()),
                };
                (
                    format!("  {pin} {label:<16} {who}   vault · {state}"),
                    false,
                )
            }
            Row::Plain { provider } => {
                let p = &self.providers[*provider];
                (
                    format!("    {} — single login, no account pool", p.plugin),
                    false,
                )
            }
        }
    }

    fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.sel)
    }

    fn move_sel(&mut self, delta: i64) {
        if self.rows.is_empty() {
            return;
        }
        let n = self.rows.len() as i64;
        self.sel = ((self.sel as i64 + delta).rem_euclid(n)) as usize;
        // Moving off the row cancels a pending confirmation.
        if matches!(self.mode, Mode::ConfirmRemove { .. }) {
            self.mode = Mode::List;
        }
    }

    /// Run one mutation verb against a provider's CLI, then reload.
    fn run_account_on(&mut self, provider: usize, args: &[&str]) {
        let Some(argv) = self.providers[provider].argv.clone() else {
            return;
        };
        self.status = Some(match account_cmd(&argv, args) {
            Ok(msg) => msg,
            Err(e) => e,
        });
        self.reload();
    }

    /// The provider the selected row belongs to, if any.
    fn row_provider(&self) -> Option<usize> {
        match self.selected_row()? {
            Row::Header { provider }
            | Row::Account { provider, .. }
            | Row::Ambient { provider }
            | Row::AddHint { provider }
            | Row::Plain { provider } => Some(*provider),
            Row::Vault { .. } => None,
        }
    }

    fn run_account(&mut self, args: &[&str]) {
        if let Some(provider) = self.row_provider() {
            self.run_account_on(provider, args);
        }
    }

    fn toggle_strategy(&mut self) {
        let idx = match self.selected_row() {
            Some(
                Row::Header { provider }
                | Row::Account { provider, .. }
                | Row::Ambient { provider }
                | Row::AddHint { provider },
            ) => *provider,
            _ => return,
        };
        let p = &self.providers[idx];
        if p.argv.is_some() {
            let cur = p
                .doc
                .as_ref()
                .and_then(|d| d["strategy"].as_str())
                .unwrap_or("fill_first");
            let next = if cur == "fill_first" {
                "least_used"
            } else {
                "fill_first"
            };
            self.run_account(&["account", "strategy", next]);
        } else if let Some(auth_ref) = p
            .doc
            .as_ref()
            .and_then(|d| d["_vault"].as_str())
            .map(str::to_string)
        {
            let path = auth_path().unwrap_or_default();
            let cur = vault_meta(&path, &auth_ref)
                .map(|(s, _)| s)
                .unwrap_or_else(|| "fill_first".into());
            let next = if cur == "fill_first" {
                "least_used"
            } else {
                "fill_first"
            };
            self.status = match vault_strategy(&path, &auth_ref, next) {
                Ok(()) => Some(format!("strategy: {next}")),
                Err(e) => Some(e.to_string()),
            };
            self.reload();
        }
    }

    fn do_use(&mut self, clear: bool) {
        match self.target() {
            Some(Target::Cli { provider, label }) => {
                self.run_account_on(
                    provider,
                    &["account", "use", if clear { "--clear" } else { &label }],
                );
            }
            Some(Target::Vault { auth_ref, label }) => {
                let path = auth_path().unwrap_or_default();
                self.status = match vault_select(
                    &path,
                    &auth_ref,
                    if clear { None } else { Some(label.as_str()) },
                ) {
                    Ok(()) => Some(if clear {
                        "pin cleared".into()
                    } else {
                        format!("pinned {label}")
                    }),
                    Err(e) => Some(e.to_string()),
                };
                self.reload();
            }
            None => {}
        }
    }

    /// `d` twice: the first press opens the confirmation, the second runs it.
    fn do_remove(&mut self) {
        let Some(target) = self.target() else {
            return;
        };
        if matches!(&self.mode, Mode::ConfirmRemove { target: pending } if *pending == target) {
            self.mode = Mode::List;
            match target {
                Target::Cli { provider, label } => {
                    self.run_account_on(provider, &["account", "remove", &label]);
                }
                Target::Vault { auth_ref, label } => {
                    let path = auth_path().unwrap_or_default();
                    self.status = match vault_remove(&path, &auth_ref, &label) {
                        Ok(()) => Some(format!("removed {label}")),
                        Err(e) => Some(e.to_string()),
                    };
                    self.reload();
                }
            }
            return;
        }
        let label = match &target {
            Target::Cli { label, .. } | Target::Vault { label, .. } => label,
        };
        self.status = Some(format!("press d again to remove {label}"));
        self.mode = Mode::ConfirmRemove { target };
    }
}

// ---------- text (non-TTY) ----------

/// The same data the TUI renders, as plain text — the `/usage` path and
/// `gray-connect usage` on a pipe.
pub fn text() -> String {
    render_text(true)
}

pub fn handoff_text() -> String {
    render_text(false)
}

fn render_text(with_usage: bool) -> String {
    let mut panel = Panel::load();
    if with_usage {
        panel.start_usage();
        panel.await_usage(USAGE_TIMEOUT + Duration::from_secs(2));
    }
    panel.rebuild_rows();
    if panel.rows.is_empty() {
        return "no provider plugins installed — /connect to add one".to_string();
    }
    let mut out = String::new();
    for row in &panel.rows {
        let (text, header) = panel.row_text(row);
        if header && !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&text);
        out.push('\n');
    }
    out
}

// ---------- TUI ----------

enum Outcome {
    Close,
    /// Suspend the TUI and run `<cli> account add <label>` on the real tty.
    Add {
        argv: Vec<String>,
        label: String,
    },
}

/// Interactive `/connect`: the ratatui panel. Runs standalone as
/// `gray-connect accounts` (the TUI verb inside a `{"terminal":…}`
/// handoff).
pub fn run() -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        println!("{}", text());
        return Ok(());
    }
    let mut panel = Panel::load();
    panel.start_usage();
    loop {
        match tui_once(&mut panel)? {
            Outcome::Close => return Ok(()),
            Outcome::Add { argv, label } => {
                let (prog, base) = argv.split_first().context("empty argv")?;
                println!(
                    "\nrunning {prog} account add {label} — complete the login, then press Enter\n"
                );
                let status = std::process::Command::new(prog)
                    .args(base)
                    .args(["account", "add", &label])
                    .env(
                        "GRAY_BIN",
                        std::env::var_os("GRAY_EXE").unwrap_or_else(|| "gray".into()),
                    )
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .status()?;
                if !status.success() {
                    println!("account add exited {status}");
                }
                println!("\npress Enter to return to accounts");
                let mut line = String::new();
                let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line);
                panel.refresh();
            }
        }
    }
}

fn tui_once(panel: &mut Panel) -> anyhow::Result<Outcome> {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, poll, read};
    use crossterm::{cursor, execute, terminal};
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    const ACCENT: Color = Color::Rgb(246, 173, 126);

    terminal::enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend)?;
    let restore = |term: &mut Terminal<CrosstermBackend<std::io::Stdout>>| {
        let _ = execute!(
            term.backend_mut(),
            cursor::Show,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    };

    let outcome = loop {
        term.draw(|frame| {
            let area = frame.area();
            if area.width < 30 || area.height < 8 {
                return;
            }
            let modal_w = (area.width.saturating_sub(4))
                .clamp(56, 110)
                .min(area.width);
            let detail = match &panel.mode {
                Mode::Detail { plugin, label } => {
                    Some(panel.detail_lines(plugin, label.as_deref()))
                }
                _ => None,
            };
            let body_h = match &detail {
                Some(lines) => lines.len() as u16 + 5,
                None => panel.rows.len().max(1) as u16 + 7,
            };
            let modal_h = body_h.min(area.height.saturating_sub(2)).max(7);
            let x = area.x + (area.width.saturating_sub(modal_w)) / 2;
            let y = area.y + (area.height.saturating_sub(modal_h)) / 2;
            let rect = Rect::new(x, y, modal_w, modal_h);
            frame.render_widget(Clear, rect);
            let block = Block::default().borders(Borders::ALL).title(" Connect ");
            let inner = block.inner(rect);
            frame.render_widget(block, rect);

            let mut lines: Vec<Line> = Vec::new();
            match detail {
                Some(body) => {
                    for text in body {
                        lines.push(Line::from(text));
                    }
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        "esc back · q close",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                None => {
                    if panel.rows.is_empty() {
                        lines.push(Line::from(Span::styled(
                            "no provider plugins installed — /connect to add one",
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                    for (i, row) in panel.rows.iter().enumerate() {
                        let (text, header) = panel.row_text(row);
                        let selected = i == panel.sel;
                        let style = if selected {
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                        } else if header {
                            Style::default().add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        let marker = if selected { "› " } else { "  " };
                        lines.push(Line::from(Span::styled(format!("{marker}{text}"), style)));
                    }
                    lines.push(Line::from(""));
                    if let Mode::Adding { buf, .. } = &panel.mode {
                        lines.push(Line::from(Span::styled(
                            format!("new account — label [home]: {buf}▌"),
                            Style::default().fg(ACCENT),
                        )));
                    }
                    if let Some(msg) = &panel.status {
                        lines.push(Line::from(Span::styled(
                            msg.clone(),
                            Style::default().fg(Color::Yellow),
                        )));
                    }
                    lines.push(Line::from(Span::styled(
                        "enter detail · a add · p pin · c unpin · d remove · s strategy · r refresh · q close",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            frame.render_widget(Paragraph::new(lines), inner);
        })?;
        panel.pump_usage();

        if !poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(KeyEvent {
            code,
            kind,
            modifiers,
            ..
        }) = read()?
        else {
            continue;
        };
        if kind == KeyEventKind::Release {
            continue;
        }

        // One mode, one exhaustive match: no two handlers can both claim a
        // key, and no combo of fields has to be inspected to know where we
        // are.
        match (&mut panel.mode, code) {
            (Mode::Adding { .. }, KeyCode::Esc) => {
                panel.mode = Mode::List;
                panel.status = None;
            }
            (Mode::Adding { provider, buf }, KeyCode::Enter) => {
                let pi = *provider;
                let text = buf.trim().to_string();
                let argv = panel.providers[pi].argv.clone();
                let mut it = text.split_whitespace();
                match (it.next(), it.next(), argv) {
                    (Some(label), home, Some(argv)) => {
                        panel.mode = Mode::List;
                        if let Some(home) = home {
                            panel.run_account_on(pi, &["account", "add", label, "--home", home]);
                        } else {
                            panel.mode = Mode::Running {
                                argv,
                                label: label.to_string(),
                            };
                        }
                    }
                    _ => panel.status = Some("add needs a label".into()),
                }
            }
            (Mode::Adding { buf, .. }, KeyCode::Backspace) => {
                buf.pop();
            }
            (Mode::Adding { buf, .. }, KeyCode::Char(c)) => buf.push(c),
            (Mode::Adding { .. }, _) => {}

            (Mode::Detail { .. }, KeyCode::Esc) => panel.mode = Mode::List,
            (Mode::Detail { .. }, KeyCode::Char('q')) => break Outcome::Close,
            (Mode::Detail { .. }, KeyCode::Char('c'))
                if modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                break Outcome::Close;
            }
            (Mode::Detail { .. }, _) => {}

            // `run` drives a suspended `account add`; no key does.
            (Mode::Running { .. }, _) => {}

            (Mode::ConfirmRemove { target }, KeyCode::Char('d')) => {
                let again = target.clone();
                panel.mode = Mode::List;
                match again {
                    Target::Cli { provider, label } => {
                        panel.run_account_on(provider, &["account", "remove", &label]);
                    }
                    Target::Vault { auth_ref, label } => {
                        let path = auth_path().unwrap_or_default();
                        panel.status = match vault_remove(&path, &auth_ref, &label) {
                            Ok(()) => Some(format!("removed {label}")),
                            Err(e) => Some(e.to_string()),
                        };
                        panel.reload();
                    }
                }
            }
            // `esc` cancels; `q` closes the panel from anywhere but a prompt.
            (Mode::ConfirmRemove { .. }, KeyCode::Esc) => panel.mode = Mode::List,
            (Mode::ConfirmRemove { .. }, KeyCode::Char('q')) => break Outcome::Close,
            (Mode::ConfirmRemove { .. }, KeyCode::Up | KeyCode::Char('k')) => {
                panel.mode = Mode::List;
                panel.move_sel(-1);
            }
            (Mode::ConfirmRemove { .. }, KeyCode::Down | KeyCode::Char('j')) => {
                panel.mode = Mode::List;
                panel.move_sel(1);
            }
            (Mode::ConfirmRemove { .. }, _) => {}

            (Mode::List, KeyCode::Esc | KeyCode::Char('q')) => break Outcome::Close,
            (Mode::List, KeyCode::Char('c'))
                if modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                break Outcome::Close;
            }
            (Mode::List, KeyCode::Up | KeyCode::Char('k')) => panel.move_sel(-1),
            (Mode::List, KeyCode::Down | KeyCode::Char('j')) => panel.move_sel(1),
            (Mode::List, KeyCode::Enter) => panel.open_detail(),
            (Mode::List, KeyCode::Char('p')) => panel.do_pin(),
            (Mode::List, KeyCode::Char('c')) => panel.do_use(true),
            (Mode::List, KeyCode::Char('d')) => panel.do_remove(),
            (Mode::List, KeyCode::Char('s')) => panel.toggle_strategy(),
            (Mode::List, KeyCode::Char('r')) => panel.refresh(),
            (Mode::List, KeyCode::Char('a')) => match panel.row_provider() {
                Some(provider) if panel.providers[provider].argv.is_some() => {
                    panel.mode = Mode::Adding {
                        provider,
                        buf: String::new(),
                    };
                    panel.status = None;
                }
                Some(_) => {
                    panel.status =
                        Some("no account CLI for this provider — add via /connect".into());
                }
                None => {}
            },
            (Mode::List, _) => {}
        }
        // The prompt asked for a tty login: hand control back to `run`.
        panel.mode = match std::mem::replace(&mut panel.mode, Mode::List) {
            Mode::Running { argv, label } => break Outcome::Add { argv, label },
            rest => rest,
        };
    };
    restore(&mut term);
    Ok(outcome)
}
