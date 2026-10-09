/* Structural JSON comparison. Array positions are compared by index, not edit distance. */
(function (global) {
  'use strict';

  const statuses = ['added', 'removed', 'changed', 'unchanged'];
  const own = (object, key) => Object.prototype.hasOwnProperty.call(object, key);
  const typeOf = value => value === null ? 'null' : Array.isArray(value) ? 'array' : typeof value;
  const isContainer = type => type === 'object' || type === 'array';
  const pointerToken = key => String(key).replace(/~/g, '~0').replace(/\//g, '~1');
  const emptyCounts = () => ({ added: 0, removed: 0, changed: 0, unchanged: 0 });

  function build(leftValue, rightValue, leftPresent, rightPresent, key, path) {
    const leftType = leftPresent ? typeOf(leftValue) : 'missing';
    const rightType = rightPresent ? typeOf(rightValue) : 'missing';
    const children = [];
    // Type changes are atomic: a scalar/object or object/array replacement is
    // one changed value. Added/removed containers still expose every property.
    const comparableContainer = (leftType === rightType && isContainer(leftType)) ||
      (!leftPresent && isContainer(rightType)) || (!rightPresent && isContainer(leftType));
    if (comparableContainer) {
      const containerType = leftPresent ? leftType : rightType;
      let keys;
      if (containerType === 'array') {
        const length = Math.max(leftPresent ? leftValue.length : 0, rightPresent ? rightValue.length : 0);
        keys = Array.from({ length }, (_, index) => String(index));
      } else {
        keys = [...new Set([
          ...(leftPresent ? Object.keys(leftValue) : []),
          ...(rightPresent ? Object.keys(rightValue) : []),
        ])].sort();
      }
      for (const childKey of keys) {
        const hasLeft = leftPresent && own(leftValue, childKey);
        const hasRight = rightPresent && own(rightValue, childKey);
        children.push(build(
          hasLeft ? leftValue[childKey] : undefined,
          hasRight ? rightValue[childKey] : undefined,
          hasLeft, hasRight, childKey, `${path}/${pointerToken(childKey)}`,
        ));
      }
    }
    let status;
    if (!leftPresent) status = 'added';
    else if (!rightPresent) status = 'removed';
    else if (leftType !== rightType) status = 'changed';
    else if (comparableContainer) status = children.some(child => child.status !== 'unchanged') ? 'changed' : 'unchanged';
    else status = leftValue === rightValue ? 'unchanged' : 'changed';

    const counts = emptyCounts();
    if (children.length) {
      for (const child of children) for (const name of statuses) counts[name] += child.counts[name];
    } else {
      // Empty containers and type replacements count once. Container summaries
      // never inflate the number of changed values reported by descendants.
      counts[status] = 1;
    }
    return {
      key, path, leftPresent, rightPresent, leftType, rightType,
      leftValue, rightValue, status, children, counts,
      selfChanged: children.length === 0 && status !== 'unchanged',
    };
  }

  function compare(left, right) {
    return build(left, right, true, true, '', '');
  }

  /**
   * Return a pruned tree, preserving ancestors of matching descendants.
   * statuses selects node statuses; query is a case-insensitive literal search
   * over the JSON Pointer and raw key. Omitted statuses includes every status;
   * an empty status list matches nothing. Original subtree counts are retained
   * for stable full-comparison summaries, including while filters are active.
   */
  function filterTree(root, options = {}) {
    const selected = new Set(options.statuses === undefined ? statuses : options.statuses);
    const query = String(options.query ?? '').toLowerCase();
    function visit(node) {
      const children = node.children.map(visit).filter(Boolean);
      const matches = selected.has(node.status) &&
        (!query || node.path.toLowerCase().includes(query) || String(node.key).toLowerCase().includes(query));
      return matches || children.length ? { ...node, children } : null;
    }
    return visit(root);
  }

  global.JsonDiffModel = Object.freeze({ compare, buildDiff: compare, filterTree });
})(globalThis);
