"""Compare compression groupings of compilation fields on a sample of parts.
Usage: python3 -I groups.py ENTITIES_DIR EVERY [workers]"""
import base64, gzip, json, os, sys, zlib
from collections import Counter
from multiprocessing import Pool

def dr(b):
    c = zlib.compressobj(9, zlib.DEFLATED, -15, 9)
    return len(c.compress(b) + c.flush())

def cj(v):
    return json.dumps(v, separators=(",", ":"), sort_keys=True, ensure_ascii=False).encode()

HOT = {"abi", "compilation"}
BIG = {"stdJsonOutput", "creationBytecode", "runtimeBytecode"}
GROUPINGS = {
    "whole": lambda f: "all",
    "field": lambda f: f,
    "hot+rest": lambda f: "hot" if f in HOT else "rest",
    "abi,comp,big,rest": lambda f: f if f in HOT else ("big" if f in BIG else "rest"),
    "abi,comp,lanes,stdout,rest": lambda f: f if f in HOT else ("lanes" if f in ("creationBytecode", "runtimeBytecode") else ("stdout" if f == "stdJsonOutput" else "rest")),
}

def scan(path):
    res = Counter()
    blobs = {}
    for line in open(path, "rb"):
        e = json.loads(line)
        kind = e["kind"][4:]
        if kind not in ("blob", "compilation"):
            continue
        raw = base64.b64decode(e["$payload"])
        dec = gzip.decompress(raw) if e["contentencoding"][4:] == "gzip" else raw
        if kind == "blob":
            blobs.setdefault(e["hash"][8:], {})[int(e["part"][4:])] = dec
            continue
        if dec.startswith(b'{"$spill"'):
            st = json.loads(dec)["$spill"]
            parts = blobs.get(st["hash"], {})
            if len(parts) != st["parts"]:
                continue
            dec = b"".join(parts[i] for i in range(st["parts"]))
        data = json.loads(dec)["data"]
        res["n"] += 1
        for name, g in GROUPINGS.items():
            groups = {}
            for f, v in data.items():
                groups.setdefault(g(f), {})[f] = v
            res[name] += sum(dr(cj(x)) for x in groups.values())
        res["hot_only"] += sum(dr(cj(data[f])) for f in HOT if f in data)
    return dict(res)

if __name__ == "__main__":
    src, every = sys.argv[1], int(sys.argv[2])
    workers = int(sys.argv[3]) if len(sys.argv) > 3 else 8
    parts = sorted(os.path.join(src, p) for p in os.listdir(src) if p.endswith(".jsonl"))[::every]
    tot = Counter()
    with Pool(workers) as pool:
        for r in pool.imap_unordered(scan, parts):
            tot.update(r)
    print(json.dumps({"parts": len(parts), **tot}, indent=1))
    w = tot["whole"]
    for k in GROUPINGS:
        print(f"{k:28} {tot[k]/1e6:10.1f} MB  {100*(tot[k]/w-1):+5.1f}% vs whole")
    print(f"hot fields only: {tot['hot_only']/tot['n']:.0f} bytes avg per compilation")
