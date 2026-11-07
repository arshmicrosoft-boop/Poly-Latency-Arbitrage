"""cashcat session-log analyzer (README next-step 3).

Reads cashcat JSONL session logs and prints the measurement-spec report:
event counts, race-latency histogram + percentiles, forward-walk verdict
tally and EV-by-offset decay, publication uncertainty windows, CDN cache
state at origin events, book health at samples, logger drops.

Usage:
    python -I scripts/analyze_races.py                # all data/*.jsonl
    python -I scripts/analyze_races.py data/race-XXX.jsonl
"""
import glob
import json
import statistics
import sys


def percentile(sorted_vals, p):
    if not sorted_vals:
        return None
    k = (len(sorted_vals) - 1) * p / 100.0
    f = int(k)
    c = min(f + 1, len(sorted_vals) - 1)
    return sorted_vals[f] + (sorted_vals[c] - sorted_vals[f]) * (k - f)


def hist(vals, edges):
    buckets = [0] * (len(edges) + 1)
    for v in vals:
        for i, e in enumerate(edges):
            if v < e:
                buckets[i] += 1
                break
        else:
            buckets[-1] += 1
    labels = [f"<{edges[0]}"] + [f"{edges[i-1]}-{edges[i]}" for i in range(1, len(edges))] + [f">={edges[-1]}"]
    return list(zip(labels, buckets))


def main(paths):
    events = []
    for p in paths:
        with open(p, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    events.append(json.loads(line))
                except json.JSONDecodeError:
                    pass

    kinds = {}
    for e in events:
        kinds[e.get("kind", "?")] = kinds.get(e.get("kind", "?"), 0) + 1

    print(f"# files: {len(paths)}  events: {len(events)}")
    print("\n## event counts")
    for k, c in sorted(kinds.items(), key=lambda kv: -kv[1]):
        print(f"  {c:6d}  {k}")

    races = [e for e in events if e.get("kind") == "race"]
    if races:
        lat = sorted(e.get("latency_ms", 0) for e in races)
        print(f"\n## race latency (origin flip -> first book move), n={len(lat)}")
        print(f"  min={lat[0]}  p50={percentile(lat,50):.0f}  p95={percentile(lat,95):.0f}  max={lat[-1]} ms")
        for label, n in hist(lat, [100, 250, 500, 1000, 2000, 5000]):
            print(f"  {label:>12}: {n:4d} {'#' * min(n, 60)}")
    else:
        print("\n## race latency: no resolved races")
    timeouts = kinds.get("race_timeout", 0)
    if races or timeouts:
        total = len(races) + timeouts
        print(f"  races resolved {len(races)}/{total} ({100*len(races)/total:.0f}%), timeouts {timeouts}")

    walks = [e for e in events if e.get("kind") == "forward_walk_verdict"]
    if walks:
        print(f"\n## forward-walk verdicts, n={len(walks)}")
        for v in ("EXECUTABLE_EDGE", "PHANTOM_EDGE", "NO_EDGE"):
            print(f"  {v:16}: {sum(1 for w in walks if w.get('verdict') == v)}")
        ev_by_off = {}
        for e in events:
            if e.get("kind") == "forward_walk_sample":
                off = e.get("offset_ms")
                ev = (e.get("sweep") or {}).get("net_if_win")
                if off is not None and ev is not None:
                    ev_by_off.setdefault(off, []).append(ev)
        print("  EV decay by offset (mean / min net_if_win on $notional):")
        for off in sorted(ev_by_off):
            vs = ev_by_off[off]
            print(f"    +{off:>3} ms: mean={statistics.mean(vs):7.2f}  min={min(vs):7.2f}  n={len(vs)}")
        healthy = sum(1 for w in walks if w.get("book_healthy_all"))
        print(f"  book healthy throughout: {healthy}/{len(walks)}")
    else:
        print("\n## forward-walk verdicts: none yet")

    origins = [e for e in events if e.get("kind") == "origin_event"]
    if origins:
        unc = sorted(e["uncertainty_ms"] for e in origins if e.get("uncertainty_ms") is not None)
        print(f"\n## origin events, n={len(origins)}")
        if unc:
            print(f"  W_uncertainty ms: min={unc[0]} p50={percentile(unc,50):.0f} p95={percentile(unc,95):.0f} max={unc[-1]}")
        def cdn(e):
            return e.get("cdn") or {}

        cdn_hits = sum(
            1 for e in origins
            if ((cdn(e).get("cf_cache_status") or "").upper() == "HIT"
                or "Hit" in (cdn(e).get("x_cache") or "")
                or (cdn(e).get("age") or "0").isdigit() and int(cdn(e).get("age") or 0) > 0)
        )
        print(f"  CDN-served (HIT or Age>0): {cdn_hits}/{len(origins)}")
        ages = [int(cdn(e)["age"]) for e in origins if (cdn(e).get("age") or "").isdigit()]
        if ages:
            print(f"  Age s: min={min(ages)} p50={percentile(sorted(ages),50):.0f} max={max(ages)}")
        statuses = {}
        for e in origins:
            statuses[e.get("status")] = statuses.get(e.get("status"), 0) + 1
        print(f"  HTTP statuses: {statuses}")

    samples = [e for e in events if e.get("kind") == "forward_walk_sample"]
    if samples:
        health = {}
        for s in samples:
            h = s.get("book_health", "?")
            health[h] = health.get(h, 0) + 1
        print(f"\n## book health at forward-walk samples: {health}")

    drops = [e.get("logger_drops", 0) for e in events if e.get("kind") == "heartbeat"]
    if drops:
        print(f"\n## logger drops at last heartbeat: {drops[-1]}")

    sess = [e for e in events if e.get("kind") == "session_start"]
    if sess:
        s = sess[0]
        print("\n## session spec (pre-declared)")
        for k in ("target_notional_usd", "cap_price", "taker_fee_bps", "min_depth_usd", "min_profit_usd", "window_s"):
            if k in s:
                print(f"  {k}: {s[k]}")


if __name__ == "__main__":
    paths = sys.argv[1:] or sorted(glob.glob("data/*.jsonl"))
    if not paths:
        print("no data/*.jsonl files found")
        sys.exit(1)
    main(paths)