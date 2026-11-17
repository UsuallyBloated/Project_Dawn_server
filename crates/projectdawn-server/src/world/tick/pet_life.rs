//! A pet's life outside its commands: summoning one scaled to its owner, who owns it, and what happens to owned pets and charmed mobs when the owner leaves or the charm ends.

use super::*;

/// Owner-derived summon wrapper (pet interim A): resolves the scaled
/// template for `pet_type` at the owner's level, applies the standard
/// +1.5 m east spawn offset, and delegates to `summon_pet_for_owner`.
/// Returns `(pet_id, pet_level)`, or `None` for an unknown `pet_type`.
/// The auto-summon, retreat-respawn and manual-summon paths all route
/// through here, so the next pet rule change happens ONCE instead of in
/// three near-identical blocks.
#[allow(clippy::too_many_arguments)]
pub(super) fn summon_scaled_pet(
    server: &mut RenetServer,
    in_world_recipients: &[ClientId],
    enemies: &mut HashMap<EntityId, Entity>,
    aoi: &mut AoiGrid,
    owner_id: EntityId,
    owner_level: u32,
    owner_pos: Vec3f,
    pet_type: &str,
    variance: u32,
    hp_fraction: f32,
    now: Instant,
    terrain: &crate::world::terrain::Terrain,
) -> Option<(EntityId, u32)> {
    let template = pet_templates::scaled(pet_type, owner_level, variance)?;
    let pet_level = template.level;
    // Spawn 1.5 m east of the owner so the pet doesn't clip the player
    // capsule, standing on the ground there (the height model). The 1.5 m is
    // walked from the owner, not jumped to: with a wall at the owner's
    // shoulder the pet appears on the owner's side of it, never beyond.
    let beside = terrain.obstacles().resolve(
        crate::world::obstacles::P2::of(owner_pos),
        crate::world::obstacles::P2::new(owner_pos.x + 1.5, owner_pos.z),
        crate::world::obstacles::BODY_RADIUS,
    );
    let spawn_pos = terrain.ground_vec(Vec3f {
        x: beside.x,
        y: owner_pos.y,
        z: beside.z,
    });
    let pet_id = summon_pet_for_owner(
        server,
        in_world_recipients,
        enemies,
        aoi,
        owner_id,
        spawn_pos,
        template,
        hp_fraction,
        now,
    );
    Some((pet_id, pet_level))
}

/// Spawns a player-owned pet at `spawn_pos`, releasing whatever pet the owner
/// already has first (`release_owned_pets`). Used by the PET_SUMMON CastSpell arm, the Beast
/// Master auto-summon on EnterWorld, and the warder death-respawn sweep,
/// all through `summon_scaled_pet`. `hp_fraction` is 1.0 for fresh
/// summons and 0.3 for the warder return. Returns the new pet's id.
#[allow(clippy::too_many_arguments)]
pub(super) fn summon_pet_for_owner(
    server: &mut RenetServer,
    in_world_recipients: &[ClientId],
    enemies: &mut HashMap<EntityId, Entity>,
    aoi: &mut AoiGrid,
    owner_id: EntityId,
    spawn_pos: Vec3f,
    template: crate::world::zones::MobTemplate,
    hp_fraction: f32,
    now: Instant,
) -> EntityId {
    // One pet per owner: whatever they had goes first.
    release_owned_pets(server, aoi, enemies, in_world_recipients, owner_id, now);

    let mut pet = Entity::from_pet_summon(owner_id, spawn_pos, template, now);
    let new_hp = (pet.max_hp * hp_fraction.clamp(0.0, 1.0)).max(1.0);
    pet.hp = new_hp;
    let pet_id = pet.id;
    let pet_cell = aoi::cell_for(pet.pos.x, pet.pos.z);
    aoi.insert(pet_id, pet_cell);
    let visible = aoi.entities_visible_from(pet_cell);
    let pet_recipients: Vec<ClientId> = in_world_recipients
        .iter()
        .copied()
        .filter(|id| visible.contains(id))
        .collect();
    if !pet_recipients.is_empty() {
        handlers::fan_out_pet_spawn(server, &pet_recipients, &pet, None);
    }
    enemies.insert(pet_id, pet);
    pet_id
}

/// A charm landing (the `PET_CHARM` arm): the mob `target_id` becomes
/// `owner_id`'s pet for `duration_secs`. The id partition is re-keyed
/// (EntityDespawn for the enemy id, PetSpawn for a fresh pet id at the same
/// place with the same HP, the spawn naming what it replaces so a client keeps
/// its target), after whatever pet the owner already had is released
/// (`release_owned_pets`: one pet at a time). When the charm ends the mob is
/// handed back as the enemy it was, hostile to its charmer
/// (`return_charmed_mob`). Returns the pet's id, or None when the victim is
/// gone, dead or already somebody's pet (nothing is released then).
#[allow(clippy::too_many_arguments)]
pub(super) fn charm_mob(
    server: &mut RenetServer,
    in_world_recipients: &[ClientId],
    enemies: &mut HashMap<EntityId, Entity>,
    aoi: &mut AoiGrid,
    owner_id: EntityId,
    target_id: EntityId,
    duration_secs: f32,
    now: Instant,
) -> Option<EntityId> {
    if !enemies
        .get(&target_id)
        .is_some_and(|e| e.is_alive() && !e.is_pet())
    {
        return None;
    }
    // The pet the caster already has goes first. A charmed one is handed back
    // and comes straight for the caster; a summoned one is dismissed. Before
    // this the arm never looked, so nothing but each charm spell's cooldown
    // (as long as its duration, in today's data) kept a caster to one.
    release_owned_pets(server, aoi, enemies, in_world_recipients, owner_id, now);
    let victim = enemies.remove(&target_id)?;
    // Despawn the old enemy id from AOI + fan.
    let enemy_cell = aoi::cell_for(victim.pos.x, victim.pos.z);
    aoi.remove(target_id, enemy_cell);
    let despawn_visible = aoi.entities_visible_from(enemy_cell);
    for &recipient in in_world_recipients.iter() {
        if despawn_visible.contains(&recipient) {
            handlers::send_entity_despawn(server, recipient, target_id);
        }
    }
    // The camp slot stays OCCUPIED for the charm's life. It used to be freed
    // here, so the camp respawned a replacement while the charmed mob still
    // existed; now that the mob comes back when the charm ends, freeing it
    // would let a charm-and-wait add a mob to the camp per cast. The slot
    // rides on the pet: the return puts the mob back into it, and a pet that
    // dies charmed frees it through the corpse sweep like any death.
    //
    // The fresh pet keeps the mob's hp, max_hp and yaw, so the charm preserves
    // its current state, and carries its slot and home so it can be given
    // back.
    let mut pet = Entity::from_pet_summon(owner_id, victim.pos, victim.mob, now);
    pet.max_hp = victim.max_hp;
    pet.hp = victim.hp;
    pet.yaw = victim.yaw;
    pet.spawn_point_idx = victim.spawn_point_idx;
    pet.spawn_pos = victim.spawn_pos;
    // Who had hurt the mob still has when it is handed back
    // (`Entity::charm_ledger`): a charm is no way to take a kill from whoever
    // did the work.
    pet.charm_ledger = victim.aggro;
    // A charmed wanderer stays a wanderer for the hand-back (a pet never
    // walks its legs: `tick_roam`).
    pet.roam = victim.roam;
    pet.charm_expires_at = Some(now + std::time::Duration::from_secs_f32(duration_secs));
    let pet_id = pet.id;
    let pet_cell = aoi::cell_for(pet.pos.x, pet.pos.z);
    aoi.insert(pet_id, pet_cell);
    let spawn_visible = aoi.entities_visible_from(pet_cell);
    let pet_recipients: Vec<ClientId> = in_world_recipients
        .iter()
        .copied()
        .filter(|id| spawn_visible.contains(id))
        .collect();
    if !pet_recipients.is_empty() {
        // Same creature, new id: the caster keeps it targeted.
        handlers::fan_out_pet_spawn(server, &pet_recipients, &pet, Some(target_id));
    }
    enemies.insert(pet_id, pet);
    Some(pet_id)
}

/// One pet at a time (user rule, 2026-11-16: "If someone casts charm on a
/// second creature, the charm on the first creature should drop immediately,
/// and when a creature comes out of charm it should be aggressive towards the
/// caster", then "same rule covers other combinations"): a new pet replaces
/// the old one, and a charmed one is handed back angry. Every path that makes
/// a pet calls this first, the charm arm and `summon_pet_for_owner` alike, so
/// a second pet never stands beside the first.
///
/// A summoned pet is dismissed (marked Dead and told to everyone; the corpse
/// sweep removes the body; a pet's death drops nothing). A charmed one is
/// handed back through `return_charmed_mob`, hostile to `owner_id`.
///
/// Only LIVING pets: a charmed pet that has died is a body the corpse sweep
/// owns (it despawns it and frees its slot). Handed back, its 0 HP would make
/// a mob the stray sweep kills next tick, dropping the mob's loot for a fight
/// nobody fought (the Sable faucet of 2026-11-09, which the expiry sweep and
/// `despawn_owned_pets` guard against the same way).
pub(super) fn release_owned_pets(
    server: &mut RenetServer,
    aoi: &mut AoiGrid,
    enemies: &mut HashMap<EntityId, Entity>,
    in_world: &[ClientId],
    owner_id: EntityId,
    now: Instant,
) {
    let owned: Vec<(EntityId, bool)> = enemies
        .iter()
        .filter(|(_, e)| e.owner == Some(owner_id) && e.is_alive())
        .map(|(id, e)| (*id, e.charm_expires_at.is_some()))
        .collect();
    for (pet_id, charmed) in owned {
        if charmed {
            let Some(pet) = enemies.remove(&pet_id) else {
                continue;
            };
            let mob_id =
                return_charmed_mob(server, aoi, enemies, in_world, pet, Some(owner_id), now);
            tracing::info!(
                owner = owner_id,
                pet_id,
                mob_id,
                "charm dropped for a new pet — mob returned hostile"
            );
        } else {
            if let Some(old) = enemies.get_mut(&pet_id) {
                old.transition(EnemyState::Dead, now);
            }
            handlers::fan_out_entity_died(server, in_world, pet_id);
        }
    }
}

/// The owner always sees their own pet, wherever either of them stands
/// (user call, 2026-10-02). A pet is never despawned for its owner by a loss
/// of view, its Position fans to the owner outside the AOI neighbourhood,
/// and the real end of it (charm expiry, the body expiring) reaches the owner
/// wherever they are. The alternative was a pet parked with `/pet guard`
/// vanishing from its owner's HUD, and every `/pet` command with it, once
/// the owner walked two cells away. Returns the owner's client id for a pet,
/// None for anything else.
pub(super) fn pet_owner_cid(entity: &Entity) -> Option<ClientId> {
    if entity.is_pet() {
        entity.owner.map(|o| o as ClientId)
    } else {
        None
    }
}

/// Despawn the pet(s) owned by `owner_entity`: fan EntityDespawn to the AOI
/// peers who could see each pet, then drop it from the grid and the enemies
/// map. A charmed mob is not dropped but handed back (`return_charmed_mob`),
/// walking home since the charmer is gone. Idempotent — a second
/// call after the pets are already gone is a no-op. Called both when a player
/// goes linkdead (the body lingers but the pet can't be commanded) and from
/// the final reap.
pub(super) fn despawn_owned_pets(
    server: &mut RenetServer,
    connections: &HashMap<ClientId, PerConnection>,
    aoi: &mut AoiGrid,
    enemies: &mut HashMap<EntityId, Entity>,
    owner_entity: EntityId,
    owner_client_id: ClientId,
    now: std::time::Instant,
) {
    // Track 11 — owner's pet dies with them. Future work: hand off pets on
    // zone change rather than instant despawn.
    // A charmed mob that died before its charmer left is a body, not a mob to
    // hand back: the corpse sweep despawns it and frees its camp slot within
    // ENEMY_DESPAWN_LINGER_SECS. Handed back, its 0 HP made a stray kill with
    // the mob's loot (the expiry sweep's bug of 2026-11-09, the same shape).
    // A summoned pet's body is despawned here as before (no slot to free).
    let owned_pets: Vec<EntityId> = enemies
        .iter()
        .filter(|(_, e)| e.owner == Some(owner_entity))
        .filter(|(_, e)| e.charm_expires_at.is_none() || e.is_alive())
        .map(|(id, _)| *id)
        .collect();
    // The leaver's transport is going or gone, so they are not an audience.
    let in_world: Vec<ClientId> = connections
        .iter()
        .filter(|(id, c)| **id != owner_client_id && c.in_world)
        .map(|(id, _)| *id)
        .collect();
    for pet_id in owned_pets {
        let Some(pet) = enemies.remove(&pet_id) else {
            continue;
        };
        if pet.charm_expires_at.is_some() {
            let mob_id = return_charmed_mob(server, aoi, enemies, &in_world, pet, None, now);
            tracing::info!(
                owner = owner_entity,
                pet_id,
                mob_id,
                "charm broken by owner leaving — mob returned"
            );
            continue;
        }
        let pet_cell = aoi::cell_for(pet.pos.x, pet.pos.z);
        aoi.remove(pet_id, pet_cell);
        let pet_visible = aoi.entities_visible_from(pet_cell);
        for peer_id in in_world
            .iter()
            .copied()
            .filter(|id| pet_visible.contains(id))
        {
            handlers::send_entity_despawn(server, peer_id, pet_id);
        }
        tracing::info!(
            owner = owner_entity,
            pet_id,
            "pet despawned on owner disconnect"
        );
    }
}

/// A charm ending, by expiry or by the charmer leaving: the pet (already
/// taken out of `enemies` by the caller) is despawned for everyone who saw it,
/// and the mob it was comes back in its place, into the camp slot the charm
/// carried, at the pet's position with the pet's HP (EQ's rule, user call
/// 2026-10-06). With `turn_on` set it comes straight for its former charmer;
/// with nobody to hate it walks home. Returns the new enemy id.
pub(super) fn return_charmed_mob(
    server: &mut RenetServer,
    aoi: &mut AoiGrid,
    enemies: &mut HashMap<EntityId, Entity>,
    in_world: &[ClientId],
    pet: Entity,
    turn_on: Option<EntityId>,
    now: std::time::Instant,
) -> EntityId {
    let cell = aoi::cell_for(pet.pos.x, pet.pos.z);
    aoi.remove(pet.id, cell);
    let visible = aoi.entities_visible_from(cell);
    for recipient in despawn_recipients(&visible, in_world, &pet) {
        handlers::send_entity_despawn(server, recipient, pet.id);
    }
    let mut mob = Entity::released_from_charm(&pet, now);
    match turn_on {
        Some(charmer) => mob.turn_on(charmer, now),
        None => mob.go_home(now),
    }
    let mob_id = mob.id;
    aoi.insert(mob_id, cell);
    let audience: Vec<ClientId> = in_world
        .iter()
        .copied()
        .filter(|cid| visible.contains(cid))
        .collect();
    // `replaces` lets a client that had the pet targeted keep the mob targeted.
    handlers::fan_out_enemy_spawn(server, &audience, &mob, Some(pet.id));
    if let Some(charmer) = turn_on {
        handlers::fan_out_entity_target(server, in_world, mob_id, Some(charmer));
    }
    enemies.insert(mob_id, mob);
    mob_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::zones::MobTemplate;
    use std::time::Duration;

    fn template(name: &str) -> MobTemplate {
        MobTemplate {
            name: name.into(),
            level: 5,
            hp: 100.0,
            dmg: 5,
            xp: 0,
            speed: 3.0,
            aggro: 10.0,
            leash: None,
            melee_range: None,
            attack_interval: None,
            named_id: None,
            faction: None,
            alignment: None,
            faction_hit: None,
        }
    }

    fn at(x: f32) -> Vec3f {
        Vec3f { x, y: 0.0, z: 0.0 }
    }

    /// A camp mob charmed by `owner`, built the way the charm arm builds one.
    fn charmed(owner: EntityId, slot: usize, x: f32, now: Instant) -> Entity {
        let mut pet = Entity::from_pet_summon(owner, at(x), template("Camp Mob"), now);
        pet.spawn_point_idx = slot;
        pet.spawn_pos = at(x + 40.0);
        pet.hp = 60.0;
        pet.charm_expires_at = Some(now + Duration::from_secs(60));
        pet
    }

    /// One pet at a time (user rule, 2026-11-16), over the pairings no class
    /// can reach in play yet: an owner planted with a summoned pet, a living
    /// charmed pet and a charmed pet that has died. The summoned one is
    /// dismissed, the living charmed one comes back as the mob it was and
    /// turns on the owner with the damage ledger it had when charmed, and the
    /// dead one stays a body for the corpse sweep (handed back at 0 HP it was
    /// a loot faucet). Another owner's pet is not touched.
    #[test]
    fn a_new_pet_replaces_the_old_and_a_charmed_one_comes_back_angry() {
        let now = Instant::now();
        let mut server = RenetServer::new(crate::world::connection_config());
        let mut aoi = AoiGrid::new();
        let mut enemies: HashMap<EntityId, Entity> = HashMap::new();
        let owner: EntityId = 7;
        let other: EntityId = 8;
        let earlier_attacker: EntityId = 9;

        let summoned = Entity::from_pet_summon(owner, at(1.0), template("Skeleton"), now);
        let summoned_id = summoned.id;
        let mut live = charmed(owner, 3, 5.0, now);
        live.charm_ledger.insert(earlier_attacker, 40.0);
        let live_id = live.id;
        let mut body = charmed(owner, 4, 9.0, now);
        body.hp = 0.0;
        body.transition(EnemyState::Dead, now);
        let body_id = body.id;
        let theirs = Entity::from_pet_summon(other, at(20.0), template("Skeleton"), now);
        let theirs_id = theirs.id;
        for e in [summoned, live, body, theirs] {
            aoi.insert(e.id, aoi::cell_for(e.pos.x, e.pos.z));
            enemies.insert(e.id, e);
        }

        release_owned_pets(&mut server, &mut aoi, &mut enemies, &[], owner, now);

        assert!(
            !enemies[&summoned_id].is_alive(),
            "the summoned pet is dismissed"
        );
        assert!(
            !enemies.contains_key(&live_id),
            "the charmed pet's id is gone"
        );
        let back: Vec<&Entity> = enemies
            .values()
            .filter(|e| e.owner.is_none() && e.is_alive())
            .collect();
        assert_eq!(back.len(), 1, "exactly one mob is handed back");
        let mob = back[0];
        assert_eq!(mob.spawn_point_idx, 3, "into the slot the charm carried");
        assert_eq!(mob.hp, 60.0, "with the pet's HP");
        assert_eq!(mob.target, Some(owner), "and it comes for the owner");
        assert_eq!(mob.state, EnemyState::Chase);
        assert_eq!(
            mob.aggro.get(&earlier_attacker),
            Some(&40.0),
            "whoever hurt it before the charm keeps the credit"
        );
        let dead = &enemies[&body_id];
        assert!(
            dead.is_pet() && !dead.is_alive(),
            "a charmed pet that died stays a body: handed back at 0 HP it drops loot for no fight"
        );
        assert!(enemies[&theirs_id].is_alive(), "another owner's pet stays");
        assert!(
            !enemies
                .values()
                .any(|e| e.owner == Some(owner) && e.is_alive()),
            "the owner holds no living pet"
        );
    }

    /// The user's rule as cast (2026-11-16): a caster charms mob A, then mob
    /// B. A's charm drops at once and A comes back as an enemy on the caster;
    /// B is the pet; the caster owns exactly one pet. Fails with the release
    /// taken out of `charm_mob`, which leaves both charmed. (At the helper
    /// because no class can reach it by casting today: each charm spell's
    /// cooldown is as long as its charm.)
    #[test]
    fn a_second_charm_drops_the_first_which_turns_on_the_caster() {
        let now = Instant::now();
        let mut server = RenetServer::new(crate::world::connection_config());
        let mut aoi = AoiGrid::new();
        let mut enemies: HashMap<EntityId, Entity> = HashMap::new();
        let caster: EntityId = 7;
        let mut a = Entity::from_spawn(3, at(5.0), template("Mob A"), now);
        a.hp = 55.0;
        a.aggro.insert(9, 45.0);
        let a_id = a.id;
        let b = Entity::from_spawn(4, at(9.0), template("Mob B"), now);
        let b_id = b.id;
        for e in [a, b] {
            aoi.insert(e.id, aoi::cell_for(e.pos.x, e.pos.z));
            enemies.insert(e.id, e);
        }

        let pet_a = charm_mob(
            &mut server,
            &[],
            &mut enemies,
            &mut aoi,
            caster,
            a_id,
            60.0,
            now,
        )
        .expect("the first charm lands");
        assert!(!enemies.contains_key(&a_id), "the enemy id is re-keyed");
        assert_eq!(enemies[&pet_a].owner, Some(caster));
        assert_eq!(enemies[&pet_a].spawn_point_idx, 3, "the slot rides the pet");

        let later = now + Duration::from_secs(5);
        let pet_b = charm_mob(
            &mut server,
            &[],
            &mut enemies,
            &mut aoi,
            caster,
            b_id,
            60.0,
            later,
        )
        .expect("the second charm lands");

        assert!(
            !enemies.contains_key(&pet_a),
            "the first charm dropped at once"
        );
        let pets: Vec<&Entity> = enemies
            .values()
            .filter(|e| e.owner == Some(caster) && e.is_alive())
            .collect();
        assert_eq!(pets.len(), 1, "exactly one pet");
        assert_eq!(pets[0].id, pet_b);
        assert_eq!(pets[0].mob.name, "Mob B");
        let back: Vec<&Entity> = enemies.values().filter(|e| e.owner.is_none()).collect();
        assert_eq!(back.len(), 1, "mob A is an enemy again");
        let mob_a = back[0];
        assert_eq!(mob_a.mob.name, "Mob A");
        assert_eq!(mob_a.spawn_point_idx, 3, "back in its own slot");
        assert_eq!(mob_a.hp, 55.0);
        assert_eq!(mob_a.target, Some(caster), "and it comes for the caster");
        assert_eq!(mob_a.state, EnemyState::Chase);
        assert_eq!(mob_a.aggro.get(&9), Some(&45.0), "its damage ledger kept");

        // A victim that is already a pet, or gone, charms nothing and
        // releases nothing.
        assert!(charm_mob(
            &mut server,
            &[],
            &mut enemies,
            &mut aoi,
            caster,
            pet_b,
            60.0,
            later
        )
        .is_none());
        assert!(
            enemies[&pet_b].is_alive() && enemies[&pet_b].owner == Some(caster),
            "a refused charm leaves the pet the caster has"
        );
    }
}
