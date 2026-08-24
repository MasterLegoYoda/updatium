#!/usr/bin/env bash
# Updatium external-provider protocol v1 — executable documentation.
#
# Discovery:   name must start with `updatium-provider-` on PATH, or be any
#              executable directly inside <config>/providers.d/.
# Handshake:   `updatium-provider-X describe`  ->  {"protocol":1,"id":"X"}
# Checking:    `updatium-provider-X check`     ->  app config JSON on stdin;
#              stdout JSON {"releases":[{version,published_at,assets:[...]}]}
# Exit codes:  nonzero = failure (stderr tail is surfaced to the user).
#
# This one scrapes AppImage links from a vendor HTML page using curl + python3,
# mirroring the built-in html-links provider. Config keys it understands:
#   url            (required) page to scrape
#   base_url       optional base for resolving relative hrefs
#   version_capture optional regex with named group "version"
set -euo pipefail

case "${1:-}" in
  describe)
    echo '{"protocol":1,"id":"html-scraper"}'
    ;;

  check)
    config=$(cat)
    export PROVIDER_CONFIG="$config"
    python3 - <<'PY'
import json, os, re, sys
try:
    cfg = json.loads(os.environ["PROVIDER_CONFIG"])
    url = cfg["url"]
except Exception as e:
    print(f"bad provider config: {e}", file=sys.stderr)
    sys.exit(1)

import urllib.request
html = urllib.request.urlopen(url, timeout=30).read().decode()
base = cfg.get("base_url") or url
ver_re = re.compile(cfg.get("version_capture") or r"(?P<version>\d+\.\d+(\.\d+)?)")

releases = {}
for href in re.findall(r'href="([^"]+\.AppImage)"', html):
    abs_url = href if "://" in href else base.rstrip("/") + "/" + href.lstrip("/")
    m = ver_re.search(href)
    if not m:
        continue
    v = m.group("version")
    asset = {"name": href.rsplit("/", 1)[-1], "url": abs_url, "size": None, "digest": None}
    if v not in releases or abs_url > releases[v]["assets"][0]["url"]:
        releases[v] = {"version": v, "published_at": None, "assets": [asset]}

print(json.dumps({"releases": sorted(releases.values(), key=lambda r: r["version"])}))
PY
    ;;

  *)
    echo "usage: $0 {describe|check}" >&2
    exit 64
    ;;
esac
