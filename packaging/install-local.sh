#!/usr/bin/env bash
set -euo pipefail
project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
binary=${1:-"$project_dir/target/release/yolocam-linux"}
data_root=${XDG_DATA_HOME:-"$HOME/.local/share"}
application="$HOME/.local/bin/yolocam-linux"
mkdir -p "$HOME/.local/bin"
temporary=$(mktemp "$HOME/.local/bin/.yolocam-linux.XXXXXX")
trap 'rm -f -- "$temporary"' EXIT
install -m755 "$binary" "$temporary"
mv -f -- "$temporary" "$application"
install -Dm644 "$project_dir/packaging/yolocam-linux.svg" "$data_root/icons/hicolor/scalable/apps/yolocam-linux.svg"
install -Dm644 "$project_dir/packaging/yolocam-linux.desktop" "$data_root/applications/yolocam-linux.desktop"
# Desktop launchers do not necessarily inherit the terminal's user-local PATH.
desktop_exec=${application//\\/\\\\}
desktop_exec=${desktop_exec//\"/\\\"}
desktop_exec=${desktop_exec//\$/\\\$}
desktop_exec=${desktop_exec//\`/\\\`}
while IFS= read -r line; do
    if [[ "$line" == Exec=* ]]; then
        printf 'Exec="%s"\n' "$desktop_exec"
    else
        printf '%s\n' "$line"
    fi
done < "$project_dir/packaging/yolocam-linux.desktop" > "$data_root/applications/yolocam-linux.desktop"
if command -v update-desktop-database >/dev/null; then
    update-desktop-database "$data_root/applications"
fi
printf 'Installed YoloCam Linux to %s\n' "$application"
