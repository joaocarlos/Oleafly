//! The persistent side of the remote daemon (feature `remote-spike`).
//!
//! `oleafly-remoted --serve` keeps running on the host after the ssh link that
//! started it is gone, listening on a Unix socket in the data folder.
//! `oleafly-remoted --attach` is what the app runs over ssh: it finds or
//! starts that daemon and relays stdio to it, so a dropped link ends the relay
//! and nothing else. The next `--attach` reaches the same daemon, with its
//! terminals and agents still alive.
//!
//! What a client missed meanwhile:
//! - Channel output (terminals) is kept in a ring per channel, indexed by the
//!   index Tauri gives each message. A client says the last index it saw per
//!   channel in its hello and gets the rest replayed (or a `gap` marker).
//! - Events (agent updates, folder changes) are not kept. The client is told
//!   with a `resync` event and refetches.
//!
//! The wire is the daemon's usual JSON lines. First line from a client:
//!   `{"hello":{"resume":{"<channel>":<last index>}}}`  -> `{"ready":true,...}`
//!   `{"probe":{"exe":{...}}}`                          -> `{"probe":{...}}`
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Bytes of channel output kept per channel for a client that comes back (the terminal's own
/// flow window is 32 chunks of 64 KiB, about 2 MiB).
const RING_BYTES: usize = 4 * 1024 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const SPAWN_WAIT: Duration = Duration::from_secs(10);
const DEFAULT_IDLE_SECS: u64 = 2 * 60 * 60;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

struct Ring {
    entries: VecDeque<(u64, Value, usize)>,
    bytes: usize,
    closed: bool,
}

struct Client {
    id: u64,
    writer: Box<dyn Write + Send>,
    close: Box<dyn Fn() + Send>,
}

/// Where the daemon's output goes: the one client attached now, plus what is kept for the next.
pub struct Hub {
    client: Mutex<Option<Client>>,
    rings: Mutex<HashMap<u64, Ring>>,
    missed: AtomicBool,
    next_client: AtomicU64,
    agent_sessions: Mutex<HashMap<String, bool>>,
    last_detach: Mutex<Instant>,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            client: Mutex::new(None),
            rings: Mutex::new(HashMap::new()),
            missed: AtomicBool::new(false),
            next_client: AtomicU64::new(1),
            agent_sessions: Mutex::new(HashMap::new()),
            last_detach: Mutex::new(Instant::now()),
        })
    }

    /// A one-shot client, the process's own stdout: stdio mode, where nothing outlives the link.
    pub fn with_stdout() -> Arc<Self> {
        let hub = Self::new();
        *lock(&hub.client) = Some(Client {
            id: 0,
            writer: Box::new(std::io::stdout()),
            close: Box::new(|| {}),
        });
        hub
    }

    pub fn send(&self, value: &Value) {
        if let Some(channel) = value.get("channel").and_then(Value::as_u64) {
            self.keep(channel, value);
        } else if value.get("event").and_then(Value::as_str) == Some("acp:session") {
            let payload = &value["payload"];
            if let (Some(id), Some(status)) =
                (payload["sessionId"].as_str(), payload["status"].as_str())
            {
                lock(&self.agent_sessions)
                    .insert(id.to_owned(), !matches!(status, "disconnected" | "closed"));
            }
        }
        let mut client = lock(&self.client);
        let Some(current) = client.as_mut() else {
            if value.get("event").is_some() {
                self.missed.store(true, Ordering::SeqCst);
            }
            return;
        };
        if write_line(&mut current.writer, value).is_err() {
            // A stalled or broken link: stop writing to it and keep what it missed.
            if let Some(gone) = client.take() {
                (gone.close)();
            }
            self.missed.store(true, Ordering::SeqCst);
            *lock(&self.last_detach) = Instant::now();
        }
    }

    fn keep(&self, channel: u64, value: &Value) {
        let index = value["index"].as_u64().unwrap_or(0);
        let size = value["message"].to_string().len();
        let mut rings = lock(&self.rings);
        let ring = rings.entry(channel).or_insert_with(|| Ring {
            entries: VecDeque::new(),
            bytes: 0,
            closed: false,
        });
        if value["message"]["event"].as_str() == Some("exit") {
            ring.closed = true;
        }
        ring.entries.push_back((index, value.clone(), size));
        ring.bytes += size;
        while ring.bytes > RING_BYTES && ring.entries.len() > 1 {
            if let Some((_, _, dropped)) = ring.entries.pop_front() {
                ring.bytes -= dropped;
            }
        }
    }

    /// Terminals still open, and agent sessions not yet disconnected.
    pub fn sessions(&self) -> usize {
        let terminals = lock(&self.rings)
            .values()
            .filter(|ring| !ring.closed)
            .count();
        let agents = lock(&self.agent_sessions)
            .values()
            .filter(|live| **live)
            .count();
        terminals + agents
    }

    fn attach(&self, mut client: Client, resume: &HashMap<u64, i64>, ready: &Value) -> u64 {
        let id = client.id;
        let mut current = lock(&self.client);
        if let Some(previous) = current.take() {
            (previous.close)();
        }
        let mut ok = write_line(&mut client.writer, ready).is_ok();
        if ok {
            let rings = lock(&self.rings);
            let mut channels: Vec<_> = resume.keys().copied().collect();
            channels.sort_unstable();
            'replay: for channel in channels {
                let Some(ring) = rings.get(&channel) else {
                    continue;
                };
                let last = resume[&channel];
                if let Some((oldest, _, _)) = ring.entries.front() {
                    if (*oldest as i64) > last + 1 {
                        let gap =
                            json!({ "gap": channel, "from": last + 1, "to": *oldest as i64 - 1 });
                        if write_line(&mut client.writer, &gap).is_err() {
                            ok = false;
                            break 'replay;
                        }
                    }
                }
                for (index, value, _) in &ring.entries {
                    if (*index as i64) > last && write_line(&mut client.writer, value).is_err() {
                        ok = false;
                        break 'replay;
                    }
                }
            }
        }
        if ok && self.missed.swap(false, Ordering::SeqCst) {
            ok = write_line(
                &mut client.writer,
                &json!({ "event": "resync", "payload": null }),
            )
            .is_ok();
        }
        if ok {
            *current = Some(client);
        } else {
            (client.close)();
        }
        id
    }

    fn detach(&self, id: u64) {
        let mut current = lock(&self.client);
        if current.as_ref().is_some_and(|client| client.id == id) {
            *current = None;
            *lock(&self.last_detach) = Instant::now();
        }
    }

    fn idle_for(&self) -> Option<Duration> {
        lock(&self.client)
            .is_none()
            .then(|| lock(&self.last_detach).elapsed())
    }
}

fn write_line(writer: &mut Box<dyn Write + Send>, value: &Value) -> std::io::Result<()> {
    let mut line = value.to_string();
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()
}

// ---- files -------------------------------------------------------------------------------

fn data_file(name: &str) -> Result<PathBuf, String> {
    Ok(crate::paths::oleafly_root()?.join(name))
}

fn socket_path() -> Result<PathBuf, String> {
    data_file("remoted.sock")
}

/// What identifies this build of the program: a rebuilt or replaced file looks different.
fn exe_identity() -> Value {
    use std::os::unix::fs::MetadataExt;
    let path = std::env::current_exe().ok();
    let meta = path.as_ref().and_then(|path| std::fs::metadata(path).ok());
    json!({
        "path": path.map(|p| p.to_string_lossy().into_owned()),
        "len": meta.as_ref().map(|m| m.len()),
        "mtime": meta.as_ref().map(|m| m.mtime_nsec() + m.mtime() * 1_000_000_000),
    })
}

// ---- daemon ------------------------------------------------------------------------------

/// Serves the socket until idle too long or told to stop. `handle` runs one request line.
pub fn serve(hub: Arc<Hub>, handle: Arc<dyn Fn(&str) + Send + Sync>) -> Result<(), String> {
    let path = socket_path()?;
    if path.exists() {
        match UnixStream::connect(&path) {
            Ok(_) => return Err("A remote daemon is already running here.".into()),
            Err(_) => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    let previous = unsafe { libc::umask(0o177) };
    let listener = UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()));
    unsafe { libc::umask(previous) };
    let listener = listener?;
    let pid_file = data_file("remoted.pid")?;
    let _ = std::fs::write(&pid_file, std::process::id().to_string());
    let started_exe = exe_identity();
    let idle_limit = Duration::from_secs(
        std::env::var("OLEAFLY_REMOTED_IDLE_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_IDLE_SECS),
    );
    let cleanup = {
        let (path, pid_file) = (path.clone(), pid_file.clone());
        move || {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&pid_file);
        }
    };
    {
        let hub = hub.clone();
        let cleanup = cleanup.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            if hub.idle_for().is_some_and(|idle| idle > idle_limit) {
                eprintln!(
                    "oleafly-remoted: idle for {}s with no client, exiting",
                    idle_limit.as_secs()
                );
                cleanup();
                std::process::exit(0);
            }
        });
    }
    eprintln!(
        "oleafly-remoted: serving {} (pid {})",
        path.display(),
        std::process::id()
    );
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let (hub, handle, cleanup, started_exe) = (
            hub.clone(),
            handle.clone(),
            cleanup.clone(),
            started_exe.clone(),
        );
        std::thread::spawn(move || connection(stream, &hub, &handle, &started_exe, &cleanup));
    }
    Ok(())
}

fn connection(
    stream: UnixStream,
    hub: &Arc<Hub>,
    handle: &Arc<dyn Fn(&str) + Send + Sync>,
    started_exe: &Value,
    cleanup: &(impl Fn() + Clone),
) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let Ok(first) = serde_json::from_str::<Value>(&first) else {
        return;
    };
    if let Some(probe) = first.get("probe") {
        let stale = probe["exe"] != *started_exe && !probe["exe"].is_null();
        let mut writer: Box<dyn Write + Send> = Box::new(stream);
        let _ = write_line(
            &mut writer,
            &json!({ "probe": { "pid": std::process::id(), "stale": stale, "sessions": hub.sessions() } }),
        );
        // The relay may ask a stale, idle daemon to go.
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) > 0 && line.contains("remote_exit") {
            let _ = write_line(&mut writer, &json!({ "ok": true }));
            cleanup();
            std::process::exit(0);
        }
        return;
    }
    let Some(hello) = first.get("hello") else {
        return;
    };
    let resume: HashMap<u64, i64> = hello["resume"]
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| Some((k.parse().ok()?, v.as_i64()?)))
                .collect()
        })
        .unwrap_or_default();
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let closer = stream.try_clone();
    let id = hub.next_client.fetch_add(1, Ordering::SeqCst);
    let client = Client {
        id,
        writer: Box::new(stream),
        close: Box::new(move || {
            if let Ok(stream) = &closer {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }),
    };
    let ready = json!({
        "ready": true,
        "pid": std::process::id(),
        "attached": crate::remote_spike::attached_ids(),
        "stale": hello["exe"] != *started_exe && !hello["exe"].is_null(),
        "sessions": hub.sessions(),
    });
    hub.attach(client, &resume, &ready);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if line.contains("\"remote_exit\"") {
            cleanup();
            std::process::exit(0);
        }
        handle(&line);
    }
    hub.detach(id);
}

// ---- relay -------------------------------------------------------------------------------

/// What the app runs over ssh: reach the daemon (starting it if needed) and relay stdio to it.
pub fn attach() -> Result<(), String> {
    let stream = connect_or_start()?;
    let reader_stream = stream.try_clone().map_err(|e| e.to_string())?;
    let mut writer_stream = stream;
    let to_stdout = std::thread::spawn(move || {
        let mut source = reader_stream;
        let mut out = std::io::stdout();
        let mut buffer = [0u8; 16 * 1024];
        while let Ok(read) = source.read(&mut buffer) {
            if read == 0 || out.write_all(&buffer[..read]).is_err() || out.flush().is_err() {
                break;
            }
        }
    });
    let mut stdin = std::io::stdin().lock();
    let mut buffer = [0u8; 16 * 1024];
    while let Ok(read) = stdin.read(&mut buffer) {
        if read == 0 || writer_stream.write_all(&buffer[..read]).is_err() {
            break;
        }
    }
    // The link is gone: let the daemon see the end of this client and keep running.
    let _ = writer_stream.shutdown(std::net::Shutdown::Both);
    let _ = to_stdout.join();
    Ok(())
}

fn probe(path: &PathBuf) -> Option<(UnixStream, Value)> {
    let mut stream = UnixStream::connect(path).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut line = json!({ "probe": { "exe": exe_identity() } }).to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).ok()?;
    let mut reply = String::new();
    BufReader::new(stream.try_clone().ok()?)
        .read_line(&mut reply)
        .ok()?;
    let reply: Value = serde_json::from_str(&reply).ok()?;
    Some((stream, reply["probe"].clone()))
}

fn connect_or_start() -> Result<UnixStream, String> {
    let path = socket_path()?;
    let lock_path = data_file("remoted.lock")?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("{}: {e}", lock_path.display()))?;
    // Two attaches at once must not start two daemons.
    use std::os::fd::AsRawFd;
    unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
    let result = (|| {
        for attempt in 0..2 {
            match probe(&path) {
                Some((mut stream, info)) => {
                    let stale = info["stale"].as_bool().unwrap_or(false);
                    let idle = info["sessions"].as_u64().unwrap_or(0) == 0;
                    if stale && idle && attempt == 0 {
                        // A rebuilt program replaces an idle daemon; one with sessions is left alone.
                        let _ = stream.write_all(b"{\"cmd\":\"remote_exit\"}\n");
                        let mut ack = String::new();
                        let _ = BufReader::new(stream).read_line(&mut ack);
                        wait_until(|| !path.exists() || UnixStream::connect(&path).is_err());
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    return UnixStream::connect(&path).map_err(|e| e.to_string());
                }
                None => {
                    let _ = std::fs::remove_file(&path);
                    spawn_daemon()?;
                    if !wait_until(|| probe(&path).is_some()) {
                        return Err(
                            "The remote daemon did not start. See remoted.log in the data folder."
                                .into(),
                        );
                    }
                    return UnixStream::connect(&path).map_err(|e| e.to_string());
                }
            }
        }
        spawn_daemon()?;
        if !wait_until(|| probe(&path).is_some()) {
            return Err("The remote daemon did not restart.".into());
        }
        UnixStream::connect(&path).map_err(|e| e.to_string())
    })();
    unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN) };
    result
}

fn wait_until(condition: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < SPAWN_WAIT {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Starts `--serve` in its own session, with no link to this process's pipes, so the end of
/// the ssh session cannot reach it.
fn spawn_daemon() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_file("remoted.log")?)
        .map_err(|e| e.to_string())?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("--serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command
        .spawn()
        .map(drop)
        .map_err(|e| format!("could not start the daemon: {e}"))
}

/// Stops the daemon from outside (tests, cleanup): SIGTERM to the pid it recorded.
pub fn stop() -> Result<(), String> {
    let pid_file = data_file("remoted.pid")?;
    let Ok(text) = std::fs::read_to_string(&pid_file) else {
        return Ok(());
    };
    if let Ok(pid) = text.trim().parse::<i32>() {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let _ = std::fs::remove_file(&pid_file);
    let _ = std::fs::remove_file(socket_path()?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client whose lines can be read back.
    #[derive(Clone, Default)]
    struct Wire(Arc<Mutex<Vec<u8>>>);

    impl Write for Wire {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Wire {
        fn lines(&self) -> Vec<Value> {
            String::from_utf8_lossy(&lock(&self.0))
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    struct Failing;

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::TimedOut.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn client(id: u64, writer: impl Write + Send + 'static, closed: Arc<AtomicBool>) -> Client {
        Client {
            id,
            writer: Box::new(writer),
            close: Box::new(move || closed.store(true, Ordering::SeqCst)),
        }
    }

    fn chunk(channel: u64, index: u64, text: &str) -> Value {
        json!({ "channel": channel, "index": index, "message": { "event": "output", "data": text } })
    }

    fn ready() -> Value {
        json!({ "ready": true })
    }

    #[test]
    fn a_client_that_comes_back_gets_what_it_missed_in_order_and_is_told_to_resync() {
        let hub = Hub::new();
        for index in 0..5 {
            hub.send(&chunk(7, index, &format!("tick{index}")));
        }
        hub.send(&json!({ "event": "acp:event", "payload": {} }));
        let wire = Wire::default();
        hub.attach(
            client(1, wire.clone(), Arc::default()),
            &HashMap::from([(7, 1)]),
            &ready(),
        );
        let lines = wire.lines();
        let indexes: Vec<u64> = lines.iter().filter_map(|l| l["index"].as_u64()).collect();
        assert_eq!(
            indexes,
            vec![2, 3, 4],
            "only what the client had not seen, once each"
        );
        assert_eq!(
            lines.first().unwrap(),
            &ready(),
            "the hello is answered before the replay"
        );
        assert_eq!(lines.last().unwrap()["event"], "resync");
        // Nothing is repeated for the next client.
        let again = Wire::default();
        hub.attach(
            client(2, again.clone(), Arc::default()),
            &HashMap::from([(7, 4)]),
            &ready(),
        );
        assert_eq!(
            again.lines().len(),
            1,
            "up to date, and no missed events: just the ready line"
        );
    }

    #[test]
    fn a_client_that_has_seen_nothing_of_a_channel_gets_all_of_it() {
        let hub = Hub::new();
        hub.send(&chunk(3, 0, "a"));
        hub.send(&chunk(3, 1, "b"));
        let wire = Wire::default();
        hub.attach(
            client(1, wire.clone(), Arc::default()),
            &HashMap::from([(3, -1)]),
            &ready(),
        );
        assert_eq!(
            wire.lines()
                .iter()
                .filter(|l| l.get("index").is_some())
                .count(),
            2
        );
        let other = Wire::default();
        hub.attach(
            client(2, other.clone(), Arc::default()),
            &HashMap::new(),
            &ready(),
        );
        assert_eq!(
            other.lines().len(),
            1,
            "a channel the client never heard of is not sent"
        );
    }

    #[test]
    fn output_older_than_the_ring_is_reported_as_a_gap() {
        let hub = Hub::new();
        let big = "x".repeat(1024 * 1024);
        for index in 0..8 {
            hub.send(&chunk(1, index, &big));
        }
        let wire = Wire::default();
        hub.attach(
            client(1, wire.clone(), Arc::default()),
            &HashMap::from([(1, -1)]),
            &ready(),
        );
        let lines = wire.lines();
        let gap = lines
            .iter()
            .find(|l| l.get("gap").is_some())
            .expect("a gap marker");
        assert_eq!(gap["from"], 0);
        assert!(
            gap["to"].as_i64().unwrap() >= 3,
            "several chunks were dropped: {gap}"
        );
        let first_kept = lines.iter().find_map(|l| l["index"].as_u64()).unwrap();
        assert_eq!(
            first_kept as i64,
            gap["to"].as_i64().unwrap() + 1,
            "the replay starts right after the gap"
        );
    }

    #[test]
    fn a_stalled_client_is_dropped_and_what_it_missed_is_kept() {
        let hub = Hub::new();
        let closed = Arc::new(AtomicBool::new(false));
        hub.attach(
            client(1, Failing, closed.clone()),
            &HashMap::new(),
            &ready(),
        );
        // The ready line itself failed to write: the client never became current.
        assert!(closed.load(Ordering::SeqCst));
        let flaky = Wire::default();
        hub.attach(
            client(2, flaky.clone(), Arc::default()),
            &HashMap::new(),
            &ready(),
        );
        // Now the link stalls mid-run.
        *lock(&hub.client) = Some(client(3, Failing, closed.clone()));
        closed.store(false, Ordering::SeqCst);
        hub.send(&chunk(5, 0, "while stalled"));
        assert!(
            closed.load(Ordering::SeqCst),
            "the stalled client is closed"
        );
        assert!(lock(&hub.client).is_none(), "and no longer the current one");
        let back = Wire::default();
        hub.attach(
            client(4, back.clone(), Arc::default()),
            &HashMap::from([(5, -1)]),
            &ready(),
        );
        let lines = back.lines();
        assert!(lines
            .iter()
            .any(|l| l["message"]["data"] == "while stalled"));
        assert_eq!(lines.last().unwrap()["event"], "resync");
    }

    #[test]
    fn a_new_client_replaces_the_one_attached() {
        let hub = Hub::new();
        let old_closed = Arc::new(AtomicBool::new(false));
        hub.attach(
            client(1, Wire::default(), old_closed.clone()),
            &HashMap::new(),
            &ready(),
        );
        let new = Wire::default();
        hub.attach(
            client(2, new.clone(), Arc::default()),
            &HashMap::new(),
            &ready(),
        );
        assert!(old_closed.load(Ordering::SeqCst));
        hub.send(&chunk(1, 0, "now"));
        assert!(new.lines().iter().any(|l| l["message"]["data"] == "now"));
        hub.detach(1);
        assert!(
            lock(&hub.client).is_some(),
            "the old client leaving must not detach the new one"
        );
    }

    #[test]
    fn sessions_count_open_terminals_and_live_agents() {
        let hub = Hub::new();
        hub.send(&chunk(1, 0, "a"));
        hub.send(
            &json!({ "event": "acp:session", "payload": { "sessionId": "s1", "status": "ready" } }),
        );
        assert_eq!(hub.sessions(), 2);
        hub.send(&chunk(1, 1, "x"));
        hub.send(&json!({ "channel": 1, "index": 2, "message": { "event": "exit" } }));
        hub.send(&json!({ "event": "acp:session", "payload": { "sessionId": "s1", "status": "disconnected" } }));
        assert_eq!(
            hub.sessions(),
            0,
            "a closed terminal and a disconnected agent are not sessions"
        );
    }
}
