//! A pet's life outside its commands: summoning one scaled to its owner, who owns it, and what happens to owned pets and charmed mobs when the owner leaves or the charm ends.

use super::*;

/// Track 12 Piece B — spawn a player-owned pet at `spawn_pos`,
/// despawning the owner's existing pet first. Used by the
/// PET_SUMMON CastSpell arm, the Beast Master auto-summon on
/// EnterWorld, and the warder death-respawn sweep. `hp_fraction`
/// is 1.0 for fresh summons and 0.3 for the warder return.
/// Returns the new pet's id.
#[allow(clippy::too_many_arguments)]
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
) -> Option<(EntityId, u32)> {
    let template = pet_templates::scaled(pet_type, owner_level, variance)?;
    let pet_level = template.level;
    // Spawn at the owner's pos + 1.5 m east so the pet doesn't clip the
    // player capsule.
    let spawn_pos = Vec3f {
        x: owner_pos.x + 1.5,
        y: owner_pos.y,
        z: owner_pos.z,
    };
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
    // Despawn existing pet first (one-pet-per-owner). Mark Dead +
    // fan EntityDied; corpse cleanup runs naturally next tick.
    let existing_pet_id: Option<EntityId> = enemies
        .iter()
        .find(|(_, e)| e.owner == Some(owner_id) && e.is_alive())
        .map(|(id, _)| *id);
    if let Some(old_id) = existing_pet_id {
        if let Some(old) = enemies.get_mut(&old_id) {
            old.transition(EnemyState::Dead, now);
        }
        handlers::fan_out_entity_died(server, in_world_recipients, old_id);
    }

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
    let owned_pets: Vec<EntityId> = enemies
        .iter()
        .filter(|(_, e)| e.owner == Some(owner_entity))
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
