#!/usr/bin/env bash
set -euo pipefail
# Piped from curl (`curl -fsSL …/install.sh | bash`): no checkout here, so fetch
# the source and run its installer from there.
if [[ -z ${BASH_SOURCE[0]:-} ]]; then
  command -v git >/dev/null || { echo "Git is needed to install nyx-transfer." >&2; exit 1; }
  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT
  git clone -q --depth 1 https://github.com/design-nexus/nyx-transfer.git "$work/src"
  bash "$work/src/install.sh" "$@"
  exit
fi
cd -- "$(dirname -- "$0")"
# A release archive (.prebuilt) brings its binaries.
[[ -f .prebuilt ]] || CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build --release --locked -p nyx-transfer -p nyx-transferd
install -Dm755 target/release/nyx-transfer "$HOME/.local/bin/nyx-transfer.new"
mv -f "$HOME/.local/bin/nyx-transfer.new" "$HOME/.local/bin/nyx-transfer"
install -Dm755 target/release/nyx-transferd "$HOME/.local/bin/nyx-transferd.new"
mv -f "$HOME/.local/bin/nyx-transferd.new" "$HOME/.local/bin/nyx-transferd"
install -Dm644 data/io.github.design_nexus.nyx.Transfer.desktop "${XDG_DATA_HOME:-$HOME/.local/share}/applications/io.github.design_nexus.nyx.Transfer.desktop"
install -Dm644 data/icons/io.github.design_nexus.nyx.Transfer.svg "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps/io.github.design_nexus.nyx.Transfer.svg"
install -Dm644 data/nyx-transfer.service "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/nyx-transfer.service"
install -Dm644 data/io.github.design_nexus.nyx.Transfer1.service "${XDG_DATA_HOME:-$HOME/.local/share}/dbus-1/services/io.github.design_nexus.nyx.Transfer1.service"
systemctl --user daemon-reload
systemctl --user enable --now nyx-transfer.service
