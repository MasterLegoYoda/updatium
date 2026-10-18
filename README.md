# updatium

One tool that tracks and applies updates for software delivered through
heterogeneous channels:

| Channel | Provider id | Example |
|---|---|---|
| GitHub release assets | `github-releases` | FreeCAD AppImages |
| Vendor website links | `html-links` | scraped `.AppImage` downloads |
| Self-managed version managers | `command` | nvm / pyenv |
| Build-from-source repos | `source-build` | llama.cpp (`b1234` tags) |
| Docker image tags | `docker-registry` | Docker Hub, ghcr.io (lobe-chat, open-webui) |
| apt repositories | `apt` | htop (track-only, or apply via sudo) |
| Flatpak remotes | `flatpak` | Firefox on Flathub |

Provider-based architecture: new sources plug in without touching the core.
First version ships **CLI + TUI**; the engine is a reusable library so a future
GUI needs no engine changes.

## Build & install

The repository includes a portable installer for Linux. It builds the release
binary from the checkout and installs it into `~/.local/bin` by default:

```bash
./install.sh
```

Use `--prefix` or `PREFIX` to choose another installation prefix:

```bash
./install.sh --prefix "$HOME/.local"
PREFIX="$HOME/.local" ./install.sh
```

Requirements: Rust 1.90+, `git`, and a C compiler for the bundled SQLite. Linux only
in this MVP (XDG dirs, freedesktop entries). Run `./install.sh --help` for all options.

For a direct development build without installing:

```bash
cargo build --release
```

## Quickstart

### FreeCAD from GitHub releases

```bash
updatium add freecad --provider github-releases   # writes the scaffold, then edit:
```

```toml
id = "freecad"
provider = "github-releases"
repo = "FreeCAD/FreeCAD"
tag_regex = '^v?\d'
asset_regex = '\.AppImage$'

[install]
type = "appimage"

[install.desktop]
name = "FreeCAD"
categories = ["Graphics"]
terminal = false
```

```bash
updatium check --all          # table of ID/PROVIDER/INSTALLED/LATEST/STATUS
updatium update freecad       # download -> verify digest -> install -> .desktop entry
```

The AppImage lands in `~/Applications` (mode 0755) and a
`~/.local/share/applications/updatium.freecad.desktop` launcher is written.

### llama.cpp built from source

```toml
id = "llama.cpp"
provider = "source-build"
repo_url = "https://github.com/ggml-org/llama.cpp"
source_dir = "/home/noesis/Software/llamacpp/llama.cpp"
install_dir = "/home/noesis/Software/llama.cpp"
ref_regex = '^refs/tags/b\d+$'

[build]
steps = [["cmake", "-B", "build", "-DGGML_CUDA=ON"], ["cmake", "--build", "build", "-j"]]
tracked_files = ["CMakeLists.txt", "cmake/**", "ggml/CMakeLists.txt", "ggml/src/**"]
artifacts = ["build/bin/*"]
install_steps = [["cmake", "--install", "build", "--prefix", "{install_dir}", "--strip"]]
timeout_secs = 3600
env = { CMAKE_BUILD_TYPE = "Release" }

[install]
type = "dir"
link_bins = ["llama-cli", "llama-server"]
```

`source_dir` reuses an existing checkout, and `install_dir` with `install_steps` stages a replacement for an external installation directory after a successful build. `{install_dir}` is substituted with a staging path before the old directory is replaced.
`tracked_files` are hashed after every build. If they change upstream,
the next update **stops before building** (`⚠ approval`) and snapshots both
states. Review and clear with:

```bash
updatium approve llamacpp --show-diff
updatium update llamacpp
```

## Runtime layout

| Purpose | Default | Override |
|---|---|---|
| Config (`config.{toml,yaml,yml,json}`, `apps.d/`, `providers.d/`) | `$XDG_CONFIG_HOME/updatium/` | |
| State (`state.db`, `installs/`, `src/`, `script-snapshots/`) | `$XDG_DATA_HOME/updatium/` | |
| Cache (`downloads/`) | `$XDG_CACHE_HOME/updatium/downloads/` | |
| Desktop entries | `~/.local/share/applications/` | |

Set `UPDATIUM_HOME=<dir>` or pass `--home <dir>` to relocate everything under
one root (used by tests and CI; nothing outside it is touched).

## Global `config.toml`

The global config file may be `config.toml`, `config.yaml`/`config.yml`, or
`config.json`; if several exist the first in that order wins.

```toml
max_parallel_checks = 4          # concurrent provider checks (default 4)
download_timeout_secs = 600      # per-download deadline (default 600)
max_download_mb = 2048           # hard mid-stream cap (default 2048)
notify_cmd = []                  # argv; JSON summary piped to stdin; empty = off

[github]
token_cmd = []                   # argv whose stdout line 1 is a PAT;
                                 # falls back to $GITHUB_TOKEN; never stored plaintext
```

## App files `apps.d/<id>.toml`

Filename stem must equal `id` regardless of extension; app files may be
`.toml`, `.yaml`/`.yml`, or `.json`, freely mixed. Unknown keys are startup
errors.

Common keys: `id`, `provider`, `enabled` (default true),
`digest_policy` = `required | prefer | off` (default `prefer`),
optional `allow_insecure_http` (permits plain http for this app), plus one
provider block and an `[install]` table.

```toml
[install]
type = "appimage"                 # appimage | archive | dir | none
target = "~/Applications"         # appimage only; default ~/Applications
bin_glob = ["bin/llama-cli"]      # archive: staged paths published into current/
link_bins = ["llama-cli"]         # names symlinked into ~/.local/bin

[install.desktop]
name = "FreeCAD"
icon = "~/.local/share/updatium/installs/freecad/current/icon.svg"   # optional
categories = ["Graphics"]
terminal = false
```

### Provider fields

**`github-releases`** — `repo` (owner/name, required), `tag_regex`,
`asset_regex`, `include_prereleases` (default false), `api_base`
(default `https://api.github.com`; override for GHES/tests).
Drafts always skipped. Digests come from the asset `digest` field or a sibling
`<asset>.sha256` / `SHA256SUMS` asset.

**`html-links`** — `url`, `link_regex` (named group `href`),
`version_capture` (named group `version`), optional `base_url`,
`digest_suffix`. Deduplicates by version keeping the lexicographically-largest
asset. A missing sidecar is “no digest”, never an error.

**`command`** — `check_cmd` (argv; newline-separated versions on stdout),
optional `apply_cmd` (`{version}` substituted; omit = track-only).

**`source-build`** — `repo_url`, `ref_regex` (matched against
`git ls-remote --tags` output), optional `api_base` to also query GitHub
releases, `[build]`: `steps`, `tracked_files`, `artifacts`, `timeout_secs`
(default 3600), `env`.

**`docker-registry`** — `image`
(`docker.io/<owner>/<name>`, bare `owner/name`, or `<registry>/<host-path>`),
`tag_regex`, required `pull_cmd`, optional `redeploy_cmd`, `api_base`.
`{version}`, `{image}`, `{registry}`, `{repo}` are substitutable in commands.

**`apt`** — `package` (required), optional `apply_cmd` (omit = track-only;
`{package}` and `{version}` substitutable). `check` reads the `Candidate:`
version from `apt-cache policy <package>`; no candidate means no release.
Nothing runs as root implicitly — an unattended `apply_cmd` should start with a
privilege-escalation prefix such as `sudo -n apt-get install --only-upgrade -y`.

**`flatpak`** — `app_id` and `remote` (required), `user` (default true:
`--user` scope; false = `--system`), `version_field`
(`"version" | "branch" | "auto"`, default `auto`: Version column when non-empty
else Branch), optional `branch_regex` (filter on branch; multiple matching
branches become multiple releases and the engine picks the highest via
`cmp_version`), optional `apply_cmd` (omit = track-only; `{app_id}`, `{remote}`,
`{version}` substitutable). `check` lists the remote with
`flatpak remote-ls --user|--system <remote> --app`.

## CLI

```
updatium add <id> --provider <p> [--edit] [--format toml|yaml|json]
updatium list [--json]
updatium check [ids...|--all] [--json]     # default: all enabled
updatium update (<id>|--all) [--yes] [--dry-run] [--force]
updatium approve <id> [--show-diff] [--yes]
updatium history [<id>] [--limit 50] [--json]
updatium plugins [--json]
updatium desktop <id> [--remove]
updatium tui [--print-frame]
updatium completions <shell>
```

Exit codes: `0` success/no-op · `1` error · `2` completed-with-failures or
pending approvals (script-friendly).

Status markers: `● ok ▲ update ⚠ approval ✖ error · unchecked`.

TUI keys: `/` search · `f` cycle status filters · `r` refresh · `u` update selected · `U` bulk update eligible apps · `a` approve · `j/k/arrows/pgup/pgdn` navigate · `d` app history · `e` edit config in `$EDITOR` · `n` create app · `x` delete config only · `g/G` write/remove desktop entry · `p` plugins · `F1` help · `q` quit. Destructive actions ask for confirmation; app-config deletion retains installed files, desktop entry, and database history/state.
`updatium tui --print-frame` renders one frame from DB state and exits — CI-safe.

## Security model

* **Injection firewall** — every variable interpolated into an argv template is
  validated against `^[A-Za-z0-9._+/=@:-]+$`; commands execute via direct argv,
  never a shell.
* **HTTPS enforced** — every fetched URL must be https unless an app explicitly
  sets `allow_insecure_http = true`.
* **Digest verification** — sha256 digests checked per policy before anything is
  installed; mismatch hard-stops with a `verify_failed` event.
* **Build-script gate** — source builds hash `tracked_files`; any change stops
  the pipeline until a human approves (diffable snapshot kept).
* **Archive safety** — zip/tar members are rejected on absolute paths, `..`
  escapes, and ALL symlink/hardlink members; formats are sniffed from magic
  bytes, not extensions.
* **Symlink hygiene** — `~/.local/bin` entries are replaced only when they are
  our own symlinks pointing into this app's install tree; foreign files are
  never overwritten (reported as conflicts).
* **Secrets** — GitHub tokens come from `token_cmd` stdout or env at runtime;
  never persisted.

## External provider protocol (v2)

Any executable named `updatium-provider-*` on `PATH`, or any executable directly
inside `<config>/providers.d/`, becomes available as
`provider = "external:<name>"`.

1. `describe` → stdout JSON `{"protocol":2,"id":"<basename>","verbs":["check"]}`.
   Protocol `1` (no `verbs` field) is still accepted, check-only.
2. `check` — receives the app's remaining config as JSON on **stdin**, replies
   with `{"releases":[...]}` where each release matches
   `{version, published_at, assets:[{name,url,size,digest}]}` (snake_case).

Nonzero exit fails the app's check with the stderr tail; timeout is 60 s.
See `examples/provider-example.sh` for a working bash+python implementation.

External-provider apps are no longer limited to tracking: with an `[install]`
section they flow through the same download → digest-verify → install pipeline
as builtin providers.

## Installer plugins

Installation itself is pluggable. Builtins cover the three native shapes
(`appimage`, `archive`, `dir`); anything else is delegated to an external
installer plugin.

**Discovery** — executables named `updatium-installer-*` on `PATH`, plus any
executable directly inside `<config>/installers.d/` (registered under its exact
file name). Reference them in an app file with:

```toml
[install]
type = "external:copytool"
timeout_secs = 120        # optional; default 600 s per install invocation
dest = "/opt/tool"        # any extra key → passed to the plugin verbatim
```

Extra `[install]` keys are only allowed with `type = "external:…"`, and are
handed to the plugin as its `config` object. Builtins keep strict
unknown-key rejection.

**Contract**

1. `describe` → stdout JSON
   `{"protocol":2,"id":"<basename>","verbs":["install"]}`. The handshake
   requires protocol ≥ 2 **and** the `install` verb **and** `id` matching the
   filename.
2. `install` — receives JSON on **stdin**:

   ```json
   {
     "app": "<id>",
     "version": "<version>",
     "config": { "...": "remaining [install] keys" },
     "paths": {
       "download": "/abs/path/or/null",
       "checkout": "/abs/path/or/null",
       "home": "<UPDATIUM_HOME or null>"
     }
   }
   ```

   It replies on stdout with:

   ```json
   {"actions":["copied by copytool"],"exec":"/abs/exec-or-null","installed":["..."]}
   ```

   `actions` appear verbatim in the update report and history; `exec` becomes
   the desktop-entry target when `[install.desktop]` is configured (if null,
   a warning is recorded instead of a desktop entry).

The plugin runs with `UPDATIUM_HOME` set in its environment when updatium's
data root was relocated (`--home`/`UPDATIUM_HOME`); under default XDG layout no
single root exists and it stays unset.

**Trust model** — installer plugins execute arbitrary code with full access to
updatium's home directories, exactly like provider plugins: dropping an
executable into a discovery location IS the opt-in. Nothing is sandboxed in
this iteration; only install plugins you have reviewed.

Run `updatium plugins` to list every registered provider and installer plugin
(builtin and external) with its source path.

## Writing and loading plugins

### Loading rules

Plugins are plain executables — any language works as long as it can read
stdin, print JSON to stdout, and exit nonzero on failure. Both plugin kinds are
loaded the same way:

* **Scanned fresh at every updatium invocation.** No cache, no restart, no
  registration step: drop a file in place and run `updatium plugins`.
* **Two locations per kind**, checked in this order:

  | Kind | On `PATH` | Config dir |
  |---|---|---|
  | Provider | `updatium-provider-<name>` | `<config>/providers.d/<file>` |
  | Installer | `updatium-installer-<name>` | `<config>/installers.d/<file>` |

* A file qualifies only if it is a regular file directly inside the directory
  (no recursion) with at least one execute bit set.
* Registry keys: a PATH entry registers under what follows the prefix
  (`updatium-installer-copytool` → `external:copytool`); a config-dir file
  registers under its exact file name, extension included
  (`copy.sh` → `external:copy.sh`). Prefer extension-less names so the name
  stays simple — for providers the describe `id` must match it exactly.
* A config-dir file overrides a same-named PATH entry; externals live under
  the `external:` prefix and can never shadow a builtin.
* `--home` / `UPDATIUM_HOME` relocates `providers.d/`/`installers.d/` too, so
  you can develop plugins against an isolated tree without touching your real
  one.

### Walkthrough: minimal installer plugin

```bash
mkdir -p ~/.config/updatium/installers.d
cat > ~/.config/updatium/installers.d/copytool <<'EOF'
#!/usr/bin/env bash
case "$1" in
  describe) echo '{"protocol":2,"id":"copytool","verbs":["install"]}' ;;
  install)
    cfg=$(cat)
    download=$(printf '%s' "$cfg" | python3 -c 'import json,sys; print(json.load(sys.stdin)["paths"]["download"])')
    dest=$(printf '%s' "$cfg" | python3 -c 'import json,sys; print(json.load(sys.stdin)["config"]["dest"])')
    cp "$download" "$dest"
    echo '{"actions":["copied by copytool"],"exec":null,"installed":[]}'
    ;;
  *) echo "unknown verb $1" >&2; exit 9 ;;
esac
EOF
chmod +x ~/.config/updatium/installers.d/copytool
```

Confirm registration, then point an app at it:

```console
$ updatium plugins        # must list `installer external:copytool`
```

```toml
# apps.d/myapp.toml (provider side can be any provider, builtin or external)
[install]
type = "external:copytool"
dest = "/opt/myapp/current"   # arbitrary keys become the plugin's `config`
```

A working end-to-end version of both halves lives in
`tests/installer_plugin.rs`; a provider plugin example is
`examples/provider-example.sh`.

### Debugging

* Registration problems → `updatium plugins`: missing row means wrong location,
  missing exec bit, nested file, or PATH-prefix mismatch.
* Handshake failures surface verbatim in check/update errors: unsupported
  protocol, undeclared verb, or describe `id` not matching the file name.
* Nonzero exits fail with the plugin's stderr tail attached — log there.
* Timeouts: 60 s hard limit for every `describe`/`check` call; installer
  `install` calls get `timeout_secs` (default 600 s). Long builds belong in
  source-build apps, not installer plugins.
* Every install attempt lands in `updatium history <id>` (`install_ok`,
  `verify_failed`, …), so a failed plugin run leaves a trace even when its own
  output is unhelpful.

## Development

```bash
cargo test              # unit + integration suites (fixture-server backed)
bash scripts/e2e.sh     # end-to-end smoke; prints E2E OK
```
