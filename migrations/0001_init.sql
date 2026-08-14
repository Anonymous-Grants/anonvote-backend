-- AnonVote backend schema.
--
-- This database stores round/proposal *metadata* and public, already-on-chain
-- facts only. It intentionally has no table, column, or foreign key anywhere
-- that links a voter to a ballot: `voter_registrations` (who registered) and
-- `ballots` (which nullifier voted for which choice) are populated from two
-- separate on-chain event streams and share no join key. That separation is
-- the whole privacy property this service provides; see the README before
-- adding any column here that would reconnect them.

CREATE TABLE rounds (
    id                  BIGINT PRIMARY KEY,               -- matches the on-chain round_id (u64) from create_round's return value
    contract_id         TEXT NOT NULL,                     -- Soroban contract address (C...) this round lives on
    admin               TEXT NOT NULL,                     -- Stellar address (G...) authorized on-chain as round.admin; this backend's operator identity for rounds it created
    title               TEXT NOT NULL,
    num_choices         INTEGER NOT NULL CHECK (num_choices > 0),
    phase               TEXT NOT NULL DEFAULT 'registration'
                            CHECK (phase IN ('registration', 'voting', 'finalized')),
    payout_pool_stroops BIGINT NOT NULL DEFAULT 0 CHECK (payout_pool_stroops >= 0),
    create_round_tx_hash TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    voting_opened_at    TIMESTAMPTZ,
    finalized_at        TIMESTAMPTZ
);

CREATE TABLE proposals (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    round_id        BIGINT NOT NULL REFERENCES rounds(id) ON DELETE CASCADE,
    choice_index    INTEGER NOT NULL CHECK (choice_index >= 0), -- matches the on-chain `choice` index cast_vote/tally use
    title           TEXT NOT NULL,
    description     TEXT,
    payout_address  TEXT NOT NULL,                              -- Stellar address (G... or C...) to receive this proposal's payout share
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (round_id, choice_index)
);

-- Mirrors the on-chain `Registered` event: address + leaf_index + commitment.
-- This is deliberately as public as the chain itself is (register() is not
-- anonymous — see README) and exists so the API can answer "did this address
-- register" and "what's the current leaf list" without re-querying RPC/events
-- on every request. It has no column referencing `ballots` or `nullifiers`.
CREATE TABLE voter_registrations (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    round_id          BIGINT NOT NULL REFERENCES rounds(id) ON DELETE CASCADE,
    voter_address     TEXT NOT NULL,
    leaf_index        INTEGER NOT NULL CHECK (leaf_index >= 0),
    commitment_hex    TEXT NOT NULL,
    register_tx_hash  TEXT NOT NULL,
    registered_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (round_id, voter_address),
    UNIQUE (round_id, leaf_index)
);

-- Mirrors the on-chain public ballot log exactly: nullifier + choice, no
-- voter identity, no foreign key to voter_registrations or any other table
-- that could re-derive one. This is what GET /rounds/{id}/tally aggregates.
CREATE TABLE ballots (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    round_id           BIGINT NOT NULL REFERENCES rounds(id) ON DELETE CASCADE,
    nullifier_hex      TEXT NOT NULL,
    choice_index       INTEGER NOT NULL CHECK (choice_index >= 0),
    cast_vote_tx_hash  TEXT NOT NULL,
    cast_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Mirrors the contract's own spent-nullifier check; belt-and-suspenders
    -- bookkeeping only; the contract is the actual source of truth for
    -- double-vote rejection.
    UNIQUE (round_id, nullifier_hex)
);

CREATE TABLE payouts (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    round_id         BIGINT NOT NULL REFERENCES rounds(id) ON DELETE CASCADE,
    proposal_id      UUID NOT NULL REFERENCES proposals(id) ON DELETE CASCADE,
    amount_stroops   BIGINT NOT NULL CHECK (amount_stroops >= 0),
    vote_share_bps   INTEGER NOT NULL CHECK (vote_share_bps >= 0 AND vote_share_bps <= 10000), -- basis points of the final tally this payout represents
    status           TEXT NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'submitted', 'confirmed', 'failed')),
    payout_tx_hash   TEXT,
    error            TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    executed_at      TIMESTAMPTZ,
    UNIQUE (round_id, proposal_id)
);

CREATE INDEX idx_proposals_round ON proposals(round_id);
CREATE INDEX idx_voter_registrations_round ON voter_registrations(round_id);
CREATE INDEX idx_ballots_round ON ballots(round_id);
CREATE INDEX idx_payouts_round ON payouts(round_id);
