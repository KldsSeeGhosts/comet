//! Noches release discovery and installation shared by the desktop and CLI.
//! Each distributed build has a compiled dev/stable channel. GitHub channel
//! manifests reference immutable release artifacts with mandatory SHA-256 and
//! size checks. Local source builds never install published updates.
//! Linux uses versioned directories and an atomic current symlink; macOS
//! stages and replaces an ad-hoc or Developer ID signed application bundle.

pub mod identity;

#[derive(Default)]
pub struct DownloadProgress {
    pub received: std::sync::atomic::AtomicU64,
    pub total: std::sync::atomic::AtomicU64,
}

tokio::task_local! { static PROGRESS: Arc<DownloadProgress>; }

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, bail};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::watch;

#[cfg(windows)]
pub mod windows;

/// The version compiled into this binary (the workspace version).
pub const fn current_version() -> &'static str {
    match option_env!("NOCHES_VERSION") {
        Some(version) => version,
        None => env!("CARGO_PKG_VERSION"),
    }
}

/// Background check cadence.
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
/// Retry sooner after a failed check (offline boot, transient edge error).
const CHECK_RETRY: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// First check waits out engine boot (room joins, doc re-sync).
const CHECK_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(20);
/// While an auto-apply is deferred behind active sessions, re-probe idleness
/// this often.
const IDLE_RECHECK: std::time::Duration = std::time::Duration::from_secs(5 * 60);

// ---------------------------------------------------------------------------
// Release metadata
// ---------------------------------------------------------------------------

/// Noches channel manifest written only after all release assets are uploaded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub product: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub notes_url: String,
    /// Artifact file name to mandatory verification metadata.
    #[serde(default)]
    pub files: BTreeMap<String, FileMeta>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileMeta {
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// Artifact-name platform pair matching the packaging scripts. Unsupported
/// updater targets retain their real OS name rather than impersonating Linux.
pub fn platform_key() -> (&'static str, &'static str) {
    let os = std::env::consts::OS;
    let arch = match (os, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64",
        (_, arch) => arch,
    };
    (os, arch)
}

fn managed_updates_supported(os: &str) -> bool {
    matches!(os, "linux" | "macos")
}

fn require_managed_update_platform() -> anyhow::Result<()> {
    if !managed_updates_supported(std::env::consts::OS) {
        bail!(
            "managed updates are not supported on {}",
            std::env::consts::OS
        );
    }
    Ok(())
}

fn require_mac_app_update_platform() -> anyhow::Result<()> {
    if std::env::consts::OS != "macos" {
        bail!(
            "macOS app updates are not supported on {}",
            std::env::consts::OS
        );
    }
    Ok(())
}

/// `zeron-<ver>-<os>-<arch>.tar.gz` — the headless/CLI tarball (Linux CI builds).
pub fn headless_artifact(version: &str) -> String {
    let (os, arch) = platform_key();
    format!("noches-{version}-{os}-{arch}.tar.gz")
}

/// `zeron-<ver>-macos-<arch>-app.tar.gz` — the macOS app update payload.
pub fn mac_app_artifact(version: &str) -> String {
    let (_, arch) = platform_key();
    format!("noches-{version}-macos-{arch}-app.tar.gz")
}

/// SemVer precedence, including numeric development build components.
/// Invalid versions and build-metadata-only changes never count as upgrades.
pub fn version_newer(latest: &str, current: &str) -> bool {
    match (
        semver::Version::parse(latest.trim().trim_start_matches('v')),
        semver::Version::parse(current.trim().trim_start_matches('v')),
    ) {
        (Ok(latest), Ok(current)) => latest.cmp_precedence(&current).is_gt(),
        _ => false,
    }
}

/// Validate that a version string is safe for use as a path component and follows
/// expected release version syntax (ASCII alphanumeric characters, dots, hyphens,
/// underscores, no path separators or `..` traversals, with at least one digit).
pub fn validate_version(version: &str) -> anyhow::Result<&str> {
    let trimmed = version.trim();
    if trimmed.is_empty() || trimmed != version || trimmed.len() > 128 {
        bail!("version string cannot be empty");
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains("..") {
        bail!("malformed version string contains path separators or traversal: {trimmed}");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        bail!("malformed version string contains invalid characters: {trimmed}");
    }
    if !trimmed.chars().any(|c| c.is_ascii_digit()) {
        bail!("version string must contain at least one digit: {trimmed}");
    }
    Ok(trimmed)
}

/// Fetch the compiled channel independently of the workspace sync endpoint.
pub async fn fetch_latest(edge_url: &str) -> anyhow::Result<Manifest> {
    anyhow::ensure!(
        identity::distributed(),
        "This is a local source build. Install Noches or Noches Dev to receive updates."
    );
    let base = release_base(edge_url)?;
    let response = http_client()?
        .get(format!("{base}/manifest.json"))
        .send()
        .await
        .context("checking for Noches updates")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("No {} release has been published yet", identity::channel());
    }
    let manifest: Manifest = response
        .error_for_status()?
        .json()
        .await
        .context("reading update manifest")?;
    validate_manifest(&manifest, identity::channel())?;
    Ok(manifest)
}

fn validate_manifest(manifest: &Manifest, channel: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        manifest.product == "noches",
        "Update belongs to a different application"
    );
    anyhow::ensure!(
        manifest.channel == channel,
        "Update belongs to a different channel"
    );
    validate_version(&manifest.version)?;
    let version = semver::Version::parse(&manifest.version)?;
    anyhow::ensure!(
        if channel == "stable" {
            version.pre.is_empty()
        } else {
            channel == "dev" && version.pre.as_str().starts_with("dev.")
        },
        "Version does not match update channel"
    );
    anyhow::ensure!(!manifest.files.is_empty(), "Update contains no artifacts");
    for meta in manifest.files.values() {
        let hash = meta.sha256.as_deref().unwrap_or("");
        anyhow::ensure!(
            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "Update requires a SHA-256 checksum"
        );
        anyhow::ensure!(
            meta.size.is_some_and(|size| size > 0),
            "Update requires an artifact size"
        );
        let url = reqwest::Url::parse(meta.url.as_deref().unwrap_or(""))?;
        anyhow::ensure!(
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none(),
            "Update artifact requires an HTTPS URL without credentials"
        );
    }
    Ok(())
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    http_client_with_timeouts(
        std::time::Duration::from_secs(15),
        std::time::Duration::from_secs(30),
    )
}

fn http_client_with_timeouts(
    connect: std::time::Duration,
    read: std::time::Duration,
) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(connect)
        // Inactivity timeout, not a total download cap: slow progressing
        // updates remain viable on constrained links.
        .read_timeout(read)
        .user_agent(format!("Noches/{}", current_version()))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many update redirects");
            }
            if attempt.previous().iter().any(|url| url.scheme() == "https")
                && attempt.url().scheme() != "https"
            {
                return attempt.error("update redirect would downgrade HTTPS");
            }
            attempt.follow()
        }))
        .build()
        .context("building http client")
}

fn validate_release_override(value: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(value.trim()).context("invalid update feed URL")?;
    anyhow::ensure!(
        url.scheme() == "https" && url.host_str().is_some(),
        "update feed must use HTTPS"
    );
    anyhow::ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "update feed must be a base URL without credentials, query, or fragment"
    );
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn release_base(_edge_url: &str) -> anyhow::Result<String> {
    if let Ok(url) = std::env::var("NOCHES_RELEASES_URL")
        && !url.trim().is_empty()
    {
        return validate_release_override(&url);
    }
    Ok(format!(
        "https://github.com/{}/releases/download/noches-{}",
        identity::repository(),
        identity::channel()
    ))
}

// ---------------------------------------------------------------------------
// Install-kind detection
// ---------------------------------------------------------------------------

/// How this binary was installed — decides the update path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// `~/.zeron/app/<ver>/zeron` behind the `current` symlink
    /// (curl|sh installer / a previous `zeron update`).
    Managed { app_root: PathBuf },
    /// Running out of a macOS `.app` bundle.
    MacApp { bundle: PathBuf },
    /// Portable Windows package with an explicit update-feed configuration.
    #[cfg(windows)]
    WindowsPortable { directory: PathBuf },
    /// Source build or hand-copied binary — updates are report-only.
    Unmanaged,
}

impl InstallKind {
    pub fn supports_desktop_update(&self) -> bool {
        if !identity::distributed() {
            return false;
        }
        match self {
            Self::MacApp { .. } => true,
            Self::Managed { .. } => cfg!(target_os = "linux"),
            #[cfg(windows)]
            Self::WindowsPortable { .. } => true,
            _ => false,
        }
    }

    pub async fn stage_desktop(
        &self,
        edge_url: &str,
        manifest: &Manifest,
        data_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        match self {
            Self::MacApp { .. } => stage_mac_app(edge_url, manifest, data_dir).await,
            Self::Managed { app_root } => stage_headless(edge_url, manifest, app_root).await,
            #[cfg(windows)]
            Self::WindowsPortable { directory } => {
                windows::stage(edge_url, manifest, directory).await
            }
            _ => bail!("this installation does not support desktop updates"),
        }
    }

    pub async fn stage_with_progress(
        &self,
        edge_url: &str,
        manifest: &Manifest,
        data_dir: &Path,
        progress: Arc<DownloadProgress>,
    ) -> anyhow::Result<PathBuf> {
        validate_manifest(manifest, identity::channel())?;
        anyhow::ensure!(
            version_newer(&manifest.version, current_version()),
            "Update is not newer than this installation"
        );
        PROGRESS
            .scope(progress, self.stage_desktop(edge_url, manifest, data_dir))
            .await
    }

    /// Install and arrange a relaunch. The UI must quit after this succeeds.
    pub fn apply_desktop(&self, staged: &Path, local_engine_checked: bool) -> anyhow::Result<()> {
        match self {
            Self::MacApp { bundle } => {
                apply_mac_app(staged, bundle)?;
                relaunch_app_after_exit(bundle)?;
                Ok(())
            }
            Self::Managed { app_root } if cfg!(target_os = "linux") => {
                if !local_engine_checked
                    && std::process::Command::new("systemctl")
                        .args(["--user", "is-active", "--quiet", identity::service_name()])
                        .status()
                        .is_ok_and(|status| status.success())
                {
                    bail!(
                        "Close the remote window and update from the local window so active local work can be checked"
                    );
                }
                anyhow::ensure!(
                    staged.parent() == Some(app_root.as_path()),
                    "Update is outside this installation"
                );
                let version = staged
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("Invalid staged version")?;
                apply_headless(app_root, version)?;
                relaunch_linux_after_exit(&app_root.join("current/zeron"))
            }
            #[cfg(windows)]
            Self::WindowsPortable { directory } => windows::apply(staged, directory, true),
            _ => bail!("this installation does not support desktop updates"),
        }
    }
}

pub fn detect_install() -> InstallKind {
    let Ok(exe) = std::env::current_exe() else {
        return InstallKind::Unmanaged;
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    detect_install_from(&exe, home.as_deref())
}

fn detect_install_from(exe: &Path, home: Option<&Path>) -> InstallKind {
    detect_install_from_for_os(exe, home, std::env::consts::OS)
}

fn detect_install_from_for_os(exe: &Path, home: Option<&Path>, os: &str) -> InstallKind {
    #[cfg(windows)]
    if os == "windows" && windows::is_managed(exe) {
        return InstallKind::WindowsPortable {
            directory: exe.parent().unwrap().to_owned(),
        };
    }
    // Never interpret a coincidental Windows `%HOME%\.zeron\app` layout as
    // the Unix symlink-managed installation.
    if !managed_updates_supported(os) {
        return InstallKind::Unmanaged;
    }
    if let Some(home) = home {
        // `current_exe` resolves the `current` symlink to the versioned dir.
        let app_root = identity::app_root(home);
        if exe.starts_with(&app_root) {
            return InstallKind::Managed { app_root };
        }
    }
    for ancestor in exe.ancestors() {
        if ancestor.extension().is_some_and(|ext| ext == "app")
            && exe.starts_with(ancestor.join("Contents").join("MacOS"))
        {
            return InstallKind::MacApp {
                bundle: ancestor.to_path_buf(),
            };
        }
    }
    InstallKind::Unmanaged
}

// ---------------------------------------------------------------------------
// Download + verify
// ---------------------------------------------------------------------------

/// Stream the manifest artifact to `dest`, verifying its mandatory SHA-256 and
/// size. Writes through a `.partial` sidecar so an interrupted download never
/// leaves a plausible-looking artifact behind.
pub async fn download_release_file(
    edge_url: &str,
    manifest: &Manifest,
    file: &str,
    dest: &Path,
) -> anyhow::Result<()> {
    download_release_file_verified(edge_url, manifest, file, dest)
        .await
        .map(|_| ())
}

async fn download_release_file_verified(
    edge_url: &str,
    manifest: &Manifest,
    file: &str,
    dest: &Path,
) -> anyhow::Result<String> {
    let _ = edge_url;
    let meta = manifest
        .files
        .get(file)
        .context("No update artifact for this platform")?;
    let url = meta.url.as_deref().context("Update artifact URL missing")?;
    let expected = meta.sha256.as_deref().context("Update checksum missing")?;
    anyhow::ensure!(
        expected.len() == 64 && expected.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid update checksum"
    );
    anyhow::ensure!(
        reqwest::Url::parse(url)?.scheme() == "https"
            || (cfg!(test) && url.starts_with("http://127.0.0.1:")),
        "Update requires HTTPS"
    );
    let _ = PROGRESS.try_with(|progress| {
        progress
            .total
            .store(meta.size.unwrap_or(0), std::sync::atomic::Ordering::Relaxed)
    });
    let mut received = 0u64;
    let partial = dest.with_extension("partial");
    let resp = http_client()?
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    let mut out = tokio::fs::File::create(&partial)
        .await
        .with_context(|| format!("creating {}", partial.display()))?;
    let mut hasher = Sha256::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading download stream")?;
        received += chunk.len() as u64;
        let _ = PROGRESS.try_with(|progress| {
            progress
                .received
                .store(received, std::sync::atomic::Ordering::Relaxed)
        });
        if let Some(size) = meta.size {
            anyhow::ensure!(received <= size, "Update exceeds advertised size");
        }
        hasher.update(&chunk);
        out.write_all(&chunk).await.context("writing download")?;
    }
    out.flush().await.context("flushing download")?;
    drop(out);
    anyhow::ensure!(meta.size == Some(received), "Update size mismatch");
    let actual = format!("{:x}", hasher.finalize());
    if let Some(expected) = Some(expected) {
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            tokio::fs::remove_file(&partial).await.ok();
            bail!("checksum mismatch for {file}: expected {expected}, got {actual}");
        }
    }
    tokio::fs::rename(&partial, dest)
        .await
        .with_context(|| format!("moving {} into place", dest.display()))?;
    Ok(actual)
}

fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Stage completion marker file written after an archive is fully unpacked
/// and verified.
const STAGE_COMPLETE_FILE: &str = ".stage-complete";

/// Metadata record stored in `STAGE_COMPLETE_FILE` tying the staged files
/// to the verified artifact digest and release version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StageRecord {
    version: String,
    artifact: String,
    sha256: String,
}

fn staged_mac_app_valid(
    dir: &Path,
    version: &str,
    file: &str,
    expected_sha256: Option<&str>,
) -> bool {
    let complete_file = dir.join(STAGE_COMPLETE_FILE);
    let Ok(data) = std::fs::read(&complete_file) else {
        return false;
    };
    let Ok(record) = serde_json::from_slice::<StageRecord>(&data) else {
        return false;
    };
    if record.version != version || record.artifact != file {
        return false;
    }
    if record.sha256.is_empty() {
        return false;
    }
    if let Some(expected) = expected_sha256 {
        if !record.sha256.eq_ignore_ascii_case(expected.trim()) {
            return false;
        }
    }
    let staged = dir.join(identity::bundle_name());
    staged.is_dir()
        && staged.join("Contents/MacOS/zeron").is_file()
        && staged.join("Contents/Info.plist").is_file()
}

// ---------------------------------------------------------------------------
// Managed (symlink) installs — the daemon/VPS path
// ---------------------------------------------------------------------------

/// Download + unpack the headless tarball into `app_root/<ver>` (idempotent —
/// an already-staged version is reused). Returns the versioned dir.
pub async fn stage_headless(
    edge_url: &str,
    manifest: &Manifest,
    app_root: &Path,
) -> anyhow::Result<PathBuf> {
    // Reject unsupported targets before creating a stage or making a request.
    require_managed_update_platform()?;
    let version = validate_version(&manifest.version)?;
    let dest = app_root.join(version);
    let file = headless_artifact(version);
    let expected = manifest
        .files
        .get(&file)
        .and_then(|meta| meta.sha256.as_deref())
        .context("Missing update checksum")?;
    let record = StageRecord {
        version: version.to_owned(),
        artifact: file.clone(),
        sha256: expected.to_owned(),
    };
    if dest.join("zeron").is_file()
        && std::fs::read(dest.join(STAGE_COMPLETE_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<StageRecord>(&bytes).ok())
            .as_ref()
            == Some(&record)
    {
        return Ok(dest);
    }
    anyhow::ensure!(
        !dest.exists(),
        "An incomplete or different version already occupies the update directory"
    );
    let stage = app_root.join(format!(".stage-{version}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).with_context(|| format!("creating {}", stage.display()))?;
    let result = async {
        let tarball = stage.join(&file);
        download_release_file(edge_url, manifest, &file, &tarball).await?;
        let unpacked = stage.join("unpacked");
        std::fs::create_dir_all(&unpacked)?;
        // Tarball root is the versioned stage dir (see scripts/package-linux.sh);
        // strip it exactly as install.sh does.
        run(
            "tar",
            &[
                "-xzf",
                &tarball.to_string_lossy(),
                "-C",
                &unpacked.to_string_lossy(),
                "--strip-components=1",
            ],
        )?;
        if !unpacked.join("zeron").is_file() {
            bail!("tarball {file} did not contain a zeron binary");
        }
        std::fs::write(
            unpacked.join(STAGE_COMPLETE_FILE),
            serde_json::to_vec(&record)?,
        )?;
        match std::fs::rename(&unpacked, &dest) {
            Ok(()) => {}
            // Lost a race with another stager — the staged copy is equivalent.
            Err(_)
                if dest.join("zeron").exists()
                    && std::fs::read(dest.join(STAGE_COMPLETE_FILE))
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<StageRecord>(&bytes).ok())
                        .as_ref()
                        == Some(&record) => {}
            Err(err) => {
                return Err(err).with_context(|| format!("moving {} into place", dest.display()));
            }
        }
        Ok(dest.clone())
    }
    .await;
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// Atomically repoint `app_root/current` at `app_root/<ver>` (symlink to a temp
/// name, then rename over — never a window with no `current`).
pub fn apply_headless(app_root: &Path, version: &str) -> anyhow::Result<()> {
    let version = validate_version(version)?;
    #[cfg(unix)]
    {
        let target = app_root.join(version);
        if !target.join("zeron").exists() {
            bail!("{} is not a staged install", target.display());
        }
        if let Ok(previous) = std::fs::read_link(app_root.join("current")) {
            let backup = app_root.join(format!(".previous-{}", std::process::id()));
            let _ = std::fs::remove_file(&backup);
            std::os::unix::fs::symlink(previous, &backup)?;
            std::fs::rename(backup, app_root.join("previous"))?;
        }
        let tmp = app_root.join(format!(".current-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&target, &tmp).context("creating current symlink")?;
        std::fs::rename(&tmp, app_root.join("current")).context("swapping current symlink")?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (app_root, version);
        require_managed_update_platform()?;
        unreachable!("supported managed-update platforms are Unix")
    }
}

/// Restart the installed engine service (the same units `zeron daemon` and the
/// curl|sh installer manage). Called after a symlink swap so the running daemon
/// picks up the new binary.
pub fn restart_service() -> anyhow::Result<()> {
    require_managed_update_platform()?;
    if cfg!(target_os = "macos") {
        let output = std::process::Command::new("id").arg("-u").output()?;
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        run(
            "launchctl",
            &[
                "kickstart",
                "-k",
                &format!("gui/{uid}/{}", identity::launchd_label()),
            ],
        )
    } else {
        run(
            "systemctl",
            &["--user", "restart", identity::service_name()],
        )
    }
}

// ---------------------------------------------------------------------------
// macOS app-bundle installs — the desktop path
// ---------------------------------------------------------------------------

/// Download + unpack the app tarball into `{data_dir}/updates/<ver>/Zeron.app`
/// (idempotent). Returns the staged bundle path.
pub async fn stage_mac_app(
    edge_url: &str,
    manifest: &Manifest,
    data_dir: &Path,
) -> anyhow::Result<PathBuf> {
    // Reject unsupported targets before creating a stage or making a request.
    require_mac_app_update_platform()?;
    let version = validate_version(&manifest.version)?;
    let updates_dir = data_dir.join("updates");
    let dir = updates_dir.join(version);
    let staged = dir.join(identity::bundle_name());
    let file = mac_app_artifact(version);
    let expected_sha256 = manifest.files.get(&file).and_then(|m| m.sha256.as_deref());

    std::fs::create_dir_all(&updates_dir)
        .with_context(|| format!("creating {}", updates_dir.display()))?;
    // Serialize publication across processes too. Await lock acquisition on a
    // blocking worker so another download never blocks the UI/Tokio executor.
    let lock_path = updates_dir.join(format!(".lock-{version}"));
    let _stage_lock = tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        file.lock()?;
        Ok(file)
    })
    .await??;
    if staged_mac_app_valid(&dir, version, &file, expected_sha256) {
        return Ok(staged);
    }
    if dir.join(STAGE_COMPLETE_FILE).exists() {
        bail!(
            "a completed stage for {version} has different metadata or missing files; refusing to replace it"
        );
    }
    let temp_stage = tempfile::Builder::new()
        .prefix(&format!(".stage-{version}-"))
        .tempdir_in(&updates_dir)
        .with_context(|| format!("creating temporary stage in {}", updates_dir.display()))?;

    let tarball = temp_stage.path().join(&file);
    let actual_sha256 = download_release_file_verified(edge_url, manifest, &file, &tarball).await?;
    run(
        "tar",
        &[
            "-xzf",
            &tarball.to_string_lossy(),
            "-C",
            &temp_stage.path().to_string_lossy(),
        ],
    )?;
    std::fs::remove_file(&tarball).ok();

    let temp_staged = temp_stage.path().join(identity::bundle_name());
    if !temp_staged.join("Contents/MacOS/zeron").is_file()
        || !temp_staged.join("Contents/Info.plist").is_file()
    {
        bail!("app tarball {file} did not contain a complete Zeron.app bundle");
    }

    let record = StageRecord {
        version: version.to_string(),
        artifact: file.clone(),
        sha256: actual_sha256,
    };
    std::fs::write(
        temp_stage.path().join(STAGE_COMPLETE_FILE),
        serde_json::to_vec(&record)?,
    )?;

    if dir.exists() {
        std::fs::remove_dir_all(&dir).context("removing incomplete update stage")?;
    }
    match std::fs::rename(temp_stage.path(), &dir) {
        Ok(()) => {
            let _ = temp_stage.keep();
        }
        Err(err) => {
            if staged_mac_app_valid(&dir, version, &file, expected_sha256) {
                return Ok(staged);
            }
            return Err(err).with_context(|| format!("moving staged bundle to {}", dir.display()));
        }
    }
    Ok(staged)
}

/// Swap the installed bundle for the staged one: `ditto` the staged copy next to
/// the target (metadata-preserving, cross-volume safe), then two renames — the
/// old bundle is restored if the second rename fails.
pub fn apply_mac_app(staged: &Path, bundle: &Path) -> anyhow::Result<()> {
    require_mac_app_update_platform()?;
    if !staged.join("Contents/MacOS/zeron").is_file()
        || !staged.join("Contents/Info.plist").is_file()
    {
        bail!("staged application is incomplete");
    }
    if identity::distributed() {
        run(
            "codesign",
            &["--verify", "--deep", "--strict", &staged.to_string_lossy()],
        )?;
        let identifier = std::process::Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", "Print :CFBundleIdentifier"])
            .arg(staged.join("Contents/Info.plist"))
            .output()?;
        anyhow::ensure!(
            identifier.status.success()
                && String::from_utf8_lossy(&identifier.stdout).trim() == identity::bundle_id(),
            "Staged app has the wrong bundle identity"
        );
        // Ad-hoc signed personal builds do not have a paid Developer ID.
        // Integrity and bundle identity are checked without requiring notarization.
    }
    let parent = bundle
        .parent()
        .context("app bundle has no parent directory")?;
    let name = bundle
        .file_name()
        .context("app bundle has no name")?
        .to_string_lossy();
    let pid = std::process::id();
    let fresh = parent.join(format!(".{name}.new-{pid}"));
    let old = parent.join(format!(".{name}.old-{pid}"));
    let _ = std::fs::remove_dir_all(&fresh);
    run(
        "ditto",
        &[&staged.to_string_lossy(), &fresh.to_string_lossy()],
    )?;
    std::fs::rename(bundle, &old).context("moving the current app aside")?;
    if let Err(err) = std::fs::rename(&fresh, bundle) {
        let _ = std::fs::rename(&old, bundle);
        let _ = std::fs::remove_dir_all(&fresh);
        return Err(err).context("installing the new app bundle");
    }
    // Keep the previous bundle for manual recovery if the new process fails to launch.
    Ok(())
}

/// Detached relauncher: waits for THIS process to exit, then `open`s the bundle.
/// (Opening before exit would race the single-instance engine lock and the IPC
/// port.) The caller quits the app after this returns.
pub fn relaunch_app_after_exit(bundle: &Path) -> anyhow::Result<()> {
    spawn_relauncher(
        "while /bin/kill -0 \"$1\" 2>/dev/null; do sleep 0.2; done; target=$2; shift 2; exec /usr/bin/open \"$target\" --args \"$@\"",
        bundle,
        false,
    )
}

fn relaunch_linux_after_exit(executable: &Path) -> anyhow::Result<()> {
    spawn_relauncher(
        "pid=$1; executable=$2; service=$3; shift 3; while kill -0 \"$pid\" 2>/dev/null; do sleep 0.2; done; if command -v systemctl >/dev/null && systemctl --user is-active --quiet \"$service\"; then systemctl --user restart \"$service\" || exit 1; fi; exec \"$executable\" \"$@\"",
        executable,
        true,
    )
}

fn spawn_relauncher(script: &str, target: &Path, linux: bool) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let mut command = std::process::Command::new("/bin/sh");
        // Paths are positional arguments, never interpolated into shell code.
        command
            .args([
                "-c",
                script,
                "noches-relaunch",
                &std::process::id().to_string(),
            ])
            .arg(target);
        if linux {
            command.arg(identity::service_name());
        }
        command.args(std::env::args_os().skip(1));
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .context("starting update relauncher")?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (script, target, linux);
        bail!("Unix relaunch unavailable")
    }
}

// ---------------------------------------------------------------------------
// Engine-side checker
// ---------------------------------------------------------------------------

/// What the engine reports over the `UpdateStatus` stream. Version facts only —
/// download/apply progress is owned by whoever drives the update (UI or CLI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub current_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default)]
    pub update_available: bool,
    /// Epoch ms of the last successful check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UpdateStatus {
    fn initial() -> Self {
        Self {
            current_version: current_version().to_string(),
            latest_version: None,
            update_available: false,
            checked_at: None,
            error: None,
        }
    }
}

/// `ZERON_AUTO_UPDATE=1|true|yes` — headless daemons apply updates themselves.
fn auto_update_enabled() -> bool {
    std::env::var("ZERON_AUTO_UPDATE")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// "Nothing would be interrupted by a restart right now" — wired by the engine
/// to its live-run and open-terminal registries. `None` = no gate.
pub type QuiescentCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// Background release checker: polls `{edge}/releases` on a 6h cadence and
/// publishes [`UpdateStatus`] over a watch channel (the `UpdateStatus` RPC
/// stream). Managed installs with `ZERON_AUTO_UPDATE` set stage + apply + service
/// restart on their own — but only in a quiet window: while `quiescent` reports
/// activity, the apply defers and re-probes every [`IDLE_RECHECK`].
#[derive(Clone)]
pub struct Updater {
    edge_url: String,
    status_tx: Arc<watch::Sender<UpdateStatus>>,
    check_tx: Arc<watch::Sender<u64>>,
    quiescent: Option<QuiescentCheck>,
    /// Flips to true exactly once; the check loop selects against it so
    /// cancellation lands at any await point (no tokio-util in this crate).
    shutdown_tx: Arc<watch::Sender<bool>>,
    check_task: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl Updater {
    /// Spawn the check loop (must run on a tokio runtime).
    pub fn spawn(edge_url: String, quiescent: Option<QuiescentCheck>) -> Self {
        let (status_tx, _) = watch::channel(UpdateStatus::initial());
        let (check_tx, _) = watch::channel(0);
        let (shutdown_tx, _) = watch::channel(false);
        let updater = Self {
            edge_url,
            status_tx: Arc::new(status_tx),
            check_tx: Arc::new(check_tx),
            quiescent,
            shutdown_tx: Arc::new(shutdown_tx),
            check_task: Arc::new(std::sync::Mutex::new(None)),
        };
        let for_loop = updater.clone();
        let task = tokio::spawn(async move { for_loop.check_loop().await });
        *updater.check_task.lock().unwrap() = Some(task);
        updater
    }

    /// Stop the check loop and wait for it to exit — a replaced runtime must
    /// not keep polling `{edge}/releases` (or auto-applying) in the background.
    /// Idempotent, and callable from any clone.
    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        let task = self
            .check_task
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    pub fn watch(&self) -> watch::Receiver<UpdateStatus> {
        self.status_tx.subscribe()
    }

    /// Wake the release checker immediately, for example when authentication
    /// recovers after the process started offline.
    pub fn check_now(&self) {
        self.check_tx
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    fn quiescent_now(&self) -> bool {
        self.quiescent.as_ref().is_none_or(|check| check())
    }

    async fn check_loop(&self) {
        let mut shutdown = self.shutdown_tx.subscribe();
        // Shutdown must cut the loop at ANY await point — including mid
        // `check_once()` / `auto_apply_when_idle()` HTTP — so the whole body
        // races the flag rather than checking it between iterations.
        tokio::select! {
            _ = shutdown.wait_for(|stop| *stop) => {}
            _ = async {
                let mut checks = self.check_tx.subscribe();
                tokio::select! {
                    _ = tokio::time::sleep(CHECK_INITIAL_DELAY) => {}
                    _ = checks.changed() => {}
                }
                loop {
                    let ok = self.check_once().await;
                    if ok
                        && self.status_tx.borrow().update_available
                        && auto_update_enabled()
                        && let InstallKind::Managed { .. } = detect_install()
                    {
                        self.auto_apply_when_idle().await;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(if ok { CHECK_INTERVAL } else { CHECK_RETRY }) => {}
                        _ = checks.changed() => {}
                    }
                }
            } => {}
        }
    }

    /// Sessions must never die to an update: pre-stage the download now
    /// (harmless while busy), wait for a quiet window (no live runs, no open
    /// terminals), then apply — which re-fetches the manifest (so a long defer
    /// lands on whatever is newest) and reuses the staged dir, keeping the
    /// idle→restart gap to well under a second.
    async fn auto_apply_when_idle(&self) {
        if let InstallKind::Managed { app_root } = detect_install() {
            match fetch_latest(&self.edge_url).await {
                Ok(manifest) if version_newer(&manifest.version, current_version()) => {
                    if let Err(err) = stage_headless(&self.edge_url, &manifest, &app_root).await {
                        tracing::warn!(error = %err, "auto-update staging failed");
                        return;
                    }
                }
                Ok(_) => return,
                Err(err) => {
                    tracing::warn!(error = %err, "auto-update staging fetch failed");
                    return;
                }
            }
        }
        let mut deferred = false;
        while !self.quiescent_now() {
            if !deferred {
                deferred = true;
                tracing::info!("auto-update deferred: sessions or terminals active");
            }
            tokio::time::sleep(IDLE_RECHECK).await;
        }
        match self.apply().await {
            Ok(version) => {
                tracing::info!(%version, "auto-update applied; service restarting")
            }
            Err(err) => tracing::warn!(error = %err, "auto-update failed"),
        }
    }

    /// One check; returns false on fetch failure (retry sooner).
    async fn check_once(&self) -> bool {
        match fetch_latest(&self.edge_url).await {
            Ok(manifest) => {
                let status = UpdateStatus {
                    current_version: current_version().to_string(),
                    update_available: version_newer(&manifest.version, current_version()),
                    latest_version: Some(manifest.version),
                    checked_at: Some(now_ms()),
                    error: None,
                };
                if status.update_available {
                    tracing::info!(
                        latest = status.latest_version.as_deref().unwrap_or(""),
                        current = %status.current_version,
                        "update available"
                    );
                }
                self.status_tx.send_replace(status);
                true
            }
            Err(err) => {
                tracing::debug!(error = %err, "update check failed");
                self.status_tx
                    .send_modify(|s| s.error = Some(format!("{err:#}")));
                false
            }
        }
    }

    /// Stage + apply the newest release on THIS device (managed installs only),
    /// then restart the service after a short delay so the caller's RPC reply
    /// flushes before systemd/launchd kills this process.
    pub async fn apply(&self) -> anyhow::Result<String> {
        anyhow::ensure!(
            self.quiescent_now(),
            "Close active sessions and terminals before updating the engine"
        );
        let InstallKind::Managed { app_root } = detect_install() else {
            bail!(
                "this install is not update-managed — the desktop app updates from its UI; \
                 source builds update via git"
            );
        };
        let manifest = fetch_latest(&self.edge_url).await?;
        if !version_newer(&manifest.version, current_version()) {
            bail!("already up to date ({})", current_version());
        }
        stage_headless(&self.edge_url, &manifest, &app_root).await?;
        apply_headless(&app_root, &manifest.version)?;
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            if let Err(err) = restart_service() {
                tracing::warn!(error = %err, "service restart failed — restart the engine to finish the update");
            }
        });
        Ok(manifest.version)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest() -> Manifest {
        serde_json::from_value(serde_json::json!({
            "product": "noches", "channel": "dev", "version": "0.3.1-dev.1",
            "files": {"noches.tar.gz": {"sha256": "a".repeat(64), "size": 7,
                "url": "https://github.com/owner/noches/releases/download/v0.3.1-dev.1/noches.tar.gz"}}
        })).unwrap()
    }

    #[test]
    fn manifest_rejects_cross_product_channel_and_unverified_payloads() {
        let good = valid_manifest();
        assert!(validate_manifest(&good, "dev").is_ok());
        assert!(validate_manifest(&good, "stable").is_err());
        let mut wrong = good.clone();
        wrong.product = "zeron".into();
        assert!(validate_manifest(&wrong, "dev").is_err());
        let mut wrong = good.clone();
        wrong.files.values_mut().next().unwrap().sha256 = None;
        assert!(validate_manifest(&wrong, "dev").is_err());
        let mut wrong = good.clone();
        wrong.files.values_mut().next().unwrap().url = Some("http://example.com/app".into());
        assert!(validate_manifest(&wrong, "dev").is_err());
        let mut wrong = good;
        wrong.channel = "stable".into();
        assert!(validate_manifest(&wrong, "stable").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn desktop_swap_retains_previous_and_rejects_unstaged_versions() {
        let root = tempfile::tempdir().unwrap();
        for version in ["0.3.1", "0.3.2"] {
            std::fs::create_dir(root.path().join(version)).unwrap();
            std::fs::write(root.path().join(version).join("zeron"), b"fixture").unwrap();
        }
        apply_headless(root.path(), "0.3.1").unwrap();
        apply_headless(root.path(), "0.3.2").unwrap();
        assert_eq!(
            std::fs::read_link(root.path().join("previous")).unwrap(),
            root.path().join("0.3.1")
        );
        assert!(apply_headless(root.path(), "0.3.3").is_err());
        assert_eq!(
            std::fs::read_link(root.path().join("current")).unwrap(),
            root.path().join("0.3.2")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn corrupt_payload_never_replaces_a_download_destination() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            socket.read(&mut buffer).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncorrupt",
                )
                .await
                .unwrap();
        });
        let mut manifest = valid_manifest();
        manifest.files.values_mut().next().unwrap().url = Some(format!("http://{address}/app"));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("download");
        std::fs::write(&dest, b"previous").unwrap();
        let error = download_release_file("", &manifest, "noches.tar.gz", &dest)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
        assert_eq!(std::fs::read(dest).unwrap(), b"previous");
        server.await.unwrap();
    }

    #[test]
    fn release_versions_cannot_escape_stage_directories() {
        for version in [
            "../1", "a/1", "a\\1", "", ".", "current", " 1.2.3", "1.2.3 ", "1.2.3\n",
        ] {
            assert!(validate_version(version).is_err(), "accepted {version:?}");
        }
        assert_eq!(validate_version("0.2.73-beta1").unwrap(), "0.2.73-beta1");
    }

    #[cfg(target_os = "macos")]
    async fn serve_archive(
        bytes: Vec<u8>,
    ) -> (
        String,
        tokio::task::JoinHandle<()>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = requests.clone();
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0; 4096];
                let n = socket.read(&mut request).await.unwrap_or(0);
                if !request[..n].starts_with(b"GET ") {
                    continue;
                }
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&bytes).await;
            }
        });
        (format!("http://{address}"), server, requests)
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn mac_stage_recovers_partial_extraction_and_serializes_publication() {
        let root = tempfile::tempdir().unwrap();
        let version = "0.2.999";
        let dir = root.path().join("updates").join(version);
        let binary = dir
            .join(identity::bundle_name())
            .join("Contents/MacOS/zeron");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, b"partial").unwrap();
        let mut manifest: Manifest =
            serde_json::from_value(serde_json::json!({"version": version})).unwrap();
        let (url, server, _) = serve_archive(b"not an archive".to_vec()).await;
        assert!(stage_mac_app(&url, &manifest, root.path()).await.is_err());
        server.abort();
        assert!(!dir.join(STAGE_COMPLETE_FILE).exists());
        assert_eq!(std::fs::read(&binary).unwrap(), b"partial");

        let source = tempfile::tempdir().unwrap();
        let contents = source.path().join(identity::bundle_name()).join("Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::write(contents.join("MacOS/zeron"), b"complete").unwrap();
        std::fs::write(contents.join("Info.plist"), b"plist fixture").unwrap();
        let archive = source.path().join("app.tgz");
        run(
            "tar",
            &[
                "-czf",
                &archive.to_string_lossy(),
                "-C",
                &source.path().to_string_lossy(),
                &identity::bundle_name(),
            ],
        )
        .unwrap();
        let bytes = std::fs::read(archive).unwrap();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        manifest = serde_json::from_value(serde_json::json!({"version": version,
            "files": { (mac_app_artifact(version)): {"sha256": digest} }}))
        .unwrap();
        let size = bytes.len() as u64;
        let (url, server, requests) = serve_archive(bytes).await;
        let meta = manifest.files.get_mut(&mac_app_artifact(version)).unwrap();
        meta.url = Some(format!("{url}/artifact"));
        meta.size = Some(size);
        let (a, b) = tokio::join!(
            stage_mac_app(&url, &manifest, root.path()),
            stage_mac_app(&url, &manifest, root.path())
        );
        let staged = a.unwrap();
        assert_eq!(staged, b.unwrap());
        assert_eq!(
            std::fs::read(staged.join("Contents/MacOS/zeron")).unwrap(),
            b"complete"
        );
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        server.abort();
        assert_eq!(
            stage_mac_app("http://127.0.0.1:1", &manifest, root.path())
                .await
                .unwrap(),
            staged
        );
        assert!(root.path().join("updates").read_dir().unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".stage-")
        }));
    }

    #[tokio::test]
    async fn stalled_update_headers_and_body_time_out_but_progressing_body_survives() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for stall_body in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                socket.read(&mut request).await.unwrap();
                if stall_body {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx")
                        .await
                        .unwrap();
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            });
            let client =
                http_client_with_timeouts(Duration::from_millis(200), Duration::from_millis(100))
                    .unwrap();
            let request = async {
                client
                    .get(format!("http://{address}"))
                    .send()
                    .await?
                    .bytes()
                    .await
            };
            let error = tokio::time::timeout(Duration::from_secs(1), request)
                .await
                .expect("bounded read")
                .unwrap_err();
            assert!(error.is_timeout());
            server.abort();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n")
                .await
                .unwrap();
            for _ in 0..10 {
                socket.write_all(b"x").await.unwrap();
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });
        let client =
            http_client_with_timeouts(Duration::from_secs(1), Duration::from_millis(200)).unwrap();
        assert_eq!(
            client
                .get(format!("http://{address}"))
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .len(),
            10
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stalled_update_tls_handshake_has_a_connect_deadline() {
        use std::time::Duration;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let client =
            http_client_with_timeouts(Duration::from_millis(100), Duration::from_secs(30)).unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            client.get(format!("https://{address}")).send(),
        )
        .await
        .expect("bounded TLS handshake")
        .unwrap_err();
        assert!(error.is_timeout());
        server.abort();
    }

    #[test]
    fn update_feed_override_requires_an_https_base_url() {
        assert_eq!(
            validate_release_override(" https://example.com/releases/ ").unwrap(),
            "https://example.com/releases"
        );
        for url in [
            "http://example.com/releases",
            "file:///tmp/update",
            "https://user:password@example.com",
            "https://example.com?feed=x",
            "https://example.com/#fragment",
        ] {
            assert!(validate_release_override(url).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn version_compare() {
        assert!(version_newer("0.1.1", "0.1.0"));
        assert!(version_newer("0.2.0", "0.1.9"));
        assert!(version_newer("0.1.10", "0.1.9"));
        assert!(version_newer("v0.1.1", "0.1.0"));
        assert!(!version_newer("0.1.0.1", "0.1.0"));
        assert!(!version_newer("0.1.0", "0.1.0"));
        assert!(!version_newer("0.1.0", "0.1.1"));
        // Garbage never counts as newer.
        assert!(!version_newer("", "0.1.0"));
        assert!(!version_newer("nightly", "0.1.0"));
        // SemVer prerelease ordering is significant.
        assert!(version_newer("0.2.81", "0.2.72-noches.1"));
        assert!(version_newer("0.2.72", "0.2.72-noches.1"));
        assert!(version_newer("0.3.0-dev.10", "0.3.0-dev.9"));
        assert!(!version_newer("0.3.0-dev.9", "0.3.0-dev.10"));
        assert!(!version_newer("0.2.71", "0.2.72-noches.1"));
        assert!(version_newer("0.2.73-beta1", "0.2.72-noches.1"));
        // Build metadata (`+`) is also stripped.
        assert!(version_newer("0.2.81", "0.2.72+build.42"));
        assert!(!version_newer("0.2.72+build.42", "0.2.72"));
    }

    #[test]
    fn install_kind_detection() {
        assert_eq!(
            detect_install_from_for_os(
                Path::new("/home/u/.zeron/app/0.1.1/zeron"),
                Some(Path::new("/home/u")),
                "linux",
            ),
            InstallKind::Managed {
                app_root: PathBuf::from("/home/u/.zeron/app")
            }
        );
        assert_eq!(
            detect_install_from_for_os(
                Path::new("/Applications/Zeron.app/Contents/MacOS/zeron"),
                Some(Path::new("/Users/u")),
                "macos",
            ),
            InstallKind::MacApp {
                bundle: PathBuf::from("/Applications/Zeron.app")
            }
        );
        // A path merely containing `.app` without the bundle layout is not a bundle.
        assert_eq!(
            detect_install_from_for_os(Path::new("/tmp/foo.app/zeron"), None, "macos"),
            InstallKind::Unmanaged
        );
        assert_eq!(
            detect_install_from_for_os(
                Path::new("/src/target/release/zeron"),
                Some(Path::new("/home/u")),
                "linux",
            ),
            InstallKind::Unmanaged
        );
    }

    #[test]
    fn artifact_names_match_packaging() {
        let (os, arch) = platform_key();
        assert!(headless_artifact("0.2.0").starts_with("noches-0.2.0-"));
        assert_eq!(
            headless_artifact("0.2.0"),
            format!("noches-0.2.0-{os}-{arch}.tar.gz")
        );
        assert!(mac_app_artifact("0.2.0").ends_with("-app.tar.gz"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_is_not_misclassified_as_linux() {
        assert_eq!(platform_key().0, "windows");
    }

    #[cfg(windows)]
    #[test]
    fn windows_install_is_always_unmanaged() {
        assert_eq!(
            detect_install_from(
                Path::new(r"C:\Users\u\.zeron\app\0.2.0\zeron.exe"),
                Some(Path::new(r"C:\Users\u")),
            ),
            InstallKind::Unmanaged
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_rejects_platform_specific_updates_before_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = Manifest {
            version: "9.9.9".into(),
            files: BTreeMap::new(),
            ..Default::default()
        };
        let app_root = tmp.path().join("app");
        let data_dir = tmp.path().join("data");

        let managed_err = stage_headless("http://127.0.0.1:1", &manifest, &app_root)
            .await
            .unwrap_err();
        assert!(managed_err.to_string().contains("not supported on windows"));
        assert!(!app_root.exists(), "managed staging must not touch disk");

        let mac_err = stage_mac_app("http://127.0.0.1:1", &manifest, &data_dir)
            .await
            .unwrap_err();
        assert!(mac_err.to_string().contains("not supported on windows"));
        assert!(!data_dir.exists(), "macOS staging must not touch disk");

        assert!(
            apply_headless(&app_root, &manifest.version)
                .unwrap_err()
                .to_string()
                .contains("not supported on windows")
        );
        assert!(
            apply_mac_app(
                &data_dir.join(identity::bundle_name()),
                &data_dir.join("Installed.app")
            )
            .unwrap_err()
            .to_string()
            .contains("not supported on windows")
        );
        assert!(
            restart_service()
                .unwrap_err()
                .to_string()
                .contains("not supported on windows")
        );
    }

    #[test]
    fn manifest_parses_with_and_without_files() {
        let full: Manifest = serde_json::from_str(
            r#"{"version":"0.1.1","files":{"zeron-0.1.1-linux-x86_64.tar.gz":{"sha256":"abc"}}}"#,
        )
        .unwrap();
        assert_eq!(full.version, "0.1.1");
        assert_eq!(
            full.files["zeron-0.1.1-linux-x86_64.tar.gz"]
                .sha256
                .as_deref(),
            Some("abc")
        );
        let bare: Manifest = serde_json::from_str(r#"{"version":"0.1.1"}"#).unwrap();
        assert!(bare.files.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn headless_symlink_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path().join("app");
        for ver in ["0.1.0", "0.1.1"] {
            std::fs::create_dir_all(app_root.join(ver)).unwrap();
            std::fs::write(app_root.join(ver).join("zeron"), ver).unwrap();
        }
        apply_headless(&app_root, "0.1.0").unwrap();
        assert_eq!(
            std::fs::read_link(app_root.join("current")).unwrap(),
            app_root.join("0.1.0")
        );
        // Swap over an existing symlink.
        apply_headless(&app_root, "0.1.1").unwrap();
        assert_eq!(
            std::fs::read_link(app_root.join("current")).unwrap(),
            app_root.join("0.1.1")
        );
        // Unstaged version refuses.
        assert!(apply_headless(&app_root, "0.2.0").is_err());
    }
}
