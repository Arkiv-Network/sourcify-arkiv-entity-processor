These five metadata-string pairs were extracted without modifying their content
from independent HTTP captures on 2026-10-13. The reference is the official
Sourcify server deployed at `https://sourcify-mirror.arkiv-global.net`; the stored
side is the Arkiv-backed server before this Unicode formatting fix, at
`https://sourcify-prefill.arkiv-global.net`.

`captured.json` records the contract address, match ID, compiler output path,
original response SHA-256 hashes, and exact string values. Four pairs came from
randomized preflight cases 6, 13, 28 and 29; the fifth came from the initial
randomized runner's `full` case for match ID 37547516 (gzip response bodies).
The complete retained captures live in the workspace's
`operations/randomized-preflight` and `operations/randomized-live-initial` runs.

Every pair has semantically equal inner JSON but a different metadata **string**:
the exported text uses `\uXXXX` for Unicode, while the reference uses UTF-8.
The test asserts exact output string equality, keeping this distinction visible.
These fixtures exercise formatting of stored compiler metadata and do not provide
runtime response data or alter the imported Arkiv records.
