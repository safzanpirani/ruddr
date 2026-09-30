#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
bin_dir=${RUDDR_BIN_DIR:-"$HOME/.local/bin"}
data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
install_dir="$data_home/ruddr"

command -v bun >/dev/null 2>&1 || {
  printf '%s\n' "ruddr install: Bun 1.4 or newer is required" >&2
  exit 1
}

bun -e 'const [major, minor] = Bun.version.split(".").map(Number); if (major < 1 || (major === 1 && minor < 4)) process.exit(1)' || {
  printf '%s\n' "ruddr install: Bun 1.4 or newer is required" >&2
  exit 1
}

bun install --cwd "$repo_dir" --frozen-lockfile --ignore-scripts
(
  CDPATH= cd -- "$repo_dir"
  bun test
  bunx tsc -p tsconfig.json --noEmit
)
go build -C "$repo_dir" -o ruddr .

mkdir -p "$bin_dir" "$install_dir/tui" "$install_dir/adapter" "$install_dir/claude" "$install_dir/opencode" "$install_dir/pi" "$install_dir/droid"
binary_tmp=$(mktemp "$bin_dir/.ruddr.XXXXXX")
/bin/cp "$repo_dir/ruddr" "$binary_tmp"
chmod 0755 "$binary_tmp"
/bin/mv "$binary_tmp" "$bin_dir/ruddr"
/bin/cp "$repo_dir/package.json" "$repo_dir/bun.lock" "$install_dir/"
/bin/cp "$repo_dir/tui/"*.ts "$install_dir/tui/"
/bin/cp "$repo_dir/adapter/"*.ts "$install_dir/adapter/"
/bin/cp "$repo_dir/claude/"*.ts "$install_dir/claude/"
/bin/cp "$repo_dir/opencode/"*.ts "$install_dir/opencode/"
/bin/cp "$repo_dir/pi/"*.ts "$install_dir/pi/"
/bin/cp "$repo_dir/droid/"*.ts "$install_dir/droid/"

bun install --cwd "$install_dir" --production --frozen-lockfile --ignore-scripts

test -f "$install_dir/tui/index.ts" || {
  printf '%s\n' "ruddr install: installed TUI entry is missing" >&2
  exit 1
}
test -f "$install_dir/claude/app-server.ts" || {
  printf '%s\n' "ruddr install: installed Claude adapter entry is missing" >&2
  exit 1
}
test -f "$install_dir/opencode/app-server.ts" || {
  printf '%s\n' "ruddr install: installed OpenCode adapter entry is missing" >&2
  exit 1
}
test -f "$install_dir/pi/app-server.ts" || {
  printf '%s\n' "ruddr install: installed Pi adapter entry is missing" >&2
  exit 1
}
test -f "$install_dir/droid/app-server.ts" || {
  printf '%s\n' "ruddr install: installed Droid adapter entry is missing" >&2
  exit 1
}

source_hash=$(shasum -a 256 "$repo_dir/ruddr" | awk '{print $1}')
installed_hash=$(shasum -a 256 "$bin_dir/ruddr" | awk '{print $1}')
test "$source_hash" = "$installed_hash" || {
  printf '%s\n' "ruddr install: installed binary verification failed" >&2
  exit 1
}

"$bin_dir/ruddr" skill install

# Keep the pre-rename command working for existing shells and scripts.
ln -sfn ruddr "$bin_dir/rudder"
printf '%s\n' "installed $bin_dir/ruddr (and the rudder alias)"
printf '%s\n' "installed TUI assets in $install_dir"
