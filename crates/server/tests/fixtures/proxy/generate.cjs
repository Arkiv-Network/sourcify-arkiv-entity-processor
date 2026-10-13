// Offline developer fixture generator. Arguments: directory containing installed
// @shazow/whatsabi 0.29.0 (npm install --prefix /tmp/reference @shazow/whatsabi@0.29.0).
// Normal Rust tests use the committed outputs and need neither Node nor network.
const {createRequire}=require('node:module');
const fs=require('node:fs');
const path=require('node:path');
const load=createRequire(path.resolve(process.argv[2], 'package.json'));
const packageDir=path.dirname(path.dirname(load.resolve('@shazow/whatsabi')));
const version=JSON.parse(fs.readFileSync(path.join(packageDir,'package.json'))).version;
if(version!=='0.29.0')throw Error(`Unexpected reference version ${version}`);
const {disasm}=require(path.join(packageDir,'lib.cjs/disasm.js'));
const {slots,slotResolvers}=require(path.join(packageDir,'lib.cjs/proxies.js'));
const upstream=JSON.parse(fs.readFileSync(path.join(__dirname,'upstream-bytecodes.json')));
const rows=[];
function add(name,code,upstreamName){
 const p=disasm(code);
 rows.push({name,...(upstreamName?{upstream:upstreamName}:{code}),factory:p.isFactory,
   proxies:p.proxies.map(p=>({name:p.name,...(p.resolvedAddress?{address:p.resolvedAddress}:{})}))});
}
for(const [name,code]of Object.entries(upstream))add(`upstream/${name}`,code,name);
const bare=s=>s.slice(2), push=s=>'7f'+bare(s);
for(const [slot,resolver]of Object.entries(slotResolvers)){
 add(`slot/${resolver.name}/${slot.slice(-4)}`, '0x'+push(slot)+'00');
 add(`aux/${resolver.name}/${slot.slice(-4)}`, '0x00fe'+bare(slot)+'0000');
}
add('sequence','0x30545af4');
add('missing_history_fixed','0x5af4');
add('missing_history_jump_boundary','0x5601f0'+push(slots.EIP1967_IMPL));
add('missing_history_codecopy_offset','0x5f390001'+push(slots.PROXIABLE));
add('sequence_push0_not_push','0x5f5af4');
for(const width of [1,9,10,11,12,19,20,21,32])add(`fixed_push${width}`,'0x'+(0x5f+width).toString(16)+'12'.repeat(width)+'5af4');
add('factory','0xf0'+push(slots.EIP1967_IMPL));
add('factory_create2','0xf5'+push(slots.EIP1967_IMPL));
add('factory_inside_push_is_data','0x60f0'+push(slots.EIP1967_IMPL));
add('first_type_dedup_quirk','0x'+push(slots.EIP1967_IMPL)+push(slots.PROXIABLE)+push(slots.PROXIABLE)+push(slots.EIP1967_BEACON));
add('aux_table_order','0x00fe'+bare(slots.MATIC_IMPL)+bare(slots.PROXIABLE)+bare(slots.EIP1967_IMPL));
add('aux_suppressed_when_instruction_found','0x'+push(slots.PROXIABLE)+'00fe'+bare(slots.EIP1967_IMPL));
add('aux_preimage','0x00fe'+Buffer.from('matic.network.proxy.implementation').toString('hex')+'00');
add('aux_nibble_match','0x00fe0'+bare(slots.EIP1967_IMPL)+'0');
add('cbor_excludes_slot','0x00fea15820'+bare(slots.EIP1967_IMPL)+'0023');
add('invalid_cbor_keeps_slot','0x00fe005820'+bare(slots.EIP1967_IMPL)+'0023');
add('halt_push32_before_boundary','0x00'+push(slots.EIP1967_IMPL));
add('halt_factory_not_executed','0x00f0'+push(slots.EIP1967_IMPL));
add('jump_target_keeps_code','0x6101005601'+push(slots.EIP1967_IMPL));
add('jump_boundary_data','0x60005601'+bare(slots.EIP1967_IMPL));
add('truncated_push','0x7f360894');
add('empty','0x');
const selectors=['f77c4791','92eefe9b','51720b41'];
const table=xs=>xs.map(s=>'63'+s+'1461010057').join('');
add('livepeer_exact','0x'+table(selectors)+'f4');
add('livepeer_subset_is_not_proxy','0x'+table(selectors.slice(0,2))+'f4');
add('livepeer_superset_is_not_proxy','0x'+table([...selectors,'12345678'])+'f4');
add('livepeer_without_delegate_is_not_proxy','0x'+table(selectors));
add('livepeer_slot_precedence','0x'+table(selectors)+'f4'+push(slots.PROXIABLE));
add('runtime_reset_drops_init_factory','0xf06008600039f30001'+push(slots.EIP1967_IMPL));
add('creation_prefix_clone','0x3d602d80600a3d3981f3'+upstream.EIP1167Proxy.slice(2));
fs.writeFileSync(path.join(__dirname,'whatsabi-0.29.0.json'),JSON.stringify({version,rows},null,2)+'\n');
console.log(`Generated ${rows.length} independent detector vectors`);
