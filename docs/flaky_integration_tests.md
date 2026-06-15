# Flaky integration tests — `tests/world_two_clients.rs`

**Status: known tech debt, deferred (2026-06-15).** Not a gameplay bug; the
features under test work. This is test-harness fragility. Recorded so a failing
`world_two_clients` run isn't mistaken for a regression.

## Symptom
Running the full `world_two_clients` suite, a small, **varying** subset of the
pet/AOE tests fails each run:

| Run | Failures |
|---|---|
| clean HEAD | `pet_command_attack_locks_onto_target`, `pet_attacks_owners_target` |
| run A | `pet_pulls_aggro_via_threat_reaggro`, `aoe_spell_damages_nearby_enemies`, `pet_command…` |
| run B | `pet_pulls_aggro…`, `pet_command…`, `pet_attacks_owners_target` |

A *changing* failure set on identical code = non-determinism, not a logic bug.

## Verification (run each in isolation, own process)
- `pet_pulls_aggro_via_threat_reaggro` → **passes** solo
- `aoe_spell_damages_nearby_enemies` → **passes** solo
- `pet_attacks_owners_target` → **passes** solo
- `pet_command_attack_locks_onto_target` → **~1 in 5** solo (4 fail / 1 pass over 5 runs);
  the one pass completed the whole chain (summon → PetSpawn → ATTACK command → pet hits
  the enemy for 8), proving the **feature works**.

## Root causes (test-side, not gameplay)
1. **Runtime contention.** ~40 integration tests run concurrently on one tokio
   runtime. The harness polls with `wait_for(channel, timeout, pred)` — tick the
   client every 50 ms, drain messages, check predicate, give up after a fixed
   window (usually 3 s). Under CPU/scheduler saturation the server tick + UDP
   delivery drift past the deadline → `wait_for` returns `None` → `.expect()` panics.
   This explains the 3 tests that pass cleanly solo.
2. **`pet_command_attack_locks_onto_target` is extra-flaky for a real reason:** it
   waits for the enemy to *hit the player*, then casts the 3 s Summon Skeleton
   **while being attacked**. The incoming-damage cast-interrupt (`skills.rs` Track
   19A — 70 % interrupt at casting skill 0, and the fresh test Necromancer has ~0)
   cancels the summon most of the time → no `PetSpawn` → the `wait_for` at
   `world_two_clients.rs:1555` times out. The mechanic is *correct*; the test just
   doesn't control for it.

## Recommended fixes (when we return)
- Run this suite **serially** (`-- --test-threads=1`) — removes the contention that
  causes most of the flakes.
- Replace fixed timeouts with poll-until-with-backoff, or widen the windows.
- For `pet_command…`: remove combat pressure before summoning (move the player out of
  aggro range, or full-heal, or use a trained caster), or retry the summon on
  `CastFail`. This is the only one with a content-level cause.
- Longer term: drive a simulated clock instead of `tokio::sleep` wall-clock waits so
  timing is deterministic.

## Not in scope of
The 2026-06-15 loot-rights/coin work (`groups`/`loot`/`coin` paths) — those changes
don't touch aggro/pet/AOE code, and the deterministic lib tests (incl. the new
loot-auth + coin-roll tests) pass every run.
