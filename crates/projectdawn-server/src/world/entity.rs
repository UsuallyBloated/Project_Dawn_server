//! Server-authoritative enemy entity. Owns its position, HP, AI state,
//! and aggro table. The tick loop holds these in
//! `HashMap<EntityId, Entity>` keyed by the server-minted enemy id
//! (partition starts at `protocol::world::ENEMY_ID_BASE`).
//!
//! The state machine mirrors the GDScript `enemy.gd` semantics —
//! Idle / Chase / Attack / Leash / Dead, plus the timers that gate
//! transitions — but with no Godot dependency. Client-side `enemy.gd`
//! remains the legacy single-player Test Room implementation; the
//! launcher-mode client will be a render-only consumer (sub-task 2).

use super::{connection::Vec3f, zones::MobTemplate};
use protocol::world::{EntityId, ENEMY_ID_BASE};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// State machine for an `Entity`. `Dead` is reserved for sub-task 3 (the
/// player-attacks-enemy path lands enemy HP and the death lifecycle); 1C
/// only drives Idle / Chase / Attack / Leash.
#[allow(dead_code)] // Dead variant lands in sub-task 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnemyState {
    /// Standing at spawn; scanning for aggro targets within `aggro_range`.
    Idle,
    /// Locked target; moving toward it. Transitions to `Attack` when in
    /// melee range, `Leash` when target leaves `leash_range`.
    Chase,
    /// In melee range of target; firing on `attack_interval`. Returns
    /// to `Chase` if target steps out.
    Attack,
    /// Returning to spawn after losing aggro. Heals on arrival.
    Leash,
    /// HP hit zero. Holds at the death position until the corpse linger
    /// expires, then `EntityDespawn` is broadcast and the entity is
    /// removed from the world map.
    Dead,
}

#[derive(Debug)]
pub struct Entity {
    pub id: EntityId,
    /// Index into the world's `spawn_points` vec. Used so the spawn
    /// point can be notified on death and start its respawn timer.
    #[allow(dead_code)] // wired in sub-task 3 (death → respawn timer)
    pub spawn_point_idx: usize,
    /// Authored archetype data (display name, base stats, speeds). Cloned
    /// from the spawn point on instantiation so resists / overrides are
    /// per-entity if a future zone needs them.
    pub mob: MobTemplate,
    /// Spawn position (canonical, no jitter applied) — leash target.
    pub spawn_pos: Vec3f,
    pub pos: Vec3f,
    pub yaw: f32,
    pub hp: f32,
    pub max_hp: f32,

    pub state: EnemyState,
    /// Current aggro target. `None` outside Chase/Attack.
    pub target: Option<EntityId>,
    /// Aggro table — accumulated damage per attacker, used on target
    /// switch evaluation. Cleared on death.
    #[allow(dead_code)] // populated by sub-task 3 when players damage enemies
    pub aggro: HashMap<EntityId, f32>,

    /// Time of last melee swing. Compared against
    /// `mob.attack_interval` to gate attack firing.
    pub last_attack_at: Option<Instant>,
    /// Time the entity entered its current state. Useful for corpse
    /// linger and stuck-state diagnostics.
    #[allow(dead_code)] // wired in sub-task 3 (corpse linger window)
    pub state_entered_at: Instant,

    /// Monotonic sequence for Position broadcasts. Same role as
    /// `PerConnection.last_move_seq` — the client uses it to drop
    /// out-of-order updates on the unreliable channel.
    pub seq: u32,
}

/// Outcome of one AI tick. Carries the events the tick loop needs to
/// fan out after the per-entity mutation pass.
#[derive(Debug, Default)]
pub struct AiEvents {
    /// `Some(new_target)` when this tick changed the entity's target
    /// (which may be `Some(id)` for a fresh acquisition or `None` for a
    /// drop). `None` (outer) means no change this tick.
    pub target_changed: Option<Option<EntityId>>,
    /// Filled when the entity's Attack state fired a melee swing this
    /// tick.
    pub hit: Option<HitIntent>,
    /// True if the entity's position moved this tick — gates Position
    /// fan-out so idle / attacking enemies don't waste bandwidth.
    pub moved: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct HitIntent {
    pub target: EntityId,
    pub amount: i32,
}

impl Entity {
    pub fn from_spawn(
        spawn_point_idx: usize,
        spawn_pos: Vec3f,
        mob: MobTemplate,
        now: Instant,
    ) -> Self {
        let id = mint_enemy_id();
        let hp = mob.hp;
        Self {
            id,
            spawn_point_idx,
            spawn_pos,
            pos: spawn_pos,
            yaw: 0.0,
            hp,
            max_hp: hp,
            state: EnemyState::Idle,
            target: None,
            aggro: HashMap::new(),
            last_attack_at: None,
            state_entered_at: now,
            mob,
            seq: 0,
        }
    }

    pub fn is_alive(&self) -> bool {
        !matches!(self.state, EnemyState::Dead)
    }

    pub fn transition(&mut self, new_state: EnemyState, now: Instant) {
        if self.state == new_state {
            return;
        }
        self.state = new_state;
        self.state_entered_at = now;
    }

    pub fn leash_range(&self) -> f32 {
        self.mob.leash.unwrap_or(self.mob.aggro * 2.0)
    }

    pub fn melee_range(&self) -> f32 {
        self.mob.melee_range.unwrap_or(1.8)
    }

    pub fn attack_interval(&self) -> f32 {
        self.mob.attack_interval.unwrap_or(2.5)
    }

    /// Drive one AI tick. Mutates state / target / pos / last_attack_at;
    /// returns the events the tick loop should fan out (target switch,
    /// melee swing, position broadcast trigger). The caller is responsible
    /// for owning `players` (a snapshot of in-world player positions for
    /// this tick — we don't borrow connections across the entity loop).
    ///
    /// Caster kiting and healer flee are not wired here; the starter zone
    /// has no caster or healer mobs (all 9 archetypes are melee). When
    /// those land, branch on `mob.spell_damage > 0` / `mob.healer_flee_hp
    /// > 0.0` here.
    pub fn tick_ai(
        &mut self,
        players: &[(EntityId, Vec3f)],
        dt: f32,
        now: Instant,
    ) -> AiEvents {
        let prev_target = self.target;
        let prev_pos = self.pos;
        let mut events = AiEvents::default();
        match self.state {
            EnemyState::Idle => self.tick_idle(players, now),
            EnemyState::Chase => self.tick_chase(players, dt, now),
            EnemyState::Attack => self.tick_attack(players, now, &mut events),
            EnemyState::Leash => self.tick_leash(dt, now),
            EnemyState::Dead => {}
        }
        if self.target != prev_target {
            events.target_changed = Some(self.target);
        }
        // ~0.005m squared threshold — float comparison would falsely flag
        // a stationary entity as "moved" due to step_toward's epsilon snap.
        if self.pos.sub(prev_pos).length() > 0.005 {
            events.moved = true;
        }
        events
    }

    fn tick_idle(&mut self, players: &[(EntityId, Vec3f)], now: Instant) {
        if let Some((id, _)) = nearest_player_within(self.pos, players, self.mob.aggro) {
            self.target = Some(id);
            self.transition(EnemyState::Chase, now);
        }
    }

    fn tick_chase(&mut self, players: &[(EntityId, Vec3f)], dt: f32, now: Instant) {
        let Some(target_id) = self.target else {
            self.transition(EnemyState::Leash, now);
            return;
        };
        let Some(target_pos) = player_pos(players, target_id) else {
            // Target left the world (disconnect, EnterWorld → out). Drop.
            self.target = None;
            self.transition(EnemyState::Leash, now);
            return;
        };
        let dist = self.pos.distance_to(target_pos);
        if dist > self.leash_range() {
            self.target = None;
            self.transition(EnemyState::Leash, now);
            return;
        }
        if dist <= self.melee_range() {
            self.transition(EnemyState::Attack, now);
            return;
        }
        let step = self.mob.speed * dt;
        self.face_toward(target_pos);
        self.pos = self.pos.step_toward(target_pos, step);
    }

    fn tick_attack(
        &mut self,
        players: &[(EntityId, Vec3f)],
        now: Instant,
        events: &mut AiEvents,
    ) {
        let Some(target_id) = self.target else {
            self.transition(EnemyState::Leash, now);
            return;
        };
        let Some(target_pos) = player_pos(players, target_id) else {
            self.target = None;
            self.transition(EnemyState::Leash, now);
            return;
        };
        let dist = self.pos.distance_to(target_pos);
        // Tolerance multiplier matches the GDScript `melee_range * 1.2`
        // hysteresis: avoids flapping Chase↔Attack at the boundary.
        if dist > self.melee_range() * 1.2 {
            self.transition(EnemyState::Chase, now);
            return;
        }
        self.face_toward(target_pos);
        let due = match self.last_attack_at {
            None => true,
            Some(t) => now.duration_since(t).as_secs_f32() >= self.attack_interval(),
        };
        if due {
            self.last_attack_at = Some(now);
            events.hit = Some(HitIntent {
                target: target_id,
                amount: self.mob.dmg,
            });
        }
    }

    fn tick_leash(&mut self, dt: f32, now: Instant) {
        const LEASH_HOME_TOLERANCE: f32 = 1.0;
        let dist = self.pos.distance_to(self.spawn_pos);
        if dist < LEASH_HOME_TOLERANCE {
            self.hp = self.max_hp;
            self.transition(EnemyState::Idle, now);
            return;
        }
        let step = self.mob.speed * dt;
        self.face_toward(self.spawn_pos);
        self.pos = self.pos.step_toward(self.spawn_pos, step);
    }

    fn face_toward(&mut self, target_pos: Vec3f) {
        let dx = target_pos.x - self.pos.x;
        let dz = target_pos.z - self.pos.z;
        // Skip nearly-coincident points to avoid yaw flutter at the threshold.
        if dx * dx + dz * dz < 0.0001 {
            return;
        }
        self.yaw = dx.atan2(dz);
    }
}

fn nearest_player_within(
    origin: Vec3f,
    players: &[(EntityId, Vec3f)],
    radius: f32,
) -> Option<(EntityId, f32)> {
    let mut best: Option<(EntityId, f32)> = None;
    for &(id, pos) in players {
        let d = origin.distance_to(pos);
        if d > radius {
            continue;
        }
        match best {
            None => best = Some((id, d)),
            Some((_, b)) if d < b => best = Some((id, d)),
            _ => {}
        }
    }
    best
}

fn player_pos(players: &[(EntityId, Vec3f)], id: EntityId) -> Option<Vec3f> {
    players.iter().find(|(p, _)| *p == id).map(|(_, pos)| *pos)
}

/// Monotonic enemy-id counter. Starts at `ENEMY_ID_BASE` and increments
/// for each spawn — never reused even after death, so a stale client
/// reference can be detected unambiguously. u64 wraparound at 2^64 is
/// not a practical concern.
static NEXT_ENEMY_ID: AtomicU64 = AtomicU64::new(ENEMY_ID_BASE);

pub fn mint_enemy_id() -> EntityId {
    NEXT_ENEMY_ID.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template() -> MobTemplate {
        MobTemplate {
            name: "Test".into(),
            level: 1,
            hp: 50.0,
            dmg: 5,
            xp: 10,
            speed: 2.5,
            aggro: 10.0,
            leash: None,
            melee_range: None,
            attack_interval: None,
        }
    }

    #[test]
    fn minted_ids_are_partitioned_above_player_range() {
        let a = mint_enemy_id();
        let b = mint_enemy_id();
        assert!(a >= ENEMY_ID_BASE);
        assert!(b > a);
    }

    #[test]
    fn transition_updates_state_and_timestamp() {
        let now = Instant::now();
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        assert_eq!(e.state, EnemyState::Idle);
        let later = now + std::time::Duration::from_millis(100);
        e.transition(EnemyState::Chase, later);
        assert_eq!(e.state, EnemyState::Chase);
        assert_eq!(e.state_entered_at, later);
    }

    #[test]
    fn transition_to_same_state_is_a_noop() {
        let now = Instant::now();
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        let later = now + std::time::Duration::from_millis(100);
        e.transition(EnemyState::Idle, later);
        assert_eq!(e.state_entered_at, now, "state_entered_at must not refresh");
    }
}
