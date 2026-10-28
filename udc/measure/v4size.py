"""Size of the full v3 seed bundle in the proposed UDC layout (docs/udc-proposal.md, "Size and cost").

Usage: python3 -I v4size.py ENTITIES_DIR OUT.json [workers]

Streams every entity part of a v3 processor seed bundle and, per entity, computes the UDC entity the
prototype (src/udc.rs) writes for it: the same attributes, raw DEFLATE level 9 per bytes value
(identity when smaller), the 120,000-byte budget with chunking, and storage units as
arkiv-state-db-experiment charges rent: 1,460 per entity (its system cells) plus, per user
attribute, name + value + 2 bytes and 128 more when indexed. It also prices the v3 entities ported
naively (payload as one unindexed bytes attribute, content type as a str, every v3 attribute
indexed). Spilled payloads are joined from their blob parts in the same part file (dependencies
precede their parent); the count of payloads that could not be joined is reported.
"""
import base64, gzip, json, os, sys, zlib
from collections import Counter, defaultdict
from multiprocessing import Pool

BUDGET = 120_000
CHUNK = 120_000
EMPTY = 1460
PREFIX = "sourcify.v4"
HOT = ("abi", "compilation")
WIDTH = {"bool": 1, "i32": 4, "u64": 8, "u256": 32, "bytes32": 32, "addr": 20, "key": 32}


def deflate(b):
    c = zlib.compressobj(9, zlib.DEFLATED, -15, 9)
    return len(c.compress(b) + c.flush())


def enc(raw):
    """Length of the encoded bytes value (codec byte included)."""
    return 1 + min(deflate(raw), len(raw))


def cj(v):
    return json.dumps(v, separators=(",", ":"), sort_keys=True, ensure_ascii=False).encode()


def cell(name, length, indexed):
    return len(name) + length + 2 + (128 if indexed else 0)


class Out:
    def __init__(self):
        self.c = defaultdict(Counter)
        self.hist = defaultdict(Counter)

    def entity(self, kind, cells, bytes_values):
        units = EMPTY + sum(cells)
        k = self.c[kind]
        k["n"] += 1
        k["units"] += units
        k["bytes"] += bytes_values
        k["attrs"] += len(cells)
        self.hist[kind][units.bit_length()] += 1
        return units

    def place(self, fields, pinned=()):
        """fields: {name: encoded length}; returns attribute cells and bytes after chunking."""
        fields = dict(fields)
        chunked = set()
        while sum(fields.values()) > BUDGET:
            cand = [n for n in fields if n not in chunked]
            pick = max(cand, key=lambda n: (n not in pinned, fields[n]))
            size = fields[pick]
            parts = -(-size // CHUNK)
            for i in range(parts):
                part = min(CHUNK, size - i * CHUNK)
                self.entity("chunk", [cell("arkiv.schema", len(PREFIX) + 6, True), cell("data", part, False)], part)
            self.c["chunked"][pick] += 1
            fields[pick] = 37 + 32 * parts
            chunked.add(pick)
        return [cell(n, l, False) for n, l in fields.items()], sum(fields.values())


def scan(path):
    o = Out()
    naive = Counter()
    blobs = defaultdict(dict)
    missing = Counter()
    for line in open(path, "rb"):
        e = json.loads(line)
        kind = e["kind"][4:]
        raw = base64.b64decode(e["$payload"])
        dec = gzip.decompress(raw) if e["contentencoding"][4:] == "gzip" else raw
        # Naive port: every v3 attribute indexed, payload and content type as stored attributes.
        cells = 0
        for name, v in e.items():
            if name.startswith("$"):
                continue
            t, val = v.split(":", 1)
            cells += cell(name, WIDTH.get(t, len(val.encode())), True)
        cells += cell("payload", len(raw), False) + cell("contentType", len(e["$contentType"]), False)
        naive["n"] += 1
        naive["units"] += EMPTY + cells
        naive["bytes"] += len(raw)
        if kind == "blob":
            blobs[e["hash"][8:]][int(e["part"][4:])] = dec
            continue
        if e["originalcontenttype"][4:] == "application/json" and dec.startswith(b'{"$spill"'):
            stub = json.loads(dec)["$spill"]
            parts = blobs.get(stub["hash"], {})
            if len(parts) != stub["parts"]:
                missing[kind] += 1
                continue
            dec = b"".join(parts[i] for i in range(stub["parts"]))
        schema = lambda k: cell("arkiv.schema", len(PREFIX) + 1 + len(k), True)
        if kind == "sourcefile":
            content = json.loads(dec)["content"].encode()
            attrs, b = o.place({"content": enc(content)})
            o.entity("source", [schema("source"), cell("hash", 32, True)] + attrs, b)
        elif kind == "code":
            attrs, b = o.place({"code": enc(dec)})
            o.entity("code", [schema("code"), cell("hash", 32, True)] + attrs, b)
        elif kind == "compilation":
            env = json.loads(dec)
            data, refs = env["data"], env["references"]
            slots = {}
            for lane, slot in (("creationBytecode", "creation"), ("runtimeBytecode", "runtime")):
                for r in refs:
                    if r["kind"] == "code" and r["path"] == f"/{lane}/recompiledBytecode":
                        slots[slot] = r["hash"]
            sources, out_refs = [], []
            for r in refs:
                if r["kind"] == "local":
                    out_refs.append({"kind": "local", "path": r["path"], "pointer": r["pointer"]})
                elif r["kind"] == "sourcefile":
                    if r["hash"] not in sources:
                        sources.append(r["hash"])
                    out_refs.append({"kind": "source", "path": r["path"], "source": sources.index(r["hash"])})
                else:
                    slot = next(s for s, h in slots.items() if h == r["hash"])
                    out_refs.append({"kind": "code", "path": r["path"], "code": slot, "encoding": r["encoding"]})
            groups = {n: data[n] for n in HOT if n in data}
            art = {n: v for n, v in data.items() if n not in HOT}
            if art:
                groups["artifacts"] = art
            stored = {n: cj(v) for n, v in groups.items()}
            import hashlib
            manifest = {"codes": slots, "digests": {n: "0x" + hashlib.sha256(b).hexdigest() for n, b in stored.items()},
                        "refs": out_refs, "sources": sources}
            fields = {"manifest": enc(cj(manifest))}
            fields.update({n: enc(b) for n, b in stored.items()})
            if sources:
                fields["sourcerefs"] = 1 + 32 * len(sources)
            attrs, b = o.place(fields, pinned=("manifest", "sourcerefs"))
            cells = [schema("compilation"), cell("hash", 32, True)] + attrs
            cells += [cell(f"{s}coderef", 32, False) for s in slots]
            o.entity("compilation", cells, b)
            o.c["compilation.fields"]["manifest"] += fields["manifest"]
            for n in stored:
                o.c["compilation.fields"][n] += fields[n]
            o.c["compilation.fields"]["sourcerefs"] += fields.get("sourcerefs", 0)
        elif kind == "verified_contract":
            env = json.loads(dec)
            data = env["data"]
            a = {n: v.split(":", 1) for n, v in e.items() if not n.startswith("$")}
            cells = [schema("contract")]
            for n in ("address", "chainid", "matchid", "verifiedat", "match", "name", "compilerversion", "language",
                      "evmversion", "optimizer", "viair", "deployer", "blocknumber", "abihash", "compilationref",
                      "creationcodehash", "runtimecodehash", "isproxy", "proxytype"):
                if n in a:
                    t, val = a[n]
                    cells.append(cell(n, WIDTH.get(t, len(val.encode())), True))
            cells.append(cell("optimizerruns", 4, True))
            inline = 0
            for r in env["references"]:
                if r["kind"] == "code":
                    fixed = r["path"] in ("/creationBytecode/onchainBytecode", "/runtimeBytecode/onchainBytecode")
                    if fixed and r["encoding"] == {"prefix": True, "uppercase": []}:
                        cells.append(cell("creationcoderef" if r["path"].startswith("/creation") else "runtimecoderef", 32, False))
                    else:
                        inline += 1
                elif r["kind"] == "sourcefile":
                    inline += 1
            o.c["contract.inline_refs"]["n"] += inline
            attrs, b = o.place({"data": enc(cj(data))})
            o.entity("contract", cells + attrs, b)
            o.hist["contract.data"][b.bit_length()] += 1
    return {"c": {k: dict(v) for k, v in o.c.items()}, "hist": {k: dict(v) for k, v in o.hist.items()},
            "naive": dict(naive), "missing": dict(missing)}


if __name__ == "__main__":
    src, dest = sys.argv[1], sys.argv[2]
    workers = int(sys.argv[3]) if len(sys.argv) > 3 else 8
    parts = sorted(os.path.join(src, p) for p in os.listdir(src) if p.endswith(".jsonl"))
    if len(sys.argv) > 4:
        parts = parts[: int(sys.argv[4])]
    total = {"c": defaultdict(Counter), "hist": defaultdict(Counter), "naive": Counter(), "missing": Counter()}
    with Pool(workers) as pool:
        for i, r in enumerate(pool.imap_unordered(scan, parts), 1):
            for k, v in r["c"].items():
                total["c"][k].update(v)
            for k, v in r["hist"].items():
                total["hist"][k].update({int(a): b for a, b in v.items()})
            total["naive"].update(r["naive"])
            total["missing"].update(r["missing"])
            if i % 100 == 0 or i == len(parts):
                print(f"{i}/{len(parts)}", flush=True)
    kinds = ("contract", "compilation", "source", "code", "chunk")
    units = sum(total["c"][k]["units"] for k in kinds)
    summary = {"parts": len(parts), "kinds": {k: dict(total["c"][k]) for k in kinds},
               "entities": sum(total["c"][k]["n"] for k in kinds), "units": units,
               "bytes": sum(total["c"][k]["bytes"] for k in kinds),
               "compilationFields": dict(total["c"]["compilation.fields"]), "chunkedFields": dict(total["c"]["chunked"]),
               "contractInlineRefs": total["c"]["contract.inline_refs"]["n"],
               "naiveV3": dict(total["naive"]), "unjoinedSpills": dict(total["missing"]),
               "hist": {k: dict(sorted(v.items())) for k, v in total["hist"].items()}}
    json.dump(summary, open(dest, "w"), indent=1)
    print(json.dumps({k: summary[k] for k in ("kinds", "entities", "units", "bytes", "chunkedFields", "naiveV3", "unjoinedSpills", "contractInlineRefs")}, indent=1))
