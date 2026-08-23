#!/usr/bin/env bash
set -euo pipefail

# Runnable conservation-audit walkthrough: audits a committed before-tree
# against an after-tree demonstrating one cross-file move, one block split
# across two files, and one indentation-only rewrite. Runs in an ignored,
# disposable directory below examples/.work/; committed fixtures are never
# modified. Unlike examples 01–09 this example needs two input trees, so it
# uses its own runner like example 10.

example_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(CDPATH= cd -- "$example_dir/../.." && pwd)

if [[ -n ${SRCMV_BIN:-} ]]; then
  srcmv_bin=$SRCMV_BIN
elif command -v srcmv >/dev/null 2>&1; then
  srcmv_bin=$(command -v srcmv)
else
  cargo build --manifest-path "$repo_dir/Cargo.toml" -p srcmv-cli
  srcmv_bin="$repo_dir/target/debug/srcmv"
fi
if [[ ! -x $srcmv_bin ]]; then
  printf 'srcmv executable is not executable: %s\n' "$srcmv_bin" >&2
  exit 1
fi

work_root="$repo_dir/examples/.work/11-conservation-audit"
case "$work_root" in
  "$repo_dir"/examples/.work/11-conservation-audit) ;;
  *) printf 'refusing unsafe work path: %s\n' "$work_root" >&2; exit 1 ;;
esac
if [[ -L $work_root ]]; then
  printf 'refusing symbolic-link work path\n' >&2
  exit 1
fi
mkdir -p "$work_root"
rm -rf -- "$work_root/base" "$work_root/head"
mkdir -p "$work_root/reports"

cp -R "$example_dir/before" "$work_root/base"
cp -R "$example_dir/after" "$work_root/head"

"$srcmv_bin" audit-conservation \
  --base "$work_root/base" \
  --head "$work_root/head" \
  --json > "$work_root/reports/audit.json"

"$srcmv_bin" audit-conservation \
  --base "$work_root/base" \
  --head "$work_root/head" \
  > "$work_root/reports/summary.txt"

cat "$work_root/reports/summary.txt"
python3 "$example_dir/verify-report.py" \
  "$work_root/base" "$work_root/head" "$work_root/reports/audit.json"
printf 'PASS conservation audit\nreport: %s\nsummary: %s\n' \
  "$work_root/reports/audit.json" "$work_root/reports/summary.txt"
