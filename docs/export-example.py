"""Export verified, real processor output and its independently hydrated JSON for the docs."""
import argparse, base64, datetime, gzip, hashlib, json, pathlib, shutil, subprocess, tarfile
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--work', required=True, type=pathlib.Path, help='Directory with sourcify-response.json, input.ndjson, bundle/')
parser.add_argument('--captured-at', required=True)
parser.add_argument('--processor', type=pathlib.Path, default=pathlib.Path(__file__).resolve().parents[1]/'target/release/arkiv-entity-processor')
args = parser.parse_args()
root = pathlib.Path(__file__).resolve().parents[1]
work, bundle, dest = args.work, args.work/'bundle', root/'docs/data'
processor = args.processor.resolve()
subprocess.run([str(processor), 'verify', str(bundle)], check=True)
hydrated_lines = subprocess.check_output([str(processor), 'hydrate', str(bundle)], text=True).splitlines()
assert len(hydrated_lines) == 1, 'Example must hydrate exactly one contract'
hydrated = json.loads(hydrated_lines[0])
source = json.loads((work/'sourcify-response.json').read_text())
assert hydrated == source, 'Hydrated JSON differs from saved source'
inputs = (work/'input.ndjson').read_text().splitlines()
assert len(inputs) == 2 and json.loads(inputs[0]) == source
assert json.loads(inputs[1]) == {'type':'source-complete','records':1}
metadata = [json.loads(line) for line in (bundle/'metadata.jsonl').read_text().splitlines()]
seed_lines = [line for path in sorted((bundle/'entities').glob('*.jsonl')) for line in path.read_text().splitlines()]
seeds = [json.loads(line) for line in seed_lines]
assert len(metadata) == len(seeds)
audit = (bundle/'payloads.bin').read_bytes()
entities = []
for row, seed in zip(metadata, seeds):
    wire = base64.b64decode(seed['$payload'])
    body = gzip.decompress(wire) if seed['contentencoding'] == 'str:gzip' else wire
    assert body == audit[row['payloadOffset']:row['payloadOffset']+row['payloadBytes']]
    assert hashlib.sha256(body).hexdigest() == row['payloadSha256']
    entity = {key: row[key] for key in ['id','key','kind','contentType','payloadSha256','payloadBytes']}
    entity.update(wirePayloadBytes=len(wire), attributes={key:value for key,value in seed.items() if not key.startswith('$')},
                  payload=json.loads(body) if row['contentType']=='application/json' else '0x'+body.hex(), seed=seed)
    entities.append(entity)
contract = next(entity for entity in entities if entity['kind']=='verified_contract')
checks = [{'check':'hydrated contract equals complete captured source JSON','passed':True}]
checks += [{'check':key,'passed':hydrated[key] == value} for key,value in source.items()]
metrics = {'originalCompactJsonBytes':len(json.dumps(source,separators=(',',':'),ensure_ascii=False).encode()),
           'storedContractDecodedBytes':contract['payloadBytes'], 'storedContractWireBytes':contract['wirePayloadBytes'],
           'allEntitiesDecodedBytes':sum(entity['payloadBytes'] for entity in entities),
           'allEntitiesWireBytes':sum(entity['wirePayloadBytes'] for entity in entities),
           'seedJsonlBytes':sum(len((line+'\n').encode()) for line in seed_lines), 'entityCount':len(entities)}
provenance = {
    'address':source['address'], 'chainId':source['chainId'], 'capturedAt':args.captured_at,
    'generatedAt':datetime.datetime.now(datetime.timezone.utc).isoformat(),
    'sourceUrl':'https://sourcify.dev/server/v2/contract/1/'+source['address']+'?fields=all',
    'processorRevision':subprocess.check_output(['git','-C',str(root),'rev-parse','HEAD'],text=True).strip(),
    'processorWorkingTreeDirty':bool(subprocess.check_output(['git','-C',str(root),'status','--porcelain'],text=True)),
    'processorBinarySha256':hashlib.sha256(processor.read_bytes()).hexdigest(),
    'processorSourceSha256':{str(path.relative_to(root)):hashlib.sha256(path.read_bytes()).hexdigest() for path in sorted((root/'src').glob('*.rs'))},
    'inputMethod':'Saved full API response passed unchanged to process; hydrate reads the actual generated seed entities. No deployed Arkiv response is implied.',
    'owner':'0x1111111111111111111111111111111111111111',
    'ownerNote':'Illustrative CLI owner, not evidence of contract ownership.',
    'scopeNotes':[
        'Stored verification JSON is a v3 envelope with stripped data and explicit references. Hydration reconstructs every original JSON value.',
        'Source JSON formatting and object order need not match; strings, nulls, absent fields, arrays and unknown fields do.',
        'API provenance has no relational sourcifyIds. The Parquet adapter carries those separately as supplementary attributes.',
        'Live proxyResolution is preserved because supplied here. Offline Parquet has no live proxy resolution; its exported metadata text can differ from a live API string.',
        'Size measurements cover only this generated example. Decoded payloads, compressed wire payload bytes and JSONL framing are different measures; no dataset savings are claimed.'
    ],
    'metrics':metrics,
    'validation':{'hydratedEquality':True,'bundleVerify':'passed','entityPayloadHashes':len(entities),'fieldChecks':checks}
}
files = {'source':'data/sourcify-response.json','hydrated':'data/hydrated-contract.json','stored':'data/stored-contract.json','entities':'data/processor-entities.json',
         'input':'data/processor-input.ndjson','seed':'data/seed-entities.jsonl','provenance':'data/provenance.json','bundle':'data/example-bundle.tar.gz'}
dest.mkdir(parents=True,exist_ok=True)
for origin,name in [(work/'sourcify-response.json','sourcify-response.json'),(work/'input.ndjson','processor-input.ndjson')]:
    shutil.copyfile(origin,dest/name)
for name,value in [('processor-entities.json',entities),('provenance.json',provenance),('hydrated-contract.json',hydrated),('stored-contract.json',contract['payload'])]:
    (dest/name).write_text(json.dumps(value,indent=2,ensure_ascii=False)+'\n')
(dest/'seed-entities.jsonl').write_text('\n'.join(seed_lines)+'\n')
example = {**provenance,'source':source,'hydrated':hydrated,'entities':entities,'files':files}
(dest/'example-data.js').write_text('window.CONTRACT_EXAMPLE = '+json.dumps(example,separators=(',',':'),ensure_ascii=False)+';\n')
with tarfile.open(dest/'example-bundle.tar.gz','w:gz') as archive:
    for path in sorted(bundle.rglob('*')):
        if path.is_file(): archive.add(path,arcname=str(path.relative_to(bundle)))
print(json.dumps({'entities':len(entities),'hydratedEquality':True,'metrics':metrics},indent=2))
