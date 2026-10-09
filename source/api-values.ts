import { keccak_256 } from '@noble/hashes/sha3';
import { bytesToHex, utf8ToBytes } from '@noble/hashes/utils';

// Sourcify server 6048af7dfb36a3f5b7b2b30ede5daca3e701f7b0 applies EIP-55
// to the request address and deployment.deployer; database exports contain raw bytes.
export function checksumAddress(value: string | null): string | null {
  if (value === null) return null;
  if (!/^0x[0-9a-fA-F]{40}$/.test(value)) throw new Error(`Invalid Ethereum address: ${value}`);
  const lower = value.slice(2).toLowerCase();
  const hash = bytesToHex(keccak_256(utf8ToBytes(lower)));
  return '0x' + [...lower].map((character, index) => parseInt(hash[index], 16) >= 8 ? character.toUpperCase() : character).join('');
}
