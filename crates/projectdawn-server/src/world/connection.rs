//! Per-connection state held by the tick loop. One entry per connected
//! `client_id` (which equals the renet `ConnectToken.client_id` we minted,
//! which equals the player's `char_id`).

use std::time::Instant;

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
    pub coins: i64,

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

    /// Track 4 sub-task 3 — last buff snapshot the client broadcast.
    /// Used to seed new joiners; live updates fan out via the
    /// BuffSnapshotFanOut outcome. Empty Vec = "no active buffs".
    pub buff_snapshot: Vec<(String, f32)>,
    pub buff_snapshot_set: bool,
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
            buff_snapshot: Vec::new(),
            buff_snapshot_set: false,
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
