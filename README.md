# anonvote-backend

Orchestration backend for [AnonVote](../anonvote-contracts): manages round and
proposal metadata, relays voter registrations and votes to the
[AnonVote Soroban contract](../anonvote-contracts), serves a live public
tally, and executes proportional payouts once a round closes.

This service holds no voter keys and never links a voter to a ballot in its
own database — see ["What this backend does and doesn't
know"](#what-this-backend-does-and-doesnt-know) below. The privacy property
comes entirely from the contract and the Noir circuit in
`anonvote-contracts`; this backend's job is orchestration, not anonymity.

## Architecture

```
src/
  main.rs            wiring: config, DB pool + migrations, router, graceful shutdown
  config.rs           env-var configuration
  state.rs             shared AppState (db pool, soroban client, proof service, config)
  error.rs              AppError -> HTTP status/JSON mapping
  models.rs              DB row types + request/response DTOs
  soroban_client.rs        drives the `stellar` CLI to talk to the contract + network
  proof_service.rs          Noir witness generation + (optional) Groth16 proving
  routes/
    rounds.rs                POST /rounds, GET /rounds/{id}, .../eligibility, .../open, .../finalize, .../tally
    voters.rs                  POST /voters/register
    votes.rs                    POST /votes
    payouts.rs                   POST /payouts/execute, GET /rounds/{id}/payouts
```

**Why the `stellar` CLI instead of hand-rolled transaction XDR?** Soroban
transactions need correct sequence-number/fee/resource-footprint bookkeeping
and `SorobanAuthorizationEntry` handling — the official CLI already
implements all of that correctly. `soroban_client.rs` drives it as a
subprocess and parses its output: successful calls print the decoded return
value as JSON on stdout (`stellar contract invoke` generates an
"implicit CLI" per contract function from its embedded spec — struct
arguments are JSON with hex-encoded `Bytes`/`BytesN` fields, e.g.
`--proof '{"a":"...","b":"...","c":"..."}'`); rejected calls exit non-zero
with an `Error(Contract, #N)` line on stderr, which is regex-parsed back into
a named error. This contract was confirmed directly against a live testnet
deployment of the AnonVote contract while building this module.

## The round lifecycle, end to end

1. **`initialize`** (once, out of band). The AnonVote contract needs a
   Groth16 verifying key from the Noir circuit's trusted setup before this
   backend can do anything — this backend doesn't run a trusted setup or
   call `initialize`/`set_verifying_key` itself. Deploy and initialize the
   contract per `anonvote-contracts`' README, then set `ANONVOTE_CONTRACT_ID`
   here.

2. **`POST /rounds`** — round organizer creates a round with its proposals:

   ```json
   {
     "title": "Q3 RetroPGF Round",
     "payout_pool_stroops": 500000000000,
     "proposals": [
       { "title": "Proposal A", "payout_address": "GABC...", "description": "..." },
       { "title": "Proposal B", "payout_address": "GDEF..." }
     ]
   }
   ```

   This backend calls the contract's `create_round` as its own operator
   identity (which becomes `round.admin` on-chain) and records the round +
   proposals in Postgres in one transaction. `proposals[i]`'s array position
   *is* its on-chain `choice` index — `cast_vote`/`tally` are index-based, so
   this ordering is load-bearing.

3. **`POST /rounds/{id}/eligibility`** — the round admin marks which Stellar
   addresses may register (`{"voters": ["G...", "G..."], "eligible": true}`).
   This is the contract's Sybil-resistance hook (see the
   anonvote-contracts README): plug in whatever eligibility list the round
   already uses upstream of this call — a badgeholder registry, a hackathon
   judge roster, a token-gated allowlist. This backend doesn't prescribe how
   that list is produced.

4. **`POST /voters/register`** — an eligible voter registers
   `commitment = Poseidon2(secret)` for a `secret` only they know. **This
   backend relays a transaction the voter's own wallet already built and
   signed; it never builds or signs it.** `register` needs
   `voter.require_auth()` — a signature this backend must never be able to
   produce on a voter's behalf, so it isn't in a position to hold that key
   at all:

   ```json
   { "round_id": 1, "voter": "GABC...", "commitment_hex": "...", "signed_xdr": "AAAA..." }
   ```

   The voter's wallet/dApp should use a standard Stellar SDK's
   prepare-then-sign-then-submit flow (simulate to record the required
   authorization entry, *then* sign, *then* submit) — signing an unsimulated
   invocation directly will fail `require_auth` when it lands on-chain. This
   step *does* reveal that `voter` registered (it's a real signed
   transaction from their address) — but not which ballot they later cast.

5. **`POST /rounds/{id}/open`** — the round admin closes registration and
   opens voting (`open_voting`). The round's Merkle root (the registered
   anonymity set) is frozen at this point; every vote for the rest of the
   round proves membership against this exact snapshot.

6. **`POST /votes`** — casts an anonymous ballot:

   ```json
   { "round_id": 1, "choice": 0, "nullifier_hex": "...", "proof": { "a": "...", "b": "...", "c": "..." } }
   ```

   `cast_vote` needs **no signature at all** — that's the design (see
   anonvote-contracts). This backend submits it as its own operator
   identity purely as a fee-paying relayer; nothing about the HTTP caller is
   recorded anywhere, only the nullifier and choice (already public
   on-chain). Instead of `proof`, you can submit `witness` (the circuit's
   private inputs: `secret`, `merkle_path`, `path_indices`) and have this
   backend generate the proof server-side via `proof_service` — see
   [Proof generation](#proof-generation) below for what that requires.

7. **`GET /rounds/{id}/tally`** — the live public tally, read directly from
   the chain on every call (`get_round` + `tally`, both simulated only — no
   fee, nothing submitted) rather than served from a cache, so it's never
   stale.

8. **`POST /rounds/{id}/finalize`** — the round admin closes voting
   (`finalize_round`) and gets the final tally back.

9. **`POST /payouts/execute`** — see [Payouts](#payouts-a-one-shot-drips-style-split)
   below.

## What this backend does and doesn't know

`voter_registrations` (who registered, their leaf index and commitment) and
`ballots` (which nullifier voted for which choice) are two separate tables
populated from two separate on-chain event streams, and **no query, join,
or foreign key anywhere in this codebase connects them** — see the
comment at the top of `migrations/0001_init.sql`. `register` is not
anonymous (a real Stellar address signs it); `cast_vote` reveals nothing
about which registrant cast it. That gap between the two is the entire
privacy property, and it's enforced by the ZK proof and the contract,
not by anything this backend chooses to log or not log — this backend
simply never has the information needed to bridge it.

## Proof generation

`proof_service.rs` has two independent pieces:

- **Poseidon2 hashing** (`compute_leaf`, `compute_nullifier`): runs
  `soroban-sdk`'s host implementation natively (`Env::default()` — no WASM,
  no network) via the `soroban-poseidon` crate, the *exact* same
  construction the deployed contract and the Noir circuit both use. `POST
  /votes`'s `witness` path uses this to check the submitted `nullifier_hex`
  actually equals `Poseidon2(secret, round_id)` before spending a proving
  attempt on an inconsistent request.
- **Witness generation + proving** (`prove`): shells out to `nargo execute`
  (verified against the real circuit in `anonvote-contracts/circuits/anonvote`
  while building this module) to solve the circuit, then to a configurable
  Barretenberg-compatible `bb` binary to turn the witness into a Groth16
  proof. **The `bb` step needs `BB_PATH` configured and is otherwise
  disabled** (`POST /votes` returns 503 for a `witness` body, but still
  works normally for an already-built `proof`) — `bb`'s exact CLI flags and
  Groth16 output layout vary by version and weren't validated end-to-end
  while building this (no Barretenberg install was available); see
  `BB_PROVE_ARGS` in `.env.example` to adapt the invocation, and
  `read_groth16_proof` in `proof_service.rs` if your version's output layout
  differs. Submitting a client-generated `proof` directly is the
  fully-tested path and doesn't need any of this.

## Payouts: a one-shot, Drips-style split

[Drips](https://drips.network) popularized continuous, proportional funding
splits. `POST /payouts/execute` borrows the "split by share" idea for a
single settlement rather than an on-chain streaming split: once a round is
`finalized`, it divides `payout_pool_stroops` across proposals in exact
proportion to their share of the final tally (largest-remainder rounding,
so the amounts always sum to exactly the pool — see the unit tests in
`routes/payouts.rs`) and submits one classic Stellar payment per proposal
with a nonzero share, from `STELLAR_TREASURY_IDENTITY`. It's idempotent per
round — a second call returns 409 rather than paying out twice — and
`GET /rounds/{id}/payouts` returns the history.

## Running locally

Requires Postgres, the [`stellar` CLI](https://developers.stellar.org/docs/tools/cli/install-cli),
and (for server-side proving only) [`nargo`](https://noir-lang.org).

```bash
createdb anonvote
cp .env.example .env   # fill in ANONVOTE_CONTRACT_ID, STELLAR_OPERATOR_IDENTITY, ...

stellar keys generate operator --network testnet --fund   # if you don't have an identity yet

cargo run   # runs migrations automatically, then listens on BIND_ADDR (default 0.0.0.0:8080)
```

`cargo test` runs the payout-allocation unit tests (no DB or network
needed). The route handlers themselves were exercised against a live
testnet deployment of the AnonVote contract while building this backend,
rather than mocked — see the module docs in `soroban_client.rs` for the
exact CLI I/O contract that was confirmed.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
