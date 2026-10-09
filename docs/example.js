(() => {
  const data = window.CONTRACT_EXAMPLE;
  if (!data) {
    document.querySelector('#contract-identity').textContent = 'The captured example data could not be loaded.';
    return;
  }
  const source = data.source;
  const entities = data.entities;
  const select = document.querySelector('#entity-select');
  const showAll = document.querySelector('#show-all-source');
  let view = 'payload';
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
    if (k === 'verified_contract') return {...pick(source, ['chainId', 'address', 'matchId', 'verifiedAt', 'match', 'creationMatch', 'runtimeMatch', 'abi', 'compilation', 'deployment', 'proxyResolution', 'additionalInput']), creationBytecode: pick(source.creationBytecode || {}, ['transformations', 'transformationValues']), runtimeBytecode: pick(source.runtimeBytecode || {}, ['transformations', 'transformationValues'])};
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
      verified_contract: 'The verification record keeps the contract’s identity, match result and ABI. Its compilationref attribute links to the compilation entity; codeRefs identifies bytecode by content hash.',
      compilation: 'The compilation keeps compiler settings and artifacts. Its sources map replaces each source body with a SHA-256 reference to the source-file entity; recompiled bytecode is referenced by hash.',
      sourcefile: 'The source text is stored once inside a JSON envelope. The compilation retains the file path and points to this entity’s hash attribute: SHA-256 of the exact UTF-8 source text.',
      code: 'Sourcify represents bytecode as hex; this entity stores the actual bytes (displayed as hex here). Its hash attribute is Keccak-256. Identical onchain and recompiled bytecode can share one entity.',
      blob: 'This entity holds one slice of a large payload. The parent’s spill reference identifies the complete body; part attributes determine the slice order.'
    };
    document.querySelector('#relationship').textContent = descriptions[k] || '';
    document.querySelector('#entity-kind').textContent = k;
    document.querySelector('#entity-key').textContent = `Key: ${entity.key}`;
    document.querySelector('#source-caption').textContent = showAll.checked ? 'Complete captured response. No fields hidden.' : 'Actual source-field excerpt for this entity. Use “Full response” for every field.';
    showJSON(document.querySelector('#source-json'), showAll.checked ? source : relevant(entity));
    showJSON(document.querySelector('#entity-json'), view === 'payload' ? entity.payload : view === 'attributes' ? entity.attributes : entity);
    for (const button of document.querySelectorAll('[data-view]')) button.setAttribute('aria-pressed', String(button.dataset.view === view));
  }
  select.addEventListener('change', render);
  showAll.addEventListener('change', render);
  for (const button of document.querySelectorAll('[data-view]')) button.addEventListener('click', () => {view = button.dataset.view; render();});
  render();
})();
