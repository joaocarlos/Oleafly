//! The helper that ships with an Oleafly release, for hosts of a kind this computer cannot build for.
//!
//! The release workflow builds `oleafly-remoted` for each Linux target and writes a small manifest
//! (version line, and per target the asset name, size and SHA-256). That manifest is handed to the
//! build of the app itself through `OLEAFLY_REMOTED_MANIFEST`, so the digests are inside the signed
//! app and a tampered release asset cannot vouch for itself. The helper is fetched on first use,
//! after the user agreed in the wizard, by the same installer that fetches the compilers
//! (`oleafly_downloads::toolchain`), which refuses a file whose size or digest differs from the
//! pinned values. A build without the variable (a development build) offers no download.
//!
//! Where it lands: `<data folder>/remote-daemons/<target>/<app version>/oleafly-remoted`. The
//! version is part of the path so that an update never picks up the previous release's helper.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use oleafly_agent::CancellationToken;
use oleafly_downloads::toolchain::{
    install_toolchain, ArchiveKind, CancelToken, DownloadTimeouts, InstallPhase, InstallProgress,
    ToolchainInstallError, ToolchainInstallRequest,
};
use serde::Deserialize;

/// What the release build embedded, if it did.
const EMBEDDED_MANIFEST: Option<&str> = option_env!("OLEAFLY_REMOTED_MANIFEST");
/// Release assets are served from here, tagged `v<app version>`. The release workflow sets
/// `OLEAFLY_REMOTED_RELEASE_BASE` to the repository that builds the app, so a fork's release finds
/// its own helpers; a build without it uses the upstream release.
const RELEASE_BASE: &str = match option_env!("OLEAFLY_REMOTED_RELEASE_BASE") {
    Some(base) => base,
    None => "https://github.com/Oleafly/Oleafly/releases/download",
};
/// github.com answers with a redirect to one of the other two.
const ALLOWED_HOSTS: [&str; 3] = [
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];
/// Folder under the data folder that holds the helpers offered to hosts.
pub(super) const DAEMONS_FOLDER: &str = "remote-daemons";
/// The Linux helper is tens of megabytes; anything near this is not the helper.
const MAX_HELPER_BYTES: u64 = 512 * 1024 * 1024;

/// One helper the release carries, as the app was built to expect it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReleaseHelper {
    pub(super) triple: String,
    pub(super) asset: String,
    pub(super) size: u64,
    pub(super) sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {
    asset: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    version: String,
    line: String,
    targets: BTreeMap<String, RawTarget>,
}

/// A manifest that belongs to this build: same version, same `--version` line, every entry sane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Manifest {
    helpers: Vec<ReleaseHelper>,
}

fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A release asset name is one path segment of plain characters, so it cannot reach another file.
fn is_asset_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.starts_with("oleafly-remoted-")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// A target name, as it appears in a path. Only the targets the setup knows how to select.
fn is_target(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

impl Manifest {
    /// Reads and checks a manifest for the app `version` whose helper prints `line`.
    pub(super) fn parse(json: &str, version: &str, line: &str) -> Result<Self, String> {
        let raw: RawManifest =
            serde_json::from_str(json).map_err(|error| format!("unreadable: {error}"))?;
        if raw.version != version {
            return Err(format!("is for version {}, not {version}", raw.version));
        }
        if raw.line != line {
            return Err(format!(
                "was built for {:?}, this app expects {line:?}",
                raw.line
            ));
        }
        let mut helpers = Vec::new();
        for (triple, target) in raw.targets {
            if !is_target(&triple) {
                return Err(format!("names a target that is not valid: {triple:?}"));
            }
            if !is_asset_name(&target.asset) {
                return Err(format!(
                    "names an asset that is not valid: {:?}",
                    target.asset
                ));
            }
            if !(1..=MAX_HELPER_BYTES).contains(&target.size) {
                return Err(format!("gives {triple} a size that is out of range"));
            }
            if !is_digest(&target.sha256) {
                return Err(format!("gives {triple} a checksum that is not a SHA-256"));
            }
            helpers.push(ReleaseHelper {
                triple,
                asset: target.asset,
                size: target.size,
                sha256: target.sha256,
            });
        }
        Ok(Self { helpers })
    }

    /// The helper for a host's target, when this release carries one.
    pub(super) fn for_triple(&self, triple: &str) -> Option<&ReleaseHelper> {
        self.helpers.iter().find(|helper| helper.triple == triple)
    }

    /// Target names, for messages.
    pub(super) fn triples(&self) -> Vec<&str> {
        self.helpers.iter().map(|h| h.triple.as_str()).collect()
    }
}

/// The manifest this build was given, when it was given a valid one.
pub(super) fn embedded() -> Option<Manifest> {
    let json = EMBEDDED_MANIFEST?;
    match Manifest::parse(
        json,
        env!("CARGO_PKG_VERSION"),
        &crate::remote_commands::daemon_version_line(),
    ) {
        Ok(manifest) => Some(manifest),
        Err(problem) => {
            // A manifest that is wrong for this build offers nothing, so the wizard says there is
            // no helper instead of fetching a file it cannot vouch for.
            eprintln!("remote helper manifest ignored: it {problem}");
            None
        }
    }
}

/// `<data folder>/remote-daemons/<target>/<version>`
pub(super) fn install_dir(data_root: &Path, triple: &str, version: &str) -> PathBuf {
    data_root.join(DAEMONS_FOLDER).join(triple).join(version)
}

pub(super) fn url(version: &str, asset: &str) -> String {
    format!("{RELEASE_BASE}/v{version}/{asset}")
}

/// The request the installer is given: the pinned size and digest, nothing taken from the network.
pub(super) fn request(
    data_root: &Path,
    helper: &ReleaseHelper,
    version: &str,
    binary_name: &str,
) -> ToolchainInstallRequest {
    ToolchainInstallRequest {
        urls: vec![url(version, &helper.asset)],
        allowed_hosts: ALLOWED_HOSTS
            .iter()
            .map(|host| (*host).to_owned())
            .collect(),
        archive_kind: ArchiveKind::Binary,
        archive_size: helper.size,
        archive_sha256: helper.sha256.clone(),
        archive_member: None,
        binary_size: helper.size,
        binary_sha256: helper.sha256.clone(),
        install_dir: install_dir(data_root, &helper.triple, version),
        binary_name: binary_name.to_owned(),
        timeouts: DownloadTimeouts::default(),
    }
}

/// Words for the user when the download or its check failed.
pub(super) fn describe_failure(
    error: &ToolchainInstallError,
    triple: &str,
    binary: &str,
) -> String {
    let what = if error.digest_mismatch() {
        "The file that was downloaded is not the one this version of Oleafly expects, so it was thrown away."
    } else {
        "Oleafly could not download the helper for this version."
    };
    format!(
        "{what} {} You can put a helper at {DAEMONS_FOLDER}/{triple}/{binary} in Oleafly's data folder instead, or build it on the host.",
        error.message()
    )
}

/// Fetches the helper into its folder (nothing is downloaded when the pinned file is there) and
/// reports `(received, total)` bytes as it goes. Returns the path of the file.
pub(super) async fn obtain<F>(
    data_root: &Path,
    helper: &ReleaseHelper,
    version: &str,
    binary_name: &str,
    cancel: &CancellationToken,
    report: F,
) -> Result<PathBuf, ToolchainInstallError>
where
    F: Fn(u64, u64) + Send + Sync + 'static,
{
    let request = request(data_root, helper, version, binary_name);
    let stop = CancelToken::new();
    if cancel.is_cancelled() {
        stop.cancel();
    }
    let relay = {
        let (cancel, stop) = (cancel.clone(), stop.clone());
        tokio::spawn(async move {
            cancel.cancelled().await;
            stop.cancel();
        })
    };
    let result = install_toolchain(request, stop, move |progress: InstallProgress| {
        if progress.phase == InstallPhase::Downloading || progress.phase == InstallPhase::Done {
            report(progress.received_bytes, progress.total_bytes);
        }
    })
    .await;
    relay.abort();
    result.map(|outcome| outcome.binary_path().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "oleafly-remoted 1.2.3 protocol 4";

    fn digest(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn manifest_json() -> String {
        format!(
            r#"{{"version":"1.2.3","line":"{LINE}","targets":{{
                "x86_64-unknown-linux-gnu":{{"asset":"oleafly-remoted-1.2.3-x86_64-unknown-linux-gnu","size":1000,"sha256":"{}"}},
                "aarch64-unknown-linux-gnu":{{"asset":"oleafly-remoted-1.2.3-aarch64-unknown-linux-gnu","size":2000,"sha256":"{}"}}
            }}}}"#,
            digest('a'),
            digest('b')
        )
    }

    fn parse(json: &str) -> Result<Manifest, String> {
        Manifest::parse(json, "1.2.3", LINE)
    }

    #[test]
    fn a_manifest_for_this_build_selects_the_helper_by_target() {
        let manifest = parse(&manifest_json()).unwrap();
        let x86 = manifest.for_triple("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(x86.size, 1000);
        assert_eq!(x86.sha256, digest('a'));
        let arm = manifest.for_triple("aarch64-unknown-linux-gnu").unwrap();
        assert_eq!(arm.asset, "oleafly-remoted-1.2.3-aarch64-unknown-linux-gnu");
        assert_eq!(arm.sha256, digest('b'));
        assert_eq!(manifest.for_triple("aarch64-apple-darwin"), None);
        assert_eq!(manifest.triples().len(), 2);
    }

    #[test]
    fn a_manifest_for_another_version_or_protocol_offers_nothing() {
        let json = manifest_json();
        assert!(Manifest::parse(&json, "1.2.4", LINE)
            .unwrap_err()
            .contains("not 1.2.4"));
        assert!(
            Manifest::parse(&json, "1.2.3", "oleafly-remoted 1.2.3 protocol 5")
                .unwrap_err()
                .contains("protocol")
        );
    }

    #[test]
    fn a_manifest_with_a_bad_entry_is_refused_whole() {
        let good = manifest_json();
        let refused = |from: &str, to: &str| parse(&good.replace(from, to)).unwrap_err();
        assert!(refused(&digest('a'), "abc").contains("SHA-256"));
        assert!(
            refused(&digest('a'), &digest('A')).contains("SHA-256"),
            "upper case is not normalised"
        );
        assert!(refused("\"size\":1000", "\"size\":0").contains("size"));
        assert!(refused("\"size\":1000", "\"size\":99999999999").contains("size"));
        assert!(
            refused("oleafly-remoted-1.2.3-x86_64-unknown-linux-gnu", "../evil").contains("asset")
        );
        assert!(refused(
            "oleafly-remoted-1.2.3-x86_64-unknown-linux-gnu",
            "oleafly-remoted-1.2.3/../../x"
        )
        .contains("asset"));
        assert!(refused("\"x86_64-unknown-linux-gnu\"", "\"../up\"").contains("target"));
        assert!(parse("not json").unwrap_err().contains("unreadable"));
        assert!(parse(r#"{"version":"1.2.3","line":"x","targets":{},"extra":1}"#).is_err());
    }

    #[test]
    fn the_request_pins_size_and_digest_and_stays_on_github() {
        let manifest = parse(&manifest_json()).unwrap();
        let helper = manifest.for_triple("aarch64-unknown-linux-gnu").unwrap();
        let request = request(Path::new("/data"), helper, "1.2.3", "oleafly-remoted");
        assert_eq!(
            request.urls,
            ["https://github.com/Oleafly/Oleafly/releases/download/v1.2.3/oleafly-remoted-1.2.3-aarch64-unknown-linux-gnu"]
        );
        assert_eq!(request.archive_kind, ArchiveKind::Binary);
        assert_eq!(
            (request.archive_size, request.binary_size),
            (2000, 2000),
            "a bare binary must match its pinned size"
        );
        assert_eq!(request.archive_sha256, digest('b'));
        assert_eq!(request.binary_sha256, digest('b'));
        assert_eq!(
            request.install_dir,
            Path::new("/data/remote-daemons/aarch64-unknown-linux-gnu/1.2.3")
        );
        assert_eq!(request.binary_name, "oleafly-remoted");
        assert!(request
            .allowed_hosts
            .iter()
            .all(|host| { host == "github.com" || host.ends_with(".githubusercontent.com") }));
        assert!(request.urls.iter().all(|url| url.starts_with("https://")));
    }

    #[test]
    fn the_embedded_manifest_is_used_only_when_it_belongs_to_this_build() {
        // The variable is set only by the release workflow, at compile time. Run this with it set
        // to see the other side: a manifest for this build is used, any other is ignored.
        let belongs = EMBEDDED_MANIFEST.is_some_and(|json| {
            Manifest::parse(
                json,
                env!("CARGO_PKG_VERSION"),
                &crate::remote_commands::daemon_version_line(),
            )
            .is_ok()
        });
        assert_eq!(embedded().is_some(), belongs);
        if EMBEDDED_MANIFEST.is_none() {
            assert_eq!(embedded(), None, "a development build offers no download");
        }
    }

    #[test]
    fn a_failed_download_names_the_fallback() {
        // `ToolchainInstallError` has no public constructor; the wording is checked through a cancelled install.
        let folder = tempfile::tempdir().unwrap();
        let manifest = parse(&manifest_json()).unwrap();
        let helper = manifest.for_triple("x86_64-unknown-linux-gnu").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = runtime
            .block_on(obtain(
                folder.path(),
                helper,
                "1.2.3",
                "oleafly-remoted",
                &cancel,
                |_, _| {},
            ))
            .unwrap_err();
        let text = describe_failure(&error, &helper.triple, "oleafly-remoted");
        assert!(text.contains("remote-daemons/x86_64-unknown-linux-gnu/oleafly-remoted"));
        assert!(text.contains("build it on the host"));
        assert!(
            !folder
                .path()
                .join("remote-daemons/x86_64-unknown-linux-gnu/1.2.3/oleafly-remoted")
                .exists(),
            "a cancelled download leaves no helper behind"
        );
    }
}
