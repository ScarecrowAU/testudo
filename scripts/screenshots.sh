#!/usr/bin/env bash
# Retake the README screenshots (docs/screenshots/*.png) from examples/acme-shop.
#
#   scripts/screenshots.sh [shot...]      # shots: focus compare history live unit-tests
#
# Linux with KDE Plasma 6 only: each shot runs Testudo in its own headless
# KWin session (kwin_wayland --virtual) and captures the window with Spectacle,
# so nothing appears on your desktop.
#
# Everything runs under a throwaway HOME holding a copy of the example at
# ~/projects/acme-shop, so the shots show no real paths, history or git branch.
# The example needs its packages first:
#   cd examples/acme-shop && npm install && npx playwright install chromium-headless-shell
#
# Environment:
#   TESTUDO_BIN, TESTUDO_CLI   binaries to use (default: a fresh debug build)
#   LIVE_WAIT, UNIT_WAIT       seconds before the live/unit-tests captures
#                              (default 2.6 and 4; tune them on slower machines)
#   OUT                        where the PNGs go (default docs/screenshots)
#   KEEP=1                     keep the throwaway HOME and logs afterwards
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
example=$repo/examples/acme-shop
out=${OUT:-$repo/docs/screenshots}
shots=("$@")
[ ${#shots[@]} -gt 0 ] || shots=(focus compare history live unit-tests)

for tool in kwin_wayland spectacle dbus-run-session; do
  command -v "$tool" >/dev/null || { echo "screenshots: $tool not found (needs KDE Plasma 6)" >&2; exit 1; }
done
[ -d "$example/node_modules" ] || { echo "screenshots: run npm install in examples/acme-shop first" >&2; exit 1; }

if [ -z "${TESTUDO_BIN:-}" ] || [ -z "${TESTUDO_CLI:-}" ]; then
  (cd "$repo" && cargo build -q -p testudo-ui -p testudo-cli)
fi
export TESTUDO_BIN=${TESTUDO_BIN:-$repo/target/debug/testudo}
TESTUDO_CLI=${TESTUDO_CLI:-$repo/target/debug/testudo-cli}

# Browsers stay in the real cache; everything else lives in the throwaway home.
export PLAYWRIGHT_BROWSERS_PATH=${PLAYWRIGHT_BROWSERS_PATH:-$HOME/.cache/ms-playwright}
work=$(mktemp -d "${TMPDIR:-/tmp}/testudo-shots.XXXXXX")
if [ "${KEEP:-}" = 1 ]; then
  echo "screenshots: keeping $work"
else
  trap 'rm -rf "$work"' EXIT
fi
export HOME=$work/home
unset TESTUDO_DATA_DIR TESTUDO_CONFIG_DIR XDG_DATA_HOME XDG_CONFIG_HOME XDG_CACHE_HOME
mkdir -p "$HOME/projects" "$out"
cp -a "$example" "$HOME/projects/"
rm -rf "$HOME/projects/acme-shop/test-results"
cd "$HOME/projects"

# One shot: start Testudo inside a fresh virtual KWin session, wait, capture
# the active window. Runs as the session's only client, so it exits with it.
cat >"$work/shot.sh" <<'EOF'
#!/usr/bin/env bash
png=$1 wait=$2
shift 2
"$TESTUDO_BIN" "$@" >"$png.log" 2>&1 &
app=$!
sleep "$wait"
spectacle --background --nonotify --activewindow --no-shadow --output "$png" >>"$png.log" 2>&1
sleep 1
kill "$app" 2>/dev/null
wait "$app" 2>/dev/null
exit 0
EOF
chmod +x "$work/shot.sh"

capture() {
  local name=$1 wait=$2
  shift 2
  local png=$work/$name.png
  # KWin splits this command line on spaces; none of the args contain one.
  timeout 120 dbus-run-session -- kwin_wayland --virtual --width 1600 --height 1000 --no-lockscreen \
    --exit-with-session "$work/shot.sh $png $wait $*" >"$work/$name.kwin.log" 2>&1 || true
  if [ -s "$png" ]; then
    cp "$png" "$out/$name.png"
    echo "screenshots: $name.png"
  else
    echo "screenshots: $name failed, see $work/$name.png.log (rerun with KEEP=1)" >&2
    return 1
  fi
}

needs_history=0
for s in "${shots[@]}"; do
  case $s in focus | compare | history) needs_history=1 ;; esac
done

# The replay shots need some history. Record it before any live shot: a live
# capture is killed mid-run and would be recorded as Interrupted.
if [ $needs_history = 1 ]; then
  echo "screenshots: recording 5 runs of acme-shop"
  for _ in 1 2 3 4 5; do
    "$TESTUDO_CLI" run "$HOME/projects/acme-shop" --quiet >/dev/null 2>&1
  done
  run=$("$TESTUDO_CLI" history | awk '/Playwright/ { print $1; exit }')
  # The checkout test's page records the most frames.
  page=$("$TESTUDO_CLI" show "$run" | awk '/frames=/ { split($3, f, "="); if (f[2] + 0 > max) { max = f[2] + 0; id = $1 } } END { print id }')
fi

dir='~/projects/acme-shop' # shown as typed, so keep the tilde
status=0
for s in "${shots[@]}"; do
  case $s in
    focus) capture focus 8 "$dir" --replay "$run" --at 5 --focus "$page" || status=1 ;;
    compare)
      TESTUDO_DEBUG_COMPARE=1 TESTUDO_DEBUG_COMPARE_COUNT=3 \
        capture compare 9 "$dir" --replay "$run" --at 5 --focus "$page" || status=1
      ;;
    history) capture history 8 "$dir" --replay "$run" --at 2.2 || status=1 ;;
    live) capture live "${LIVE_WAIT:-2.6}" "$dir" --run all || status=1 ;;
    unit-tests) capture unit-tests "${UNIT_WAIT:-4}" "$dir" --run vitest || status=1 ;;
    *) echo "screenshots: unknown shot $s" >&2; status=1 ;;
  esac
done
exit $status
