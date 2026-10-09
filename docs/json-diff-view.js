/* Property-level DOM renderer; values always enter the document through textContent. */
(() => {
  const element = (tag, className, text) => {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  };
  const title = status => status[0].toUpperCase() + status.slice(1);
  function mount(container) {
    const controls = element('div', 'diff-controls');
    const searchLabel = element('label', 'diff-search-label', 'Find property or path');
    const search = element('input', 'diff-search');
    search.type = 'search'; search.placeholder = 'e.g. sources, /deployment/blockNumber';
    searchLabel.append(search);
    const onlyLabel = element('label', 'diff-only-label');
    const only = element('input'); only.type = 'checkbox';
    onlyLabel.append(only, document.createTextNode(' Differences only'));
    const expand = element('button', '', 'Expand all'); expand.type = 'button';
    const collapse = element('button', '', 'Collapse all'); collapse.type = 'button';
    controls.append(searchLabel, onlyLabel, expand, collapse);
    const counts = element('div', 'diff-counts'); counts.setAttribute('aria-live', 'polite');
    const note = element('p', 'diff-note');
    const headings = element('div', 'diff-headings');
    const properties = element('div', '', 'Property / JSON Pointer');
    const leftHeading = element('div'); const rightHeading = element('div');
    headings.append(properties, leftHeading, rightHeading);
    const tree = element('div', 'diff-tree'); tree.setAttribute('aria-label', 'Property comparison');
    container.append(controls, counts, note, headings, tree);
    let options, root, opened = new Set();
    const referenceAt = node => options.references?.find(ref => node.path === ref.path || node.path.startsWith(ref.path + '/'));
    function isReferenced(node) { return node.status !== 'unchanged' && Boolean(referenceAt(node)); }
    function summary(value, type) {
      if (type === 'array') return `[${value.length} ${value.length === 1 ? 'item' : 'items'}]`;
      if (type === 'object') return `{${Object.keys(value).length} ${Object.keys(value).length === 1 ? 'property' : 'properties'}}`;
      return JSON.stringify(value);
    }
    function valueCell(node, side) {
      const cell = element('div', `diff-value diff-value-${side}`);
      const present = node[side + 'Present'];
      const value = node[side + 'Value']; const type = node[side + 'Type'];
      const ref = side === 'right' && isReferenced(node) ? referenceAt(node) : undefined;
      if (ref) {
        cell.append(element('span', 'diff-reference-note', 'Stored by reference'));
        const follow = element('button', 'diff-reference-target', `${ref.kind}${ref.pointer ? ' ' + ref.pointer : ''}`);
        follow.type = 'button'; follow.title = ref.hash || 'Within this compilation';
        follow.addEventListener('click', () => options.onFollowReference?.(ref));
        cell.append(follow);
        if (present && value === null) cell.append(element('small', '', 'null placeholder in stored array'));
        return cell;
      }
      if (!present) { cell.append(element('span', 'diff-missing', 'Missing')); return cell; }
      cell.append(element('span', 'diff-value-type', type));
      const display = (type === 'object' || type === 'array') && !node.children?.length ? JSON.stringify(value, null, 2) : summary(value, type);
      const text = element('div', 'diff-value-text');
      if (typeof display === 'string' && display.length > 200) {
        let full = false;
        const toggle = element('button', 'diff-value-toggle', `Show full value (${display.length.toLocaleString()} characters)`);
        toggle.type = 'button'; toggle.setAttribute('aria-expanded', 'false');
        text.textContent = display.slice(0, 180) + '…';
        toggle.addEventListener('click', () => {
          full = !full; text.textContent = full ? display : display.slice(0, 180) + '…';
          text.classList.toggle('diff-value-full', full);
          toggle.textContent = full ? 'Collapse value' : `Show full value (${display.length.toLocaleString()} characters)`;
          toggle.setAttribute('aria-expanded', String(full));
        });
        cell.append(text, toggle);
      } else { text.textContent = display; cell.append(text); }
      return cell;
    }
    function visit(node, parent, depth) {
      const group = element('div', 'diff-property');
      group.dataset.path = node.path; group.dataset.status = node.status;
      const referenced = isReferenced(node);
      if (referenced) group.dataset.referenced = 'true';
      const row = element('div', `diff-row diff-${referenced ? 'referenced' : node.status}`);
      const label = element('div', 'diff-property-label');
      label.style.setProperty('--depth', Math.min(depth, 6));
      const children = node.children || [];
      const open = opened.has(node.path);
      if (children.length) {
        const toggle = element('button', 'diff-expand', open ? '−' : '+');
        toggle.type = 'button'; toggle.setAttribute('aria-expanded', String(open));
        toggle.setAttribute('aria-label', `${open ? 'Collapse' : 'Expand'} ${node.path || 'root'}`);
        toggle.addEventListener('click', () => { open ? opened.delete(node.path) : opened.add(node.path); render(); });
        label.append(toggle);
      } else label.append(element('span', 'diff-leaf-marker', '·'));
      const names = element('div', 'diff-property-names');
      names.append(element('strong', '', node.path ? String(node.key) : '(root)'), element('code', 'diff-path', node.path || '"" (root)'));
      names.append(element('span', `diff-badge diff-badge-${referenced ? 'referenced' : node.status}`, referenced ? 'Referenced' : title(node.status)));
      label.append(names);
      row.append(label, valueCell(node, 'left'), valueCell(node, 'right'));
      group.append(row); parent.append(group);
      if (open && children.length) {
        const nested = element('div', 'diff-children'); group.append(nested);
        for (const child of children) visit(child, nested, depth + 1);
      }
    }
    function render() {
      if (!root) return;
      const filtered = globalThis.JsonDiffModel.filterTree(root, {
        statuses: only.checked ? ['added', 'removed', 'changed'] : ['added', 'removed', 'changed', 'unchanged'],
        query: search.value.trim()
      });
      tree.replaceChildren();
      if (!filtered) { tree.append(element('p', 'diff-empty', only.checked && root.status === 'unchanged' ? 'No differences. Every JSON property matches.' : 'No properties match these filters.')); return; }
      if (filtered.children?.length && root.leftType === root.rightType) {
        for (const child of filtered.children) visit(child, tree, 0);
      } else visit(filtered, tree, 0);
    }
    function walk(node, callback, depth = 0) { callback(node, depth); for (const child of node.children || []) walk(child, callback, depth + 1); }
    search.addEventListener('input', () => {
      if (search.value.trim()) walk(root, node => opened.add(node.path));
      render();
    });
    only.addEventListener('change', render);
    expand.addEventListener('click', () => { walk(root, node => opened.add(node.path)); render(); });
    collapse.addEventListener('click', () => { opened.clear(); render(); });
    return {
      update(next) {
        options = next; root = globalThis.JsonDiffModel.compare(next.left, next.right);
        opened = new Set(); walk(root, (node, depth) => { if (depth <= 1) opened.add(node.path); });
        leftHeading.textContent = next.leftLabel; rightHeading.textContent = next.rightLabel;
        note.textContent = next.description + ' Arrays are compared by index; a missing property is different from null. Counts describe leaf values and empty containers; parent badges summarize their children.';
        const totals = {added:0, removed:0, changed:0, unchanged:0, referenced:0};
        walk(root, node => { if (!node.children?.length) totals[isReferenced(node) ? 'referenced' : node.status]++; });
        counts.replaceChildren();
        for (const [status, count] of Object.entries(totals)) {
          const badge = element('span', `diff-count diff-badge-${status}`, `${count.toLocaleString()} ${status}`);
          badge.dataset.count = status; counts.append(badge);
        }
        render();
      }
    };
  }
  globalThis.JsonDiffView = { mount };
})();
