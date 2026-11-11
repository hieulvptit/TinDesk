#!/usr/bin/env bash
# Verify setup leaves existing formulae alone; still probe the actual runtime.
set -euo pipefail
cd "$(dirname "$0")/.."
fixture=$(mktemp -d /tmp/binachicken-media-setup.XXXXXX)
trap 'rm -rf "$fixture"' EXIT
export BINACHICKEN_SETUP_BREW_LOG="$fixture/brew.log"
export BINACHICKEN_SETUP_BREW_PREFIX="$(brew --prefix)"
cat > "$fixture/brew" <<'BREW'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$BINACHICKEN_SETUP_BREW_LOG"
[[ "${HOMEBREW_NO_AUTO_UPDATE:-}" == 1 && "${HOMEBREW_NO_INSTALL_UPGRADE:-}" == 1 ]] || exit 90
case "$1" in
  list) exit 0 ;;
  --prefix) echo "$BINACHICKEN_SETUP_BREW_PREFIX" ;;
  *) echo 'Unexpected mutating brew command' >&2; exit 91 ;;
esac
BREW
chmod +x "$fixture/brew"
PATH="$fixture:$PATH" scripts/setup-macos-media.sh
[[ $(rg -c '^list --versions' "$BINACHICKEN_SETUP_BREW_LOG") == 3 ]]
! rg '^(install|upgrade|update)' "$BINACHICKEN_SETUP_BREW_LOG"
echo 'Setup preserved all existing formulae: PASS'
