//! The enemies' turn (steps 4i and 4j): each mob and pet runs its state machine and
//! swings, and dead mobs are cleared after their linger.

use super::*;

pub(super) fn run_enemy_ai(
    world: &mut WorldState,
    _q: &mut Intents,
    now: Instant,
    dt: f32,
    in_world_recipients_now: &[ClientId],
) {
    let WorldState {
        server,
        connections,
        aoi,
        enemies,
        loot_bags,
        group_manager,
        ..
    } = world;
    // 4i. Enemy AI tick. Each alive enemy evaluates its state machine
    //     against the snapshot of in_world player positions, advances
    //     its own pos / target / state, and yields events for the
    //     post-loop fan-out (target switch, melee swing). Position
    //     broadcasts ride the same step-6 fan-out as players.
    //
    //     Track 11 — pets share this loop. Before the per-entity
    //     tick, resolve each pet's inherited target from its owner's
    //     `last_attacked_enemy` (if fresh and the target is still a
    //     live enemy). Pets also need a snapshot of all live enemy
    //     positions so they can chase their target inside the
    //     per-entity borrow.
    if !enemies.is_empty() && !in_world_recipients_now.is_empty() {
        const PET_TARGET_DECAY_SECS: f32 = 10.0;
        // Dead players are NOT aggro-able. Enemies previously kept chasing and
        // hitting a corpse because this list filtered only on `in_world` —
        // note the pet-AI snapshot a few lines below always carried
        // `c.hp > 0.0`, so pets already knew and only the enemy AI did not.
        // Dropping the dead from the list makes an enemy lose its target the
        // moment you die, so it leashes home instead of beating your body
        // (playtest 2026-08-12).
        let player_snapshots: Vec<(EntityId, Vec3f)> = connections
            .values()
            .filter(|c| c.in_world && c.hp > 0.0)
            .map(|c| (c.char_id as u64, c.pos))
            .collect();
        // Track 11.4 — enemies aggro on pets too. Build a combined
        // targets slice (players + alive pets) so enemy tick_idle
        // can pick the nearest of either. Pets share melee
        // melee range / chase behaviour with players from the
        // enemy's POV; the new enemy→pet hit dispatch below
        // applies damage to pets.
        // Players a Hide holds (active skills): an idle mob's proximity
        // scan passes over them; a provoked mob still finds them.
        let hidden_players: Vec<EntityId> = connections
            .values()
            .filter(|c| c.in_world && buffs::is_hidden(&c.active_buffs))
            .map(|c| c.char_id as u64)
            .collect();
        let mut targets_for_enemy_ai: Vec<(EntityId, Vec3f)> = player_snapshots.clone();
        for (id, entity) in enemies.iter() {
            if entity.is_pet() && entity.is_alive() {
                targets_for_enemy_ai.push((*id, entity.pos));
            }
        }
        // Snapshot live enemies (mobs AND pets) plus players so pet
        // AI can read its target's position + alive-status without
        // re-borrowing the map inside the per-entity loop. Pets and
        // players were excluded pre-Round-4 because pet AI only
        // engaged NPC mobs; /pet attack on a peer player or peer's
        // pet now passes the PvP gate at command time, so the
        // snapshot has to cover all three target classes or the pet
        // just stands still (no target_info → falls back to follow).
        let mut enemy_target_snapshots: Vec<(EntityId, Vec3f, bool)> = enemies
            .iter()
            .map(|(id, e)| (*id, e.pos, e.is_alive()))
            .collect();
        for c in connections.values() {
            if c.in_world {
                enemy_target_snapshots.push((c.char_id as u64, c.pos, c.hp > 0.0));
            }
        }
        // Pre-pass: drive each pet's target from its owner's last
        // attack. None if the inheritance has decayed or the
        // target is gone. Track 12 Piece A — pets with a fresh
        // `command_at` are excluded from re-inheritance: the
        // commanded target sticks until the window decays or the
        // owner explicitly issues another command.
        const PET_COMMAND_STICKY_SECS: f32 = 30.0;
        let pet_target_updates: Vec<(EntityId, Option<EntityId>)> = enemies
            .iter()
            .filter(|(_, e)| e.is_pet() && e.is_alive())
            // Track 15.3 — Guard / Sit pets never auto-inherit
            // from the owner's last attack. They only engage via
            // an explicit /pet attack or (for Guard) by being
            // attacked themselves.
            .filter(|(_, e)| e.stance == entity::PetStance::Follow)
            .filter(|(_, e)| {
                // Skip pets under an active command.
                e.command_at
                    .map(|t| now.duration_since(t).as_secs_f32() > PET_COMMAND_STICKY_SECS)
                    .unwrap_or(true)
            })
            .map(|(pet_id, pet)| {
                // Track 16.0 bug 4 — only inherit when the pet has
                // no live target. The old pre-pass overrode every
                // tick, so a stale owner attack (>10s) would zero
                // out the pet's target mid-fight (pet wanders back
                // to owner). Keep the current target whenever it's
                // still alive.
                let current_alive = pet.target.and_then(|t| {
                    enemy_target_snapshots
                        .iter()
                        .find(|(id, _, alive)| *id == t && *alive)
                        .map(|_| t)
                });
                if current_alive.is_some() {
                    return (*pet_id, current_alive);
                }
                let Some(owner_id) = pet.owner else {
                    tracing::error!(pet = *pet_id, "a pet with no owner; it inherits no target");
                    return (*pet_id, None);
                };
                let owner_cid = owner_id as ClientId;
                let owner = connections.get(&owner_cid);
                let inherited = owner.and_then(|c| {
                    let attacked = c.last_attacked_enemy?;
                    let at = c.last_attacked_at?;
                    if now.duration_since(at).as_secs_f32() > PET_TARGET_DECAY_SECS {
                        return None;
                    }
                    // Confirm the target is still a live enemy.
                    let alive = enemy_target_snapshots
                        .iter()
                        .any(|(id, _, alive)| *id == attacked && *alive);
                    if alive {
                        Some(attacked)
                    } else {
                        None
                    }
                });
                (*pet_id, inherited)
            })
            .collect();
        for (pet_id, target) in pet_target_updates {
            if let Some(pet) = enemies.get_mut(&pet_id) {
                pet.target = target;
            }
        }
        // Drop expired command stickiness so a stale Attack
        // command doesn't keep the pet pinned to a dead target
        // when the player just stops giving commands.
        //
        // Additionally, clear the sticky as soon as a Follow-stance
        // pet (no target) reaches its owner — the /pet back case.
        // Without this, the 30 s window blocks inheritance for
        // several seconds AFTER the pet has visibly arrived,
        // making the player feel like the pet "forgot" how to
        // engage. Round-5 playtest feedback: re-engage should kick
        // in the moment the pet is in close proximity to the
        // owner. Guard / Sit stances keep their sticky (those
        // stances *intentionally* suppress inheritance).
        const PET_BACK_HOME_DISTANCE: f32 = 4.0;
        for pet in enemies.values_mut() {
            if !pet.is_pet() {
                continue;
            }
            let Some(t) = pet.command_at else {
                continue;
            };
            if now.duration_since(t).as_secs_f32() > PET_COMMAND_STICKY_SECS {
                pet.command_at = None;
                continue;
            }
            if pet.target.is_some() {
                continue;
            }
            if pet.stance != entity::PetStance::Follow {
                continue;
            }
            let Some(owner_id) = pet.owner else {
                continue;
            };
            let Some(owner_pos) = connections
                .get(&(owner_id as ClientId))
                .filter(|c| c.in_world)
                .map(|c| c.pos)
            else {
                continue;
            };
            if pet.pos.distance_to(owner_pos) <= PET_BACK_HOME_DISTANCE {
                pet.command_at = None;
            }
        }
        let mut target_changes: Vec<(EntityId, Option<EntityId>)> = Vec::new();
        let mut enemy_hits: Vec<(EntityId, HitIntent)> = Vec::new();
        // DoT ticks dealt this sweep (spell batch step 5): the entity,
        // the tick, its hp and max_hp after, and whether it died of it.
        // Fanned and, on a kill, rewarded after the loop, since the kill
        // step needs the whole enemies map.
        let mut dot_ticks: Vec<(EntityId, entity::DotTick, f32, f32, bool)> = Vec::new();
        // Entities whose status set (buffs, DoTs, CC) changed this sweep;
        // their BuffSnapshot re-fans after the loop so the target frame
        // shows what is on a mob (spell batch step 5).
        let mut status_changed: Vec<EntityId> = Vec::new();
        // Alive-by-state bodies found at 0 HP with no DoT tick to blame
        // this sweep: killed properly after the loop, never left standing.
        let mut zero_hp_strays: Vec<EntityId> = Vec::new();
        // Collect enemy and pet cell changes so the aoi grid stays
        // current (player cell-change fan-outs read it to find what is
        // now visible) AND so each crossing can fan its own spawn /
        // despawn to the players it brought into or out of view.
        let mut enemy_cell_changes: Vec<(EntityId, (i32, i32), (i32, i32))> = Vec::new();
        for entity in enemies.values_mut() {
            if !entity.is_alive() {
                continue;
            }
            // Named-mob enrage. Checked here rather than at each damage
            // site because enemy HP is reduced in six separate places with
            // no shared helper, and this sweep runs downstream of all of
            // them in the same tick. One check, every damage source, plus
            // any added later.
            if let Some(named) = entity
                .mob
                .named_id
                .as_deref()
                .and_then(crate::world::named::lookup)
            {
                if entity.maybe_enrage(named) {
                    tracing::info!(
                        entity_id = entity.id,
                        mob = %entity.mob.name,
                        hp = entity.hp,
                        max_hp = entity.max_hp,
                        "named mob enraged"
                    );
                }
            }
            // Damage over time ticks (spell batch step 5), before the AI
            // step: a tick builds the caster's aggro and threat like a
            // hit (so an idle mob turns on them), and a tick that kills
            // is handled after the loop, with nothing below running on
            // the body this tick.
            let mut killed_by_dot = false;
            for tick in entity.tick_dots(dt) {
                entity.hp = (entity.hp - tick.amount as f32).max(0.0);
                *entity.aggro.entry(tick.caster).or_insert(0.0) += tick.amount as f32;
                *entity.threat.entry(tick.caster).or_insert(0.0) += tick.amount as f32;
                let died = entity.hp <= 0.0;
                dot_ticks.push((entity.id, tick, entity.hp, entity.max_hp, died));
                if died {
                    killed_by_dot = true;
                    break;
                }
            }
            if entity.hp <= 0.0 {
                if !killed_by_dot {
                    zero_hp_strays.push(entity.id);
                }
                continue;
            }
            let old_enemy_cell = aoi::cell_for(entity.pos.x, entity.pos.z);
            let events = entity.tick_ai(
                &targets_for_enemy_ai,
                &enemy_target_snapshots,
                &hidden_players,
                dt,
                now,
            );
            if let Some(new_target) = events.target_changed {
                target_changes.push((entity.id, new_target));
            }
            if let Some(hit) = events.hit {
                enemy_hits.push((entity.id, hit));
            }
            // Re-fan the status snapshot when the set of names on the
            // entity changed (a DoT or CC landing or running out).
            // The signature carries whole seconds, so a same-name refresh
            // (a re-cast DoT, a stronger snare) re-fans too, once a
            // second while anything is on the mob and never otherwise.
            let signature: Vec<String> = entity
                .status_pairs()
                .into_iter()
                .map(|(n, secs)| format!("{n}:{}", secs.max(0.0) as u32))
                .collect();
            if signature != entity.last_status_fanned {
                entity.last_status_fanned = signature;
                status_changed.push(entity.id);
            }
            let new_enemy_cell = aoi::cell_for(entity.pos.x, entity.pos.z);
            if new_enemy_cell != old_enemy_cell {
                enemy_cell_changes.push((entity.id, old_enemy_cell, new_enemy_cell));
            }
        }
        for (id, old_cell, new_cell) in enemy_cell_changes {
            let (gained, lost) = aoi.update(id, old_cell, new_cell);
            if let Some(entity) = enemies.get(&id) {
                fan_entity_cell_crossing(&mut *server, aoi, connections, entity, &gained, &lost);
            }
        }
        for (id, target) in target_changes {
            handlers::fan_out_entity_target(&mut *server, in_world_recipients_now, id, target);
        }
        // DoT ticks (spell batch step 5): everyone sees the hit and the
        // bar; the caster alone gets the named "<spell> for N" line (the
        // proc message, which the client already renders); a tick that
        // killed goes through the one kill step, so a DoT kill pays
        // exactly what a direct hit would.
        for (entity_id, tick, hp_after, max_hp, died) in dot_ticks {
            handlers::fan_out_hit(
                &mut *server,
                in_world_recipients_now,
                tick.caster,
                entity_id,
                tick.amount,
                false,
                tick.dmg_type,
            );
            handlers::fan_out_health_update(
                &mut *server,
                in_world_recipients_now,
                entity_id,
                hp_after,
                max_hp,
            );
            let caster_cid = tick.caster as ClientId;
            if connections.get(&caster_cid).is_some_and(|c| c.in_world) {
                handlers::fan_out_proc_triggered(
                    &mut *server,
                    &[caster_cid],
                    tick.caster,
                    entity_id,
                    tick.name.clone(),
                    tick.amount,
                    false,
                    tick.dmg_type,
                );
            }
            if died {
                tracing::info!(
                    entity_id,
                    caster = tick.caster,
                    spell = %tick.name,
                    "enemy killed by a damage-over-time tick"
                );
                kill_enemy(
                    &mut *server,
                    in_world_recipients_now,
                    &mut *connections,
                    &mut *enemies,
                    &mut *loot_bags,
                    &mut *aoi,
                    group_manager,
                    entity_id,
                    "damage-over-time kill",
                    now,
                );
            }
        }
        // A body at 0 HP that nothing killed this sweep (no path leaves
        // one today; this keeps it from standing invisible to the AI,
        // alive by state, forever).
        for entity_id in zero_hp_strays {
            kill_enemy(
                &mut *server,
                in_world_recipients_now,
                &mut *connections,
                &mut *enemies,
                &mut *loot_bags,
                &mut *aoi,
                group_manager,
                entity_id,
                "stray kill",
                now,
            );
        }
        // Status snapshots go to the players who can see the mob, as the
        // pet buff sweep's do, not to everyone in the world.
        for entity_id in status_changed {
            if let Some(entity) = enemies.get(&entity_id) {
                let cell = aoi::cell_for(entity.pos.x, entity.pos.z);
                let visible = aoi.entities_visible_from(cell);
                let audience: Vec<ClientId> = in_world_recipients_now
                    .iter()
                    .copied()
                    .filter(|cid| visible.contains(cid))
                    .collect();
                fan_out_pet_buff_snapshot(&mut *server, &audience, entity);
            }
        }
        for (attacker, hit) in enemy_hits {
            // Track 11.4 — enemy → pet. Mirror of the enemy →
            // player branch below but simpler: pets have no armor /
            // absorb and no XP awarded on death. Just apply HP, fan
            // Hit + HealthUpdate, and let corpse-cleanup remove the
            // pet after the linger window.
            // Track 13: a pet carries its buffs here too — a Thorns
            // damage shield reflects onto the attacker below (mirrors
            // the enemy→player reflect; a reflect-kill drops the enemy
            // with no XP/loot, same as the player path).
            if IdKind::of(attacker) == IdKind::Enemy && IdKind::of(hit.target) == IdKind::Pet {
                let amount = hit.amount.max(0);
                // Track 13 — Thorns reflect: captured while the pet is
                // borrowed, applied to the attacker after the borrow drops.
                let mut pet_shield: f32 = 0.0;
                let mut pet_shield_name: Option<String> = None;
                if let Some(pet) = enemies.get_mut(&hit.target) {
                    if !pet.is_alive() {
                        continue;
                    }
                    pet.hp = (pet.hp - amount as f32).max(0.0);
                    let new_hp = pet.hp;
                    let max_hp = pet.max_hp;
                    let died = new_hp <= 0.0;
                    pet_shield = buffs::damage_shield_total(&pet.active_buffs);
                    pet_shield_name =
                        buffs::first_damage_shield_name(&pet.active_buffs).map(|s| s.to_string());
                    if died {
                        pet.transition(EnemyState::Dead, now);
                    }
                    // Track 15.3 — Guard pets retaliate on
                    // incoming damage when they don't already
                    // have a target. Follow stance gets its
                    // target via the owner-inheritance pre-pass
                    // (or by being commanded); Sit ignores
                    // incoming damage on purpose.
                    if !died && pet.stance == entity::PetStance::Guard && pet.target.is_none() {
                        pet.target = Some(attacker);
                        pet.command_at = Some(now);
                    }
                    handlers::fan_out_hit(
                        &mut *server,
                        in_world_recipients_now,
                        attacker,
                        hit.target,
                        amount,
                        false,
                        protocol::world::DamageType::Physical,
                    );
                    handlers::fan_out_health_update(
                        &mut *server,
                        in_world_recipients_now,
                        hit.target,
                        new_hp,
                        max_hp,
                    );
                    if died {
                        // Track 12 Piece B — if this pet was a
                        // Beast Master warder, schedule its
                        // respawn on the owner. WARDER_RETREAT_SECS
                        // matches the GDScript WarderAI constant.
                        const WARDER_RETREAT_SECS: f32 = 15.0;
                        let respawn_owner: Option<(ClientId, std::time::Instant)> =
                            enemies.get(&hit.target).and_then(|p| {
                                if crate::world::pet_templates::is_warder_template(&p.mob.name) {
                                    let due = now
                                        + std::time::Duration::from_secs_f32(WARDER_RETREAT_SECS);
                                    p.owner.map(|o| (o as ClientId, due))
                                } else {
                                    None
                                }
                            });
                        if let Some((cid, due)) = respawn_owner {
                            if let Some(conn) = connections.get_mut(&cid) {
                                conn.warder_respawn_at = Some(due);
                                tracing::info!(
                                    owner = cid,
                                    retreat_secs = WARDER_RETREAT_SECS,
                                    "warder retreating; respawn scheduled",
                                );
                            }
                        }
                        handlers::fan_out_entity_died(
                            &mut *server,
                            in_world_recipients_now,
                            hit.target,
                        );
                        tracing::info!(
                            pet_id = hit.target,
                            killer = attacker,
                            "pet killed by enemy"
                        );
                    }
                }
                // Track 13 — Thorns reflect on a pet: the enemy that
                // struck a shielded pet takes the shield damage back.
                // Mirrors the enemy→player reflect. The reflect counts as
                // the pet owner's damage (aggro), so a reflect kill pays
                // them like any other (spell batch step 5; it used to pay
                // nothing).
                if pet_shield > 0.0 {
                    let pet_owner = enemies.get(&hit.target).and_then(|p| p.owner);
                    let mut reflect_killed = false;
                    if let Some(att_entity) = enemies.get_mut(&attacker) {
                        if att_entity.is_alive() {
                            let dmg = pet_shield as i32;
                            att_entity.hp = (att_entity.hp - dmg as f32).max(0.0);
                            if let Some(owner) = pet_owner {
                                *att_entity.aggro.entry(owner).or_insert(0.0) += dmg as f32;
                                *att_entity.threat.entry(owner).or_insert(0.0) += dmg as f32;
                            }
                            handlers::fan_out_health_update(
                                &mut *server,
                                in_world_recipients_now,
                                attacker,
                                att_entity.hp,
                                att_entity.max_hp,
                            );
                            if let Some(name) = pet_shield_name.as_ref() {
                                handlers::fan_out_damage_shield_trigger(
                                    &mut *server,
                                    in_world_recipients_now,
                                    hit.target,
                                    attacker,
                                    dmg,
                                    name.clone(),
                                );
                            }
                            reflect_killed = att_entity.hp <= 0.0;
                        }
                    }
                    if reflect_killed {
                        kill_enemy(
                            &mut *server,
                            in_world_recipients_now,
                            &mut *connections,
                            &mut *enemies,
                            &mut *loot_bags,
                            &mut *aoi,
                            group_manager,
                            attacker,
                            "reflect kill",
                            now,
                        );
                    }
                }
                continue;
            }
            // Track 11.3 — pet swings hit enemies. Attacker id
            // identifies the source: pets are in the PET_ID_BASE
            // partition. For pet→enemy hits we apply damage to
            // the target enemy directly, fan Hit + HealthUpdate,
            // and handle death (kill credit + loot routed to the
            // pet's owner, not the pet itself, via the aggro map
            // we accumulate under the owner's id).
            if IdKind::of(attacker) == IdKind::Pet
                && matches!(IdKind::of(hit.target), IdKind::Enemy | IdKind::Pet)
            {
                let owner_id_opt = enemies.get(&attacker).and_then(|p| p.owner);
                let amount = hit.amount.max(0);
                if let Some(target_entity) = enemies.get_mut(&hit.target) {
                    if !target_entity.is_alive() {
                        continue;
                    }
                    target_entity.hp = (target_entity.hp - amount as f32).max(0.0);
                    // Aggro credit under the owner so XP / top-
                    // damager logic naturally routes to them
                    // (pets don't level themselves).
                    if let Some(owner) = owner_id_opt {
                        *target_entity.aggro.entry(owner).or_insert(0.0) += amount as f32;
                    }
                    // Track 12 Piece A2 — threat accrues under
                    // the pet's own id so the enemy can pull
                    // onto a hard-hitting pet via the AI re-eval
                    // even when the owner has been damaging it
                    // longer.
                    *target_entity.threat.entry(attacker).or_insert(0.0) += amount as f32;
                    let new_hp = target_entity.hp;
                    let max_hp = target_entity.max_hp;
                    let died = new_hp <= 0.0;
                    // EQ quadratic per-kill award from the mob's level (see
                    // progression::kill_xp); pet kills credit the owner below.
                    let mob_xp = crate::world::progression::kill_xp(
                        target_entity.mob.level as i32,
                        crate::world::progression::ZEM_NORMAL,
                    );
                    let mob_level = target_entity.mob.level;
                    let mob_name_dead = if died {
                        target_entity.transition(EnemyState::Dead, now);
                        Some(target_entity.mob.name.clone())
                    } else {
                        None
                    };
                    // Owned victims (pets / charmed mobs) grant no quest
                    // credit — the warder is literally named "Wolf".
                    let victim_owned = target_entity.owner.is_some();
                    // Capture the dying entity's owner so a
                    // warder-template kill can schedule the
                    // owner's respawn after the borrow drops. None
                    // for NPC enemies; Some for pets.
                    let dead_pet_owner_opt: Option<EntityId> =
                        if died { target_entity.owner } else { None };
                    let death_pos = target_entity.pos;
                    let faction_kill = FactionKill::of(target_entity);
                    let credit_id_opt = if died {
                        target_entity
                            .aggro
                            .iter()
                            .max_by(|a, b| {
                                a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal)
                            })
                            .map(|(&id, _)| id)
                    } else {
                        None
                    };
                    // dmg breaks mez on the victim, matching the
                    // single-target ENEMY arm semantics.
                    if amount > 0 {
                        target_entity.clear_mez();
                    }
                    handlers::fan_out_hit(
                        &mut *server,
                        in_world_recipients_now,
                        attacker,
                        hit.target,
                        amount,
                        false,
                        protocol::world::DamageType::Physical,
                    );
                    handlers::fan_out_health_update(
                        &mut *server,
                        in_world_recipients_now,
                        hit.target,
                        new_hp,
                        max_hp,
                    );
                    if died {
                        handlers::fan_out_entity_died(
                            &mut *server,
                            in_world_recipients_now,
                            hit.target,
                        );
                        // Warder respawn — if a pet kills another
                        // pet that's a Beast Master warder, schedule
                        // the same retreat-and-respawn the melee /
                        // spell paths use.
                        if let Some(mob_name) = mob_name_dead.as_ref() {
                            if crate::world::pet_templates::is_warder_template(mob_name) {
                                if let Some(owner_id) = dead_pet_owner_opt {
                                    const WARDER_RETREAT_SECS: f32 = 15.0;
                                    let due = now
                                        + std::time::Duration::from_secs_f32(WARDER_RETREAT_SECS);
                                    let owner_cid = owner_id as ClientId;
                                    if let Some(conn) = connections.get_mut(&owner_cid) {
                                        conn.warder_respawn_at = Some(due);
                                        tracing::info!(
                                            owner = owner_cid as u64,
                                            killer = attacker,
                                            retreat_secs = WARDER_RETREAT_SECS,
                                            "warder retreating after pet kill",
                                        );
                                    }
                                }
                            }
                        }
                        if let Some(credit_id) = credit_id_opt {
                            // Pet kills credit the OWNER (aggro accrues
                            // under the owner's id), through the same
                            // group split + quest credit as melee/spell
                            // kills. Name from the dying mob; a live mob
                            // never reaches here with credit set.
                            if let Some(name) = mob_name_dead.as_ref() {
                                award_kill(
                                    &mut *server,
                                    &mut *connections,
                                    group_manager,
                                    credit_id,
                                    mob_xp,
                                    name,
                                    victim_owned,
                                    death_pos,
                                    faction_kill,
                                );
                            }
                        }
                        // An owned victim drops nothing (see kill_enemy).
                        if let Some(mob_name) = mob_name_dead.as_ref().filter(|_| !victim_owned) {
                            let loot_items = loot::roll_for_mob(mob_name).unwrap_or_default();
                            let loot_coins = loot::roll_coin_for_mob(mob_name, mob_level);
                            if !loot_items.is_empty() || loot_coins != protocol::world::Coins::ZERO
                            {
                                // Pet kills credit the pet's player owner;
                                // that player (and group) owns the corpse.
                                let owner_cid_opt = credit_id_opt
                                    .filter(|&id| IdKind::of(id) == IdKind::Player)
                                    .map(|id| id as ClientId);
                                let bag = LootBag::new(
                                    death_pos,
                                    loot_items,
                                    loot_coins,
                                    mob_name.clone(),
                                    owner_cid_opt,
                                    now,
                                );
                                let bag_id = bag.id;
                                let bag_cell = aoi::cell_for(bag.pos.x, bag.pos.z);
                                aoi.insert(bag_id, bag_cell);
                                let visible = aoi.entities_visible_from(bag_cell);
                                let bag_recipients: Vec<ClientId> = in_world_recipients_now
                                    .iter()
                                    .copied()
                                    .filter(|id| visible.contains(id))
                                    .collect();
                                if !bag_recipients.is_empty() {
                                    handlers::fan_out_loot_bag_spawn(
                                        &mut *server,
                                        &bag_recipients,
                                        &bag,
                                    );
                                }
                                loot_bags.insert(bag.id, bag);
                                tracing::info!(
                                    mob = %mob_name,
                                    bag_id,
                                    "loot bag spawned (pet kill)"
                                );
                            }
                        }
                    }
                }
                continue;
            }
            // Track 6: apply HP delta server-side when the enemy's
            // target is a player. The target_id space encodes
            // players below ENEMY_ID_BASE — anything in that range
            // is a char_id we can look up directly. Bigger ids
            // (other enemies, loot bags) shouldn't happen here
            // (enemy AI never targets non-players) but the
            // partition guards against it. Sub-task 3 applies the
            // same armor reduction the player-target branch uses.
            // The fan_out_hit amount tracks the post-reduction
            // value so the floating number matches the bar drop.
            let mut damaged_player: Option<u64> = None;
            let mut shield_to_attacker: f32 = 0.0;
            let mut shield_name_pve: Option<String> = None;
            let mut absorb_buff_to_strip: Option<usize> = None;
            // Evade (active skills, 2026-10-19): while it holds, every
            // melee swing at the player is deflected, fanned as an Evade
            // with nothing applied.
            let evaded = IdKind::of(hit.target) == IdKind::Player
                && connections.get(&(hit.target as ClientId)).is_some_and(|t| {
                    t.in_world && t.hp > 0.0 && buffs::is_evading(&t.active_buffs)
                });
            let final_amount = if IdKind::of(hit.target) == IdKind::Player {
                let target_cid = hit.target as ClientId;
                if let Some(target_conn) = connections.get_mut(&target_cid) {
                    if target_conn.in_world && target_conn.hp > 0.0 && !evaded {
                        // Seated-combat rule (classic EQ): a sitting
                        // target takes the swing with no armor, and a
                        // mob's swing is flat, so its max IS the amount.
                        // The hit then stands them (note_damage_taken).
                        let mut reduced = combat::mitigated(
                            hit.amount,
                            target_conn.equipped_armor,
                            target_conn.is_sitting,
                        );
                        // Track 6 sub-task 4c — consume absorb
                        // pool before applying damage. Returns
                        // the residual + whether the pool hit 0
                        // (caller removes the buff).
                        let (after_absorb, absorb_exhausted) =
                            buffs::consume_absorb(&mut target_conn.active_buffs, reduced);
                        reduced = after_absorb;
                        if absorb_exhausted {
                            if let Some(idx) = target_conn
                                .active_buffs
                                .iter()
                                .position(|b| matches!(b.effect, buffs::BuffEffect::Absorb { .. }))
                            {
                                absorb_buff_to_strip = Some(idx);
                            }
                        }
                        // Track 6 sub-task 4c — damage shield
                        // reflects damage back at the attacker.
                        // Read amount before mutating HP so a
                        // killing blow still triggers thorns.
                        shield_to_attacker = buffs::damage_shield_total(&target_conn.active_buffs);
                        shield_name_pve =
                            buffs::first_damage_shield_name(&target_conn.active_buffs)
                                .map(|s| s.to_string());
                        target_conn.hp = (target_conn.hp - reduced as f32).max(0.0);
                        regen::mark_dirty(target_conn);
                        target_conn.note_damage_taken(now); // camp breaks on damage
                                                            // Track 15.2 follow-up — feed the pet
                                                            // inheritance pre-pass so a FOLLOW-stance
                                                            // pet auto-engages whatever's hitting its
                                                            // owner (matches player behaviour: pet
                                                            // helps when you're being attacked, not
                                                            // only when you swing). Uses the same
                                                            // last_attacked_enemy/at fields the
                                                            // owner-attacks path already populates.
                        target_conn.last_attacked_enemy = Some(attacker);
                        target_conn.last_attacked_at = Some(now);
                        // Track 18.1 — armor skill advance: each
                        // unique equipped armor_type gets one
                        // try_advance roll per incoming hit. Also
                        // gives the defender a dodge roll. Mirrors
                        // GDScript ArmorSkills.try_advance_worn
                        // plus WeaponSkills "dodge" path. Collect
                        // armor types first (immutable borrow of
                        // target_conn via its inventory map),
                        // then drive try_advance (mutable borrow).
                        let mut armor_types: Vec<String> = Vec::new();
                        let mut seen: std::collections::HashSet<String> =
                            std::collections::HashSet::new();
                        for entry in target_conn.inventory.equipment.values() {
                            if let Some(item) = items::lookup(&entry.item_path) {
                                if !item.armor_type.is_empty()
                                    && seen.insert(item.armor_type.clone())
                                {
                                    armor_types.push(item.armor_type.clone());
                                }
                            }
                        }
                        for at in &armor_types {
                            if let Some(new_score) =
                                skills::try_advance(target_conn, skills::Skill::Armor, at)
                            {
                                handlers::send_skill_progress_update(
                                    &mut *server,
                                    target_cid,
                                    skills::Skill::Armor.as_protocol(),
                                    at.clone(),
                                    new_score,
                                );
                            }
                        }
                        if let Some(new_score) =
                            skills::try_advance(target_conn, skills::Skill::Weapon, "dodge")
                        {
                            handlers::send_skill_progress_update(
                                &mut *server,
                                target_cid,
                                skills::Skill::Weapon.as_protocol(),
                                "dodge".to_string(),
                                new_score,
                            );
                        }
                        // Track 19A — channeling-based cast interrupt
                        // on a hit landing during a cast. Helper
                        // mutates the cast cache + channeling score;
                        // we fan CastFail / SkillProgressUpdate
                        // based on the outcome.
                        let interrupt_outcome = roll_cast_interrupt(target_conn, now, reduced);
                        let (target_mp, target_max_mp) = (target_conn.mp, target_conn.max_mp);
                        match &interrupt_outcome {
                            InterruptOutcome::Interrupted {
                                spell_name,
                                mana_charged,
                            } => {
                                handlers::fan_out_cast_fail(
                                    &mut *server,
                                    in_world_recipients_now,
                                    hit.target,
                                    "interrupted (hit during cast)".to_string(),
                                );
                                tell_interrupt_charge(
                                    &mut *server,
                                    target_cid,
                                    hit.target,
                                    target_mp,
                                    target_max_mp,
                                    *mana_charged,
                                );
                                tracing::info!(
                                    caster = hit.target,
                                    spell = %spell_name,
                                    mana_charged,
                                    "cast interrupted by incoming damage"
                                );
                            }
                            InterruptOutcome::Survived {
                                advanced_to: Some(new_score),
                            } => {
                                handlers::send_skill_progress_update(
                                    &mut *server,
                                    target_cid,
                                    skills::Skill::Casting.as_protocol(),
                                    "channeling".to_string(),
                                    *new_score,
                                );
                            }
                            InterruptOutcome::Survived { advanced_to: None }
                            | InterruptOutcome::NotCasting => {}
                        }
                        damaged_player = Some(hit.target);
                        tracing::info!(
                            attacker,
                            target = hit.target,
                            raw_amount = hit.amount,
                            reduced,
                            armor = target_conn.equipped_armor,
                            hp_after = target_conn.hp,
                            "player damaged by enemy"
                        );
                        reduced
                    } else {
                        hit.amount
                    }
                } else {
                    hit.amount
                }
            } else {
                hit.amount
            };
            // Strip the exhausted absorb buff after the immutable
            // borrow chain ends. Fan BuffSnapshot too.
            if let (Some(target_id), Some(idx)) = (damaged_player, absorb_buff_to_strip) {
                let target_cid = target_id as ClientId;
                if let Some(tc) = connections.get_mut(&target_cid) {
                    if idx < tc.active_buffs.len() {
                        tc.active_buffs.remove(idx);
                    }
                }
                if let Some(tc) = connections.get(&target_cid) {
                    handlers::fan_out_buff_snapshot(&mut *server, in_world_recipients_now, tc);
                }
            }
            // Apply damage shield to attacker (the enemy entity).
            // Look up by attacker id in the enemies map; if not
            // present (attacker died this tick), skip silently.
            if shield_to_attacker > 0.0 {
                let mut reflect_killed = false;
                if let Some(att_entity) = enemies.get_mut(&attacker) {
                    if att_entity.is_alive() {
                        let dmg = shield_to_attacker as i32;
                        att_entity.hp = (att_entity.hp - dmg as f32).max(0.0);
                        // The reflect is the defender's damage (aggro), so
                        // a reflect kill pays them like any other (spell
                        // batch step 5; it used to pay nothing).
                        if let Some(defender) = damaged_player {
                            *att_entity.aggro.entry(defender).or_insert(0.0) += dmg as f32;
                            *att_entity.threat.entry(defender).or_insert(0.0) += dmg as f32;
                        }
                        handlers::fan_out_health_update(
                            &mut *server,
                            in_world_recipients_now,
                            attacker,
                            att_entity.hp,
                            att_entity.max_hp,
                        );
                        if let (Some(defender), Some(name)) =
                            (damaged_player, shield_name_pve.as_ref())
                        {
                            handlers::fan_out_damage_shield_trigger(
                                &mut *server,
                                in_world_recipients_now,
                                defender,
                                attacker,
                                dmg,
                                name.clone(),
                            );
                        }
                        reflect_killed = att_entity.hp <= 0.0;
                    }
                }
                if reflect_killed {
                    kill_enemy(
                        &mut *server,
                        in_world_recipients_now,
                        &mut *connections,
                        &mut *enemies,
                        &mut *loot_bags,
                        &mut *aoi,
                        group_manager,
                        attacker,
                        "reflect kill",
                        now,
                    );
                }
            }
            if evaded {
                handlers::fan_out_evade(
                    &mut *server,
                    in_world_recipients_now,
                    attacker,
                    hit.target,
                );
            } else {
                handlers::fan_out_hit(
                    &mut *server,
                    in_world_recipients_now,
                    attacker,
                    hit.target,
                    final_amount,
                    false,
                    DamageType::Physical,
                );
            }
            if let Some(target_id) = damaged_player {
                // Server's regen broadcast loop (step 4l) would catch
                // this within MAX_BROADCAST_GAP, but a fresh HP fan-out
                // *now* keeps the target's HUD in lockstep with the Hit
                // floating-number landing on the same tick.
                if let Some(target_conn) = connections.get(&(target_id as ClientId)) {
                    handlers::fan_out_health_update(
                        &mut *server,
                        in_world_recipients_now,
                        target_id,
                        target_conn.hp,
                        target_conn.max_hp,
                    );
                }
            }
        }
    }
}

pub(super) async fn clean_up_enemy_corpses(
    world: &mut WorldState,
    q: &mut Intents,
    now: Instant,
    in_world_recipients_now: &[ClientId],
) {
    let WorldState {
        pool,
        server,
        aoi,
        spawner,
        enemies,
        ..
    } = world;
    let Intents {
        spell_gem_saves, ..
    } = q;
    // 4j. Corpse cleanup. Dead enemies hold at their death pos for
    //     ENEMY_DESPAWN_LINGER_SECS so the client can play the fall-over
    //     animation; afterwards we fan out EntityDespawn, arm the
    //     spawn point's respawn timer, and drop the row from the
    //     world map. Collect ids in a first pass to avoid borrowing
    //     `enemies` mutably twice in the same loop.
    let corpse_linger = Duration::from_secs_f32(ENEMY_DESPAWN_LINGER_SECS);
    let mut expired_ids: Vec<EntityId> = Vec::new();
    for entity in enemies.values() {
        if entity.is_alive() {
            continue;
        }
        if now.duration_since(entity.state_entered_at) >= corpse_linger {
            expired_ids.push(entity.id);
        }
    }
    for id in expired_ids {
        let Some(entity) = enemies.remove(&id) else {
            continue;
        };
        // Track 7: remove from AOI; fan EntityDespawn only to players
        // who could see the enemy's cell (plus a dead pet's owner,
        // wherever they are).
        let enemy_cell = aoi::cell_for(entity.pos.x, entity.pos.z);
        aoi.remove(entity.id, enemy_cell);
        let visible = aoi.entities_visible_from(enemy_cell);
        for recipient in despawn_recipients(&visible, in_world_recipients_now, &entity) {
            handlers::send_entity_despawn(&mut *server, recipient, entity.id);
        }
        spawner.on_enemy_died(entity.spawn_point_idx, now);
    }

    // The spell bar, written as soon as it changes (a memorize is
    // deliberate and rare; the 60 s checkpoint would lose one to a crash).
    for (char_id, gems, seeded) in spell_gem_saves.drain(..) {
        if let Err(e) = db::save_spell_gems(pool, char_id, &gems, seeded).await {
            tracing::warn!(char_id, error = %e, "save_spell_gems failed; the bar is live in memory but not persisted");
        }
    }
}
