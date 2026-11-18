#!/bin/sh
set -eu

bundle_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
data_dir=${XDG_DATA_HOME:-}
case "$data_dir" in
    /*) ;;
    *) data_dir=$HOME/.local/share ;;
esac
app_dir=$data_dir/unbind/desktop
menu_dir=$data_dir/applications
icon_dir=$data_dir/icons/hicolor/scalable/apps
desktop_id=dev.unbind.Unbind.desktop
binary=$app_dir/unbind-desktop

case "$binary" in
    *"
"*|*"$(printf '\r')"*)
        printf '%s\n' 'The desktop installation path cannot contain a line break.' >&2
        exit 1
        ;;
esac

# Desktop Exec quoting is separate from shell quoting. The launcher receives
# a URL as one argument and never interprets it as a command.
escaped=$(printf '%s' "$binary" | sed \
    -e 's/\\/\\\\\\\\/g' -e 's/["`$]/\\\\&/g' -e 's/%/%%/g')

mkdir -p "$app_dir"
staged_binary=$(mktemp "$app_dir/.unbind-desktop.XXXXXX")
trap 'rm -f -- "$staged_binary"' EXIT
install -m755 "$bundle_dir/unbind-desktop" "$staged_binary"
mv -f -- "$staged_binary" "$binary"
install -Dm644 "$bundle_dir/Unbind.svg" "$icon_dir/dev.unbind.Unbind.svg"
mkdir -p "$menu_dir"
cat >"$menu_dir/$desktop_id" <<EOF
[Desktop Entry]
Type=Application
Name=Unbind
Comment=Share local services over an encrypted peer connection
Exec="$escaped" %u
Icon=dev.unbind.Unbind
Terminal=false
Categories=Network;Utility;
MimeType=x-scheme-handler/unbind;
EOF

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$menu_dir"
fi
if command -v xdg-mime >/dev/null 2>&1; then
    xdg-mime default "$desktop_id" x-scheme-handler/unbind
else
    printf '%s\n' 'Install xdg-utils and rerun this script to enable unbind:// links.' >&2
fi

printf '%s\n' 'Open Unbind from your application menu.'
