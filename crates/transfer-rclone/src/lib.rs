//! Managed rclone subprocesses. No shell or persistent rclone configuration.
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{mpsc, watch},
};
pub use transfer_core::TransferEvent as Event;
use transfer_core::{
    Auth, Capabilities, ConnectionProfile, FileEntry, Location, Protocol, StorageBackend,
    SyncAction, SyncChange, SyncMode, SyncPreview, TransferError,
};
mod workflow;
pub use workflow::{preview_workflow, run_workflow};

#[derive(Clone, Debug)]
pub struct CredentialedProfile {
    pub profile: ConnectionProfile,
    pub password: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RcloneBackend {
    profiles: HashMap<String, CredentialedProfile>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AvailableRemote {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub source: String,
}

pub fn default_config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("RCLONE_CONFIG") {
        return PathBuf::from(path);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("rclone/rclone.conf")
}

pub async fn available_remotes(config: &Path) -> Result<Vec<AvailableRemote>, TransferError> {
    let config = std::fs::canonicalize(config)
        .map_err(|e| TransferError::Backend(format!("Cannot open rclone config: {e}")))?;
    let output = base_command(Some(&config))
        .args(["listremotes", "--json"])
        .output()
        .await
        .map_err(|e| TransferError::Backend(e.to_string()))?;
    if !output.status.success() {
        return Err(classify(&output.stderr));
    }
    let remotes: Vec<AvailableRemote> = serde_json::from_slice(&output.stdout)
        .map_err(|e| TransferError::Backend(format!("Cannot read rclone remotes: {e}")))?;
    Ok(remotes
        .into_iter()
        .filter(|r| r.source == "file" && valid_remote_name(&r.name))
        .collect())
}

fn valid_remote_name(name: &str) -> bool {
    !name.is_empty()
        && !name.eq_ignore_ascii_case("NYX_A")
        && !name.eq_ignore_ascii_case("NYX_B")
        && !name.contains([':', '/', '\\', '\n', '\r'])
}

fn config_for<'a>(
    locations: impl IntoIterator<Item = &'a Location>,
    profiles: &HashMap<String, CredentialedProfile>,
) -> Result<Option<PathBuf>, TransferError> {
    let mut config = None;
    for location in locations {
        let Location::Remote { profile_id, .. } = location else {
            continue;
        };
        let profile = &profiles
            .get(profile_id)
            .ok_or(TransferError::MissingProfile)?
            .profile;
        if profile.protocol != Protocol::Rclone {
            continue;
        }
        let path = profile
            .rclone_config
            .as_ref()
            .ok_or(TransferError::MissingProfile)?;
        let path = std::fs::canonicalize(path)
            .map_err(|e| TransferError::Backend(format!("Cannot open rclone config: {e}")))?;
        if config.as_ref().is_some_and(|existing| existing != &path) {
            return Err(TransferError::Backend(
                "Imported remotes use different rclone config files".into(),
            ));
        }
        config = Some(path);
    }
    Ok(config)
}

impl RcloneBackend {
    pub fn new(profiles: HashMap<String, CredentialedProfile>) -> Self {
        Self { profiles }
    }
}

#[async_trait::async_trait]
impl StorageBackend for RcloneBackend {
    fn capabilities(&self, _location: &Location) -> Capabilities {
        Capabilities {
            copy: true,
            rename: false,
            checksum: false,
            resume: false,
        }
    }
    async fn list(&self, location: &Location) -> Result<Vec<FileEntry>, TransferError> {
        list(location, &self.profiles).await
    }
    async fn copy(
        &self,
        source: Location,
        destination: Location,
        events: mpsc::UnboundedSender<Event>,
        cancel: watch::Receiver<bool>,
    ) {
        copy(source, destination, self.profiles.clone(), events, cancel).await
    }
}

#[derive(Deserialize)]
struct JsonEntry {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "IsDir")]
    is_dir: bool,
    #[serde(rename = "Size")]
    size: Option<u64>,
    #[serde(rename = "ModTime")]
    modified: Option<String>,
    #[serde(rename = "MimeType")]
    mime: Option<String>,
}

pub async fn list(
    location: &Location,
    profiles: &HashMap<String, CredentialedProfile>,
) -> Result<Vec<FileEntry>, TransferError> {
    let config = config_for([location], profiles)?;
    let mut cmd = base_command(config.as_deref());
    let path = configure(&mut cmd, location, profiles, "NYX_A").await?;
    cmd.arg("lsjson")
        .args(["--retries", "1", "--low-level-retries", "1"])
        .arg("--")
        .arg(path);
    let output = cmd
        .output()
        .await
        .map_err(|e| TransferError::Backend(e.to_string()))?;
    if !output.status.success() {
        return Err(classify(&output.stderr));
    }
    let mut entries: Vec<FileEntry> = serde_json::from_slice::<Vec<JsonEntry>>(&output.stdout)
        .map_err(|e| TransferError::Backend(format!("Cannot read directory listing: {e}")))?
        .into_iter()
        .map(|e| FileEntry {
            name: e.name,
            is_dir: e.is_dir,
            size: e.size,
            modified: e.modified,
            mime: e.mime,
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

pub async fn copy(
    source: Location,
    destination: Location,
    profiles: HashMap<String, CredentialedProfile>,
    events: mpsc::UnboundedSender<Event>,
    mut cancel: watch::Receiver<bool>,
) {
    let result = copy_inner(&source, &destination, &profiles, &events, &mut cancel).await;
    let _ = events.send(Event::Finished(result));
}

async fn copy_inner(
    source: &Location,
    destination: &Location,
    profiles: &HashMap<String, CredentialedProfile>,
    events: &mpsc::UnboundedSender<Event>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), TransferError> {
    if *cancel.borrow() {
        return Err(TransferError::Cancelled);
    }
    let config = config_for([source, destination], profiles)?;
    let mut cmd = base_command(config.as_deref());
    let src = configure(&mut cmd, source, profiles, "NYX_A").await?;
    let dst = configure(&mut cmd, destination, profiles, "NYX_B").await?;
    cmd.arg("copyto").args([
        "--immutable",
        "--stats",
        "1s",
        "--stats-one-line",
        "--use-json-log",
        "--stats-log-level",
        "NOTICE",
        "--retries",
        "1",
        "--low-level-retries",
        "1",
    ]);
    cmd.arg("--").arg(src).arg(dst);
    run_command(cmd, events, cancel).await
}

pub async fn preview_sync(
    source: &Location,
    destination: &Location,
    mode: SyncMode,
    profiles: &HashMap<String, CredentialedProfile>,
) -> Result<SyncPreview, TransferError> {
    preview_sync_with_options(source, destination, mode, profiles, None, false).await
}

async fn preview_sync_with_options(
    source: &Location,
    destination: &Location,
    mode: SyncMode,
    profiles: &HashMap<String, CredentialedProfile>,
    filter_file: Option<&Path>,
    checksum: bool,
) -> Result<SyncPreview, TransferError> {
    if mode == SyncMode::Bidirectional {
        return Err(TransferError::Backend(
            "Use a saved workflow for bidirectional sync".into(),
        ));
    }
    let config = config_for([source, destination], profiles)?;
    let mut cmd = base_command(config.as_deref());
    let src = configure(&mut cmd, source, profiles, "NYX_A").await?;
    let dst = configure(&mut cmd, destination, profiles, "NYX_B").await?;
    cmd.arg(match mode {
        SyncMode::Copy => "copy",
        SyncMode::Mirror => "sync",
        SyncMode::Bidirectional => unreachable!(),
    });
    if let Some(filter) = filter_file {
        cmd.arg("--filter-from").arg(filter);
    }
    if checksum {
        cmd.arg("--checksum");
    }
    cmd.args([
        "--dry-run",
        "--combined",
        "-",
        "--retries",
        "1",
        "--low-level-retries",
        "1",
    ])
    .arg("--")
    .arg(src)
    .arg(dst)
    .kill_on_drop(true);
    let output = cmd
        .output()
        .await
        .map_err(|e| TransferError::Backend(e.to_string()))?;
    if !output.status.success() {
        return Err(classify(&output.stderr));
    }
    let report = String::from_utf8(output.stdout)
        .map_err(|e| TransferError::Backend(format!("Invalid comparison report: {e}")))?;
    let mut changes = Vec::new();
    for line in report.lines() {
        let (action, path) = line
            .split_at_checked(2)
            .ok_or_else(|| TransferError::Backend("Invalid comparison report".into()))?;
        let action = match action {
            "+ " => Some(SyncAction::Copy),
            "* " => Some(SyncAction::Update),
            "- " if mode == SyncMode::Mirror => Some(SyncAction::Delete),
            "- " | "= " => None,
            "! " => {
                return Err(TransferError::Backend(format!(
                    "Comparison failed for {path}"
                )));
            }
            _ => return Err(TransferError::Backend("Unknown comparison result".into())),
        };
        if let Some(action) = action {
            changes.push(SyncChange {
                action,
                path: path.to_owned(),
            });
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.action.cmp(&b.action)));
    let bytes = serde_json::to_vec(&changes).map_err(|e| TransferError::Backend(e.to_string()))?;
    let fingerprint = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(SyncPreview {
        changes,
        fingerprint,
    })
}

pub async fn sync(
    source: Location,
    destination: Location,
    mode: SyncMode,
    expected_fingerprint: String,
    profiles: HashMap<String, CredentialedProfile>,
    events: mpsc::UnboundedSender<Event>,
    mut cancel: watch::Receiver<bool>,
) {
    let result = sync_inner(
        &source,
        &destination,
        mode,
        &expected_fingerprint,
        &profiles,
        &events,
        &mut cancel,
    )
    .await;
    let _ = events.send(Event::Finished(result));
}

async fn sync_inner(
    source: &Location,
    destination: &Location,
    mode: SyncMode,
    expected_fingerprint: &str,
    profiles: &HashMap<String, CredentialedProfile>,
    events: &mpsc::UnboundedSender<Event>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), TransferError> {
    if *cancel.borrow() {
        return Err(TransferError::Cancelled);
    }
    let preview = tokio::select! {
        result = preview_sync(source, destination, mode, profiles) => result?,
        changed = cancel.changed() => {
            if changed.is_ok() && *cancel.borrow() { return Err(TransferError::Cancelled); }
            preview_sync(source, destination, mode, profiles).await?
        },
    };
    if preview.fingerprint != expected_fingerprint {
        return Err(TransferError::Conflict(
            "Directory contents changed since preview; preview again".into(),
        ));
    }
    let config = config_for([source, destination], profiles)?;
    let mut cmd = base_command(config.as_deref());
    let src = configure(&mut cmd, source, profiles, "NYX_A").await?;
    let dst = configure(&mut cmd, destination, profiles, "NYX_B").await?;
    cmd.arg(match mode {
        SyncMode::Copy => "copy",
        SyncMode::Mirror => "sync",
        SyncMode::Bidirectional => {
            return Err(TransferError::Backend(
                "Use a saved workflow for bidirectional sync".into(),
            ));
        }
    })
    .args([
        "--stats",
        "1s",
        "--stats-one-line",
        "--use-json-log",
        "--stats-log-level",
        "NOTICE",
        "--retries",
        "1",
        "--low-level-retries",
        "1",
    ]);
    if mode == SyncMode::Mirror {
        let deletes = preview
            .changes
            .iter()
            .filter(|change| change.action == SyncAction::Delete)
            .count();
        cmd.arg("--max-delete").arg(deletes.to_string());
    }
    cmd.arg("--").arg(src).arg(dst);
    run_command(cmd, events, cancel).await
}

async fn run_command(
    mut cmd: Command,
    events: &mpsc::UnboundedSender<Event>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), TransferError> {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| TransferError::Backend(e.to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| TransferError::Backend("rclone progress unavailable".into()))?;
    let mut lines = BufReader::new(stderr).lines();
    let mut last_error = String::new();
    loop {
        tokio::select! {
            changed = cancel.changed() => {
                if changed.is_ok() && *cancel.borrow() { child.kill().await.ok(); return Err(TransferError::Cancelled); }
            }
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    let value = serde_json::from_str::<serde_json::Value>(&line).ok();
                    if let Some(stats) = value.as_ref().and_then(|v| v.get("stats")) {
                        let bytes = stats.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                        let total_bytes = stats.get("totalBytes").and_then(|v| v.as_u64());
                        let speed_bytes_per_sec = stats.get("speed").and_then(|v| v.as_f64());
                        let eta_seconds = stats.get("eta").and_then(|v| v.as_f64()).map(|n| n.max(0.0).ceil() as u64);
                        let _ = events.send(Event::Metrics { bytes, total_bytes, speed_bytes_per_sec, eta_seconds });
                    }
                    let msg = value.as_ref().and_then(|v| v.get("msg").and_then(|m| m.as_str()))
                        .map(str::to_owned).unwrap_or(line);
                    if msg.contains("Transferred:") || msg.contains('%') { let _ = events.send(Event::Progress(msg.clone())); }
                    if msg.to_lowercase().contains("error") { last_error = msg; }
                }
                Ok(None) => break,
                Err(e) => return Err(TransferError::Backend(e.to_string())),
            }
        }
    }
    let status = child
        .wait()
        .await
        .map_err(|e| TransferError::Backend(e.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(classify(last_error.as_bytes()))
    }
}

fn base_command(config: Option<&Path>) -> Command {
    let mut cmd = Command::new("rclone");
    // Inherited RCLONE_* settings could bypass TLS or host-key checks.
    cmd.env_clear();
    for name in [
        "HOME",
        "PATH",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "XDG_CACHE_HOME",
        "XDG_RUNTIME_DIR",
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
    ] {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    cmd.arg("--config")
        .arg(config.unwrap_or(Path::new("/dev/null")));
    cmd
}

async fn configure(
    cmd: &mut Command,
    location: &Location,
    profiles: &HashMap<String, CredentialedProfile>,
    name: &str,
) -> Result<String, TransferError> {
    match location {
        Location::Local(path) => path
            .to_str()
            .map(str::to_owned)
            .ok_or(TransferError::InvalidPath),
        Location::Remote { profile_id, path } => {
            if path.starts_with('/') || path.split('/').any(|p| p == "..") {
                return Err(TransferError::InvalidPath);
            }
            let item = profiles
                .get(profile_id)
                .ok_or(TransferError::MissingProfile)?;
            let p = &item.profile;
            if p.protocol == Protocol::Rclone {
                let remote = p
                    .rclone_remote
                    .as_deref()
                    .ok_or(TransferError::MissingProfile)?;
                if !valid_remote_name(remote) || p.rclone_config.is_none() {
                    return Err(TransferError::MissingProfile);
                }
                return Ok(format!("{remote}:{path}"));
            }
            if p.host.contains('@') || p.host.contains('?') || p.host.contains('#') {
                return Err(TransferError::Backend(
                    "Connection host contains embedded credentials or parameters".into(),
                ));
            }
            let prefix = format!("RCLONE_CONFIG_{name}_");
            cmd.arg("--contimeout").arg(format!("{}s", p.timeout_secs));
            cmd.arg("--timeout").arg(format!("{}s", p.timeout_secs));
            let kind = match p.protocol {
                Protocol::Sftp => "sftp",
                Protocol::Ftp | Protocol::FtpsExplicit | Protocol::FtpsImplicit => "ftp",
                Protocol::Webdav => "webdav",
                Protocol::Rclone => unreachable!(),
            };
            cmd.env(format!("{prefix}TYPE"), kind);
            if p.protocol != Protocol::Webdav {
                cmd.env(format!("{prefix}HOST"), &p.host);
                cmd.env(format!("{prefix}PORT"), p.port.to_string());
            }
            cmd.env(format!("{prefix}USER"), &p.username);
            if let Some(ca) = &p.ca_cert {
                cmd.arg("--ca-cert").arg(ca);
            }
            match p.protocol {
                Protocol::Sftp => {
                    let known = p.known_hosts_file.as_ref().ok_or_else(|| {
                        TransferError::HostKey("Choose a known_hosts file".into())
                    })?;
                    if !known.is_file() {
                        return Err(TransferError::HostKey("Known-hosts file is missing".into()));
                    }
                    cmd.env(format!("{prefix}KNOWN_HOSTS_FILE"), known);
                    match p.auth {
                        Auth::Agent => {
                            cmd.env(format!("{prefix}KEY_USE_AGENT"), "true");
                        }
                        Auth::KeyFile => {
                            let key = p.ssh_key_file.as_ref().ok_or_else(|| {
                                TransferError::Authentication("Choose an SSH key".into())
                            })?;
                            cmd.env(format!("{prefix}KEY_FILE"), key);
                        }
                        Auth::Password => {}
                    }
                }
                Protocol::Ftp => {
                    if !p.allow_plaintext {
                        return Err(TransferError::Backend(
                            "Plain FTP must be explicitly enabled".into(),
                        ));
                    }
                }
                Protocol::FtpsExplicit => {
                    cmd.env(format!("{prefix}EXPLICIT_TLS"), "true");
                }
                Protocol::FtpsImplicit => {
                    cmd.env(format!("{prefix}TLS"), "true");
                }
                Protocol::Webdav => {
                    if !p.host.starts_with("https://")
                        && !(p.allow_plaintext && p.host.starts_with("http://"))
                    {
                        return Err(TransferError::Backend(
                            "WebDAV needs HTTPS or explicit HTTP permission".into(),
                        ));
                    }
                    cmd.env(format!("{prefix}URL"), &p.host);
                    cmd.env(format!("{prefix}VENDOR"), "other");
                }
                Protocol::Rclone => unreachable!(),
            }
            if let Some(secret) = &item.password {
                cmd.env(format!("{prefix}PASS"), obscure(secret).await?);
            }
            Ok(format!("{name}:{path}"))
        }
    }
}

async fn obscure(secret: &str) -> Result<String, TransferError> {
    if secret.is_empty() || secret.contains(['\n', '\r']) {
        return Err(TransferError::Authentication(
            "Invalid saved password".into(),
        ));
    }
    let mut child = Command::new("rclone")
        .args(["obscure", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| TransferError::Backend(e.to_string()))?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| TransferError::Backend("Cannot supply credential".into()))?;
    input
        .write_all(secret.as_bytes())
        .await
        .map_err(|e| TransferError::Backend(e.to_string()))?;
    input
        .write_all(b"\n")
        .await
        .map_err(|e| TransferError::Backend(e.to_string()))?;
    drop(input);
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| TransferError::Backend(e.to_string()))?;
    if !output.status.success() {
        return Err(TransferError::Backend("Cannot prepare credential".into()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn classify(stderr: &[u8]) -> TransferError {
    let text = String::from_utf8_lossy(stderr);
    let lower = text.to_lowercase();
    let safe = text
        .lines()
        .last()
        .unwrap_or("rclone failed")
        .chars()
        .take(240)
        .collect::<String>();
    if lower.contains("host key") || lower.contains("knownhosts") {
        TransferError::HostKey(safe)
    } else if lower.contains("certificate") || lower.contains("x509") {
        TransferError::Certificate(safe)
    } else if lower.contains("permission denied") {
        TransferError::Permission(safe)
    } else if lower.contains("authentication")
        || lower.contains("login")
        || lower.contains("password")
    {
        TransferError::Authentication("Check the saved credentials".into())
    } else if lower.contains("connection") || lower.contains("timeout") || lower.contains("network")
    {
        TransferError::Network(safe)
    } else {
        TransferError::Backend(safe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    async fn copy_checked(
        source: Location,
        destination: Location,
        profiles: HashMap<String, CredentialedProfile>,
    ) {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let (_, cancel) = watch::channel(false);
        copy(source, destination, profiles, sender, cancel).await;
        while let Some(event) = receiver.recv().await {
            if let Event::Finished(result) = event {
                result.unwrap();
                return;
            }
        }
        panic!("transfer did not finish");
    }
    #[tokio::test]
    async fn local_listing_and_copy() {
        let root = std::env::temp_dir().join(format!("nyx-transfer-test-{}", std::process::id()));
        let source = root.join("source");
        let target = root.join("target");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("snow ☃.txt"), b"transfer works").unwrap();
        let entries = list(&Location::Local(source.clone()), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(entries[0].name, "snow ☃.txt");
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let (_, cancel) = watch::channel(false);
        copy(
            Location::Local(source.join("snow ☃.txt")),
            Location::Local(target.join("snow ☃.txt")),
            HashMap::new(),
            sender,
            cancel,
        )
        .await;
        let mut result = None;
        let mut metrics = false;
        while let Some(event) = receiver.recv().await {
            match event {
                Event::Metrics { bytes, .. } if bytes > 0 => metrics = true,
                Event::Finished(done) => {
                    result = Some(done);
                    break;
                }
                _ => {}
            }
        }
        result.unwrap().unwrap();
        assert!(metrics, "rclone did not report transfer metrics");
        assert_eq!(
            std::fs::read(target.join("snow ☃.txt")).unwrap(),
            b"transfer works"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cancelled_before_start_does_not_write_destination() {
        let root =
            std::env::temp_dir().join(format!("nyx-transfer-cancel-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source");
        let destination = root.join("destination");
        std::fs::write(&source, b"hello").unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let (cancel_sender, cancel) = watch::channel(false);
        cancel_sender.send(true).unwrap();
        copy(
            Location::Local(source),
            Location::Local(destination.clone()),
            HashMap::new(),
            sender,
            cancel,
        )
        .await;
        assert!(matches!(
            receiver.recv().await,
            Some(Event::Finished(Err(TransferError::Cancelled)))
        ));
        assert!(!destination.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn imported_remotes_copy_between_locations() {
        let root =
            std::env::temp_dir().join(format!("nyx-transfer-import-test-{}", std::process::id()));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("cloud ☃.txt"), b"from configured remote").unwrap();
        let config = root.join("rclone.conf");
        std::fs::write(
            &config,
            format!(
                "[first]\ntype = alias\nremote = {}\n[second]\ntype = alias\nremote = {}\n",
                first.display(),
                second.display()
            ),
        )
        .unwrap();
        let remotes = available_remotes(&config).await.unwrap();
        assert_eq!(remotes.len(), 2);
        assert!(remotes.iter().all(|r| r.source == "file"));
        let profiles: HashMap<String, CredentialedProfile> = ["first", "second"]
            .into_iter()
            .map(|remote| {
                let profile = ConnectionProfile {
                    id: remote.into(),
                    name: remote.into(),
                    protocol: Protocol::Rclone,
                    host: String::new(),
                    port: 0,
                    username: String::new(),
                    auth: Auth::Agent,
                    default_dir: String::new(),
                    timeout_secs: 30,
                    ssh_key_file: None,
                    known_hosts_file: None,
                    ca_cert: None,
                    allow_plaintext: false,
                    rclone_remote: Some(remote.into()),
                    rclone_config: Some(config.clone()),
                };
                (
                    remote.into(),
                    CredentialedProfile {
                        profile,
                        password: None,
                    },
                )
            })
            .collect();
        let entries = list(
            &Location::Remote {
                profile_id: "first".into(),
                path: String::new(),
            },
            &profiles,
        )
        .await
        .unwrap();
        assert_eq!(entries[0].name, "cloud ☃.txt");
        copy_checked(
            Location::Remote {
                profile_id: "first".into(),
                path: "cloud ☃.txt".into(),
            },
            Location::Remote {
                profile_id: "second".into(),
                path: "cloud ☃.txt".into(),
            },
            profiles,
        )
        .await;
        assert_eq!(
            std::fs::read(second.join("cloud ☃.txt")).unwrap(),
            b"from configured remote"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn sync_preview_blocks_stale_mirror_and_applies_reviewed_changes() {
        let root =
            std::env::temp_dir().join(format!("nyx-transfer-sync-test-{}", std::process::id()));
        let source = root.join("source");
        let destination_path = root.join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination_path).unwrap();
        std::fs::write(source.join("new ☃.txt"), b"new").unwrap();
        std::fs::write(destination_path.join("old file.txt"), b"old").unwrap();
        let source = Location::Local(source);
        let destination = Location::Local(destination_path.clone());
        let profiles = HashMap::new();
        let preview = preview_sync(&source, &destination, SyncMode::Mirror, &profiles)
            .await
            .unwrap();
        assert!(
            preview
                .changes
                .iter()
                .any(|c| c.action == SyncAction::Delete && c.path == "old file.txt")
        );
        assert!(
            preview
                .changes
                .iter()
                .any(|c| c.action == SyncAction::Copy && c.path == "new ☃.txt")
        );
        std::fs::write(destination_path.join("surprise.txt"), b"surprise").unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let (_, cancel) = watch::channel(false);
        sync(
            source.clone(),
            destination.clone(),
            SyncMode::Mirror,
            preview.fingerprint,
            profiles.clone(),
            sender,
            cancel,
        )
        .await;
        let mut finished = None;
        while let Some(event) = receiver.recv().await {
            if let Event::Finished(result) = event {
                finished = Some(result);
                break;
            }
        }
        assert!(
            matches!(finished, Some(Err(TransferError::Conflict(_)))),
            "{finished:?}"
        );
        assert!(destination_path.join("old file.txt").exists());
        let preview = preview_sync(&source, &destination, SyncMode::Mirror, &profiles)
            .await
            .unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let (_, cancel) = watch::channel(false);
        sync(
            source,
            destination,
            SyncMode::Mirror,
            preview.fingerprint,
            profiles,
            sender,
            cancel,
        )
        .await;
        let mut finished = None;
        while let Some(event) = receiver.recv().await {
            if let Event::Finished(result) = event {
                finished = Some(result);
                break;
            }
        }
        finished.unwrap().unwrap();
        assert!(destination_path.join("new ☃.txt").exists());
        assert!(!destination_path.join("old file.txt").exists());
        assert!(!destination_path.join("surprise.txt").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn disposable_ftp_and_webdav_listings() {
        let root =
            std::env::temp_dir().join(format!("nyx-transfer-remote-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("remote ☃.txt"), b"hello").unwrap();
        for (protocol, serve, extra) in [
            (
                Protocol::Ftp,
                "ftp",
                vec!["--user", "tester", "--pass", "test-password"],
            ),
            (
                Protocol::Webdav,
                "webdav",
                vec!["--user", "tester", "--pass", "test-password"],
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            let address = format!("127.0.0.1:{port}");
            let mut server = std::process::Command::new("rclone")
                .args(["serve", serve, "--addr", &address])
                .args(extra)
                .arg(&root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let profile = ConnectionProfile {
                id: "remote".into(),
                name: "Test".into(),
                protocol: protocol.clone(),
                host: if protocol == Protocol::Webdav {
                    format!("http://{address}")
                } else {
                    "127.0.0.1".into()
                },
                port,
                username: "tester".into(),
                auth: Auth::Password,
                default_dir: String::new(),
                timeout_secs: 5,
                ssh_key_file: None,
                known_hosts_file: None,
                ca_cert: None,
                allow_plaintext: true,
                rclone_remote: None,
                rclone_config: None,
            };
            let profiles = HashMap::from([(
                "remote".into(),
                CredentialedProfile {
                    profile,
                    password: Some("test-password".into()),
                },
            )]);
            let result = list(
                &Location::Remote {
                    profile_id: "remote".into(),
                    path: String::new(),
                },
                &profiles,
            )
            .await;
            if result.is_ok() {
                copy_checked(
                    Location::Local(root.join("remote ☃.txt")),
                    Location::Remote {
                        profile_id: "remote".into(),
                        path: "uploaded.txt".into(),
                    },
                    profiles.clone(),
                )
                .await;
                copy_checked(
                    Location::Remote {
                        profile_id: "remote".into(),
                        path: "uploaded.txt".into(),
                    },
                    Location::Local(root.join("downloaded.txt")),
                    profiles,
                )
                .await;
                assert_eq!(
                    std::fs::read(root.join("downloaded.txt")).unwrap(),
                    b"hello"
                );
            }
            server.kill().ok();
            server.wait().ok();
            let entries = result.unwrap();
            assert!(entries.iter().any(|e| e.name == "remote ☃.txt"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn disposable_sftp_requires_known_host() {
        let root =
            std::env::temp_dir().join(format!("nyx-transfer-sftp-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("remote file.txt"), b"hello").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let address = format!("127.0.0.1:{port}");
        let mut server = std::process::Command::new("rclone")
            .args([
                "serve",
                "sftp",
                "--addr",
                &address,
                "--user",
                "tester",
                "--pass",
                "test-password",
            ])
            .arg(&root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let scanned = std::process::Command::new("ssh-keyscan")
            .args(["-T", "5", "-p", &port.to_string(), "127.0.0.1"])
            .output()
            .unwrap();
        let known = root.join("known_hosts");
        std::fs::write(&known, scanned.stdout).unwrap();
        let profile = ConnectionProfile {
            id: "sftp".into(),
            name: "Test".into(),
            protocol: Protocol::Sftp,
            host: "127.0.0.1".into(),
            port,
            username: "tester".into(),
            auth: Auth::Password,
            default_dir: String::new(),
            timeout_secs: 5,
            ssh_key_file: None,
            known_hosts_file: Some(known),
            ca_cert: None,
            allow_plaintext: false,
            rclone_remote: None,
            rclone_config: None,
        };
        let profiles = HashMap::from([(
            "sftp".into(),
            CredentialedProfile {
                profile,
                password: Some("test-password".into()),
            },
        )]);
        let result = list(
            &Location::Remote {
                profile_id: "sftp".into(),
                path: String::new(),
            },
            &profiles,
        )
        .await;
        if result.is_ok() {
            copy_checked(
                Location::Local(root.join("remote file.txt")),
                Location::Remote {
                    profile_id: "sftp".into(),
                    path: "uploaded.txt".into(),
                },
                profiles.clone(),
            )
            .await;
            copy_checked(
                Location::Remote {
                    profile_id: "sftp".into(),
                    path: "uploaded.txt".into(),
                },
                Location::Local(root.join("downloaded.txt")),
                profiles,
            )
            .await;
            assert_eq!(
                std::fs::read(root.join("downloaded.txt")).unwrap(),
                b"hello"
            );
        }
        server.kill().ok();
        server.wait().ok();
        let entries = result.unwrap();
        assert!(entries.iter().any(|e| e.name == "remote file.txt"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
