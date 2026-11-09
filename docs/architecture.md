# Architecture and trust boundaries

## Components

`restic-core` owns validated local profiles, structured subprocess arguments,
bounded output processing, outcomes, snapshot models, recovery, and profile/history
persistence. `restic-runner` provides a CLI and serializes runner jobs with flock.
`restic-gui` contains GTK4/libadwaita controls, theme selection, background worker
threads, bounded progress messages, and graceful backup cancellation. Snapshot
directory entries are streamed from Restic; the experimental GUI displays the first
500 entries and reports the total.

No shell executes user data. Restic performs encryption and repository operations;
repository internals are not rewritten. The executable defaults to `/usr/bin/restic`.
The locally tested Restic version is 0.19.1. A complete capability compatibility
matrix is still required before advertising support for other versions.

## Credentials and output

Passwords travel through a private stdin pipe via Restic's `--password-file
/dev/stdin`; they are not command arguments or saved configuration. Inherited
RESTIC_* settings are removed from children. Passwords remain ordinary Rust strings;
memory erasure is not guaranteed. Structured error messages are bounded and redact
the password and repository location. Raw non-JSON diagnostics are currently discarded.
Job history contains outcomes and errors, not file contents or passwords.

Readers independently drain stdout/stderr through a bounded 128-record channel.
Each record is limited to 1 MiB. Snapshot arrays larger than that limit fail explicitly;
streaming large arrays remains future work. Backup partial exit status is never success.
Cancellation uses SIGINT and waits for Restic cleanup; there is no forced-kill UI yet.
The GUI checks the device number of a selected destination folder before each
operation. The core pins the repository directory for the duration of each Restic
subprocess, so an unmount or path replacement cannot redirect an operation already
in progress through that pathname. Backup and restore keep that same repository
directory open from the Restic ID check through the operation. The first successful
operation pins the Restic ID and, when available, the udev disk WWN or serial number.
Later operations refuse a changed or unavailable pinned disk identifier. Disks without
an exposed identifier retain only the Restic ID check; identifiers can be duplicated
or change with adapters, and are not proof of physical identity. The pins are stored in
`repository-ids.json` in the application's state directory.

## Restore transaction

Only new-directory restores are implemented. Pin the private destination parent
with an open directory descriptor and create private staging beneath its procfs
descriptor path. Restore from the selected snapshot subtree with escaped include
patterns, no overwrite, and content verification. Preserve the original snapshot
path beneath the destination without restoring unrelated ancestor metadata.
Publish with directory-relative Linux renameat2(RENAME_NOREPLACE). Existing
destinations cannot be replaced in a race. A publication failure retains staging;
other failures leave the requested destination unchanged.

This workflow requires Linux procfs and renameat2 support. No arbitrary in-place
placement, metadata migration, or system recovery is implemented.

## Persistence and restart

Profiles are atomic JSON replacements with mode 0600 in private directories.
Runner activity is append-only JSONL in XDG state. SQLite and portable TOML have
not been introduced yet. Interrupted-job reconciliation is not implemented.

## References

- [Restic scripting and JSON](https://restic.readthedocs.io/en/stable/075_scripting.html)
- [Restore behavior](https://restic.readthedocs.io/en/stable/050_restore.html)
- [Retention safety](https://restic.readthedocs.io/en/stable/060_forget.html)
- [gtk-rs](https://gtk-rs.org/)
- [GIO](https://docs.gtk.org/gio/)
- [Secret Service](https://specifications.freedesktop.org/secret-service/latest/)
- [Systemd timers](https://www.freedesktop.org/software/systemd/man/latest/systemd.timer.html)
- [XDG directories](https://specifications.freedesktop.org/basedir/latest/)
- [Debian policy](https://www.debian.org/doc/debian-policy/)
