import type { Row } from './database';

/** Reconstruct the API's database selectors, not the original compiler invocation/output.
 * https://github.com/argotorg/sourcify/blob/6048af7dfb36a3f5b7b2b30ede5daca3e701f7b0/services/server/src/server/services/utils/database-util.ts
 * (buildStdJsonInputSelector and STORED_PROPERTIES_TO_SELECTORS.std_json_output).
 * In particular outputSelection is not restored, and output contains just the verified target.
 */
export function buildStandardJson({ compilation, sources, metadata, creationBytecode, runtimeBytecode, artifacts, additionalInput }: {
  compilation: { language: string; compilerSettings: unknown; fullyQualifiedName: string };
  sources: Row | null;
  /** Raw exported JSON text: SQL casts it to text without reserializing it. */
  metadata: string | null | undefined;
  creationBytecode: Row;
  runtimeBytecode: Row;
  artifacts: Row;
  additionalInput: Row | null;
}) {
  const separator = compilation.fullyQualifiedName.lastIndexOf(':');
  if (separator < 0) throw new Error('Compilation fully qualified name must contain a colon');
  const path = compilation.fullyQualifiedName.slice(0, separator);
  const name = compilation.fullyQualifiedName.slice(separator + 1);
  const bytecodeObject = (bytecode: unknown) => {
    if (bytecode == null) return null;
    const hex = String(bytecode).replace(/^0x/i, '').toLowerCase();
    return hex || null;
  };
  const outputBytecode = (bytecode: Row) => ({
    object: bytecodeObject(bytecode.recompiledBytecode),
    sourceMap: bytecode.sourceMap ?? null,
    linkReferences: bytecode.linkReferences ?? null,
  });
  return {
    stdJsonInput: {
      language: compilation.language,
      // PostgreSQL json_object_agg returns SQL NULL if there are no source rows.
      sources: sources && Object.keys(sources).length ? sources : null,
      settings: compilation.compilerSettings ?? null,
      ...additionalInput,
    },
    stdJsonOutput: {
      sources: artifacts.sources ?? null,
      contracts: {
        [path]: {
          [name]: {
            abi: artifacts.abi ?? null,
            metadata: metadata ?? null,
            userdoc: artifacts.userdoc ?? null,
            devdoc: artifacts.devdoc ?? null,
            storageLayout: artifacts.storageLayout ?? null,
            transientStorageLayout: artifacts.transientStorageLayout ?? null,
            evm: {
              bytecode: outputBytecode(creationBytecode),
              deployedBytecode: { ...outputBytecode(runtimeBytecode), immutableReferences: runtimeBytecode.immutableReferences ?? null },
            },
          },
        },
      },
    },
  };
}
