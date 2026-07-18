#!/usr/bin/env bash
#
# Bindfetto installer — downloads the latest GitHub release artifacts and
# installs the components you choose.
#
#   ./install.sh                 # interactive menu
#   ./install.sh --all           # install everything applicable to this host
#   ./install.sh --runtime --dlt # pick components non-interactively
#
# Components:
#   runtime  the on-device capture binary   -> adb push to /data/local/tmp (needs a device)
#   app      the control app APK             -> adb install -r          (needs a device)
#   dlt      the DLT Viewer decoder plugin   -> copied into DLT Viewer's plugins dir
#   vscode   the VS Code decode extension    -> code --install-extension
#
# Options:
#   --all                 select every component this OS/host supports
#   --runtime --app --dlt --vscode   select individual components (skips the menu)
#   --tag <tag>           install a specific release tag (default: latest)
#   --dlt-plugin-dir <d>  force the DLT Viewer plugins directory (skip auto-search)
#   --yes                 assume yes for confirmations (non-interactive)
#   -h, --help            show this help

set -euo pipefail

REPO="tortishead/bindfetto"
API="https://api.github.com/repos/${REPO}/releases"

# ---- pretty output ---------------------------------------------------------
if [ -t 1 ]; then
  B=$'\033[1m'; DIM=$'\033[2m'; GRN=$'\033[32m'; YLW=$'\033[33m'; RED=$'\033[31m'; RST=$'\033[0m'
else
  B=""; DIM=""; GRN=""; YLW=""; RED=""; RST=""
fi
info() { printf '%s\n' "${B}==>${RST} $*"; }
ok()   { printf '%s\n' "${GRN}  ok${RST} $*"; }
warn() { printf '%s\n' "${YLW}  !${RST} $*" >&2; }
die()  { printf '%s\n' "${RED}error:${RST} $*" >&2; exit 1; }

usage() { sed -n '2,/^set -euo/p' "$0" | sed 's/^#\{0,1\} \{0,1\}//; $d'; exit 0; }

# ---- args ------------------------------------------------------------------
TAG=""
DLT_PLUGIN_DIR=""
ASSUME_YES=0
SEL_RUNTIME=0 SEL_APP=0 SEL_DLT=0 SEL_VSCODE=0
ANY_FLAG=0

while [ $# -gt 0 ]; do
  case "$1" in
    --all)            SEL_RUNTIME=1; SEL_APP=1; SEL_DLT=1; SEL_VSCODE=1; ANY_FLAG=1 ;;
    --runtime)        SEL_RUNTIME=1; ANY_FLAG=1 ;;
    --app)            SEL_APP=1;     ANY_FLAG=1 ;;
    --dlt)            SEL_DLT=1;     ANY_FLAG=1 ;;
    --vscode)         SEL_VSCODE=1;  ANY_FLAG=1 ;;
    --tag)            TAG="${2:-}"; shift ;;
    --dlt-plugin-dir) DLT_PLUGIN_DIR="${2:-}"; shift ;;
    --yes|-y)         ASSUME_YES=1 ;;
    -h|--help)        usage ;;
    *)                die "unknown option: $1 (see --help)" ;;
  esac
  shift
done

# ---- host detection --------------------------------------------------------
case "$(uname -s)" in
  Darwin) OS="macos" ;;
  Linux)  OS="linux" ;;
  *)      die "unsupported OS: $(uname -s). This installer supports macOS and Linux; Windows is not supported." ;;
esac

command -v curl >/dev/null 2>&1 || die "curl is required."
command -v python3 >/dev/null 2>&1 || die "python3 is required (used to parse the release JSON)."

# ---- fetch release metadata ------------------------------------------------
if [ -n "$TAG" ]; then
  REL_URL="${API}/tags/${TAG}"
else
  REL_URL="${API}/latest"
fi

info "Fetching release metadata from ${DIM}${REPO}${RST}"
REL_JSON="$(curl -fsSL -H 'Accept: application/vnd.github+json' "$REL_URL")" \
  || die "could not fetch release info. Check the network or the tag name."

RELEASE_TAG="$(printf '%s' "$REL_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("tag_name",""))')"
[ -n "$RELEASE_TAG" ] || die "no release found."
ok "Release ${B}${RELEASE_TAG}${RST}"

# asset_url <asset-name> -> prints browser_download_url or empty
asset_url() {
  printf '%s' "$REL_JSON" | python3 -c '
import json,sys
name=sys.argv[1]
data=json.load(sys.stdin)
for a in data.get("assets",[]):
    if a.get("name")==name:
        print(a.get("browser_download_url","")); break
' "$1"
}

# ---- asset name selection (per OS) ----------------------------------------
A_RUNTIME="bindfetto-aarch64-android"
A_APP="bindfetto-app-debug.apk"
if [ "$OS" = "macos" ]; then
  A_DLT="libbindfettodecoderplugin-macos-arm64.so"
else
  A_DLT="libbindfettodecoderplugin.so"
fi
# vsix name carries the version; resolve it from the asset list
A_VSIX="$(printf '%s' "$REL_JSON" | python3 -c '
import json,sys
for a in json.load(sys.stdin).get("assets",[]):
    if a.get("name","").endswith(".vsix"):
        print(a["name"]); break
')"

# ---- interactive menu ------------------------------------------------------
if [ "$ANY_FLAG" -eq 0 ]; then
  if [ ! -t 0 ]; then
    die "no component flags given and no TTY for the menu. Use --all or --runtime/--app/--dlt/--vscode."
  fi
  printf '\n%sSelect components to install%s (space/comma separated numbers, or "a" for all):\n' "$B" "$RST"
  printf '  1) runtime  on-device capture binary   (adb push, needs a device)\n'
  printf '  2) app      control app APK            (adb install, needs a device)\n'
  printf '  3) dlt      DLT Viewer decoder plugin\n'
  printf '  4) vscode   VS Code decode extension\n'
  printf '%sChoice> %s' "$B" "$RST"
  read -r choice
  case "$choice" in
    *a*|*A*) SEL_RUNTIME=1; SEL_APP=1; SEL_DLT=1; SEL_VSCODE=1 ;;
  esac
  case "$choice" in *1*) SEL_RUNTIME=1 ;; esac
  case "$choice" in *2*) SEL_APP=1 ;; esac
  case "$choice" in *3*) SEL_DLT=1 ;; esac
  case "$choice" in *4*) SEL_VSCODE=1 ;; esac
fi

if [ $((SEL_RUNTIME+SEL_APP+SEL_DLT+SEL_VSCODE)) -eq 0 ]; then
  die "nothing selected."
fi

# ---- download helpers ------------------------------------------------------
WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/bindfetto-install.XXXXXX")"
trap 'rm -rf "$WORKDIR"' EXIT

download() { # <asset-name> -> echoes local path
  local name="$1" url dest
  url="$(asset_url "$name")"
  [ -n "$url" ] || { warn "asset not in release: $name"; return 1; }
  dest="$WORKDIR/$name"
  info "Downloading ${DIM}${name}${RST}" >&2
  curl -fsSL -o "$dest" "$url" || { warn "download failed: $name"; return 1; }
  printf '%s' "$dest"
}

# ---- adb device check ------------------------------------------------------
adb_device_ready() {
  command -v adb >/dev/null 2>&1 || { warn "adb not found on PATH."; return 1; }
  local n
  n="$(adb devices | awk 'NR>1 && $2=="device"' | wc -l | tr -d ' ')"
  if [ "$n" -eq 0 ]; then
    warn "no adb device connected (need exactly one authorized device)."
    return 1
  fi
  if [ "$n" -gt 1 ]; then
    warn "multiple adb devices connected; set ANDROID_SERIAL to pick one."
    return 1
  fi
  return 0
}

# ---- DLT plugin dir auto-search --------------------------------------------
find_dlt_plugin_dir() {
  [ -n "$DLT_PLUGIN_DIR" ] && { printf '%s' "$DLT_PLUGIN_DIR"; return 0; }
  local c candidates=()
  if [ "$OS" = "macos" ]; then
    candidates+=(
      "/Applications/DLT Viewer.app/Contents/PlugIns"
      "/Applications/DLT-Viewer.app/Contents/PlugIns"
      "$HOME/Applications/DLT Viewer.app/Contents/PlugIns"
      "$HOME/Applications/DLT-Viewer.app/Contents/PlugIns"
    )
    for c in "${candidates[@]}"; do [ -d "$c" ] && { printf '%s' "$c"; return 0; }; done
    # broad search for a DLT Viewer bundle
    c="$(find /Applications "$HOME/Applications" -maxdepth 2 -iname '*lt*iewer*.app' 2>/dev/null | head -1)"
    [ -n "$c" ] && [ -d "$c/Contents/PlugIns" ] && { printf '%s' "$c/Contents/PlugIns"; return 0; }
  else
    candidates+=(
      "/usr/share/dlt-viewer/plugins"
      "/usr/lib/dlt-viewer/plugins"
      "/usr/lib/x86_64-linux-gnu/dlt-viewer/plugins"
      "/usr/local/share/dlt-viewer/plugins"
      "$HOME/.local/share/dlt-viewer/plugins"
    )
    for c in "${candidates[@]}"; do [ -d "$c" ] && { printf '%s' "$c"; return 0; }; done
    # derive from the dlt-viewer binary location
    if command -v dlt-viewer >/dev/null 2>&1; then
      local base; base="$(dirname "$(dirname "$(readlink -f "$(command -v dlt-viewer)")")")"
      for c in "$base/lib/dlt-viewer/plugins" "$base/share/dlt-viewer/plugins" "$base/plugins"; do
        [ -d "$c" ] && { printf '%s' "$c"; return 0; }
      done
    fi
  fi
  return 1
}

# ---------------------------------------------------------------------------
# Install: runtime binary
# ---------------------------------------------------------------------------
if [ "$SEL_RUNTIME" -eq 1 ]; then
  info "${B}runtime${RST} — on-device capture binary"
  if adb_device_ready; then
    if f="$(download "$A_RUNTIME")"; then
      adb push "$f" /data/local/tmp/bindfetto
      adb shell chmod 755 /data/local/tmp/bindfetto
      ok "pushed to /data/local/tmp/bindfetto (run as root: adb shell /data/local/tmp/bindfetto)"
    fi
  else
    warn "skipping runtime — no usable adb device."
  fi
fi

# ---------------------------------------------------------------------------
# Install: control app APK
# ---------------------------------------------------------------------------
if [ "$SEL_APP" -eq 1 ]; then
  info "${B}app${RST} — control app APK"
  if adb_device_ready; then
    if f="$(download "$A_APP")"; then
      adb install -r "$f"
      ok "installed com.bindfetto.control"
    fi
  else
    warn "skipping app — no usable adb device."
  fi
fi

# ---------------------------------------------------------------------------
# Install: DLT Viewer plugin
# ---------------------------------------------------------------------------
if [ "$SEL_DLT" -eq 1 ]; then
  info "${B}dlt${RST} — DLT Viewer decoder plugin"
  if dir="$(find_dlt_plugin_dir)"; then
    ok "DLT Viewer plugins dir: ${DIM}${dir}${RST}"
  else
    warn "could not locate a DLT Viewer plugins directory."
    if [ -t 0 ] && [ "$ASSUME_YES" -eq 0 ]; then
      printf '%sEnter plugins dir (blank to skip)> %s' "$B" "$RST"; read -r dir
    fi
  fi
  if [ -n "${dir:-}" ]; then
    if f="$(download "$A_DLT")"; then
      # DLT Viewer expects the plugin named libbindfettodecoderplugin.so regardless of host tag
      target="$dir/libbindfettodecoderplugin.so"
      if [ -w "$dir" ]; then
        cp "$f" "$target"
      else
        warn "no write access to $dir — using sudo."
        sudo cp "$f" "$target"
      fi
      ok "installed plugin -> $target (enable it in DLT Viewer: Settings -> Plugins, set config to your catalog.json)"
    fi
  else
    warn "skipping dlt — no plugins directory."
  fi
fi

# ---------------------------------------------------------------------------
# Install: VS Code extension
# ---------------------------------------------------------------------------
if [ "$SEL_VSCODE" -eq 1 ]; then
  info "${B}vscode${RST} — decode extension"
  if [ -z "$A_VSIX" ]; then
    warn "no .vsix asset in this release — skipping."
  elif ! command -v code >/dev/null 2>&1; then
    warn "'code' CLI not found on PATH. In VS Code run 'Shell Command: Install code command in PATH', then re-run with --vscode."
  else
    if f="$(download "$A_VSIX")"; then
      code --install-extension "$f" --force
      ok "installed VS Code extension"
    fi
  fi
fi

info "${GRN}Done.${RST}"
