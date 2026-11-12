//! Setting up a remote workspace from the app (feature `remote-bridge`).
//!
//! The steps, in the order the wizard takes them: confirm the host's key, probe the host, put the
//! Oleafly helper (`oleafly-remoted`) on it, choose a folder there, bind a project to that folder.
//! Every step is safe to repeat, and nothing is written on this computer until the last one.
//!
//! What gets installed, and where, is fixed here and shown to the user before it happens:
//! `~/.oleafly/remoted/<app version>/oleafly-remoted` on the host, with its data in
//! `~/.oleafly/remote-data`. Nothing outside `~/.oleafly` is touched, and deleting that folder
//! removes it all. The helper is copied over the user's own ssh connection and checked by its
//! SHA-256 on the host before it is put in place.
//!
//! The helper must be the same version and protocol as the app (it runs the app's own commands),
//! so an installed one is reused only when `--version` prints exactly what
//! `remote_commands::daemon_version_line` does.
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use oleafly_agent::CancellationToken;
use oleafly_research::runs::remote::host_keys::{self, HostKeyFingerprint};
use oleafly_research::runs::remote::transport::{SshInput, SshLimits};
use oleafly_research::runs::RunError;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, ReadBuf};

use super::helper_release::{self, Manifest, ReleaseHelper};
use super::*;

/// Where the helper lives on the host, relative to the user's home.
const HELPER_ROOT: &str = ".oleafly/remoted";
const DATA_FOLDER: &str = ".oleafly/remote-data";
const HELPER_NAME: &str = "oleafly-remoted";
const PROBE_LIMIT: Duration = Duration::from_secs(45);
const FOLDER_LIMIT: Duration = Duration::from_secs(30);
const MAX_OUTPUT: usize = 256 * 1024;
const MAX_DIRS: usize = 300;
const MAX_FINGERPRINTS: usize = 16;
/// Slowest upload the time limit allows for: a bit over 1 Mbit/s.
const SLOWEST_UPLOAD_BYTES_PER_SEC: u64 = 150_000;
const PROGRESS_EVERY: Duration = Duration::from_millis(200);
/// A Unix socket path must fit in 104 bytes on macOS and 108 on Linux, and the helper keeps one
/// in its data folder.
const MAX_SOCKET_PATH: usize = 100;
const SOCKET_NAME: &str = "remoted.sock";

/// Libraries the Linux helper links to (the app is built on Tauri's webview stack). A server
/// usually lacks them, and the helper cannot start without them.
const LINUX_LIBS: [&str; 2] = ["libwebkit2gtk-4.1.so.0", "libgtk-3.so.0"];

fn require_main<R: Runtime>(webview: &tauri::Webview<R>) -> Result<(), String> {
    if webview.label() == "main" && webview.window().label() == "main" {
        Ok(())
    } else {
        Err("Remote workspaces are set up from the main window.".into())
    }
}

// ---------------------------------------------------------------------------------------------
// The host

/// A host as the wizard names it. A `user@host` in the host field is split here.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostArg {
    host: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    port: Option<u16>,
}

fn valid_user(user: &str) -> bool {
    let mut chars = user.chars();
    user.len() <= 64
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_'))
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl HostArg {
    fn endpoint(&self) -> Result<SshEndpoint, String> {
        let mut host = self.host.trim().to_owned();
        let mut user = self
            .user
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_owned);
        if let Some((name, rest)) = host.rsplit_once('@') {
            if user.is_none() {
                user = Some(name.to_owned());
            }
            host = rest.to_owned();
        }
        let host_ok = !host.is_empty()
            && host.len() <= 253
            && !host.starts_with('-')
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !host_ok {
            return Err(
                "The host name can have letters, digits, dots, dashes and underscores.".into(),
            );
        }
        if let Some(user) = &user {
            if !valid_user(user) {
                return Err("The user name has characters ssh will not accept here.".into());
            }
        }
        if self.port == Some(0) {
            return Err("The port must be between 1 and 65535.".into());
        }
        Ok(SshEndpoint {
            host,
            user,
            port: self.port,
        })
    }
}

fn ssh_context() -> Result<SshContext, String> {
    let root = crate::paths::oleafly_root()?;
    SshContext::new(&root, &|name| path_search(name)).map_err(String::from)
}

/// An absolute path as the host reports it: one line, no control characters.
fn plain_absolute(path: &str) -> bool {
    path.starts_with('/') && !path.chars().any(char::is_control) && path.len() <= 4096
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HashTool {
    Sha256sum,
    Shasum,
}

impl HashTool {
    fn command(self) -> &'static str {
        match self {
            Self::Sha256sum => "sha256sum",
            Self::Shasum => "shasum -a 256",
        }
    }
}

/// What the probe script printed, before the helper plan is added.
#[derive(Debug, Default, PartialEq, Eq)]
struct Facts {
    os: String,
    arch: String,
    home: String,
    libc: Option<String>,
    libs_known: bool,
    missing_libs: Vec<String>,
    latexmk: bool,
    git: bool,
    hash: Option<HashTool>,
    binary: Option<String>,
    sha: Option<String>,
    version: Option<String>,
}

/// The Rust target the helper is built for on a host, from `uname -s` and `uname -m`.
fn triple_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("Linux", "x86_64" | "amd64") => Some("x86_64-unknown-linux-gnu"),
        ("Linux", "aarch64" | "arm64") => Some("aarch64-unknown-linux-gnu"),
        ("Darwin", "arm64" | "aarch64") => Some("aarch64-apple-darwin"),
        ("Darwin", "x86_64") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

fn probe_script() -> String {
    // The version goes into the script as a quoted value; the rest is fixed text. One command per
    // line is fine here: it is sent on standard input to `sh -s`, not through the login shell.
    let libs = LINUX_LIBS
        .iter()
        .map(|lib| {
            format!("case \"$lc\" in *{lib}*) ;; *) echo \"OLEAFLY_PROBE missing {lib}\";; esac")
        })
        .collect::<Vec<_>>()
        .join("\n    ");
    format!(
        r#"V={version}
echo "OLEAFLY_PROBE os $(uname -s 2>/dev/null)"
echo "OLEAFLY_PROBE arch $(uname -m 2>/dev/null)"
echo "OLEAFLY_PROBE home $HOME"
if command -v latexmk >/dev/null 2>&1; then echo "OLEAFLY_PROBE latexmk yes"; fi
if command -v git >/dev/null 2>&1; then echo "OLEAFLY_PROBE git yes"; fi
h=
if command -v sha256sum >/dev/null 2>&1; then h=sha256sum; echo "OLEAFLY_PROBE hash sha256sum"
elif command -v shasum >/dev/null 2>&1; then h="shasum -a 256"; echo "OLEAFLY_PROBE hash shasum"; fi
if command -v ldd >/dev/null 2>&1; then echo "OLEAFLY_PROBE libc $(ldd --version 2>&1 | head -n 1)"; fi
if [ "$(uname -s 2>/dev/null)" = Linux ]; then
  lc=$( (/sbin/ldconfig -p 2>/dev/null || ldconfig -p 2>/dev/null) )
  if [ -n "$lc" ]; then
    echo "OLEAFLY_PROBE libs known"
    {libs}
  fi
fi
bin="$HOME/{root}/$V/{name}"
if [ -f "$bin" ] && [ -x "$bin" ]; then
  echo "OLEAFLY_PROBE binary $bin"
  if [ -n "$h" ]; then echo "OLEAFLY_PROBE sha $($h "$bin" 2>/dev/null | cut -d' ' -f1)"; fi
  echo "OLEAFLY_PROBE version $("$bin" --version 2>&1 | head -n 1)"
fi
"#,
        version = sh_quote(env!("CARGO_PKG_VERSION")),
        root = HELPER_ROOT,
        name = HELPER_NAME,
        libs = libs,
    )
}

fn parse_probe(stdout: &str) -> Result<Facts, String> {
    let mut facts = Facts::default();
    let mut seen_home = false;
    for line in stdout.lines().map(|l| l.trim_end_matches('\r')) {
        let Some(rest) = line.strip_prefix("OLEAFLY_PROBE ") else {
            continue;
        };
        let (key, value) = rest.split_once(' ').unwrap_or((rest, ""));
        let value = value.trim();
        match key {
            // First answer wins: a later line cannot override what the host said about itself.
            "os" if facts.os.is_empty() => facts.os = value.to_owned(),
            "arch" if facts.arch.is_empty() => facts.arch = value.to_owned(),
            "home" if !seen_home => {
                seen_home = true;
                facts.home = value.to_owned();
            }
            "latexmk" => facts.latexmk = value == "yes",
            "git" => facts.git = value == "yes",
            "hash" if facts.hash.is_none() => {
                facts.hash = match value {
                    "sha256sum" => Some(HashTool::Sha256sum),
                    "shasum" => Some(HashTool::Shasum),
                    _ => None,
                }
            }
            "libc" if facts.libc.is_none() && !value.is_empty() => {
                let lower = value.to_ascii_lowercase();
                facts.libc = Some(if lower.contains("musl") {
                    "musl".to_owned()
                } else {
                    let version = value
                        .split_whitespace()
                        .rev()
                        .find(|word| word.chars().next().is_some_and(|c| c.is_ascii_digit()));
                    match version {
                        Some(version) => format!("glibc {version}"),
                        None => "glibc".to_owned(),
                    }
                });
            }
            "libs" => facts.libs_known = value == "known",
            "missing" if LINUX_LIBS.contains(&value) => facts.missing_libs.push(value.to_owned()),
            "binary" if facts.binary.is_none() => facts.binary = Some(value.to_owned()),
            "sha" if facts.sha.is_none() => {
                let sha = value.to_ascii_lowercase();
                if sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
                    facts.sha = Some(sha);
                }
            }
            "version" if facts.version.is_none() => facts.version = Some(value.to_owned()),
            _ => {}
        }
    }
    if facts.os.is_empty() || facts.arch.is_empty() {
        return Err("The host did not say what system it runs.".into());
    }
    if !plain_absolute(&facts.home) {
        return Err("The host's home folder is not an absolute path.".into());
    }
    Ok(facts)
}

// ---------------------------------------------------------------------------------------------
// The helper this app can install

/// A helper binary on this computer, ready to be copied to a host.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalHelper {
    path: PathBuf,
    sha256: String,
    size: u64,
    source: HelperSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum HelperSource {
    /// `<data folder>/remote-daemons/<target>/oleafly-remoted`: what an installer or a user puts there.
    Provided,
    /// Next to the app's own program: a development build, or a host of this computer's own kind.
    NextToApp,
    /// Fetched from this version's release earlier, and still the file the app was built to expect.
    Downloaded,
    /// Not on this computer yet: the app will download it from this version's release when the
    /// user agrees to install.
    Release,
}

/// The target this computer's own program is built for, when the helper next to it could run on a host.
fn local_triple() -> Option<&'static str> {
    triple_for(
        match std::env::consts::OS {
            "macos" => "Darwin",
            "linux" => "Linux",
            _ => return None,
        },
        std::env::consts::ARCH,
    )
}

fn helper_candidates(
    data_root: &Path,
    triple: &str,
    exe: Option<&Path>,
) -> Vec<(PathBuf, HelperSource)> {
    let mut list = vec![(
        data_root
            .join(helper_release::DAEMONS_FOLDER)
            .join(triple)
            .join(HELPER_NAME),
        HelperSource::Provided,
    )];
    if local_triple() == Some(triple) {
        if let Some(folder) = exe.and_then(Path::parent) {
            list.push((folder.join(HELPER_NAME), HelperSource::NextToApp));
        }
    }
    // Last: a file the user or an installer put there on purpose wins over one fetched earlier.
    list.push((
        helper_release::install_dir(data_root, triple, env!("CARGO_PKG_VERSION")).join(HELPER_NAME),
        HelperSource::Downloaded,
    ));
    list
}

/// Whether the first bytes of a file are an executable for `triple`. A wrong file (another
/// system's helper, a text file) is refused before it is copied to a host it cannot run on.
fn header_matches_triple(header: &[u8], triple: &str) -> bool {
    let little = |range: std::ops::Range<usize>| -> Option<u32> {
        let bytes = header.get(range)?;
        Some(
            bytes
                .iter()
                .rev()
                .fold(0u32, |n, b| (n << 8) | u32::from(*b)),
        )
    };
    if triple.ends_with("-linux-gnu") {
        let machine = match triple.split('-').next() {
            Some("x86_64") => 0x3e,
            Some("aarch64") => 0xb7,
            _ => return false,
        };
        return header.starts_with(&[0x7f, b'E', b'L', b'F'])
            && header.get(4) == Some(&2)
            && header.get(5) == Some(&1)
            && little(18..20) == Some(machine);
    }
    if triple.ends_with("-apple-darwin") {
        // A universal binary holds both; a thin one names its CPU.
        if header.starts_with(&[0xca, 0xfe, 0xba, 0xbe]) {
            return true;
        }
        let cpu = match triple.split('-').next() {
            Some("x86_64") => 0x0100_0007,
            Some("aarch64") => 0x0100_000c,
            _ => return false,
        };
        return header.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) && little(4..8) == Some(cpu);
    }
    false
}

fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), size))
}

fn find_helper(candidates: &[(PathBuf, HelperSource)], triple: &str) -> Option<LocalHelper> {
    use std::io::Read;
    for (path, source) in candidates {
        let Ok(metadata) = std::fs::metadata(path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let mut header = [0u8; 32];
        let Ok(read) = std::fs::File::open(path).and_then(|mut f| f.read(&mut header)) else {
            continue;
        };
        if !header_matches_triple(&header[..read], triple) {
            continue;
        }
        let Ok((sha256, size)) = hash_file(path) else {
            continue;
        };
        return Some(LocalHelper {
            path: path.clone(),
            sha256,
            size,
            source: *source,
        });
    }
    None
}

/// The first candidate that fits the host. A downloaded file counts only when it is exactly the
/// one this build pins (same size and digest), so a damaged or replaced file is fetched again.
fn find_acceptable_helper(
    candidates: &[(PathBuf, HelperSource)],
    triple: &str,
    pinned: Option<&ReleaseHelper>,
) -> Option<LocalHelper> {
    candidates.iter().find_map(|candidate| {
        find_helper(std::slice::from_ref(candidate), triple).filter(|helper| {
            helper.source != HelperSource::Downloaded
                || pinned.is_some_and(|pinned| {
                    helper.sha256 == pinned.sha256 && helper.size == pinned.size
                })
        })
    })
}

fn local_helper_for(triple: &str, pinned: Option<&ReleaseHelper>) -> Option<LocalHelper> {
    let root = crate::paths::oleafly_root().ok()?;
    let exe = std::env::current_exe().ok();
    find_acceptable_helper(
        &helper_candidates(&root, triple, exe.as_deref()),
        triple,
        pinned,
    )
}

/// The helper this release carries for a host's target, when this build was given a manifest.
fn release_helper_for(triple: &str) -> Option<ReleaseHelper> {
    helper_release::embedded()?.for_triple(triple).cloned()
}

// ---------------------------------------------------------------------------------------------
// What the wizard is told about a host

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Installed {
    path: String,
    /// What `--version` printed, or the error it ran into.
    detail: Option<String>,
    /// It is this app's version and protocol.
    current: bool,
    sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperPlan {
    /// This app has a helper for the host's system.
    available: bool,
    size: Option<u64>,
    source: Option<HelperSource>,
    /// Why not, in words for the user.
    reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostProbe {
    os: String,
    arch: String,
    home: String,
    triple: Option<String>,
    libc: Option<String>,
    /// Runtime libraries the helper needs and the host lacks (Linux).
    missing_libs: Vec<String>,
    latexmk: bool,
    git: bool,
    installed: Option<Installed>,
    helper: HelperPlan,
    version: String,
    helper_dir: String,
    data_dir: String,
}

fn describe_system(os: &str, arch: &str) -> String {
    format!("{os} {arch}")
}

fn plan_helper(
    triple: Option<&str>,
    libc: Option<&str>,
    os: &str,
    arch: &str,
) -> (HelperPlan, Option<LocalHelper>) {
    let manifest = helper_release::embedded();
    plan_with(
        triple,
        libc,
        os,
        arch,
        |pinned| local_helper_for(triple.unwrap_or_default(), pinned),
        manifest.as_ref(),
    )
}

/// The plan for a host, given how to look for a file on this computer and what this build's
/// release carries. A file on this computer comes first; otherwise the release's helper is offered
/// as a download; otherwise the user is told why there is none.
fn plan_with(
    triple: Option<&str>,
    libc: Option<&str>,
    os: &str,
    arch: &str,
    local: impl FnOnce(Option<&ReleaseHelper>) -> Option<LocalHelper>,
    manifest: Option<&Manifest>,
) -> (HelperPlan, Option<LocalHelper>) {
    let none = |reason: String| {
        (
            HelperPlan {
                available: false,
                size: None,
                source: None,
                reason: Some(reason),
            },
            None,
        )
    };
    let Some(triple) = triple else {
        return none(format!(
            "Oleafly has no helper for {} yet. Linux and macOS hosts are supported.",
            describe_system(os, arch)
        ));
    };
    if libc == Some("musl") {
        return none("This host uses musl (for example Alpine Linux). The helper needs a glibc system such as Ubuntu, Debian, Fedora or Rocky.".into());
    }
    let pinned = manifest.and_then(|manifest| manifest.for_triple(triple));
    if let Some(helper) = local(pinned) {
        return (
            HelperPlan {
                available: true,
                size: Some(helper.size),
                source: Some(helper.source),
                reason: None,
            },
            Some(helper),
        );
    }
    if let Some(pinned) = pinned {
        return (
            HelperPlan {
                available: true,
                size: Some(pinned.size),
                source: Some(HelperSource::Release),
                reason: None,
            },
            None,
        );
    }
    let carried = manifest.map(|manifest| manifest.triples().join(", "));
    let which = match carried {
        Some(list) => format!(
            "This version of Oleafly carries helpers for {list}, not for {} ({triple}).",
            describe_system(os, arch)
        ),
        None => format!(
            "This copy of Oleafly has no helper for {} ({triple}).",
            describe_system(os, arch)
        ),
    };
    none(format!(
        "{which} Put one at remote-daemons/{triple}/{HELPER_NAME} in Oleafly's data folder, or build it on the host."
    ))
}

fn host_paths(home: &str) -> (String, String) {
    (
        format!(
            "{home}/{HELPER_ROOT}/{}/{HELPER_NAME}",
            env!("CARGO_PKG_VERSION")
        ),
        format!("{home}/{DATA_FOLDER}"),
    )
}

/// Whether the helper's socket would have a path the system accepts under this home folder.
fn socket_path_fits(home: &str) -> bool {
    format!("{home}/{DATA_FOLDER}/{SOCKET_NAME}").len() <= MAX_SOCKET_PATH
}

fn build_probe(facts: Facts) -> (HostProbe, Option<LocalHelper>, Option<HashTool>) {
    let triple = triple_for(&facts.os, &facts.arch);
    let (mut helper, mut local) =
        plan_helper(triple, facts.libc.as_deref(), &facts.os, &facts.arch);
    if !socket_path_fits(&facts.home) {
        helper = HelperPlan {
            available: false,
            size: None,
            source: None,
            reason: Some(format!(
                "The home folder on this host ({}) has a path too long for the helper's socket. Use an account with a shorter home path.",
                facts.home
            )),
        };
        local = None;
    }
    let expected = crate::remote_commands::daemon_version_line();
    let installed = facts.binary.as_ref().map(|path| Installed {
        path: path.clone(),
        current: facts.version.as_deref() == Some(expected.as_str()),
        detail: facts.version.clone().filter(|v| !v.is_empty()),
        sha256: facts.sha.clone(),
    });
    let (helper_path, data_dir) = host_paths(&facts.home);
    let helper_dir = helper_path
        .rsplit_once('/')
        .map_or(helper_path.clone(), |(dir, _)| dir.to_owned());
    let probe = HostProbe {
        os: facts.os,
        arch: facts.arch,
        home: facts.home,
        triple: triple.map(str::to_owned),
        libc: facts.libc,
        missing_libs: facts.missing_libs,
        latexmk: facts.latexmk,
        git: facts.git,
        installed,
        helper,
        version: expected,
        helper_dir,
        data_dir,
    };
    (probe, local, facts.hash)
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FailReason {
    /// No `ssh` program on this computer.
    Ssh,
    /// The host refused the keys.
    Auth,
    Network,
    Cancelled,
    Other,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ProbeOutcome {
    Ready {
        probe: Box<HostProbe>,
    },
    /// The host's key is new, or has changed: the user decides.
    HostKey {
        host: String,
        keys: Vec<HostKeyFingerprint>,
        changed: bool,
        message: Option<String>,
    },
    Failed {
        reason: FailReason,
        message: String,
    },
}

fn failed(error: &RunError) -> ProbeOutcome {
    let reason = match error {
        RunError::Unavailable(_) => FailReason::Ssh,
        RunError::Auth(_) => FailReason::Auth,
        RunError::Network(_) => FailReason::Network,
        RunError::Cancelled => FailReason::Cancelled,
        _ => FailReason::Other,
    };
    ProbeOutcome::Failed {
        reason,
        message: error.to_string(),
    }
}

fn changed_key_message(host: &str) -> String {
    format!(
        "The host key of {host} has changed. This can mean someone is intercepting the connection, or that the host was reinstalled. Check with its administrator. If you know the change is expected, remove the old key from ~/.ssh/known_hosts (ssh-keygen -R {host}) and try again."
    )
}

async fn probe_host(
    ctx: &SshContext,
    endpoint: &SshEndpoint,
    cancel: &CancellationToken,
) -> Result<(HostProbe, Option<LocalHelper>, Option<HashTool>), ProbeOutcome> {
    let output = transport::run_ok(
        ctx,
        endpoint,
        &HostKeyMode::Strict,
        "sh -s",
        SshInput::Script(probe_script()),
        SshLimits::new(PROBE_LIMIT, MAX_OUTPUT),
        cancel,
    )
    .await;
    match output {
        Ok(output) => parse_probe(&output.text())
            .map(build_probe)
            .map_err(|message| ProbeOutcome::Failed {
                reason: FailReason::Other,
                message,
            }),
        Err(RunError::HostKeyUnknown(_)) => {
            Err(match host_keys::probe(ctx, endpoint, cancel).await {
                Ok(lines) => ProbeOutcome::HostKey {
                    host: endpoint.host.clone(),
                    keys: lines
                        .iter()
                        .filter_map(|line| host_keys::fingerprint(line))
                        .collect(),
                    changed: false,
                    message: None,
                },
                Err(error) => failed(&error),
            })
        }
        Err(RunError::HostKeyChanged(_)) => Err(ProbeOutcome::HostKey {
            host: endpoint.host.clone(),
            keys: Vec::new(),
            changed: true,
            message: Some(changed_key_message(&endpoint.host)),
        }),
        Err(error) => Err(failed(&error)),
    }
}

// ---------------------------------------------------------------------------------------------
// Putting the helper on the host

/// The copy script, run as `sh -c '<script>'` with the helper on standard input. One line, with
/// no `!`, so it survives a login shell that is not POSIX. The helper reaches its final name only
/// after its checksum matches, and what the host answers to `--version` is reported, not judged.
fn install_script(dir: &str, sha256: &str, hash: HashTool) -> String {
    let dir = sh_quote(dir);
    [
        "umask 077".to_owned(),
        format!("d={dir}"),
        "mkdir -p \"$d\" || exit 2".to_owned(),
        format!("t=\"$d/.{HELPER_NAME}.$$\""),
        "trap 'rm -f \"$t\"' EXIT HUP INT TERM".to_owned(),
        "cat > \"$t\" || exit 2".to_owned(),
        "chmod 755 \"$t\" || exit 2".to_owned(),
        format!("s=$({} \"$t\" | cut -d' ' -f1)", hash.command()),
        format!("[ \"$s\" = {} ] || {{ echo 'The copy on the host differs from the original.' >&2; exit 3; }}", sh_quote(sha256)),
        format!("mv -f \"$t\" \"$d/{HELPER_NAME}\" || exit 2"),
        "trap - EXIT HUP INT TERM".to_owned(),
        "echo \"OLEAFLY_INSTALL sha $s\"".to_owned(),
        format!("echo \"OLEAFLY_INSTALL version $(\"$d/{HELPER_NAME}\" --version 2>&1 | head -n 1)\""),
    ]
    .join("; ")
}

/// Reports the bytes it reads, at most a few times a second. It holds a closure rather than the
/// app handle so that it can be boxed as an unpinned reader whatever the runtime is.
struct ProgressReader {
    file: tokio::fs::File,
    sent: u64,
    total: u64,
    last: Instant,
    report: Box<dyn Fn(u64, u64) + Send>,
}

impl AsyncRead for ProgressReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let this = self.get_mut();
        let poll = Pin::new(&mut this.file).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            this.sent += (buf.filled().len() - before) as u64;
            if this.last.elapsed() >= PROGRESS_EVERY || this.sent >= this.total {
                this.last = Instant::now();
                (this.report)(this.sent, this.total);
            }
        }
        poll
    }
}

fn progress<R: Runtime>(app: &AppHandle<R>, host: &str, phase: &str, sent: u64, total: u64) {
    let _ = app.emit(
        "remote:setup",
        json!({ "host": host, "phase": phase, "sent": sent, "total": total }),
    );
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallReport {
    binary: String,
    data_dir: String,
    version: String,
    /// The right helper was already there, so nothing was copied.
    already_installed: bool,
    bytes: u64,
    missing_libs: Vec<String>,
}

/// What went wrong with a helper that is on the host but cannot start, in words for the user.
fn cannot_start(detail: &str, missing_libs: &[String]) -> String {
    let lower = detail.to_ascii_lowercase();
    if lower.contains("error while loading shared libraries")
        || lower.contains("cannot open shared object")
        || !missing_libs.is_empty()
    {
        let libs = if missing_libs.is_empty() {
            detail.to_owned()
        } else {
            missing_libs.join(", ")
        };
        return format!(
            "The helper is on the host but cannot start: it needs libraries the host lacks ({libs}). On Ubuntu or Debian, install them with: sudo apt install libwebkit2gtk-4.1-0 libgtk-3-0. On Fedora or Rocky: sudo dnf install webkit2gtk4.1 gtk3. Then try again; nothing needs to be copied again."
        );
    }
    if lower.contains("glibc") || lower.contains("version `glibc") {
        return format!(
            "The helper needs a newer C library than this host has ({detail}). Use a newer system, or build the helper on the host."
        );
    }
    if lower.contains("permission denied") || lower.contains("noexec") {
        return "The host does not let programs run from your home folder (it may be mounted noexec). Ask its administrator, or build the helper on the host.".into();
    }
    format!("The helper is on the host but cannot start: {detail}")
}

fn install_failed(error: RunError) -> String {
    match error {
        RunError::Remote(message) if message.contains("exit code 3") => {
            "The copy on the host did not match the original, so it was discarded. Try again; if it keeps happening, the connection is altering the data.".into()
        }
        RunError::Remote(message) if message.contains("exit code 2") => {
            "Could not write to ~/.oleafly on the host. Check that its home folder has free space and is writable.".into()
        }
        other => other.to_string(),
    }
}

/// Downloads the helper this release carries for the host's target, unless the pinned file is
/// already in Oleafly's data folder. The installer refuses a file whose size or digest differs from
/// the values this app was built with.
async fn fetch_release_helper<R: Runtime>(
    app: &AppHandle<R>,
    host: &str,
    release: &ReleaseHelper,
    cancel: &CancellationToken,
) -> Result<LocalHelper, String> {
    let root = crate::paths::oleafly_root()?;
    progress(app, host, "download", 0, release.size);
    let (reporter, name) = (app.clone(), host.to_owned());
    let path = helper_release::obtain(
        &root,
        release,
        env!("CARGO_PKG_VERSION"),
        HELPER_NAME,
        cancel,
        move |received, total| progress(&reporter, &name, "download", received, total),
    )
    .await
    .map_err(|error| {
        if cancel.is_cancelled() {
            "Cancelled.".to_owned()
        } else {
            helper_release::describe_failure(&error, &release.triple, HELPER_NAME)
        }
    })?;
    Ok(LocalHelper {
        path,
        sha256: release.sha256.clone(),
        size: release.size,
        source: HelperSource::Downloaded,
    })
}

async fn ensure_helper<R: Runtime>(
    app: &AppHandle<R>,
    ctx: &SshContext,
    endpoint: &SshEndpoint,
    cancel: &CancellationToken,
) -> Result<InstallReport, String> {
    progress(app, &endpoint.host, "check", 0, 0);
    let (probe, local, hash) = match probe_host(ctx, endpoint, cancel).await {
        Ok(found) => found,
        Err(ProbeOutcome::Failed { message, .. }) => return Err(message),
        Err(ProbeOutcome::HostKey {
            changed, message, ..
        }) => {
            return Err(if changed {
                message.unwrap_or_else(|| changed_key_message(&endpoint.host))
            } else {
                "Confirm the host's key first.".into()
            })
        }
        Err(ProbeOutcome::Ready { .. }) => unreachable!("an error is never Ready"),
    };
    let (binary, data_dir) = host_paths(&probe.home);
    let report = |already_installed: bool, bytes: u64| InstallReport {
        binary: binary.clone(),
        data_dir: data_dir.clone(),
        version: probe.version.clone(),
        already_installed,
        bytes,
        missing_libs: probe.missing_libs.clone(),
    };
    // A helper this app has to download is known by its pinned digest before it is fetched, so an
    // identical one on the host needs no download at all.
    let to_fetch = match (&local, probe.helper.source, probe.triple.as_deref()) {
        (None, Some(HelperSource::Release), Some(triple)) => release_helper_for(triple),
        _ => None,
    };
    let expected_sha = local
        .as_ref()
        .map(|local| local.sha256.as_str())
        .or(to_fetch.as_ref().map(|release| release.sha256.as_str()));
    // Same bytes already there (or nothing to compare with, and the right version): reuse it.
    let same = match (&probe.installed, expected_sha) {
        (Some(installed), Some(sha)) => installed.sha256.as_deref() == Some(sha),
        (Some(installed), None) => installed.current,
        _ => false,
    };
    if same {
        let installed = probe.installed.as_ref().expect("same implies installed");
        if installed.current {
            return Ok(report(true, 0));
        }
        return Err(cannot_start(
            installed.detail.as_deref().unwrap_or(""),
            &probe.missing_libs,
        ));
    }
    if local.is_none() && to_fetch.is_none() {
        return Err(probe
            .helper
            .reason
            .clone()
            .unwrap_or_else(|| "There is no helper to install on this host.".into()));
    }
    let Some(hash) = hash else {
        return Err("The host has neither sha256sum nor shasum, so the copy cannot be checked. Install coreutils there and try again.".into());
    };
    let local = match (local, to_fetch) {
        (Some(local), _) => local,
        (None, Some(release)) => {
            fetch_release_helper(app, &endpoint.host, &release, cancel).await?
        }
        (None, None) => unreachable!("handled above"),
    };
    let file = tokio::fs::File::open(&local.path)
        .await
        .map_err(|error| format!("Could not read the helper on this computer: {error}"))?;
    let dir = binary
        .rsplit_once('/')
        .map_or(binary.as_str(), |(dir, _)| dir)
        .to_owned();
    let script = install_script(&dir, &local.sha256, hash);
    let remote = format!("sh -c {}", sh_quote(&script));
    let (reporter, host) = (app.clone(), endpoint.host.clone());
    let reader = ProgressReader {
        file,
        sent: 0,
        total: local.size,
        last: Instant::now(),
        report: Box::new(move |sent, total| progress(&reporter, &host, "upload", sent, total)),
    };
    progress(app, &endpoint.host, "upload", 0, local.size);
    let limit = Duration::from_secs(120 + local.size / SLOWEST_UPLOAD_BYTES_PER_SEC);
    let output = transport::run_ok(
        ctx,
        endpoint,
        &HostKeyMode::Strict,
        &remote,
        SshInput::Reader(Box::new(reader)),
        SshLimits::new(limit, MAX_OUTPUT),
        cancel,
    )
    .await
    .map_err(install_failed)?;
    progress(app, &endpoint.host, "verify", local.size, local.size);
    let text = output.text();
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("OLEAFLY_INSTALL {name} ")))
            .map(|value| value.trim().to_owned())
    };
    if field("sha").as_deref() != Some(local.sha256.as_str()) {
        return Err("The host did not confirm the copy.".into());
    }
    let version = field("version").unwrap_or_default();
    if version != probe.version {
        return Err(cannot_start(&version, &probe.missing_libs));
    }
    progress(app, &endpoint.host, "done", local.size, local.size);
    Ok(report(false, local.size))
}

// ---------------------------------------------------------------------------------------------
// Folders on the host

/// `~` and `~/x` mean the host's home. Anything else must be absolute.
const RESOLVE_PATH: &str = r#"case "$P" in "~") P="$HOME";; "~/"*) P="$HOME/${P#??}";; esac"#;

fn list_script(path: &str) -> String {
    format!(
        r#"P={path}
{RESOLVE_PATH}
[ -d "$P" ] || {{ echo "OLEAFLY_DIR missing"; exit 0; }}
cd "$P" 2>/dev/null || {{ echo "OLEAFLY_DIR denied"; exit 0; }}
echo "OLEAFLY_DIR path $(pwd -P)"
[ -w . ] && echo "OLEAFLY_DIR writable"
[ -d .git ] && echo "OLEAFLY_DIR git"
ls ./*.tex >/dev/null 2>&1 && echo "OLEAFLY_DIR tex"
echo "OLEAFLY_DIR entries $(ls -A 2>/dev/null | wc -l | tr -d ' ')"
n=0
for d in */; do
  [ -d "$d" ] || continue
  n=$((n+1))
  if [ "$n" -gt {MAX_DIRS} ]; then echo "OLEAFLY_DIR more"; break; fi
  echo "OLEAFLY_DIR dir ${{d%/}}"
done
"#,
        path = sh_quote(path),
    )
}

fn make_dir_script(path: &str) -> String {
    format!(
        r#"P={path}
{RESOLVE_PATH}
mkdir -p "$P" 2>/dev/null || {{ echo "OLEAFLY_DIR denied"; exit 0; }}
cd "$P" 2>/dev/null || {{ echo "OLEAFLY_DIR denied"; exit 0; }}
echo "OLEAFLY_DIR path $(pwd -P)"
"#,
        path = sh_quote(path),
    )
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DirState {
    Ok,
    Missing,
    Denied,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DirListing {
    state: DirState,
    /// The folder as the host resolves it (symbolic links followed).
    path: String,
    parent: Option<String>,
    dirs: Vec<String>,
    truncated: bool,
    entries: u64,
    writable: bool,
    has_git: bool,
    has_tex: bool,
}

fn parse_listing(stdout: &str, asked: &str) -> DirListing {
    let mut listing = DirListing {
        state: DirState::Ok,
        path: String::new(),
        parent: None,
        dirs: Vec::new(),
        truncated: false,
        entries: 0,
        writable: false,
        has_git: false,
        has_tex: false,
    };
    for line in stdout.lines().map(|l| l.trim_end_matches('\r')) {
        let Some(rest) = line.strip_prefix("OLEAFLY_DIR ") else {
            continue;
        };
        let (key, value) = rest.split_once(' ').unwrap_or((rest, ""));
        match key {
            "missing" => listing.state = DirState::Missing,
            "denied" => listing.state = DirState::Denied,
            // The first `path` is the host's own; a folder named like a protocol line cannot replace it.
            "path" if listing.path.is_empty() && plain_absolute(value) => {
                listing.path = value.to_owned();
            }
            "writable" => listing.writable = true,
            "git" => listing.has_git = true,
            "tex" => listing.has_tex = true,
            "entries" => listing.entries = value.parse().unwrap_or(0),
            "more" => listing.truncated = true,
            "dir"
                if listing.dirs.len() < MAX_DIRS
                    && !value.is_empty()
                    && !value.contains('/')
                    && !value.chars().any(char::is_control) =>
            {
                listing.dirs.push(value.to_owned());
            }
            _ => {}
        }
    }
    if listing.state == DirState::Ok && listing.path.is_empty() {
        listing.state = DirState::Missing;
    }
    if listing.state != DirState::Ok {
        listing.path = asked.to_owned();
        listing.dirs.clear();
    }
    listing.parent = parent_of(&listing.path);
    listing.dirs.sort_by_key(|name| name.to_lowercase());
    listing
}

fn parent_of(path: &str) -> Option<String> {
    if path == "/" || !path.starts_with('/') {
        return None;
    }
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_owned()),
        Some(at) => Some(trimmed[..at].to_owned()),
        None => None,
    }
}

/// A folder that should not become a project: the whole disk, the home folder itself, or a
/// top-level system folder. The local "open folder" refuses the same kind of choice.
fn too_broad(path: &str, home: &str) -> bool {
    let normal = path.trim_end_matches('/');
    normal.is_empty() || normal == home.trim_end_matches('/') || normal.matches('/').count() < 2
}

// ---------------------------------------------------------------------------------------------
// Commands

/// The one setup that can be cancelled at a time (the upload is the long part).
static ACTIVE: Mutex<Option<CancellationToken>> = Mutex::new(None);

fn begin() -> CancellationToken {
    let token = CancellationToken::new();
    if let Some(previous) = lock(&ACTIVE).replace(token.clone()) {
        previous.cancel();
    }
    token
}

/// Looks at a host: its key, its system, what Oleafly would install and what is already there.
#[tauri::command]
pub async fn remote_probe_host<R: Runtime>(
    webview: tauri::Webview<R>,
    host: HostArg,
) -> Result<ProbeOutcome, String> {
    require_main(&webview)?;
    let endpoint = host.endpoint()?;
    let ctx = ssh_context()?;
    let cancel = CancellationToken::new();
    Ok(match probe_host(&ctx, &endpoint, &cancel).await {
        Ok((probe, _, _)) => ProbeOutcome::Ready {
            probe: Box::new(probe),
        },
        Err(outcome) => outcome,
    })
}

/// Remembers the host's key after the user has compared the fingerprints shown to them.
#[tauri::command]
pub async fn remote_trust_host<R: Runtime>(
    webview: tauri::Webview<R>,
    host: HostArg,
    fingerprints: Vec<String>,
) -> Result<(), String> {
    require_main(&webview)?;
    if fingerprints.is_empty() || fingerprints.len() > MAX_FINGERPRINTS {
        return Err("Confirm the host's key fingerprints first.".into());
    }
    let endpoint = host.endpoint()?;
    let ctx = ssh_context()?;
    let cancel = CancellationToken::new();
    tokio::time::timeout(
        PROBE_LIMIT,
        host_keys::trust(&ctx, &endpoint, &fingerprints, &cancel),
    )
    .await
    .map_err(|_| "Checking the host key took too long.".to_string())?
    .map_err(String::from)
}

/// Installs the helper on the host, or confirms it is there. Emits `remote:setup` progress.
#[tauri::command]
pub async fn remote_install_helper<R: Runtime>(
    webview: tauri::Webview<R>,
    app: AppHandle<R>,
    host: HostArg,
) -> Result<InstallReport, String> {
    require_main(&webview)?;
    let endpoint = host.endpoint()?;
    let ctx = ssh_context()?;
    let cancel = begin();
    let result = ensure_helper(&app, &ctx, &endpoint, &cancel).await;
    lock(&ACTIVE).take();
    if cancel.is_cancelled() {
        return Err("Cancelled.".into());
    }
    result
}

/// Stops the install in progress, if any.
#[tauri::command]
pub fn remote_cancel_setup<R: Runtime>(webview: tauri::Webview<R>) -> Result<(), String> {
    require_main(&webview)?;
    if let Some(token) = lock(&ACTIVE).take() {
        token.cancel();
    }
    Ok(())
}

async fn run_script(host: &HostArg, script: String) -> Result<String, String> {
    let endpoint = host.endpoint()?;
    let ctx = ssh_context()?;
    let cancel = CancellationToken::new();
    transport::run_ok(
        &ctx,
        &endpoint,
        &HostKeyMode::Strict,
        "sh -s",
        SshInput::Script(script),
        SshLimits::new(FOLDER_LIMIT, MAX_OUTPUT),
        &cancel,
    )
    .await
    .map(|output| output.text())
    .map_err(String::from)
}

fn valid_host_path(path: &str) -> Result<(), String> {
    let ok = !path.is_empty()
        && path.len() <= 4096
        && !path.chars().any(char::is_control)
        && (path.starts_with('/') || path == "~" || path.starts_with("~/"));
    if ok {
        Ok(())
    } else {
        Err("The folder must be an absolute path on the host, or start with ~/.".into())
    }
}

/// Lists the folders inside a folder on the host, for the folder picker.
#[tauri::command]
pub async fn remote_list_dir<R: Runtime>(
    webview: tauri::Webview<R>,
    host: HostArg,
    path: String,
) -> Result<DirListing, String> {
    require_main(&webview)?;
    valid_host_path(&path)?;
    let text = run_script(&host, list_script(&path)).await?;
    Ok(parse_listing(&text, &path))
}

/// Creates a folder (and its parents) on the host and returns where it is.
#[tauri::command]
pub async fn remote_make_dir<R: Runtime>(
    webview: tauri::Webview<R>,
    host: HostArg,
    path: String,
) -> Result<String, String> {
    require_main(&webview)?;
    valid_host_path(&path)?;
    let text = run_script(&host, make_dir_script(&path)).await?;
    let listing = parse_listing(&text, &path);
    match listing.state {
        DirState::Ok => Ok(listing.path),
        _ => Err(format!("Could not create {path} on the host.")),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRequest {
    host: HostArg,
    path: String,
    name: String,
    /// Create the folder when it does not exist yet.
    #[serde(default)]
    create_folder: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedProject {
    project_id: String,
}

fn project_name(requested: &str, folder: &str) -> Result<String, String> {
    let name = requested.trim();
    let name = if name.is_empty() {
        folder
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or("Remote project")
    } else {
        name
    };
    if name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err("The project name is too long or has control characters.".into());
    }
    Ok(name.to_owned())
}

/// The last step: a project in the library whose files, terminal and agents are on the host.
/// The local project holds only its name; the binding is written last, then checked with a real
/// call to the host, and both are taken back if that call fails.
#[tauri::command]
pub async fn remote_create_project<R: Runtime>(
    webview: tauri::Webview<R>,
    app: AppHandle<R>,
    bridge: State<'_, RemoteBridge>,
    request: CreateRequest,
) -> Result<CreatedProject, String> {
    require_main(&webview)?;
    create_project_on_host(&app, &bridge, request).await
}

async fn create_project_on_host<R: Runtime>(
    app: &AppHandle<R>,
    bridge: &RemoteBridge,
    request: CreateRequest,
) -> Result<CreatedProject, String> {
    valid_host_path(&request.path)?;
    let endpoint = request.host.endpoint()?;
    let ctx = ssh_context()?;
    let cancel = CancellationToken::new();
    let probe = match probe_host(&ctx, &endpoint, &cancel).await {
        Ok((probe, _, _)) => probe,
        Err(ProbeOutcome::Failed { message, .. }) => return Err(message),
        Err(_) => return Err("Confirm the host's key first.".into()),
    };
    let current = probe.installed.as_ref().is_some_and(|i| i.current);
    if !current {
        return Err("The Oleafly helper is not installed on this host yet.".into());
    }
    let script = if request.create_folder {
        make_dir_script(&request.path)
    } else {
        list_script(&request.path)
    };
    let listing = parse_listing(&run_script(&request.host, script).await?, &request.path);
    match listing.state {
        DirState::Ok => {}
        DirState::Missing => return Err(format!("{} does not exist on the host.", request.path)),
        DirState::Denied => return Err(format!("The host did not let you open {}.", request.path)),
    }
    if too_broad(&listing.path, &probe.home) {
        return Err(format!(
            "Choose a folder inside {}, or create one there. A whole home folder or disk is too broad for a project.",
            probe.home
        ));
    }
    let name = project_name(&request.name, &listing.path)?;
    let (binary, data_dir) = host_paths(&probe.home);
    let binding = Binding {
        host: endpoint.host.clone(),
        user: endpoint.user.clone(),
        port: endpoint.port,
        path: listing.path.clone(),
        main_doc: None,
        engine: None,
        persistent: true,
        binary,
        data_dir,
    };
    binding.valid()?;

    let project_id =
        tauri::async_runtime::spawn_blocking(move || crate::project::create_remote_stub(name))
            .await
            .map_err(|error| error.to_string())??;
    let saved = {
        let (id, entry) = (project_id.clone(), binding.clone());
        update_config(move |config| {
            config.projects.insert(id, entry);
            Ok(())
        })
    };
    if let Err(error) = saved {
        let _ = crate::project::discard_remote_stub(&project_id);
        return Err(error);
    }
    // A real call through the bridge: it connects, starts the helper and attaches the folder.
    match call(
        app,
        bridge,
        &project_id,
        "get_project",
        json!({ "projectId": project_id }),
        None,
    )
    .await
    {
        Ok(_) => Ok(CreatedProject { project_id }),
        Err(error) => {
            let message = match &error {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            let id = project_id.clone();
            let _ = update_config(move |config| {
                config.projects.remove(&id);
                Ok(())
            });
            if let Some(conn) = bridge.conns.lock().await.remove(&binding.key()) {
                conn.user_closed.store(true, Ordering::SeqCst);
            }
            let _ = crate::project::discard_remote_stub(&project_id);
            Err(format!("The host did not accept the project: {message}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::Listener;

    const GLIBC_PROBE: &str = "\
OLEAFLY_PROBE os Linux
OLEAFLY_PROBE arch x86_64
OLEAFLY_PROBE home /home/ana
OLEAFLY_PROBE latexmk yes
OLEAFLY_PROBE git yes
OLEAFLY_PROBE hash sha256sum
OLEAFLY_PROBE libc ldd (Ubuntu GLIBC 2.35-0ubuntu3.8) 2.35
OLEAFLY_PROBE libs known
OLEAFLY_PROBE missing libwebkit2gtk-4.1.so.0
";

    #[test]
    fn reads_what_the_host_says_about_itself() {
        let facts = parse_probe(GLIBC_PROBE).unwrap();
        assert_eq!(facts.os, "Linux");
        assert_eq!(facts.arch, "x86_64");
        assert_eq!(facts.home, "/home/ana");
        assert_eq!(facts.libc.as_deref(), Some("glibc 2.35"));
        assert_eq!(facts.hash, Some(HashTool::Sha256sum));
        assert!(facts.latexmk && facts.git && facts.libs_known);
        assert_eq!(facts.missing_libs, ["libwebkit2gtk-4.1.so.0"]);
        assert_eq!(facts.binary, None);
        assert_eq!(
            triple_for(&facts.os, &facts.arch),
            Some("x86_64-unknown-linux-gnu")
        );
    }

    #[test]
    fn noise_on_standard_output_and_late_lines_do_not_change_the_answer() {
        let text = format!(
            "Welcome to the cluster!\n{GLIBC_PROBE}OLEAFLY_PROBE os Plan9\nOLEAFLY_PROBE home /etc\nOLEAFLY_PROBE missing /etc/passwd\n"
        );
        let facts = parse_probe(&text).unwrap();
        assert_eq!(facts.os, "Linux");
        assert_eq!(facts.home, "/home/ana");
        assert_eq!(facts.missing_libs, ["libwebkit2gtk-4.1.so.0"]);
    }

    #[test]
    fn a_host_without_a_usable_home_or_system_is_refused() {
        assert!(parse_probe(
            "OLEAFLY_PROBE os Linux\nOLEAFLY_PROBE arch x86_64\nOLEAFLY_PROBE home relative\n"
        )
        .is_err());
        assert!(parse_probe(
            "OLEAFLY_PROBE os Linux\nOLEAFLY_PROBE arch x86_64\nOLEAFLY_PROBE home \n"
        )
        .is_err());
        assert!(parse_probe("nothing useful").is_err());
    }

    #[test]
    fn musl_and_bare_glibc_are_told_apart() {
        let musl = "OLEAFLY_PROBE os Linux\nOLEAFLY_PROBE arch aarch64\nOLEAFLY_PROBE home /root\nOLEAFLY_PROBE libc musl libc (aarch64)\n";
        assert_eq!(parse_probe(musl).unwrap().libc.as_deref(), Some("musl"));
        let (helper, local) = plan_helper(
            Some("aarch64-unknown-linux-gnu"),
            Some("musl"),
            "Linux",
            "aarch64",
        );
        assert!(!helper.available && local.is_none());
        assert!(helper.reason.unwrap().contains("musl"));
    }

    #[test]
    fn systems_without_a_helper_say_so() {
        assert_eq!(triple_for("FreeBSD", "amd64"), None);
        assert_eq!(triple_for("Darwin", "arm64"), Some("aarch64-apple-darwin"));
        assert_eq!(
            triple_for("Linux", "arm64"),
            Some("aarch64-unknown-linux-gnu")
        );
        let (helper, _) = plan_helper(None, None, "FreeBSD", "amd64");
        assert!(helper.reason.unwrap().contains("FreeBSD amd64"));
    }

    /// A release manifest for tests: a helper for x86_64 Linux only.
    fn release_manifest() -> Manifest {
        let line = crate::remote_commands::daemon_version_line();
        let version = env!("CARGO_PKG_VERSION");
        let json = format!(
            r#"{{"version":"{version}","line":"{line}","targets":{{"x86_64-unknown-linux-gnu":{{"asset":"oleafly-remoted-{version}-x86_64-unknown-linux-gnu","size":4096,"sha256":"{}"}}}}}}"#,
            "c".repeat(64)
        );
        Manifest::parse(&json, version, &line).unwrap()
    }

    fn some_helper(source: HelperSource) -> LocalHelper {
        LocalHelper {
            path: PathBuf::from("/data/remote-daemons/x/oleafly-remoted"),
            sha256: "d".repeat(64),
            size: 10,
            source,
        }
    }

    #[test]
    fn a_release_helper_is_offered_as_a_download_when_no_file_is_on_this_computer() {
        let manifest = release_manifest();
        let (plan, local) = plan_with(
            Some("x86_64-unknown-linux-gnu"),
            Some("glibc"),
            "Linux",
            "x86_64",
            |pinned| {
                assert!(pinned.is_some(), "the lookup is told what the release pins");
                None
            },
            Some(&manifest),
        );
        assert!(local.is_none());
        assert!(plan.available && plan.reason.is_none());
        assert_eq!(plan.size, Some(4096));
        assert_eq!(plan.source, Some(HelperSource::Release));
        // A file the user provided wins, and nothing is downloaded.
        let (plan, local) = plan_with(
            Some("x86_64-unknown-linux-gnu"),
            None,
            "Linux",
            "x86_64",
            |_| Some(some_helper(HelperSource::Provided)),
            Some(&manifest),
        );
        assert_eq!(plan.source, Some(HelperSource::Provided));
        assert_eq!(local.unwrap().size, 10);
    }

    #[test]
    fn no_helper_still_says_so_and_names_the_way_out() {
        let manifest = release_manifest();
        let nothing = |_: Option<&ReleaseHelper>| None;
        // A development build: no manifest at all.
        let (plan, local) = plan_with(
            Some("aarch64-unknown-linux-gnu"),
            None,
            "Linux",
            "aarch64",
            nothing,
            None,
        );
        let reason = plan.reason.clone().unwrap();
        assert!(!plan.available && local.is_none() && plan.source.is_none() && plan.size.is_none());
        assert!(reason.contains("This copy of Oleafly has no helper for Linux aarch64"));
        assert!(reason.contains("remote-daemons/aarch64-unknown-linux-gnu/oleafly-remoted"));
        assert!(reason.contains("build it on the host"));
        // A release that carries x86_64 only, asked about an ARM host and a Mac.
        for (triple, os, arch) in [
            ("aarch64-unknown-linux-gnu", "Linux", "aarch64"),
            ("aarch64-apple-darwin", "Darwin", "arm64"),
        ] {
            let (plan, _) = plan_with(Some(triple), None, os, arch, nothing, Some(&manifest));
            let reason = plan.reason.unwrap();
            assert!(!plan.available);
            assert!(
                reason.contains("carries helpers for x86_64-unknown-linux-gnu"),
                "{reason}"
            );
            assert!(reason.contains(&format!("remote-daemons/{triple}/oleafly-remoted")));
        }
        // musl is refused for its own reason even though the release has a Linux helper.
        let (plan, _) = plan_with(
            Some("x86_64-unknown-linux-gnu"),
            Some("musl"),
            "Linux",
            "x86_64",
            nothing,
            Some(&manifest),
        );
        assert!(!plan.available && plan.reason.unwrap().contains("musl"));
        // And a system with no target at all.
        let (plan, _) = plan_with(None, None, "FreeBSD", "amd64", nothing, Some(&manifest));
        assert!(plan.reason.unwrap().contains("FreeBSD amd64"));
    }

    fn elf_for(machine: u8) -> Vec<u8> {
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        elf[4] = 2;
        elf[5] = 1;
        elf[18] = machine;
        elf
    }

    #[test]
    fn a_downloaded_file_counts_only_when_it_is_the_pinned_one() {
        let triple = "x86_64-unknown-linux-gnu";
        let root = tempfile::tempdir().unwrap();
        let version = env!("CARGO_PKG_VERSION");
        let dir = helper_release::install_dir(root.path(), triple, version);
        std::fs::create_dir_all(&dir).unwrap();
        let elf = elf_for(0x3e);
        std::fs::write(dir.join(HELPER_NAME), &elf).unwrap();
        let (sha256, size) = hash_file(&dir.join(HELPER_NAME)).unwrap();
        let pinned = |sha256: &str, size: u64| ReleaseHelper {
            triple: triple.into(),
            asset: format!("oleafly-remoted-{version}-{triple}"),
            size,
            sha256: sha256.into(),
        };
        let candidates = helper_candidates(root.path(), triple, None);
        let found = find_acceptable_helper(&candidates, triple, Some(&pinned(&sha256, size)));
        assert_eq!(found.unwrap().source, HelperSource::Downloaded);
        assert_eq!(
            find_acceptable_helper(&candidates, triple, Some(&pinned(&"0".repeat(64), size))),
            None,
            "a file with another digest is fetched again"
        );
        assert_eq!(
            find_acceptable_helper(&candidates, triple, Some(&pinned(&sha256, size + 1))),
            None,
            "and one of another size"
        );
        assert_eq!(
            find_acceptable_helper(&candidates, triple, None),
            None,
            "a build that pins nothing does not trust a file it did not fetch"
        );
        // A file in the folder of another version is not looked at.
        let other = helper_release::install_dir(root.path(), triple, "0.0.1");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join(HELPER_NAME), &elf).unwrap();
        assert!(candidates.iter().all(|(path, _)| !path.starts_with(&other)));
        // One put there on purpose is taken as it is, and wins.
        let provided = root.path().join("remote-daemons").join(triple);
        std::fs::write(provided.join(HELPER_NAME), elf_for(0x3e)).unwrap();
        let found = find_acceptable_helper(&candidates, triple, None).unwrap();
        assert_eq!(found.source, HelperSource::Provided);
    }

    #[test]
    fn an_installed_helper_counts_only_when_version_and_protocol_match() {
        let expected = crate::remote_commands::daemon_version_line();
        let sha = "a".repeat(64);
        let text = |version: &str| {
            format!(
                "{GLIBC_PROBE}OLEAFLY_PROBE binary /home/ana/.oleafly/remoted/x/oleafly-remoted\nOLEAFLY_PROBE sha {sha}\nOLEAFLY_PROBE version {version}\n"
            )
        };
        let (probe, _, _) = build_probe(parse_probe(&text(&expected)).unwrap());
        let installed = probe.installed.unwrap();
        assert!(installed.current);
        assert_eq!(installed.sha256.as_deref(), Some(sha.as_str()));
        let (probe, _, _) =
            build_probe(parse_probe(&text("oleafly-remoted 0.0.1 protocol 1")).unwrap());
        assert!(!probe.installed.unwrap().current);
        let (probe, _, _) = build_probe(
            parse_probe(&text("error while loading shared libraries: libgtk-3.so.0")).unwrap(),
        );
        assert!(!probe.installed.unwrap().current);
    }

    #[test]
    fn a_checksum_that_is_not_a_sha256_is_dropped() {
        let text =
            format!("{GLIBC_PROBE}OLEAFLY_PROBE binary /h/b\nOLEAFLY_PROBE sha not-a-hash\n");
        assert_eq!(parse_probe(&text).unwrap().sha, None);
    }

    #[test]
    fn a_home_folder_too_long_for_the_helper_s_socket_is_said_so_before_anything_is_copied() {
        let long = format!("/home/{}", "a".repeat(90));
        assert!(socket_path_fits("/home/ana") && !socket_path_fits(&long));
        let text = format!(
            "OLEAFLY_PROBE os Linux\nOLEAFLY_PROBE arch x86_64\nOLEAFLY_PROBE home {long}\n"
        );
        let (probe, local, _) = build_probe(parse_probe(&text).unwrap());
        assert!(!probe.helper.available && local.is_none());
        assert!(probe.helper.reason.unwrap().contains("too long"));
    }

    #[test]
    fn the_paths_on_the_host_are_under_the_users_oleafly_folder() {
        let (binary, data) = host_paths("/home/ana");
        assert_eq!(
            binary,
            format!(
                "/home/ana/.oleafly/remoted/{}/oleafly-remoted",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert_eq!(data, "/home/ana/.oleafly/remote-data");
    }

    #[test]
    fn the_probe_script_names_the_version_it_expects_and_stays_in_oleafly_s_folder() {
        let script = probe_script();
        assert!(script.starts_with(&format!("V='{}'", env!("CARGO_PKG_VERSION"))));
        assert!(script.contains("$HOME/.oleafly/remoted/$V/oleafly-remoted"));
        for lib in LINUX_LIBS {
            assert!(script.contains(lib));
        }
        assert!(!script.contains("rm "), "the probe only reads");
    }

    #[test]
    fn the_install_script_is_one_line_checks_before_it_moves_and_quotes_everything() {
        let script = install_script(
            "/home/o'brien/.oleafly/remoted/0.4.11",
            &"b".repeat(64),
            HashTool::Shasum,
        );
        assert!(!script.contains('\n'));
        assert!(
            !script.contains('!'),
            "a login shell that expands ! would break it"
        );
        let sum = script.find("shasum -a 256").unwrap();
        let moved = script.find("mv -f").unwrap();
        assert!(
            sum < moved,
            "the checksum is checked before the helper gets its name"
        );
        assert!(script.contains(r#"d='/home/o'"'"'brien/.oleafly/remoted/0.4.11'"#));
        assert!(script.contains("umask 077"));
        assert!(script.contains("trap 'rm -f \"$t\"'"));
        // What the login shell sees is one quoted argument to `sh -c`.
        let remote = format!("sh -c {}", sh_quote(&script));
        assert!(remote.starts_with("sh -c '"));
    }

    #[test]
    fn a_helper_is_recognised_by_its_header() {
        let mut elf = vec![0u8; 32];
        elf[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        elf[4] = 2;
        elf[5] = 1;
        elf[18] = 0x3e;
        assert!(header_matches_triple(&elf, "x86_64-unknown-linux-gnu"));
        assert!(!header_matches_triple(&elf, "aarch64-unknown-linux-gnu"));
        assert!(!header_matches_triple(&elf, "aarch64-apple-darwin"));
        elf[18] = 0xb7;
        assert!(header_matches_triple(&elf, "aarch64-unknown-linux-gnu"));
        let mut macho = vec![0u8; 32];
        macho[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        macho[4..8].copy_from_slice(&0x0100_000cu32.to_le_bytes());
        assert!(header_matches_triple(&macho, "aarch64-apple-darwin"));
        assert!(!header_matches_triple(&macho, "x86_64-apple-darwin"));
        assert!(!header_matches_triple(&macho, "x86_64-unknown-linux-gnu"));
        assert!(!header_matches_triple(
            b"#!/bin/sh\necho hi\n",
            "x86_64-unknown-linux-gnu"
        ));
        assert!(!header_matches_triple(&[], "aarch64-apple-darwin"));
    }

    #[test]
    fn the_helper_is_found_where_it_was_provided_and_only_when_it_fits_the_host() {
        let root = tempfile::tempdir().unwrap();
        let triple = "aarch64-unknown-linux-gnu";
        let dir = root.path().join("remote-daemons").join(triple);
        std::fs::create_dir_all(&dir).unwrap();
        let candidates = helper_candidates(root.path(), triple, None);
        assert_eq!(find_helper(&candidates, triple), None, "nothing there yet");
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        elf[4] = 2;
        elf[5] = 1;
        elf[18] = 0xb7;
        std::fs::write(dir.join(HELPER_NAME), &elf).unwrap();
        let found = find_helper(&candidates, triple).unwrap();
        assert_eq!(found.size, 64);
        assert_eq!(found.sha256.len(), 64);
        assert_eq!(found.source, HelperSource::Provided);
        assert_eq!(
            find_helper(&candidates, "x86_64-unknown-linux-gnu"),
            None,
            "wrong machine type"
        );
        assert_eq!(found.sha256, hash_file(&dir.join(HELPER_NAME)).unwrap().0);
    }

    #[test]
    fn a_helper_next_to_the_app_is_offered_only_for_this_computers_own_kind() {
        let Some(own) = local_triple() else { return };
        let candidates =
            helper_candidates(Path::new("/data"), own, Some(Path::new("/app/bin/oleafly")));
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[1].0, Path::new("/app/bin/oleafly-remoted"));
        assert_eq!(
            candidates[2].1,
            HelperSource::Downloaded,
            "a fetched file comes last"
        );
        let other = if own.starts_with("x86_64") {
            "aarch64-unknown-linux-gnu"
        } else {
            "x86_64-unknown-linux-gnu"
        };
        assert_eq!(
            helper_candidates(
                Path::new("/data"),
                other,
                Some(Path::new("/app/bin/oleafly"))
            )
            .len(),
            2,
            "no file next to the app for another kind of host"
        );
    }

    #[test]
    fn explains_why_a_helper_that_is_there_cannot_start() {
        let libs = cannot_start("", &["libwebkit2gtk-4.1.so.0".to_owned()]);
        assert!(libs.contains("apt install") && libs.contains("libwebkit2gtk-4.1.so.0"));
        let loader = cannot_start("oleafly-remoted: error while loading shared libraries: libgtk-3.so.0: cannot open shared object file", &[]);
        assert!(loader.contains("needs libraries"));
        let glibc = cannot_start(
            "/lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.35' not found",
            &[],
        );
        assert!(glibc.contains("C library"));
        assert!(cannot_start("bash: ./x: Permission denied", &[]).contains("noexec"));
        assert!(cannot_start("boom", &[]).ends_with("boom"));
    }

    #[test]
    fn hosts_are_validated_before_ssh_sees_them() {
        let ok = |host: &str, user: Option<&str>, port: Option<u16>| {
            HostArg {
                host: host.into(),
                user: user.map(Into::into),
                port,
            }
            .endpoint()
        };
        let endpoint = ok("ana@lab-gpu.example.edu", None, Some(2222)).unwrap();
        assert_eq!(endpoint.host, "lab-gpu.example.edu");
        assert_eq!(endpoint.user.as_deref(), Some("ana"));
        assert_eq!(endpoint.port, Some(2222));
        assert_eq!(
            ok("box", Some("bob"), None).unwrap().user.as_deref(),
            Some("bob")
        );
        assert!(ok("-oProxyCommand=x", None, None).is_err());
        assert!(ok("a b", None, None).is_err());
        assert!(ok("", None, None).is_err());
        assert!(ok("box", Some("-l"), None).is_err());
        assert!(ok("box", Some("a;b"), None).is_err());
        assert!(ok("box", None, Some(0)).is_err());
        assert!(ok(&"a".repeat(300), None, None).is_err());
    }

    #[test]
    fn folder_scripts_quote_the_path_and_expand_only_the_home_prefix() {
        let script = list_script("~/my work/it's");
        assert!(script.starts_with("P='~/my work/it'\"'\"'s'"));
        assert!(script.contains(RESOLVE_PATH));
        assert!(!make_dir_script("/srv/x").contains("rm "));
        assert!(
            valid_host_path("~").is_ok()
                && valid_host_path("/srv").is_ok()
                && valid_host_path("~/a").is_ok()
        );
        assert!(
            valid_host_path("relative").is_err()
                && valid_host_path("").is_err()
                && valid_host_path("/a\nb").is_err()
        );
    }

    #[test]
    fn a_listing_keeps_the_first_path_and_ignores_folders_named_like_protocol_lines() {
        let text = "\
OLEAFLY_DIR path /home/ana/papers
OLEAFLY_DIR writable
OLEAFLY_DIR git
OLEAFLY_DIR tex
OLEAFLY_DIR entries 12
OLEAFLY_DIR dir zeta
OLEAFLY_DIR dir Alpha
OLEAFLY_DIR dir ok
OLEAFLY_DIR path /etc
OLEAFLY_DIR dir a/b
";
        let listing = parse_listing(text, "~/papers");
        assert_eq!(listing.state, DirState::Ok);
        assert_eq!(listing.path, "/home/ana/papers");
        assert_eq!(listing.parent.as_deref(), Some("/home/ana"));
        assert_eq!(listing.dirs, ["Alpha", "ok", "zeta"]);
        assert!(listing.writable && listing.has_git && listing.has_tex);
        assert_eq!(listing.entries, 12);
    }

    #[test]
    fn a_missing_or_closed_folder_lists_nothing() {
        let missing = parse_listing("OLEAFLY_DIR missing\n", "/srv/none");
        assert_eq!(
            (missing.state, missing.path.as_str()),
            (DirState::Missing, "/srv/none")
        );
        assert_eq!(
            parse_listing("OLEAFLY_DIR denied\n", "/root").state,
            DirState::Denied
        );
        assert_eq!(
            parse_listing("", "/x").state,
            DirState::Missing,
            "no answer is not an empty folder"
        );
        assert_eq!(parent_of("/"), None);
        assert_eq!(parent_of("/home"), Some("/".into()));
    }

    #[test]
    fn broad_folders_are_refused() {
        assert!(too_broad("/", "/home/ana"));
        assert!(too_broad("/home/ana", "/home/ana"));
        assert!(too_broad("/home/ana/", "/home/ana"));
        assert!(too_broad("/home", "/home/ana"));
        assert!(too_broad("/srv", "/home/ana"));
        assert!(!too_broad("/home/ana/papers", "/home/ana"));
        assert!(!too_broad("/srv/data/paper", "/home/ana"));
    }

    #[test]
    fn a_project_is_named_for_the_user_or_for_its_folder() {
        assert_eq!(project_name("  My paper ", "/h/x").unwrap(), "My paper");
        assert_eq!(project_name("", "/home/ana/thesis/").unwrap(), "thesis");
        assert!(project_name(&"x".repeat(81), "/h/x").is_err());
        assert!(project_name("a\nb", "/h/x").is_err());
    }

    #[test]
    fn failures_are_sorted_for_the_wizard() {
        let reason = |error: RunError| match failed(&error) {
            ProbeOutcome::Failed { reason, .. } => reason,
            _ => unreachable!(),
        };
        assert_eq!(reason(RunError::Auth("x".into())), FailReason::Auth);
        assert_eq!(reason(RunError::Network("x".into())), FailReason::Network);
        assert_eq!(reason(RunError::Unavailable("x".into())), FailReason::Ssh);
        assert_eq!(reason(RunError::Cancelled), FailReason::Cancelled);
        assert_eq!(reason(RunError::Remote("x".into())), FailReason::Other);
    }

    #[test]
    fn the_outcome_has_the_shape_the_frontend_reads() {
        let value = serde_json::to_value(ProbeOutcome::HostKey {
            host: "h".into(),
            keys: vec![HostKeyFingerprint {
                key_type: "ssh-ed25519".into(),
                fingerprint: "SHA256:x".into(),
            }],
            changed: false,
            message: None,
        })
        .unwrap();
        assert_eq!(value["status"], "hostKey");
        assert_eq!(value["keys"][0]["keyType"], "ssh-ed25519");
        assert_eq!(value["changed"], false);
        let value = serde_json::to_value(ProbeOutcome::Failed {
            reason: FailReason::Auth,
            message: "m".into(),
        })
        .unwrap();
        assert_eq!(
            (value["status"].as_str(), value["reason"].as_str()),
            (Some("failed"), Some("auth"))
        );
    }

    fn binding(host: &str, path: &str) -> Binding {
        Binding {
            host: host.into(),
            user: None,
            port: None,
            path: path.into(),
            main_doc: None,
            engine: None,
            persistent: true,
            binary: "/home/ana/.oleafly/remoted/x/oleafly-remoted".into(),
            data_dir: "/home/ana/.oleafly/remote-data".into(),
        }
    }

    /// Runs `body` with the data folder pointing at a folder of its own, as the project tests do.
    fn with_data_dir(name: &str, body: impl FnOnce(&Path)) {
        let _guard = crate::paths::data_dir_env_lock();
        let root = tempfile::Builder::new().prefix(name).tempdir().unwrap();
        std::env::set_var("OLEAFLY_DATA_DIR", root.path());
        body(root.path());
        std::env::remove_var("OLEAFLY_DATA_DIR");
    }

    #[test]
    fn a_binding_is_added_without_losing_the_others() {
        with_data_dir("bindings-add", |root| {
            let file = root.join("remote-workspaces.json");
            std::fs::write(
                &file,
                r#"{"projects":{"hand-made":{"host":"lab","path":"~/work","binary":"/b","dataDir":"/d","user":"ana","port":2200,"mainDoc":"paper.tex","engine":"latexmk"}}}"#,
            )
            .unwrap();
            update_config(|config| {
                config
                    .projects
                    .insert("new-one".into(), binding("box", "/srv/paper"));
                Ok(())
            })
            .unwrap();
            let text = std::fs::read_to_string(&file).unwrap();
            let config: Config = serde_json::from_str(&text).unwrap();
            assert_eq!(config.projects.len(), 2);
            let kept = &config.projects["hand-made"];
            assert_eq!(
                (kept.user.as_deref(), kept.port, kept.main_doc.as_deref()),
                (Some("ana"), Some(2200), Some("paper.tex"))
            );
            assert_eq!(kept.engine.as_deref(), Some("latexmk"));
            assert_eq!(config.projects["new-one"].path, "/srv/paper");
            assert!(text.contains("\"dataDir\"") && !text.contains("\"user\": null"));
            // The lookup the bridge makes finds both.
            assert_eq!(binding_for("new-one").unwrap().host, "box");
            assert_eq!(binding_for("hand-made").unwrap().host, "lab");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        });
    }

    #[test]
    fn a_bindings_file_that_cannot_be_read_is_never_overwritten() {
        with_data_dir("bindings-broken", |root| {
            let file = root.join("remote-workspaces.json");
            std::fs::write(&file, "{ this is not json").unwrap();
            let outcome = update_config(|config| {
                config.projects.insert("x".into(), binding("box", "/srv/p"));
                Ok(())
            });
            assert!(outcome.unwrap_err().contains("cannot be read"));
            assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                "{ this is not json"
            );
            // A change that fails leaves the file as it was, too.
            std::fs::write(&file, r#"{"projects":{}}"#).unwrap();
            let refused: Result<(), String> = update_config(|_| Err("no".into()));
            assert!(refused.is_err());
            assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                r#"{"projects":{}}"#
            );
            // Missing and empty files are an empty list.
            std::fs::remove_file(&file).unwrap();
            update_config(|config| {
                assert!(config.projects.is_empty());
                Ok(())
            })
            .unwrap();
        });
    }

    #[test]
    fn a_link_comes_back_only_while_a_project_is_still_bound_to_it() {
        with_data_dir("still-bound", |_| {
            let one = binding("box", "/srv/p1");
            assert!(!still_bound(&one), "nothing is bound yet");
            update_config(|config| {
                config.projects.insert("p1".into(), one.clone());
                Ok(())
            })
            .unwrap();
            assert!(still_bound(&one));
            // Another folder on the same host and helper shares the link.
            assert!(still_bound(&binding("box", "/srv/other")));
            assert!(!still_bound(&binding("elsewhere", "/srv/p1")));
            update_config(|config| {
                config.projects.remove("p1");
                Ok(())
            })
            .unwrap();
            assert!(!still_bound(&one));
        });
    }

    #[test]
    fn two_setups_at_once_both_keep_their_binding() {
        with_data_dir("bindings-race", |_| {
            let workers: Vec<_> = (0..8)
                .map(|n| {
                    std::thread::spawn(move || {
                        update_config(|config| {
                            config
                                .projects
                                .insert(format!("p{n}"), binding("box", &format!("/srv/p{n}")));
                            Ok(())
                        })
                    })
                })
                .collect();
            for worker in workers {
                worker.join().unwrap().unwrap();
            }
            assert_eq!(load_config().projects.len(), 8);
        });
    }

    #[test]
    fn the_local_half_of_a_remote_project_is_a_manifest_and_nothing_else() {
        with_data_dir("remote-stub", |root| {
            let id = crate::project::create_remote_stub("Remote paper".into()).unwrap();
            let dir = root.join("projects").join(&id);
            let names: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, ["project.json"], "no main.tex, no .git");
            let listed = crate::project::list_projects_blocking().unwrap();
            let entry = listed
                .iter()
                .find(|p| p.id == id)
                .expect("the library lists it");
            assert_eq!(entry.name, "Remote paper");
            crate::project::discard_remote_stub(&id).unwrap();
            assert!(!dir.exists());
            assert!(crate::project::list_projects_blocking()
                .unwrap()
                .iter()
                .all(|p| p.id != id));
            crate::project::discard_remote_stub(&id).unwrap();
        });
    }

    #[test]
    fn a_project_that_has_been_used_is_not_discarded() {
        with_data_dir("remote-stub-kept", |root| {
            let id = crate::project::create_remote_stub("Used".into()).unwrap();
            let dir = root.join("projects").join(&id);
            std::fs::write(dir.join("notes.txt"), "mine").unwrap();
            assert!(crate::project::discard_remote_stub(&id).is_err());
            assert!(dir.join("notes.txt").exists());
            assert!(crate::project::discard_remote_stub("../escape").is_err());
        });
    }

    /// The whole setup against a real host, as the wizard runs it. Needs a host reachable with
    /// the keys ssh finds by itself and a helper built for that host's kind:
    ///
    /// ```text
    /// REMOTE_SETUP_HOST=127.0.0.1 REMOTE_SETUP_PORT=22022 REMOTE_SETUP_USER=me \
    /// REMOTE_SETUP_HELPER=/path/to/oleafly-remoted \
    ///   cargo test -p oleafly --features remote-bridge --lib remote_bridge::setup -- --ignored --nocapture
    /// ```
    ///
    /// The host's `$HOME` is where `~/.oleafly` is written, so point it at a scratch home.
    #[test]
    #[ignore = "needs an ssh host and a built helper"]
    fn sets_a_host_up_and_binds_a_project() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let host = HostArg {
            host: var("REMOTE_SETUP_HOST"),
            user: std::env::var("REMOTE_SETUP_USER").ok(),
            port: std::env::var("REMOTE_SETUP_PORT")
                .ok()
                .and_then(|p| p.parse().ok()),
        };
        // Without REMOTE_SETUP_HELPER the helper must come from the release this build pinned
        // (a build given OLEAFLY_REMOTED_MANIFEST and OLEAFLY_REMOTED_RELEASE_BASE): the app then
        // downloads it itself, as an installed one does.
        let helper = std::env::var("REMOTE_SETUP_HELPER").ok().map(PathBuf::from);
        with_data_dir("setup-e2e", |root| {
            // The helper must be built for the host's kind; by default that is this computer's own.
            let triple_owned = std::env::var("REMOTE_SETUP_TRIPLE")
                .ok()
                .or_else(|| local_triple().map(str::to_owned))
                .expect("set REMOTE_SETUP_TRIPLE, or run this on a Mac or a Linux machine");
            let triple = triple_owned.as_str();
            // A folder on the host for the project; make it one the test may leave behind.
            let folder =
                std::env::var("REMOTE_SETUP_FOLDER").unwrap_or_else(|_| "~/papers/e2e".into());
            if let Some(helper) = &helper {
                let provided = root.join("remote-daemons").join(triple);
                std::fs::create_dir_all(&provided).unwrap();
                std::fs::copy(helper, provided.join(HELPER_NAME)).unwrap();
            }

            let app = tauri::test::mock_builder()
                .manage(RemoteBridge::default())
                .build(tauri::test::mock_context(tauri::test::noop_assets()))
                .unwrap();
            let handle = app.handle().clone();
            let bridge = handle.state::<RemoteBridge>();
            let progress = Arc::new(Mutex::new(Vec::<Value>::new()));
            let sink = progress.clone();
            handle.listen_any("remote:setup", move |event| {
                if let Ok(value) = serde_json::from_str::<Value>(event.payload()) {
                    lock(&sink).push(value);
                }
            });
            let states = Arc::new(Mutex::new(Vec::<String>::new()));
            let sink = states.clone();
            handle.listen_any("remote:state", move |event| {
                if let Ok(value) = serde_json::from_str::<Value>(event.payload()) {
                    if let Some(state) = value["state"].as_str() {
                        lock(&sink).push(state.to_owned());
                    }
                }
            });
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                let endpoint = host.endpoint().unwrap();
                let ctx = ssh_context().unwrap();
                let cancel = CancellationToken::new();

                // 1. A host nobody has trusted yet: the wizard is told its key, and nothing else happens.
                // (A host whose key ssh already knows goes straight on.)
                match probe_host(&ctx, &endpoint, &cancel).await {
                    Err(ProbeOutcome::HostKey { keys, changed, .. }) => {
                        assert!(!changed && !keys.is_empty());
                        println!("host keys offered: {keys:?}");
                        let fingerprints: Vec<String> = keys.iter().map(|k| k.fingerprint.clone()).collect();
                        host_keys::trust(&ctx, &endpoint, &fingerprints, &cancel).await.unwrap();
                    }
                    Err(other) => panic!("expected the host key question, got {other:?}"),
                    Ok(_) => println!("the host's key was already known"),
                }

                // 2. Looked at: this host's system, no helper yet, and one this app can install.
                let (probe, local, _) = probe_host(&ctx, &endpoint, &cancel).await.unwrap();
                println!("probe: {probe:#?}");
                assert_eq!(probe.triple.as_deref(), Some(triple));
                assert!(probe.installed.is_none(), "the scratch home starts empty");
                assert!(probe.helper.available);
                match &helper {
                    Some(_) => assert!(local.is_some() && probe.helper.source == Some(HelperSource::Provided)),
                    None => assert!(local.is_none() && probe.helper.source == Some(HelperSource::Release), "{:?}", probe.helper),
                }
                assert!(probe.home.starts_with('/'));
                let expected_sha = match &local {
                    Some(local) => local.sha256.clone(),
                    None => release_helper_for(triple).expect("the build pins a helper for this target").sha256,
                };

                // 3. Installed, checked on the host, and reported while it goes.
                let report = ensure_helper(&handle, &ctx, &endpoint, &cancel).await.unwrap();
                println!("install: {report:?}");
                assert!(!report.already_installed && report.bytes > 0);
                assert!(report.binary.starts_with(&probe.home));
                let phases: Vec<String> = lock(&progress)
                    .iter()
                    .filter_map(|v| v["phase"].as_str().map(str::to_owned))
                    .collect();
                assert!(phases.contains(&"upload".to_owned()) && phases.contains(&"done".to_owned()), "{phases:?}");

                // 4. Doing it again copies nothing, and the host now says the helper is current.
                let again = ensure_helper(&handle, &ctx, &endpoint, &cancel).await.unwrap();
                assert!(again.already_installed && again.bytes == 0);
                let (probe, _, _) = probe_host(&ctx, &endpoint, &cancel).await.unwrap();
                let installed = probe.installed.clone().expect("installed now");
                assert!(installed.current, "{installed:?}");
                assert_eq!(installed.sha256.as_deref(), Some(expected_sha.as_str()));

                // 5. A damaged copy on the host is replaced, not trusted.
                run_script(
                    &host,
                    format!("printf x >> '{}'\n", report.binary),
                )
                .await
                .unwrap();
                let repaired = ensure_helper(&handle, &ctx, &endpoint, &cancel).await.unwrap();
                assert!(!repaired.already_installed && repaired.bytes > 0);

                // 6. Folders: list the home, make one, refuse the broad ones.
                let home = parse_listing(&run_script(&host, list_script("~")).await.unwrap(), "~");
                assert_eq!((home.state, home.path.as_str()), (DirState::Ok, probe.home.as_str()));
                assert!(too_broad(&home.path, &probe.home));
                let made = parse_listing(&run_script(&host, make_dir_script(&folder)).await.unwrap(), "");
                assert_eq!(made.path, folder.replacen('~', &probe.home, 1));
                run_script(&host, format!("printf '\\documentclass{{article}}\\begin{{document}}Hi\\end{{document}}\n' > '{}/paper.tex'\n", made.path)).await.unwrap();
                let listing = parse_listing(&run_script(&host, list_script(&folder)).await.unwrap(), "");
                assert!(listing.has_tex && listing.writable && !listing.has_git, "{listing:?}");

                // 7. The refusals leave nothing behind.
                let before = crate::project::list_projects_blocking().unwrap().len();
                let broad = create_project_on_host(&handle, &bridge, CreateRequest { host: host.clone(), path: "~".into(), name: "x".into(), create_folder: false }).await;
                assert!(broad.unwrap_err().contains("too broad"));
                let missing = create_project_on_host(&handle, &bridge, CreateRequest { host: host.clone(), path: "~/nowhere/at/all".into(), name: "x".into(), create_folder: false }).await;
                assert!(missing.unwrap_err().contains("does not exist"));
                assert_eq!(crate::project::list_projects_blocking().unwrap().len(), before);
                assert!(load_config().projects.is_empty());

                // 7b. A helper that cannot start (its data folder is closed to writes): the project and the
                // binding are taken back at once, and no link keeps trying to come back for it.
                let data_dir = host_paths(&probe.home).1;
                run_script(&host, format!("mkdir -p '{data_dir}' && chmod 500 '{data_dir}'\n")).await.unwrap();
                let started = std::time::Instant::now();
                let broken = create_project_on_host(&handle, &bridge, CreateRequest { host: host.clone(), path: folder.clone(), name: "x".into(), create_folder: false }).await;
                let failure = broken.expect_err("the helper cannot open its lock");
                println!("create failed after {:?}: {failure}", started.elapsed());
                assert!(started.elapsed() < Duration::from_secs(20), "failing waited for the 30 second timeout");
                assert!(failure.contains("did not accept the project"), "{failure}");
                assert!(failure.contains("Permission denied"), "the host's reason is kept: {failure}");
                assert!(load_config().projects.is_empty(), "the binding is taken back");
                assert_eq!(crate::project::list_projects_blocking().unwrap().len(), before, "and so is the project");
                tokio::time::sleep(Duration::from_secs(4)).await;
                let seen = lock(&states).clone();
                assert!(!seen.iter().any(|s| s == "reconnecting"), "a failed setup started a reconnect loop: {seen:?}");
                run_script(&host, format!("chmod 700 '{data_dir}'\n")).await.unwrap();

                // 8. The project: a real call reaches the host's helper, and the folder is the host's.
                let created = create_project_on_host(&handle, &bridge, CreateRequest { host: host.clone(), path: folder.clone(), name: "  E2E paper ".into(), create_folder: false }).await.unwrap();
                let id = created.project_id;
                let binding = binding_for(&id).unwrap();
                assert_eq!((binding.path.as_str(), binding.persistent), (made.path.as_str(), true));
                assert_eq!(binding.binary, report.binary);
                let local_entry = crate::project::list_projects_blocking().unwrap().into_iter().find(|p| p.id == id).unwrap();
                assert_eq!(local_entry.name, "E2E paper");
                let tree = call(&handle, &bridge, &id, "list_file_tree", json!({ "projectId": id }), None).await.unwrap().json();
                println!("tree on the host: {tree}");
                assert!(tree.to_string().contains("paper.tex"));
                let project = call(&handle, &bridge, &id, "get_project", json!({ "projectId": id }), None).await.unwrap().json();
                assert_eq!(project["main_doc"], "paper.tex");
                // Nothing of the project is on this computer but its manifest.
                let names: Vec<String> = std::fs::read_dir(root.join("projects").join(&id)).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
                assert_eq!(names, ["project.json"]);

                // Cleanup: stop the helper's daemon.
                let _ = run_script(&host, format!("OLEAFLY_DATA_DIR='{}' '{}' --stop\n", binding.data_dir, binding.binary)).await;
            });
        });
    }

    /// The probe and the folder scripts against a real host, with nothing installed there: it reads
    /// the system, lists the home folder, and makes and removes one empty folder under
    /// `~/oleafly-scratch`. The host's key must already be known to ssh.
    ///
    /// ```text
    /// REMOTE_SETUP_HOST=my-host cargo test -p oleafly --features remote-spike,remote-bridge --lib \
    ///   remote_bridge::setup::tests::reads_a_real_host -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs an ssh host"]
    fn reads_a_real_host_and_leaves_nothing_behind() {
        let host = HostArg {
            host: std::env::var("REMOTE_SETUP_HOST").expect("set REMOTE_SETUP_HOST"),
            user: std::env::var("REMOTE_SETUP_USER").ok(),
            port: std::env::var("REMOTE_SETUP_PORT")
                .ok()
                .and_then(|p| p.parse().ok()),
        };
        with_data_dir("setup-real-host", |_| {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                let endpoint = host.endpoint().unwrap();
                let ctx = ssh_context().unwrap();
                let cancel = CancellationToken::new();
                let (probe, _, hash) = match probe_host(&ctx, &endpoint, &cancel).await {
                    Ok(found) => found,
                    Err(outcome) => panic!("the probe did not get an answer: {outcome:?}"),
                };
                println!("probe: {probe:#?}\nhash tool: {hash:?}");
                assert!(!probe.home.is_empty() && hash.is_some());

                let listing =
                    parse_listing(&run_script(&host, list_script("~")).await.unwrap(), "~");
                println!(
                    "home: {} dirs, {} entries, writable {}",
                    listing.dirs.len(),
                    listing.entries,
                    listing.writable
                );
                assert_eq!(
                    (listing.state, listing.path.as_str()),
                    (DirState::Ok, probe.home.as_str())
                );
                assert!(too_broad(&listing.path, &probe.home));
                assert_eq!(listing.parent.as_deref(), parent_of(&probe.home).as_deref());

                let missing = parse_listing(
                    &run_script(&host, list_script("~/definitely/not/here"))
                        .await
                        .unwrap(),
                    "x",
                );
                assert_eq!(missing.state, DirState::Missing);

                let name = format!("~/oleafly-scratch/wizard-probe-{}", std::process::id());
                let made = parse_listing(
                    &run_script(&host, make_dir_script(&name)).await.unwrap(),
                    "",
                );
                assert_eq!(made.state, DirState::Ok);
                let inside =
                    parse_listing(&run_script(&host, list_script(&name)).await.unwrap(), "");
                println!(
                    "made {} (writable {}, entries {})",
                    inside.path, inside.writable, inside.entries
                );
                assert!(inside.writable && inside.entries == 0 && inside.dirs.is_empty());
                assert!(!too_broad(&inside.path, &probe.home));
                // Only the empty folder this test made; rmdir refuses anything else.
                run_script(&host, format!("rmdir '{}'\n", inside.path))
                    .await
                    .unwrap();
                let gone = parse_listing(&run_script(&host, list_script(&name)).await.unwrap(), "");
                assert_eq!(gone.state, DirState::Missing, "the test folder was removed");
            });
        });
    }
}
