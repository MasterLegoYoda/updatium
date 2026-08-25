#!/usr/bin/env bash
# End-to-end smoke for updatium. Runs entirely under a relocated UPDATIUM_HOME.
# Prints "E2E OK" and exits 0 on success; exits nonzero on the first failure.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/updatium"
FIXTURE="$ROOT/target/debug/examples/fixture-server"

command -v "$BIN" >/dev/null || { echo "binary missing — run cargo build" >&2; exit 1; }

export UPDATIUM_HOME="$(mktemp -d)"
echo "UPDATIUM_HOME=$UPDATIUM_HOME"

cleanup() {
    [[ -n "${FIXTURE_PID:-}" ]] && kill "$FIXTURE_PID" 2>/dev/null || true
}
trap cleanup EXIT

# --- start fixture server ---------------------------------------------------
"$FIXTURE" > /tmp/updatium-e2e-fixture.log 2>&1 &
FIXTURE_PID=$!
for _ in $(seq 1 50); do
    BASE=$(sed -n 's/^BASE_URL=//p' /tmp/updatium-e2e-fixture.log | head -1)
    [[ -n "$BASE" ]] && break
    sleep 0.1
done
# Wait until the server actually accepts connections.
if command -v curl >/dev/null; then
    for _ in $(seq 1 50); do
        if curl -fsS "$BASE/site/" >/dev/null 2>&1; then break; fi
        sleep 0.1
    done
fi
[[ -n "${BASE:-}" ]] || { echo "fixture did not report BASE_URL" >&2; exit 1; }
echo "fixture: $BASE"

expect_exit() { # expect_exit <code> <cmd...>
    local want=$1; shift
    set +e
    "$@" >/tmp/updatium-e2e-out.log 2>&1
    local got=$?
    set -e
    if [[ "$got" != "$want" ]]; then
        echo "FAIL: expected exit $want, got $got: $*" >&2
        cat /tmp/updatium-e2e-out.log >&2
        exit 1
    fi
}

# --- 1. scaffold github-releases app ---------------------------------------
"$BIN" add demo --provider github-releases
DEMO_TOML="$UPDATIUM_HOME/config/apps.d/demo.toml"
python3 - "$BASE" "$DEMO_TOML" <<'EOF'
import sys, pathlib
base, p = sys.argv[1], pathlib.Path(sys.argv[2])
t = p.read_text()
t = t.replace('repo = "owner/name"              # required: owner/name',
              'repo = "demo/app"\nallow_insecure_http = true\ndigest_policy = "required"')
t = t.replace("\n[install]", f'\napi_base = "{base}/gh"\n\n[install]')
p.write_text(t)
EOF
test -f "$DEMO_TOML"

# --- 2. check --all --json --------------------------------------------------
CHECK_JSON=$("$BIN" check --all --json)
echo "$CHECK_JSON" | grep -q '"latest": "2.1.0"' || { echo "FAIL check json:"; echo "$CHECK_JSON"; exit 1; }
echo "$CHECK_JSON" | grep -q '"app": "demo"' || { echo "FAIL app missing:"; echo "$CHECK_JSON"; exit 1; }

# --- 3. dry run mutates nothing ---------------------------------------------
"$BIN" update demo --dry-run | grep -q "would download"
grep -q demo "$UPDATIUM_HOME/state/state.db"  # synced row exists...
"$BIN" list --json | grep -q '"installed": null'

# --- 4. real install ----------------------------------------------------------
"$BIN" update demo --yes | grep -qi "downloaded"
APPIMG="$UPDATIUM_HOME/state/Applications/freecad-demo-2.1.0-x86_64.AppImage"
test -f "$APPIMG"
test -x "$APPIMG"
test -f "$UPDATIUM_HOME/state/applications/updatium.demo.desktop"
"$BIN" list --json | grep -q '"installed": "2.1.0"'
"$BIN" history demo | grep -q download_done
"$BIN" history demo | grep -q install_ok

# re-update is a no-op; forced run succeeds again
expect_exit 0 "$BIN" update demo --yes

# --- 5. digest tamper hard-stops --------------------------------------------
"$BIN" add poison --provider html-links >/dev/null
POISON_TOML="$UPDATIUM_HOME/config/apps.d/poison.toml"
python3 - "$BASE" "$POISON_TOML" <<'EOF'
import sys, pathlib
base, p = sys.argv[1], pathlib.Path(sys.argv[2])
p.write_text(f'''
id = "poison"
provider = "html-links"
url = "{base}/site/"
allow_insecure_http = true
link_regex = 'href="(?P<href>[^"]+\\.AppImage)"'
version_capture = '(?P<version>\\d+\\.\\d+(\\.\\d+)?)'
digest_suffix = ".sha256"
digest_policy = "required"

[install]
type = "none"
''')
EOF
expect_exit 2 "$BIN" update poison --yes
"$BIN" history poison | grep -q verify_failed
"$BIN" list --json | python3 -c \
  'import json,sys; rows=json.load(sys.stdin); assert all(r["id"]!="poison" or r["installed"] is None for r in rows)'

# --- 6. source-build approval gate -------------------------------------------
REPO_DIR="$(mktemp -d)"
git -C "$REPO_DIR" init -q
echo "build me v1" > "$REPO_DIR/README.md"
git -C "$REPO_DIR" add .
GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t \
    git -C "$REPO_DIR" commit -qm one
git -C "$REPO_DIR" tag b2000

cat > "$UPDATIUM_HOME/config/apps.d/srcdemo.toml" <<EOF
id = "srcdemo"
provider = "source-build"
repo_url = "$REPO_DIR"
ref_regex = '^refs/tags/b\d+\$'

[build]
steps = [["cp", "README.md", "out.txt"]]
tracked_files = ["README.md"]
artifacts = ["out.txt"]

[install]
type = "dir"
EOF

"$BIN" update srcdemo --yes | grep -q "published"
test "$(cat "$UPDATIUM_HOME/state/installs/srcdemo/current/out.txt")" = "build me v1"

# scripts change upstream -> gate stops with exit 2
echo "build me EVIL v2" > "$REPO_DIR/README.md"
git -C "$REPO_DIR" commit -qam evil
git -C "$REPO_DIR" tag b3000
expect_exit 2 "$BIN" update srcdemo --yes
"$BIN" history srcdemo | grep -q script_changed
test "$(cat "$UPDATIUM_HOME/state/installs/srcdemo/current/out.txt")" = "build me v1"

DIFF_OUT=$("$BIN" approve srcdemo --show-diff --yes)
echo "$DIFF_OUT" | grep -q "EVIL"
"$BIN" approve srcdemo --yes >/dev/null
"$BIN" update srcdemo --yes | grep -q "published"
test "$(cat "$UPDATIUM_HOME/state/installs/srcdemo/current/out.txt")" = "build me EVIL v2"

# --- 7. TUI frame (CI-verifiable) ---------------------------------------------
"$BIN" add older --provider github-releases >/dev/null
OLDER_TOML="$UPDATIUM_HOME/config/apps.d/older.toml"
python3 - "$BASE" "$OLDER_TOML" <<'EOF'
import sys, pathlib
base, p = sys.argv[1], pathlib.Path(sys.argv[2])
t = p.read_text().replace('repo = "owner/name"              # required: owner/name',
                          'repo = "demo/app"\nallow_insecure_http = true')
t = t.replace("tag_regex = '^v?\\d'", "tag_regex = '^v1\\.8'")
t = t.replace("\n[install]", f'\napi_base = "{base}/gh"\n\n[install]')
p.write_text(t)
EOF
# Install the pinned old release, then widen the filter so an update appears.
"$BIN" check --all >/dev/null
"$BIN" update older --yes >/dev/null
python3 - "$OLDER_TOML" <<'PYW'
import sys, pathlib
p = pathlib.Path(sys.argv[1])
t = p.read_text().replace("tag_regex = '^v1\\.8'", "tag_regex = '^v?\\d'")
p.write_text(t)
PYW
"$BIN" check --all >/dev/null
FRAME=$("$BIN" tui --print-frame)
echo "$FRAME" | grep -q demo
echo "$FRAME" | grep -qE '(▲|update)'
echo "--- tui frame ---"
echo "$FRAME"
echo "----------------"

# --- 8. yaml/json app files and scaffolds ------------------------------------
cat > "$UPDATIUM_HOME/config/apps.d/ydemo.yaml" <<EOF
id: ydemo
provider: html-links
url: "$BASE/site/"
allow_insecure_http: true
link_regex: 'href="(?P<href>[^"]+\\.AppImage)"'
version_capture: '(?P<version>\\d+\\.\\d+(\\.\\d+)?)'
digest_suffix: ".sha256"
digest_policy: prefer

install:
  type: none
EOF
# Loader picks up a .yaml app alongside the .toml ones.
YCHECK=$("$BIN" check ydemo --json)
echo "$YCHECK" | grep -q '"app": "ydemo"' || { echo "FAIL yaml app check:"; echo "$YCHECK"; exit 1; }

"$BIN" add yadd --provider github-releases --format yaml >/dev/null
test -f "$UPDATIUM_HOME/config/apps.d/yadd.yaml"
grep -q '^provider: github-releases' "$UPDATIUM_HOME/config/apps.d/yadd.yaml"
"$BIN" add yj --provider command --format json >/dev/null
test -f "$UPDATIUM_HOME/config/apps.d/yj.json"
python3 -c 'import json,sys; json.load(open(sys.argv[1]))' \
    "$UPDATIUM_HOME/config/apps.d/yj.json" || { echo "FAIL invalid json scaffold"; exit 1; }

echo "E2E OK"
