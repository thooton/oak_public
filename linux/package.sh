#!/usr/bin/env bash
# Builds OAK for Linux as a self-contained folder and tarball, like Zed's:
#
#   target/dist/oak-linux-x86_64/          the folder: run ./oak from it
#   target/dist/oak-linux-x86_64.tar.gz    the same, packed
#
# The binary links only glibc and libgcc_s. Everything else is in it (fonts,
# pictures, the system prompt, TLS, the file dialog through the desktop
# portal), or is loaded at run time from what any desktop has: Mesa's
# OpenGL, X11 or Wayland and libxkbcommon. Nothing needs installing to run it.
#
# To build it you need Rust and a C compiler; on Arch:
#   sudo pacman -S --needed rust gcc pkgconf
set -euo pipefail
cd "$(dirname "$0")/.."

target_dir="${CARGO_TARGET_DIR:-target}"
arch="$(uname -m)"
name="oak-linux-$arch"
out="$target_dir/dist/$name"

cargo build --release
rm -rf "$out"
mkdir -p "$out"
strip -o "$out/oak" "$target_dir/release/oak"
cp linux/install.sh linux/oak.desktop "$out/"
cp assets/oak-icon.png "$out/oak.png"
chmod +x "$out/oak" "$out/install.sh"
cat > "$out/README.txt" <<'EOF'
OAK for Linux

Run it:            ./oak
Add it to the app launcher and to PATH (~/.local/bin/oak):
                   ./install.sh
Remove that again: ./install.sh --uninstall

Nothing else needs installing: it uses OpenGL, X11/Wayland and libxkbcommon,
which every Plasma (or GNOME, ...) desktop has. Settings and conversations
are kept in ~/.config/oak.

Optional, for the agent's tools: a Chromium browser (chromium, google-chrome,
brave or microsoft-edge on PATH) for FETCH render and SCREENSHOT.
EOF
tar -C "$target_dir/dist" -czf "$target_dir/dist/$name.tar.gz" "$name"

echo
echo "Linked libraries (should be only glibc and libgcc_s):"
readelf -d "$out/oak" | grep NEEDED | sed 's/.*\[\(.*\)\]/  \1/'
echo "Needs glibc $(objdump -T "$out/oak" | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1 | cut -d_ -f2) or newer."
echo
echo "Built $out ($(du -h "$out/oak" | cut -f1))"
echo "  and $target_dir/dist/$name.tar.gz"
