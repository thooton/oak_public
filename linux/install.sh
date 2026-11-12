#!/usr/bin/env bash
# Puts OAK in the app launcher and on PATH for this user, running it from
# this folder (nothing is copied, no root needed):
#   ~/.local/bin/oak                            -> ./oak
#   ~/.local/share/applications/oak.desktop
#   ~/.local/share/icons/hicolor/256x256/apps/oak.png  (OAK's icon)
# (OAK also puts its icon there itself when it starts.)
# ./install.sh --uninstall takes them away again.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
bin="$HOME/.local/bin/oak"
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
desktop="$apps/oak.desktop"
icons="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"
icon="$icons/256x256/apps/oak.png"

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$bin" "$desktop" "$icon" "$icons/scalable/apps/oak.svg"
    command -v update-desktop-database >/dev/null && update-desktop-database "$apps" 2>/dev/null || true
    echo "Removed $bin, $desktop and the icon."
    exit 0
fi

mkdir -p "$(dirname "$bin")" "$apps" "$(dirname "$icon")"
ln -sf "$here/oak" "$bin"
[ -f "$here/oak.png" ] && cp "$here/oak.png" "$icon"
sed "s|^Exec=.*|Exec=\"$here/oak\"|" "$here/oak.desktop" > "$desktop"
command -v update-desktop-database >/dev/null && update-desktop-database "$apps" 2>/dev/null || true
echo "OAK is in the app launcher, and on PATH as $bin."
case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) echo "(~/.local/bin isn't on your PATH; the launcher entry works anyway.)" ;;
esac
