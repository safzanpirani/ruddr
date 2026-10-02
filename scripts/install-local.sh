#!/bin/sh
# Builds Ruddr from this checkout and installs the binary and the delegate
# skill. Ruddr is one native binary; nothing else is installed.
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
bin_dir=${RUDDR_BIN_DIR:-"$HOME/.local/bin"}

command -v cargo >/dev/null 2>&1 || {
  printf '%s\n' "ruddr install: Rust (cargo) is required: https://rustup.rs" >&2
  exit 1
}

# mbx shares compiled crates across checkouts when it is installed.
if command -v mbx >/dev/null 2>&1; then
  cargo_cmd=mbx
else
  cargo_cmd=cargo
fi

(
  CDPATH= cd -- "$repo_dir"
  "$cargo_cmd" test --workspace --locked
  "$cargo_cmd" build --release --locked -p ruddr-cli
)

built="$repo_dir/target/release/ruddr"
test -x "$built" || {
  printf '%s\n' "ruddr install: the release build did not produce $built" >&2
  exit 1
}

mkdir -p "$bin_dir"
binary_tmp=$(mktemp "$bin_dir/.ruddr.XXXXXX")
/bin/cp "$built" "$binary_tmp"
chmod 0755 "$binary_tmp"
/bin/mv "$binary_tmp" "$bin_dir/ruddr"

source_hash=$(shasum -a 256 "$built" | awk '{print $1}')
installed_hash=$(shasum -a 256 "$bin_dir/ruddr" | awk '{print $1}')
test "$source_hash" = "$installed_hash" || {
  printf '%s\n' "ruddr install: installed binary verification failed" >&2
  exit 1
}

"$bin_dir/ruddr" skill install

# Keep the pre-rename command working for existing shells and scripts.
ln -sfn ruddr "$bin_dir/rudder"
printf '%s\n' "installed $bin_dir/ruddr (and the rudder alias)"
