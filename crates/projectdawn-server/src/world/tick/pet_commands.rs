//! Pet commands (step 4hc): the owner's `/pet` commands applied to their pet.

use super::*;

pub(super) fn apply_pet_commands(world: &mut WorldState, q: &mut Intents, now: Instant) {
    let WorldState { server, connections, enemies, .. } = world;
    let Intents { pet_command_intents, .. } = q;
    // 4hc. Track 12 Piece A — apply pet commands. Locate each
    //      caster's pet in `enemies`; for Attack validate the
    //      target enemy is alive; for Back clear the pet's
    //      target. Both stamp `command_at` so the pre-AI
    //      inheritance pass (4i) skips re-targeting from
    //      `last_attacked_enemy` while the sticky window holds.
    if !pet_command_intents.is_empty() {
        for intent in pet_command_intents.drain(..) {
            use protocol::world::pet_command as cmd;
            let pet_id_opt: Option<EntityId> = enemies
                .iter()
                .find(|(_, e)| e.owner == Some(intent.owner) && e.is_alive())
                .map(|(id, _)| *id);
            let Some(pet_id) = pet_id_opt else {
                tracing::debug!(owner = intent.owner, command = intent.command, "PetCommand dropped — no live pet");
                continue;
            };
            match intent.command {
                cmd::ATTACK => {
                    let Some(target_id) = intent.target_id else {
                        tracing::debug!(owner = intent.owner, "PetCommand ATTACK dropped — no target_id");
                        continue;
                    };
                    // Pet-on-player engage: target ids below
                    // ENEMY_ID_BASE are player char_ids. PvP rules
                    // mirror direct player-on-player melee (both
                    // need /pvp on, can't target self).
                    if target_id < protocol::world::ENEMY_ID_BASE {
                        let owner_cid = intent.owner as ClientId;
                        let target_cid = target_id as ClientId;
                        let allowed = if target_cid == owner_cid {
                            false
                        } else {
                            match (
                                connections.get(&owner_cid),
                                connections.get(&target_cid),
                            ) {
                                (Some(a), Some(t)) if t.in_world && t.hp > 0.0 => combat::can_attack(
                                    a, t,
                                    a.zone.as_deref(),
                                    t.zone.as_deref(),
                                ),
                                _ => false,
                            }
                        };
                        if !allowed {
                            let t_name = connections
                                .get(&target_cid)
                                .map(|c| c.name.clone())
                                .unwrap_or_default();
                            let line = if !t_name.is_empty() {
                                format!("Unable to attack {}.", t_name)
                            } else {
                                "Unable to attack that target.".to_string()
                            };
                            handlers::fan_out_chat_message(
                                &mut *server,
                                &[owner_cid],
                                "",
                                protocol::world::ChatChannel::System,
                                &line,
                            );
                            tracing::debug!(
                                owner = intent.owner,
                                target = target_id,
                                "PetCommand ATTACK rejected — player PvP gate"
                            );
                            continue;
                        }
                        if let Some(pet) = enemies.get_mut(&pet_id) {
                            pet.target = Some(target_id);
                            pet.command_at = Some(now);
                            pet.stance = entity::PetStance::Follow;
                        }
                        tracing::info!(owner = intent.owner, pet_id, target = target_id, "PetCommand ATTACK (player)");
                        continue;
                    }
                    // Resolve target's liveness, owner (for pet
                    // targets), and pet-ness in one immutable
                    // lookup so the PvP gate below can run before
                    // the mutable pet-borrow at line ~3767.
                    let (target_alive, target_owner_opt, target_is_pet) =
                        match enemies.get(&target_id) {
                            Some(e) => (e.is_alive(), e.owner, e.is_pet()),
                            None => (false, None, false),
                        };
                    if !target_alive {
                        tracing::debug!(owner = intent.owner, target = target_id, "PetCommand ATTACK dropped — target not alive");
                        continue;
                    }
                    // Pet-on-pet engage: owner cannot point their
                    // pet at their own pet, and the two owners must
                    // both have /pvp on (mirrors the player-on-pet
                    // gate at line ~2085). Rejection routes the
                    // same "Unable to attack" line to the owner so
                    // the failure mode matches direct attacks.
                    if target_is_pet {
                        let owner_cid = intent.owner as ClientId;
                        // Own pet — refuse with a distinct line so
                        // the player isn't confused by an
                        // "Unable to attack Self's Wolf." rejection.
                        if target_owner_opt == Some(intent.owner) {
                            handlers::fan_out_chat_message(
                                &mut *server,
                                &[owner_cid],
                                "",
                                protocol::world::ChatChannel::System,
                                "Your pet won't attack itself.",
                            );
                            continue;
                        }
                        let allowed = match target_owner_opt {
                            Some(t_owner) => {
                                let t_cid = t_owner as ClientId;
                                match (
                                    connections.get(&owner_cid),
                                    connections.get(&t_cid),
                                ) {
                                    (Some(a), Some(b)) => combat::can_attack(
                                        a, b,
                                        a.zone.as_deref(),
                                        b.zone.as_deref(),
                                    ),
                                    _ => false,
                                }
                            }
                            None => true, // unowned pet (charm etc.) — treat as enemy
                        };
                        if !allowed {
                            let t_owner_name = target_owner_opt
                                .and_then(|id| connections.get(&(id as ClientId)).map(|c| c.name.clone()))
                                .unwrap_or_default();
                            let pet_name = enemies
                                .get(&target_id)
                                .map(|p| p.mob.name.clone())
                                .unwrap_or_default();
                            let line = if !t_owner_name.is_empty() && !pet_name.is_empty() {
                                format!("Unable to attack {}'s {}.", t_owner_name, pet_name)
                            } else if !pet_name.is_empty() {
                                format!("Unable to attack the {}.", pet_name)
                            } else {
                                "Unable to attack that target.".to_string()
                            };
                            handlers::fan_out_chat_message(
                                &mut *server,
                                &[owner_cid],
                                "",
                                protocol::world::ChatChannel::System,
                                &line,
                            );
                            tracing::debug!(
                                owner = intent.owner,
                                target = target_id,
                                "PetCommand ATTACK rejected — pet PvP gate"
                            );
                            continue;
                        }
                    }
                    if let Some(pet) = enemies.get_mut(&pet_id) {
                        pet.target = Some(target_id);
                        pet.command_at = Some(now);
                        // ATTACK implies "engage" — restore Follow
                        // stance so the pet chases / returns to
                        // the owner after the kill.
                        pet.stance = entity::PetStance::Follow;
                    }
                    tracing::info!(owner = intent.owner, pet_id, target = target_id, "PetCommand ATTACK");
                }
                cmd::BACK | cmd::FOLLOW => {
                    if let Some(pet) = enemies.get_mut(&pet_id) {
                        pet.target = None;
                        pet.command_at = Some(now);
                        pet.stance = entity::PetStance::Follow;
                    }
                    tracing::info!(owner = intent.owner, pet_id, "PetCommand BACK/FOLLOW");
                }
                cmd::GUARD => {
                    if let Some(pet) = enemies.get_mut(&pet_id) {
                        // Park the pet at its current spot. Drop
                        // any inherited target so the pet doesn't
                        // immediately chase off — Guard only
                        // engages via incoming damage or a follow-
                        // up Attack command.
                        pet.target = None;
                        pet.command_at = Some(now);
                        pet.stance = entity::PetStance::Guard;
                    }
                    tracing::info!(owner = intent.owner, pet_id, "PetCommand GUARD");
                }
                cmd::SIT => {
                    if let Some(pet) = enemies.get_mut(&pet_id) {
                        // Full passive: drop target, stand still,
                        // ignore incoming damage retargeting.
                        pet.target = None;
                        pet.command_at = Some(now);
                        pet.stance = entity::PetStance::Sit;
                    }
                    tracing::info!(owner = intent.owner, pet_id, "PetCommand SIT");
                }
                _ => {
                    tracing::debug!(owner = intent.owner, command = intent.command, "PetCommand variant not yet implemented");
                }
            }
        }
    }
}
