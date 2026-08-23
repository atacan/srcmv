#!/usr/bin/env python3
"""Verifies the conservation-audit example report against fixture-derived
expectations, exiting nonzero on any drift.

Usage: verify-report.py BASE_DIR HEAD_DIR REPORT_JSON

The expectations are recomputed from the fixture files themselves so the
script never hardcodes byte counts: the moved block must be conserved exactly,
the split block must reappear as two fragment destinations, the indentation
churn must surface as formatting-only bytes, and the ledger identity of plan
§9.2 must hold.
"""

import hashlib
import json
import pathlib
import sys


def fail(message):
    print(f"FAIL {message}", file=sys.stderr)
    sys.exit(1)


def check(condition, message):
    if not condition:
        fail(message)


def main():
    if len(sys.argv) != 4:
        print(
            "usage: verify-report.py BASE_DIR HEAD_DIR REPORT_JSON",
            file=sys.stderr,
        )
        return 2
    base_dir, head_dir, report_path = (pathlib.Path(a) for a in sys.argv[1:])
    try:
        report = json.loads(report_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        fail(f"report is not readable JSON: {error}")

    moved = (base_dir / "src" / "moved-block.txt").read_bytes()
    split = (base_dir / "src" / "split-block.txt").read_bytes()
    wrapper = (head_dir / "src" / "applied" / "moved.txt").read_bytes()
    part_a = (head_dir / "src" / "parts" / "part-a.txt").read_bytes()
    part_b = (head_dir / "src" / "parts" / "part-b.txt").read_bytes()

    # Fixture recipe sanity: the two committed parts must be non-empty,
    # disjoint slices of the original block (each comfortably above the
    # 16-byte fragment floor) that reassemble it exactly.
    body_a = part_a.removesuffix(b"\n")
    body_b = part_b.removesuffix(b"\n")
    check(len(body_a) >= 16 and len(body_b) >= 16, "split parts fall below the fragment floor")
    check(
        split.startswith(body_a)
        and split.endswith(body_b)
        and len(body_a) + len(body_b) == len(split),
        "committed parts do not reassemble the original block",
    )
    check(
        wrapper == b"// applied:\n" + moved + b"\n// end\n",
        "moved wrapper does not embed the block verbatim",
    )

    check(report.get("conservation_audit_version") == 1, "wrong surface version")
    check(report["verdict"] == "pass", f"expected pass, got {report['verdict']}")
    check(report["inconclusive_reasons"] == [], "unexpected inconclusive reasons")

    totals = report["totals"]
    expected_new = (len(wrapper) - len(moved)) + 1 + 1
    checks = {
        "removed_regions": 3,
        "conserved_exact_bytes": len(moved),
        "conserved_fragments_bytes": len(split),
        "formatting_only_bytes>0": True,
        "insubstantial_residue_bytes": 0,
        "unmatched_substantial_bytes": 0,
        "ambiguous_common_bytes": 0,
        "unanalyzed_bytes": 0,
        "new_bytes": expected_new,
    }
    for key, expected in checks.items():
        if key.endswith(">0"):
            name = key[:-2]
            check(totals[name] > 0, f"{name} should exceed zero, got {totals[name]}")
            continue
        check(
            totals[key] == expected,
            f"{key} drifted: expected {expected}, got {totals[key]}",
        )

    # Ledger identity (plan §9.2): every removed byte lands in one term.
    terms = (
        totals["conserved_exact_bytes"]
        + totals["conserved_fragments_bytes"]
        + totals["insubstantial_residue_bytes"]
        + totals["unmatched_substantial_bytes"]
        + totals["formatting_only_bytes"]
        + totals["ambiguous_common_bytes"]
        + totals["unanalyzed_bytes"]
    )
    check(terms == totals["removed_bytes"], "§9.2 ledger identity violated")

    regions = report["regions"]
    moved_region = next(
        (r for r in regions if r["source_path"] == "src/moved-block.txt"), None
    )
    check(moved_region is not None, "moved block produced no conserved record")
    destination = moved_region["destinations"][0]
    check(
        destination["byte_range"] == [12, 12 + len(moved)],
        f"moved destination range drifted: {destination['byte_range']}",
    )
    check(
        hashlib.sha256(moved).hexdigest() == moved_region["sha256"].split(":", 1)[1],
        "moved record digest mismatch",
    )

    split_region = next(
        (
            r
            for r in regions
            if r["source_path"] == "src/split-block.txt"
            and len(r["destinations"]) == 2
        ),
        None,
    )
    check(split_region is not None, "split block lacks a two-destination record")
    paths = [d["path"] for d in split_region["destinations"]]
    check(paths == ["src/parts/part-a.txt", "src/parts/part-b.txt"],
          f"split destinations out of order: {paths}")

    formatting = report["formatting"]
    check(len(formatting) >= 1, "indentation churn produced no formatting record")

    print("verified: verdict=pass "
          f"exact={totals['conserved_exact_bytes']} "
          f"fragments={totals['conserved_fragments_bytes']} "
          f"formatting={totals['formatting_only_bytes']} "
          f"new={totals['new_bytes']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
