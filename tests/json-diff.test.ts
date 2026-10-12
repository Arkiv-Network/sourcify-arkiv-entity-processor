import { describe, expect, test } from 'bun:test';
import '../docs/json-diff-model.js';

type DiffNode = {
  key: string; path: string; leftPresent: boolean; rightPresent: boolean;
  leftType: string; rightType: string; leftValue: unknown; rightValue: unknown;
  status: string; selfChanged: boolean; children: DiffNode[];
  counts: { added: number; removed: number; changed: number; unchanged: number };
};
const model = (globalThis as any).JsonDiffModel as {
  compare(left: unknown, right: unknown): DiffNode;
  filterTree(node: DiffNode, options?: { statuses?: string[]; query?: string }): DiffNode | null;
};
const paths = (node: DiffNode | null): string[] => node ? [node.path, ...node.children.flatMap(paths)] : [];
const child = (node: DiffNode, key: string) => node.children.find(item => item.key === key)!;

describe('structural JSON diff model', () => {
  test('distinguishes missing, null, scalar types and unchanged properties', () => {
    const diff = model.compare({ removed: null, number: 1, same: false }, { added: null, number: '1', same: false });
    expect(child(diff, 'removed')).toMatchObject({ status: 'removed', leftPresent: true, rightPresent: false, leftType: 'null', rightType: 'missing' });
    expect(child(diff, 'added')).toMatchObject({ status: 'added', leftPresent: false, rightPresent: true, rightType: 'null' });
    expect(child(diff, 'number')).toMatchObject({ status: 'changed', leftType: 'number', rightType: 'string' });
    expect(diff.counts).toEqual({ added: 1, removed: 1, changed: 1, unchanged: 1 });
    expect(diff.selfChanged).toBe(false);
  });

  test('object insertion order is irrelevant, and inputs remain untouched', () => {
    const left = Object.freeze({ z: Object.freeze({ b: 2, a: 1 }), a: Object.freeze([null, true]) });
    const right = { a: [null, true], z: { a: 1, b: 2 } };
    const before = JSON.stringify(right);
    const diff = model.compare(left, right);
    expect(diff.status).toBe('unchanged');
    expect(paths(diff)).toEqual(['', '/a', '/a/0', '/a/1', '/z', '/z/a', '/z/b']);
    expect(JSON.stringify(right)).toBe(before);
    expect(diff.counts).toEqual({ added: 0, removed: 0, changed: 0, unchanged: 4 });
  });

  test('added and removed containers include their properties without double counting', () => {
    const diff = model.compare({ old: { deep: [1, {}] } }, { fresh: { nested: true, empty: [] } });
    expect(paths(diff)).toEqual(['', '/fresh', '/fresh/empty', '/fresh/nested', '/old', '/old/deep', '/old/deep/0', '/old/deep/1']);
    expect(diff.counts).toEqual({ added: 2, removed: 2, changed: 0, unchanged: 0 });
    expect(child(diff, 'fresh').status).toBe('added');
    expect(child(diff, 'old').selfChanged).toBe(false);
    expect(child(child(diff, 'fresh'), 'empty').selfChanged).toBe(true);
  });

  test('empty containers and type replacements are explicit atomic values', () => {
    expect(model.compare({}, {}).counts.unchanged).toBe(1);
    expect(model.compare([], []).counts.unchanged).toBe(1);
    for (const [left, right] of [[{}, []], [{ nested: 1 }, null], [false, { nested: 1 }]]) {
      const diff = model.compare(left, right);
      expect(diff.status).toBe('changed');
      expect(diff.selfChanged).toBe(true);
      expect(diff.children).toEqual([]);
      expect(diff.counts).toEqual({ added: 0, removed: 0, changed: 1, unchanged: 0 });
    }
  });

  test('arrays compare positions deliberately rather than matching moved values', () => {
    const diff = model.compare(['a', 'b'], ['x', 'a', 'b']);
    expect(diff.children.map(node => [node.path, node.status])).toEqual([
      ['/0', 'changed'], ['/1', 'changed'], ['/2', 'added'],
    ]);
    expect(diff.counts).toEqual({ added: 1, removed: 0, changed: 2, unchanged: 0 });
    expect(model.compare([0, 1], [0]).children[1].status).toBe('removed');
  });

  test('escapes JSON Pointer tokens and treats prototype-looking keys as ordinary data', () => {
    const left = JSON.parse('{"__proto__":{"constructor":1},"a/b":{"~":null},"":false}');
    const right = JSON.parse('{"__proto__":{"constructor":2},"a/b":{"~":null},"":false}');
    const diff = model.compare(left, right);
    expect(paths(diff)).toEqual(['', '/', '/__proto__', '/__proto__/constructor', '/a~1b', '/a~1b/~0']);
    expect(child(child(diff, '__proto__'), 'constructor').status).toBe('changed');
    expect(Object.getPrototypeOf(left)).toBe(Object.prototype);
    expect(({} as any).constructor).toBe(Object);
  });

  test('status and path filters retain ancestors and never mutate comparison results', () => {
    const diff = model.compare({ branch: { added: 1, stable: true }, other: 1 }, { branch: { added: 1, stable: true, newKey: null }, other: 2 });
    const before = paths(diff);
    const filtered = model.filterTree(diff, { statuses: ['added'], query: 'NEWkey' });
    expect(paths(filtered)).toEqual(['', '/branch', '/branch/newKey']);
    expect(filtered!.counts).toEqual(diff.counts);
    expect(paths(diff)).toEqual(before);
    expect(model.filterTree(diff, { query: 'no such path' })).toBeNull();
    expect(model.filterTree(diff, { statuses: [] })).toBeNull();
    expect(paths(model.filterTree(diff))).toEqual(before);
  });

  test('path query also accepts an unescaped raw property key', () => {
    const diff = model.compare({}, { 'some/key': 1 });
    expect(paths(model.filterTree(diff, { query: 'some/key' }))).toEqual(['', '/some~1key']);
  });
});
