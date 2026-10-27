# Transfer

Transfer is Nyx's standalone local and remote file-transfer application. The
first release browses local, SFTP, FTP, FTPS, and WebDAV locations and copies
files or directories between any two of them. It uses rclone as a subprocess;
network protocols are not implemented in this repository.
Existing rclone remotes can be imported for cloud and other providers. Import
stores the remote name and config path; rclone continues to manage its own
credentials and may refresh provider tokens in that file.

Build with `cargo build --release --locked --workspace` and install with `./install.sh`.
The application is named **Transfer**, its binary is `nyx-transfer`, and its
application ID is `io.github.design_nexus.nyx.Transfer`.

Transfers run in `nyx-transferd` and continue when the window closes. Jobs and
history are stored in SQLite. Queued jobs restart with the service; an active
job interrupted by service shutdown is marked failed for an explicit safe retry.
Transfers protect existing targets from replacement. SFTP requires a known SSH host key; use the connection
dialog to inspect and trust a new fingerprint. FTP and HTTP WebDAV require an
explicit plaintext choice. FTPS keeps certificate validation enabled and accepts
an optional custom CA certificate. The roadmap describes synchronization and
cross-application workflows.
The Sync folders dialog previews one-way copy or mirror changes between the
current panes. Workflows save locations, mode, include/exclude patterns,
conflict handling, checksum comparison, retry count, and an optional bandwidth limit.
Bidirectional workflows use rclone bisync's prior listings to track changes
and preserve conflicting versions by default. The first run requires a chosen
resync preference and confirmation. All workflow jobs run in the background
and stop when the previewed directory state changes before execution.

The browser has independent connection tabs, back/forward history, breadcrumbs,
sorting, filtering, hidden-file controls, context actions, and drag-and-drop
copying between panes or from local file managers. The collapsible queue shows
progress, speed, ETA, retries, and errors. The disposable integration checks
cover local, SFTP, FTP, and WebDAV uploads and downloads. FTPS uses rclone's
explicit or implicit TLS modes, but a compatible disposable FTPS server was
not available for an end-to-end check.

Targeted service check: `cargo build --locked -p nyx-transferd` followed by
`dbus-run-session -- python3 tests/service.py`. It uses temporary data and a
private bus, with no personal accounts.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/design-nexus/nyx-transfer/main/install.sh | bash
```

