//! Spawns (steps 4g to 4g4): camp respawn timers and the Test Panel's spawns and
//! despawns, the warder's auto-summon and respawn, and charms running out. Returns the
//! in-world recipient list the later steps fan to.

use super::*;

pub(super) fn run_spawning(
    world: &mut WorldState,
    q: &mut Intents,
    now: Instant,
    in_world_recipients_now: &[ClientId],
) {
    let WorldState {
        server,
        connections,
        aoi,
        spawner,
        enemies,
        terrain,
        ..
    } = world;
    let Intents {
        newly_in_world,
        dev_spawn_intents,
        dev_despawn_intents,
        ..
    } = q;
    // 4g. Enemy spawner phase. Tick the respawn timers; for any spawn
    //     point that fires this frame, instantiate the entity, register
    //     it in the world map, and fan EnemySpawn out to every in_world
    //     client. Recipients computed AFTER the spawner tick so a
    //     client that just sent EnterWorld this tick (and was seeded
    //     with the prior enemy set in step 4a) also receives the new
    //     spawn — no duplicate seeds because the seed loop above ran
    //     against the pre-spawn map.
    {
        let newly_spawned = spawner.tick(now, terrain);
        if !newly_spawned.is_empty() {
            let spawn_recipients: Vec<ClientId> = connections
                .iter()
                .filter(|(_, c)| c.in_world)
                .map(|(id, _)| *id)
                .collect();
            for entity in newly_spawned {
                // Track 7: add the enemy to the AOI grid so player
                // cell-change fan-outs can find it, then fan EnemySpawn
                // only to players who can see its cell.
                let enemy_cell = aoi::cell_for(entity.pos.x, entity.pos.z);
                aoi.insert(entity.id, enemy_cell);
                if !spawn_recipients.is_empty() {
                    let visible = aoi.entities_visible_from(enemy_cell);
                    let aoi_recipients: Vec<ClientId> = spawn_recipients
                        .iter()
                        .copied()
                        .filter(|id| visible.contains(id))
                        .collect();
                    if !aoi_recipients.is_empty() {
                        handlers::fan_out_enemy_spawn(&mut *server, &aoi_recipients, &entity, None);
                    }
                }
                enemies.insert(entity.id, entity);
            }
        }
    }

    // PD_W0023 — dev-gated Test Panel spawns: same AOI-insert + EnemySpawn
    // fan as the natural spawner above, but one-shot (spawn_point_idx =
    // usize::MAX like pets, so death never arms a respawn timer — the
    // spawner's on_enemy_died .get_mut() on it is a safe no-op).
    if !dev_spawn_intents.is_empty() {
        let spawn_recipients: Vec<ClientId> = connections
            .iter()
            .filter(|(_, c)| c.in_world)
            .map(|(id, _)| *id)
            .collect();
        for (pos, mob) in dev_spawn_intents.drain(..) {
            // On the ground in front of the GM, like a camp spawn.
            let entity = crate::world::entity::Entity::from_spawn(
                usize::MAX,
                terrain.ground_vec(pos),
                mob,
                now,
            );
            let enemy_cell = aoi::cell_for(entity.pos.x, entity.pos.z);
            aoi.insert(entity.id, enemy_cell);
            if !spawn_recipients.is_empty() {
                let visible = aoi.entities_visible_from(enemy_cell);
                let aoi_recipients: Vec<ClientId> = spawn_recipients
                    .iter()
                    .copied()
                    .filter(|id| visible.contains(id))
                    .collect();
                if !aoi_recipients.is_empty() {
                    handlers::fan_out_enemy_spawn(&mut *server, &aoi_recipients, &entity, None);
                }
            }
            enemies.insert(entity.id, entity);
        }
    }

    // Test Panel "Despawn All Enemies" (dev-gated and audited in the
    // handler): every enemy the GM can see goes, the same set the
    // client-side button used to hide. Unowned only, so a player's pet
    // and a charmed mob stay. It is a removal, not a death: no XP, no
    // loot, no corpse. The camp slot is freed exactly as the body sweep
    // frees it, so camp mobs respawn on their timer (a dev spawn's slot
    // is usize::MAX, a no-op). Anything still pointing at a removed id
    // (a pet's target, an owner's last-attacked) is dropped by the same
    // liveness checks the charm landing relies on.
    for (requester, gm_pos) in dev_despawn_intents.drain(..) {
        let near = aoi.entities_visible_from(aoi::cell_for(gm_pos.x, gm_pos.z));
        let doomed: Vec<EntityId> = enemies
            .values()
            .filter(|e| e.owner.is_none() && near.contains(&e.id))
            .map(|e| e.id)
            .collect();
        let in_world: Vec<ClientId> = connections
            .iter()
            .filter(|(_, c)| c.in_world)
            .map(|(id, _)| *id)
            .collect();
        for id in &doomed {
            let Some(entity) = enemies.remove(id) else {
                continue;
            };
            let cell = aoi::cell_for(entity.pos.x, entity.pos.z);
            aoi.remove(entity.id, cell);
            let visible = aoi.entities_visible_from(cell);
            for recipient in despawn_recipients(&visible, &in_world, &entity) {
                handlers::send_entity_despawn(&mut *server, recipient, entity.id);
            }
            spawner.on_enemy_died(entity.spawn_point_idx, now);
        }
        tracing::info!(
            count = doomed.len(),
            x = gm_pos.x,
            z = gm_pos.z,
            "dev despawn enemies"
        );
        // Only an authorized request gets here, so the count tells a plain
        // account nothing about the dev gate.
        handlers::send_system_line(
            &mut *server,
            requester,
            &format!("Despawned {} enemies.", doomed.len()),
        );
    }

    // 4g2. Track 12 Piece B — auto-summon a warder for each
    //      freshly-EnterWorld'd Beast Master. Runs once when
    //      `newly_in_world` is non-empty so the spawn lands the
    //      same tick the client's EntitySpawn fan-out happens;
    //      AOI is already populated by the spawn loop above.
    if !newly_in_world.is_empty() {
        let beast_master_summons: Vec<(EntityId, Vec3f, u32)> = newly_in_world
            .iter()
            .filter_map(|cid| {
                let conn = connections.get(cid)?;
                if conn.class.eq_ignore_ascii_case("Beast Master") && conn.in_world {
                    Some((conn.char_id as u64, conn.pos, conn.level.max(1) as u32))
                } else {
                    None
                }
            })
            .collect();
        for (owner_id, caster_pos, owner_level) in beast_master_summons {
            // Owner-derived warder (pet interim A): deterministic
            // owner - 1, no variance on a free auto-summon.
            let Some((pet_id, pet_level)) = summon_scaled_pet(
                &mut *server,
                in_world_recipients_now,
                &mut *enemies,
                &mut *aoi,
                owner_id,
                owner_level,
                caster_pos,
                "warder",
                0,
                1.0,
                now,
                terrain,
            ) else {
                continue;
            };
            tracing::info!(
                owner = owner_id,
                pet_id,
                level = pet_level,
                "Beast Master warder auto-summoned"
            );
        }
    }

    // 4g3. Track 12 Piece B — warder respawn sweep. Beast Masters
    //      whose warder died get a fresh one at 30% HP after
    //      WARDER_RETREAT_SECS (set on death; checked here).
    //      Collected then drained so we don't overlap a
    //      `connections` borrow with the `enemies` mutation in
    //      the helper.
    let due_warder_respawns: Vec<(EntityId, Vec3f, u32)> = connections
        .values()
        .filter_map(|c| {
            if !c.in_world {
                return None;
            }
            let due = c.warder_respawn_at?;
            // `Instant::duration_since` saturates to zero when the
            // argument is in the future, so the previous
            // `>= 0.0` check was always true and the warder
            // respawned on the next tick instead of after the
            // WARDER_RETREAT_SECS retreat.
            if now >= due {
                Some((c.char_id as u64, c.pos, c.level.max(1) as u32))
            } else {
                None
            }
        })
        .collect();
    for (owner_id, caster_pos, owner_level) in due_warder_respawns {
        // Same deterministic owner - 1 as the auto-summon: the respawn
        // is the same free ritual, so no variance here either.
        let Some((pet_id, pet_level)) = summon_scaled_pet(
            &mut *server,
            in_world_recipients_now,
            &mut *enemies,
            &mut *aoi,
            owner_id,
            owner_level,
            caster_pos,
            "warder",
            0,
            0.3,
            now,
            terrain,
        ) else {
            continue;
        };
        if let Some(conn) = connections.get_mut(&(owner_id as ClientId)) {
            conn.warder_respawn_at = None;
        }
        tracing::info!(
            owner = owner_id,
            pet_id,
            level = pet_level,
            "warder respawned after retreat"
        );
    }

    // 4g4. Track 12 Piece C — charm decay sweep. A pet whose
    //      charm_expires_at has passed is handed back as the mob it
    //      was, and comes for its charmer if they are still here to
    //      be hated (EQ's rule, user call 2026-10-06; v1 despawned it
    //      as "mob runs away"). Collect then drain to avoid an
    //      `enemies` iter+mut overlap.
    let expired_charms: Vec<EntityId> = enemies
        .iter()
        .filter_map(|(id, e)| {
            let exp = e.charm_expires_at?;
            // `Instant::duration_since` saturates to zero when the
            // argument is still in the future, so the old `>= 0.0`
            // test was true on the very next tick: every charm ended
            // 50 ms after it landed (seen in play 2026-10-06, twice).
            // Same bug the warder retreat timer had.
            //
            // A pet that DIED inside the last ENEMY_DESPAWN_LINGER_SECS of its
            // charm is still here as a body, and is the corpse sweep's: it
            // despawns it and frees its camp slot. Handing it back as a mob
            // copied its 0 HP into the new enemy, the stray sweep killed that
            // the next tick, and the bag held the mob's loot for a fight
            // nobody fought (Sable the Dark, 2026-11-09: a loot faucet).
            if now >= exp && e.is_alive() {
                Some(*id)
            } else {
                None
            }
        })
        .collect();
    for pet_id in expired_charms {
        let Some(pet) = enemies.remove(&pet_id) else {
            continue;
        };
        // The charmer, if still in the world and alive.
        let charmer = pet.owner.filter(|o| {
            connections
                .get(&(*o as ClientId))
                .map(|c| c.in_world && c.hp > 0.0 && !c.death_processed)
                .unwrap_or(false)
        });
        let mob_id = return_charmed_mob(
            &mut *server,
            &mut *aoi,
            &mut *enemies,
            in_world_recipients_now,
            pet,
            charmer,
            now,
        );
        tracing::info!(pet_id, mob_id, charmer = ?charmer, "charm expired — mob returned hostile");
    }
}
