//! Server-side spawn-point management. One `SpawnPoint` per authored
//! `CampSpawn`; it owns the respawn timer and the optional reference
//! to its currently-alive enemy.
//!
//! The tick loop drives this via [`tick`], which:
//!   1. counts down respawn timers on idle points
//!   2. instantiates a fresh `Entity` when a timer hits zero
//!   3. returns the newly spawned entities for fan-out
//!
//! Spawn-point order is preserved from the TOML, so spawn-point
//! indices are stable across runs and can be referenced from `Entity`
//! for death-notification.

use super::{
    connection::Vec3f,
    entity::Entity,
    zones::{load_camps, CampSpawn},
};
use rand::Rng;
use std::time::{Duration, Instant};

/// Runtime state for one authored spawn position.
#[derive(Debug)]
pub struct SpawnPoint {
    pub camp: CampSpawn,
    /// `None` while a live enemy occupies this point; `Some(t)` when the
    /// point is vacant and should mint a fresh entity once `now >= t`.
    pub due_at: Option<Instant>,
}

impl SpawnPoint {
    fn from_camp(camp: CampSpawn, now: Instant) -> Self {
        // First spawn fires on the very next tick — no initial wait, mirror
        // the GDScript `EnemySpawner._ready -> call_deferred("_spawn")`
        // behaviour. The due time is stored DIRECTLY rather than backdating
        // a death timestamp by `respawn_secs`: `now - respawn_secs` can
        // underflow the Instant epoch on a freshly booted machine (Instant
        // counts from boot on Windows and from an arbitrary recent epoch
        // elsewhere), and the old backdating's fallback silently delayed a
        // long-respawn camp by its full timer — a 600 s named den would
        // take ten minutes to appear after every boot-time server start,
        // which is exactly how the R720 starts (systemd, seconds after
        // boot).
        Self {
            camp,
            due_at: Some(now),
        }
    }
}

/// Owning collection. Held by the tick loop alongside the enemy map.
#[derive(Debug)]
pub struct Spawner {
    pub points: Vec<SpawnPoint>,
}

impl Spawner {
    pub fn new(now: Instant) -> Self {
        let camps = load_camps();
        let points = camps
            .into_iter()
            .map(|c| SpawnPoint::from_camp(c, now))
            .collect();
        Self { points }
    }

    /// Drive respawn timers and instantiate any enemies that are due to
    /// spawn this tick. Returns the freshly-spawned entities for the
    /// caller to insert into the world's enemy map and broadcast.
    pub fn tick(&mut self, now: Instant) -> Vec<Entity> {
        let mut out = Vec::new();
        let mut rng = rand::thread_rng();
        for (idx, point) in self.points.iter_mut().enumerate() {
            let Some(due_at) = point.due_at else {
                continue;
            };
            if now < due_at {
                continue;
            }
            // XZ jitter inside the camp's radius. Matches the GDScript
            // EnemySpawner's `randf_range(-spawn_radius, spawn_radius)`
            // applied to x and z.
            let r = point.camp.radius;
            let jitter_x: f32 = rng.gen_range(-r..=r);
            let jitter_z: f32 = rng.gen_range(-r..=r);
            let spawn_pos = Vec3f {
                x: point.camp.pos[0] + jitter_x,
                y: point.camp.pos[1],
                z: point.camp.pos[2] + jitter_z,
            };
            let entity = Entity::from_spawn(idx, spawn_pos, point.camp.mob.clone(), now);
            point.due_at = None;
            out.push(entity);
        }
        out
    }

    /// Notify the spawner that an enemy bound to `spawn_point_idx` has
    /// died — start its respawn timer.
    pub fn on_enemy_died(&mut self, spawn_point_idx: usize, now: Instant) {
        if let Some(point) = self.points.get_mut(spawn_point_idx) {
            point.due_at = Some(now + Duration::from_secs_f32(point.camp.respawn_secs));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_tick_spawns_everything() {
        let now = Instant::now();
        let mut sp = Spawner::new(now);
        let spawned = sp.tick(now);
        // 54 spawn points in the starter zone TOML (phase 4 layout,
        // 2026-09-10) → first tick fires all. This must hold regardless of
        // machine uptime: the old backdating implementation failed here for
        // any camp whose respawn_secs exceeded the time since boot.
        assert_eq!(spawned.len(), 54);
        for p in &sp.points {
            assert!(p.due_at.is_none(), "live points must clear due_at");
        }
    }

    #[test]
    fn second_tick_after_no_deaths_spawns_nothing() {
        let now = Instant::now();
        let mut sp = Spawner::new(now);
        let _ = sp.tick(now);
        let next = now + Duration::from_millis(50);
        let spawned = sp.tick(next);
        assert!(spawned.is_empty());
    }

    #[test]
    fn death_notification_arms_respawn_timer() {
        let now = Instant::now();
        let mut sp = Spawner::new(now);
        let _ = sp.tick(now);
        sp.on_enemy_died(0, now);
        // Before respawn_secs elapses, nothing happens.
        let early = now + Duration::from_secs_f32(1.0);
        assert!(sp.tick(early).is_empty());
        // After respawn_secs elapses, the point fires again.
        let respawn = sp.points[0].camp.respawn_secs;
        let late = now + Duration::from_secs_f32(respawn + 0.1);
        let spawned = sp.tick(late);
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].spawn_point_idx, 0);
    }
}
