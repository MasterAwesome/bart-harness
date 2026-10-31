#!/bin/sh
set -eu

release=https://github.com/MasterAwesome/bart-harness/releases/latest/download

case "$(uname -s)-$(uname -m)" in
Linux-x86_64) asset=bart-linux-x86_64 ;;
Linux-aarch64) asset=bart-linux-aarch64 ;;
Darwin-arm64) asset=bart-darwin-arm64 ;;
Darwin-x86_64) asset=bart-darwin-x86_64 ;;
*)
	echo 'Bart supports Linux and macOS on x86_64 and ARM64.' >&2
	exit 1
	;;
esac

dir=$HOME/.local/bin
mkdir -p "$dir"
tmp=$(mktemp -d "$dir/.bart-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM
cd "$tmp"

curl -fsSL -o "$asset" "$release/$asset"
curl -fsSL -o SHA256SUMS "$release/SHA256SUMS"
sha256() {
	if command -v sha256sum >/dev/null; then
		sha256sum "$@"
	else
		shasum -a 256 "$@"
	fi
}
if ! grep " $asset\$" SHA256SUMS >expected || ! sha256 -c expected >/dev/null; then
	echo 'Checksum verification failed; Bart was not installed.' >&2
	exit 1
fi

chmod 755 "$asset"
mv -f "$asset" "$dir/bart"
echo "Installed Bart to $dir/bart"
case ":$PATH:" in
*":$dir:"*) echo 'Run bart to get started.' ;;
*)
	echo 'Add this to your shell configuration, then open a new terminal and run bart:'
	echo "export PATH=\"\$HOME/.local/bin:\$PATH\""
	;;
esac
