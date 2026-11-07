<p align="center">
  <a href="https://openvclient.xyz"><picture><source media="(prefers-color-scheme: dark)" srcset="assets/brand/openvclient-logo-dark.svg"><img src="assets/brand/openvclient-logo-light.svg" alt="OpenVClient" height="88"></picture></a>
</p>

<h3 align="center">Your whole stack. One friendly database client.</h3>

<p align="center">
  A free, open-source desktop app for <b>databases</b>, <b>object storage</b> and <b>servers</b>.<br>
  PostgreSQL to S3, Redis to SSH: connect, query, browse and fix things without juggling ten tools.
</p>

<p align="center">
  <a href="https://openvclient.xyz"><b>🌐 openvclient.xyz</b></a> ·
  <a href="https://github.com/openvclient/openvclient/releases/latest"><b>⬇️ Download</b></a> ·
  <a href="#screenshots">Screenshots</a> ·
  <a href="#features">Features</a> ·
  <a href="#supported-connectors">Connectors</a> ·
  <a href="CONTRIBUTING.md">Contribute</a> ·
  <a href="https://buymeacoffee.com/rohansirohi"><b>☕ Buy me a coffee</b></a>
</p>

<p align="center">
  <a href="https://github.com/openvclient/openvclient/releases"><img alt="Latest release" src="https://img.shields.io/github/v/release/openvclient/openvclient?include_prereleases&label=release&color=22c55e"></a>
  <a href="https://github.com/openvclient/openvclient/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/openvclient/openvclient/ci.yml?branch=main&label=CI"></a>
  <img alt="Platforms" src="https://img.shields.io/badge/macOS%20%C2%B7%20Windows%20%C2%B7%20Linux-1a1612">
  <img alt="Licence" src="https://img.shields.io/badge/licence-MIT%20or%20Apache--2.0-ffdb58">
  <img alt="Written in Rust" src="https://img.shields.io/badge/native-Rust%20%2B%20egui-4ecdc4">
  <a href="https://buymeacoffee.com/rohansirohi"><img alt="Buy me a coffee" src="https://img.shields.io/badge/Buy%20me%20a%20coffee-support-ffdd00?logo=buymeacoffee&logoColor=000000"></a>
</p>

<p align="center">
  <a href="https://openvclient.xyz"><img src="docs/screenshots/home-cartoon-light.webp" alt="OpenVClient home screen in the Cartoon style: favourite connections pinned at the top, every connection with its connector sticker and environment badge" width="900"></a>
</p>

OpenVClient is a native, GPU-rendered desktop client written in Rust with [egui](https://github.com/emilk/egui). It runs on macOS, Windows and Linux, starts in a moment and keeps every connection in one window: SQL databases, NoSQL stores, message queues, S3 and Azure storage, SSH servers and Docker.

**Free and open source, all of it.** No premium tier, no account, no telemetry. Passwords live in your operating system's keychain.

> **Status: public beta (1.0.0 betas).** Expect rough edges and please [report them](https://github.com/openvclient/openvclient/issues). The project follows one rule: *never pretend a feature is supported.* The app only offers an action when the connector really implements it, and this README only lists what exists in the code today.

---

## Screenshots

All screenshots use demo data. Pick a **style** (Original or Cartoon) and a **mode** (light, dark or your system's) in Settings → Appearance.

### Two styles, light and dark

| Original · dark | Cartoon · light |
|---|---|
| <img src="docs/screenshots/home-original-dark.webp" alt="Home screen in the Original style, dark mode: favourites, connections grouped in folders, environment badges"> | <img src="docs/screenshots/home-cartoon-light.webp" alt="Home screen in the Cartoon style, light mode, with connector stickers and outlined cards"> |

### SQL editor and results

| Original · dark | Original · light |
|---|---|
| <img src="docs/screenshots/sql-original-dark.webp" alt="SQL editor running a revenue-by-country query against the demo shop database, with the result grid below"> | <img src="docs/screenshots/sql-original-light.webp" alt="The same query and results in light mode"> |

### Table data and cloud storage

| Table browser · Cartoon dark | File manager · Original light |
|---|---|
| <img src="docs/screenshots/table-cartoon-dark.webp" alt="Table browser on the orders table: filter, paging, add and delete rows, import and export, in the Cartoon style at night"> | <img src="docs/screenshots/files-original-light.webp" alt="File manager browsing an Azure Blob Storage folder with Parquet, CSV and JSON files and a JSON preview panel"> |

| File manager · Cartoon light | File manager · Cartoon dark |
|---|---|
| <img src="docs/screenshots/files-cartoon-light.webp" alt="The file manager in the Cartoon style by day, with sticker icons in tabs and breadcrumb"> | <img src="docs/screenshots/files-cartoon-dark.webp" alt="The file manager in the Cartoon style by night"> |

### Connectors, settings and shortcuts

| New connection · Cartoon light | Settings · Original dark | Keyboard shortcuts · Cartoon light |
|---|---|---|
| <img src="docs/screenshots/picker-cartoon-light.webp" alt="New connection picker listing every connector with its sticker"> | <img src="docs/screenshots/settings-original-dark.webp" alt="Settings: style, mode, editor font size, updates, scroll speed, data limits, production safety"> | <img src="docs/screenshots/shortcuts-cartoon-light.webp" alt="Keyboard shortcuts window grouped into General and SQL editor"> |

<sub>Regenerate them with `cargo test -p openvclient-app readme_screenshots -- --ignored` (see the test for the environment variables).</sub>

---

## What you can do with it

| If you… | OpenVClient gives you |
|---|---|
| **write SQL every day** | One editor for PostgreSQL, MySQL/MariaDB, SQL Server, SQLite, ClickHouse and GaussDB: autocomplete, run selection or script, cancel, history, graphical execution plans |
| **look after production databases** | Environment tags with a type-the-name check before destructive actions, read-only connections, live sessions, locks and metrics, users and grants, backup and restore |
| **live in the cloud** | A real file manager for Amazon S3, Azure Blob Storage, Backblaze B2, Cloudflare R2, Wasabi, MinIO and 10 more, plus FTP/FTPS and SFTP: upload, download, preview, share links |
| **manage servers** | A built-in SSH terminal with jump hosts, an SFTP browser, Docker containers, images, volumes and logs |
| **debug caches and queues** | Redis key browser and editor, MongoDB documents and indexes, Elasticsearch indices and search, RabbitMQ queues and exchanges |
| **reach private networks** | SSH tunnels (with jump hosts), SOCKS5 and HTTP proxies for every network connector |
| **juggle many connections** | Folders, favourites pinned on Home, ⌘K search across connections, objects and commands |

---

## Features

### Connections
- Connection profiles organised in **nested folders**; create, rename, move, duplicate and delete profiles and folders.
- **Favourites**: star a connection to pin it at the top of Home.
- **Environment tags** (development, testing, staging, production). Production connections can require you to type the name before a destructive action runs (Settings → Safety).
- A **read-only** flag for each profile, and **Test connection** before saving.
- Passwords and other secrets are stored in the **OS keychain**, never in the workspace file.
- **Network paths** for every TCP connector: direct, **SSH tunnel** (password, keyboard-interactive, private key, certificate or agent, with jump hosts), **SOCKS5** and **HTTP CONNECT** proxies.
- SSH host keys are checked against `~/.ssh/known_hosts`; a new key is shown with its fingerprint and trusted only when you confirm it.

### SQL databases
- An object tree with databases, schemas, tables, views, materialized views, sequences, functions, procedures, triggers and events. Each engine shows what it supports; SQL Server gets an SSMS-style layout.
- **SQL editor** with syntax highlighting and autocomplete (keywords, objects, columns, including after table aliases). Run the statement under the cursor or the selection (⌘/Ctrl+Enter), run the whole script (⌘/Ctrl+Shift+Enter or F5) and **cancel** a running query (⌘/Ctrl+.). The script splitter understands PostgreSQL `$$` bodies, MySQL `DELIMITER` and T-SQL `GO`.
- **EXPLAIN / EXPLAIN ANALYZE** (⌘/Ctrl+E) with a **graphical plan diagram** in the style of SQL Server Management Studio.
- A **table browser** with server-side filtering, sorting, paging and row counts. **Edit cells, insert and delete rows**; changes apply in one transaction. When rows can't be addressed by a primary key or row id, the grid is read-only and says why.
- **Export** results to CSV, TSV, JSON, SQL `INSERT` or Markdown; **import** CSV into a table.
- **Structure and DDL** view for every object.
- **Administration** where the engine supports it: create and drop databases and schemas, truncate, live **monitoring** (metrics, sessions, locks; cancel or terminate), and **users, roles and privileges**.
- **Backup and restore**: SQLite online backup, `pg_dump`/`pg_restore`, `mysqldump`/`mariadb-dump`, and SQL Server `BACKUP`/`RESTORE`. External tools must be on your `PATH`; when they're missing, the actions are hidden.

### Cloud storage and file servers
- **File manager** for Amazon S3 and S3-compatible providers, Azure Blob Storage / ADLS Gen2, FTP/FTPS and SFTP: back, forward and up, a clickable breadcrumb, sortable file list with sizes, times and tiers, multi-select.
- **Upload** (button or drag and drop from Finder / Explorer), **new folder**, **download** (one file or many), **share links** (Azure SAS, S3 presigned URLs), **rename** and **delete** with confirmation, a right-click menu and a **preview panel** for text and JSON.

### Other services
- **Redis**: database list, `SCAN` browsing with type filter, viewer and editor for every key type (string, hash, list, set, zset, stream), TTL, rename, bulk delete, command console, `INFO`, client list and slow log.
- **MongoDB**: databases, collections, views and indexes; filter, page, insert, update and delete documents; import and export JSON; database command console.
- **Elasticsearch / OpenSearch**: indices (open, close, refresh, delete), document search, index and delete, delete-by-query, REST console.
- **RabbitMQ**: queues, exchanges and bindings through the management API.
- **SSH**: an interactive terminal tab, SFTP browser and remote commands.
- **Docker**: containers (start, stop, restart, pause, kill, remove), logs, inspect, live stats and `exec`; images, volumes, networks and system info.

### Workspace
- A tabbed and tiled workspace: drag a tab to the edge of another to split.
- **Command palette** (⌘/Ctrl+K) with fuzzy search over connections, objects and actions.
- **Keyboard shortcuts window** (⌘/Ctrl+/, or Settings → Help).
- **Two styles, two modes**: the clean Original style or the Cartoon style of [openvclient.xyz](https://openvclient.xyz), with connector stickers and cartoon icons; each in light, dark or system mode.
- **Plain-language error hints** ("nothing is listening on that host and port", "the server accepts: publickey, keyboard-interactive"), **Copy diagnostics**, and a log file you can open from Settings.
- **About** panel with the version and **Check for updates** against GitHub Releases (can be turned off).

---

## Supported connectors

A connector is marked ✅ only when it is implemented **and** tested against a live server. See [ROADMAP.md](ROADMAP.md) for details.

| Service | Family | Client library | Status |
|---|---|---|---|
| MySQL | SQL | mysql_async | ✅ |
| MariaDB | SQL | mysql_async (MariaDB mode) | ✅ |
| PostgreSQL | SQL | tokio-postgres | ✅ |
| SQLite | SQL | rusqlite (bundled) | ✅ |
| SQL Server / Azure SQL | SQL | tiberius (TDS, pure Rust) | ✅ |
| ClickHouse | SQL (analytical) | HTTP interface | ✅ |
| MongoDB | Documents | mongodb | ✅ |
| Redis | Key-value | redis | ✅ |
| Elasticsearch | Search | REST | ✅ |
| Docker | Containers | bollard | ✅ |
| GaussDB / openGauss | SQL | PostgreSQL wire protocol | ✅ tested against openGauss |
| Kingbase | SQL | PostgreSQL wire protocol | 🟡 registered, untested |
| RabbitMQ | Messaging | Management HTTP API | ✅ |
| SSH: terminal, SFTP, remote commands | Remote | russh / russh-sftp | ✅ |
| FTP / FTPS (explicit and implicit TLS) | Files | suppaftp (rustls) | ✅ FTP; FTPS implemented, TLS not yet live-tested |
| Amazon S3 and S3-compatible: Backblaze B2, IDrive e2, Wasabi, Cloudflare R2, DigitalOcean Spaces, Google Cloud Storage (HMAC), Oracle, Linode, Scaleway, Hetzner, Storj, MinIO, Alibaba OSS | Object storage | S3 REST (SigV4) | ✅ tested against MinIO and SeaweedFS; cloud providers are presets of the same connector |
| Azure Blob Storage / ADLS Gen2 | Object storage | Blob REST (Shared Key, SAS, Entra ID) | ✅ tested against Azurite |
| Kafka, Cassandra | various | — | ⏳ next |
| Oracle, Db2, JDBC | SQL | need vendor client libraries | ⏳ planned, **not available yet** |

### SSH authentication

Password (falls back to keyboard-interactive like OpenSSH, for PAM servers such as Ubuntu and Linux Mint) · keyboard-interactive (PAM / OTP / 2FA prompts) · private key file (with passphrase) · pasted private key · OpenSSH user certificate · ssh-agent / Pageant / Windows OpenSSH agent · `none`. Jump hosts (ProxyJump) are supported. Host keys are checked against `~/.ssh/known_hosts` first, then pinned on first use. Kerberos/GSSAPI is not supported yet.

### Azure Blob Storage authentication

Account name + key · connection string (including `UseDevelopmentStorage=true`) · SAS URL or token (account or container level) · Microsoft Entra ID: interactive device-code sign-in, service principal, managed identity, default chain (env vars → managed identity → Azure CLI), user + password · anonymous public containers · Azurite emulator. Public, China and US Government clouds.

### SQL Server authentication

| Method (`auth`) | Notes |
|---|---|
| `sql` | SQL Server login (default) |
| `windows` | Windows/NTLM user (`DOMAIN\user`). **Windows builds only.** |
| `integrated` | Current Windows login (SSPI). **Windows builds only.** |
| `entra_password` | Microsoft Entra ID user and password |
| `entra_service_principal` | App registration: client ID and secret |
| `entra_managed_identity` | System-assigned or user-assigned managed identity |
| `entra_interactive` | Device-code sign-in. The code is shown in the app. |
| `entra_default` | Environment variables (`AZURE_TENANT_ID`/`AZURE_CLIENT_ID`/`AZURE_CLIENT_SECRET`), then managed identity, then Azure CLI (`az login`) |
| `entra_token` | A raw access token that you paste |

Named instances (for example `SQLEXPRESS`) are resolved through **SQL Server Browser** when the port is left empty.

---

## Install

**Download the latest build from [GitHub Releases](https://github.com/openvclient/openvclient/releases/latest)** or [openvclient.xyz](https://openvclient.xyz/#download):

| Platform | File | Install |
|---|---|---|
| **macOS** 11+ (Apple Silicon and Intel) | `OpenVClient-<version>-macos-universal.dmg` | Open it and drag OpenVClient onto Applications |
| **Windows** 10/11 (x64 and ARM64) | `OpenVClient-<version>-windows-x64-setup.exe` / `-arm64-setup.exe` | Run the setup wizard (no admin needed) |
| **Debian / Ubuntu** | `openvclient_<version>_amd64.deb` / `_arm64.deb` | `sudo apt install ./openvclient_*.deb` |
| **Fedora / RHEL / openSUSE** | `openvclient-<version>-1.*.rpm` | `sudo dnf install ./openvclient-*.rpm` |
| **Any Linux** | `OpenVClient-<version>-*.AppImage` | `chmod +x` and run |

Betas are not code-signed yet: on macOS right-click the app → Open the first time; on Windows SmartScreen choose More info → Run anyway. `SHA256SUMS` lists every file's checksum.

### Build the installers yourself

Installers are produced by the release workflow (see [docs/RELEASE_PLAN.md](docs/RELEASE_PLAN.md)); you can also build them yourself:

| Platform | Command | Result |
|----------|---------|--------|
| macOS | `./scripts/package-macos-dmg.sh` | `dist/OpenVClient-<version>-macos-<arch>.dmg`: open it and drag OpenVClient onto Applications |
| Windows | `.\scripts\bundle-windows.ps1` (needs [Inno Setup 6.6+](https://jrsoftware.org/isinfo.php)) | `dist\OpenVClient-<version>-windows-x64-setup.exe`: a setup wizard (per-user or all users, licence, folder, Start menu, desktop shortcut, launch; light or dark to match Windows) plus a portable zip |
| Linux | `./scripts/bundle-linux.sh` | `.deb`, `.rpm`, AppImage and a tar.gz with `install.sh`; all add a menu entry and icon |

Builds that are not signed with an Apple Developer ID / Windows code-signing certificate show a first-run warning (macOS: right-click → Open; Windows SmartScreen: More info → Run anyway). The release workflow signs and notarizes when the signing secrets are configured.

Building on a fresh Windows or Linux machine with Claude Code: see [docs/WINDOWS_BUILD_PROMPT.md](docs/WINDOWS_BUILD_PROMPT.md) and [docs/LINUX_BUILD_PROMPT.md](docs/LINUX_BUILD_PROMPT.md).

To build from source:

### Prerequisites (all platforms)
- **Rust 1.95 or newer** (edition 2024). Install it with [rustup](https://rustup.rs).
- A GPU driver with OpenGL 3.x or newer. The app renders with `glow` (OpenGL).
- Optional: `pg_dump`/`pg_restore`, `mysqldump`/`mariadb-dump`/`mysql` on `PATH` for backup and restore (on Windows the default install folders under Program Files are searched too). The Azure CLI (`az`) is used by `entra_default`.

### macOS
```sh
xcode-select --install          # Command Line Tools (clang, codesign)
git clone https://github.com/openvclient/openvclient.git
cd openvclient
cargo run --release             # or build the app bundle:
./scripts/bundle-macos.sh       # -> dist/OpenVClient.app (ad-hoc signed)
./scripts/package-macos-dmg.sh  # -> drag-to-Applications .dmg
```
OpenVClient needs macOS 11 or later. TLS for PostgreSQL, MySQL and Redis uses the system Security framework.

### Windows
1. Install the [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with the **Desktop development with C++** workload (MSVC and the Windows SDK; `rc.exe` embeds the app icon).
2. Install Rust with `rustup-init.exe` (the default `x86_64-pc-windows-msvc` toolchain).
3. For the installer only: Inno Setup 6.6 or newer (`winget install JRSoftware.InnoSetup`).
4. Build:
   ```powershell
   git clone https://github.com/openvclient/openvclient.git
   cd openvclient
   cargo run --release
   # or: .\scripts\bundle-windows.ps1   -> setup.exe installer + portable zip in dist\
   ```
Windows builds are the only builds that support SQL Server `windows` and `integrated` authentication (SSPI). TLS uses SChannel. SSH agent authentication uses the Windows OpenSSH agent (`ssh-agent` service) and falls back to Pageant. Rendering uses OpenGL (`glow`); where OpenGL 2+ is missing — Remote Desktop sessions, virtual machines with the Basic Display Adapter, very old drivers — the app automatically retries with Direct3D 12 through `wgpu`, which falls back to WARP software rendering. Backup and restore find `pg_dump`/`mysqldump` on `PATH` or in the default install folders (`C:\Program Files\PostgreSQL\<version>\bin`, `C:\Program Files\MySQL\MySQL Server <version>\bin`, `C:\Program Files\MariaDB <version>\bin`).

### Linux
Install the build dependencies first. eframe and winit need X11 and Wayland headers. PostgreSQL, MySQL and Redis TLS uses OpenSSL (through `native-tls`), so the OpenSSL headers are required.

**Debian / Ubuntu**
```sh
sudo apt-get install -y build-essential pkg-config libssl-dev \
  libxkbcommon-dev libwayland-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libx11-dev libxcursor-dev libxrandr-dev libxi-dev libgl1-mesa-dev
```
**Fedora**
```sh
sudo dnf install -y gcc pkg-config openssl-devel libxkbcommon-devel wayland-devel \
  libxcb-devel libX11-devel libXcursor-devel libXrandr-devel libXi-devel mesa-libGL-devel
```
**Arch**
```sh
sudo pacman -S --needed base-devel pkgconf openssl libxkbcommon wayland libxcb libx11 mesa
```
Then build:
```sh
git clone https://github.com/openvclient/openvclient.git
cd openvclient
cargo run --release
# or: ./scripts/bundle-linux.sh    -> dist/openvclient-<version>-linux-<arch>.tar.gz
```
Some Linux runtime services are needed at runtime only, not to build:
- **Secret Service** (GNOME Keyring, KWallet ≥ 5.97 or KeePassXC) on the session D-Bus, to store passwords. The keyring backend talks to D-Bus in pure Rust, so `libdbus` is not needed.
- **xdg-desktop-portal** (with a GTK, KDE or GNOME backend) for the native open and save dialogs.

### Build without some connectors
Every connector is a cargo feature of `openvclient-app`. The defaults are `sqlite postgres mysql redis docker sqlserver mongodb http`.
```sh
cargo run --release --no-default-features --features "sqlite,postgres"
```

---

## Running

```sh
cargo run --release           # the binary is named `openvclient`
```

| Variable | Purpose |
|---|---|
| `OPENVCLIENT_CONFIG_DIR` | Use a different configuration and workspace directory (useful for testing). |
| `OPENVCLIENT_LOG` | Log filter, `tracing` `EnvFilter` syntax. Default `warn,openvclient=info`. Example: `OPENVCLIENT_LOG=debug`. |

### Configuration directory

All profiles, folders and settings live in `workspace.json` in this directory. Secrets are kept in the keychain instead.

| OS | Location |
|---|---|
| macOS | `~/Library/Application Support/dev.OpenVClient.OpenVClient/` |
| Windows | `%APPDATA%\OpenVClient\OpenVClient\config\` (the uninstaller offers to remove `%APPDATA%\OpenVClient`) |
| Linux | `$XDG_CONFIG_HOME/openvclient/` (usually `~/.config/openvclient/`) |

Settings → *Workspace file* shows the exact path on your machine. If `workspace.json` cannot be read, OpenVClient keeps a copy as `workspace.broken-<timestamp>.json` and starts with an empty workspace. It never overwrites the broken file silently.

### Logs
Logs go to stderr and to `<config dir>/logs/openvclient.log`. The file is rotated to `openvclient.1.log` at 5 MB. Settings → **Open logs folder** opens the directory. Logs record host, port and outcome for each connection. They **never contain secrets**.

---

## Security model

- **Secrets stay in the OS keychain.** This is macOS Keychain, Windows Credential Manager or the Linux Secret Service. Entries use the service name `dev.openvclient.client`, keyed by the profile id. `workspace.json` holds no passwords, tokens or passphrases. SSH passwords, key passphrases and proxy passwords are stored the same way.
- **SSH host keys are never trusted silently.** A key listed in `~/.ssh/known_hosts` is accepted; any other new key is shown with its SHA-256 fingerprint and only trusted when you confirm it, then pinned in the profile. A key that changes later is refused with an explanation.
- **Workspace data at rest:** SQL history and connection URIs with embedded passwords are encrypted (AES-256-GCM) in `workspace.json`, with the key held in the system keychain; the file is owner-only on macOS/Linux.
- **TLS.** SQL Server uses rustls. Encryption is on by default; `required` also verifies the certificate. PostgreSQL, MySQL and Redis use the platform TLS stack, with each engine's usual SSL modes.
- **Safety rails.** Destructive actions ask for confirmation. Profiles tagged *production* can require you to type the connection name. Read-only profiles refuse changes.
- **No telemetry.** OpenVClient connects only to the servers you configure, Microsoft Entra ID endpoints when you choose an Entra sign-in, and GitHub Releases to check for a new version (Settings → Updates; can be turned off). There are no analytics.

To report a vulnerability, see [SECURITY.md](SECURITY.md).

---

## Support the project

OpenVClient is free and open source, built and maintained by one person. If it saves you time, you can support its development:

<p>
  <a href="https://buymeacoffee.com/rohansirohi"><img alt="Buy me a coffee" src="https://img.shields.io/badge/Buy%20me%20a%20coffee-rohansirohi-ffdd00?style=for-the-badge&logo=buymeacoffee&logoColor=000000"></a>
</p>

Starring the repository, reporting bugs and sharing it with your team help just as much.

## Contributing

Contributions are welcome: bugs, connector requests and pull requests. [CONTRIBUTING.md](CONTRIBUTING.md) covers the workspace layout, adding a connector, the UI conventions and running the live tests. The manual and automated test catalogue is in [docs/TEST_CASES.md](docs/TEST_CASES.md). Please follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

OpenVClient is dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in the work, as defined in the Apache-2.0 license, shall be dual-licensed as above, without any additional terms or conditions.

---

<p align="center">
  Created and maintained by <a href="https://github.com/Mrhudson69"><b>Rohan Sirohi</b></a> · <a href="https://openvclient.xyz">openvclient.xyz</a><br>
  <sub>If OpenVClient saves you time, a ⭐ on GitHub helps others find it.</sub>
</p>
