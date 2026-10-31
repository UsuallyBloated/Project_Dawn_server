//! Per-connection state held by the tick loop. One entry per connected
//! `client_id` (which equals the renet `ConnectToken.client_id` we minted,
//! which equals the player's `char_id`).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

/// Returns true when the server was launched with `PD_DEV_CMDS=1`.
/// Checked once at first call; cached for the process lifetime.
/// Gates `HealSelf` / `DamageSelf` so random players can't restore
/// themselves in a real deployment. Wire to a DB `is_gm` flag once
/// auth lands.
pub(super) fn dev_cmds_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("PD_DEV_CMDS").as_deref() == Ok("1"))
}

/// One authorized dev/GM command waiting for the tick's audit flush to write
/// it to `gm_actions`. Queued by [`PerConnection::authorize_dev_cmd`].
#[derive(Debug, Clone, PartialEq)]
pub struct GmAuditEntry {
    pub command: &'static str,
    pub args: String,
}

/// Dev/GM command budget: a token bucket per connection. A command that
/// cannot be recorded is REFUSED, never run unrecorded, so the budget is also
/// the bound on audit writes. The burst covers the Test Panel's largest
/// honest batch (its crafting-materials button sends ~34 `give`s in one
/// frame) several times over; the refill is what a flooding GM client is
/// held to afterwards.
pub const GM_CMD_BURST: f32 = 128.0;
pub const GM_CMD_REFILL_PER_SEC: f32 = 8.0;

/// What `PerConnection::authorize_dev_cmd` decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevCmdAuth {
    /// Authorized and recorded: run the command.
    Allowed,
    /// Not a dev server and not a GM account: ignore silently (a reply would
    /// be an oracle for whether dev commands are on).
    Denied,
    /// Authorized but over budget: do NOT run it. `notify` is true at most
    /// once a second, so the refusal line cannot itself become a flood.
    RateLimited { notify: bool },
}

/// Longest `args` text stored per audit row. Some of it is client-supplied
/// (an item name, a mob name), so it is cut here rather than trusted.
pub const GM_AUDIT_ARGS_MAX: usize = 200;

#[derive(Debug, Clone, Copy)]
pub struct Vec3f {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3f {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0, z: 0.0 };

    pub fn from_tuple(t: (f32, f32, f32)) -> Self {
        Self { x: t.0, y: t.1, z: t.2 }
    }

    pub fn into_tuple(self) -> (f32, f32, f32) {
        (self.x, self.y, self.z)
    }

    pub fn length(self) -> f32 {
        (self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    /// Clamp length to `max`. Used to enforce the server-side speed cap on
    /// move intents — anything longer becomes a unit-vector × max.
    pub fn clamp_length(self, max: f32) -> Self {
        let len = self.length();
        if len > max && len > 0.0 {
            let s = max / len;
            Self { x: self.x * s, y: self.y * s, z: self.z * s }
        } else {
            self
        }
    }

    pub fn sub(self, other: Self) -> Self {
        Self { x: self.x - other.x, y: self.y - other.y, z: self.z - other.z }
    }

    pub fn distance_to(self, other: Self) -> f32 {
        self.sub(other).length()
    }

    /// Returns `self / length` for non-zero vectors, else `ZERO`. Avoids
    /// NaN propagation when the AI computes a direction to a coincident
    /// target.
    pub fn normalize_or_zero(self) -> Self {
        let len = self.length();
        if len > 0.0 {
            Self { x: self.x / len, y: self.y / len, z: self.z / len }
        } else {
            Self::ZERO
        }
    }

    /// Step `self` toward `target` by at most `step` units. The y axis is
    /// preserved on `self` (enemies don't fly toward a player who's on a
    /// raised mesh; matches the GDScript `_move_at_speed` behaviour).
    pub fn step_toward(self, target: Self, step: f32) -> Self {
        let delta = target.sub(self);
        let dist = delta.length();
        if dist <= step || dist <= f32::EPSILON {
            Self { x: target.x, y: self.y, z: target.z }
        } else {
            let dir = delta.normalize_or_zero();
            Self {
                x: self.x + dir.x * step,
                y: self.y,
                z: self.z + dir.z * step,
            }
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)] // account_id/charisma land in audit + chat once those paths exist
pub struct PerConnection {
    pub char_id: i64,
    pub account_id: i64,
    pub name: String,
    pub race: String,
    pub class: String,
    pub level: i32,
    pub zone: Option<String>,
    pub pos: Vec3f,
    pub yaw: f32,
    /// Last application-layer message arrival. Used for our 10 s app-level
    /// heartbeat timeout; renet's transport has its own.
    pub last_packet: Instant,
    /// Tracks last persisted snapshot so we can skip checkpoint writes
    /// when nothing has moved.
    pub last_persisted_pos: Vec3f,
    pub last_persisted_yaw: f32,
    /// Last persisted snapshot of resources (Track 6). Compared against
    /// live values in the 60 s checkpoint pass to skip writes when nothing
    /// has changed. Distinct from the broadcast-throttle baseline below.
    pub last_persisted_hp: f32,
    pub last_persisted_mp: f32,
    pub last_persisted_stamina: f32,
    pub last_persisted_xp: i32,
    pub last_persisted_level: i32,
    /// Food and water (`world/hunger.rs`): seconds of fullness left, 0 to the
    /// cap. They run down while alive in the world; at zero the server eats
    /// the first food in slot order, and until then natural regen stops.
    pub hunger_secs: f32,
    pub thirst_secs: f32,
    pub last_persisted_hunger: f32,
    pub last_persisted_thirst: f32,
    /// Whether the character has been told the stage it logged in at (a
    /// character can arrive hungry or out; `drain` reports transitions only).
    pub appetite_announced: bool,
    /// The hunger window's feed (`hunger::update_due`): set when a counter
    /// jumps (an eat, a drink) or crosses a stage, so the next regen step
    /// sends a `HungerUpdate` instead of waiting out the minute.
    pub hunger_dirty: bool,
    /// When the last `HungerUpdate` went to this client, and the drain rate
    /// it carried (a change of rate, death or respawn, is a reason to send).
    pub hunger_sent_at: Option<Instant>,
    pub hunger_rate_sent: f32,
    /// Set after the application-layer Connect handshake completes. Until
    /// then we won't broadcast positions to this client.
    pub ready: bool,
    /// Set after the client sends `EnterWorld` (i.e. left the lobby). Gates
    /// EntitySpawn fan-out so peers don't see ghost bodies for clients
    /// still on the Enter World screen.
    pub in_world: bool,

    /// Camp + linkdead: set on a CLEAN disconnect (a `Disconnect` intent from
    /// Quit Game, or a completed `/camp`). The tick loop reads it when the
    /// transport drops to decide whether to reap the body immediately (clean)
    /// or let it linger as linkdead (unclean). See docs/design/camp_and_linkdead.md.
    pub clean_disconnect: bool,
    /// Camp + linkdead: when `Some`, this connection's transport has dropped
    /// uncleanly and the body is lingering in-world (still targetable, frozen
    /// in place). The reaper sweep removes it once `now - linkdead_since`
    /// passes [`super::LINKDEAD_SECS`]. `None` for a live connection.
    pub linkdead_since: Option<Instant>,

    /// Camp slice B: when `Some`, a voluntary `/camp` countdown is in progress
    /// (set when `Camp` is accepted). The per-tick camp sweep cancels it if the
    /// player stands/moves (`!is_sitting`) or has taken damage since it began,
    /// and completes it (clean logout) once `now - camp_since` passes
    /// [`super::CAMP_SECS`]. `None` when not camping.
    pub camp_since: Option<Instant>,
    /// Camp slice B: wall-clock of the most recent incoming damage applied to
    /// this player. The camp sweep compares it against `camp_since` to cancel a
    /// camp on "took damage since it started". Set at the (few) player-damage
    /// application sites in the tick loop; never cleared (it's a monotonic
    /// last-hit marker, only meaningful relative to `camp_since`).
    pub last_damaged_at: Option<Instant>,

    /// Wall-clock of this connection's most recent melee swing, stamped in the
    /// attack-apply. With `last_damaged_at` it marks "in combat" so the seated
    /// regen bonus is suppressed mid-fight (see `regen::sitting_bonus_applies`).
    /// Hand-agnostic on purpose — it is a combat marker, not a rate limit.
    pub last_attack_at: Option<Instant>,

    /// Server-authoritative bind point: where `Respawn` teleports this player.
    /// `None` = never bound, which respawns them at the starter spawn instead.
    /// Loaded from `characters.bind_*`, set by `BindAtCurrentLocation`.
    pub bind: Option<crate::db::BindPoint>,

    /// Phase 1 exploit gate — per-hand last-ACCEPTED-swing timestamps for the
    /// melee swing-rate limit. Index 0 = main hand, 1 = off hand (keyed by the
    /// Attack's `is_offhand`). Kept separate from `last_attack_at` because
    /// dual-wield swings two independent hands that legitimately co-fire in one
    /// tick; a single combined timer would false-throttle a legit dual-wielder.
    /// Advanced only on a swing the rate gate accepts (`tick.rs`).
    pub last_swing_at: [Option<Instant>; 2],
    /// The pacing the hand's last ACCEPTED swing was judged by (the honest
    /// interval from its weapon and the haste the server saw at the time),
    /// `f32::INFINITY` before the hand has swung. The limiter holds a swing
    /// to the lesser of this and the current pacing, because the client
    /// timed THIS swing when the LAST one fired: a Haste buff that ran out,
    /// or a slower weapon swapped in, between the two must not cost the
    /// honest swing already timed under the old pacing. Stamped with
    /// `last_swing_at`.
    pub last_swing_pace: [f32; 2],

    /// Wall-clock of the last Meditate skill-up tick. The regen loop advances
    /// the `meditate` casting skill roughly once per 6 s of medding (sitting with
    /// mana below full); this gates it to that cadence.
    pub last_meditate_at: Option<Instant>,

    /// Corpse/resurrection Slice 0: set true when the server-side death sweep
    /// has already processed this player's death (applied the xp penalty, fanned
    /// EntityDied). Guards the sweep from re-running every tick while hp stays 0,
    /// and is cleared on `Respawn` so the next death is processed afresh.
    pub death_processed: bool,
    /// The dead-state gate has told this player "You cannot do that while
    /// dead." since they died. The line is said once per death, not once per
    /// refused message (a client sends several while dead; the chat used to
    /// fill with it). Cleared with `death_processed` on `Respawn`.
    pub dead_refusal_told: bool,
    /// Corpse/resurrection Slice 1: set true by `kill_player`, so EVERY death
    /// path flags a corpse (the server-detected death sweep AND a client-first
    /// `DeathBroadcast` — Trigger Death, fall damage, a linkdead body that dies).
    /// The corpse-creation pass consumes it (makes the corpse, then clears it).
    /// Distinct from `death_processed` so a client-first death still leaves one.
    pub corpse_pending: bool,
    /// Corpse/resurrection Slice 3: the XP this death cost (nominal
    /// `floor(xp_to_next * 0.05)`, or 0 for a grace / sub-level-5 death).
    /// Captured in `apply_death_penalty` and read by the corpse-creation pass
    /// onto the corpse, so a Cleric/Paladin res can refund a % of it.
    pub death_lost_xp: i32,
    /// Corpse/resurrection Slice 3: a pending resurrection offer on this player's
    /// corpse — `(corpse_id, xp_percent)` recorded when a Cleric/Paladin casts a
    /// res on it, read when the player accepts (so the refund % can't be forged by
    /// the client) and cleared on accept/decline. In-memory only — a server
    /// restart simply drops an un-answered offer.
    pub pending_res_offer: Option<(protocol::world::EntityId, u32)>,
    /// Quest ids this character has already turned in (server-authoritative
    /// quest rewards: a quest pays once per character, ever). Loaded from the
    /// `completed_quests` table at login; the tick loop checks it before
    /// awarding a `CompleteQuest` and inserts after the DB write succeeds.
    pub completed_quests: std::collections::HashSet<String>,
    /// PD_W0024: accepted-but-not-completed quests → per-objective progress
    /// counts, in the same order as the quest's objectives in quests.toml.
    /// Loaded from the `active_quests` table at login (normalized in
    /// `from_spawn`), mutated by the kill sweep + accept/abandon/turn-in
    /// intents, and persisted per-mutation (`db::save_quest_progress`) — NOT
    /// via the 60s checkpoint, so neither a crash nor the disconnect flush
    /// can lose counted kills.
    pub active_quests: std::collections::HashMap<String, Vec<i32>>,
    /// Quest ids whose progress changed this tick (the kill sweep is sync, so
    /// it can't await the DB). Drained by the end-of-tick quest flush and by
    /// the disconnect path in `reap_connection`.
    pub quests_dirty: std::collections::HashSet<String>,
    /// Highest move sequence we've accepted from this client. Out-of-order
    /// packets get dropped (unreliable channel, so reorder is expected).
    pub last_move_seq: u32,
    /// Most recent movement intent (unit vector, or zero for "stop").
    /// Updated when a Move message is accepted; integrated once per
    /// tick by the tick loop, NOT per-message. Per-message integration
    /// would 3× speed under typical client send rates.
    pub latest_direction: Vec3f,
    /// Wall-clock time of the most recent accepted Move. The tick loop
    /// stops integrating `latest_direction` once this gets older than
    /// [`super::STALE_MOVE_THRESHOLD`] so a crashed client doesn't keep
    /// visually moving until heartbeat timeout.
    pub last_move_received: Option<Instant>,
    /// The `jumping` flag of the last accepted Move, so a jump is relayed on
    /// its rising edge and a held flag relays nothing more.
    pub last_move_jumping: bool,
    /// When this player's last jump was relayed (`JUMP_RELAY_MIN_INTERVAL`).
    pub last_jump_relayed_at: Option<Instant>,

    /// Track 6: authoritative resources. Seeded from DB at spawn, mutated
    /// by the regen tick + future combat / heal / damage paths, fanned out
    /// as `HealthUpdate` / `ManaUpdate` / `StaminaUpdate` whenever a
    /// threshold-crossing change lands.
    pub hp: f32,
    pub max_hp: f32,
    pub mp: f32,
    pub max_mp: f32,
    pub stamina: f32,
    pub max_stamina: f32,
    pub xp: i32,
    pub xp_to_next: i32,
    pub coins: protocol::world::Coins,
    /// Per-character bank wallet (Banker NPC, slice 1) — zero-weight coin
    /// storage. Mutated by deposit / withdraw / exchange intents and
    /// persisted via `bank_dirty`; seeded from the DB at load.
    pub bank_coins: protocol::world::Coins,

    /// Track 6: authoritative base stats. The damage formula port (sub-task
    /// 2) reads these; for now sub-task 1 just loads them so the values are
    /// available downstream.
    pub strength: i32,
    pub dexterity: i32,
    pub agility: i32,
    pub intelligence: i32,
    pub wisdom: i32,
    pub charisma: i32,
    pub constitution: i32,

    /// Track 6: sitting state. Flipped by the `Sit` / `Stand` client
    /// intents; multiplies regen rate per `regen::SITTING_HP_MULT` etc.
    pub is_sitting: bool,
    /// When the character last sat down. A memorize must stay seated from
    /// its start, so standing and sitting again in between voids it.
    pub sat_at: Option<Instant>,

    /// The memorized spell bar (client-authority audit fix 5): gem to spell
    /// name. Loaded from `character_spell_gems` at entry, changed only by a
    /// memorize, a forget or the one-time seed; a cast of a spell not in it is
    /// refused.
    pub spell_gems: std::collections::BTreeMap<i32, String>,
    /// The server holds this character's bar (after a memorize or the seed).
    pub spell_gems_seeded: bool,
    /// A memorize in progress: (gem, spell name, when it began).
    pub memorize_pending: Option<(i32, String, Instant)>,

    /// Track 6 sub-task 3 — sum of armor_class across equipped armor.
    /// Updated by `EquipUpdate` intents; consumed by the damage formula
    /// (combat::receive_damage) to compute the AC/(AC+ARMOR_DR_DIVISOR)
    /// reduction matching `autoloads/combat.gd`. Zero until the client
    /// sends its first EquipUpdate.
    pub equipped_armor: i32,

    /// Track 6 sub-task 3 — dev /pvp toggle. Both attacker and target
    /// must have this flipped on for `combat::can_attack` to allow PvP
    /// damage. Future duel-acceptance / PvP-zone / PvP-server rules
    /// will layer atop the same flag.
    pub pvp_override_on: bool,

    /// True when the server was started with `PD_DEV_CMDS=1`. A process-wide
    /// switch: same for every connection. Good for local solo iteration, but
    /// all-or-nothing on a shared server. Gate dev commands via
    /// `can_use_dev_cmds`, not this field directly.
    pub is_dev: bool,

    /// Whether this caster's spells can fizzle: `Config::fizzles`, stamped at
    /// the connect site. Always true on a running server; the integration
    /// harness turns it off for every test but the fizzle test.
    pub fizzles: bool,

    /// Per-account GM flag, carried from the signed connect token's `user_data`
    /// (the auth service reads it from `accounts.is_gm` at token mint). Unlike
    /// `is_dev`, this is per-connection, so a hosted server run with
    /// `PD_DEV_CMDS` off can grant dev tools to a GM account only. A modified
    /// client can't forge it: `user_data` rides the signed netcode token.
    pub is_gm: bool,

    /// Authorized dev/GM commands issued this tick, waiting for the tick's
    /// audit flush to write them to `gm_actions`. Filled only by
    /// `authorize_dev_cmd`, which is also the gate: a command is queued here
    /// or it does not run.
    pub gm_audit: Vec<GmAuditEntry>,
    /// The item ledger's queue (`world/ledger.rs`): one row per movement of
    /// items or coin into or out of this character's holdings since the last
    /// store write, pushed by `ledger` beside the dirty flag the move set and
    /// written inside the next transaction that persists the stores (the
    /// flush, the trade commit, the corpse loot, the death strip), which
    /// clears it. Rolled back with the in-memory move on a failed persist.
    pub item_events: Vec<crate::db::ItemEventRow>,
    /// The command budget (see `GM_CMD_BURST`): tokens left, and when they
    /// were last refilled.
    pub gm_cmd_tokens: f32,
    pub gm_cmd_refill_at: Instant,
    /// Commands refused over budget since the last `audit_overflow` row, and
    /// when that row was last written / the GM last told (each at most once
    /// a second).
    pub gm_cmd_refused: u32,
    pub gm_overflow_row_at: Option<Instant>,
    pub gm_rate_notice_at: Option<Instant>,

    /// Set when the server has sent this connection a Kick (the ban sweep).
    /// The tick drops the transport `KICK_FLUSH_GRACE` later; from there the
    /// character leaves by the ordinary unclean-exit path.
    pub kicked_at: Option<Instant>,

    /// Track 7 — AOI grid cell the player currently occupies. Derived from
    /// `pos.x` / `pos.z` via `aoi::cell_for`; updated by the tick loop
    /// whenever the player's position crosses a cell boundary. Used to
    /// filter position broadcasts and drive the spawn/despawn fan-out when
    /// the neighborhood set changes.
    pub aoi_cell: (i32, i32),

    /// Track 6: fractional regen accumulator. The 20 Hz tick produces
    /// sub-integer amounts; we accumulate and only mutate `hp`/`mp`/
    /// `stamina` (and fan out) when the integer part bumps. Reset to 0.0
    /// on reset events (death, zone, etc.).
    pub regen_hp_acc: f32,
    pub regen_mp_acc: f32,
    pub regen_stamina_acc: f32,

    /// Track 6: broadcast-throttle baselines. `HealthUpdate` /
    /// `ManaUpdate` / `StaminaUpdate` fire when either:
    ///   • the current value diverged from the last broadcast by >5% of
    ///     max (big swings land immediately), OR
    ///   • at least `RESOURCE_MAX_BROADCAST_GAP` elapsed since the last
    ///     fan-out (slow regen still ticks the UI on a clock).
    pub last_bcast_hp: f32,
    pub last_bcast_mp: f32,
    pub last_bcast_stamina: f32,
    pub last_bcast_at: Option<Instant>,

    /// Track 4 sub-task 2 — last-known casting state, used to seed a peer
    /// who enters the world mid-cast. `cast_spell_name` is empty when not
    /// casting. `cast_remaining_at_set` and `cast_set_at` together let the
    /// server estimate "how much time is left right now" when forwarding
    /// to a late joiner (clamped to >= 0).
    pub cast_spell_name: String,
    pub cast_total_duration: f32,
    pub cast_set_at: Option<Instant>,

    /// Track 17.2 — caster's position at the moment CastStartBroadcast
    /// arrived. The CastSpell gate compares this against `pos` at gate
    /// time; >MAX_CAST_MOVE_DISTANCE rejects the cast. Closes the
    /// "forged client casts while running" hole the cast-time gate
    /// alone couldn't catch.
    pub cast_start_pos: Vec3f,
    /// When the last on-hit channeling roll interrupted a cast. The CastSpell
    /// gate refuses a completion whose bar began at or before this instant,
    /// so a hit and a finished cast in the same tick cannot both win (the
    /// gate otherwise reads the cache as it was at dispatch, before the hit
    /// cleared it). Spell batch step 0.
    pub cast_interrupted_at: Option<Instant>,

    /// Track 17.2 — per-spell cooldown map. CastSpell rejects a cast
    /// whose entry is in the future; a successful cast writes the
    /// next-ready instant from `spells.toml::cooldown`. Spell-name
    /// keyed (matches `cast_spell_name`); cleared on disconnect via
    /// `connections.remove(&cid)`.
    pub spell_cooldowns: HashMap<String, Instant>,

    /// Active skills (client-authority audit item 6): the instant each used
    /// skill is ready again, by name. A `UseSkill` inside it (less
    /// `active_skills::COOLDOWN_GRACE`) is refused at no cost. Cleared on
    /// disconnect with the connection, as the spell cooldowns are.
    pub skill_cooldowns: HashMap<String, Instant>,

    /// Bard songs (the rework of 2026-10-20): the one song being sung, if
    /// any, and when it next pulses. Set when a song's cast lands, cleared by
    /// re-singing it, `StopSong`, any non-song cast, death, or the
    /// connection going (the body never sings).
    pub active_song: Option<String>,
    pub song_next_pulse_at: Option<Instant>,

    /// The global cooldown (spell batch step 4): no spell may START inside
    /// `GLOBAL_COOLDOWN` of the last accepted cast, Bard songs excepted on
    /// both sides (twisting is casting songs back to back). Stamped when a
    /// CastSpell is accepted, checked at CastStart (a timed cast) and at
    /// CastSpell (an instant one).
    pub gcd_until: Option<Instant>,

    /// Track 4 sub-task 3 — last buff snapshot the client broadcast.
    /// Used to seed new joiners; live updates fan out via the
    /// BuffSnapshotFanOut outcome. Empty Vec = "no active buffs".
    /// Track 6 sub-task 4a: this becomes a SERVER-DERIVED cache —
    /// rebuilt from `active_buffs` whenever buff state changes.
    /// Clients no longer originate the snapshot (BuffSnapshotBroadcast
    /// is deprecated; sub-task 4b removes it after the full lift).
    pub buff_snapshot: Vec<(String, f32)>,
    pub buff_snapshot_set: bool,

    /// Track 6 sub-task 4a — server-authoritative active buffs.
    /// HoT / MP regen / Lich Form for 4a; stat buffs / speed / haste /
    /// shield / absorb / CC follow in 4b. Ticked every tick;
    /// expirations + applies refresh buff_snapshot for the
    /// fan-out path.
    pub active_buffs: Vec<super::buffs::ActiveBuff>,

    /// Track 22.H — the player's current target id (peer / enemy /
    /// pet / none). Fanned to AOI peers as `EntityTarget` on every
    /// `SetTarget` intent so the target-of-target HUD frame can
    /// resolve what tracked remote players are attacking.
    pub current_target: Option<protocol::world::EntityId>,

    /// Track 11.3 — last enemy this player attacked. Pet AI reads this
    /// to inherit the owner's target so the pet auto-engages whatever
    /// the player is fighting. `last_attacked_at` decays the
    /// inheritance after `PET_TARGET_DECAY_SECS` so the pet returns to
    /// follow when the player stops attacking.
    pub last_attacked_enemy: Option<protocol::world::EntityId>,
    pub last_attacked_at: Option<Instant>,

    /// Track 12 Piece B — Beast Master warder respawn timer. Set
    /// when the warder dies (`Some(now + WARDER_RETREAT_SECS)`);
    /// the tick loop's pre-AI sweep auto-summons a fresh warder at
    /// 30 % HP when this time has passed. Cleared on respawn or on
    /// disconnect.
    pub warder_respawn_at: Option<Instant>,

    /// Track 13.1 — server-side inventory snapshot. Loaded from the
    /// `character_items` table on `load_character`, mutated when
    /// loot grants land (loot intent dispatch in `tick.rs`),
    /// persisted on disconnect + the periodic checkpoint cadence.
    /// `inventory_dirty` flips when add_item / future move/drop
    /// mutates the snapshot; the persistence sweep clears it.
    pub inventory: super::inventory::PlayerInventory,
    pub inventory_dirty: bool,

    /// Flips when anything mutates `coins` (vendor buy/sell, dev grants);
    /// the persistence sweep + disconnect flush clear it. Without this the
    /// wallet silently resets to the DB row on next login.
    pub coins_dirty: bool,

    /// Dirty flag for `bank_coins`; set on any deposit / withdraw / exchange,
    /// cleared by the checkpoint sweep + disconnect flush (mirrors
    /// `coins_dirty`).
    pub bank_dirty: bool,

    /// Banker slice 2 — per-character item vault (10 slots, char-keyed).
    /// Loaded at connect, persisted char-keyed via `bank_items_dirty`.
    pub bank_items: super::inventory::ItemVault,
    pub bank_items_dirty: bool,

    /// Banker slice 2 — the account-shared item vault (2 slots, EQ shared
    /// bank). The authoritative copy is per-connection only because login is
    /// single-session-per-account (enforced at connect); persisted by
    /// `account_id` via `account_bank_items_dirty`, never by char_id.
    pub account_bank_items: super::inventory::ItemVault,
    pub account_bank_items_dirty: bool,

    /// Per-player `/autosplit` toggle (default on). Governs only this
    /// player's *own* loots: on + Round Robin → coin they loot splits
    /// among the nearby group; off → they keep it all. Session-scoped for
    /// now (not persisted). See docs/design/group_loot_and_coin.md.
    pub autosplit: bool,

    /// Track 18.1 — server-side passive skill scores. Three parallel
    /// maps mirror WeaponSkills / ArmorSkills / CastingSkills on the
    /// client. Seeded from `character_skills` at load (`seed_starting_scores`
    /// fills untrained classes' rows with 0); mutated on the attack /
    /// cast / armor-hit paths via `skills::try_advance`; persisted on
    /// checkpoint + disconnect cadence. `skills_dirty` flips when any
    /// score changes; the persistence sweep clears it.
    pub weapon_skills: HashMap<String, i32>,
    pub armor_skills: HashMap<String, i32>,
    pub casting_skills: HashMap<String, i32>,
    pub skills_dirty: bool,

    /// Faction standings by faction id (`world/factions.rs`), only the
    /// factions this character has ever moved; a missing one sits at its
    /// starting value. `factions_dirty` flips on any change; the persistence
    /// sweep clears it.
    pub factions: HashMap<String, f32>,
    pub factions_dirty: bool,
    /// The alignment score, the server's since migration 0016
    /// (`world/alignment.rs`): moved by kills, quests and spells, read by
    /// the prestige-class gates. Persisted with the resources; `None` for
    /// the last persisted value means the server never wrote one (a
    /// character seeded at this load), so the next checkpoint writes it.
    pub alignment: f32,
    pub last_persisted_alignment: Option<f32>,
    /// The score the client was last told (`AlignmentUpdate`); None until
    /// the enter-world send.
    pub alignment_sent: Option<f32>,

    /// Track 14.2 — last accumulated stat bonuses from equipped items.
    /// `recompute_equipped_stats` reads this to know what to subtract
    /// before re-summing across the current equipment map. Stays
    /// orthogonal to `active_buffs` deltas: gear baseline lives here;
    /// buffs add/undo on top of `conn.strength` / `conn.max_hp` /
    /// etc. directly. Starts at zero; populated when load_inventory
    /// completes (so persisted equip rebuilds bonuses on connect).
    pub equip_stat_bonuses: super::inventory::EquipStatBonuses,
}

impl PerConnection {
    pub fn from_spawn(spawn: crate::db::CharacterSpawn, now: Instant) -> Self {
        let pos = Vec3f::from_tuple(spawn.pos);
        let completed_quests: std::collections::HashSet<String> =
            spawn.completed_quests.into_iter().collect();
        // Normalize the loaded journal against the live quest table: drop
        // quests that no longer exist in quests.toml or that are somehow both
        // active and completed (a crash between the completion record and the
        // active-row delete), and shape every progress vector to exactly one
        // non-negative count per objective so QuestProgress indexing and
        // objectives_met never see a malformed row.
        let active_quests: std::collections::HashMap<String, Vec<i32>> = spawn
            .active_quests
            .into_iter()
            .filter_map(|(quest_id, mut progress)| {
                let Some(quest) = super::quests::lookup(&quest_id) else {
                    tracing::warn!(quest_id = ?quest_id,
                        "active quest not in quests.toml; dropping from journal");
                    return None;
                };
                if completed_quests.contains(&quest_id) {
                    return None;
                }
                progress.resize(quest.objectives.len(), 0);
                for p in &mut progress {
                    *p = (*p).max(0);
                }
                Some((quest_id, progress))
            })
            .collect();
        // Factions and alignment: a character the server has never held a
        // score for is seeded from its class's start, exactly as the client
        // seeded itself before the score moved server-side. A standing for a
        // faction no longer in factions.toml is dropped.
        let factions: HashMap<String, f32> = spawn
            .factions
            .iter()
            .filter(|(id, _)| super::factions::lookup(id).is_some())
            .map(|(id, standing)| {
                (
                    id.clone(),
                    standing.clamp(-super::factions::STANDING_CAP, super::factions::STANDING_CAP),
                )
            })
            .collect();
        let alignment = spawn
            .alignment
            .unwrap_or_else(|| super::alignment::starting_score(&spawn.class))
            .clamp(-super::alignment::SCORE_CAP, super::alignment::SCORE_CAP);
        Self {
            char_id: spawn.char_id,
            account_id: spawn.account_id,
            name: spawn.name,
            race: spawn.race,
            class: spawn.class,
            level: spawn.level,
            zone: spawn.zone,
            pos,
            yaw: spawn.yaw,
            last_packet: now,
            last_persisted_pos: pos,
            last_persisted_yaw: spawn.yaw,
            last_persisted_hp: spawn.hp,
            last_persisted_mp: spawn.mp,
            last_persisted_stamina: spawn.stamina,
            hunger_secs: spawn.hunger.clamp(0.0, super::hunger::FULLNESS_CAP_SECS),
            thirst_secs: spawn.thirst.clamp(0.0, super::hunger::FULLNESS_CAP_SECS),
            last_persisted_hunger: spawn.hunger,
            last_persisted_thirst: spawn.thirst,
            appetite_announced: false,
            hunger_dirty: true,
            hunger_sent_at: None,
            hunger_rate_sent: -1.0,
            last_persisted_xp: spawn.xp,
            last_persisted_level: spawn.level,
            ready: false,
            in_world: false,
            clean_disconnect: false,
            linkdead_since: None,
            camp_since: None,
            last_damaged_at: None,
            last_attack_at: None,
            bind: spawn.bind,
            last_swing_at: [None, None],
            last_swing_pace: [f32::INFINITY; 2],
            last_meditate_at: None,
            // A character loaded at 0 HP died before it left (a crash, a
            // window-X, a server restart while dead) and has paid: the death
            // sweep must not kill it again (a second XP penalty and a second,
            // empty corpse; found by the 2026-10-12 review). Respawn still
            // works, since it gates on exactly this flag. The client learns it
            // is dead from the HP seed, as it always did.
            death_processed: spawn.hp <= 0.0,
            dead_refusal_told: false,
            corpse_pending: false,
            death_lost_xp: 0,
            pending_res_offer: None,
            completed_quests,
            active_quests,
            quests_dirty: std::collections::HashSet::new(),
            last_move_seq: 0,
            latest_direction: Vec3f::ZERO,
            last_move_received: None,
            last_move_jumping: false,
            last_jump_relayed_at: None,
            hp: spawn.hp,
            max_hp: spawn.max_hp,
            mp: spawn.mp,
            max_mp: spawn.max_mp,
            stamina: spawn.stamina,
            max_stamina: spawn.max_stamina,
            xp: spawn.xp,
            xp_to_next: spawn.xp_to_next,
            coins: spawn.coins,
            bank_coins: spawn.bank_coins,
            strength: spawn.strength,
            dexterity: spawn.dexterity,
            agility: spawn.agility,
            intelligence: spawn.intelligence,
            wisdom: spawn.wisdom,
            charisma: spawn.charisma,
            constitution: spawn.constitution,
            is_sitting: false,
            sat_at: None,
            spell_gems: std::collections::BTreeMap::new(),
            spell_gems_seeded: false,
            memorize_pending: None,
            equipped_armor: 0,
            pvp_override_on: false,
            is_dev: dev_cmds_enabled(),
            fizzles: true,
            // Defaulted here; the real value is stamped from the connect
            // token's user_data at the connect site in tick.rs (from_spawn
            // only sees the character spawn, not the account's GM flag).
            is_gm: false,
            gm_audit: Vec::new(),
            item_events: Vec::new(),
            gm_cmd_tokens: GM_CMD_BURST,
            gm_cmd_refill_at: now,
            gm_cmd_refused: 0,
            gm_overflow_row_at: None,
            gm_rate_notice_at: None,
            kicked_at: None,
            aoi_cell: (0, 0), // tick.rs sets the real cell from aoi::cell_for after construction
            regen_hp_acc: 0.0,
            regen_mp_acc: 0.0,
            regen_stamina_acc: 0.0,
            last_bcast_hp: spawn.hp,
            last_bcast_mp: spawn.mp,
            last_bcast_stamina: spawn.stamina,
            last_bcast_at: None,
            cast_spell_name: String::new(),
            cast_total_duration: 0.0,
            cast_set_at: None,
            cast_start_pos: Vec3f::ZERO,
            cast_interrupted_at: None,
            spell_cooldowns: HashMap::new(),
            skill_cooldowns: HashMap::new(),
            active_song: None,
            song_next_pulse_at: None,
            gcd_until: None,
            buff_snapshot: Vec::new(),
            buff_snapshot_set: false,
            active_buffs: Vec::new(),
            last_attacked_enemy: None,
            last_attacked_at: None,
            current_target: None,
            warder_respawn_at: None,
            inventory: super::inventory::PlayerInventory::new(),
            inventory_dirty: false,
            coins_dirty: false,
            bank_dirty: false,
            bank_items: super::inventory::ItemVault::new(super::inventory::BANK_VAULT_SLOTS),
            bank_items_dirty: false,
            account_bank_items: super::inventory::ItemVault::new(super::inventory::ACCOUNT_VAULT_SLOTS),
            account_bank_items_dirty: false,
            autosplit: true,
            equip_stat_bonuses: super::inventory::EquipStatBonuses::default(),
            weapon_skills: HashMap::new(),
            armor_skills: HashMap::new(),
            casting_skills: HashMap::new(),
            skills_dirty: false,
            factions,
            factions_dirty: false,
            alignment,
            last_persisted_alignment: spawn.alignment,
            alignment_sent: None,
        }
    }

    /// Every standing as rows for `db::save_factions`.
    pub fn faction_rows(&self) -> Vec<(String, f32)> {
        self.factions.iter().map(|(id, s)| (id.clone(), *s)).collect()
    }

    /// This character's PERSONAL standing with a faction: what kills, quests
    /// and the GM tool have moved, zero when nothing has.
    pub fn faction_standing(&self, faction: &super::factions::Faction) -> f32 {
        self.factions.get(&faction.id).copied().unwrap_or(0.0)
    }

    /// The standing the faction's NPCs see: the personal number plus the
    /// character's starting modifier with it for its race and class.
    pub fn faction_shown(&self, faction: &super::factions::Faction) -> f32 {
        super::factions::shown_standing(self.faction_standing(faction), faction, &self.race, &self.class)
    }

    pub fn faction_level(&self, faction: &super::factions::Faction) -> super::factions::Level {
        super::factions::level_for(self.faction_shown(faction))
    }

    /// How an NPC's faction regards this character; Indifferent for an NPC
    /// with no faction (the soul binder serves everyone).
    pub fn npc_faction_level(&self, npc: &super::npcs::Npc) -> super::factions::Level {
        npc.faction
            .as_deref()
            .and_then(super::factions::lookup)
            .map(|f| self.faction_level(f))
            .unwrap_or(super::factions::Level::Indifferent)
    }

    /// Will `npc` deal with this character? Its faction must regard them at
    /// `min` or better: Apprehensive for a vendor or banker (EQ's merchants
    /// refuse at Dubious), Indifferent for a quest giver.
    pub fn npc_serves(&self, npc: &super::npcs::Npc, min: super::factions::Level) -> bool {
        self.npc_faction_level(npc) >= min
    }

    /// The NPC-service gate with factions: Ok(the NPC) when an NPC of `kind`
    /// is in service range and will deal with this character; Err(Some(npc))
    /// when one is in range but refuses them (the first such); Err(None)
    /// when none is in range at all.
    pub fn npc_service(
        &self,
        kind: &str,
    ) -> Result<&'static super::npcs::Npc, Option<&'static super::npcs::Npc>> {
        let in_range = super::npcs::all_within_range(kind, self.pos);
        if let Some(npc) = in_range.iter().find(|n| self.npc_serves(n, super::factions::Level::Apprehensive)) {
            return Ok(npc);
        }
        Err(in_range.first().copied())
    }

    /// The class this character casts and uses skills as: its own, or the
    /// prestige class its alignment has carried it to.
    pub fn effective_class(&self) -> String {
        super::alignment::effective_class(&self.class, self.alignment)
    }

    /// May this connection use dev / GM commands (`/give`, `HealSelf`,
    /// spawn-mob, grant-XP, etc.)? True if the whole server is a dev box
    /// (`PD_DEV_CMDS=1`, so `is_dev`) OR this specific account is a GM
    /// (`is_gm`). Every dev-command handler gates on this, never on the raw
    /// fields, so a hosted server can run with `PD_DEV_CMDS` off and still let
    /// a GM account keep its tools.
    pub fn can_use_dev_cmds(&self) -> bool {
        self.is_dev || self.is_gm
    }

    /// The one gate for a dev/GM command: authorize it, charge its budget and
    /// queue its audit row (`gm_actions`), all or nothing. Every dev handler
    /// calls this at the point the command is about to take effect and runs
    /// it only on `Allowed`, so "authorized" always means "recorded": a
    /// command that cannot be recorded is refused, never run unrecorded.
    ///
    /// The row holds what an authorized account ISSUED (the server log holds
    /// the outcome), and `via` names every authority in force: `gm` for a GM
    /// account, `dev` for a `PD_DEV_CMDS` server, `gm+dev` for both. On a
    /// hosted server every row should read plain `via=gm`; anything with
    /// `dev` in it means the process ran with dev commands on for everyone.
    pub fn authorize_dev_cmd(
        &mut self,
        now: Instant,
        command: &'static str,
        args: String,
    ) -> DevCmdAuth {
        if !self.can_use_dev_cmds() {
            return DevCmdAuth::Denied;
        }
        let elapsed = now.saturating_duration_since(self.gm_cmd_refill_at).as_secs_f32();
        self.gm_cmd_tokens = (self.gm_cmd_tokens + elapsed * GM_CMD_REFILL_PER_SEC).min(GM_CMD_BURST);
        self.gm_cmd_refill_at = now;
        if self.gm_cmd_tokens < 1.0 {
            self.gm_cmd_refused = self.gm_cmd_refused.saturating_add(1);
            let notify = self
                .gm_rate_notice_at
                .is_none_or(|t| now.saturating_duration_since(t).as_secs_f32() >= 1.0);
            if notify {
                self.gm_rate_notice_at = Some(now);
            }
            return DevCmdAuth::RateLimited { notify };
        }
        self.gm_cmd_tokens -= 1.0;
        let via = match (self.is_gm, self.is_dev) {
            (true, true) => "gm+dev",
            (true, false) => "gm",
            _ => "dev",
        };
        let args: String = args.chars().take(GM_AUDIT_ARGS_MAX).collect();
        self.gm_audit.push(GmAuditEntry {
            command,
            args: format!("{args} via={via}"),
        });
        DevCmdAuth::Allowed
    }

    /// True if the connection has gone silent for at least
    /// [`super::HEARTBEAT_TIMEOUT`] of wall-clock time.
    pub fn is_app_idle(&self, now: Instant) -> bool {
        now.duration_since(self.last_packet) >= super::HEARTBEAT_TIMEOUT
    }

    /// Record a Move's `jumping` flag and say whether a jump should be
    /// relayed now: only on the flag's rising edge (a held flag is one jump),
    /// and at most once per [`super::JUMP_RELAY_MIN_INTERVAL`]. A relayed
    /// jump stands the player, as a step does. The flag never touches
    /// position: the relay is for the peers' screens only.
    pub fn note_move_jump(&mut self, jumping: bool, now: Instant) -> bool {
        let rising = jumping && !self.last_move_jumping;
        self.last_move_jumping = jumping;
        if !rising {
            return false;
        }
        let due = self
            .last_jump_relayed_at
            .map_or(true, |t| now.duration_since(t) >= super::JUMP_RELAY_MIN_INTERVAL);
        if !due {
            return false;
        }
        self.last_jump_relayed_at = Some(now);
        self.is_sitting = false;
        true
    }

    pub fn touch(&mut self, now: Instant) {
        self.last_packet = now;
    }

    /// Has the in-memory position/yaw drifted from the last persisted
    /// snapshot? Cheap exact-equality check — the 60s checkpoint cadence
    /// makes float-equality drift a non-issue (we'll write again next pass).
    pub fn is_dirty_for_persist(&self) -> bool {
        let p = self.pos;
        let lp = self.last_persisted_pos;
        p.x != lp.x || p.y != lp.y || p.z != lp.z || self.yaw != self.last_persisted_yaw
    }

    /// Is any item or coin store (inventory, wallet, either vault) waiting to
    /// be written? The one list of those stores: `persist_dirty_stores` writes
    /// exactly these, and the logout retry queue holds a character while any
    /// is set, so a store added here is covered by both.
    pub fn any_store_dirty(&self) -> bool {
        self.inventory_dirty
            || self.coins_dirty
            || self.bank_dirty
            || self.bank_items_dirty
            || self.account_bank_items_dirty
    }

    /// Queue an item ledger row for this character, stamped now
    /// (`world/ledger.rs`). Called at the site that makes the move in memory,
    /// beside the dirty flag it sets; the row is written with the next store
    /// transaction and leaves the queue only when that commits.
    pub fn ledger(&mut self, mut row: crate::db::ItemEventRow) {
        row.at = super::ledger::unix_now_secs();
        row.char_id = self.char_id;
        self.item_events.push(row);
    }

    /// Where the ledger queue stands, for a site that may have to undo its
    /// in-memory move on a failed persist: `ledger_rollback(mark)` drops
    /// everything queued since, as the inventory snapshot it restores drops
    /// the move.
    pub fn ledger_mark(&self) -> usize {
        self.item_events.len()
    }

    pub fn ledger_rollback(&mut self, mark: usize) {
        self.item_events.truncate(mark);
    }

    pub fn mark_persisted(&mut self) {
        self.last_persisted_pos = self.pos;
        self.last_persisted_yaw = self.yaw;
    }

    /// Track 6: resources / xp / level dirty since last
    /// `checkpoint_resources` write. The 60 s persistence pass uses this
    /// to skip the write when a connection has been idle (no regen
    /// happened, no combat). `level` participates because level-ups
    /// mutate it server-side now too.
    pub fn is_dirty_for_resource_persist(&self) -> bool {
        self.hp != self.last_persisted_hp
            || self.mp != self.last_persisted_mp
            || self.stamina != self.last_persisted_stamina
            || self.xp != self.last_persisted_xp
            || self.level != self.last_persisted_level
            // The counters move every tick while in the world; a second's
            // worth is the threshold, so an idle lobby connection stays clean.
            || (self.hunger_secs - self.last_persisted_hunger).abs() >= 1.0
            || (self.thirst_secs - self.last_persisted_thirst).abs() >= 1.0
            || Some(self.alignment) != self.last_persisted_alignment
    }

    pub fn mark_resources_persisted(&mut self) {
        self.last_persisted_hp = self.hp;
        self.last_persisted_mp = self.mp;
        self.last_persisted_stamina = self.stamina;
        self.last_persisted_xp = self.xp;
        self.last_persisted_level = self.level;
        self.last_persisted_hunger = self.hunger_secs;
        self.last_persisted_thirst = self.thirst_secs;
        self.last_persisted_alignment = Some(self.alignment);
    }

    /// Camp + linkdead: true once a body that dropped uncleanly has lingered at
    /// least `window`, i.e. the reaper should remove it now. A live connection
    /// (`linkdead_since == None`) is never expired. The tick loop's reaper
    /// sweep uses this with [`super::LINKDEAD_SECS`].
    pub fn linkdead_expired(&self, now: Instant, window: std::time::Duration) -> bool {
        self.linkdead_since
            .is_some_and(|t| now.duration_since(t) >= window)
    }

    /// Record that incoming damage just landed on this player. Two things ride
    /// on it. Camp slice B: the per-tick camp sweep cancels an in-progress
    /// `/camp` ("cancelled by damage"). And the seated-combat rule (classic
    /// EQ, user call 2026-10-14): ANY damage stands a seated player, so the
    /// seated regen rate, `/camp` and the seated-target damage rule all end
    /// with the first hit; the client stands its own body on the Hit it
    /// receives, since nothing on the wire carries the flag. Called at every
    /// player-damage application site in the tick loop (melee, spells, DoT
    /// ticks, damage shields): one home for what taking damage does to you.
    pub fn note_damage_taken(&mut self, now: Instant) {
        self.last_damaged_at = Some(now);
        self.is_sitting = false;
    }

    /// The sum of the player's active Haste buffs (the Enchanter's Haste, a
    /// Bard's Wanderer's Chord), 0.0 with none: what the client's attack
    /// timer paces by, read from the same buffs the server fans it. The
    /// swing-rate limit reads it so an unhasted player is held to an
    /// unhasted pace.
    pub fn haste_total(&self) -> f32 {
        self.active_buffs
            .iter()
            .filter_map(|b| match b.effect {
                super::buffs::BuffEffect::Haste { amount } => Some(amount),
                _ => None,
            })
            .sum()
    }

    /// Where `Respawn` should put this player: their bind point, or the starter
    /// spawn if they have never bound. Never returns the death position — that
    /// was the old behaviour (respawn restored HP but left position untouched)
    /// and it produced an unwinnable loop for anyone who died beside a mob.
    pub fn respawn_destination(&self) -> Vec3f {
        self.bind
            .map(|b| Vec3f::from_tuple(b.pos))
            .unwrap_or(super::STARTER_SPAWN)
    }

    /// The `item_path` of the weapon the SERVER considers equipped in the hand
    /// this swing uses — main hand (equip slot 0) or off hand (slot 1), per the
    /// `protocol::world::EquipSlot` order. Empty string = that hand is bare (an
    /// unarmed swing). The attack resolver reads this instead of the client's
    /// claimed `weapon_path`, so a modified client can't swing a weapon it hasn't
    /// equipped (Phase 1 exploit gate, audit finding 5). Equipping only happens
    /// through the server-side `EquipItem` intent, so this map is authoritative.
    pub fn equipped_weapon_path(&self, is_offhand: bool) -> String {
        let slot: u8 = if is_offhand { 1 } else { 0 };
        self.inventory
            .equipment
            .get(&slot)
            .map(|e| e.item_path.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The seated-combat rule's stand half: any damage stands a seated player.
    #[test]
    fn taking_damage_stands_a_seated_player() {
        let mut c = PerConnection::from_spawn(test_spawn(), Instant::now());
        c.is_sitting = true;
        c.note_damage_taken(Instant::now());
        assert!(!c.is_sitting, "a hit stands you");
        assert!(c.last_damaged_at.is_some());
    }
    use std::time::Duration;

    /// Minimal spawn for connection-state tests. Field values are arbitrary;
    /// only the lifecycle flags under test matter.
    fn test_spawn() -> crate::db::CharacterSpawn {
        crate::db::CharacterSpawn {
            hunger: crate::world::hunger::FULLNESS_CAP_SECS,
            thirst: crate::world::hunger::FULLNESS_CAP_SECS,
            alignment: None,
            factions: Vec::new(),
            char_id: 1,
            account_id: 1,
            name: "Tester".into(),
            race: "Human".into(),
            class: "Warrior".into(),
            level: 1,
            xp: 0,
            xp_to_next: 100,
            strength: 10,
            dexterity: 10,
            agility: 10,
            intelligence: 10,
            wisdom: 10,
            charisma: 10,
            constitution: 10,
            max_hp: 100.0,
            max_mp: 100.0,
            max_stamina: 100.0,
            hp: 100.0,
            mp: 100.0,
            stamina: 100.0,
            coins: Default::default(),
            bank_coins: Default::default(),
            zone: None,
            pos: (0.0, 0.0, 0.0),
            yaw: 0.0,
            completed_quests: Vec::new(),
            active_quests: Vec::new(),
            bind: None,
        }
    }

    // The attack resolver reads the equipped weapon from the SERVER's equipment
    // map, not the client's claimed weapon_path (Phase 1 exploit gate, finding 5):
    // main hand = slot 0, off hand = slot 1, empty slot = unarmed swing.
    #[test]
    fn equipped_weapon_path_reads_the_server_slot() {
        use super::super::inventory::InventoryEntry;
        let mut conn = PerConnection::from_spawn(test_spawn(), Instant::now());
        // Bare hands: both slots empty -> unarmed (empty path -> fists downstream).
        assert_eq!(conn.equipped_weapon_path(false), "");
        assert_eq!(conn.equipped_weapon_path(true), "");
        // Equip a main-hand and an off-hand weapon.
        conn.inventory.equipment.insert(
            0,
            InventoryEntry { item_path: "res://items/iron_short_sword.tres".into(), count: 1 },
        );
        conn.inventory.equipment.insert(
            1,
            InventoryEntry { item_path: "res://items/rusty_dagger.tres".into(), count: 1 },
        );
        assert_eq!(conn.equipped_weapon_path(false), "res://items/iron_short_sword.tres");
        assert_eq!(conn.equipped_weapon_path(true), "res://items/rusty_dagger.tres");
        // An off-hand swing with an empty off-hand is unarmed, NOT the main-hand
        // weapon — a client can't fabricate an off-hand hit it isn't geared for.
        conn.inventory.equipment.remove(&1);
        assert_eq!(conn.equipped_weapon_path(true), "");
        assert_eq!(conn.equipped_weapon_path(false), "res://items/iron_short_sword.tres");
    }

    // Respawn must never leave the player where they died. An unbound character
    // falls back to the starter spawn; a bound one goes to their bind point.
    #[test]
    fn respawn_destination_prefers_bind_then_starter_spawn() {
        let mut conn = PerConnection::from_spawn(test_spawn(), Instant::now());
        // Simulate having died somewhere dangerous.
        conn.pos = Vec3f { x: 120.0, y: 4.0, z: -80.0 };

        // Unbound -> starter spawn, NOT the death position.
        assert!(conn.bind.is_none(), "fixture starts unbound");
        let d = conn.respawn_destination();
        assert_eq!((d.x, d.y, d.z), (super::super::STARTER_SPAWN.x,
                                     super::super::STARTER_SPAWN.y,
                                     super::super::STARTER_SPAWN.z));
        assert_ne!((d.x, d.z), (conn.pos.x, conn.pos.z), "must not respawn at the death site");

        // Bound -> the bind point.
        conn.bind = Some(crate::db::BindPoint { pos: (10.0, 1.0, -5.0) });
        let d2 = conn.respawn_destination();
        assert_eq!((d2.x, d2.y, d2.z), (10.0, 1.0, -5.0));
    }

    // Dev/GM commands gate on can_use_dev_cmds() = is_dev || is_gm, so a hosted
    // server (PD_DEV_CMDS off) can still grant tools to a GM account only.
    #[test]
    fn can_use_dev_cmds_is_dev_or_gm() {
        let mut conn = PerConnection::from_spawn(test_spawn(), Instant::now());
        // Pin both flags so the assertion doesn't depend on the ambient
        // PD_DEV_CMDS in the test environment.
        conn.is_dev = false;
        conn.is_gm = false;
        assert!(!conn.can_use_dev_cmds(), "neither dev nor GM");
        conn.is_gm = true;
        assert!(conn.can_use_dev_cmds(), "a GM account may use dev commands");
        conn.is_gm = false;
        conn.is_dev = true;
        assert!(conn.can_use_dev_cmds(), "a dev server may use dev commands");
    }

    // The dev-command gate: unauthorized is silent and writes nothing; an
    // authorized command is recorded with every authority in force.
    #[test]
    fn authorize_dev_cmd_records_and_names_the_authority() {
        let now = Instant::now();
        let mut conn = PerConnection::from_spawn(test_spawn(), now);
        conn.is_dev = false;
        conn.is_gm = false;
        assert_eq!(
            conn.authorize_dev_cmd(now, "heal_self", "amount=50".to_string()),
            DevCmdAuth::Denied
        );
        assert!(conn.gm_audit.is_empty(), "a refused attempt must queue no write");
        assert_eq!(conn.gm_cmd_tokens, GM_CMD_BURST, "nor spend any budget");

        conn.is_gm = true;
        assert_eq!(
            conn.authorize_dev_cmd(now, "heal_self", "amount=50".to_string()),
            DevCmdAuth::Allowed
        );
        assert_eq!(
            conn.gm_audit,
            vec![GmAuditEntry { command: "heal_self", args: "amount=50 via=gm".to_string() }]
        );

        // A GM on a dev server must not read as a clean hosted row.
        conn.is_dev = true;
        conn.authorize_dev_cmd(now, "give", "item=\"Bread Loaf\" qty=1".to_string());
        assert!(conn.gm_audit[1].args.ends_with("via=gm+dev"));
        conn.is_gm = false;
        conn.authorize_dev_cmd(now, "give", "x".repeat(5_000));
        assert!(conn.gm_audit[2].args.ends_with("via=dev"));
        // Client-supplied text is cut, not trusted.
        assert!(conn.gm_audit[2].args.len() <= GM_AUDIT_ARGS_MAX + " via=dev".len());
    }

    // The budget: a burst the size of the Test Panel's biggest batch passes
    // whole, a flood is REFUSED (not run unrecorded), and time refills it.
    #[test]
    fn authorize_dev_cmd_refuses_over_budget_instead_of_running_unrecorded() {
        let t0 = Instant::now();
        let mut conn = PerConnection::from_spawn(test_spawn(), t0);
        conn.is_dev = false;
        conn.is_gm = true;

        // The Test Panel's crafting-materials button: ~34 gives in one frame.
        for _ in 0..34 {
            assert_eq!(
                conn.authorize_dev_cmd(t0, "give", "item=x qty=1".to_string()),
                DevCmdAuth::Allowed
            );
        }
        assert_eq!(conn.gm_audit.len(), 34, "every one of them is recorded");

        // Drain the rest of the burst in the same instant, then one more.
        let rest = GM_CMD_BURST as usize - 34;
        for _ in 0..rest {
            assert_eq!(
                conn.authorize_dev_cmd(t0, "heal_self", "amount=0".to_string()),
                DevCmdAuth::Allowed
            );
        }
        let recorded = conn.gm_audit.len();
        assert_eq!(
            conn.authorize_dev_cmd(t0, "give_coins", "p=1000000 g=0 s=0 c=0".to_string()),
            DevCmdAuth::RateLimited { notify: true },
            "the command hidden behind the flood is refused, not granted"
        );
        assert_eq!(conn.gm_audit.len(), recorded, "and nothing was queued for it");
        assert_eq!(conn.gm_cmd_refused, 1);
        assert_eq!(
            conn.authorize_dev_cmd(t0, "heal_self", "amount=0".to_string()),
            DevCmdAuth::RateLimited { notify: false },
            "the notice is at most one a second"
        );

        // One second later the refill has bought GM_CMD_REFILL_PER_SEC more.
        let t1 = t0 + Duration::from_secs(1);
        for _ in 0..GM_CMD_REFILL_PER_SEC as usize {
            assert_eq!(
                conn.authorize_dev_cmd(t1, "heal_self", "amount=0".to_string()),
                DevCmdAuth::Allowed
            );
        }
        assert!(matches!(
            conn.authorize_dev_cmd(t1, "heal_self", "amount=0".to_string()),
            DevCmdAuth::RateLimited { .. }
        ));
    }

    // PD_W0024 — the login-time journal normalization: unknown quest ids
    // drop, an active quest that is somehow also completed drops (crash
    // between the completion record and the active-row delete), short
    // progress vectors pad with zeros, and negative counts clamp to 0. A
    // malformed row must only ever UNDER-credit.
    #[test]
    fn from_spawn_normalizes_the_loaded_journal() {
        let mut spawn = test_spawn();
        spawn.completed_quests = vec!["rat_infestation".into()];
        spawn.active_quests = vec![
            ("wolf_threat".into(), vec![-2]),        // negative -> clamps to 0
            ("rat_infestation".into(), vec![8]),     // also completed -> drops
            ("deleted_old_quest".into(), vec![1]),   // not in quests.toml -> drops
            ("test_q1".into(), Vec::new()),          // short (corrupt row) -> pads to len 1
        ];
        let conn = PerConnection::from_spawn(spawn, Instant::now());
        assert_eq!(conn.active_quests.len(), 2);
        assert_eq!(conn.active_quests.get("wolf_threat"), Some(&vec![0]));
        assert_eq!(conn.active_quests.get("test_q1"), Some(&vec![0]));
        assert!(!conn.active_quests.contains_key("rat_infestation"));
        assert!(!conn.active_quests.contains_key("deleted_old_quest"));
        assert!(conn.quests_dirty.is_empty());
    }

    #[test]
    fn linkdead_expired_false_when_not_linkdead() {
        let now = Instant::now();
        let conn = PerConnection::from_spawn(test_spawn(), now);
        // A live connection (linkdead_since == None) is never reaped, no matter
        // how much later we sweep.
        assert!(!conn.linkdead_expired(now, Duration::from_secs(30)));
        assert!(!conn.linkdead_expired(now + Duration::from_secs(120), Duration::from_secs(30)));
    }

    #[test]
    fn linkdead_expired_respects_window() {
        let base = Instant::now();
        let mut conn = PerConnection::from_spawn(test_spawn(), base);
        conn.linkdead_since = Some(base);
        let window = Duration::from_secs(30);
        // Still inside the window: not yet reapable.
        assert!(!conn.linkdead_expired(base + Duration::from_secs(29), window));
        // At and past the window: reapable.
        assert!(conn.linkdead_expired(base + window, window));
        assert!(conn.linkdead_expired(base + Duration::from_secs(31), window));
    }

    /// The jump relay's rule: a jump is relayed on the flag's rising edge
    /// only (a held flag is one jump, a cleared flag relays nothing), and
    /// not again inside JUMP_RELAY_MIN_INTERVAL however many edges a forged
    /// stream sends; a relayed jump stands the player.
    #[test]
    fn a_jump_relays_on_the_rising_edge_and_not_again_inside_the_interval() {
        let t0 = Instant::now();
        let mut conn = PerConnection::from_spawn(test_spawn(), t0);
        conn.is_sitting = true;
        assert!(!conn.note_move_jump(false, t0), "no flag, no jump");
        assert!(conn.note_move_jump(true, t0), "the rising edge relays");
        assert!(!conn.is_sitting, "a jump stands the player");
        assert!(!conn.note_move_jump(true, t0 + Duration::from_millis(50)), "a held flag is the same jump");
        assert!(!conn.note_move_jump(false, t0 + Duration::from_millis(100)), "clearing it relays nothing");
        assert!(
            !conn.note_move_jump(true, t0 + Duration::from_millis(150)),
            "a second edge inside the interval is dropped"
        );
        let later = t0 + super::super::JUMP_RELAY_MIN_INTERVAL + Duration::from_millis(1);
        assert!(!conn.note_move_jump(false, later), "the flag clears between jumps");
        assert!(conn.note_move_jump(true, later), "past the interval the next edge relays again");
    }
}
