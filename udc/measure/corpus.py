"""Per-kind statistics of a v3 seed bundle: sizes, spills, code sharing, compilation fields.

Usage: python3 -I corpus.py ENTITIES_DIR OUT_DIR [workers [parts]]
Writes one JSON per part file to OUT_DIR; corpus_merge.py combines them (results/corpus-20261023.json).
"""
import base64, gzip, hashlib, json, os, sys, zlib
from collections import Counter, defaultdict
from multiprocessing import Pool

BUDGET = 120_000  # proposed per-entity bytes budget (bytes attribute values)


def b2(n):
    """log2 bucket"""
    return 0 if n <= 0 else n.bit_length()


def deflate_raw(b, level=9):
    c = zlib.compressobj(level, zlib.DEFLATED, -15, 9)
    return len(c.compress(b) + c.flush())


def cjson(v):
    return json.dumps(v, separators=(",", ":"), sort_keys=True, ensure_ascii=False).encode()


def h8(hexstr):
    return int(hexstr[2:18], 16)


LANE = ["recompiledBytecode", "sourceMap", "linkReferences", "immutableReferences", "cborAuxdata"]


def scan(path):
    out = {
        "kinds": defaultdict(lambda: Counter()),
        "hist": defaultdict(lambda: Counter()),
        "attrnames": Counter(),
        "spills": defaultdict(lambda: Counter()),
        "spillhist": Counter(),
        "blobs": {},  # hash -> [wire, decoded, parts]
        "vc_creation": Counter(), "vc_runtime": Counter(),
        "code_size": {},
        "vc": Counter(), "vchist": Counter(), "vckeys": Counter(),
        "cp": Counter(), "cphist": Counter(), "cpfield": defaultdict(lambda: Counter()),
        "cpfieldhist": defaultdict(lambda: Counter()),
        "cpdigest": defaultdict(Counter),
        "srchist": Counter(), "src": Counter(),
        "cpsources": Counter(),
        "pending_spill_cp": 0,
    }
    blobs_local = defaultdict(dict)  # hash -> part -> payload bytes (for compilation reassembly)
    with open(path, "rb") as f:
        for line in f:
            e = json.loads(line)
            kind = e["kind"][4:]
            raw = base64.b64decode(e["$payload"])
            enc = e["contentencoding"][4:]
            dec = gzip.decompress(raw) if enc == "gzip" else raw
            k = out["kinds"][kind]
            k["n"] += 1
            k["wire"] += len(raw)
            k["decoded"] += len(dec)
            k[enc] += 1
            attrs = {a: v for a, v in e.items() if not a.startswith("$")}
            k["attrs"] += len(attrs)
            k["attrbytes"] += sum(len(a) + len(v.split(":", 1)[1]) for a, v in attrs.items())
            for a in attrs:
                out["attrnames"][kind + "." + a] += 1
            out["hist"][kind + ".wire"][b2(len(raw))] += 1
            out["hist"][kind + ".decoded"][b2(len(dec))] += 1
            ctype = e["originalcontenttype"][4:]
            stub = None
            if ctype == "application/json" and dec.startswith(b'{"$spill"'):
                v = json.loads(dec)
                if len(v) == 1:
                    stub = v["$spill"]
                    s = out["spills"][kind]
                    s["n"] += 1
                    s["bytes"] += stub["bytes"]
                    s["parts"] += stub["parts"]
                    out["spillhist"][kind + ":" + str(b2(stub["bytes"]))] += 1
            if kind == "blob":
                hsh = e["hash"][8:]
                part = int(e["part"][4:])
                b = out["blobs"].setdefault(hsh, [0, 0, 0, int(e["parts"][4:])])
                b[0] += len(raw); b[1] += len(dec); b[2] += 1
                blobs_local[hsh][part] = dec
                continue
            if kind == "code":
                out["code_size"][h8(e["hash"][8:])] = int(e["size"][4:])
                continue
            if kind == "sourcefile":
                sz = int(e["size"][4:])
                out["srchist"][b2(sz)] += 1
                out["src"]["n"] += 1
                out["src"]["content"] += sz
                if stub is not None:
                    parts = blobs_local.get(stub["hash"], {})
                    if len(parts) != stub["parts"]:
                        out["src"]["spill_missing"] += 1
                        continue
                    dec = b"".join(parts[i] for i in range(stub["parts"]))
                    out["src"]["spilled_reassembled"] += 1
                content = json.loads(dec)["content"].encode()
                out["src"]["deflate"] += deflate_raw(content)
                out["src"]["json_wrapper"] += len(dec) - len(content)
                continue
            if kind == "verified_contract":
                if "creationcodehash" in e:
                    out["vc_creation"][h8(e["creationcodehash"][8:])] += 1
                if "runtimecodehash" in e:
                    out["vc_runtime"][h8(e["runtimecodehash"][8:])] += 1
                env = json.loads(dec) if stub is None else None
                if env is None:
                    out["vc"]["spilled"] += 1
                    continue
                data = env["data"]
                cd = cjson(data)
                vc = out["vc"]
                vc["n"] += 1
                vc["data"] += len(cd)
                dd = deflate_raw(cd)
                vc["data_deflate"] += dd
                out["vchist"][b2(dd)] += 1
                for key in data:
                    out["vckeys"][key] += 1
                for r in env["references"]:
                    vc["ref." + r["kind"]] += 1
                    if r["kind"] == "code":
                        en = r["encoding"]
                        if not (en["prefix"] and not en["uppercase"]):
                            vc["code.noncanonical"] += 1
                if data.get("proxyResolution") is not None:
                    vc["proxyResolution"] += 1
                continue
            if kind == "compilation":
                if stub is not None:
                    parts = blobs_local.get(stub["hash"], {})
                    if len(parts) != stub["parts"]:
                        out["pending_spill_cp"] += 1
                        continue
                    dec = b"".join(parts[i] for i in range(stub["parts"]))
                    out["cp"]["spilled_reassembled"] += 1
                env = json.loads(dec)
                data = env["data"]
                cp = out["cp"]
                cp["n"] += 1
                cp["decoded"] += len(dec)
                tot = 0
                fields = {}
                for name, val in data.items():
                    if name in ("creationBytecode", "runtimeBytecode") and isinstance(val, dict):
                        fields[name] = val
                    else:
                        fields[name] = val
                for name, val in fields.items():
                    c = cjson(val)
                    d = deflate_raw(c)
                    fc = out["cpfield"][name]
                    fc["n"] += 1
                    fc["json"] += len(c)
                    fc["deflate"] += d
                    fc["max_deflate"] = max(fc["max_deflate"], d)
                    if c == b"null":
                        fc["null"] += 1
                    out["cpfieldhist"][name][b2(d)] += 1
                    tot += d
                    if name in ("stdJsonInput", "stdJsonOutput", "metadata", "abi", "sources"):
                        out["cpdigest"][name][hashlib.sha256(c).hexdigest()[:16]] += 1
                whole = deflate_raw(cjson(data))
                cp["sum_field_deflate"] += tot
                cp["whole_deflate"] += whole
                out["cphist"][b2(tot)] += 1
                if tot > BUDGET:
                    cp["over_budget"] += 1
                refs = env["references"]
                cp["refs"] += len(refs)
                nsrc = 0
                for r in refs:
                    cp["ref." + r["kind"]] += 1
                    if r["kind"] == "sourcefile":
                        nsrc += 1
                    if r["kind"] == "code":
                        en = r["encoding"]
                        if not (en["prefix"] and not en["uppercase"]):
                            cp["code.noncanonical"] += 1
                        if not en["prefix"] and not en["uppercase"]:
                            cp["code.noprefix_lower"] += 1
                srcs = data.get("sources")
                ns = len(srcs) if isinstance(srcs, dict) else 0
                out["cpsources"][b2(ns)] += 1
                cp["sources_paths"] += ns
                cp["max_sources"] = max(cp["max_sources"], ns)
                cp["src_refs"] += nsrc
                cp["max_src_refs"] = max(cp["max_src_refs"], nsrc)
                refsz = deflate_raw(cjson(refs))
                cp["refs_deflate"] += refsz
                continue
    # serialize
    res = {}
    for k, v in out.items():
        if isinstance(v, defaultdict):
            res[k] = {a: dict(b) if isinstance(b, Counter) else b for a, b in v.items()}
        elif isinstance(v, Counter):
            res[k] = dict(v)
        else:
            res[k] = v
    res["code_size"] = list(out["code_size"].items())
    res["vc_creation"] = list(out["vc_creation"].items())
    res["vc_runtime"] = list(out["vc_runtime"].items())
    res["blobs"] = out["blobs"]
    return res


def job(args):
    path, outdir = args
    name = os.path.basename(path).replace(".jsonl", ".json")
    dest = os.path.join(outdir, name)
    if os.path.exists(dest):
        return name
    res = scan(path)
    tmp = dest + ".tmp"
    with open(tmp, "w") as f:
        json.dump(res, f)
    os.replace(tmp, dest)
    return name


if __name__ == "__main__":
    src, outdir = sys.argv[1], sys.argv[2]
    workers = int(sys.argv[3]) if len(sys.argv) > 3 else 8
    limit = int(sys.argv[4]) if len(sys.argv) > 4 else None
    os.makedirs(outdir, exist_ok=True)
    parts = sorted(os.path.join(src, p) for p in os.listdir(src) if p.endswith(".jsonl"))
    if limit:
        parts = parts[:limit]
    done = 0
    with Pool(workers) as pool:
        for name in pool.imap_unordered(job, [(p, outdir) for p in parts]):
            done += 1
            if done % 50 == 0 or done == len(parts):
                print(f"{done}/{len(parts)}", flush=True)
