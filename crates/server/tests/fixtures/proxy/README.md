These offline reference vectors pin the proxy behavior used by Sourcify server
4.1.2 at commit `6048af7dfb36a3f5b7b2b30ede5daca3e701f7b0`.

`upstream-bytecodes.json` is copied unchanged from Sourcify's
`services/server/test/unit/utils/proxy-bytecodes.json`. It includes real creation
and runtime bytecode fixtures for clones, diamonds, EIP-1967, Matic, Livepeer,
non-proxies, and a factory. Its MIT license is included as `SOURCIFY-LICENSE`.

`whatsabi-0.29.0.json` contains independent outputs of the exact pinned
`@shazow/whatsabi` 0.29.0 JavaScript disassembler. Its npm distribution integrity is
`sha512-8v064w+p/xpX01aTEpoxHeC4jQvEyrAp96dwEreDsnphoiusm9g4IiJEydp/7VhULLxQ6k+T+Duy6sSIz9IxJg==`.
`generate.cjs` combines the upstream fixtures with crafted boundary, ordering,
metadata, factory, fixed-address, and selector cases. The Rust tests compare the
factory flag and the full ordered resolver list, including fixed addresses.
They do not calculate expected results using the Rust implementation.

To regenerate deliberately during a dependency review:

```sh
npm install --prefix /tmp/proxy-reference --ignore-scripts --no-audit --no-fund @shazow/whatsabi@0.29.0
node crates/server/tests/fixtures/proxy/generate.cjs /tmp/proxy-reference
cargo test -p arkiv-sourcify-server --lib proxy::tests
```

Normal tests and production serving require neither Node nor npm nor network
access to Sourcify. The Rust port retains the upstream MIT notice in
`crates/server/src/proxy-whatsabi-LICENSE.txt`.

The resolver tests additionally check scripted Ethereum requests, beacon
selector/revert fallback, Diamond call/decode failure handling, implementation
ordering and duplicates, zero addresses, creation-code prefix semantics, and
cooperative cancellation. Sourcify's cached dummy-provider Diamond facet scan
was checked against the pinned JavaScript: it yields an empty facet map and
makes no real Ethereum request, so the port omits that no-op phase.

`references.json` retains nine independent responses captured on 2026-10-13 from
`https://sourcify-mirror.arkiv-global.net`, a self-hosted official Sourcify service.
Each entry contains its name, path, status and body. The source capture archive is
`/home/ubuntu/arkiv-network/operations/proxy-resolution-validation/`. Cases cover
Safe, USDC, WETH, EIP-1967, Diamond, a factory, raw nested selectors, invalid nested
selectors, and Safe `fields=all`. Live state can change after capture. These
responses are independent test expectations and are never a runtime fallback.
