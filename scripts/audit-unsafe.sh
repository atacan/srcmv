#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repository_root"

for crate_root in crates/*/src/lib.rs; do
  if command -v rg >/dev/null 2>&1; then
    rg -q '^#!\[forbid\(unsafe_code\)\]$' "$crate_root"
  else
    grep -q -E '^#!\[forbid\(unsafe_code\)\]$' "$crate_root"
  fi
done

if command -v rg >/dev/null 2>&1; then
  direct_unsafe="$(rg -n --glob '*.rs' '(^|[^[:alnum:]_])unsafe[[:space:]]*(\{|fn|impl|trait)' crates || true)"
else
  direct_unsafe="$(grep -R -n -E --include='*.rs' '(^|[^[:alnum:]_])unsafe[[:space:]]*(\{|fn|impl|trait)' crates || true)"
fi
if [[ -n "$direct_unsafe" ]]; then
  printf '%s\n' "$direct_unsafe" >&2
  printf '%s\n' 'direct unsafe code exists outside the reviewed safe no-replace wrapper' >&2
  exit 1
fi

# AC#8 of docs/conservation-audit-plan.md: the conservation engine performs
# zero syscalls by construction and shares nothing with srcmv's edit
# machinery. The scans below pin both properties against
# crates/srcmv-core/src/conservation/.
#
# Curated identifier denylist proving no request/journal/plan-hash/log
# coupling inside core::conservation. Every pattern was grepped against the
# current tree before being pinned here and matched nothing (zero false
# positives); if a future legitimate match appears, extend this comment with
# the justification rather than silently loosening the list:
#   Journal, plan_digest, PlanDigest, RequestDto — edit-machinery types the
#     audit must never consult (plan §1);
#   transaction — transaction-journal vocabulary;
#   tracing, log:: — logging frameworks (the engine has no diagnostics side
#     channel).
conservation_dir="crates/srcmv-core/src/conservation"

scan_conservation() {
  if command -v rg >/dev/null 2>&1; then
    rg -n --glob '*.rs' "$1" "$conservation_dir" || true
  else
    grep -R -n -E --include='*.rs' "$1" "$conservation_dir" || true
  fi
}

conservation_std_fs="$(scan_conservation 'std::fs')"
if [[ -n "$conservation_std_fs" ]]; then
  printf '%s\n' "$conservation_std_fs" >&2
  printf '%s\n' 'core::conservation must not import or use std::fs (AC#8: zero syscalls by construction)' >&2
  exit 1
fi
printf 'PASS: no std::fs usage anywhere in core::conservation\n'

conservation_coupling='Journal|plan_digest|PlanDigest|RequestDto|transaction|tracing|log::'
conservation_coupling_hits="$(scan_conservation "$conservation_coupling")"
if [[ -n "$conservation_coupling_hits" ]]; then
  printf '%s\n' "$conservation_coupling_hits" >&2
  printf '%s\n' 'core::conservation must not reference request/journal/plan-hash/log identifiers (AC#8)' >&2
  exit 1
fi
printf 'PASS: no request/journal/plan-hash/log coupling identifiers in core::conservation\n'

RUSTFLAGS='-Funsafe-code' cargo check --workspace --all-targets --all-features
