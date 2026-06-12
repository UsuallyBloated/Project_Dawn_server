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
    /// Set after the application-layer Connect handshake completes. Until
    /// then we won't broadcast positions to this client.
    pub ready: bool,
    /// Set after the client sends `EnterWorld` (i.e. left the lobby). Gates
    /// EntitySpawn fan-out so peers don't see ghost bodies for clients
    /// still on the Enter World screen.
    pub in_world: bool,
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

    /// True when the server was started with `PD_DEV_CMDS=1`. Gates
    /// `HealSelf` / `DamageSelf` so non-dev clients can't use them.
    /// Future: wire to a DB `is_gm` flag from the auth token.
    pub is_dev: bool,

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

    /// Track 17.2 — per-spell cooldown map. CastSpell rejects a cast
    /// whose entry is in the future; a successful cast writes the
    /// next-ready instant from `spells.toml::cooldown`. Spell-name
    /// keyed (matches `cast_spell_name`); cleared on disconnect via
    /// `connections.remove(&cid)`.
    pub spell_cooldowns: HashMap<String, Instant>,

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
            last_persisted_xp: spawn.xp,
            last_persisted_level: spawn.level,
            ready: false,
            in_world: false,
            last_move_seq: 0,
            latest_direction: Vec3f::ZERO,
            last_move_received: None,
            hp: spawn.hp,
            max_hp: spawn.max_hp,
            mp: spawn.mp,
            max_mp: spawn.max_mp,
            stamina: spawn.stamina,
            max_stamina: spawn.max_stamina,
            xp: spawn.xp,
            xp_to_next: spawn.xp_to_next,
            coins: spawn.coins,
            strength: spawn.strength,
            dexterity: spawn.dexterity,
            agility: spawn.agility,
            intelligence: spawn.intelligence,
            wisdom: spawn.wisdom,
            charisma: spawn.charisma,
            constitution: spawn.constitution,
            is_sitting: false,
            equipped_armor: 0,
            pvp_override_on: false,
            is_dev: dev_cmds_enabled(),
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
            spell_cooldowns: HashMap::new(),
            buff_snapshot: Vec::new(),
            buff_snapshot_set: false,
            active_buffs: Vec::new(),
            last_attacked_enemy: None,
            last_attacked_at: None,
            current_target: None,
            warder_respawn_at: None,
            inventory: super::inventory::PlayerInventory::new(),
            inventory_dirty: false,
            equip_stat_bonuses: super::inventory::EquipStatBonuses::default(),
            weapon_skills: HashMap::new(),
            armor_skills: HashMap::new(),
            casting_skills: HashMap::new(),
            skills_dirty: false,
        }
    }

    /// True if the connection has gone silent for at least
    /// [`super::HEARTBEAT_TIMEOUT`] of wall-clock time.
    pub fn is_app_idle(&self, now: Instant) -> bool {
        now.duration_since(self.last_packet) >= super::HEARTBEAT_TIMEOUT
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
    }

    pub fn mark_resources_persisted(&mut self) {
        self.last_persisted_hp = self.hp;
        self.last_persisted_mp = self.mp;
        self.last_persisted_stamina = self.stamina;
        self.last_persisted_xp = self.xp;
        self.last_persisted_level = self.level;
    }
}
