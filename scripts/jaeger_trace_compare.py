#!/usr/bin/env python3
import argparse
import json
import re
import subprocess
import sys
import urllib.parse
import urllib.request
import urllib.error
from collections import defaultdict


def fetch_traces(jaeger_url: str, service: str, limit: int):
    query = urllib.parse.urlencode({"service": service, "limit": limit})
    url = f"{jaeger_url.rstrip('/')}/api/traces?{query}"
    with urllib.request.urlopen(url) as resp:
        payload = json.loads(resp.read().decode("utf-8"))
    return payload.get("data", [])


def tags_to_map(tags):
    out = {}
    for tag in tags or []:
        key = tag.get("key")
        if key:
            out[key] = tag.get("value")
    return out


def commit_matches(stored: str, query: str) -> bool:
    if not stored or not query:
        return False
    s = stored.lower()
    q = query.lower()
    return s.startswith(q) or q.startswith(s)


def version_matches(stored: str, query: str) -> bool:
    if not stored or not query:
        return False
    s = stored.lower()
    q = query.lower()
    return s == q or f"v{s}" == q or (s.startswith("v") and s[1:] == q)


def resolve_ref_to_commit(selector: str) -> str:
    selector = selector.strip()
    if not selector:
        return selector
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--short=12", selector],
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError:
        return selector
    if out.returncode != 0:
        return selector
    resolved = out.stdout.strip()
    return resolved or selector


def selector_candidates(selector: str):
    base = selector.strip()
    if not base:
        return []

    candidates = [base]
    resolved = resolve_ref_to_commit(base)
    if resolved and resolved not in candidates:
        candidates.append(resolved)

    if re.fullmatch(r"[0-9a-fA-F]{7,40}", base):
        pass
    elif base.startswith("v") and len(base) > 1:
        candidates.append(base[1:])
    else:
        candidates.append(f"v{base}")

    deduped = []
    for c in candidates:
        if c and c not in deduped:
            deduped.append(c)
    return deduped


def row_matches_selector(row, selector: str) -> bool:
    for candidate in selector_candidates(selector):
        if commit_matches(row.get("commit", ""), candidate):
            return True
        if version_matches(row.get("service_version", ""), candidate):
            return True
    return False


def extract_root_spans(trace):
    return [s for s in trace.get("spans", []) if not s.get("references")]


def extract_top_child(root_span, spans):
    root_id = root_span.get("spanID")
    children = []
    for span in spans:
        for ref in span.get("references", []):
            if ref.get("refType") == "CHILD_OF" and ref.get("spanID") == root_id:
                children.append(span)
                break

    if not children:
        return "-", 0.0

    longest = max(children, key=lambda s: s.get("duration", 0))
    return longest.get("operationName", "-"), longest.get("duration", 0) / 1_000_000.0


def collect_run_rows(traces):
    rows = []
    for trace in traces:
        spans = trace.get("spans", [])
        process_map = trace.get("processes", {})
        roots = extract_root_spans(trace)
        for root in roots:
            process_id = root.get("processID")
            process = process_map.get(process_id, {})
            process_tags = tags_to_map(process.get("tags", []))
            root_tags = tags_to_map(root.get("tags", []))
            commit = process_tags.get("git.commit") or root_tags.get("git.commit") or "unknown"
            service_version = process_tags.get("service.version") or "unknown"
            top_child_name, top_child_sec = extract_top_child(root, spans)
            rows.append(
                {
                    "trace_id": trace.get("traceID", "-"),
                    "operation": root.get("operationName", "-"),
                    "duration_sec": root.get("duration", 0) / 1_000_000.0,
                    "start_time": root.get("startTime", 0),
                    "commit": commit,
                    "service_version": service_version,
                    "top_child": top_child_name,
                    "top_child_sec": top_child_sec,
                }
            )
    return rows


def latest_by_operation(rows):
    grouped = defaultdict(list)
    for row in rows:
        grouped[row["operation"]].append(row)

    out = {}
    for op, items in grouped.items():
        out[op] = max(items, key=lambda x: x["start_time"])
    return out


def fmt_sec(value):
    return f"{value:.3f}"


def print_header(service, base=None, head=None, commit=None):
    print("| key | value |")
    print("|---|---|")
    print(f"| service | `{service}` |")
    if commit is not None:
        print(f"| commit | `{commit}` |")
    if base is not None:
        print(f"| base | `{base}` |")
    if head is not None:
        print(f"| head | `{head}` |")
    print()


def cmd_report(args):
    try:
        traces = fetch_traces(args.jaeger, args.service, args.limit)
    except (urllib.error.URLError, json.JSONDecodeError) as exc:
        print(f"Failed to fetch traces from Jaeger: {exc}", file=sys.stderr)
        return 1
    runs = collect_run_rows(traces)
    selected = [r for r in runs if row_matches_selector(r, args.commit)]
    latest = latest_by_operation(selected)

    if not latest:
        print_header(args.service, commit=args.commit)
        print("No traces found for the specified commit.")
        return 1

    print_header(args.service, commit=args.commit)
    print("| operation | trace_id | duration_sec | top_child | top_child_sec |")
    print("|---|---|---:|---|---:|")
    for op in sorted(latest.keys()):
        row = latest[op]
        print(
            f"| `{op}` | `{row['trace_id']}` | {fmt_sec(row['duration_sec'])} | `{row['top_child']}` | {fmt_sec(row['top_child_sec'])} |"
        )
    return 0


def cmd_compare(args):
    try:
        traces = fetch_traces(args.jaeger, args.service, args.limit)
    except (urllib.error.URLError, json.JSONDecodeError) as exc:
        print(f"Failed to fetch traces from Jaeger: {exc}", file=sys.stderr)
        return 1
    runs = collect_run_rows(traces)

    base_rows = [r for r in runs if row_matches_selector(r, args.base)]
    head_rows = [r for r in runs if row_matches_selector(r, args.head)]

    base_latest = latest_by_operation(base_rows)
    head_latest = latest_by_operation(head_rows)

    if not base_latest and not head_latest:
        print_header(args.service, base=args.base, head=args.head)
        print("No traces found for either commit.")
        return 1

    ops = sorted(set(base_latest.keys()) | set(head_latest.keys()))

    print_header(args.service, base=args.base, head=args.head)
    print("| operation | base_trace_id | base_sec | base_top_child (sec) | head_trace_id | head_sec | head_top_child (sec) | delta_sec | delta_% |")
    print("|---|---|---:|---|---|---:|---|---:|---:|")

    for op in ops:
        b = base_latest.get(op)
        h = head_latest.get(op)

        b_trace = f"`{b['trace_id']}`" if b else "-"
        h_trace = f"`{h['trace_id']}`" if h else "-"

        b_sec = b["duration_sec"] if b else None
        h_sec = h["duration_sec"] if h else None

        b_child = (
            f"`{b['top_child']}` ({fmt_sec(b['top_child_sec'])})" if b else "-"
        )
        h_child = (
            f"`{h['top_child']}` ({fmt_sec(h['top_child_sec'])})" if h else "-"
        )

        if b_sec is not None and h_sec is not None:
            delta = h_sec - b_sec
            pct = (delta / b_sec * 100.0) if b_sec != 0 else 0.0
            delta_s = fmt_sec(delta)
            pct_s = fmt_sec(pct)
            b_s = fmt_sec(b_sec)
            h_s = fmt_sec(h_sec)
        else:
            delta_s = "-"
            pct_s = "-"
            b_s = fmt_sec(b_sec) if b_sec is not None else "-"
            h_s = fmt_sec(h_sec) if h_sec is not None else "-"

        print(
            f"| `{op}` | {b_trace} | {b_s} | {b_child} | {h_trace} | {h_s} | {h_child} | {delta_s} | {pct_s} |"
        )

    return 0


def main():
    parser = argparse.ArgumentParser(
        description="Compare Jaeger trace durations by git ref (commit/tag) or service.version."
    )
    parser.add_argument("--jaeger", default="http://localhost:16686", help="Jaeger base URL")
    parser.add_argument("--service", default="opz-e2e", help="Service name")
    parser.add_argument("--limit", type=int, default=200, help="Number of traces to fetch")

    sub = parser.add_subparsers(dest="command", required=True)

    report = sub.add_parser(
        "report", help="Show latest trace metrics for a selector (commit/tag/version)"
    )
    report.add_argument(
        "--commit",
        required=True,
        help="Commit hash, git tag, or version (prefix/hash allowed)",
    )

    compare = sub.add_parser(
        "compare", help="Compare latest trace metrics between selectors"
    )
    compare.add_argument(
        "--base",
        required=True,
        help="Base selector (commit hash, git tag, or version)",
    )
    compare.add_argument(
        "--head",
        required=True,
        help="Head selector (commit hash, git tag, or version)",
    )

    args = parser.parse_args()

    if args.command == "report":
        return cmd_report(args)
    if args.command == "compare":
        return cmd_compare(args)
    return 2


if __name__ == "__main__":
    sys.exit(main())
