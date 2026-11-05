#!/bin/sh
set -eu

project_root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT
chmod 0755 "$stage"

runner="$project_root/target/release/restic-studio"
gui="$project_root/crates/restic-gui/target/release/restic-studio-gui"
test -x "$runner" && test -x "$gui" || {
	echo 'Build both release executables before packaging.' >&2
	exit 1
}

install -Dm755 "$runner" "$stage/usr/bin/restic-studio"
install -Dm755 "$gui" "$stage/usr/bin/restic-studio-gui"
install -Dm644 "$project_root/packaging/desktop/org.resticstudio.Studio.desktop" "$stage/usr/share/applications/org.resticstudio.Studio.desktop"
install -Dm644 "$project_root/packaging/desktop/org.resticstudio.Studio.svg" "$stage/usr/share/icons/hicolor/scalable/apps/org.resticstudio.Studio.svg"
install -Dm644 "$project_root/README.md" "$stage/usr/share/doc/restic-studio/README.md"
install -Dm644 "$project_root/docs/architecture.md" "$stage/usr/share/doc/restic-studio/architecture.md"
install -Dm644 "$project_root/docs/verification.md" "$stage/usr/share/doc/restic-studio/verification.md"
install -Dm644 "$project_root/docs/installation.md" "$stage/usr/share/doc/restic-studio/installation.md"

mkdir -p "$stage/DEBIAN"
architecture=$(dpkg --print-architecture)
cat >"$stage/DEBIAN/control" <<EOF
Package: restic-studio
Version: 0.1.0-1
Section: utils
Priority: optional
Architecture: $architecture
Maintainer: Restic Studio Project <noreply@localhost>
Depends: restic (>= 0.19), libgtk-4-1 (>= 4.24), libadwaita-1-0 (>= 1.10), libc6 (>= 2.43)
Description: Native desktop interface for local Restic backups
 Backup, browse, verify and restore local Restic repositories.
 This development preview requires user review before relying on it for recovery.
EOF
dpkg-deb --build --root-owner-group "$stage" "$project_root/restic-studio_0.1.0-1_${architecture}.deb"
