# Restic Studio — development preview

Rust backup/recovery core, headless runner, and GTK4/libadwaita client source.
This is **not a production-ready release**. The complete roadmap is not implemented.

## Build

The core and runner build with Rust stable and the system Restic executable:

```sh
cargo build --offline
cargo test --offline
cargo fmt --check
cargo clippy --offline --all-targets --all-features -- -D warnings
cargo build --offline --release
target/release/restic-studio version
```

For the graphical client, install Debian development dependencies in an environment
where package installation is authorized:

```sh
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev restic
cargo build --manifest-path crates/restic-gui/Cargo.toml
cargo run --manifest-path crates/restic-gui/Cargo.toml
```

The GUI is a separate Cargo workspace so the runner builds independently.
The GUI builds on the current Debian forky host; desktop launch and Debian 13
compatibility have not been verified in this sandbox.

In the GUI, choose **Choose external drive or destination…** and select a mounted
drive folder. If the folder itself or exactly one immediate child is an existing
Restic repository, the picker uses that repository. If several child repositories
exist, select the intended repository folder directly. A new repository uses the
drive folder's `restic-studio` subfolder. Click **Initialize empty repository**
only for a new, empty destination. If the drive is missing, backup stops because
its repository configuration is unavailable; the app never creates a replacement
repository during backup.

## Runner

Commands: `version`, `init REPO`, `snapshots REPO`, `ls REPO SNAPSHOT DIRECTORY`, `check REPO`,
`backup PROFILE.json`, `run-profile ID`, and
`restore REPO FULL_SNAPSHOT_ID SNAPSHOT_PATH NEW_DIRECTORY`.
All paths must be absolute. Restore reconstructs snapshot-relative paths inside a
new destination, preserving existing destinations. The parent must be private,
owned by the invoking user, and already exist. `/`, `/etc`, `/usr`, and `/boot`
are prohibited restore parents.

Supply a password using a protected stdin pipe, closed after its final newline.
There is no interactive hidden terminal prompt yet. Never put passwords in command
arguments or shell history. The application does not save passwords.

Profile JSON contains `id`, `name`, `repository: {"location": "/absolute/repo"}`,
`sources: ["/absolute/source"]`, `excludes: []`, and `one_file_system: false`.
`run-profile` loads `$XDG_STATE_HOME/restic-studio/profiles/ID.json`
(default `~/.local/state/restic-studio/profiles/ID.json`). It requires credentials
on stdin and does not yet constitute unattended scheduling.

## Status and safety

See [architecture](docs/architecture.md), [verification](docs/verification.md), and
[installation](docs/installation.md), and [remaining work](docs/remaining-work.md). No automatic unlock, forget, prune,
mount, partition, ownership repair, or system configuration changes are performed.
No Debian package is claimed: the native GUI must build and pass recovery acceptance
before a complete application package can be delivered.
