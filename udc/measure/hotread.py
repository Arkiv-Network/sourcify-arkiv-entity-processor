"""Hot-read sizes: digests-only attribute against the manifest, and references into abi/compilation.
Usage: python3 -I hotread.py ENTITIES_DIR (every 25th part file)"""
import base64, gzip, json, os, sys, zlib, hashlib
from collections import Counter
from multiprocessing import Pool
def enc(b):
    c = zlib.compressobj(9, zlib.DEFLATED, -15, 9); d = c.compress(b) + c.flush()
    return 1 + min(len(d), len(b))
cj = lambda v: json.dumps(v, separators=(",", ":"), sort_keys=True, ensure_ascii=False).encode()
def scan(path):
    r = Counter(); blobs = {}
    for line in open(path, "rb"):
        e = json.loads(line); kind = e["kind"][4:]
        if kind not in ("blob", "compilation"): continue
        raw = base64.b64decode(e["$payload"]); dec = gzip.decompress(raw) if e["contentencoding"][4:] == "gzip" else raw
        if kind == "blob": blobs.setdefault(e["hash"][8:], {})[int(e["part"][4:])] = dec; continue
        if dec.startswith(b'{"$spill"'):
            st = json.loads(dec)["$spill"]; p = blobs.get(st["hash"], {})
            if len(p) != st["parts"]: continue
            dec = b"".join(p[i] for i in range(st["parts"]))
        env = json.loads(dec); data = env["data"]
        groups = {n: data[n] for n in ("abi", "compilation") if n in data}
        art = {n: v for n, v in data.items() if n not in ("abi", "compilation")}
        if art: groups["artifacts"] = art
        digests = {n: "0x" + hashlib.sha256(cj(v)).hexdigest() for n, v in groups.items()}
        hot_refs = [x for x in env["references"] if x["path"].startswith(("/abi", "/compilation"))]
        r["n"] += 1; r["digests"] += enc(cj({"digests": digests})); r["hot_refs"] += len(hot_refs)
        r["hot_refs_contracts"] += 1 if hot_refs else 0
        r["abi"] += enc(cj(groups.get("abi"))); r["compilation"] += enc(cj(groups.get("compilation")))
    return dict(r)
if __name__ == "__main__":
    src = sys.argv[1]
    parts = sorted(os.path.join(src, p) for p in os.listdir(src) if p.endswith(".jsonl"))[::25]
    t = Counter()
    with Pool(8) as pool:
        for x in pool.imap_unordered(scan, parts): t.update(x)
    n = t["n"]
    print(json.dumps({"compilations": n, "digests_avg": t["digests"]/n, "abi_avg": t["abi"]/n, "compilation_avg": t["compilation"]/n, "compilations_with_refs_into_abi_or_compilation": t["hot_refs_contracts"], "refs_into_hot": t["hot_refs"]}, indent=1))
