"""cashcat scan analyzer: recurring-series detection + origin-domain census.

Reads `cashcat list` TSV output (slug, end_date, question, tags, origin_url)
on stdin and reports:
  1. recurring market series (slugs differing only in date tokens) — the
     only kind worth a latency-arb program, since they generate repeated
     race events
  2. top origin domains across all markets

Usage: python -I scripts/scan_candidates.py < data/scan-zero-delay.tsv
"""
import collections
import re
import sys

MONTHS = {
    'january', 'february', 'march', 'april', 'may', 'june', 'july',
    'august', 'september', 'october', 'november', 'december',
    'jan', 'feb', 'mar', 'apr', 'jun', 'jul', 'aug', 'sep', 'sept',
    'oct', 'nov', 'dec',
}
YEARS = {str(y) for y in range(2024, 2032)}


def normalize(slug: str) -> str:
    """Slug with date-ish tokens removed; identical result = same series."""
    out = []
    for t in slug.split('-'):
        if t in MONTHS or t in YEARS:
            continue
        if re.fullmatch(r'\d+', t) or re.fullmatch(r'\d{1,2}(st|nd|rd|th)', t):
            continue
        out.append(t)
    return '-'.join(out)


def domain(url: str):
    m = re.match(r'https?://([^/]+)/?', url)
    return m.group(1).lower() if m else None


def main() -> None:
    rows, seen = [], set()
    for line in sys.stdin:
        parts = line.rstrip('\n').split('\t')
        if len(parts) < 4:
            continue
        slug = parts[0]
        if slug in seen:
            continue
        seen.add(slug)
        url = parts[4] if len(parts) > 4 else ''
        rows.append((slug, parts[1], parts[2], url))

    series = collections.defaultdict(list)
    for r in rows:
        series[normalize(r[0])].append(r)

    recur = sorted(
        ((k, v) for k, v in series.items() if len(v) >= 3),
        key=lambda kv: -len(kv[1]),
    )
    print(f"# unique markets: {len(rows)}; recurring series (>=3 instances): {len(recur)}")
    print()
    print("## recurring series (top 25)")
    for k, v in recur[:25]:
        doms = collections.Counter(d for d in (domain(r[3]) for r in v) if d)
        dom_txt = ', '.join(f'{d}x{c}' for d, c in doms.most_common(3)) or 'no-url'
        print(f"{len(v):4d}  {k}")
        print(f"      origins: {dom_txt}")
        print(f"      e.g. \"{v[0][2][:100]}\"")
    print()
    print("## top origin domains (all markets)")
    all_doms = collections.Counter(d for d in (domain(r[3]) for r in rows) if d)
    for d, c in all_doms.most_common(25):
        print(f"  {c:4d}  {d}")


if __name__ == '__main__':
    main()