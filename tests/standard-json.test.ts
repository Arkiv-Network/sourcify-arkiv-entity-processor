import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { buildStandardJson } from '../source/standard-json';

const captured = JSON.parse(readFileSync(new URL('../docs/data/sourcify-response.json', import.meta.url), 'utf8'));
const capturedOutputContract = Object.values(Object.values(captured.stdJsonOutput.contracts)[0] as Record<string, any>)[0];
const input = () => ({
  compilation: captured.compilation,
  sources: captured.sources,
  metadata: capturedOutputContract.metadata,
  creationBytecode: captured.creationBytecode,
  runtimeBytecode: captured.runtimeBytecode,
  artifacts: {
    abi: captured.abi, userdoc: captured.userdoc, devdoc: captured.devdoc,
    storageLayout: captured.storageLayout, transientStorageLayout: captured.transientStorageLayout,
    sources: captured.sourceIds,
  },
  additionalInput: captured.additionalInput,
});

describe('Sourcify standard JSON selectors', () => {
  test('matches both standard JSON documents in the captured API response', () => {
    expect(buildStandardJson(input())).toEqual({ stdJsonInput: captured.stdJsonInput, stdJsonOutput: captured.stdJsonOutput });
  });

  test('preserves explicit SQL nulls, empty bytecode, and absent source rows', () => {
    const result = buildStandardJson({
      compilation: { language: 'Solidity', compilerSettings: null, fullyQualifiedName: 'Empty.sol:Empty' },
      sources: {}, metadata: undefined, artifacts: {}, additionalInput: null,
      creationBytecode: { recompiledBytecode: '0x' }, runtimeBytecode: { recompiledBytecode: null },
    });
    expect(result).toEqual({
      stdJsonInput: { language: 'Solidity', sources: null, settings: null },
      stdJsonOutput: { sources: null, contracts: { 'Empty.sol': { Empty: {
        abi: null, metadata: null, userdoc: null, devdoc: null, storageLayout: null, transientStorageLayout: null,
        evm: { bytecode: { object: null, sourceMap: null, linkReferences: null },
          deployedBytecode: { object: null, sourceMap: null, linkReferences: null, immutableReferences: null } },
      } } } },
    });
  });

  test('keeps Vyper additional input, source maps, layouts and raw metadata text', () => {
    const options = input();
    const metadata = '{ "language": "Vyper", "number": 1.00 }';
    const overrides = { 'Vault.vy': { supply: { slot: 0, type: 'uint256' } } };
    const sourceMap = { pc_pos_map: { '0': [1, 0, 1, 10] } };
    const result = buildStandardJson({ ...options,
      compilation: { language: 'Vyper', compilerSettings: { optimize: 'gas' }, fullyQualifiedName: 'Vault.vy:Vault' },
      metadata, additionalInput: { storage_layout_overrides: overrides },
      runtimeBytecode: { recompiledBytecode: '0xABCD', sourceMap, immutableReferences: {} },
      artifacts: { storageLayout: { supply: { slot: 0 } } },
    });
    expect(result.stdJsonInput).toEqual({ language: 'Vyper', sources: options.sources, settings: { optimize: 'gas' }, storage_layout_overrides: overrides });
    const contract = result.stdJsonOutput.contracts['Vault.vy'].Vault;
    expect(contract.metadata).toBe(metadata);
    expect(contract.storageLayout).toEqual({ supply: { slot: 0 } });
    expect(contract.evm.deployedBytecode).toEqual({ object: 'abcd', sourceMap, linkReferences: null, immutableReferences: {} });
    expect(contract).not.toHaveProperty('layout');
  });

  test('splits the target at the final colon and preserves special object keys', () => {
    const result = buildStandardJson({ ...input(), compilation: { language: 'Yul', compilerSettings: {}, fullyQualifiedName: 'https://example.org/a:yul:__proto__' } });
    expect(Object.keys(result.stdJsonOutput.contracts)).toEqual(['https://example.org/a:yul']);
    expect(Object.keys(result.stdJsonOutput.contracts['https://example.org/a:yul'])).toEqual(['__proto__']);
    expect(result.stdJsonInput.settings).toEqual({});
  });

  test('does not invent language-dependent output or restore outputSelection', () => {
    for (const language of ['Solidity', 'Vyper', 'Yul', 'Fe']) {
      const result = buildStandardJson({ ...input(), compilation: { language, compilerSettings: {}, fullyQualifiedName: 'source:Target' }, metadata: null });
      expect(result.stdJsonInput).toMatchObject({ language, settings: {} });
      expect(result.stdJsonInput.settings).not.toHaveProperty('outputSelection');
      expect(result.stdJsonOutput.contracts.source.Target.metadata).toBeNull();
      expect(result.stdJsonOutput.contracts.source.Target.evm.bytecode.object).toBe(captured.creationBytecode.recompiledBytecode.slice(2));
    }
  });
});
