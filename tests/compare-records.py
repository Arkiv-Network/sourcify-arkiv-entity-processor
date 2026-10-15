"""Compare source records (stream.ts NDJSON, {"record": ...}) with GET /v2/contract/1/<address> of a Sourcify API,
field by field.

    python3 -I tests/compare-records.py RECORDS.ndjson API_BASE [--pause SECONDS] [--every N]

proxyResolution is computed live by the API and not part of a record, so it is not requested. --every N checks every
Nth record. Against a public API (e.g. https://sourcify.dev/server) keep requests sparse: --pause 1 or more, and a
sample (--every). Exits 1 if any record differs or cannot be fetched.
"""
import json, sys, time, urllib.error, urllib.request

FIELDS = ("creationBytecode,runtimeBytecode,deployment,sources,compilation,abi,metadata,storageLayout,transientStorageLayout,"
          "userdoc,devdoc,sourceIds,additionalInput,stdJsonInput,stdJsonOutput,signatures")
UA = {"User-Agent": "sourcify-arkiv-entity-processor record check (https://github.com/Arkiv-Network/sourcify-arkiv-entity-processor)"}


def get(url):
    for attempt in range(6):
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=120) as r:
                return r.status, json.loads(r.read())
        except urllib.error.HTTPError as e:
            if e.code == 429:
                time.sleep(int(e.headers.get("Retry-After") or 30))
                continue
            return e.code, None
    return 429, None


def diff(a, b, path=""):
    if type(a) != type(b):
        return [f"{path or '/'} (type {type(a).__name__} != {type(b).__name__})"]
    if isinstance(a, dict):
        out = []
        for k in sorted(set(a) | set(b)):
            if k not in a or k not in b:
                out.append(f"{path}/{k} (only {'api' if k not in a else 'record'})")
            else:
                out.extend(diff(a[k], b[k], f"{path}/{k}"))
        return out
    if isinstance(a, list):
        if len(a) != len(b):
            return [f"{path} (length {len(a)} != {len(b)})"]
        return [d for i, (x, y) in enumerate(zip(a, b)) for d in diff(x, y, f"{path}[{i}]")]
    return [] if a == b else [path]


def main():
    if len(sys.argv) < 3 or len(sys.argv) % 2 == 0:
        sys.exit(__doc__)
    records, base = sys.argv[1], sys.argv[2].rstrip("/")
    opts = dict(zip(sys.argv[3::2], sys.argv[4::2]))
    pause, every = float(opts.get("--pause", 0)), int(opts.get("--every", 1))
    checked = identical = differ = errors = 0
    paths = {}
    for n, line in enumerate(open(records)):
        item = json.loads(line)
        if "record" not in item or n % every:
            continue
        record = item["record"]
        status, api = get(f"{base}/v2/contract/1/{record['address']}?fields={FIELDS}")
        checked += 1
        if status != 200:
            errors += 1
            print(f"{record['matchId']} {record['address']} HTTP {status}")
        else:
            d = diff(record, api)
            if d:
                differ += 1
                for p in d:
                    key = p.split("[")[0].split(" (")[0]
                    paths[key] = paths.get(key, 0) + 1
                print(f"{record['matchId']} {record['address']} DIFFERS: {d[:6]}")
            else:
                identical += 1
        if pause:
            time.sleep(pause)
    print(json.dumps({"checked": checked, "identical": identical, "differ": differ, "http_errors": errors, "differing_paths": paths}))
    sys.exit(1 if differ or errors else 0)


if __name__ == "__main__":
    main()
