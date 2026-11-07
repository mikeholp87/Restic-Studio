# Verification report — 2026-11-04

## Environment

Debian forky/sid, GNOME Wayland session, x86-64; Rust 1.96.0;
Restic 0.19.1; GTK runtime 4.24.1; libadwaita runtime 1.10.0.
All repository-changing tests used synthetic files and temporary repositories.
No real repository, source directory, desktop configuration, or system package was changed.

## Passed

- `cargo test --offline`: 3 tests passed. Two-snapshot recovery, metadata/content
  comparisons, single-file recovery with glob metacharacters, directory recovery,
  symlinks, empty directories, Unicode, hidden files, large fixture, repository
  structure check, bad-password rejection, existing-destination rejection,
  recursive-configuration rejection, missing-repository rejection, outcome/ID
  validation, and headless run-profile/activity integration.
- `cargo build --offline --release`: runner and core release build succeeded.
- `cargo fmt --check`: passed for core/runner workspace.
- GUI source formatting check passed separately.
- `cargo clippy --offline --all-targets --all-features -- -D warnings`: passed
  for core/runner workspace. GUI is not part of this claim.
- Release runner `version`: successfully detected Restic 0.19.1.
- Initial gitleaks directory scan reported no leaks, with two transient build-file
  lookup warnings. A final source-only rescan is recorded in task output.

## Review and fixes

A read-only review identified silent oversized snapshot responses, missing
diagnostics, restore-parent races, and unbounded password input. Snapshot listing
now fails explicitly when no valid bounded array was read. Structured errors are
bounded and redacted. Restore pins a private parent and publishes without replacement.
Credential input is bounded. Full repository identity and removable-mount validation
remain release blockers.

## Blocked / not verified

At the initial review, `libadwaita` and GTK development packages were unavailable.
They became available later, and the GUI now builds, passes Clippy, and passes its
unit tests. The sandbox still cannot connect to the desktop display, so the GUI
has not been visually inspected or tested end to end. The first mandatory
graphical milestone has therefore **not passed**.

No Debian 13, KDE, X11, remote-provider, Secret Service, systemd encrypted credential,
timer, hostile-snapshot, abrupt-kill, or concurrent desktop-client acceptance tests
were performed. No full security or dependency audit was completed. No startup,
memory, CPU, throughput, or responsiveness benchmarks were collected.

No complete application Debian package was produced. Advanced features and the
daily-use production acceptance criteria remain incomplete; see remaining-work.md.

## Debian package — 2026-11-05

Built `restic-studio_0.1.0-1_amd64.deb` for this Debian forky/sid host. The
archive contains both release executables, a validated desktop entry, an SVG
icon and documentation. `dpkg-deb` inspection, extraction, runner version
probe, linked-library check, shellcheck, shfmt and secret scan passed.
System installation was not performed because the execution environment rejected
the privileged `dpkg -i` request. GUI launch and desktop integration remain
unverified, and the package must not be treated as a production backup release.

## Backup preview — 2026-11-05

Added Restic `backup --dry-run` to the shared engine, GUI, and runner. The disposable
recovery test verifies that preview completes and snapshot listing stays empty. The
headless runner test independently invokes preview and confirms no snapshot exists
before its synthetic backup. Core/runner and GUI tests and Clippy pass.

No dry-run was executed against the user's external repository: its password is not
available to this process. Desktop GUI interaction remains unverified because the
sandbox cannot connect to the display. The `/home` source includes an unreadable
`lost+found` directory, so its preview may report a partial result.

The user's GUI preview of the existing `DebianBackup` repository failed while reading
root-owned snapshot files with owner-only permissions. The application does not run
Restic as root or repair repository ownership. Access and recovery of that repository
remain unverified.

## Progress and cancellation — 2026-11-05

Backup, preview, and restore now show a modal progress window with a graceful Stop
control. Progress uses Restic JSON status fields; the bar is indeterminate until
Restic reports a total. A disposable restore test verifies that a cancellation
request leaves the destination unpublished. Core/runner and GUI tests and Clippy
pass. The GTK window itself could not be exercised in this sandbox display session.

## Backup and restore audit — 2026-11-05

Reviewed the GUI destination selection, subprocess boundary, profile validation,
snapshot listing, staging restore, and headless runner. Fixed these concrete issues:

- GUI selection of a drive now records its filesystem device and stops operations
  if the selected folder disappears or its device changes. The drive must then be
  selected again.
- Restic subprocesses now use a pinned directory descriptor for the repository,
  reducing mount/path replacement races during an operation.
- Non-JSON process output is retained as bounded, password-redacted diagnostic
  text rather than being silently discarded.

The disposable two-snapshot restore test and headless runner test pass after the
repository-pinning change. The GUI tests cover path selection and a disappearing
selected destination. Core/runner Clippy and GUI Clippy pass with warnings denied.
The recovery fixture also restores a symlink pointing outside the restored tree
and confirms the outside file remains unchanged.

Limits: a manually typed destination is not bound to a selected drive; the GUI
drive guard is session-only and compares filesystem device numbers rather than
stable drive UUIDs. A drive can be swapped before selection. There is no verified
physical-disk warning, repository identity pin across sessions, desktop UI test,
large snapshot array test, or adversarial symlink restore test. The application
cannot yet be certified for unattended or full-system backups.

## Progress dialog manual check — 2026-11-07

The user launched the installed `0.1.0-8` GUI on a desktop session and exercised the
backup/restore progress dialog against a disposable repository; it displayed correctly.
Which of backup, restore, Stop and preview were exercised was not recorded. This is
a manual spot check, not an end-to-end acceptance pass; the first graphical milestone
in remaining-work.md is still open.
