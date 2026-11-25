#!/usr/bin/env bash
# Publishes one release's crates to crates.io.
#
# The repository's version is `x.y.z-YYYYMMDDNN` and its internal dependencies
# carry only a path (CLAUDE.md §4). crates.io gets plain `x.y.z`, once per
# `x.y.z`, and wants a version on every dependency. So this works on a
# throwaway git worktree of the release in the system temp directory: it sets
# the workspace version to `x.y.z`, pins every internal path dependency to
# `=x.y.z`, and runs `cargo publish` (verification included) for each
# publishable member in dependency order, `agix` last. `agix` at `x.y.z` on
# crates.io therefore means the release is complete, and a run that finds it
# there refuses.
#
# A run after a failure skips the crates already up. crates.io lets an account
# create only a few new crates in a burst; a 429 waits and retries.
#
#   scripts/publish-crates.sh [--dry-run] [Agix-v<x.y.z>-<YYYYMMDDNN>]
#   scripts/publish-crates.sh [--dry-run] --ref <git ref>
#
# Without a tag the newest `Agix-v*` tag is published. `--ref` takes any
# commit instead, with that commit's version; it is for dry runs of work that
# has no tag yet. `--dry-run` packages and verifies all the crates together
# (`cargo publish --workspace --dry-run`) and uploads nothing.
#
# The token is CARGO_REGISTRY_TOKEN from the environment, else the
# `CARGO_REGISTRY_TOKEN=` line of the repository's `.env`.

set -euo pipefail

die() {
  echo "publish-crates: $*" >&2
  exit 1
}

dry_run=false
ref=""
tag=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) dry_run=true ;;
    --ref)
      [ $# -ge 2 ] || die "--ref needs a git ref"
      ref=$2
      shift
      ;;
    --ref=*) ref=${1#--ref=} ;;
    -h | --help)
      sed -n '2,/^$/s/^# \{0,1\}//p' "$0"
      exit 0
      ;;
    -*) die "unknown option $1" ;;
    *)
      [ -z "$tag" ] || die "one tag at most"
      tag=$1
      ;;
  esac
  shift
done
[ -z "$ref" ] || [ -z "$tag" ] || die "give a tag or --ref, not both"

for tool in git cargo curl python3; do
  command -v "$tool" >/dev/null || die "needs $tool on PATH"
done

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$repo"
[ -z "$(git status --porcelain)" ] || die "the working tree is not clean"

if [ -z "$ref" ]; then
  if [ -z "$tag" ]; then
    tag=$(git tag -l 'Agix-v*' --sort=-creatordate | head -n 1)
    [ -n "$tag" ] || die "there is no Agix-v* tag"
  fi
  [[ $tag =~ ^Agix-v[0-9]+\.[0-9]+\.[0-9]+-[0-9]{10}$ ]] ||
    die "$tag is not of the form Agix-v<x.y.z>-<YYYYMMDDNN>"
  git rev-parse -q --verify "refs/tags/$tag" >/dev/null || die "there is no tag $tag"
  ref=$tag
fi
commit=$(git rev-parse -q --verify "$ref^{commit}") || die "$ref names no commit"

full=$(git show "$commit:Cargo.toml" |
  sed -n '/^\[workspace\.package\]/,/^\[/s/^version *= *"\([^"]*\)".*/\1/p')
version=${full%%-*}
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "$ref has the workspace version '$full'"
if [ -n "$tag" ] && [ "Agix-v$full" != "$tag" ]; then
  die "$tag has the workspace version $full"
fi

user_agent="agix-publish-crates"

# Exit status 0 when crates.io has `$1` at `$2`, 1 when it does not.
on_crates_io() {
  local code attempt
  for attempt in 1 2 3 4 5; do
    code=$(curl -s -o /dev/null -w '%{http_code}' -A "$user_agent" \
      "https://crates.io/api/v1/crates/$1/$2") || code=000
    case $code in
      200) return 0 ;;
      404) return 1 ;;
    esac
    sleep $((attempt * 5))
  done
  die "crates.io answered $code when asked for $1 $2"
}

if on_crates_io agix "$version"; then
  die "agix $version is on crates.io already: $version has been published"
fi

if ! $dry_run; then
  if [ -z "${CARGO_REGISTRY_TOKEN:-}" ] && [ -f "$repo/.env" ]; then
    CARGO_REGISTRY_TOKEN=$(sed -n \
      's/^[[:space:]]*\(export[[:space:]][[:space:]]*\)\{0,1\}CARGO_REGISTRY_TOKEN[[:space:]]*=[[:space:]]*//p' \
      "$repo/.env" | tail -n 1 | tr -d '\r' |
      sed -e 's/[[:space:]]*$//' -e 's/^"\(.*\)"$/\1/' -e "s/^'\\(.*\\)'\$/\\1/")
  fi
  [ -n "${CARGO_REGISTRY_TOKEN:-}" ] ||
    die "set CARGO_REGISTRY_TOKEN, in the environment or in $repo/.env"
  export CARGO_REGISTRY_TOKEN
fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/agix-publish.XXXXXX")
src="$tmp/src"
cleanup() {
  cd "$repo"
  git worktree remove --force "$src" >/dev/null 2>&1 || true
  rm -rf "$tmp"
  git worktree prune
}
trap cleanup EXIT
trap 'exit 130' INT TERM

git worktree add --quiet --detach "$src" "$commit"
cd "$src"
export CARGO_TARGET_DIR="$tmp/target"

# Rewrites the manifests in place and prints the publishable members in the
# order they go up.
order=$(python3 - "$version" <<'PY'
import json, pathlib, re, subprocess, sys

version = sys.argv[1]
meta = json.loads(subprocess.run(
    ["cargo", "metadata", "--no-deps", "--format-version", "1"],
    check=True, capture_output=True, text=True).stdout)
root = pathlib.Path(meta["workspace_root"]).resolve()

def publishable(p):
    return p["publish"] is None or "crates-io" in p["publish"]

members = {pathlib.Path(p["manifest_path"]).parent.resolve(): p for p in meta["packages"]}
published = {d: p["name"] for d, p in members.items() if publishable(p)}

path_entry = re.compile(r'^(\s*[A-Za-z0-9_.-]+\s*=\s*\{)(.*\bpath\s*=\s*"([^"]+)".*\}\s*)$')

def pin(manifest, own_dir=None):
    lines = manifest.read_text().split("\n")
    table = ""
    for i, line in enumerate(lines):
        header = re.match(r'^\s*\[([^\]]+)\]', line)
        if header:
            table = header.group(1).strip()
            continue
        if not (table == "workspace.dependencies" or table.endswith("dependencies")):
            continue
        m = path_entry.match(line)
        if not m or re.search(r'\b(version|workspace)\s*=', m.group(2)):
            continue
        target = (manifest.parent / m.group(3)).resolve()
        if target in published and target != own_dir:
            lines[i] = f'{m.group(1)} version = "={version}",{m.group(2)}'
    return "\n".join(lines)

root_manifest = root / "Cargo.toml"
text = pin(root_manifest)
text, n = re.subn(r'(\[workspace\.package\][^\[]*?\nversion\s*=\s*)"[^"]*"',
                  rf'\1"{version}"', text, count=1)
if n != 1:
    sys.exit("no version in [workspace.package]")
root_manifest.write_text(text)
for d in published:
    manifest = d / "Cargo.toml"
    if manifest != root_manifest:
        manifest.write_text(pin(manifest, own_dir=d))

needs = {}
for d, name in published.items():
    needs[name] = sorted({
        published[pathlib.Path(dep["path"]).resolve()]
        for dep in members[d]["dependencies"]
        if dep.get("path") and pathlib.Path(dep["path"]).resolve() in published
        and pathlib.Path(dep["path"]).resolve() != d
    })
if "agix" not in needs:
    sys.exit("agix is not publishable at this commit (publish = false): it predates crates.io publishing")
if any("agix" in deps for deps in needs.values()):
    sys.exit("a published crate depends on agix, so agix cannot go up last")

order, done = [], set()
def visit(name, stack=()):
    if name in done:
        return
    if name in stack:
        sys.exit("dependency cycle: " + " -> ".join(stack + (name,)))
    for dep in needs[name]:
        visit(dep, stack + (name,))
    done.add(name)
    order.append(name)
for name in sorted(n for n in needs if n != "agix"):
    visit(name)
visit("agix")
print("\n".join(order))
PY
) || die "could not rewrite the manifests"

excludes=()
for name in $(cargo metadata --no-deps --format-version 1 |
  python3 -c 'import json,sys; print("\n".join(p["name"] for p in json.load(sys.stdin)["packages"] if p["publish"] == []))'); do
  excludes+=(--exclude "$name")
done

count=$(echo "$order" | wc -l | tr -d ' ')
echo "== $count crates at $version from $ref ($commit), in this order:"
echo "$order" | nl -w 3 -s '  '

# What cargo said it packaged, per crate, from the logs of this run.
report_sizes() {
  ls "$tmp"/*.log >/dev/null 2>&1 || return 0
  echo "== packaged (compressed is what crates.io stores; its limit is 10 MB)"
  python3 - "$tmp"/*.log <<'PY'
import re, sys
units = {"B": 1, "KiB": 1 << 10, "MiB": 1 << 20, "GiB": 1 << 30}
too_big = []
for path in sys.argv[1:]:
    name = None
    for line in open(path, errors="replace"):
        m = re.match(r"\s*Packaging (\S+) v", line)
        if m:
            name = m.group(1)
        m = re.match(r"\s*Packaged (\d+) files, (\S+) \(([\d.]+)(B|KiB|MiB|GiB) compressed\)", line)
        if m and name:
            size = float(m.group(3)) * units[m.group(4)]
            print(f"  {name:<22} {m.group(1):>4} files  {m.group(2):>9}  {m.group(3)}{m.group(4)} compressed")
            if size > 10 * 1000 * 1000:
                too_big.append(name)
if too_big:
    sys.exit("over crates.io's 10 MB limit: " + ", ".join(too_big))
PY
}

if $dry_run; then
  cargo publish --workspace --dry-run --allow-dirty ${excludes[@]+"${excludes[@]}"} 2>&1 |
    tee "$tmp/dry-run.log"
  report_sizes
  echo "== dry run of $version done; nothing was uploaded"
  exit 0
fi

# Seconds until the time a 429 names ("try again after <date>"), if it names one.
retry_after() {
  python3 - "$1" <<'PY'
import email.utils, re, sys, time
text = open(sys.argv[1], errors="replace").read()
m = re.search(r"try again after (.+? GMT)", text)
if m:
    try:
        when = email.utils.parsedate_to_datetime(m.group(1)).timestamp()
        print(max(30, int(when - time.time()) + 15))
    except (TypeError, ValueError):
        pass
PY
}

publish_one() {
  local crate=$1 log="$tmp/publish-$1.log" backoff=60 attempt=0 named
  while :; do
    attempt=$((attempt + 1))
    [ "$attempt" -le 60 ] || die "$crate: still failing after 60 attempts"
    echo "== $crate $version (attempt $attempt)"
    if cargo publish -p "$crate" --allow-dirty 2>&1 | tee "$log"; then
      return 0
    fi
    if on_crates_io "$crate" "$version"; then
      return 0
    fi
    if grep -Eq '429|Too Many Requests|too many new crates' "$log"; then
      named=$(retry_after "$log")
      echo "== rate limited; waiting ${named:-$backoff}s"
      sleep "${named:-$backoff}"
      backoff=$((backoff * 2 > 900 ? 900 : backoff * 2))
      continue
    fi
    if grep -Eq 'failed to select a version|no matching package named' "$log"; then
      echo "== a dependency is not in the index yet; waiting 30s"
      sleep 30
      continue
    fi
    die "cargo publish -p $crate failed (output above); fix it and run again: crates already up are skipped"
  done
}

for crate in $order; do
  if on_crates_io "$crate" "$version"; then
    echo "== $crate $version is on crates.io already"
    continue
  fi
  publish_one "$crate"
done
report_sizes
echo "== $version is on crates.io"
