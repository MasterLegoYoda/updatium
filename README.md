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

Provider-based architecture: new sources plug in without touching the core.
First version ships **CLI + TUI**; the engine is a reusable library so a future
GUI needs no engine changes.

## Build & install

```bash
cargo build --release
install -Dm755 target/release/updatium ~/.local/bin/updatium
```

Requirements: Rust 1.90+, `git`, a C compiler for the bundled SQLite. Linux only
in this MVP (XDG dirs, freedesktop entries).

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
id = "llamacpp"
provider = "source-build"
repo_url = "https://github.com/ggml-org/llama.cpp"
ref_regex = '^refs/tags/b\d+$'

[build]
steps = [["cmake", "-B", "build"], ["cmake", "--build", "build", "-j"]]
tracked_files = ["CMakeLists.txt", "cmake/**", "scripts/*.sh"]
artifacts = ["build/bin/*"]
timeout_secs = 3600
env = { CMAKE_BUILD_TYPE = "Release" }

[install]
type = "dir"
link_bins = ["llama-cli"]
```

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
| Config (`config.toml`, `apps.d/`, `providers.d/`) | `$XDG_CONFIG_HOME/updatium/` | |
| State (`state.db`, `installs/`, `src/`, `script-snapshots/`) | `$XDG_DATA_HOME/updatium/` | |
| Cache (`downloads/`) | `$XDG_CACHE_HOME/updatium/downloads/` | |
| Desktop entries | `~/.local/share/applications/` | |

Set `UPDATIUM_HOME=<dir>` or pass `--home <dir>` to relocate everything under
one root (used by tests and CI; nothing outside it is touched).

## Global `config.toml`

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

Filename stem must equal `id`. Unknown keys are startup errors.

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

## CLI

```
updatium add <id> --provider <p> [--edit]
updatium list [--json]
updatium check [ids...|--all] [--json]     # default: all enabled
updatium update (<id>|--all) [--yes] [--dry-run] [--force]
updatium approve <id> [--show-diff] [--yes]
updatium history [<id>] [--limit 50] [--json]
updatium desktop <id> [--remove]
updatium tui [--print-frame]
updatium completions <shell>
```

Exit codes: `0` success/no-op · `1` error · `2` completed-with-failures or
pending approvals (script-friendly).

Status markers: `● ok ▲ update ⚠ approval ✖ error · unchecked`.

TUI keys: `r` refresh · `u` update selected · `a` approve selected ·
`j/k/arrows/pgup/pgdn` navigate · `d` detail pane · `q` quit.
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

## External provider protocol (v1)

Any executable named `updatium-provider-*` on `PATH`, or any executable directly
inside `<config>/providers.d/`, becomes available as
`provider = "external:<name>"`.

1. `describe` → stdout JSON `{"protocol":1,"id":"<basename>"}`.
2. `check` — receives the app's remaining config as JSON on **stdin**, replies
   with `{"releases":[...]}` where each release matches
   `{version, published_at, assets:[{name,url,size,digest}]}` (snake_case).

Nonzero exit fails the app's check with the stderr tail; timeout is 60 s.
See `examples/provider-example.sh` for a working bash+python implementation.

## Development

```bash
cargo test              # unit + integration suites (fixture-server backed)
bash scripts/e2e.sh     # end-to-end smoke; prints E2E OK
```
