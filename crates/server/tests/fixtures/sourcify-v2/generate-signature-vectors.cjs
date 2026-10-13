// Regenerate offline with NODE_PATH pointing to an ethers@6.17.0 installation.
const {Fragment,id,version}=require('ethers');
const fs=require('fs');
const cases=[
{name:'standard_types_and_overloads',abi:[{type:'function',name:'transfer',inputs:[{type:'address'},{type:'uint'}],outputs:[{type:'bool'}]},{type:'function',name:'transfer',inputs:[{type:'bytes32'},{type:'int'}]},{type:'function',name:'transfer',inputs:[{type:'address'},{type:'uint256'}]},{type:'constructor',inputs:[{type:'uint256'}]},{type:'fallback'},{type:'receive'}]},
{name:'tuple_arrays',abi:[{type:'function',name:'f',inputs:[{type:'tuple[][02]',components:[{type:'uint'},{type:'tuple',components:[{type:'address'},{type:'bytes32[]'}]}]},{type:'int8[0]'}],outputs:[]}]},
{name:'event_error_anonymous',abi:[{type:'event',name:'Transfer',anonymous:false,inputs:[{type:'address',indexed:true},{type:'address',indexed:true},{type:'uint256',indexed:false}]},{type:'event',name:'Anonymous',anonymous:true,inputs:[{type:'uint256',indexed:true}]},{type:'error',name:'Unauthorized',inputs:[{type:'address'}]},{type:'error',name:'NoArgs'}]},
{name:'invalid_fragments',abi:[{type:'function',name:'library',inputs:[{type:'MyLibrary.Data'}]},{type:'function',name:'badOutput',inputs:[],outputs:[{type:'MyLibrary.Data'}]},{type:'function',name:'bad-width',inputs:[]},{type:'function',name:'badWidth',inputs:[{type:'uint7'}]},{type:'function',name:'badBytes',inputs:[{type:'bytes33'}]},{type:'function',name:'valid',inputs:[]}]},
{name:'empty',abi:[]}
];
for(const c of cases){
 const groups={function:new Map(),event:new Map(),error:new Map()};
 for(const item of c.abi){let f;try{f=Fragment.from(item)}catch{continue}if(!Object.hasOwn(groups,f.type))continue;const signature=f.format('sighash');const hash=id(signature);groups[f.type].set(signature,{signature,signatureHash4:hash.slice(0,10),signatureHash32:hash});}
 c.expected=Object.fromEntries(Object.entries(groups).map(([k,m])=>[k,[...m.keys()].sort().map(s=>m.get(s))]));
}
const out={provenance:{ethers:version,sourcifyCommit:'6048af7dfb36a3f5b7b2b30ede5daca3e701f7b0',method:'Fragment.from(item), format("sighash"), id(signature); ignore invalid fragments, group, deduplicate, sort, as Sourcify signature-util.ts and database-util.ts.'},cases};
fs.writeFileSync(require('path').join(__dirname, 'signature-vectors.json'),JSON.stringify(out,null,2)+'\n');
