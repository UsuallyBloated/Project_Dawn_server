# Flaky integration tests — `tests/world_two_clients.rs`

**Status: largely fixed 2026-08-11 (commit `6a1a92e`); a small genuinely-flaky
residue remains.** Read the update below before the original 2026-06-15 notes,
which are kept for history but no longer describe the main problem.

---

## Update 2026-08-11 — most of this was NOT flakiness

Between roughly 2026-07-20 and 2026-08-11 this suite sat at a **stable**
`29 passed; 13 failed`. The *same* 13 failed in parallel, with `--test-threads=1`,
and on a stashed tree — which is the opposite of the varying set described below.
That stability was the tell: it was **stale test fixtures**, not timing.

The file was last edited 2026-07-19 and three server changes landed after it:

| Cause | Tests | What the test assumed |
|---|---|---|
| CastSpell class/level gate (`a96826d`) | 11 | that a **level 1** character could cast Healing Wave (min 4), Summon Skeleton (6), Inferno (12), Charm (20). Three also cast a **Shaman-only** spell as a Cleric. |
| Melee swing-rate limit (`335b5b1`) | 1 | that 10 Attacks in one frame all land. The limiter correctly drops 9 as forgery. |
| `meditate` casting skill (`d97031a`) | 1 | that there are 6 casting keys. There are 7. |

**The server was correct in every case.** The tests were asserting pre-gate
behavior. Fixed by teaching the fixtures about the gates: a `set_char_level`
helper (which also tops up mana, since the loader recomputes `max_mp` from level
but carries current `mp` over), Shaman casters for the Shaman-only spell, paced
swings, and an updated key count.

Two timing weaknesses surfaced while fixing those and were also corrected: a test
that started a 3 s summon **while being hit** (Track 19A's on-hit interrupt made
that unwinnable — the chance never drops below 10% even at max channeling), and a
2 s approach walk that only clipped camp 0's aggro radius.

### What "green" looks like now
39 to 42 of 42, and a **fully green run is reachable** (previously the ceiling was
29). The residue is three enemy-AI-aggro tests that are genuinely load-sensitive
and vary run to run, all of which pass in isolation:

- `aoe_spell_damages_nearby_enemies`
- `pet_attacks_owners_target`
- `pet_pulls_aggro_via_threat_reaggro`

**Practical rule: a failure OUTSIDE those three is a real regression.** That is
the property the suite lost for three weeks and has now got back. Re-run, or run
the named test in isolation, before blaming a change.

The original 2026-06-15 analysis below still describes that residue accurately.

---

## Original notes (2026-06-15)

**Status: known tech debt, deferred.** Not a gameplay bug; the
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
