(() => {
  const data = window.CONTRACT_EXAMPLE;
  if (!data) {
    document.querySelector('#contract-identity').textContent = 'The captured example data could not be loaded.';
    return;
  }
  const source = data.source;
  const entities = data.entities;
  const select = document.querySelector('#entity-select');
  let mode = 'payload';
  const labels = {verified_contract: 'Verification record', compilation: 'Compilation', sourcefile: 'Source file', code: 'Bytecode', blob: 'Large-payload part'};
  const priority = {verified_contract: 0, compilation: 1, sourcefile: 2, code: 3, blob: 4};
  const attr = (entity, name) => {
    const attrs = entity.attributes;
    if (Array.isArray(attrs)) {
      const a = attrs.find(item => item.key === name || item.name === name);
      return a?.value?.value ?? a?.value;
    }
    return attrs?.[name]?.value ?? attrs?.[name];
  };
  const kind = entity => entity.kind || attr(entity, 'kind');
  const pick = (object, keys) => Object.fromEntries(keys.filter(key => Object.hasOwn(object, key)).map(key => [key, object[key]]));
  const codeRoles = entity => ['creationBytecode', 'runtimeBytecode'].flatMap(group => ['onchainBytecode', 'recompiledBytecode'].filter(field => source[group]?.[field] != null && source[group][field].toLowerCase() === String(entity.payload).toLowerCase()).map(field => ({group, field})));
  const sorted = entities.map((entity, index) => ({entity, index})).sort((a, b) => (priority[kind(a.entity)] ?? 9) - (priority[kind(b.entity)] ?? 9));
  const counts = {};
  for (const {entity, index} of sorted) {
    const k = kind(entity);
    counts[k] = (counts[k] || 0) + 1;
    const option = document.createElement('option');
    option.value = String(index);
    option.textContent = k === 'code' ? `${codeRoles(entity).map(({group, field}) => `${group === 'creationBytecode' ? 'Creation' : 'Runtime'} ${field === 'onchainBytecode' ? 'onchain' : 'recompiled'}`).join(' + ')} · code` : `${labels[k] || k} · ${k}${entities.filter(e => kind(e) === k).length > 1 ? ` ${counts[k]}` : ''}`;
    select.append(option);
  }
  const name = source.compilation?.name || 'Verified contract';
  document.querySelector('#contract-identity').textContent = `${name} · Ethereum mainnet · ${data.address || source.address} · match ${source.matchId}`;
  document.querySelector('#source-download').href = data.files.source;
  document.querySelector('#entities-download').href = data.files.entities;
  document.querySelector('#seed-download').href = data.files.seed;
  const capture = document.querySelector('#capture-details');
  capture.append(document.createTextNode(`Captured ${data.capturedAt}. Processor revision ${data.processorRevision.slice(0, 7)}. ${entities.length} generated entities. Source: `));
  const sourceLink = document.createElement('a');
  sourceLink.href = data.sourceUrl;
  sourceLink.textContent = 'Sourcify fields=all response';
  capture.append(sourceLink);
  document.querySelector('#sample-checks').textContent = `${data.validation?.fieldChecks?.length || 0} source-to-entity field checks passed; ${data.validation?.entityPayloadHashes || 0} payload hashes checked. The full seed bundle passed processor verification.`;
  const provenanceLink = document.createElement('a');
  provenanceLink.href = data.files.provenance;
  provenanceLink.textContent = ' Detailed provenance and checks';
  document.querySelector('#sample-checks').append(provenanceLink);
  document.querySelector('#bundle-download').href = data.files.bundle;
  function relevant(entity) {
    const k = kind(entity);
    if (k === 'verified_contract') return source;
    if (k === 'compilation') return {...pick(source, ['compilation', 'sources', 'metadata', 'storageLayout', 'transientStorageLayout', 'userdoc', 'devdoc', 'sourceIds']), creationBytecode: pick(source.creationBytecode || {}, ['recompiledBytecode', 'sourceMap', 'linkReferences', 'cborAuxdata']), runtimeBytecode: pick(source.runtimeBytecode || {}, ['recompiledBytecode', 'sourceMap', 'linkReferences', 'immutableReferences', 'cborAuxdata'])};
    if (k === 'sourcefile') {
      const content = entity.payload?.content;
      const matches = Object.entries(source.sources || {}).filter(([, value]) => value.content === content);
      return {sources: Object.fromEntries(matches.length ? matches : Object.entries(source.sources || {}))};
    }
    if (k === 'code') {
      const values = {};
      for (const {group, field} of codeRoles(entity)) {values[group] ||= {}; values[group][field] = source[group][field];}
      return values;
    }
    return source;
  }
  function flattenForDisplay(value) {
    const entries = [];
    function visit(current, path) {
      if (current !== null && !Array.isArray(current) && typeof current === 'object' && Object.keys(current).length) {
        for (const [key, child] of Object.entries(current)) {
          const segment = /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(key)
            ? `${path ? '.' : ''}${key}` : `[${JSON.stringify(key)}]`;
          visit(child, path + segment);
        }
      } else {
        entries.push([path, current]);
      }
    }
    if (Object.keys(value).length === 0) return {};
    visit(value, '');
    return Object.fromEntries(entries);
  }
  function flattenAttributeInputs(inputs) {
    const entries = [];
    function visit(value, path) {
      if (Array.isArray(value) && value.length) {
        value.forEach((child, index) => visit(child, `${path}[${index}]`));
      } else if (value !== null && !Array.isArray(value) && typeof value === 'object' && Object.keys(value).length) {
        for (const [key, child] of Object.entries(value)) {
          const segment = /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(key) ? `.${key}` : `[${JSON.stringify(key)}]`;
          visit(child, path + segment);
        }
      } else {
        entries.push([path, value]);
      }
    }
    for (const [path, value] of Object.entries(inputs)) visit(value, path);
    return Object.fromEntries(entries);
  }
  function attributeComparison(entity) {
    const k = kind(entity);
    const inputs = {};
    const rows = [];
    const flattened = flattenForDisplay(source);
    function map(name, path, explanation = 'Copied with the attribute’s type prefix.') {
      if (!Object.hasOwn(entity.attributes, name)) return;
      if (Object.hasOwn(flattened, path)) inputs[path] = flattened[path];
      rows.push([name, `${path} — ${Object.hasOwn(flattened, path) ? explanation : 'absent in this response; ' + explanation}`]);
    }
    if (k === 'compilation' || k === 'verified_contract') {
      for (const [attribute, field] of [['compiler','compiler'],['compilerversion','compilerVersion'],['language','language'],['name','name']]) map(attribute, `compilation.${field}`, 'Text index; strings over 128 UTF-8 bytes are shortened.');
      map('evmversion', 'compilation.compilerSettings.evmVersion', 'Text index; missing value defaults to "default".');
      map('optimizer', 'compilation.compilerSettings.optimizer.enabled', 'Boolean index; missing value defaults to false.');
      map('optimizerruns', 'compilation.compilerSettings.optimizer.runs', 'Integer index; missing value defaults to 0.');
      map('viair', 'compilation.compilerSettings.viaIR', 'Boolean index; missing value defaults to false.');
    }
    if (k === 'verified_contract') {
      for (const [attribute, path, explanation] of [
        ['chainid','chainId','Decimal chain ID stored as u64.'], ['address','address','Address stored with checksum casing.'], ['match','match','Match status stored as text.'],
        ['verifiedat','verifiedAt','ISO timestamp converted to Unix seconds.'], ['matchid','matchId','Decimal ID stored as u64.'], ['isproxy','proxyResolution.isProxy','Boolean from this live response.'], ['proxytype','proxyResolution.proxyType','Text proxy type from this live response.'],
        ['blocknumber','deployment.blockNumber','Decimal block number stored as u64; missing value uses index sentinel 0.'], ['deployer','deployment.deployer','Checksummed address; missing value uses zero-address index sentinel.'],
        ['abihash','abi','Derived: SHA-256 of serialized ABI JSON.'], ['creationcodehash','creationBytecode.onchainBytecode','Derived: Keccak-256 of bytecode bytes.'], ['runtimecodehash','runtimeBytecode.onchainBytecode','Derived: Keccak-256 of bytecode bytes.']
      ]) map(attribute,path,explanation);
      rows.push(['compilationref','Generated reference to the compilation entity key. No direct Sourcify API field.']);
    }
    if (k === 'sourcefile' || k === 'code') {
      Object.assign(inputs, flattenForDisplay(relevant(entity)));
      rows.push(['hash', k === 'sourcefile' ? 'Derived: SHA-256 of the UTF-8 source content shown on the left.' : 'Derived: Keccak-256 of the bytecode bytes shown on the left.']);
      rows.push(['size', k === 'sourcefile' ? 'Derived: source content length in UTF-8 bytes.' : 'Derived: bytecode length in bytes, excluding the hex prefix.']);
    }
    if (k === 'compilation') rows.push(['hash','Derived fingerprint: compiler/version/language/FQN/settings, source path–hash map and four bytecode hashes. Not a direct Sourcify field or full-payload hash. Left shows compiler index inputs only; payload mode and downloads retain other fingerprint inputs.']);
    rows.push(['projectid, kind','Generated dataset namespace and entity kind; no direct Sourcify field.']);
    rows.push(['contentencoding, originalcontenttype, payloadsha256, stagingid, uploadschema','Generated storage/encoding metadata and integrity hashes; no direct Sourcify fields. Included on the right exactly as written to the seed.']);
    return {inputs, rows};
  }
  function sortedForDisplay(value) {
    if (Array.isArray(value)) return value.map(sortedForDisplay);
    if (value !== null && typeof value === 'object') {
      return Object.fromEntries(Object.keys(value).sort().map(key => [key, sortedForDisplay(value[key])]));
    }
    return value;
  }
  function showJSON(node, value) {
    const json = JSON.stringify(sortedForDisplay(value), null, 2);
    const pattern = /"(?:\\.|[^"\\])*"\s*:|"(?:\\.|[^"\\])*"|\b(?:true|false|null|-?\d+(?:\.\d+)?)\b/g;
    node.replaceChildren();
    let last = 0;
    for (const match of json.matchAll(pattern)) {
      node.append(document.createTextNode(json.slice(last, match.index)));
      const span = document.createElement('span');
      span.className = match[0].endsWith(':') ? 'json-key' : match[0].startsWith('"') ? 'json-string' : 'json-literal';
      span.textContent = match[0];
      node.append(span);
      last = match.index + match[0].length;
    }
    node.append(document.createTextNode(json.slice(last)));
  }
  function render() {
    const entity = entities[Number(select.value)];
    const k = kind(entity);
    const descriptions = {
      verified_contract: 'The verification payload preserves the entire input JSON, including nested sources and bytecode. The compilationref and code-hash attributes provide additional links and indexes.',
      compilation: 'The compilation keeps compiler settings and artifacts. Its sources map replaces each source body with a SHA-256 reference to the source-file entity; recompiled bytecode is referenced by hash.',
      sourcefile: 'The shared source-file entity stores source text inside a JSON envelope; the complete verification payload retains it too. The compilation retains the file path and points to this entity’s hash attribute: SHA-256 of the exact UTF-8 source text.',
      code: 'Sourcify represents bytecode as hex; this entity stores the actual bytes (displayed as hex here). Its hash attribute is Keccak-256. Identical onchain and recompiled bytecode can share one entity.',
      blob: 'This entity holds one slice of a large payload. The parent’s spill reference identifies the complete body; part attributes determine the slice order.'
    };
    const equal = JSON.stringify(sortedForDisplay(source)) === JSON.stringify(sortedForDisplay(entity.payload));
    document.querySelector('#relationship').textContent = (descriptions[k] || '') + (k === 'verified_contract' ? (equal ? ' Comparison: all JSON values equal.' : ' Comparison: payload differs from the captured source.') : '');
    document.querySelector('#entity-kind').textContent = k;
    document.querySelector('#entity-key').textContent = `Generated entity key: ${entity.key}`;
    const attributesMode = mode === 'attributes';
    document.querySelector('#source-heading').textContent = attributesMode ? 'Sourcify · flat attribute inputs' : 'Sourcify · nested fields';
    document.querySelector('#entity-heading').textContent = attributesMode ? 'Arkiv · attributes' : 'Arkiv · payload';
    document.querySelector('#source-caption').textContent = attributesMode ? 'Actual source inputs. Derived hashes and processor metadata have no direct field equivalent.' : (k === 'verified_contract' ? 'Complete captured Sourcify response; no fields omitted.' : 'Actual nested source-field excerpt. Download the full response above.');
    document.querySelector('#entity-caption').textContent = attributesMode ? 'Actual typed index attributes, including generated storage metadata. See mappings below.' : 'Actual decoded payload. Binary bytecode is displayed as hex.';
    const comparison = attributeComparison(entity);
    showJSON(document.querySelector('#source-json'), attributesMode ? flattenAttributeInputs(comparison.inputs) : relevant(entity));
    showJSON(document.querySelector('#entity-json'), attributesMode ? entity.attributes : entity.payload);
    const mapping = document.querySelector('#attribute-mapping');
    mapping.hidden = !attributesMode;
    const rows = document.querySelector('#mapping-rows');
    rows.replaceChildren();
    for (const [name, explanation] of comparison.rows) {
      const row = document.createElement('tr');
      for (const value of [name, explanation]) {const cell = document.createElement('td'); cell.textContent = value; row.append(cell);}
      rows.append(row);
    }
    for (const button of document.querySelectorAll('[data-mode]')) button.setAttribute('aria-pressed', String(button.dataset.mode === mode));
  }
  select.addEventListener('change', render);
  for (const button of document.querySelectorAll('[data-mode]')) button.addEventListener('click', () => {mode = button.dataset.mode; render();});
  render();
})();
