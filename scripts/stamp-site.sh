#!/usr/bin/env bash
# Fill @VERSION@, @DATE@ and @DATE_HUMAN@ in a built page from the latest GitHub release.
# usage: stamp-site.sh FILE [FALLBACK_VERSION]
# Needs GH_TOKEN and GITHUB_REPOSITORY (set by Actions). Without a release it uses the
# fallback version and today's date, so the page never ships with a placeholder.
set -euo pipefail

file=${1:?usage: stamp-site.sh FILE [FALLBACK_VERSION]}
version=''
day=''
if [[ -n ${GH_TOKEN:-} && -n ${GITHUB_REPOSITORY:-} ]] &&
	info=$(gh api "repos/$GITHUB_REPOSITORY/releases/latest" --jq '[.tag_name, .published_at] | @tsv' 2>/dev/null); then
	IFS=$'\t' read -r tag published <<<"$info"
	version=${tag#v}
	day=${published%%T*}
fi
version=${version:-${2:-}}
[[ -n $version ]] || {
	echo "stamp-site: no release and no fallback version" >&2
	exit 1
}
case $day in '' | 0001*) day=$(date -u +%F) ;; esac # a missing or unset release date means today
human=$(date -u -d "$day" '+%B %-d, %Y')

sed -i -e "s/@VERSION@/$version/g" -e "s/@DATE@/$day/g" -e "s/@DATE_HUMAN@/$human/g" "$file"
if grep -q '@VERSION@\|@DATE@\|@DATE_HUMAN@' "$file"; then
	echo "stamp-site: placeholder left in $file" >&2
	exit 1
fi
echo "stamp-site: $file -> $version, $day"
