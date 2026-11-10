//! Remote workspace bridge (feature `remote-bridge`).
//!
//! A project can be bound to a folder on another machine. The terminal and the
//! agents of that project then run there: this module keeps one
//! `ssh host oleafly-remoted` child per binding and relays the frontend's
//! calls to it as JSON lines (see `remote_spike.rs` for the other end).
//!
//! The bindings live in `remote-workspaces.json` in Oleafly's data folder:
//!
//! ```json
//! { "projects": { "<projectId>": {
//!     "host": "example-host", "path": "~/work/analysis",
//!     "binary": "/home/me/oleafly-remoted", "dataDir": "/home/me/.oleafly-remote" } } }
//! ```
//!
//! ssh runs through the same hardened arguments the research runs use: the
//! user's own OpenSSH, keys or agent only, no prompts, known hosts checked.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use oleafly_research::runs::remote::path_search;
use oleafly_research::runs::remote::scripts::sh_quote;
use oleafly_research::runs::remote::transport::{self, HostKeyMode, SshContext, SshEndpoint};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager, Runtime, State};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex};

/// The helper that comes with a release, fetched on first use for hosts of another kind.
mod helper_release;
/// Setting a remote workspace up from the app: the host check, the helper's installation, the
/// folder and the project. It reaches into this module's private binding file and connection code.
pub(crate) mod setup;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Binding {
    host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    main_doc: Option<String>,
    /// LaTeX engine for a project seeded on the host; `latexmk` unless set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    engine: Option<String>,
    /// Keep the daemon running on the host between connections (reattach after a drop).
    #[serde(default)]
    persistent: bool,
    binary: String,
    data_dir: String,
}

impl Binding {
    fn key(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.host,
            self.user.as_deref().unwrap_or(""),
            self.port.unwrap_or(0),
            self.binary,
            self.data_dir
        )
    }

    fn valid(&self) -> Result<(), String> {
        let plain = |text: &str| !text.is_empty() && !text.contains(['\0', '\r', '\n']);
        let host_ok = plain(&self.host)
            && !self.host.starts_with('-')
            && self
                .host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'));
        if !host_ok {
            return Err("The remote host is not a plain host name.".into());
        }
        if !plain(&self.path) || !plain(&self.binary) || !plain(&self.data_dir) {
            return Err("The remote workspace settings are incomplete.".into());
        }
        if !self.binary.starts_with('/') || !self.data_dir.starts_with('/') {
            return Err("The remote program and data folder need absolute paths.".into());
        }
        Ok(())
    }
}

#[derive(Default, Deserialize, Serialize)]
struct Config {
    #[serde(default)]
    projects: BTreeMap<String, Binding>,
}

fn config_file() -> Result<PathBuf, String> {
    Ok(crate::paths::oleafly_root()?.join("remote-workspaces.json"))
}

fn load_config() -> Config {
    config_file()
        .ok()
        .and_then(|file| std::fs::read(file).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The bindings file as a change must see it. `load_config` reads an unreadable file as empty,
/// which is right for looking up one binding but would, on a write, erase the user's other
/// bindings: here a file that exists and cannot be parsed is an error.
fn read_config_for_update(file: &Path) -> Result<Config, String> {
    match std::fs::read(file) {
        Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => Ok(Config::default()),
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "remote-workspaces.json cannot be read ({error}). Fix or remove it, then try again."
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(format!("remote-workspaces.json cannot be read: {error}")),
    }
}

/// Changes the bindings file under a lock, and replaces it atomically, so two windows (or a
/// setup and a hand edit) cannot lose each other's entries and a crash leaves the old file.
fn update_config<T>(change: impl FnOnce(&mut Config) -> Result<T, String>) -> Result<T, String> {
    let file = config_file()?;
    let folder = file
        .parent()
        .ok_or("Oleafly's data folder is missing.")?
        .to_path_buf();
    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(file.with_extension("lock"))
        .map_err(|error| error.to_string())?;
    fs4::FileExt::lock(&lock).map_err(|error| error.to_string())?;
    let mut config = read_config_for_update(&file)?;
    let result = change(&mut config)?;
    let text = serde_json::to_vec_pretty(&config).map_err(|error| error.to_string())?;
    let temporary = tempfile::NamedTempFile::new_in(&folder).map_err(|error| error.to_string())?;
    std::fs::write(temporary.path(), text).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o600));
    }
    temporary
        .persist(&file)
        .map_err(|error| error.error.to_string())?;
    Ok(result)
}

fn binding_for(project_id: &str) -> Result<Binding, String> {
    let binding = load_config()
        .projects
        .remove(project_id)
        .ok_or("This project has no remote workspace.")?;
    binding.valid()?;
    Ok(binding)
}

/// What a call returns: JSON, or raw bytes for the file commands that return a `Response`.
#[derive(Debug)]
enum Payload {
    Json(Value),
    Raw(Vec<u8>),
}

impl Payload {
    fn into_response(self) -> tauri::ipc::Response {
        match self {
            Payload::Json(value) => tauri::ipc::Response::new(value.to_string()),
            Payload::Raw(bytes) => tauri::ipc::Response::new(bytes),
        }
    }

    #[cfg(test)]
    fn json(self) -> Value {
        match self {
            Payload::Json(value) => value,
            Payload::Raw(bytes) => panic!("expected JSON, got {} raw bytes", bytes.len()),
        }
    }
}

type Reply = Result<Payload, Value>;

/// A channel the frontend opened (a terminal's output). It outlives any one connection, so a
/// terminal keeps its pane after a reconnect; `last` is the newest message it was given.
struct ChannelState {
    channel: Channel<Value>,
    last: Option<u64>,
}

/// What lives across connections: request and channel ids stay unique, so a late reply from a dead
/// link cannot answer a new call, and the channels are where replayed output goes.
struct Shared {
    next: AtomicU64,
    channels: Mutex<HashMap<u64, ChannelState>>,
    reconnecting: Mutex<HashSet<String>>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            next: AtomicU64::new(1),
            channels: Mutex::new(HashMap::new()),
            reconnecting: Mutex::new(HashSet::new()),
        }
    }
}

struct Conn {
    key: String,
    label: String,
    binding: Binding,
    shared: Arc<Shared>,
    /// The local `ssh` process, so a test can cut the link.
    #[cfg_attr(not(test), allow(dead_code))]
    pid: u32,
    stdin: AsyncMutex<ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    attached: Mutex<HashSet<String>>,
    stderr_tail: Mutex<String>,
    closed: AtomicBool,
    /// Set when the user ended the link, so it is not brought back.
    user_closed: AtomicBool,
    ready: Mutex<Option<oneshot::Sender<Value>>>,
}

#[derive(Default)]
pub struct RemoteBridge {
    conns: AsyncMutex<HashMap<String, Arc<Conn>>>,
    shared: Arc<Shared>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Conn {
    fn fail_all(&self, message: &str) {
        let pending: Vec<_> = lock(&self.pending).drain().collect();
        for (_, sender) in pending {
            let _ = sender.send(Err(Value::String(message.to_owned())));
        }
    }

    async fn send(
        &self,
        cmd: &str,
        args: Value,
        channel: Option<Channel<Value>>,
    ) -> Result<oneshot::Receiver<Reply>, String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(format!("The connection to {} is closed.", self.label));
        }
        let id = self.shared.next.fetch_add(1, Ordering::SeqCst);
        let mut request = json!({ "id": id, "cmd": cmd, "args": args });
        if let Some(channel) = channel {
            let channel_id = self.shared.next.fetch_add(1, Ordering::SeqCst);
            lock(&self.shared.channels).insert(
                channel_id,
                ChannelState {
                    channel,
                    last: None,
                },
            );
            request["channels"] = json!({ "channel": channel_id });
        }
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(id, sender);
        self.write_line(&request).await.inspect_err(|_| {
            lock(&self.pending).remove(&id);
        })?;
        Ok(receiver)
    }

    async fn write_line(&self, value: &Value) -> Result<(), String> {
        let mut line = value.to_string();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| format!("Could not reach {}: {error}", self.label))?;
        let _ = stdin.flush().await;
        Ok(())
    }

    async fn request(&self, cmd: &str, args: Value, channel: Option<Channel<Value>>) -> Reply {
        let receiver = self.send(cmd, args, channel).await.map_err(Value::String)?;
        receiver.await.unwrap_or_else(|_| {
            Err(Value::String(format!(
                "The connection to {} closed.",
                self.label
            )))
        })
    }

    async fn ensure_attached(&self, project_id: &str, binding: &Binding) -> Result<(), Value> {
        if lock(&self.attached).contains(project_id) {
            return Ok(());
        }
        self.request(
            "remote_attach",
            json!({ "projectId": project_id, "path": binding.path, "mainDoc": binding.main_doc, "engine": binding.engine }),
            None,
        )
        .await?;
        lock(&self.attached).insert(project_id.to_owned());
        Ok(())
    }
}

fn announce<R: Runtime>(app: &AppHandle<R>, host: &str, state: &str, detail: &str) {
    let _ = app.emit(
        "remote:state",
        json!({ "host": host, "state": state, "detail": detail }),
    );
}

/// Backoff for bringing a dropped link back: quick at first, then every 30 seconds, for about ten minutes.
const RECONNECT_DELAYS_SECS: [u64; 8] = [1, 2, 4, 8, 15, 30, 30, 30];
const RECONNECT_ATTEMPTS: usize = 24;

/// Whether some project is still bound to this host and helper. A binding that was removed (a setup
/// that failed, a hand edit) must not keep a link coming back.
fn still_bound(binding: &Binding) -> bool {
    let key = binding.key();
    load_config()
        .projects
        .values()
        .any(|other| other.key() == key)
}

async fn reconnect_loop<R: Runtime>(app: AppHandle<R>, binding: Binding) {
    let Some(bridge) = app.try_state::<RemoteBridge>() else {
        return;
    };
    let key = binding.key();
    if !still_bound(&binding) || !lock(&bridge.shared.reconnecting).insert(key.clone()) {
        return;
    }
    announce(
        &app,
        &binding.host,
        "reconnecting",
        "The connection dropped. Trying to reconnect.",
    );
    let mut restored = false;
    for attempt in 0..RECONNECT_ATTEMPTS {
        let wait = RECONNECT_DELAYS_SECS[attempt.min(RECONNECT_DELAYS_SECS.len() - 1)];
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        if !still_bound(&binding) {
            lock(&bridge.shared.reconnecting).remove(&key);
            return;
        }
        if connect(&app, &bridge, &binding).await.is_ok() {
            restored = true;
            break;
        }
    }
    lock(&bridge.shared.reconnecting).remove(&key);
    if !restored {
        announce(
            &app,
            &binding.host,
            "lost",
            "Could not reach the host again. Its terminals and agents may still be running there.",
        );
    }
}

fn connect<'a, R: Runtime>(
    app: &'a AppHandle<R>,
    bridge: &'a RemoteBridge,
    binding: &'a Binding,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Arc<Conn>, String>> + Send + 'a>> {
    Box::pin(connect_inner(app, bridge, binding))
}

async fn connect_inner<R: Runtime>(
    app: &AppHandle<R>,
    bridge: &RemoteBridge,
    binding: &Binding,
) -> Result<Arc<Conn>, String> {
    let key = binding.key();
    let mut conns = bridge.conns.lock().await;
    if let Some(existing) = conns.get(&key) {
        if !existing.closed.load(Ordering::SeqCst) {
            return Ok(existing.clone());
        }
    }
    let data_root = crate::paths::oleafly_root()?;
    let context = SshContext::new(&data_root, &|name| path_search(name)).map_err(String::from)?;
    let endpoint = SshEndpoint {
        host: binding.host.clone(),
        user: binding.user.clone(),
        port: binding.port,
    };
    let remote = format!(
        "env OLEAFLY_DATA_DIR={} {}{}",
        sh_quote(&binding.data_dir),
        sh_quote(&binding.binary),
        if binding.persistent { " --attach" } else { "" }
    );
    let mut args = transport::ssh_args(&context, &endpoint, &HostKeyMode::Strict, &remote);
    // The first value of an option wins in ssh, so these come before the defaults: a dead link
    // is noticed in about 30 seconds, not two minutes.
    let tuned: Vec<std::ffi::OsString> = [
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=3",
    ]
    .map(Into::into)
    .into();
    args.splice(1..1, tuned);
    let mut command = Command::new(&context.program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    transport::no_window(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start ssh: {error}"))?;
    let stdin = child.stdin.take().ok_or("ssh has no input.")?;
    let stdout = child.stdout.take().ok_or("ssh has no output.")?;
    let stderr = child.stderr.take().ok_or("ssh has no error output.")?;
    let (ready_tx, ready_rx) = oneshot::channel();
    let conn = Arc::new(Conn {
        key: key.clone(),
        label: binding.host.clone(),
        binding: binding.clone(),
        shared: bridge.shared.clone(),
        pid: child.id().unwrap_or(0),
        stdin: AsyncMutex::new(stdin),
        pending: Mutex::new(HashMap::new()),
        attached: Mutex::new(HashSet::new()),
        stderr_tail: Mutex::new(String::new()),
        closed: AtomicBool::new(false),
        user_closed: AtomicBool::new(false),
        ready: Mutex::new(Some(ready_tx)),
    });

    let errors = conn.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.starts_with("term: ") {
                continue;
            }
            let mut tail = lock(&errors.stderr_tail);
            tail.push_str(&line);
            tail.push('\n');
            let excess = tail.len().saturating_sub(2048);
            if excess > 0 {
                let cut = (excess..tail.len())
                    .find(|i| tail.is_char_boundary(*i))
                    .unwrap_or(0);
                tail.drain(..cut);
            }
        }
    });

    let reader = conn.clone();
    let events = app.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("ready").is_some() {
                let attached = message["attached"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                lock(&reader.attached).extend(attached);
                if message["stale"] == true {
                    announce(&events, &reader.label, "stale", "The program on the host was rebuilt, but it still has sessions running, so the older one keeps serving.");
                }
                if let Some(sender) = lock(&reader.ready).take() {
                    let _ = sender.send(message.clone());
                }
            } else if let Some(channel_id) = message.get("channel").and_then(Value::as_u64) {
                let body = message["message"].clone();
                let index = message["index"].as_u64();
                let finished = body.get("event").and_then(Value::as_str) == Some("exit");
                // Output replayed after a reconnect may overlap what already arrived: each message once.
                let deliver = {
                    let mut channels = lock(&reader.shared.channels);
                    match channels.get_mut(&channel_id) {
                        Some(state)
                            if index.is_some_and(|i| state.last.is_some_and(|last| i <= last)) =>
                        {
                            None
                        }
                        Some(state) => {
                            state.last = index.or(state.last);
                            Some(state.channel.clone())
                        }
                        None => None,
                    }
                };
                if let Some(channel) = deliver {
                    let _ = channel.send(body);
                }
                if finished {
                    lock(&reader.shared.channels).remove(&channel_id);
                }
            } else if message.get("gap").is_some() {
                announce(
                    &events,
                    &reader.label,
                    "gap",
                    "Some terminal output was lost while the connection was down.",
                );
            } else if let Some(id) = message.get("id").and_then(Value::as_u64) {
                let sender = lock(&reader.pending).remove(&id);
                if let Some(sender) = sender {
                    let reply = if let Some(error) = message.get("err") {
                        Err(error.clone())
                    } else if let Some(encoded) = message.get("okRaw").and_then(Value::as_str) {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD
                            .decode(encoded)
                            .map(Payload::Raw)
                            .map_err(|_| {
                                Value::String("The host sent unreadable file bytes.".into())
                            })
                    } else {
                        Ok(Payload::Json(
                            message.get("ok").cloned().unwrap_or(Value::Null),
                        ))
                    };
                    let _ = sender.send(reply);
                }
            } else if let Some(name) = message.get("event").and_then(Value::as_str) {
                if name == "resync" {
                    // The host sent things this link was not there for: have the app ask again.
                    let _ = events.emit("acp:resync", ());
                    let projects: Vec<String> = lock(&reader.attached).iter().cloned().collect();
                    for project in projects {
                        let _ = events.emit(
                            "project-folder-changed",
                            json!({ "projectId": project, "paths": [], "rescan": true, "structural": false }),
                        );
                    }
                } else if crate::remote_commands::EVENTS.contains(&name) {
                    let _ = events.emit(name, message["payload"].clone());
                }
            }
        }
        reader.closed.store(true, Ordering::SeqCst);
        // What ssh said on its way out is part of the answer to whoever is waiting for `ready`.
        let _ = stderr_task.await;
        let tail = lock(&reader.stderr_tail).trim().to_owned();
        let mut message = format!("The connection to {} closed.", reader.label);
        if !tail.is_empty() {
            message.push(' ');
            message.push_str(&tail);
        }
        reader.fail_all(&message);
        announce(&events, &reader.label, "disconnected", &message);
        let _ = child.wait().await;
        // A host that keeps its daemon between connections can be reached again; one that does not has lost its sessions.
        // Only a link that had come up is worth bringing back: one that never said `ready` failed to start the
        // program (a first connection, or a setup being checked), and retrying it would only repeat the failure.
        // Taking the sender also ends the wait in `connect` at once, instead of after its 30 seconds.
        let was_up = lock(&reader.ready).take().is_none();
        if reader.binding.persistent && was_up && !reader.user_closed.load(Ordering::SeqCst) {
            reconnect_loop(events, reader.binding.clone()).await;
        }
    });
    conns.insert(key, conn.clone());

    if binding.persistent {
        let resume: serde_json::Map<String, Value> = lock(&bridge.shared.channels)
            .iter()
            .map(|(id, state)| {
                (
                    id.to_string(),
                    json!(state.last.map_or(-1, |last| last as i64)),
                )
            })
            .collect();
        conn.write_line(&json!({ "hello": { "resume": resume } }))
            .await?;
    }
    drop(conns);
    match tokio::time::timeout(std::time::Duration::from_secs(30), ready_rx).await {
        Ok(Ok(_)) => {
            announce(app, &binding.host, "connected", "");
            Ok(conn)
        }
        _ => {
            conn.closed.store(true, Ordering::SeqCst);
            let tail = lock(&conn.stderr_tail).trim().to_owned();
            Err(format!(
                "Could not start the remote program on {}. {tail}",
                binding.host
            ))
        }
    }
}

fn routable(cmd: &str) -> bool {
    crate::remote_commands::routable(cmd)
}

async fn call<R: Runtime>(
    app: &AppHandle<R>,
    bridge: &RemoteBridge,
    project_id: &str,
    cmd: &str,
    args: Value,
    channel: Option<Channel<Value>>,
) -> Reply {
    if !routable(cmd) {
        return Err(Value::String(format!(
            "{cmd} cannot run on a remote workspace."
        )));
    }
    let binding = binding_for(project_id).map_err(Value::String)?;
    let conn = connect(app, bridge, &binding)
        .await
        .map_err(Value::String)?;
    conn.ensure_attached(project_id, &binding).await?;
    let result = conn.request(cmd, args, channel).await;
    if conn.closed.load(Ordering::SeqCst) {
        bridge.conns.lock().await.remove(&conn.key);
    }
    result
}

/// Tracing aid: records, once per command, every call that names a remote-bound
/// project but still ran on this computer. The list is what is left to port.
pub(crate) fn trace_local(command: &str, body: &tauri::ipc::InvokeBody) {
    use std::io::Write;
    use std::sync::OnceLock;
    let tauri::ipc::InvokeBody::Json(value) = body else {
        return;
    };
    let Some(project) = value.get("projectId").and_then(Value::as_str) else {
        return;
    };
    if !load_config().projects.contains_key(project) {
        return;
    }
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let first = lock(SEEN.get_or_init(Default::default)).insert(command.to_owned());
    if !first {
        return;
    }
    let keys: Vec<&str> = value
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    if let Ok(root) = crate::paths::oleafly_root() {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("remote-trace.log"))
        {
            let _ = writeln!(file, "{command} {keys:?}");
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingView {
    project_id: String,
    host: String,
    path: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteInfo {
    bindings: Vec<BindingView>,
    /// File commands that run on the host; agent and terminal commands go by prefix.
    files: Vec<&'static str>,
}

#[tauri::command]
pub fn remote_bindings() -> RemoteInfo {
    RemoteInfo {
        bindings: binding_views(),
        files: crate::remote_commands::FILES
            .iter()
            .chain(crate::remote_commands::GIT)
            .chain(crate::remote_commands::ASKED_HERE)
            .copied()
            .collect(),
    }
}

fn binding_views() -> Vec<BindingView> {
    let mut views: Vec<BindingView> = load_config()
        .projects
        .into_iter()
        .filter(|(_, binding)| binding.valid().is_ok())
        .map(|(project_id, binding)| BindingView {
            project_id,
            host: binding.host,
            path: binding.path,
        })
        .collect();
    views.sort_by(|a, b| a.project_id.cmp(&b.project_id));
    views
}

#[tauri::command]
pub async fn remote_call<R: Runtime>(
    app: AppHandle<R>,
    bridge: State<'_, RemoteBridge>,
    project_id: String,
    cmd: String,
    args: Option<Value>,
) -> Result<tauri::ipc::Response, Value> {
    call(
        &app,
        &bridge,
        &project_id,
        &cmd,
        args.unwrap_or_else(|| json!({})),
        None,
    )
    .await
    .map(Payload::into_response)
}

#[tauri::command]
pub async fn remote_call_channel<R: Runtime>(
    app: AppHandle<R>,
    bridge: State<'_, RemoteBridge>,
    project_id: String,
    cmd: String,
    args: Option<Value>,
    channel: Channel<Value>,
) -> Result<tauri::ipc::Response, Value> {
    call(
        &app,
        &bridge,
        &project_id,
        &cmd,
        args.unwrap_or_else(|| json!({})),
        Some(channel),
    )
    .await
    .map(Payload::into_response)
}

/// Trusting a folder lets code in it run, so the question is asked on this
/// computer even though the folder is on the host; the grant is stored there.
#[tauri::command]
pub async fn remote_trust_folder<R: Runtime>(
    webview: tauri::Webview<R>,
    app: AppHandle<R>,
    bridge: State<'_, RemoteBridge>,
    project_id: String,
    scope: Option<Value>,
) -> Result<Value, Value> {
    if webview.label() != "main" || webview.window().label() != "main" {
        return Err(Value::String(
            "Folder trust is only granted from the main window.".into(),
        ));
    }
    let binding = binding_for(&project_id).map_err(Value::String)?;
    match crate::trust::confirm_remote_folder(&app, &binding.host, &binding.path).await {
        Ok(true) => {}
        Ok(false) => {
            return Err(Value::String(String::from(
                crate::app_error::AppError::new("trust.declined"),
            )))
        }
        Err(error) => return Err(Value::String(error)),
    }
    let conn = connect(&app, &bridge, &binding)
        .await
        .map_err(Value::String)?;
    conn.ensure_attached(&project_id, &binding).await?;
    let reply = conn
        .request(
            "remote_trust",
            json!({ "projectId": project_id, "scope": scope.unwrap_or_else(|| json!("folder")) }),
            None,
        )
        .await?;
    let _ = app.emit("project-trust-changed", &project_id);
    match reply {
        Payload::Json(value) => Ok(value),
        Payload::Raw(_) => Err(Value::String("The host sent an unexpected reply.".into())),
    }
}

/// Ends the link without stopping the host's daemon: terminals and agents keep running there.
#[tauri::command]
pub async fn remote_disconnect(
    bridge: State<'_, RemoteBridge>,
    project_id: String,
) -> Result<(), String> {
    let binding = binding_for(&project_id)?;
    if let Some(conn) = bridge.conns.lock().await.remove(&binding.key()) {
        conn.user_closed.store(true, Ordering::SeqCst);
        conn.closed.store(true, Ordering::SeqCst);
        conn.fail_all("Disconnected.");
        // Closing the relay's input detaches this client; a persistent daemon carries on.
        let _ = conn.stdin.lock().await.shutdown().await;
    }
    Ok(())
}

/// Stops the daemon on the host: its terminals and agents end.
#[tauri::command]
pub async fn remote_stop_host<R: Runtime>(
    app: AppHandle<R>,
    bridge: State<'_, RemoteBridge>,
    project_id: String,
) -> Result<(), String> {
    let binding = binding_for(&project_id)?;
    let conn = connect(&app, &bridge, &binding).await?;
    conn.user_closed.store(true, Ordering::SeqCst);
    let _ = conn.write_line(&json!({ "cmd": "remote_exit" })).await;
    lock(&bridge.shared.channels).clear();
    bridge.conns.lock().await.remove(&binding.key());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::{Listener, Manager};

    fn host_ssh(host: &str, script: String) -> String {
        let out = std::process::Command::new("ssh")
            .args(["-o", "BatchMode=yes", host, &script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ssh failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// `REMOTE_BRIDGE_PERSISTENT=1` runs every test against a daemon that outlives its ssh link.
    fn persistent_flag() -> bool {
        std::env::var("REMOTE_BRIDGE_PERSISTENT").is_ok_and(|v| v == "1")
    }

    /// Needs a reachable host that has `oleafly-remoted`; run with
    /// `REMOTE_BRIDGE_HOST=... REMOTE_BRIDGE_BINARY=... REMOTE_BRIDGE_DATA=... \
    ///  REMOTE_BRIDGE_PATH=... cargo test -p oleafly --features remote-bridge \
    ///  remote_bridge -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a remote host"]
    fn drives_a_remote_terminal_and_agent_catalog() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        // A folder of its own, new each run: trust is remembered per folder.
        let folder = "~/oleafly-scratch/termtest";
        host_ssh(
            &var("REMOTE_BRIDGE_HOST"),
            format!("rm -rf {folder} && mkdir -p {folder}"),
        );
        // In tests `oleafly_root()` is its own temporary folder, not OLEAFLY_DATA_DIR.
        let _data_dir = crate::paths::data_dir_env_lock();
        std::fs::write(
            config_file().unwrap(),
            json!({ "projects": { "bridge1": {
                "host": var("REMOTE_BRIDGE_HOST"), "path": folder,
                "persistent": persistent_flag(), "binary": var("REMOTE_BRIDGE_BINARY"), "dataDir": var("REMOTE_BRIDGE_DATA") } } })
            .to_string(),
        )
        .unwrap();
        assert_eq!(remote_bindings().bindings.len(), 1);

        let app = tauri::test::mock_builder()
            .manage(RemoteBridge::default())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();
        let bridge = handle.state::<RemoteBridge>();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let shells = call(&handle, &bridge, "bridge1", "term_shells", json!({}), None).await;
            println!("term_shells -> {shells:?}");
            assert!(shells.is_ok());

            let seen = Arc::new(Mutex::new(String::new()));
            let sink = seen.clone();
            let channel = Channel::<Value>::new(move |body| {
                if let tauri::ipc::InvokeResponseBody::Json(text) = body {
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        if let Some(data) = value["data"].as_str() {
                            lock(&sink).push_str(data);
                        }
                    }
                }
                Ok(())
            });
            let opened = call(
                &handle,
                &bridge,
                "bridge1",
                "term_open",
                json!({ "projectId": "bridge1", "cols": 80, "rows": 24 }),
                Some(channel),
            )
            .await
            .expect("term_open")
            .json();
            println!("term_open -> {opened}");
            let id = opened["id"].as_str().unwrap().to_owned();
            call(
                &handle,
                &bridge,
                "bridge1",
                "term_write",
                json!({ "projectId": "bridge1", "id": id, "data": "echo BRIDGE_$((6*7)); pwd\n" }),
                None,
            )
            .await
            .expect("term_write");
            for _ in 0..100 {
                if lock(&seen).contains("BRIDGE_42") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            let output = lock(&seen).clone();
            println!(
                "terminal output tail: {:?}",
                &output[output.len().saturating_sub(160)..]
            );
            assert!(output.contains("BRIDGE_42"));
            call(
                &handle,
                &bridge,
                "bridge1",
                "term_kill",
                json!({ "projectId": "bridge1", "id": id }),
                None,
            )
            .await
            .expect("term_kill");

            let catalog = call(
                &handle,
                &bridge,
                "bridge1",
                "acp_catalog",
                json!({ "probe": false }),
                None,
            )
            .await
            .expect("acp_catalog")
            .json();
            println!(
                "acp_catalog entries: {}",
                catalog.as_array().map_or(0, Vec::len)
            );
            assert!(catalog.as_array().is_some_and(|list| !list.is_empty()));

            // Agent events: the daemon's `acp:event` / `acp:session` must come out of the
            // app as the same events the agent panel listens to.
            let agent_events = Arc::new(AtomicU64::new(0));
            let session_events = Arc::new(AtomicU64::new(0));
            let counter = agent_events.clone();
            handle.listen_any("acp:event", move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            });
            let counter = session_events.clone();
            handle.listen_any("acp:session", move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            });
            let agent = std::env::var("REMOTE_BRIDGE_AGENT").unwrap_or_else(|_| "opencode".into());
            // A folder on a host is untrusted until the user says so; agents refuse to start.
            let refused = call(
                &handle,
                &bridge,
                "bridge1",
                "acp_start",
                json!({ "projectId": "bridge1", "agentId": agent }),
                None,
            )
            .await;
            let refusal = refused
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            println!("acp_start before trust -> {refusal}");
            assert!(
                refusal.contains("trust.agents"),
                "an untrusted host folder must refuse agents"
            );
            // The grant itself, as `remote_trust_folder` sends it once the user confirmed here.
            let binding = binding_for("bridge1").unwrap();
            let conn = connect(&handle, &bridge, &binding).await.unwrap();
            let trust = conn
                .request(
                    "remote_trust",
                    json!({ "projectId": "bridge1", "scope": "folder" }),
                    None,
                )
                .await
                .expect("remote_trust");
            println!("remote_trust -> {trust:?}");
            let started = call(
                &handle,
                &bridge,
                "bridge1",
                "acp_start",
                json!({ "projectId": "bridge1", "agentId": agent }),
                None,
            )
            .await
            .expect("acp_start")
            .json();
            let session = started["session"]["id"].as_str().unwrap().to_owned();
            println!(
                "acp_start -> session {session} at {}",
                started["session"]["projectPath"]
            );
            for _ in 0..50 {
                if agent_events.load(Ordering::SeqCst) > 0
                    && session_events.load(Ordering::SeqCst) > 0
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            println!(
                "acp:event seen {}, acp:session seen {}",
                agent_events.load(Ordering::SeqCst),
                session_events.load(Ordering::SeqCst)
            );
            call(
                &handle,
                &bridge,
                "bridge1",
                "acp_disconnect",
                json!({ "projectId": "bridge1", "sessionId": session }),
                None,
            )
            .await
            .expect("acp_disconnect");
            assert!(
                agent_events.load(Ordering::SeqCst) > 0,
                "no acp:event reached the app"
            );
            assert!(
                session_events.load(Ordering::SeqCst) > 0,
                "no acp:session reached the app"
            );

            let refused = call(&handle, &bridge, "bridge1", "read_file", json!({}), None).await;
            assert!(
                refused.is_err(),
                "only acp_ and term_ commands may be relayed"
            );
            remote_disconnect_for_test(&bridge, "bridge1").await;
        });
        host_ssh(&var("REMOTE_BRIDGE_HOST"), format!("rm -rf {folder}"));
    }

    /// The text files live on the host: list, read, write, binary read, and the
    /// folder watcher telling the app about a change made there.
    #[test]
    #[ignore = "needs a remote host"]
    fn edits_the_project_files_on_the_host() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let _data_dir = crate::paths::data_dir_env_lock();
        let host = var("REMOTE_BRIDGE_HOST");
        // Its own folder and file: nothing here depends on what the editor did to another project.
        let folder = "~/oleafly-scratch/filetest".to_owned();
        host_ssh(&host, format!("rm -rf {folder} && mkdir -p {folder} && printf '%s\\n' 'Text that exists only on the host: REMOTE_TEXT_OK' > {folder}/paper.tex && echo 'print(1)' > {folder}/analysis.py"));
        std::fs::write(
            config_file().unwrap(),
            json!({ "projects": { "bridge2": {
                "host": host, "path": folder, "mainDoc": "paper.tex",
                "persistent": persistent_flag(), "binary": var("REMOTE_BRIDGE_BINARY"), "dataDir": var("REMOTE_BRIDGE_DATA") } } })
            .to_string(),
        )
        .unwrap();
        let app = tauri::test::mock_builder()
            .manage(RemoteBridge::default())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();
        let bridge = handle.state::<RemoteBridge>();
        let changes = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = changes.clone();
        handle.listen_any("project-folder-changed", move |event| {
            lock(&sink).push(event.payload().to_owned())
        });
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let run = |cmd: &'static str, args: Value| {
                let (handle, bridge) = (handle.clone(), bridge.clone());
                async move { call(&handle, &bridge, "bridge2", cmd, args, None).await }
            };
            let project = run("get_project", json!({ "projectId": "bridge2" })).await.expect("get_project").json();
            println!("get_project -> name {} main_doc {}", project["name"], project["main_doc"]);
            assert_eq!(project["main_doc"], "paper.tex");

            let tree = run("list_file_tree", json!({ "projectId": "bridge2" })).await.expect("list_file_tree").json();
            println!("list_file_tree -> {}", tree.to_string().chars().take(200).collect::<String>());
            assert!(tree.to_string().contains("paper.tex"), "the host's file is not in the tree");

            let text = run("read_file", json!({ "projectId": "bridge2", "path": "paper.tex" })).await.expect("read_file").json();
            println!("read_file -> {text}");
            assert!(text.as_str().is_some_and(|text| text.contains("REMOTE_TEXT_OK")));

            let written = run("write_file", json!({ "projectId": "bridge2", "path": "from_the_mac.tex", "content": "WRITTEN_FROM_THE_MAC\n" }))
                .await.expect("write_file").json();
            println!("write_file -> {written}");

            let raw = run("read_project_bytes", json!({ "projectId": "bridge2", "relPath": "from_the_mac.tex" })).await.expect("read_project_bytes");
            match raw {
                Payload::Raw(bytes) => assert_eq!(bytes, b"WRITTEN_FROM_THE_MAC\n"),
                other => panic!("expected raw bytes, got {other:?}"),
            }

            let token = run("watch_project_folder", json!({ "projectId": "bridge2" })).await.expect("watch_project_folder").json();
            println!("watch_project_folder -> {token}");
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            let touch = std::process::Command::new("ssh")
                .args(["-o", "BatchMode=yes", &var("REMOTE_BRIDGE_HOST"), &format!("echo '% touched on the host' >> {folder}/paper.tex")])
                .status()
                .unwrap();
            assert!(touch.success());
            for _ in 0..100 {
                if !lock(&changes).is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            println!("project-folder-changed seen: {:?}", lock(&changes));
            assert!(!lock(&changes).is_empty(), "no folder change reached the app");
            run("unwatch_project_folder", json!({ "projectId": "bridge2", "token": token })).await.expect("unwatch");

            run("delete_file", json!({ "projectId": "bridge2", "path": "from_the_mac.tex", "permanent": true })).await.expect("delete_file");
            remote_disconnect_for_test(&bridge, "bridge2").await;
        });
        host_ssh(&host, format!("rm -rf {folder}"));
    }

    /// Source control on the host: its own `git`, its own repository.
    #[test]
    #[ignore = "needs a remote host"]
    fn runs_git_on_the_host() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let _data_dir = crate::paths::data_dir_env_lock();
        let host = var("REMOTE_BRIDGE_HOST");
        let folder = "~/oleafly-scratch/gittest";
        let ssh = |script: String| {
            let out = std::process::Command::new("ssh")
                .args(["-o", "BatchMode=yes", &host, &script])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "ssh failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        ssh(format!(
            "rm -rf {folder} && mkdir -p {folder} && echo hello > {folder}/a.tex"
        ));
        std::fs::write(
            config_file().unwrap(),
            json!({ "projects": { "bridge3": {
                "host": host, "path": folder, "mainDoc": "a.tex",
                "persistent": persistent_flag(), "binary": var("REMOTE_BRIDGE_BINARY"), "dataDir": var("REMOTE_BRIDGE_DATA") } } })
            .to_string(),
        )
        .unwrap();
        let app = tauri::test::mock_builder()
            .manage(RemoteBridge::default())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();
        let bridge = handle.state::<RemoteBridge>();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let run = |cmd: &'static str, args: Value| {
                let (handle, bridge) = (handle.clone(), bridge.clone());
                async move {
                    call(&handle, &bridge, "bridge3", cmd, args, None)
                        .await
                        .map(Payload::json)
                }
            };
            let p = json!({ "projectId": "bridge3" });
            // Git can run hooks, so an untrusted host folder is refused until the user trusts it here.
            let refusal = run("git_is_initialized", p.clone())
                .await
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            println!("git_is_initialized before trust -> {refusal}");
            assert!(
                refusal.contains("trust.git"),
                "an untrusted host folder must refuse git"
            );
            let binding = binding_for("bridge3").unwrap();
            let conn = connect(&handle, &bridge, &binding).await.unwrap();
            conn.request(
                "remote_trust",
                json!({ "projectId": "bridge3", "scope": "folder" }),
                None,
            )
            .await
            .expect("remote_trust");
            println!(
                "git_is_initialized (before init) -> {:?}",
                run("git_is_initialized", p.clone()).await
            );
            let init = run("git_initialize", p.clone()).await;
            println!("git_initialize -> {init:?}");
            init.expect("git_initialize");
            let status = run("git_status", p.clone()).await.expect("git_status");
            println!("git_status -> {status}");
            assert!(
                status.to_string().contains("a.tex"),
                "the host's untracked file is not listed"
            );
            run("git_stage_all", p.clone())
                .await
                .expect("git_stage_all");
            let committed = run(
                "git_commit",
                json!({ "projectId": "bridge3", "message": "first commit from the app" }),
            )
            .await;
            println!("git_commit -> {committed:?}");
            committed.expect("git_commit");
            let log = run("git_log", p.clone()).await.expect("git_log");
            println!(
                "git_log -> {}",
                log.to_string().chars().take(220).collect::<String>()
            );
            assert!(log.to_string().contains("first commit from the app"));
            println!(
                "git_current_branch -> {:?}",
                run("git_current_branch", p.clone()).await
            );
            ssh(format!("echo edited >> {folder}/a.tex"));
            let diff = run(
                "git_diff",
                json!({ "projectId": "bridge3", "path": "a.tex", "staged": false }),
            )
            .await
            .expect("git_diff");
            println!(
                "git_diff -> {}",
                diff.to_string().chars().take(160).collect::<String>()
            );
            assert!(diff.to_string().contains("edited"));
            remote_disconnect_for_test(&bridge, "bridge3").await;
        });
        let on_host = ssh(format!("git -C {folder} log --oneline"));
        println!("git log on the host -> {on_host}");
        assert!(on_host.contains("first commit from the app"));
        let leftovers = ssh(format!("ls -A {folder}"));
        println!("host folder after -> {}", leftovers.replace('\n', " "));
        ssh(format!("rm -rf {folder}"));
    }

    /// A LaTeX project builds on the host with its own latexmk, and the PDF comes back.
    #[test]
    #[ignore = "needs a remote host"]
    fn compiles_on_the_host() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let _data_dir = crate::paths::data_dir_env_lock();
        let host = var("REMOTE_BRIDGE_HOST");
        let folder = "~/oleafly-scratch/compiletest";
        let ssh = |script: String| {
            let out = std::process::Command::new("ssh")
                .args(["-o", "BatchMode=yes", &host, &script])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "ssh failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        ssh(format!(
            "rm -rf {folder} ~/.oleafly-spike/linked/bridge4 && mkdir -p {folder} && printf '%s\\n' '\\documentclass{{article}}' '\\begin{{document}}' 'Compiled on the host: COMPILEOK' '\\end{{document}}' > {folder}/main.tex"
        ));
        std::fs::write(
            config_file().unwrap(),
            json!({ "projects": { "bridge4": {
                "host": host, "path": folder, "mainDoc": "main.tex",
                "persistent": persistent_flag(), "binary": var("REMOTE_BRIDGE_BINARY"), "dataDir": var("REMOTE_BRIDGE_DATA") } } })
            .to_string(),
        )
        .unwrap();
        let app = tauri::test::mock_builder()
            .manage(RemoteBridge::default())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();
        let bridge = handle.state::<RemoteBridge>();
        let logs = Arc::new(Mutex::new(0usize));
        let log_text = Arc::new(Mutex::new(String::new()));
        let (counter, text_sink) = (logs.clone(), log_text.clone());
        handle.listen_any("compile:log", move |event| {
            *lock(&counter) += 1;
            lock(&text_sink).push_str(event.payload());
        });
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let run = |cmd: &'static str, args: Value| {
                let (handle, bridge) = (handle.clone(), bridge.clone());
                async move { call(&handle, &bridge, "bridge4", cmd, args, None).await }
            };
            let compile = json!({ "projectId": "bridge4", "mainDoc": "main.tex" });
            // Choosing latexmk is what needs trust; building with it afterwards does not.
            let binding = binding_for("bridge4").unwrap();
            let conn = connect(&handle, &bridge, &binding).await.unwrap();
            // As `remote_trust_folder` does: the host must know the folder before it can trust it.
            conn.ensure_attached("bridge4", &binding)
                .await
                .expect("attach");
            conn.request(
                "remote_trust",
                json!({ "projectId": "bridge4", "scope": "folder" }),
                None,
            )
            .await
            .expect("remote_trust");

            println!(
                "project_trust_state -> {:?}",
                run("project_trust_state", json!({ "projectId": "bridge4" }))
                    .await
                    .map(Payload::json)
            );
            println!(
                "get_project -> {:?}",
                run("get_project", json!({ "projectId": "bridge4" }))
                    .await
                    .map(Payload::json)
            );
            println!(
                "project_engine -> {:?}",
                run("project_engine", json!({ "projectId": "bridge4" }))
                    .await
                    .map(Payload::json)
            );
            let status = run("project_tex_status", json!({ "projectId": "bridge4" }))
                .await
                .expect("project_tex_status")
                .json();
            println!("project_tex_status -> {status}");
            let result = run("compile_project", compile)
                .await
                .expect("compile_project")
                .json();
            println!(
                "compile_project -> ok {} has_pdf {} engine-log: {}",
                result["ok"],
                result["has_pdf"],
                result["log"]
                    .as_str()
                    .unwrap_or("")
                    .lines()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
            if result["ok"] != true {
                println!("compile errors -> {}", result["errors"]);
                println!("daemon stderr -> {}", lock(&conn.stderr_tail));
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                println!("compile:log text -> {}", lock(&log_text));
            }
            assert_eq!(result["ok"], true, "compile failed: {}", result["log"]);
            assert_eq!(result["has_pdf"], true);

            match run("read_compiled_pdf", json!({ "projectId": "bridge4" }))
                .await
                .expect("read_compiled_pdf")
            {
                Payload::Raw(bytes) => {
                    println!(
                        "read_compiled_pdf -> {} raw bytes, starts {:?}",
                        bytes.len(),
                        String::from_utf8_lossy(&bytes[..8.min(bytes.len())])
                    );
                    assert!(bytes.starts_with(b"%PDF") && bytes.len() > 1000);
                }
                other => panic!("expected raw bytes, got {other:?}"),
            }
            for _ in 0..30 {
                if *lock(&logs) > 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            println!("compile:log events seen: {}", *lock(&logs));
            assert!(*lock(&logs) > 0, "no compile:log reached the app");
            remote_disconnect_for_test(&bridge, "bridge4").await;
        });
        println!(
            "code folder after -> {}",
            ssh(format!("ls -A {folder}")).replace('\n', " ")
        );
        println!(
            "pdf under the daemon's state -> {}",
            ssh("find ~/.oleafly-spike/linked/bridge4 -name '*.pdf' | head -2".into())
        );
        assert_eq!(
            ssh(format!("ls -A {folder}")),
            "main.tex",
            "the build wrote into the code folder"
        );
        ssh(format!("rm -rf {folder}"));
    }

    /// The link is cut mid-run (the local ssh is killed). Without a persistent daemon the host's
    /// shell dies with it; with one (`REMOTE_BRIDGE_PERSISTENT=1`) the same shell keeps running and
    /// every tick arrives once and in order, ticks printed during the gap included.
    #[test]
    #[ignore = "needs a remote host"]
    fn survives_a_dropped_link() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let persistent = std::env::var("REMOTE_BRIDGE_PERSISTENT").is_ok_and(|v| v == "1");
        let _data_dir = crate::paths::data_dir_env_lock();
        let host = var("REMOTE_BRIDGE_HOST");
        let binary = var("REMOTE_BRIDGE_BINARY");
        let data = var("REMOTE_BRIDGE_DATA");
        let folder = "~/oleafly-scratch/droptest";
        let ssh = |script: String| {
            let out = std::process::Command::new("ssh")
                .args(["-o", "BatchMode=yes", &host, &script])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        // A daemon left from an earlier run would be answering: start from none.
        ssh(format!(
            "OLEAFLY_DATA_DIR={data} {binary} --stop; rm -rf {folder} && mkdir -p {folder}"
        ));
        std::fs::write(
            config_file().unwrap(),
            json!({ "projects": { "bridge5": {
                "host": host, "path": folder, "persistent": persistent,
                "binary": binary, "dataDir": data } } })
            .to_string(),
        )
        .unwrap();
        let app = tauri::test::mock_builder()
            .manage(RemoteBridge::default())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();
        let bridge = handle.state::<RemoteBridge>();
        let states = Arc::new(Mutex::new(Vec::<String>::new()));
        let state_sink = states.clone();
        handle.listen_any("remote:state", move |event| {
            if let Ok(value) = serde_json::from_str::<Value>(event.payload()) {
                lock(&state_sink).push(value["state"].as_str().unwrap_or("?").to_owned());
            }
        });
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let sink = seen.clone();
        let (pid_before, pid_after, write_ok) = runtime.block_on(async {
            let run = |cmd: &'static str, args: Value, channel: Option<Channel<Value>>| {
                let (handle, bridge) = (handle.clone(), bridge.clone());
                async move { call(&handle, &bridge, "bridge5", cmd, args, channel).await.map(Payload::json) }
            };
            let channel = Channel::<Value>::new(move |body| {
                if let tauri::ipc::InvokeResponseBody::Json(text) = body {
                    if let Some(data) = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["data"].as_str().map(str::to_owned)) {
                        lock(&sink).push_str(&data);
                    }
                }
                Ok(())
            });
            let opened = run("term_open", json!({ "projectId": "bridge5", "cols": 100, "rows": 24 }), Some(channel)).await.expect("term_open");
            let id = opened["id"].as_str().unwrap().to_owned();
            let pid_of = |text: &str, tag: &str| text.split(&format!("{tag}_1_")).nth(1).map(|rest| rest.chars().take_while(char::is_ascii_digit).collect::<String>());
            run("term_write", json!({ "projectId": "bridge5", "id": id, "data": "echo SHELLPID_$((1+0))_$$\n" }), None).await.expect("pid write");
            for _ in 0..100 {
                if pid_of(&lock(&seen), "SHELLPID").is_some_and(|p| !p.is_empty()) { break; }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            let pid_before = pid_of(&lock(&seen), "SHELLPID").unwrap_or_default();
            run("term_write", json!({ "projectId": "bridge5", "id": id, "data": "for i in $(seq 1 8); do echo TICK$i; sleep 1; done; echo ALL$((1+0))DONE\n" }), None).await.expect("term_write");
            for _ in 0..100 {
                if lock(&seen).contains("TICK2") { break; }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            assert!(lock(&seen).contains("TICK2"), "the ticks never started");
            let conn = bridge.conns.lock().await.values().next().cloned().expect("a connection");
            println!("cutting the link: killing local ssh pid {}", conn.pid);
            unsafe { libc::kill(conn.pid as i32, libc::SIGKILL) };
            // Ticks keep printing on the host while nobody is attached.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let after = run("term_write", json!({ "projectId": "bridge5", "id": id, "data": "echo AFTER_$((1+0))DROP; echo SHELLNOW_$((1+0))_$$\n" }), None).await;
            println!("term_write after the drop -> {after:?}");
            for _ in 0..200 {
                let text = lock(&seen).clone();
                if text.contains("ALL1DONE") && text.contains("AFTER_1DROP") && pid_of(&text, "SHELLNOW").is_some_and(|p| !p.is_empty()) { break; }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            let pid_after = pid_of(&lock(&seen), "SHELLNOW").unwrap_or_default();
            if persistent && after.is_ok() {
                // Same terminal, same daemon: end the shell cleanly, then exercise the stale-binary path.
                run("term_kill", json!({ "projectId": "bridge5", "id": id }), None).await.expect("term_kill");
            }
            (pid_before, pid_after, after.is_ok())
        });
        let text = lock(&seen).clone();
        let positions: Vec<Option<usize>> = (1..=8)
            .map(|n| text.find(&format!("TICK{n}\r\n")))
            .collect();
        let ticks: Vec<usize> = (1..=8).filter(|n| positions[n - 1].is_some()).collect();
        let once = (1..=8).all(|n| text.matches(&format!("TICK{n}\r\n")).count() == 1);
        let ordered = positions
            .windows(2)
            .all(|w| matches!((w[0], w[1]), (Some(a), Some(b)) if a < b));
        let daemons = ssh("pgrep -fc '[o]leafly-remoted --serve' || true".into());
        println!("persistent={persistent} shell pid {pid_before} -> {pid_after} write_after_ok={write_ok} ticks={ticks:?} once={once} ordered={ordered} daemons on the host={daemons} states={:?}", lock(&states));
        if persistent {
            assert!(
                !pid_before.is_empty() && pid_before == pid_after,
                "the shell changed across the drop: {pid_before} -> {pid_after}"
            );
            assert_eq!(
                ticks,
                (1..=8).collect::<Vec<_>>(),
                "ticks went missing across the drop"
            );
            assert!(once && ordered, "a tick was repeated or out of order");
            assert!(
                text.contains("ALL1DONE") && text.contains("AFTER_1DROP") && write_ok,
                "the terminal did not survive"
            );
            assert_eq!(
                daemons.trim(),
                "1",
                "there should be exactly one daemon on the host"
            );
            assert!(
                lock(&states).iter().any(|s| s == "disconnected")
                    && lock(&states).iter().any(|s| s == "connected"),
                "the app was not told"
            );

            // A rebuilt program replaces an idle daemon: the terminal is closed, so none is busy.
            let before = ssh(format!("cat {data}/remoted.pid"));
            ssh(format!("touch {binary}"));
            runtime.block_on(async { remote_disconnect_for_test(&bridge, "bridge5").await });
            let shells = runtime.block_on(async {
                call(&handle, &bridge, "bridge5", "term_shells", json!({}), None).await
            });
            let after = ssh(format!("cat {data}/remoted.pid"));
            println!(
                "stale binary: daemon pid {before} -> {after}, term_shells ok={}",
                shells.is_ok()
            );
            assert!(
                shells.is_ok() && !before.is_empty() && before != after,
                "an idle daemon on an older build should have been replaced"
            );
            runtime.block_on(async { remote_disconnect_for_test(&bridge, "bridge5").await });
            ssh(format!("OLEAFLY_DATA_DIR={data} {binary} --stop"));
        } else {
            assert!(
                !text.contains("ALL1DONE") && !write_ok,
                "baseline: the shell was expected to die with the link"
            );
            println!("BASELINE: the terminal is lost when the link drops");
            runtime.block_on(async { remote_disconnect_for_test(&bridge, "bridge5").await });
        }
        ssh(format!("rm -rf {folder}"));
    }

    /// An agent session is a child of the persistent daemon, so it outlives the link: after a
    /// reconnect the same session answers and the same agent process is still running.
    #[test]
    #[ignore = "needs a remote host"]
    fn an_agent_session_survives_a_dropped_link() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        assert!(
            persistent_flag(),
            "this test is about the persistent daemon: set REMOTE_BRIDGE_PERSISTENT=1"
        );
        let _data_dir = crate::paths::data_dir_env_lock();
        let host = var("REMOTE_BRIDGE_HOST");
        let binary = var("REMOTE_BRIDGE_BINARY");
        let data = var("REMOTE_BRIDGE_DATA");
        let folder = "~/oleafly-scratch/agentdrop";
        let ssh = |script: String| {
            let out = std::process::Command::new("ssh")
                .args(["-o", "BatchMode=yes", &host, &script])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        ssh(format!(
            "OLEAFLY_DATA_DIR={data} {binary} --stop; rm -rf {folder} && mkdir -p {folder}"
        ));
        std::fs::write(
            config_file().unwrap(),
            json!({ "projects": { "bridge6": {
                "host": host, "path": folder, "persistent": true, "binary": binary, "dataDir": data } } })
            .to_string(),
        )
        .unwrap();
        let app = tauri::test::mock_builder()
            .manage(RemoteBridge::default())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();
        let bridge = handle.state::<RemoteBridge>();
        let resyncs = Arc::new(Mutex::new(0usize));
        let counter = resyncs.clone();
        handle.listen_any("acp:resync", move |_| *lock(&counter) += 1);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let run = |cmd: &'static str, args: Value| {
                let (handle, bridge) = (handle.clone(), bridge.clone());
                async move { call(&handle, &bridge, "bridge6", cmd, args, None).await.map(Payload::json) }
            };
            let binding = binding_for("bridge6").unwrap();
            let conn = connect(&handle, &bridge, &binding).await.unwrap();
            conn.ensure_attached("bridge6", &binding).await.expect("attach");
            conn.request("remote_trust", json!({ "projectId": "bridge6", "scope": "folder" }), None).await.expect("remote_trust");
            let agent = std::env::var("REMOTE_BRIDGE_AGENT").unwrap_or_else(|_| "opencode".into());
            let started = run("acp_start", json!({ "projectId": "bridge6", "agentId": agent })).await.expect("acp_start");
            let session = started["session"]["id"].as_str().unwrap().to_owned();
            let agent_pids = || ssh("pgrep -u $USER -f '[o]pencode acp' | sort -n | tr '\\n' ' '".into());
            let before = agent_pids();
            println!("session {session} started, agent process(es) on the host: {before}");
            assert!(!before.trim().is_empty(), "no agent process on the host");

            // The agent is mid-turn when the link goes: the answer must still finish on the host.
            let cut = async {
                tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                unsafe { libc::kill(conn.pid as i32, libc::SIGKILL) };
            };
            let (prompt_result, ()) = tokio::join!(
                run("acp_prompt", json!({
                    "projectId": "bridge6", "sessionId": session,
                    "input": { "text": "Reply with exactly the word OK and nothing else.", "images": [], "files": [], "selections": [] }
                })),
                cut
            );
            println!("the prompt call, cut by the drop -> {}", match &prompt_result { Ok(_) => "finished before the cut".to_owned(), Err(e) => e.to_string().chars().take(80).collect() });
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let after_drop = agent_pids();
            println!("agent process(es) while the link is down: {after_drop}");
            assert_eq!(before, after_drop, "the agent process went away with the link");

            let snapshot = run("acp_snapshot", json!({ "projectId": "bridge6", "sessionId": session })).await.expect("acp_snapshot after reconnect");
            println!("acp_snapshot after the reconnect -> session {} status {}", snapshot["session"]["id"], snapshot["session"]["status"]);
            assert_eq!(snapshot["session"]["id"], json!(session));
            // The turn finishes on the host without us; its events are there to be read afterwards.
            let mut finished = false;
            for _ in 0..90 {
                let page = run("acp_events", json!({ "projectId": "bridge6", "sessionId": session, "after": 0, "limit": 300 })).await.expect("acp_events");
                let text = page.to_string();
                if text.contains("turn_complete") && text.contains("agent_message_chunk") {
                    finished = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            println!("the turn finished on the host while detached: {finished}");
            assert!(finished, "the agent's turn did not complete after the reconnect");
            for _ in 0..30 {
                if *lock(&resyncs) > 0 { break; }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            println!("acp:resync events: {}", *lock(&resyncs));
            run("acp_disconnect", json!({ "projectId": "bridge6", "sessionId": session })).await.expect("acp_disconnect");
            remote_disconnect_for_test(&bridge, "bridge6").await;
        });
        ssh(format!(
            "OLEAFLY_DATA_DIR={data} {binary} --stop; rm -rf {folder}"
        ));
    }

    async fn remote_disconnect_for_test(bridge: &RemoteBridge, project_id: &str) {
        let binding = binding_for(project_id).unwrap();
        if let Some(conn) = bridge.conns.lock().await.remove(&binding.key()) {
            conn.user_closed.store(true, Ordering::SeqCst);
            conn.closed.store(true, Ordering::SeqCst);
            let _ = conn.stdin.lock().await.shutdown().await;
        }
    }
}
