//! Server-side spawn management: the camps' spawn points and the roam
//! table's slots.
//!
//! One `SpawnPoint` per authored `CampSpawn`: it owns the respawn timer for
//! the one enemy that stands at that spot. One `RoamSlot` per wandering
//! animal or pack (`roam.rs`): it owns the timer for its heads, which appear
//! anywhere in its region.
//!
//! The tick loop drives this via [`Spawner::tick`], which:
//!   1. counts down respawn timers on vacant points and short slots
//!   2. instantiates a fresh `Entity` when a timer hits zero
//!   3. returns the newly spawned entities for fan-out
//!
//! Order is preserved from the TOML files, so indices are stable across runs
//! and can be referenced from `Entity::spawn_point_idx` for
//! death-notification. The camps' points come first; roam slot `i` has index
//! `points.len() + i`.

use super::{
    clock,
    connection::Vec3f,
    entity::Entity,
    roam::{self, Roam, RoamDef},
    roam_map::RoamMap,
    zones::{load_camps, CampSpawn},
};
use protocol::world::EntityId;
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
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

/// Runtime state for one roam slot: a wandering animal, or a pack of them.
#[derive(Debug)]
pub struct RoamSlot {
    pub def: RoamDef,
    /// `Some(t)`: the slot may be short of heads and is looked at once
    /// `now >= t`. `None`: every head was present when last looked at; a
    /// death sets it again.
    pub due_at: Option<Instant>,
}

/// What the spawner reads of the world to place wanderers: who is alive (a
/// slot's head count is its living members, charmed ones included), where the
/// players' bodies are (nothing appears on top of one), and the hour (night
/// animals wait for night).
pub struct SpawnWorld<'a> {
    pub enemies: &'a HashMap<EntityId, Entity>,
    /// Every in-world player's position: living, dead or linkdead.
    pub bodies: &'a [Vec3f],
    /// The game hour, `clock::current_hour()` for a running server.
    pub hour: f32,
}

/// Owning collection. Held by the tick loop alongside the enemy map.
#[derive(Debug)]
pub struct Spawner {
    pub points: Vec<SpawnPoint>,
    /// The wanderers' slots; empty when roaming is off (`Config::roamers`).
    pub roams: Vec<RoamSlot>,
    /// Where a wanderer may stand; `None` with no roam slots.
    roam_map: Option<Arc<RoamMap>>,
}

impl Spawner {
    /// The camps alone: nothing roams.
    pub fn new(now: Instant) -> Self {
        let camps = load_camps();
        let points = camps
            .into_iter()
            .map(|c| SpawnPoint::from_camp(c, now))
            .collect();
        Self {
            points,
            roams: Vec::new(),
            roam_map: None,
        }
    }

    /// The camps and a roam table, its wanderers placed on `map`. Every slot
    /// is due at once, like the camps (a night slot then waits for night).
    pub fn with_roamers(now: Instant, defs: Vec<RoamDef>, map: Arc<RoamMap>) -> Self {
        let mut spawner = Self::new(now);
        spawner.roams = defs
            .into_iter()
            .map(|def| RoamSlot {
                def,
                due_at: Some(now),
            })
            .collect();
        spawner.roam_map = Some(map);
        spawner
    }

    /// How many animals the roam table holds when every slot is full.
    pub fn roam_heads(&self) -> usize {
        self.roams.iter().map(|s| s.def.pack).sum()
    }

    /// Drive respawn timers and instantiate any enemies that are due to
    /// spawn this tick, each standing on `terrain`'s ground. Returns the
    /// freshly-spawned entities for the caller to insert into the world's
    /// enemy map and broadcast.
    pub fn tick(
        &mut self,
        now: Instant,
        terrain: &super::terrain::Terrain,
        world: &SpawnWorld,
    ) -> Vec<Entity> {
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
            // The camp's authored y is never read: the ground decides (the
            // height model's decision 4).
            let x = point.camp.pos[0] + jitter_x;
            let z = point.camp.pos[2] + jitter_z;
            // Clear of the camp's own scenery (a tent, a rock, a wall): a
            // camp is drawn where the camp is, so a spawn can fall on its set
            // piece; the mob stands at the nearest free point, and that is
            // its home.
            let spawn_pos = terrain.obstacles().place(terrain, Vec3f { x, y: 0.0, z });
            let entity = Entity::from_spawn(idx, spawn_pos, point.camp.mob.clone(), now);
            point.due_at = None;
            out.push(entity);
        }
        self.tick_roams(now, terrain, world, &mut rng, &mut out);
        out
    }

    /// The roam slots' half of `tick`: each due slot is brought back up to
    /// its head count.
    ///
    /// A slot counts HEADS, not one entity: its living members are every
    /// living enemy carrying its index, charmed ones included. A pack short
    /// of members is refilled beside its leader once its timer is due, rather
    /// than waiting for the last member to die (anyone could hold a pack out
    /// of the world by keeping one member alive). A charmed member holds its
    /// head for the charm's life, as a charmed camp mob holds its point:
    /// otherwise charm one, wait for the refill, let the charm end, and the
    /// slot has one more than it should, every cast.
    fn tick_roams(
        &mut self,
        now: Instant,
        terrain: &super::terrain::Terrain,
        world: &SpawnWorld,
        rng: &mut impl Rng,
        out: &mut Vec<Entity>,
    ) {
        let Some(map) = self.roam_map.clone() else {
            return;
        };
        let base = self.points.len();
        let night = clock::is_night(world.hour);
        for (i, slot) in self.roams.iter_mut().enumerate() {
            let Some(due_at) = slot.due_at else {
                continue;
            };
            if now < due_at {
                continue;
            }
            // A night slot due by day waits for dusk, still due.
            if slot.def.night_only && !night {
                continue;
            }
            let idx = base + i;
            let mut heads = 0;
            // The pack's leader, as `tick_roam` reads it: its living, unowned
            // member with the lowest id.
            let mut leader: Option<(EntityId, Vec3f)> = None;
            for e in world.enemies.values() {
                if e.spawn_point_idx != idx || !e.is_alive() {
                    continue;
                }
                heads += 1;
                if e.owner.is_none() && leader.is_none_or(|(id, _)| e.id < id) {
                    leader = Some((e.id, e.pos));
                }
            }
            let missing = slot.def.pack.saturating_sub(heads);
            if missing == 0 {
                slot.due_at = None;
                continue;
            }
            let retry = Some(now + Duration::from_secs_f32(roam::ROAM_RETRY_SECS));
            let region = &slot.def.region;
            // Beside the leader when there is one (and nobody is near it),
            // else a fresh spot somewhere in the region.
            let anchor = match leader {
                Some((_, pos)) => {
                    roam::clear_of_bodies(pos.x, pos.z, world.bodies).then_some((pos.x, pos.z))
                }
                None => roam::pick_spot(&map, region, world.bodies, rng),
            };
            let Some(anchor) = anchor else {
                slot.due_at = retry;
                continue;
            };
            let mut placed = 0;
            for k in 0..missing {
                // The first of a fresh pack stands on the anchor itself.
                let spot = if leader.is_none() && k == 0 {
                    Some(anchor)
                } else {
                    roam::pick_near(&map, region, anchor, world.bodies, rng)
                };
                let Some((x, z)) = spot else {
                    continue;
                };
                let pos = Vec3f {
                    x,
                    y: terrain.ground(x, z),
                    z,
                };
                let mut entity = Entity::from_spawn(idx, pos, slot.def.mob.clone(), now);
                entity.roam = Some(Roam::of(&slot.def));
                out.push(entity);
                placed += 1;
            }
            // A member that found no spot beside the anchor comes next try.
            slot.due_at = if placed == missing { None } else { retry };
        }
    }

    /// Notify the spawner that an enemy bound to `spawn_point_idx` has
    /// died — start its respawn timer. For a roam slot the timer is rolled
    /// `ROAM_RESPAWN_JITTER` either side of its respawn, and a timer already
    /// running is never pushed back by a second death.
    pub fn on_enemy_died(&mut self, spawn_point_idx: usize, now: Instant) {
        if let Some(point) = self.points.get_mut(spawn_point_idx) {
            point.due_at = Some(now + Duration::from_secs_f32(point.camp.respawn_secs));
            return;
        }
        let base = self.points.len();
        let Some(slot) = spawn_point_idx
            .checked_sub(base)
            .and_then(|i| self.roams.get_mut(i))
        else {
            return;
        };
        let jitter = rand::thread_rng()
            .gen_range(1.0 - roam::ROAM_RESPAWN_JITTER..=1.0 + roam::ROAM_RESPAWN_JITTER);
        let due = now + Duration::from_secs_f32(slot.def.respawn_secs * jitter);
        slot.due_at = Some(slot.due_at.map_or(due, |t| t.min(due)));
    }

    /// A night animal left by day (`run_spawning`'s daylight sweep): its slot
    /// is due at once, which for a night slot means at the next dusk.
    pub fn on_night_animal_left(&mut self, spawn_point_idx: usize, now: Instant) {
        let base = self.points.len();
        if let Some(slot) = spawn_point_idx
            .checked_sub(base)
            .and_then(|i| self.roams.get_mut(i))
        {
            slot.due_at = Some(slot.due_at.map_or(now, |t| t.min(now)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::entity::EnemyState;
    use crate::world::terrain::Terrain;

    /// An empty world at noon.
    fn nobody(enemies: &HashMap<EntityId, Entity>) -> SpawnWorld<'_> {
        SpawnWorld {
            enemies,
            bodies: &[],
            hour: 12.0,
        }
    }

    #[test]
    fn first_tick_spawns_everything() {
        let now = Instant::now();
        let none = HashMap::new();
        let mut sp = Spawner::new(now);
        let spawned = sp.tick(now, super::super::terrain::compiled(), &nobody(&none));
        // 54 spawn points in the starter zone TOML (phase 4 layout,
        // 2026-09-10) → first tick fires all. This must hold regardless of
        // machine uptime: the old backdating implementation failed here for
        // any camp whose respawn_secs exceeded the time since boot.
        assert_eq!(spawned.len(), 54);
        for p in &sp.points {
            assert!(p.due_at.is_none(), "live points must clear due_at");
        }
    }

    /// A camp's spawn that falls on scenery (the camp's own tent or wall)
    /// stands its mob at the nearest free point, and that is the mob's home.
    /// Here a solid covers the whole of the first camp's spawn square.
    #[test]
    fn a_spawn_under_scenery_stands_beside_it() {
        use crate::world::obstacles::{Obstacles, Prism, BODY_RADIUS, P2};
        let camp = &crate::world::zones::load_camps()[0];
        let (x, z, r) = (camp.pos[0], camp.pos[2], camp.radius + 1.0);
        let obstacles =
            Obstacles::from_prisms(&[Prism::rect(x - r, z - r, x + r, z + r, 0.0, 5.0)]).unwrap();
        let ground = super::super::terrain::compiled()
            .clone()
            .with_obstacles(Arc::new(obstacles));
        let now = Instant::now();
        let none = HashMap::new();
        let mut sp = Spawner::new(now);
        let spawned = sp.tick(now, &ground, &nobody(&none));
        let first = spawned
            .iter()
            .find(|e| e.spawn_point_idx == 0)
            .expect("the first camp spawned");
        assert!(
            ground.obstacles().is_free(P2::of(first.pos), BODY_RADIUS),
            "spawned inside the solid at {:?}",
            first.pos
        );
        assert!(
            first.pos.xz_distance_to(first.spawn_pos) < 1e-4,
            "and its home is where it stands"
        );
        assert!(
            (first.pos.x - x).abs().max((first.pos.z - z).abs()) < r + 1.0,
            "beside the solid, not across the zone: {:?}",
            first.pos
        );
    }

    #[test]
    fn second_tick_after_no_deaths_spawns_nothing() {
        let now = Instant::now();
        let none = HashMap::new();
        let mut sp = Spawner::new(now);
        let _ = sp.tick(now, super::super::terrain::compiled(), &nobody(&none));
        let next = now + Duration::from_millis(50);
        let spawned = sp.tick(next, super::super::terrain::compiled(), &nobody(&none));
        assert!(spawned.is_empty());
    }

    #[test]
    fn death_notification_arms_respawn_timer() {
        let now = Instant::now();
        let none = HashMap::new();
        let mut sp = Spawner::new(now);
        let _ = sp.tick(now, super::super::terrain::compiled(), &nobody(&none));
        sp.on_enemy_died(0, now);
        // Before respawn_secs elapses, nothing happens.
        let early = now + Duration::from_secs_f32(1.0);
        assert!(sp
            .tick(early, super::super::terrain::compiled(), &nobody(&none))
            .is_empty());
        // After respawn_secs elapses, the point fires again.
        let respawn = sp.points[0].camp.respawn_secs;
        let late = now + Duration::from_secs_f32(respawn + 0.1);
        let spawned = sp.tick(late, super::super::terrain::compiled(), &nobody(&none));
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].spawn_point_idx, 0);
    }

    // ── Roam slots (wandering wildlife) ──────────────────────────────────

    /// An 800 m flat plate and its roam map.
    fn plate() -> (Terrain, Arc<RoamMap>) {
        let t = Terrain::flat(40, 20.0, 0.0);
        let map = Arc::new(RoamMap::build(&t, (0.0, 0.0)));
        (t, map)
    }

    /// One roam entry: `count` slots of `pack` in a 40 m circle 300 m south
    /// of the town, 10 s respawn.
    fn table(count: usize, pack: usize, night: bool) -> Vec<RoamDef> {
        let time = if night { "time = \"night\"" } else { "" };
        roam::parse(&format!(
            r#"
            [[roam]]
            desc = "test"
            mob = {{ name = "Test Wolf", level = 3, hp = 36.0, dmg = 7, speed = 4.0, aggro = 12.0 }}
            region = {{ center = [0.0, 300.0], radius = 40.0 }}
            count = {count}
            pack = {pack}
            respawn = 10.0
            {time}
        "#
        ))
        .expect("a valid table")
    }

    fn world<'a>(
        enemies: &'a HashMap<EntityId, Entity>,
        bodies: &'a [Vec3f],
        hour: f32,
    ) -> SpawnWorld<'a> {
        SpawnWorld {
            enemies,
            bodies,
            hour,
        }
    }

    fn roamers(spawned: Vec<Entity>) -> Vec<Entity> {
        spawned.into_iter().filter(|e| e.roam.is_some()).collect()
    }

    fn keep(enemies: &mut HashMap<EntityId, Entity>, spawned: Vec<Entity>) {
        for e in spawned {
            enemies.insert(e.id, e);
        }
    }

    /// Past every rolled respawn (10 s, 25% either way).
    const LATER: Duration = Duration::from_secs(13);

    /// The zone's own table on the zone's own ground: the camps still spawn
    /// their 54, every day animal appears on the first tick, the night ones
    /// wait for night, and with roaming off nothing but the camps spawns.
    #[test]
    fn the_zone_fills_its_roam_slots_and_roaming_off_spawns_only_camps() {
        let now = Instant::now();
        let ground = super::super::terrain::compiled();
        let map = Arc::new(RoamMap::build(ground, (0.0, 0.0)));
        let mut enemies = HashMap::new();
        let mut sp = Spawner::with_roamers(now, roam::load(None), map.clone());
        assert_eq!(sp.roam_heads(), 84);
        let mut spawned = sp.tick(now, ground, &world(&enemies, &[], 12.0));
        // A slot whose draws all missed its region's ground tries again a
        // second later; a few tries fill every one.
        for retry in 1..=10 {
            if spawned.len() == 54 + 70 {
                break;
            }
            let later = now + Duration::from_secs_f32(1.1 * retry as f32);
            let mut have: HashMap<EntityId, Entity> = HashMap::new();
            for e in spawned.drain(..) {
                have.insert(e.id, e);
            }
            let more = sp.tick(later, ground, &world(&have, &[], 12.0));
            spawned = have.into_values().chain(more).collect();
        }
        assert_eq!(spawned.len(), 54 + 70, "camps and the day animals");
        for e in spawned.iter().filter(|e| e.roam.is_some()) {
            assert!(e.spawn_point_idx >= 54, "roam slots come after the camps");
            assert!(
                map.roamable(e.pos.x, e.pos.z),
                "{} off the roam map",
                e.mob.name
            );
            let r = e.roam.expect("a wanderer").region;
            assert!(
                r.contains(e.pos.x, e.pos.z),
                "{} outside its region",
                e.mob.name
            );
            assert_eq!(e.pos.y, ground.ground(e.pos.x, e.pos.z), "on the ground");
            assert_eq!(
                e.spawn_pos.distance_to(e.pos),
                0.0,
                "its first home is where it appeared"
            );
        }
        keep(&mut enemies, spawned);
        // Nothing more by day; the night animals at night.
        let t1 = now + Duration::from_secs(2);
        assert!(sp.tick(t1, ground, &world(&enemies, &[], 15.0)).is_empty());
        let night = sp.tick(t1, ground, &world(&enemies, &[], 22.0));
        assert_eq!(night.len(), 14);
        assert!(night.iter().all(|e| e.roam.is_some_and(|r| r.night_only)));

        let mut off = Spawner::new(now);
        assert_eq!(off.roam_heads(), 0);
        assert_eq!(off.tick(now, ground, &nobody(&HashMap::new())).len(), 54);
    }

    /// Where a wanderer appears, over a few hundred respawns: on the roam
    /// map, in its region, outside the town clearance, and never within
    /// `ROAM_POP_CLEARANCE` of a body (so nothing pops onto a fight, and a
    /// camper cannot hold a spawn point). With a body covering the whole
    /// region nothing appears at all, and the slot keeps trying.
    #[test]
    fn a_wanderer_appears_in_its_region_clear_of_every_body() {
        let (ground, map) = plate();
        let mut now = Instant::now();
        let mut sp = Spawner::with_roamers(now, table(1, 1, false), map.clone());
        let slot = sp.points.len();
        let none = HashMap::new();
        // A body 50 m from the region's middle: most of the circle is in its
        // reach, a crescent on the far side is not.
        let bodies = [Vec3f {
            x: 0.0,
            y: 0.0,
            z: 250.0,
        }];
        let mut seen = 0;
        for _ in 0..300 {
            let spawned = roamers(sp.tick(now, &ground, &world(&none, &bodies, 12.0)));
            if let Some(e) = spawned.first() {
                seen += 1;
                assert!(map.roamable(e.pos.x, e.pos.z));
                assert!(e
                    .roam
                    .expect("a wanderer")
                    .region
                    .contains(e.pos.x, e.pos.z));
                assert!(
                    e.pos.xz_distance_to(bodies[0]) >= roam::ROAM_POP_CLEARANCE,
                    "appeared {:.1} m from a body",
                    e.pos.xz_distance_to(bodies[0])
                );
                sp.on_enemy_died(slot, now);
            }
            now += LATER;
        }
        assert!(seen > 200, "most ticks find a spot ({seen})");

        let on_top = [Vec3f {
            x: 0.0,
            y: 0.0,
            z: 300.0,
        }];
        let mut blocked = Spawner::with_roamers(now, table(1, 1, false), map);
        for _ in 0..20 {
            assert!(roamers(blocked.tick(now, &ground, &world(&none, &on_top, 12.0))).is_empty());
            now += LATER;
        }
        assert!(
            !roamers(blocked.tick(now, &ground, &world(&none, &[], 12.0))).is_empty(),
            "it appears once the body has gone"
        );
    }

    /// A pack counts heads. Short of members with its timer due, it is
    /// refilled beside its living leader, not left until the last one dies
    /// (anyone could hold a pack out of the world by keeping one alive); with
    /// a player near the leader the refill waits; a pack with none left
    /// appears whole somewhere new.
    #[test]
    fn a_pack_is_refilled_beside_its_leader_while_one_member_lives() {
        let (ground, map) = plate();
        let now = Instant::now();
        let mut sp = Spawner::with_roamers(now, table(1, 3, false), map);
        let slot = sp.points.len();
        let mut enemies = HashMap::new();
        let pack = roamers(sp.tick(now, &ground, &world(&enemies, &[], 12.0)));
        assert_eq!(pack.len(), 3);
        let first = pack[0].pos;
        for e in &pack {
            assert!(e.roam.expect("a wanderer").pack);
            assert!(e.pos.xz_distance_to(first) <= roam::PACK_SPREAD + 0.01);
        }
        let leader_id = pack.iter().map(|e| e.id).min().expect("three");
        keep(&mut enemies, pack);
        let leader_pos = enemies[&leader_id].pos;

        // Two die (their bodies swept); the leader lives.
        enemies.retain(|id, _| *id == leader_id);
        sp.on_enemy_died(slot, now);
        sp.on_enemy_died(slot, now);
        // Not before the timer.
        let early = now + Duration::from_secs(5);
        assert!(sp
            .tick(early, &ground, &world(&enemies, &[], 12.0))
            .is_empty());
        // Not while a player stands near the leader.
        let late = now + LATER;
        let near = [Vec3f {
            x: leader_pos.x + 20.0,
            ..leader_pos
        }];
        assert!(sp
            .tick(late, &ground, &world(&enemies, &near, 12.0))
            .is_empty());
        // Then both, beside the leader.
        let later = late + Duration::from_secs(2);
        let refill = sp.tick(later, &ground, &world(&enemies, &[], 12.0));
        assert_eq!(refill.len(), 2, "the two missing heads, not a second pack");
        for e in &refill {
            assert_eq!(e.spawn_point_idx, slot);
            assert!(e.pos.xz_distance_to(leader_pos) <= roam::PACK_SPREAD + 0.01);
        }
        keep(&mut enemies, refill);
        assert!(sp.roams[0].due_at.is_none(), "full again");

        // All dead: a whole new pack.
        enemies.clear();
        sp.on_enemy_died(slot, later);
        let fresh = sp.tick(later + LATER, &ground, &world(&enemies, &[], 12.0));
        assert_eq!(fresh.len(), 3);
    }

    /// A charmed member holds its head (it is alive and carries the slot,
    /// whoever owns it). Without that: charm a wolf, wait for the refill, let
    /// the charm end, and the pack has one more than it should, every cast.
    #[test]
    fn a_charmed_member_holds_its_head_so_a_charm_never_overfills_a_slot() {
        let (ground, map) = plate();
        let now = Instant::now();
        let mut sp = Spawner::with_roamers(now, table(1, 3, false), map);
        let slot = sp.points.len();
        let mut enemies = HashMap::new();
        let pack = roamers(sp.tick(now, &ground, &world(&enemies, &[], 12.0)));
        keep(&mut enemies, pack);
        // One is charmed: owned, same slot (as `charm_mob` leaves it).
        let charmed_id = *enemies.keys().max().expect("three");
        enemies.get_mut(&charmed_id).expect("it").owner = Some(7);
        // Something arms the timer (a GM's despawn, a stray death notice).
        sp.on_enemy_died(slot, now);
        let late = now + LATER;
        assert!(
            sp.tick(late, &ground, &world(&enemies, &[], 12.0))
                .is_empty(),
            "three heads are present; the charmed one counts"
        );
        // An unowned member dies while the charm holds: one head is refilled,
        // and with the charmed one the slot holds three, never four.
        let dead = *enemies
            .iter()
            .find(|(_, e)| e.owner.is_none())
            .map(|(id, _)| id)
            .expect("an unowned member");
        enemies.remove(&dead);
        sp.on_enemy_died(slot, late);
        let refill = sp.tick(late + LATER, &ground, &world(&enemies, &[], 12.0));
        assert_eq!(refill.len(), 1);
        keep(&mut enemies, refill);
        let heads = enemies
            .values()
            .filter(|e| e.spawn_point_idx == slot && e.is_alive())
            .count();
        assert_eq!(heads, 3);
        // A member lying dead (not yet swept) is not a head.
        let body = *enemies
            .iter()
            .find(|(_, e)| e.owner.is_none())
            .map(|(id, _)| id)
            .expect("an unowned member");
        enemies
            .get_mut(&body)
            .expect("it")
            .transition(EnemyState::Dead, late);
        sp.on_enemy_died(slot, late + LATER);
        let again = sp.tick(late + LATER + LATER, &ground, &world(&enemies, &[], 12.0));
        assert_eq!(again.len(), 1, "the dead one's place is refilled");
    }

    /// Night slots are alive only at night: one due by day waits for dusk
    /// (still due), a death by night respawns by night, and an animal that
    /// left by day comes back at dusk.
    #[test]
    fn a_night_slot_waits_for_dusk() {
        let (ground, map) = plate();
        let now = Instant::now();
        let mut sp = Spawner::with_roamers(now, table(2, 1, true), map);
        let slot = sp.points.len();
        let none = HashMap::new();
        for hour in [5.0, 12.0, 20.9] {
            assert!(
                roamers(sp.tick(now, &ground, &world(&none, &[], hour))).is_empty(),
                "nothing at {hour}"
            );
        }
        assert_eq!(
            roamers(sp.tick(now, &ground, &world(&none, &[], 21.0))).len(),
            2
        );
        // A death at night: back after its timer, at night only.
        sp.on_enemy_died(slot, now);
        let late = now + LATER;
        assert!(roamers(sp.tick(late, &ground, &world(&none, &[], 6.0))).is_empty());
        assert_eq!(
            roamers(sp.tick(late, &ground, &world(&none, &[], 23.5))).len(),
            1
        );
        // One left at dawn: its slot is due at once, which means at dusk.
        sp.on_night_animal_left(slot + 1, late);
        assert!(roamers(sp.tick(late, &ground, &world(&none, &[], 9.0))).is_empty());
        assert_eq!(
            roamers(sp.tick(late, &ground, &world(&none, &[], 2.0))).len(),
            1
        );
    }

    /// A second death in a slot never pushes a running timer back, a camp
    /// point's death still arms the camp's own timer, and an index past both
    /// tables (a dev spawn's, a summoned pet's) arms nothing.
    #[test]
    fn a_slots_timer_is_rolled_once_and_only_its_own() {
        let (_, map) = plate();
        let now = Instant::now();
        let mut sp = Spawner::with_roamers(now, table(1, 3, false), map);
        let slot = sp.points.len();
        sp.roams[0].due_at = None;
        sp.on_enemy_died(slot, now);
        let first = sp.roams[0].due_at.expect("armed");
        let secs = first.duration_since(now).as_secs_f32();
        assert!(
            (7.5..=12.5).contains(&secs),
            "10 s rolled 25% either way: {secs}"
        );
        sp.on_enemy_died(slot, now + Duration::from_secs(6));
        assert!(sp.roams[0].due_at.expect("armed") <= first);
        sp.points[0].due_at = None;
        sp.on_enemy_died(usize::MAX, now);
        sp.on_enemy_died(slot + 1, now);
        assert!(sp.points[0].due_at.is_none());
        sp.on_enemy_died(0, now);
        assert!(sp.points[0].due_at.is_some());
    }
}
