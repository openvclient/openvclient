# Changelog

All notable changes to OpenVClient are listed here. The project follows
[Semantic Versioning](https://semver.org); pre-releases are tagged
`vX.Y.Z-beta.N` and published as GitHub pre-releases.

## [1.0.0-beta.5] — 2026-11-07

Notes: [docs/release-notes/v1.0.0-beta.5.md](docs/release-notes/v1.0.0-beta.5.md).

### Added
- ER diagram and schema compare (PostgreSQL, MySQL/MariaDB, SQLite, SQL Server).
- Charts from query results; SQL formatter (⌘⇧F); snippets; manual commit mode.
- Session restore offer on Home; recent queries; Home grouped by folder.
- Frozen grid columns; filter or exclude by a cell's value; recent WHERE filters.
- Image and PDF previews, per-file transfer progress and folder item counts in the file manager.
- Tab menu (close others / to the right / all, reopen closed), ⌘⇧T, ⌘1–9, ⌘B.
- Connection URL paste; export and import of connections; interface size, query timeout, Staging confirmation, NULL label and timestamp settings.
- SSH lab (`scripts/ssh-lab.sh`) with 45 checks against real OpenSSH servers.

### Fixed
- SSH: older servers (hmac-sha1, group14-sha1, CBC), key-and-password logins, passphrase errors, symlinks, folder delete, streamed transfers.
- Wide results scroll sideways instead of cutting off columns.
- Toasts, truncated names, file list alignment, tab titles and the plan diagram (see the notes).

## [1.0.0-beta.4] — 2026-11-06

Notes: [docs/release-notes/v1.0.0-beta.4.md](docs/release-notes/v1.0.0-beta.4.md).

### Added
- Cartoon style (Settings → Appearance → Style), with connector stickers and cartoon icons for folders, containers, databases, tables, files and tabs; Mode (System/Dark/Light) is a separate setting.
- Favourite connections, pinned at the top of Home.
- Keyboard shortcuts window (⌘/, Settings → Help, About).
- GaussDB / openGauss support end to end.

### Fixed
- SSH "Password" login on PAM servers (Ubuntu, Linux Mint): falls back to keyboard-interactive.
- Connected storage and service connections open their own view instead of the Docker view.
- Long sidebar error messages no longer widen the panel.
- The workspace encryption key is never replaced when the keychain can't be read (a locked keychain or a denied prompt could make saved history unreadable).

### Changed
- The project moved to https://github.com/openvclient/openvclient.

## [1.0.0-beta.3] — 2026-11-06

Notes: [docs/release-notes/v1.0.0-beta.3.md](docs/release-notes/v1.0.0-beta.3.md).

### Added
- File manager for Azure Blob, S3 and S3-compatible storage, FTP/FTPS and SFTP: breadcrumb navigation with back/forward/up, sortable file list, multi-select, upload (button and drag and drop), new folder, download, share link, rename, delete, right-click menu and preview panel.
- About panel with version, Check for updates and project links; version shown in Settings.
- "Vee" logo and brand assets; new app icon and installer artwork.
- Readable labels in all dropdowns.

### Fixed
- Storage connections no longer show "localhost".
- Connector tiles are fully clickable.

## [1.0.0-beta.2] — 2026-11-06

Notes: [docs/release-notes/v1.0.0-beta.2.md](docs/release-notes/v1.0.0-beta.2.md).

### Fixed
- System clipboard: copy, cut and paste in all text fields, and the in-app Copy buttons, on macOS, Windows and Linux (eframe was built without its clipboard feature).
- Hyperlinks open in the browser.
- Update check: SemVer ordering for pre-releases and newer betas offered to beta users; stable users still see stable releases only.
- Linux: .rpm files named with the release version so SHA256SUMS matches the downloads.

### Build
- Release workflow: macOS DMG step pins Python 3.12; later betas are titled "Beta vX.Y.Z (beta N)".

## [1.0.0-beta.1] — 2026-11-06

First public beta. Full notes: [docs/release-notes/v1.0.0-beta.1.md](docs/release-notes/v1.0.0-beta.1.md).

### Added
- Connectors: PostgreSQL, MySQL, MariaDB, SQL Server / Azure SQL (SQL, Windows, integrated and Entra ID sign-in), SQLite, ClickHouse, Kingbase and GaussDB presets, MongoDB, Redis, Elasticsearch/OpenSearch, RabbitMQ, Docker, SSH (terminal, SFTP, commands), FTP/FTPS, Amazon S3 and 13 S3-compatible providers, Azure Blob Storage.
- SSH tunnels (with jump hosts), SOCKS5 and HTTP CONNECT proxies for TCP connectors.
- SQL editor, SSMS-style graphical execution plans, results grid with editing, import/export, object explorer, users and privileges, monitoring, backup and restore.
- Folders, environments, ⌘K/Ctrl+K search, dark and light themes, in-app update check.
- Installers: macOS drag-to-Applications DMG, Windows setup wizard and portable zip, Linux .deb, .rpm, AppImage and tar.gz.

### Security
- Secrets only in the OS credential store; SQL history and URIs with passwords encrypted at rest.
- SSH host keys verified against known_hosts or confirmed by the user before first use.
- See docs/SECURITY_AUDIT.md for the audit and fixes.

[1.0.0-beta.4]: https://github.com/openvclient/openvclient/releases/tag/v1.0.0-beta.4
[1.0.0-beta.3]: https://github.com/openvclient/openvclient/releases/tag/v1.0.0-beta.3
[1.0.0-beta.2]: https://github.com/openvclient/openvclient/releases/tag/v1.0.0-beta.2
[1.0.0-beta.1]: https://github.com/openvclient/openvclient/releases/tag/v1.0.0-beta.1
