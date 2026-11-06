-- Invite codes (the public hosting plan, slice 1).
--
-- On a private tailnet, reaching the auth port was itself the invitation; on
-- a public port anyone can reach it, so registration needs a code the
-- operator minted. One code makes one account: `Register` burns the code
-- (sets `redeemed_at`) and inserts the account in ONE transaction, so two
-- registrations racing on the same code cannot both succeed, and a refused
-- registration (name taken) leaves the code unspent.
--
-- Codes are 128 random bits, stored as 32 lowercase hex characters, minted
-- by the `invite` bin and shown once. `note` is the operator's memo (who the
-- code is for). `redeemed_by` is a convenience for the listing; the burn
-- marker is `redeemed_at`, so a deleted account never frees its code.

CREATE TABLE IF NOT EXISTS invites (
    code         TEXT PRIMARY KEY,
    created_at   TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    note         TEXT,
    redeemed_by  INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
    redeemed_at  TIMESTAMP,
    revoked_at   TIMESTAMP
);
