# The signed roadmap snapshot

The hub can publish its public roadmap in a form anyone can check **without trusting the hub's servers**: a canonical JSON
document, signed as a Sui personal message, stored on Walrus. Design and custody: the operator's ADR-0006 §5 and ADR-0007
(`docs/architecture` in the deployment workspace). This page is the reference for the format and for running it.

## The document

```json
{
  "schema": "content-hub.roadmap-snapshot/1",
  "human": { "schema_version": 1, "updated_at": "…", "phases": [ … ] },
  "bot":   { "schema_version": 1, "updated_at": "…", "phases": [ … ] }
}
```

`human` and `bot` are the documents of `GET /public/v1/roadmap` and `GET /public/v1/roadmap-bot`, produced by the same
function, so a snapshot contains exactly what the public routes serve and nothing internal (tested: internal names,
descriptions, keys and visibility flags never appear).

**Canonical form:** UTF-8 JSON, keys sorted at every level, no whitespace, integers only (a non-integer number is refused,
so there is nothing to format differently), at most 1 MiB. Unchanged data gives identical bytes and an identical SHA-256, so
an unchanged roadmap is never stored twice.

## The signature

A Sui **personal message** signature by the snapshot signer (Ed25519). The signed digest is BLAKE2b-256 over the intent
`[3, 0, 0]`, the BCS length (ULEB128) of the document and the document bytes. The serialised signature is
`base64(0x00 ‖ signature(64) ‖ public key(32))`; the address is BLAKE2b-256(`0x00 ‖ public key`) as `0x` and 64 hex digits.
The committed fixture `tests/contract/snapshot-fixture.json` is signed by a **test** key and verified in CI with
`@mysten/sui`, so the Rust signer and the Sui SDK agree byte for byte.

## Verifying a snapshot

1. `GET /public/v1/snapshot` (or the roadmap site's "verifiable record") gives `sha256`, `signature`, `signer_address`,
   `blob_id` and, when the operator configures a public aggregator, `blob_url`.
2. Fetch the blob from any Walrus aggregator by `blob_id`.
3. Check that its SHA-256 equals `sha256`.
4. Verify the signature over the blob's bytes:

   ```ts
   import { verifyPersonalMessageSignature } from '@mysten/sui/verify'
   const publicKey = await verifyPersonalMessageSignature(blobBytes, signature)
   if (publicKey.toSuiAddress() !== signerAddress) throw new Error('not signed by the published signer')
   ```

5. Compare `signerAddress` with the address the operator has published (their site, `docs`, or the ADR changelog). The
   address is the identity; the hub's own answer for it proves nothing.

## Running it

| Subcommand | What it does |
| --- | --- |
| `content-hub snapshot-key --out PATH` | generates the signer's seed into a new 0600 file (never overwrites) and prints the address only |
| `content-hub snapshot-address` | prints the address of `SNAPSHOT_SIGNER_SEED` (never the seed) |
| `content-hub export-snapshot` | builds, signs, stores and records one snapshot, then exits (a CronJob) |

| Variable | Default | Meaning |
| --- | --- | --- |
| `SNAPSHOT_SIGNER_SEED` | — (required) | the base64 of the 32-byte seed. Holds no funds and owns nothing |
| `SNAPSHOT_NETWORK` | `testnet` | `localnet`, `testnet` or `mainnet`. **Mainnet is refused** unless `SNAPSHOT_ALLOW_MAINNET=1` |
| `SNAPSHOT_EPOCHS` | testnet 30, mainnet 2 | epochs to store for (1 to 53) |
| `SNAPSHOT_MIN_INTERVAL_HOURS` | `24` | at most one **new** snapshot per interval (1 to 720) |
| `SNAPSHOT_RENEW_MARGIN_DAYS` | `7` | an unchanged snapshot is stored again when its estimated storage end is within this long (1 to 30) |
| `WALRUS_PUBLISHER_URL` | unset | the internal Walrus publisher, `http(s)://host[:port]`. Unset: signed and recorded, **not stored** |
| `WALRUS_PUBLISHER_JWT_SECRET` | unset | the publisher's `--jwt-decode-secret` (`0x…` is hex). Unset: no token |
| `WALRUS_AGGREGATOR_URL` | unset | (`public`) a public aggregator, so the snapshot route can link the blob |

The export is safe to run at any time and from more than one place: a session lock makes a second run report `busy`; a run
that cannot store records nothing (the next run tries again); `unchanged` and `too_soon` are successful runs.

### The publisher

The hub stores a blob with `PUT {publisher}/v1/blobs?epochs=N&permanent=true` and a short-lived HS256 token whose claims
bind the **exact** `epochs` and `size` and carry a fresh `jti` (so a token cannot be replayed or reused for another
upload). The publisher must run with `--jwt-decode-secret` and, once confirmed on testnet, `--jwt-verify-upload`; it must be
reachable only from the hub (a NetworkPolicy), because it spends from its own wallet. The hub's client for it may call
**only** the configured host.

### Expiry

Walrus storage is bought in epochs (testnet one day, mainnet 14 days). The recorded `expires_at` is an **estimate**: now plus
`epochs − 1` whole epochs, deliberately early. The export stores an unchanged snapshot again before that estimate minus
the renew margin, so an error in the estimate is absorbed.

## Failure modes

| Failure | Result |
| --- | --- |
| Publisher unreachable or refuses | the run fails and records nothing; the next run tries again; `GET /public/v1/snapshot` keeps serving the last snapshot |
| The database is unreachable | the run fails; nothing is signed |
| The signer seed is wrong | the run refuses to start (a clear message, no seed printed) |
| The document would exceed 1 MiB | the run fails (`too large`) |
| Two runs at once | the second reports `busy` |
| The roadmap changed within 24 hours | `too_soon`, with the earliest time a new one may be made |
| Signer key suspected leaked | rotate (ADR-0007 §5): a new key, a published notice, snapshots between the suspected date and the rotation are treated as unverified |
