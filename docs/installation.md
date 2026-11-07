# Installing the development package

The package `restic-studio_0.1.0-11_amd64.deb` was built on Debian forky/sid.
It depends on the installed system Restic, GTK4, libadwaita and glibc versions.
Do not use this binary package on Debian 13 without rebuilding and testing there.

Install from the project directory:

```sh
sudo apt install ./restic-studio_0.1.0-11_amd64.deb
```

Launch **Restic Studio** from the application menu, or run
`restic-studio-gui` in a desktop terminal. The headless runner is
`restic-studio`.

To rebuild after code changes:

```sh
cargo build --release
cargo build --manifest-path crates/restic-gui/Cargo.toml --release
sh packaging/debian/build.sh
```

Uninstall with `sudo apt remove restic-studio`. The package contains no backup
repository, snapshot, profile, password or user data; removing the package
does not remove them. The app is a development preview, not a verified daily
backup solution. See `verification.md` before relying on it for recovery.
