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
use protocol::world::{EntityId, ENEMY_ID_BASE, PET_ID_BASE};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcKind {
    Mez,
    Root,
    Snare { factor_pct: u8 }, // 0–100; 50 = half speed
    AttackSlow { factor_pct: u8 }, // extra delay fraction × 100
}

#[derive(Debug, Clone)]
pub struct ActiveCc {
    pub kind: CcKind,
    pub remaining: f32,
}

impl ActiveCc {
    pub fn new_mez(duration: f32) -> Self {
        Self { kind: CcKind::Mez, remaining: duration }
    }
    pub fn new_root(duration: f32) -> Self {
        Self { kind: CcKind::Root, remaining: duration }
    }
    pub fn new_snare(slow_amount: f32, duration: f32) -> Self {
        let factor_pct = (slow_amount.clamp(0.0, 1.0) * 100.0) as u8;
        Self { kind: CcKind::Snare { factor_pct }, remaining: duration }
    }
    pub fn new_attack_slow(slow_amount: f32, duration: f32) -> Self {
        let factor_pct = (slow_amount.clamp(0.0, 1.0) * 100.0) as u8;
        Self { kind: CcKind::AttackSlow { factor_pct }, remaining: duration }
    }
}

/// State machine for an `Entity`.
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
    pub aggro: HashMap<EntityId, f32>,

    /// Time of last melee swing. Compared against
    /// `mob.attack_interval` to gate attack firing.
    pub last_attack_at: Option<Instant>,
    /// Time the entity entered its current state. Gates the corpse-
    /// linger window before EntityDespawn fires.
    pub state_entered_at: Instant,

    /// Monotonic sequence for Position broadcasts. Same role as
    /// `PerConnection.last_move_seq` — the client uses it to drop
    /// out-of-order updates on the unreliable channel.
    pub seq: u32,

    /// Active crowd-control effects. Ticked every AI frame; empty is the
    /// common case (no per-tick allocation cost when idle).
    pub active_cc: Vec<ActiveCc>,

    /// Track 11 — player-owned pet. `None` for world-spawned enemies
    /// (the default); `Some(owner_char_id)` for pets summoned via
    /// PET_SUMMON. Owner determines despawn-on-disconnect and (later)
    /// follow-and-attack AI; identity also drives id partition
    /// (>= PET_ID_BASE).
    pub owner: Option<EntityId>,
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
            active_cc: Vec::new(),
            owner: None,
        }
    }

    /// Track 11 — instantiate a player-owned pet. Uses the pet id
    /// partition (`>= PET_ID_BASE`) so the client routes pet-related
    /// broadcasts separately from enemies. `spawn_point_idx` is set
    /// to `usize::MAX` since pets aren't tied to a respawn point;
    /// nothing in the code path that consumes this field runs for
    /// pets (corpse cleanup arms by id partition for the despawn
    /// fan-out instead).
    pub fn from_pet_summon(
        owner: EntityId,
        pos: Vec3f,
        mob: MobTemplate,
        now: Instant,
    ) -> Self {
        let id = mint_pet_id();
        let hp = mob.hp;
        Self {
            id,
            spawn_point_idx: usize::MAX,
            spawn_pos: pos,
            pos,
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
            active_cc: Vec::new(),
            owner: Some(owner),
        }
    }

    pub fn is_pet(&self) -> bool {
        self.owner.is_some()
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
        let base = self.mob.attack_interval.unwrap_or(2.5);
        // Attack slow adds fractional delay on top of the base interval.
        let slow_mult = 1.0 + self.attack_slow_factor();
        base * slow_mult
    }

    /// Tick all active CC durations down by `dt`. Call once per AI tick
    /// before the state machine.
    pub fn tick_cc(&mut self, dt: f32) {
        self.active_cc.retain_mut(|cc| {
            cc.remaining -= dt;
            cc.remaining > 0.0
        });
    }

    /// Apply a CC effect, replacing any existing instance of the same kind
    /// (re-cast refreshes duration rather than stacking).
    pub fn apply_cc(&mut self, cc: ActiveCc) {
        let kind_disc = std::mem::discriminant(&cc.kind);
        self.active_cc.retain(|c| std::mem::discriminant(&c.kind) != kind_disc);
        self.active_cc.push(cc);
    }

    /// Clear all mez effects (called when the enemy takes damage).
    pub fn clear_mez(&mut self) {
        self.active_cc.retain(|c| !matches!(c.kind, CcKind::Mez));
    }

    pub fn is_mezzed(&self) -> bool {
        self.active_cc.iter().any(|c| matches!(c.kind, CcKind::Mez))
    }

    pub fn is_rooted(&self) -> bool {
        self.active_cc.iter().any(|c| matches!(c.kind, CcKind::Root))
    }

    /// Speed multiplier from snare (0.0 = full speed, 0.5 = half speed).
    fn snare_factor(&self) -> f32 {
        self.active_cc
            .iter()
            .filter_map(|c| {
                if let CcKind::Snare { factor_pct } = c.kind {
                    Some(factor_pct as f32 / 100.0)
                } else {
                    None
                }
            })
            .fold(0.0_f32, f32::max)
    }

    /// Extra attack-interval fraction from attack-slow effects.
    fn attack_slow_factor(&self) -> f32 {
        self.active_cc
            .iter()
            .filter_map(|c| {
                if let CcKind::AttackSlow { factor_pct } = c.kind {
                    Some(factor_pct as f32 / 100.0)
                } else {
                    None
                }
            })
            .fold(0.0_f32, f32::max)
    }

    /// Drive one AI tick. Mutates state / target / pos / last_attack_at;
    /// returns the events the tick loop should fan out (target switch,
    /// melee swing, position broadcast trigger). The caller is responsible
    /// for owning `targets` (a snapshot of aggro-able entity positions —
    /// players plus alive pets — for this tick; we don't borrow connections
    /// across the entity loop) and `enemy_targets` (pet targets — alive
    /// non-pet enemies, used by pet AI to chase its inherited target).
    ///
    /// Caster kiting and healer flee are not wired here; the starter zone
    /// has no caster or healer mobs (all 9 archetypes are melee). When
    /// those land, branch on `mob.spell_damage > 0` / `mob.healer_flee_hp
    /// > 0.0` here.
    pub fn tick_ai(
        &mut self,
        targets: &[(EntityId, Vec3f)],
        enemy_targets: &[(EntityId, Vec3f, bool)],
        dt: f32,
        now: Instant,
    ) -> AiEvents {
        self.tick_cc(dt);
        let prev_target = self.target;
        let prev_pos = self.pos;
        let mut events = AiEvents::default();
        // Mez skips the entire state machine (mob stands frozen).
        if self.is_mezzed() {
            return events;
        }
        // Track 11 — pets run a distinct state machine. They follow
        // their owner by default and inherit attack targets from the
        // owner's last melee/spell hit (set by the tick loop before
        // this AI pass runs).
        if self.is_pet() {
            self.tick_pet_ai(targets, enemy_targets, dt, now, &mut events);
        } else {
            match self.state {
                EnemyState::Idle => self.tick_idle(targets, now),
                EnemyState::Chase => self.tick_chase(targets, dt, now),
                EnemyState::Attack => self.tick_attack(targets, now, &mut events),
                EnemyState::Leash => self.tick_leash(dt, now),
                EnemyState::Dead => {}
            }
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

    /// Track 11 — pet AI. Follow owner unless owner has acquired an
    /// enemy target recently (set externally by the tick loop into
    /// `self.target`); in that case, chase + melee the target until
    /// it dies or moves out of leash.
    ///
    /// The pet's `spawn_pos` is reused as a "home" reference but
    /// follow doesn't leash to it (pets follow their owner across
    /// the world). Leash transitions are skipped entirely; pets only
    /// despawn when their owner disconnects (handled in the
    /// transport disconnect arm).
    fn tick_pet_ai(
        &mut self,
        targets: &[(EntityId, Vec3f)],
        enemy_targets: &[(EntityId, Vec3f, bool)],
        dt: f32,
        now: Instant,
        events: &mut AiEvents,
    ) {
        const PET_FOLLOW_DISTANCE: f32 = 3.0;
        let Some(owner_id) = self.owner else {
            // Pet with no owner — pathological state, do nothing.
            return;
        };
        let owner_pos_opt = target_pos(targets, owner_id);
        let target_info: Option<(Vec3f, bool)> = self.target.and_then(|tid| {
            enemy_targets
                .iter()
                .find(|(id, _, _)| *id == tid)
                .map(|(_, pos, alive)| (*pos, *alive))
        });
        // If we have a live target, engage it; otherwise follow owner.
        match target_info {
            Some((target_pos, true)) => {
                let dist = self.pos.distance_to(target_pos);
                let melee = self.melee_range();
                if dist <= melee {
                    // In melee range — swing on cadence.
                    self.face_toward(target_pos);
                    let due = match self.last_attack_at {
                        None => true,
                        Some(t) => {
                            now.duration_since(t).as_secs_f32() >= self.attack_interval()
                        }
                    };
                    if due {
                        self.last_attack_at = Some(now);
                        events.hit = Some(HitIntent {
                            target: self.target.expect("had target_info"),
                            amount: self.mob.dmg,
                        });
                    }
                } else if !self.is_rooted() {
                    let snare = self.snare_factor();
                    let step = self.mob.speed * (1.0 - snare) * dt;
                    self.face_toward(target_pos);
                    self.pos = self.pos.step_toward(target_pos, step);
                }
            }
            _ => {
                // Drop dead/missing target then fall back to follow.
                if target_info.is_some() {
                    self.target = None;
                }
                let Some(owner_pos) = owner_pos_opt else {
                    // Owner offline — stand still (cleanup runs on
                    // disconnect, so this is short-lived).
                    return;
                };
                let dist = self.pos.distance_to(owner_pos);
                if dist > PET_FOLLOW_DISTANCE && !self.is_rooted() {
                    let snare = self.snare_factor();
                    let step = self.mob.speed * (1.0 - snare) * dt;
                    self.face_toward(owner_pos);
                    self.pos = self.pos.step_toward(owner_pos, step);
                }
            }
        }
    }

    fn tick_idle(&mut self, targets: &[(EntityId, Vec3f)], now: Instant) {
        if let Some((id, _)) = nearest_target_within(self.pos, targets, self.mob.aggro) {
            self.target = Some(id);
            self.transition(EnemyState::Chase, now);
        }
    }

    fn tick_chase(&mut self, targets: &[(EntityId, Vec3f)], dt: f32, now: Instant) {
        let Some(target_id) = self.target else {
            self.transition(EnemyState::Leash, now);
            return;
        };
        let Some(target_pos) = target_pos(targets, target_id) else {
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
        // Root and snare block / slow movement.
        if self.is_rooted() {
            return;
        }
        let snare = self.snare_factor();
        let step = self.mob.speed * (1.0 - snare) * dt;
        self.face_toward(target_pos);
        self.pos = self.pos.step_toward(target_pos, step);
    }

    fn tick_attack(
        &mut self,
        targets: &[(EntityId, Vec3f)],
        now: Instant,
        events: &mut AiEvents,
    ) {
        let Some(target_id) = self.target else {
            self.transition(EnemyState::Leash, now);
            return;
        };
        let Some(target_pos) = target_pos(targets, target_id) else {
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

fn nearest_target_within(
    origin: Vec3f,
    targets: &[(EntityId, Vec3f)],
    radius: f32,
) -> Option<(EntityId, f32)> {
    let mut best: Option<(EntityId, f32)> = None;
    for &(id, pos) in targets {
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

fn target_pos(targets: &[(EntityId, Vec3f)], id: EntityId) -> Option<Vec3f> {
    targets.iter().find(|(p, _)| *p == id).map(|(_, pos)| *pos)
}

/// Monotonic enemy-id counter. Starts at `ENEMY_ID_BASE` and increments
/// for each spawn — never reused even after death, so a stale client
/// reference can be detected unambiguously. u64 wraparound at 2^64 is
/// not a practical concern.
static NEXT_ENEMY_ID: AtomicU64 = AtomicU64::new(ENEMY_ID_BASE);

pub fn mint_enemy_id() -> EntityId {
    NEXT_ENEMY_ID.fetch_add(1, Ordering::Relaxed)
}

/// Track 11 — monotonic pet-id counter, partitioned above bags. Same
/// never-reused semantics as enemy ids.
static NEXT_PET_ID: AtomicU64 = AtomicU64::new(PET_ID_BASE);

pub fn mint_pet_id() -> EntityId {
    NEXT_PET_ID.fetch_add(1, Ordering::Relaxed)
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

    /// Track 11.4 — enemy targets pets as valid aggro candidates.
    /// When the targets slice contains only a pet id (no player), the
    /// enemy's tick_idle should still pick it up and transition to
    /// Chase. Verifies the rename from `players` to `targets` actually
    /// extended the aggro pool to pets.
    #[test]
    fn enemy_aggros_on_pet_when_no_player_nearby() {
        let now = Instant::now();
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        // Pet id sits in the dedicated partition; the value doesn't
        // matter for aggro logic (tick_idle picks by distance, not id).
        let pet_id = PET_ID_BASE + 17;
        let targets = vec![(pet_id, Vec3f { x: 2.0, y: 0.0, z: 0.0 })];
        let enemy_targets: Vec<(EntityId, Vec3f, bool)> = vec![];
        let _events = e.tick_ai(&targets, &enemy_targets, 0.05, now);
        assert_eq!(e.target, Some(pet_id), "enemy should aggro on nearest target regardless of id partition");
        assert!(matches!(e.state, EnemyState::Chase), "transitioning Idle → Chase on acquisition");
    }
}
