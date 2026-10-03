# yaya-wire

What every participant of the yaya.tech agent network agrees on. Pure functions over their inputs — no sockets, no database, no ambient clock — so the phone core, the gateway and any third-party verifier share one implementation and one test suite.

| module | what it settles |
|---|---|
| `id` | `agent:<ed25519 hex>` ids, `did:key` form, base58 |
| `keypair` | the signing half: envelopes and request signatures |
| `envelope` | `{payload, agent, sig}` signed JSON; `verify` returns the signer |
| `reqsig` | proof of possession for HTTP — the `X-Agent-Auth` header |
| `secret` | constant-time compare; refusing placeholder secrets |
| `net` | client IP behind a trusted reverse proxy |
| `attest` | Android Key Attestation verification with a policy |

## Request signatures (`reqsig` v1)

A bearer that is a *public* key proves nothing. Every authenticated request therefore carries a signature over what it is doing:

```
Authorization: Bearer agent:<hex>
X-Agent-Auth:  v1.<unix ts>.<nonce hex>.<sig hex>

sig = ed25519( "yaya-reqsig-v1\n" METHOD "\n" PATH?QUERY "\n" ts "\n" nonce "\n" BODY_HASH )
```

`BODY_HASH` is hex SHA-256 of the body, or `-` for a streamed body the client cannot hash up front. The server accepts a timestamp within ±300 s and each `(agent, nonce)` once, so a captured header cannot be replayed.

## Attestation policy

`attest::verify` checks, in order: every link signs the previous one **and is a CA** (an app's own attested key must never issue a certificate), every certificate is within validity, the last is a pinned Google root, the challenge equals `sha256("agente-attest:v1:" + agent id)`, the revocation list says OK for every non-root serial, and the leaf's application id matches the policy — package name **and** signing-certificate digest. The forged-leaf case is a test.
