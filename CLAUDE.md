# OpenVClient client

Rust + egui desktop app for managing databases and infrastructure. The workspace lives in `crates/`: `openvclient-core` defines the contracts, `openvclient-<engine>` crates are the connectors, and `openvclient-app` is the UI.

## UI design work
- Use the `ui-ux-pro-max` skill (`.claude/skills/ui-ux-pro-max`) for **all** UI design work: new views, restyling, layout, colour, typography, interaction and accessibility reviews.
- Read `design-system/openvclient/MASTER.md` first. A page file in `design-system/openvclient/pages/` overrides it for that view.
- The skill has no egui stack. Use `--design-system` and `--domain ux|style|color|typography` searches, then map the results to egui yourself.
- Colours come from `theme::tokens(ui)` and icons from `egui_material_icons`. Never use raw hex values in views, and never use emoji as icons.

## Testing
Connector tests run against live local servers and skip themselves when a server is unreachable. SSH has its own lab covering every login method, key type, jump host, tunnel and SFTP edge case: `scripts/ssh-lab.sh up`, then `scripts/ssh-lab.sh test`, then `scripts/ssh-lab.sh down`. They need PostgreSQL on localhost, MySQL on 127.0.0.1 as root, and Redis on port 56379 (`docker run -d --rm --name openvclient-test-redis -p 56379:6379 redis:7-alpine`).

## Build and UI check
- `./scripts/bundle-macos.sh` builds `dist/OpenVClient.app`: a release build with an ad-hoc signature.
- Headless UI walkthrough: it drives the real app against the local services and writes PNG screenshots. Use it to check layout instead of OS screen capture, which needs Screen Recording permission.
  `OPENVCLIENT_CONFIG_DIR=<copy of workspace dir> OPENVCLIENT_SHOTS=<out dir> cargo test -p openvclient-app ui_walkthrough -- --ignored --nocapture`
- Large and strong text can look faint in kittest renders even though it is fine in the real window. Don't chase that.
- Demo data: `demo/shop.db` (SQLite) and the `openv_demo` database on local Postgres (walkthrough override: `OPENVCLIENT_DEMO_DB`).
