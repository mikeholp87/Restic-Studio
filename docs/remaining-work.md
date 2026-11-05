# Release blockers and remaining plan

1. Obtain gtk-rs crates and native development packages. Build and launch the GUI;
   perform the first complete graphical backup/recovery acceptance test. The unverified
   GUI has a basic directory browser capped at 500 entries per directory.
2. Add capability probing, persistent repository identity and removable-device checks,
   bounded large-array snapshot parsing, diagnostic provenance, durable job transitions,
   cross-client repository job coordination, and subprocess failure tests.
3. Implement profile management and the eight-section navigation, setup wizard,
   file browsing/search/diff, conflict preview, and selective home migration.
4. Add Secret Service and systemd encrypted credential support, generated user timers,
   unattended runner credential retrieval, bounded retries, and missed-run behavior.
   Do not enable schedules with unavailable credentials.
5. Add health history, verification policies, retention preview/authorization,
   separately scheduled pruning, storage accounting, notifications, and diagnostics.
6. Add remote backend configuration, stable external-drive identity, package inventories,
   read-authorized system backup, and offline recovery documentation.
7. Complete adversarial recovery testing, KDE/GNOME compatibility, accessibility,
   dependency/security audit, measured performance, Debian packaging, and installation
   lifecycle tests. Do not label this preview suitable for daily backup reliance.

No feature in this list is represented as completed or verified.
