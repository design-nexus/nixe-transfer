# Backends

| Kind | First release | Security and capability boundary |
| --- | --- | --- |
| Local | rclone local path | Never infer POSIX behavior for other backends. |
| SFTP | rclone SFTP | Require verified host keys; agent, key-file, or Secret Service password. |
| FTP | rclone FTP | Plaintext compatibility choice with a visible warning. |
| FTPS | rclone FTP with explicit or implicit TLS | Certificate validation stays enabled; no silent fallback. |
| WebDAV | rclone WebDAV | HTTPS is the default; HTTP requires explicit acknowledgement. |
| Imported rclone remote | Existing user rclone config | Import stores only remote name and config path. Authentication and provider security settings remain managed by rclone. |

Rclone provides listing, stat, copy, and progress. Capability checks decide
whether rename, hashes, or server-side operations can be used; same-provider
behavior is never assumed across unrelated locations. SMB, NFS, and cloud
providers follow in later releases. Mounted SMB/NFS directories are available
through the local pane. The Import rclone remote action lists file-backed
remotes from the user's rclone config; it does not copy credentials into the
Transfer database. Imported remotes can be used in either pane and in the
same persistent queue as local and manually configured connections. rclone
may refresh provider tokens in its config during a transfer. Two imported
remotes must use the same config file for a direct transfer. Additional native
adapters require a demonstrated rclone capability gap.
