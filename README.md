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

Providers discover releases; installers publish the selected artifacts. External
providers and installer plugins use the same engine lifecycle as their builtin
counterparts. Builtin command, package-manager, Docker, and source-build flows
still have explicit engine dispatch because their apply operations differ.
The engine is also a reusable library.

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
installed_version_cmd = ["sh", "-c", "~/Software/llama.cpp/llama-cli --version 2>&1 | sed -n 's/.*build \\([0-9]*\\).*/b\\1/p'"]

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

Archive and source-build publication match artifact paths before copying them;
artifact contents are not retained in memory for selection. Tracked build
scripts are read separately for integrity hashing and review snapshots.
ZIP and tar files retain their stored Unix permission bits when extracted and
published, including executable bits.

Managed archive/source directories and external `install_dir` replacements use
an atomic directory exchange on Linux. Before the exchange, the previous
installation remains active; after it, the new installation is active and the
previous one remains in a sibling `.updatium-staging-*` directory until cleanup.
An interrupted process needs no rollback to keep an installation available.
Archive extraction and publication use independently owned staging directories;
normal completion removes them, but a crash can leave them behind. Older
`.staging-*` directories are not automatically removed. Before removing any
abandoned stage, confirm that no process is using it and inspect its contents;
an exchanged stage may contain the previous installation. If the filesystem does
not support atomic exchange, replacement fails without removing the existing
installation. AppImage replacement at the same filename uses an atomic file
rename, but does not retain the old file for rollback after success; a changed
filename leaves the old file in place. Command, apt, Flatpak, and Docker apply
operations, external installer plugins, and source-build install commands control
their own side effects; Updatium cannot roll those effects back. Source-build
`install_dir` publication is protected only after those commands finish staging.

These guarantees cover process interruption, not power-loss durability. Before
running an update, Updatium records a pending operation; after the file cutover
it records the installed version and `install_ok` together in one database
transaction, before attempting desktop or symlink integration. A crash between
cutover and that transaction can leave the recorded version stale; a crash
afterward can leave integration incomplete. A reported failure after a build,
installer, or apply operation starts also retains the pending marker because the
operation may already have changed files. On the next `list` or `check`, the
pending update is reported as incomplete or still running. A normal `update`
refuses to guess what happened. Inspect the actual installation and history,
then use `updatium update <id> --force` to retry the current latest release;
the marker clears after a successful retry. Retrying user-supplied commands or
external plugins can repeat side effects, so inspect those before forcing.

Updates and approvals for the same app ID under the same Updatium home use a
nonblocking per-app lock; a concurrent operation is refused. This protects the
managed checkout, staging, download use, and installed-state writes of those
operations. Download caches are isolated by app ID. Locks are advisory and do
not protect files from unrelated programs, different Updatium homes, or different
app IDs configured to share a checkout or installation target. Avoid sharing
those paths across concurrently updated apps.

Cancelling a running Updatium command future requests termination of its direct
child. On timeout, Updatium also waits up to one second for the child to be
reaped; cleanup after that is best effort. Descendants are not tracked or killed
and can continue modifying files after the app lock is released. Process crash,
forced termination, and detached processes can leave even the direct command or
plugin running. Before forcing a retry or removing staging, partial downloads,
or an incomplete source checkout, inspect for and stop surviving work; preserve
local checkout changes. An interrupted clone may leave a non-git directory that
must be inspected and moved or removed manually before retrying. Never delete
lock files to stop a running operation.

The installer or apply operation completing is reported as `installed`; the
version and `install_ok` event are then committed together (`recorded` in JSON).
If that commit fails, the installation is retained and the pending marker
requires inspection and a forced retry. Desktop entries and binary symlinks are
separate integration work: failures leave the installed files and recorded
version in place, emit `integration_failed` events, and return exit code 2.
Update JSON reports contain `installed`, `recorded`, and `integration_failures`;
the latter also reports post-install bookkeeping errors. `failed` describes an
installation failure, not a rollback. To retry integration after fixing its
cause, use `--force`, which reruns the installer as well. Warnings unrelated to
integration do not by themselves change the exit code.

There is no automatic integration-only retry. The database records the version
and, for supported install shapes, a target path, but not the exact published
files, plugin-provided executable hint, or integration configuration used by
the successful installation. Reconstructing these from current configuration
could target a different executable or installation. Inspect and repair the
desktop entry or links manually, or explicitly rerun the installer with `--force`
after accepting its side effects.

Before using unattended bulk updates, test each actual configured apply command,
build/install step, and installer plugin in an isolated representative environment,
including partial failure and retry. Fixture tests cannot establish that a
user's commands or plugins are idempotent, or that external side effects can be
rolled back.

When Updatium has no recorded install, it can infer a source-build version from
the checkout tag if the configured install target exists. A checkout is not
proof that the installed binaries match it. `tracked_files` are hashed after
every build. If they change upstream,
the next update **stops before building** (`⚠ approval`) and snapshots both
states. Review and clear with:

```bash
updatium approve llamacpp --show-diff
updatium update llamacpp
```

### Adopting an install made outside Updatium

Set `installed_version_cmd` to observe installations managed inside or outside
Updatium. Its first non-empty stdout line is authoritative on every `list`,
`check`, update selection, and dashboard refresh, even after a successful
Updatium update. Empty output, a nonzero exit, or a failure to start means the
installed version is unknown; Updatium does not silently use an older record.
An update with an unknown configured probe is refused unless `--force` is used
after inspection; track-only updates and dry runs do not apply an installation.

```toml
# vllm.cpp ships a VERSION file (version=0.0.2)
installed_version_cmd = ["sed", "-n", "s/^version=//p", "~/Software/vllm.cpp/VERSION"]

# llama.cpp exposes its build number; map it to the `b<number>` tag form
installed_version_cmd = ["sh", "-c", "~/Software/llama.cpp/llama-cli --version 2>&1 | sed -n 's/.*build \\([0-9]*\\).*/b\\1/p'"]
```

Version provenance is shown as **observed** (successful configured probe),
**inferred** (source checkout tag and an existing install target), **recorded**
(last successful Updatium installation, not independently verified), or
**unknown**. `list` and `check` JSON expose `installed_source`. Observations
are not persisted over the installation record: removing the probe exposes the
last recorded installation, not the last observation.

For new managed directory, external source-directory, and AppImage installs,
Updatium records the installation path and reports unknown if that file or
directory is missing. Legacy managed directories can also be checked, but older
AppImage records may lack a known path. Existence alone cannot detect changed
contents, out-of-band upgrades or downgrades; configure a probe for those and
for package managers, plugins, and command-driven installs. A source checkout may
change without changing installed files, so its tag is only an inference and
never overrides a recorded version.

Probes run afresh on reads, once per app per check/dashboard refresh; they are
not cached. Each probe has a three-second deadline and a 64 KiB limit on each of
stdout and stderr. Timeout, excessive output, or command failure leaves the
installed version unknown and reports a diagnostic; the recorded version is not
used instead. Updatium requests termination of the direct child on timeout or
output-limit failure and waits up to one second to reap it; later cleanup is best
effort. Descendants are not terminated and can survive; they may also keep pipes
open until the deadline. Use fast, side-effect-free probes that manage their own
children. Source-checkout inference runs a local `git tag` command when needed.

Track-only updates (`command`, `apt`, or `flatpak` without `apply_cmd`, and
external providers with `install.type = "none"`) observe the selected release
without changing installed state or running an installation. An existing
`installed_version_cmd` remains effective, and a newer release remains an
outstanding update. Update reports identify these runs as track-only; JSON
reports set `track_only` to `true`, and `to` is the observed target, not a
claim that it was installed. A track-only update exits successfully.

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
# proxy = "http://127.0.0.1:3128"  # optional; applies to all apps

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
optional `allow_insecure_http` (permits plain http for this app),
optional `installed_version_cmd` (argv whose first non-empty stdout line is the
currently installed version, for installs made outside Updatium), plus one
provider block and an `[install]` table.

Set `proxy` in an app file to override the global proxy for that app's provider
checks and downloads:

```toml
id = "example"
provider = "html-links"
url = "https://example.com/downloads/"
link_regex = 'href="(?P<href>[^"]+\.AppImage)"'
version_capture = '(?P<version>\d+\.\d+)'
proxy = "socks5h://127.0.0.1:1080"
```

Proxy URLs support `http://`, `https://`, `socks5://` and `socks5h://` (SOCKS5
with proxy-side DNS), including optional `user:password@` credentials. Protect
config files containing credentials. Explicit proxy settings apply to both HTTP
and HTTPS destinations and override inherited proxy and `no_proxy` settings.
Without a configured proxy, Updatium's built-in HTTP client connects directly;
subprocesses retain their inherited environment. Provider, git, build and apply
subprocesses receive standard proxy environment variables, but each external
program or daemon must support and honor them independently (for example, a
Docker daemon may require its own proxy configuration).

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
(default `https://api.github.com`; override for GHES/tests). `tag_regex` matches
the tag, not the display name. Drafts always skipped. Digests come from the asset `digest` field or a sibling
`<asset>.sha256` / `SHA256SUMS` asset.

**`html-links`** — `url`, `link_regex` (named group `href`),
`version_capture` (named group `version`), optional `base_url`,
`digest_suffix`. Deduplicates by version keeping the lexicographically-largest
asset. A missing sidecar is “no digest”, never an error.

**`command`** — `check_cmd` (argv; newline-separated versions on stdout),
optional `apply_cmd` (`{version}` substituted; omit = track-only).

**`source-build`** — `repo_url`, `ref_regex` (matched against full
`refs/tags/<tag>` names from Git and optional GitHub releases), optional
`api_base` to also query GitHub releases, `[build]`: `steps`, `tracked_files`,
`artifacts`, `timeout_secs` (default 3600), `env`.

**`docker-registry`** — `image`
(`docker.io/<owner>/<name>`, bare `owner/name`, or `<registry>/<host-path>`),
`tag_regex`, required `pull_cmd`, optional `redeploy_cmd`, `api_base`.
`{version}`, `{image}`, `{registry}`, `{repo}` are substitutable in commands.

**`apt`** — `package` (required), optional `apply_cmd` (omit = track-only;
`{package}` and `{version}` substitutable). `check` reads the `Candidate:`
version from `apt-cache policy <package>`; no candidate means no release.
Installed and candidate versions use Debian/dpkg ordering, including epochs,
revisions and `~`; configure `installed_version_cmd` to return a Debian version.
Nothing runs as root implicitly — an unattended `apply_cmd` should start with a
privilege-escalation prefix such as `sudo -n apt-get install --only-upgrade -y`.

**`flatpak`** — `app_id` and `remote` (required), `user` (default true:
`--user` scope; false = `--system`), `version_field`
(`"version" | "branch" | "auto"`, default `auto`: Version column when non-empty
else Branch), optional `branch_regex` (filter on branch), and optional `apply_cmd`
(omit = track-only; `{app_id}`, `{remote}`, `{version}` substitutable). Multiple
matching branches are rejected: select one with an anchored filter such as
`^stable$`, and ensure the apply command targets the same branch. Flatpak refs
are not ordered against other branches. If `version_field = "branch"`, a fixed
branch name cannot identify changes to its underlying commit. `check` lists the
remote with `flatpak remote-ls --user|--system <remote> --app`.

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
`LATEST` is the newest version in the current provider result; versions removed
by a config filter (such as `tag_regex` or `ref_regex`) do not linger.

Except for apt, version selection uses SemVer precedence for three-component
SemVer versions, including tags prefixed with `v` or `V`; build metadata does not
change precedence. Two-component numeric versions are compared with a zero
patch; numeric cores with leading zeros are normalized. Other versions, such as
`b1234`, use natural ordering. Mixed SemVer-like and natural-order releases, or
an installed version from a different class, produce an error rather than an
arbitrary update decision, even with `--force`. Constrain the source using
`tag_regex`, `ref_regex`, `version_capture`, or `branch_regex`, and make the
installed-version probe use the same format. These filters are source-specific;
arbitrary command and plugin outputs must be constrained by their author. Original
GitHub and source-build tags, including `v`/`V`, are retained for selection,
fetching, applying and recorded state; older records without the prefix continue
to compare equal. Natural ordering does not reproduce application-specific
version rules.

TUI keys: `/` search · `f` cycle status filters · `r` refresh · `u` update the highlighted app · `U` bulk update eligible apps · `Space` toggle a mark · `s` update marked apps · `c` clear all marks · `a` approve · `j/k/arrows/pgup/pgdn` navigate · `d` app history · `e` edit config in `$EDITOR` · `n` create app · `x` delete config only · `g/G` write/remove desktop entry · `p` plugins · `F1` help · `q` quit. Destructive actions ask for confirmation; app-config deletion retains installed files, desktop entry, and database history/state.

Marks are identified by app ID and persist across searches, status filters, and refreshes; the marked count includes hidden apps. Removed apps are unmarked on refresh. `s` confirms the complete marked list in ID order (scroll with `j/k` or arrows); an empty selection does nothing, and the operation is cancelled if an app is removed or disabled before confirmation. Enabled marked apps can be updated regardless of their displayed status, including apt and flatpak apps. Their configured `apply_cmd` determines the actual operation and may update dependencies or other packages beyond the selected apps; selection does not constrain package-manager side effects.

During an update, the activity line shows the current app and stage. Asset downloads show bytes received and, when the server provides a content length, the total and percentage. Git, package-manager, Docker, and plugin operations report their stage but not internal transfer progress.

Interrupted asset downloads can resume when the server supplies a strong ETag and supports validated HTTP range requests. If the resource changes or the server cannot validate the partial download, the download restarts. The configured timeout and size cap still apply; available asset digests are verified as usual.
Engine downloads use per-app cache directories; older partial downloads in the
shared cache root are not migrated or resumed. Inspect them and confirm no older
Updatium process is using them before removing them.

`updatium tui --print-frame` renders one frame from DB state and exits — CI-safe.

## Security model

* **Injection firewall** — every variable interpolated into an argv template is
  validated against `^[A-Za-z0-9._+/=@:-]+$`; commands execute via direct argv,
  never a shell.
* **HTTPS enforced** — every fetched URL must be https unless an app explicitly
  sets `allow_insecure_http = true`.
* **Digest verification** — sha256 digests are checked in bounded memory by
  streaming the downloaded file before installation; mismatch hard-stops with a
  `verify_failed` event.
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
