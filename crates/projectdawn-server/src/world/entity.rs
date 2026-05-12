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
    /// Time the entity entered its current state. Useful for corpse
    /// linger and stuck-state diagnostics.
    pub state_entered_at: Instant,
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
