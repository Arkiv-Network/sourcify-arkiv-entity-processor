"""Merge corpus.py part results: python3 -I corpus_merge.py OUT_DIR SUMMARY.json"""
import json, os, sys
from collections import Counter, defaultdict

src = sys.argv[1]
dest = sys.argv[2]
kinds = defaultdict(Counter); hist = defaultdict(Counter); attrnames = Counter()
spills = defaultdict(Counter); spillhist = Counter()
blobs = {}
vc_creation = Counter(); vc_runtime = Counter(); code_size = {}
vc = Counter(); vchist = Counter(); vckeys = Counter()
cp = Counter(); cphist = Counter(); cpfield = defaultdict(Counter); cpfieldhist = defaultdict(Counter)
cpdigest = defaultdict(Counter); srchist = Counter(); src_ = Counter(); cpsources = Counter()
pending = 0
MAXKEYS = {"max_deflate", "max_sources", "max_src_refs"}


def add(c, d):
    for k, v in d.items():
        if k in MAXKEYS:
            c[k] = max(c[k], v)
        else:
            c[k] += v


files = sorted(f for f in os.listdir(src) if f.endswith(".json"))
for i, name in enumerate(files):
    r = json.load(open(os.path.join(src, name)))
    for k, v in r["kinds"].items(): add(kinds[k], v)
    for k, v in r["hist"].items(): add(hist[k], v)
    add(attrnames, r["attrnames"])
    for k, v in r["spills"].items(): add(spills[k], v)
    add(spillhist, r["spillhist"])
    for h, (w, d, p, n) in r["blobs"].items():
        b = blobs.setdefault(h, [0, 0, 0, n]); b[0] += w; b[1] += d; b[2] += p
    for h, n in r["vc_creation"]: vc_creation[h] += n
    for h, n in r["vc_runtime"]: vc_runtime[h] += n
    for h, s in r["code_size"]: code_size[h] = s
    add(vc, r["vc"]); add(vchist, r["vchist"]); add(vckeys, r["vckeys"])
    add(cp, r["cp"]); add(cphist, r["cphist"])
    for k, v in r["cpfield"].items(): add(cpfield[k], v)
    for k, v in r["cpfieldhist"].items(): add(cpfieldhist[k], v)
    for k, v in r["cpdigest"].items(): add(cpdigest[k], v)
    add(srchist, r["srchist"]); add(src_, r["src"]); add(cpsources, r["cpsources"])
    pending += r["pending_spill_cp"]

# spilled payloads: compressed size of each logical payload (sum of per-part gzip)
spilled = [(w, d, p, n) for (w, d, p, n) in blobs.values()]
complete = [x for x in spilled if x[2] == x[3]]
fit = Counter()
for w, d, p, n in complete:
    for budget in (100_000, 120_000, 131_072):
        if w <= budget:
            fit[str(budget)] += 1
    fit["chunks120k"] += -(-w // 120_000)
spill_summary = {
    "logical_payloads": len(spilled), "complete": len(complete),
    "parts": sum(x[2] for x in spilled), "decoded": sum(x[1] for x in spilled),
    "wire_gzip": sum(x[0] for x in spilled), "fits_in_one_entity_if_whole_gzip": dict(fit),
    "max_wire": max((x[0] for x in spilled), default=0), "max_decoded": max((x[1] for x in spilled), default=0),
    "wire_hist": dict(Counter((x[0]).bit_length() for x in spilled)),
}

# onchain code sharing
def share(counter):
    refs = sum(counter.values())
    distinct = len(counter)
    raw_total = sum(code_size.get(h, 0) * n for h, n in counter.items())
    raw_distinct = sum(code_size.get(h, 0) for h in counter)
    missing = sum(1 for h in counter if h not in code_size)
    top = counter.most_common(5)
    shared = sum(1 for h, n in counter.items() if n > 1)
    refs_in_shared = sum(n for h, n in counter.items() if n > 1)
    return {"refs": refs, "distinct": distinct, "raw_bytes_if_inline": raw_total,
            "raw_bytes_distinct": raw_distinct, "missing_size": missing,
            "distinct_shared_by_2plus": shared, "refs_to_shared": refs_in_shared,
            "top_counts": [n for _, n in top]}

onchain_union = set(vc_creation) | set(vc_runtime)
code_summary = {
    "code_entities": len(code_size),
    "code_raw_bytes": sum(code_size.values()),
    "creation": share(vc_creation), "runtime": share(vc_runtime),
    "distinct_onchain_hashes": len(onchain_union),
    "code_entities_not_onchain_ref": len(set(code_size) - onchain_union),
    "creation_and_runtime_same_hash": len(set(vc_creation) & set(vc_runtime)),
}

dig = {}
for name, c in cpdigest.items():
    dig[name] = {"values": sum(c.values()), "distinct": len(c),
                 "dup_values": sum(n - 1 for n in c.values()), "top": [n for _, n in c.most_common(5)]}

summary = {
    "parts": len(files), "kinds": kinds, "hist": hist, "attrnames": attrnames, "spills": spills,
    "spillhist": spillhist, "spill_summary": spill_summary, "code": code_summary,
    "vc": vc, "vchist": vchist, "vckeys": vckeys, "cp": cp, "cphist": cphist,
    "cpfield": cpfield, "cpfieldhist": cpfieldhist, "cpdigest": dig, "srchist": srchist,
    "src": src_, "cpsources": cpsources, "pending_spill_cp": pending,
}
json.dump(summary, open(dest, "w"), indent=1, sort_keys=True)
print(json.dumps({k: summary[k] for k in ("kinds", "spills", "spill_summary", "code", "vc", "cp", "src", "cpdigest", "pending_spill_cp")}, indent=1))
