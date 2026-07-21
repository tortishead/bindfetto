#!/usr/bin/env bash
# Package the built artifacts under canonical, versioned asset names and (optionally)
# upload them to the matching GitHub release. install.sh resolves each component by a
# stable prefix+suffix pattern, so the version may sit anywhere in the middle.
#
# Asset name contract (must stay in sync with install.sh's resolve_asset patterns):
#   bindfetto-<ver>-aarch64-android                      runtime capture binary
#   bindfetto-app-<ver>.apk                              control app
#   bindfetto-decode-<ver>.vsix                          VS Code extension
#   libbindfettodecoderplugin-<ver>-macos-arm64.so       DLT plugin (macOS)
#   libbindfettodecoderplugin-<ver>-linux.so             DLT plugin (Linux)
#
# Each component is staged only if its build output exists, so this can run per-host
# (e.g. the macOS .so on a Mac, the Linux .so on Linux) and upload with --clobber.
#
# Usage:
#   ./release.sh [version] [--tag <tag>] [--upload] [--repo <owner/name>]
#     version   defaults to the version in runtime/Cargo.toml
#     --tag     release tag to upload to (default: the version, matching existing tags)
#     --upload  create/refresh the GitHub release and upload; without it, only stage dist/
#     --repo    GitHub repo (default: origin remote)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

B=$'\033[1m'; DIM=$'\033[2m'; GRN=$'\033[32m'; YLW=$'\033[33m'; RST=$'\033[0m'
info() { printf '%s\n' "${B}$*${RST}"; }
warn() { printf '%s\n' "${YLW}! $*${RST}" >&2; }
ok()   { printf '%s\n' "${GRN}✓ $*${RST}"; }
die()  { printf '%s\n' "${YLW}error:${RST} $*" >&2; exit 1; }

VERSION=""; TAG=""; UPLOAD=0; REPO=""
while [ $# -gt 0 ]; do
  case "$1" in
    --tag)    TAG="$2"; shift 2 ;;
    --repo)   REPO="$2"; shift 2 ;;
    --upload) UPLOAD=1; shift ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    -*)       die "unknown option: $1" ;;
    *)        VERSION="$1"; shift ;;
  esac
done

[ -n "$VERSION" ] || VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' runtime/Cargo.toml | head -1)"
[ -n "$VERSION" ] || die "could not determine version; pass it explicitly."
[ -n "$TAG" ] || TAG="$VERSION"
[ -n "$REPO" ] || REPO="$(git config --get remote.origin.url | sed -E 's#.*github.com[:/]##; s#\.git$##')"

# ---- preflight: every manifest must agree on VERSION (bump-version.sh keeps them in
# lockstep). Hard-fail before an --upload; warn on a dry run so you can still inspect dist/.
V_RUNTIME="$(sed -n 's/^version = "\(.*\)"/\1/p' runtime/Cargo.toml | head -1)"
V_DECODE="$(sed -n 's/^version = "\(.*\)"/\1/p' decode/Cargo.toml | head -1)"
V_VSCODE="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' plugins/vscode/package.json | head -1)"
V_DLT="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' plugins/dlt/bindfettodecoderplugin.json | head -1)"
V_APP="$(sed -n 's/.*versionName = "\([^"]*\)".*/\1/p' bindfetto-app/app/build.gradle.kts | head -1)"
mismatch=""
for pair in "runtime/Cargo.toml=$V_RUNTIME" "decode/Cargo.toml=$V_DECODE" \
            "plugins/vscode/package.json=$V_VSCODE" "dlt/bindfettodecoderplugin.json=$V_DLT" \
            "app/build.gradle.kts=$V_APP"; do
  [ "${pair#*=}" = "$VERSION" ] || mismatch="${mismatch}"$'\n'"  ${pair%%=*}: ${pair#*=} (want ${VERSION})"
done
if [ -n "$mismatch" ]; then
  if [ "$UPLOAD" -eq 1 ]; then
    die "version mismatch — run ./bump-version.sh ${VERSION} first:${mismatch}"
  else
    warn "version mismatch (dry run continues):${mismatch}"
  fi
fi

DIST="$ROOT/dist"
rm -rf "$DIST"; mkdir -p "$DIST"
info "Packaging bindfetto ${VERSION} (tag ${TAG}) -> dist/"

staged=0
stage() { # <src> <dest-basename>
  if [ -f "$1" ]; then
    cp "$1" "$DIST/$2"; ok "staged $2"; staged=$((staged+1))
  else
    warn "skip $2 — not built ($1)"
  fi
}

# ---- runtime capture binary ----
stage "runtime/target/aarch64-linux-android/release/bindfetto" \
      "bindfetto-${VERSION}-aarch64-android"

# ---- control app APK (sign an unsigned release build if needed) ----
APK_SIGNED="bindfetto-app/app/build/outputs/apk/release/bindfetto-app-${VERSION}.apk"
APK_UNSIGNED="bindfetto-app/app/build/outputs/apk/release/app-release-unsigned.apk"
if [ ! -f "$APK_SIGNED" ] && [ -f "$APK_UNSIGNED" ]; then
  BT="$(ls -d "${ANDROID_HOME:-$HOME/Library/Android/sdk}"/build-tools/* 2>/dev/null | sort -V | tail -1)"
  KS="${BINDFETTO_KEYSTORE:-$HOME/.android/debug.keystore}"
  KS_PASS="${BINDFETTO_KEYSTORE_PASS:-android}"
  KS_ALIAS="${BINDFETTO_KEY_ALIAS:-androiddebugkey}"
  KEY_PASS="${BINDFETTO_KEY_PASS:-android}"
  if [ -n "$BT" ] && [ -f "$KS" ]; then
    "$BT/zipalign" -f -p 4 "$APK_UNSIGNED" "${APK_UNSIGNED%.apk}-aligned.apk"
    "$BT/apksigner" sign --ks "$KS" --ks-pass "pass:$KS_PASS" \
      --ks-key-alias "$KS_ALIAS" --key-pass "pass:$KEY_PASS" \
      --out "$APK_SIGNED" "${APK_UNSIGNED%.apk}-aligned.apk"
    [ "$KS" = "$HOME/.android/debug.keystore" ] && warn "APK signed with the debug keystore (not a release cert)."
  else
    warn "cannot sign APK — missing build-tools or keystore."
  fi
fi
stage "$APK_SIGNED" "bindfetto-app-${VERSION}.apk"

# ---- VS Code extension (vsce already names it with the version) ----
stage "plugins/vscode/bindfetto-decode-${VERSION}.vsix" \
      "bindfetto-decode-${VERSION}.vsix"

# ---- DLT Viewer plugin (staged for the host OS only: a .so built here is a
# native binary for this platform, so never cross-label it for another OS) ----
case "$(uname -s)" in
  Darwin) stage "plugins/dlt/build-mac/libbindfettodecoderplugin.so" \
                "libbindfettodecoderplugin-${VERSION}-macos-arm64.so" ;;
  Linux)  stage "plugins/dlt/build/libbindfettodecoderplugin.so" \
                "libbindfettodecoderplugin-${VERSION}-linux.so" ;;
esac

[ "$staged" -gt 0 ] || die "nothing staged — build the components first."
info "Staged ${staged} asset(s) in dist/"

if [ "$UPLOAD" -eq 0 ]; then
  ok "dry run — pass --upload to publish to ${REPO} ${TAG}"
  exit 0
fi

command -v gh >/dev/null 2>&1 || die "gh CLI required for --upload (brew install gh; gh auth login)."
if ! gh release view "$TAG" -R "$REPO" >/dev/null 2>&1; then
  info "Creating release ${TAG}"
  gh release create "$TAG" -R "$REPO" --title "Bindfetto ${VERSION}" --notes "Release ${VERSION}"
fi
info "Uploading assets to ${REPO} ${TAG}"
gh release upload "$TAG" -R "$REPO" "$DIST"/* --clobber
ok "published ${staged} asset(s) to ${TAG}"
