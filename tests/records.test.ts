import { expect, test } from 'bun:test';
import { assembleContract } from '../source/records';

test('adapter follows API null fields and excludes export bookkeeping', () => {
  const record = assembleContract({
    match: { id: '1', creation_match: 'perfect', runtime_match: null, created_at: '2026-10-09 12:34:56.123456' },
    verification: {}, deployment: { address: '0x52908400098527886e0f7030069857d2e4169ee7', transaction_hex: '0x', deployer_hex: '0x' },
    compilation: { language: 'solidity', compiler: 'solc', fully_qualified_name: 'a:b.sol:C', name: 'C', version: '0.8.30', creation_code_artifacts: '{"extra":"excluded"}' },
    deployed: {}, metadataRows: [], sources: null, code: () => '0x', signatures: { function: [], event: [], error: [] },
  });
  expect(record.address).toBe('0x52908400098527886E0F7030069857D2E4169EE7');
  expect(record.verifiedAt).toBe('2026-10-09T12:34:56Z');
  expect(record.sources).toBeNull();
  expect(record.metadata).toBeNull();
  expect(record.abi).toBeNull();
  expect(record.deployment).toEqual({ transactionHash: null, blockNumber: null, transactionIndex: null, deployer: null });
  expect(record.creationBytecode).toEqual({ onchainBytecode: null, recompiledBytecode: null, sourceMap: null, linkReferences: null, cborAuxdata: null, transformations: null, transformationValues: null });
  expect(record.runtimeBytecode).toEqual({ ...record.creationBytecode, immutableReferences: null });
  expect(record).not.toHaveProperty('sourcifyIds');
  expect(record.stdJsonInput).toEqual({ language: 'Solidity', sources: null, settings: null });
  expect(record.stdJsonOutput.contracts['a:b.sol'].C.evm.bytecode.object).toBeNull();
});
