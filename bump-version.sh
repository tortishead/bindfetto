#!/usr/bin/env bash
# Set the product version across every manifest that carries one, in lockstep.
# bindfetto ships as one bundle, so all components share a single version; this is
# the only supported way to change it (release.sh refuses to publish a mismatched set).
#
# Sources bumped:
#   runtime/Cargo.toml            [workspace.package] version   (binary + all member crates)
#   decode/Cargo.toml             [package] version
#   plugins/vscode/package.json   "version"
#   bindfetto-app/app/build.gradle.kts   versionName + versionCode (int, auto-incremented)
#
# Usage:
#   ./bump-version.sh <version> [versionCode]
#     version       new semver, e.g. 0.2.0
#     versionCode   optional Android integer; defaults to current + 1
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

B=$'\033[1m'; GRN=$'\033[32m'; YLW=$'\033[33m'; RST=$'\033[0m'
die() { printf '%s\n' "${YLW}error:${RST} $*" >&2; exit 1; }
ok()  { printf '%s\n' "${GRN}✓ $*${RST}"; }

VERSION="${1:-}"
[ -n "$VERSION" ] || die "usage: ./bump-version.sh <version> [versionCode]"
printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)?$' \
  || die "not a semver: $VERSION"

GRADLE="bindfetto-app/app/build.gradle.kts"
CUR_CODE="$(grep -oE 'versionCode = [0-9]+' "$GRADLE" | grep -oE '[0-9]+')"
NEW_CODE="${2:-$((CUR_CODE + 1))}"
printf '%s' "$NEW_CODE" | grep -Eq '^[0-9]+$' || die "versionCode must be an integer: $NEW_CODE"

# Replace `version = "..."` only inside the given TOML section header.
set_toml_version() { # <file> <section-header> <new>
  awk -v sec="$2" -v ver="$3" '
    /^\[/ { insec = ($0 == sec) }
    insec && /^version[[:space:]]*=[[:space:]]*"/ {
      sub(/"[^"]*"/, "\"" ver "\""); done=1
    }
    { print }
    END { if (!done) exit 3 }
  ' "$1" > "$1.tmp" || { rm -f "$1.tmp"; die "no version line in $2 of $1"; }
  mv "$1.tmp" "$1"
}

set_toml_version runtime/Cargo.toml '[workspace.package]' "$VERSION"
set_toml_version decode/Cargo.toml   '[package]'          "$VERSION"

# package.json: first top-level "version" key only.
awk -v ver="$VERSION" '
  !done && /"version"[[:space:]]*:/ {
    sub(/"version"[[:space:]]*:[[:space:]]*"[^"]*"/, "\"version\": \"" ver "\""); done=1
  }
  { print }
' plugins/vscode/package.json > plugins/vscode/package.json.tmp \
  && mv plugins/vscode/package.json.tmp plugins/vscode/package.json

# gradle: versionName string + versionCode int.
sed -i.bak -E \
  -e "s/versionName = \"[^\"]*\"/versionName = \"$VERSION\"/" \
  -e "s/versionCode = [0-9]+/versionCode = $NEW_CODE/" \
  "$GRADLE" && rm -f "$GRADLE.bak"

ok "runtime/Cargo.toml           -> $VERSION"
ok "decode/Cargo.toml            -> $VERSION"
ok "plugins/vscode/package.json  -> $VERSION"
ok "$GRADLE  -> versionName $VERSION, versionCode $NEW_CODE"
printf '%s\n' "${B}Bumped to ${VERSION} (code ${NEW_CODE}). Review, commit, then build + ./release.sh ${VERSION} --upload${RST}"
