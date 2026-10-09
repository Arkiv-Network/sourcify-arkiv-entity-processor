import base64,datetime,gzip,hashlib,json,pathlib,shutil,subprocess
# Export the saved example after running the processor; standard library only.
import argparse
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--work',required=True,type=pathlib.Path,help='Directory with sourcify-response.json, input.ndjson, bundle/')
parser.add_argument('--captured-at',required=True,help='UTC timestamp of original API capture')
parser.add_argument('--processor',type=pathlib.Path,default=pathlib.Path(__file__).resolve().parents[1]/'target/release/arkiv-entity-processor',help='Built processor binary used to verify the bundle')
args=parser.parse_args()
root=pathlib.Path(__file__).resolve().parents[1]; work=args.work; dest=root/'docs/data'; bundle=work/'bundle'
subprocess.run([str(args.processor), 'verify', str(bundle)], check=True)
dest.mkdir(parents=True,exist_ok=True)
source=json.loads((work/'sourcify-response.json').read_text()); meta=[json.loads(x) for x in (bundle/'metadata.jsonl').read_text().splitlines()]; seeds=[json.loads(x) for x in (bundle/'entities/000001.jsonl').read_text().splitlines()]; entities=[]; raw=(bundle/'payloads.bin').read_bytes()
assert len(meta) == len(seeds) == 6, 'Expected the six-entity SafeProxy sample'
input_lines = (work/'input.ndjson').read_text().splitlines()
assert len(input_lines) == 2 and json.loads(input_lines[0]) == source
assert json.loads(input_lines[1]) == {'type':'source-complete','records':1}
for m,s in zip(meta,seeds):
 b=base64.b64decode(s['$payload']); b=gzip.decompress(b) if s['$contentType']=='application/gzip' else b
 assert b==raw[m['payloadOffset']:m['payloadOffset']+m['payloadBytes']]
 assert hashlib.sha256(b).hexdigest()==m['payloadSha256']
 e={k:m[k] for k in ['id','key','kind','contentType','payloadSha256','payloadBytes']};e['attributes']={k:v for k,v in s.items() if not k.startswith('$')};e['payload']=json.loads(b) if m['contentType']=='application/json' else '0x'+b.hex();e['seed']=s;entities.append(e)
vc=next(e for e in entities if e['kind']=='verified_contract');cp=next(e for e in entities if e['kind']=='compilation');sf=next(e for e in entities if e['kind']=='sourcefile')
checks=[]
def check(label,a,b):
 assert a==b,label
 checks.append({'check':label,'passed':True,'valueSha256':hashlib.sha256(json.dumps(a,sort_keys=True,separators=(',',':'),ensure_ascii=False).encode()).hexdigest()})
check('complete contract payload equals full source JSON', source, vc['payload'])
for k in source: check(k, source[k], vc['payload'][k])
for k,v in source['compilation'].items():check('compilation.'+k,v,cp['payload'][k])
for k in ['metadata','storageLayout','transientStorageLayout','userdoc','devdoc','sourceIds']:check(k,source[k],cp['payload'][k])
for path,v in source['sources'].items():
 check('sources.'+path+'.content',v['content'],sf['payload']['content']);check('source content SHA-256', '0x'+hashlib.sha256(v['content'].encode()).hexdigest(),cp['payload']['sources'][path])
for prefix in ['creation','runtime']:
 for k in ['sourceMap','linkReferences','cborAuxdata']+(['immutableReferences'] if prefix=='runtime' else []):check(prefix+'Bytecode.'+k,source[prefix+'Bytecode'][k],cp['payload'][prefix+'CodeArtifacts'][k])
 for k in ['transformations','transformationValues']:check(prefix+'Bytecode.'+k,source[prefix+'Bytecode'][k],vc['payload'][prefix+'Bytecode'][k])
 for a,b in [('Onchain','onchainBytecode'),('Recompiled','recompiledBytecode')]:
  ent=next(e for e in entities if e['kind']=='code' and e['payload']==source[prefix+'Bytecode'][b]);check(prefix+'Bytecode.'+b,source[prefix+'Bytecode'][b],ent['payload'])
check('resolved compilation entity key',vc['attributes']['compilationref'],'key:'+cp['key'])
provenance={'address':source['address'],'chainId':source['chainId'],'capturedAt':args.captured_at,'sourceUrl':'https://sourcify.dev/server/v2/contract/1/'+source['address']+'?fields=all','processorRevision':subprocess.check_output(['git','-C',str(root),'rev-parse','HEAD'],text=True).strip(),'inputMethod':'The saved live Sourcify API JSON was passed unchanged as one NDJSON record to the current processor CLI, followed by a source-complete marker. This is an API-input illustration, not a production Parquet export or a deployed Arkiv response.','owner':'0x1111111111111111111111111111111111111111','ownerNote':'Explicit illustrative owner supplied to CLI; it is not a contract owner or proof of deployment.','scopeNotes':['Sourcify API has no sourcifyIds; these are therefore absent. The Parquet adapter carries source database IDs outside the record as wrapper provenance and entity attributes.','proxyResolution was returned by the live Sourcify API and preserved. The offline Parquet adapter cannot resolve current proxies.','The complete contract payload equals the full supplied API JSON, including signatures, stdJsonInput and stdJsonOutput. Object-key order and JSON whitespace may differ. The Parquet adapter reconstructs those fields from exported tables; exact snapshot-to-live equality still has metadata-text and live-proxy limitations.','Code payloads are binary bytes shown here as hexadecimal. JSON payloads are decoded from actual gzip seed output.','Entity keys are processor-generated seed keys. The bundle has not been deployed to Arkiv.'],'validation':{'fullPayloadEquality':True,'bundleVerify':'passed','entityPayloadHashes':len(entities),'fieldChecks':checks}}
files={'source':'data/sourcify-response.json','entities':'data/processor-entities.json','input':'data/processor-input.ndjson','seed':'data/seed-entities.jsonl','provenance':'data/provenance.json','bundle':'data/example-bundle.tar.gz'}
for src,name in [(work/'sourcify-response.json','sourcify-response.json'),(work/'input.ndjson','processor-input.ndjson'),(bundle/'entities/000001.jsonl','seed-entities.jsonl')]:shutil.copyfile(src,dest/name)
(dest/'processor-entities.json').write_text(json.dumps(entities,indent=2,ensure_ascii=False)+'\n');(dest/'provenance.json').write_text(json.dumps(provenance,indent=2)+'\n')
example={**provenance,'source':source,'entities':entities,'files':files};(dest/'example-data.js').write_text('window.CONTRACT_EXAMPLE = '+json.dumps(example,separators=(',',':'),ensure_ascii=False)+';\n')
import tarfile
with tarfile.open(dest/'example-bundle.tar.gz','w:gz') as t:
 for p in sorted(bundle.rglob('*')):
  if p.is_file():t.add(p,arcname=str(p.relative_to(bundle)))
print('Wrote',len(entities),'entities;',len(checks),'field checks passed;',sum(p.stat().st_size for p in dest.iterdir()),'bytes')
