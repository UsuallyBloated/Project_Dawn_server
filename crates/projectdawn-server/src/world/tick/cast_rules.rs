//! The rules a cast is judged by: the target pre-flight, the refusal and refund, the interrupt roll and its mana charge, and a bard's song start, stop and pulse.

use super::*;

/// Track 6 sub-task 3b — buffered spell-cast intent. Server resolves
/// the spell in spells.toml, validates mana / target, and applies
/// damage or heal authoritatively.
///
/// Track 10 — `cast_name_at_dispatch` / `cast_set_at_at_dispatch` are
/// the caster's cast cache state at the moment handle_message decoded
/// this intent. The gate uses them to verify a CastStartBroadcast
/// for this spell actually ran for long enough; snapshotted at
/// dispatch (not gate) time so a same-batch CastCompleteBroadcast
/// doesn't blank the cache before the gate fires.
/// Track 19A — outcome of the on-hit cast-interrupt roll.
/// `Interrupted` means the cast cache was cleared; caller fans
/// `CastFail`. `Survived` means channeling advanced (or didn't);
/// caller fans a `SkillProgressUpdate` when `advanced_to` is Some.
/// `NotCasting` skips both.
pub(super) enum InterruptOutcome {
    NotCasting,
    /// The cast cache was cleared; `mana_charged` is what the interrupt
    /// cost (spell batch step 3), already taken off the caster.
    Interrupted {
        spell_name: String,
        mana_charged: f32,
    },
    Survived {
        advanced_to: Option<i32>,
    },
}

/// Track 19A — roll the channeling-based interrupt on a target who
/// just took damage. Mirrors GDScript `Spells.try_interrupt_cast` +
/// `_finish_cast`'s "advance on survival" path. Mutates the conn's
/// cast cache + channeling score in place; caller handles fan-out.
/// `damage_applied` is what the hit really took off the caster: an
/// interrupt charges the spell's mana in proportion to how far the bar
/// had run (`cast_gate::interrupt_charge`), and a hit that did nothing
/// charges nothing.
pub(super) fn roll_cast_interrupt(
    conn: &mut PerConnection,
    now: Instant,
    damage_applied: i32,
) -> InterruptOutcome {
    if conn.cast_spell_name.is_empty() || conn.cast_set_at.is_none() {
        return InterruptOutcome::NotCasting;
    }
    let cap = skills::cap_for(
        skills::Skill::Casting,
        &conn.class,
        conn.level,
        "channeling",
    );
    let score = conn.casting_skills.get("channeling").copied().unwrap_or(0);
    let chance = skills::channeling_interrupt_chance(score, cap);
    if rand::thread_rng().gen::<f32>() < chance {
        let spell_name = conn.cast_spell_name.clone();
        // The server's cast time, never the client's bar length: the
        // fraction is elapsed over the spell's own duration.
        let elapsed_ms = conn.cast_set_at.map(|t| now.duration_since(t).as_millis());
        let mana_charged = spells::lookup(&spell_name)
            .map(|s| {
                crate::world::cast_gate::interrupt_charge(
                    s.mana_cost,
                    s.cast_time,
                    elapsed_ms,
                    damage_applied,
                    conn.mp,
                )
            })
            .unwrap_or(0.0);
        if mana_charged > 0.0 {
            conn.mp = (conn.mp - mana_charged).max(0.0);
            regen::mark_dirty(conn);
        }
        conn.cast_spell_name.clear();
        conn.cast_total_duration = 0.0;
        conn.cast_set_at = None;
        // Stamped so a CastSpell for this bar that is already in the tick's
        // queue is refused too (`cast_gate::beaten_by_interrupt`).
        conn.cast_interrupted_at = Some(now);
        InterruptOutcome::Interrupted {
            spell_name,
            mana_charged,
        }
    } else {
        let advanced_to = skills::try_advance(conn, skills::Skill::Casting, "channeling");
        InterruptOutcome::Survived { advanced_to }
    }
}

/// The one line for a spell target that is out of reach, offline, or an id
/// that never existed. Deliberately the SAME line for all three: telling
/// "far" from "absent" would make a cast an is-online oracle for anyone
/// enumerating ids. No refusal names a player; the only names the caster is
/// given are the ones already on a nameplate within reach.
pub(super) const TARGET_UNREACHABLE: &str = "That target is too far away or no longer here.";

/// The line for a spell the server has no effect for yet: a target type with
/// no arm (BIND, PORT), an AOE with no radius, a summon with no known pet.
pub(super) const NO_EFFECT_YET: &str = "That magic has no effect here yet.";

/// What the cast pre-flight resolved the cast to, for the arm to act on
/// without looking the target up a second time or re-deciding reach,
/// liveness or the PvP gates. The pre-flight and the arm run back to back for
/// one intent, with nothing in between, so the arm may trust it; if an arm
/// ever finds its target gone anyway, `cast_resolution_lost` says so loudly
/// and refunds. One home for every rule (the 2026-10-02 review): a new target
/// kind or a per-spell range is decided here and nowhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResolvedTarget {
    /// Nothing to resolve: SELF, AOE, PET_SUMMON, PORT.
    Nothing,
    /// A player in the world and in reach: ENEMY's PvP path, ALLY on a
    /// player or on oneself, BIND on oneself or a group member.
    Player(ClientId),
    /// A live mob in reach: ENEMY's mob path, PET_CHARM's victim.
    Mob(EntityId),
    /// A live pet in reach: ENEMY on another player's pet, ALLY on a pet,
    /// PET_HEAL's own pet.
    Pet(EntityId),
    /// A corpse in reach that is not yet resurrected, and its owner, who is
    /// in the world.
    Corpse(EntityId, ClientId),
}

/// Cast eligibility, decided BEFORE any side effect. Returns the resolved
/// target for the arm to act on, or the refusal line.
///
/// Two reasons this is a pre-flight rather than checks inside the
/// per-target-type arms:
///
/// 1. **Reach.** ALLY spells (heals and buffs, on a player or a pet) and ENEMY
///    spells against a player had no range check at all, so a modified client
///    could heal a friend in a dungeon from town, or nuke a PvP-flagged player
///    from across the world, by naming an id. Every targeted cast now needs
///    its target within the spell's own reach (`Spell::reach`, 30 m unless
///    the spell names its own). Written keep-only-when-in-reach, so a
///    non-finite distance refuses.
/// 2. **Order.** The handler takes the mana, stamps the cooldown and rolls the
///    casting skill before its arms run. A refusal inside an arm could hand
///    the mana back but could not un-stamp the cooldown or un-roll the skill:
///    a cast the server was always going to refuse (Bind Affinity, which has
///    no server arm; a nuke at nothing) was a free casting-skill attempt from
///    any hotbar, and a refused Charm still burned its 60 s cooldown.
///    Everything decidable from immutable state is decided here, so a refused
///    cast costs nothing at all.
///
/// Every target type is covered: the three with a target (ENEMY, ALLY,
/// PET_CHARM), PET_HEAL (the caster's own pet), CORPSE, the spell-data checks
/// (an AOE with no radius, a summon with no known pet), and any type with no
/// arm. The arms carry NO copy of these checks (the 2026-10-02 review found
/// two homes for one rule): they consume the `ResolvedTarget` and treat a
/// target gone between here and there as the bug it would be.
///
/// 3. **Sight.** Last, once the target is resolved: the caster must see it
///    across the terrain (`Terrain::can_see`, the height model's 3.8), heals
///    and buffs included (the user's call of 2026-11-03; EQEmu exempts them).
///    Decided at the release, as EQEmu's `SpellFinished` does; a target that
///    steps behind a crest during the bar breaks the spell here, as one that
///    walks out of reach does, at no cost.
#[allow(clippy::too_many_arguments)]
pub(super) fn cast_target_refusal(
    spell: &spells::Spell,
    target_id: Option<EntityId>,
    caster_id: EntityId,
    caster_cid: ClientId,
    caster_pos: Vec3f,
    connections: &HashMap<ClientId, PerConnection>,
    enemies: &HashMap<EntityId, Entity>,
    corpses: &HashMap<EntityId, crate::world::corpses::Corpse>,
    group_manager: &GroupManager,
    terrain: &crate::world::terrain::Terrain,
) -> Result<ResolvedTarget, String> {
    let resolved = resolve_cast_target(
        spell,
        target_id,
        caster_id,
        caster_cid,
        caster_pos,
        connections,
        enemies,
        corpses,
        group_manager,
    )?;
    let target_pos = match resolved {
        ResolvedTarget::Nothing => None,
        ResolvedTarget::Player(cid) if cid == caster_cid => None,
        ResolvedTarget::Player(cid) => connections.get(&cid).map(|c| c.pos),
        ResolvedTarget::Mob(id) | ResolvedTarget::Pet(id) => enemies.get(&id).map(|e| e.pos),
        ResolvedTarget::Corpse(id, _) => corpses.get(&id).map(|c| c.pos),
    };
    if let Some(p) = target_pos {
        if !terrain.can_see(caster_pos, p) {
            return Err(CANNOT_SEE_TARGET.to_string());
        }
    }
    Ok(resolved)
}

/// EQ's line for a target the terrain hides (EQEmu's `CANT_SEE_TARGET`).
pub(super) const CANNOT_SEE_TARGET: &str = "You cannot see your target.";

/// The pre-flight's first two parts: presence, reach, liveness and the gates,
/// by target type. Sight is `cast_target_refusal`'s.
#[allow(clippy::too_many_arguments)]
fn resolve_cast_target(
    spell: &spells::Spell,
    target_id: Option<EntityId>,
    caster_id: EntityId,
    caster_cid: ClientId,
    caster_pos: Vec3f,
    connections: &HashMap<ClientId, PerConnection>,
    enemies: &HashMap<EntityId, Entity>,
    corpses: &HashMap<EntityId, crate::world::corpses::Corpse>,
    group_manager: &GroupManager,
) -> Result<ResolvedTarget, String> {
    let Some(caster) = connections.get(&caster_cid) else {
        return Err(TARGET_UNREACHABLE.to_string());
    };
    // Each spell's own reach (per-spell ranges, 2026-10-19), exact: a 30 m
    // spell is out of range at 30.1 (user, 2026-10-27), and the client checks
    // the same full reach, so a target that walks out during a cast bar costs
    // a free refusal and nothing else. Binding a group member reaches as far
    // as the bind spell does.
    let reach = spell.reach();
    let in_reach = |pos: Vec3f| pos.distance_to(caster_pos) <= reach;
    let unreachable = || Err(TARGET_UNREACHABLE.to_string());
    let no_effect = || Err(NO_EFFECT_YET.to_string());
    let player_in_reach = |cid: ClientId| {
        connections
            .get(&cid)
            .filter(|c| c.in_world && in_reach(c.pos))
    };
    let entity_in_reach =
        |id: EntityId| enemies.get(&id).filter(|e| e.is_alive() && in_reach(e.pos));
    let attackable = |other: &PerConnection| {
        combat::can_attack(caster, other, caster.zone.as_deref(), other.zone.as_deref())
    };

    match spell.target_type.as_str() {
        "ENEMY" => {
            let Some(target) = target_id else {
                return Err("You need a target for that spell.".to_string());
            };
            if IdKind::of(target) == IdKind::Player {
                // A player: the PvP path.
                let target_cid = target as ClientId;
                if target_cid == caster_cid {
                    return Err("You cannot cast that on yourself.".to_string());
                }
                let Some(t) = player_in_reach(target_cid) else {
                    return unreachable();
                };
                if !attackable(t) {
                    return Err("You cannot attack that player.".to_string());
                }
                if t.hp <= 0.0 {
                    return Err("That target is no longer here.".to_string());
                }
                return Ok(ResolvedTarget::Player(target_cid));
            }
            let Some(e) = entity_in_reach(target) else {
                return unreachable();
            };
            // Another player's pet: allowed only if its owner is attackable,
            // and never the caster's own.
            if let Some(owner_id) = e.owner {
                let owner_cid = owner_id as ClientId;
                let allowed =
                    owner_cid != caster_cid && connections.get(&owner_cid).is_some_and(attackable);
                if !allowed {
                    return Err("You cannot attack that pet.".to_string());
                }
            }
            // One attack slow at a time, strongest wins (spell batch step 1):
            // a weaker slow on a target that already carries a stronger one
            // is refused here, at no cost, rather than replacing it.
            if spell.attack_slow_amount > 0.0
                && e.attack_slow_pct() > (spell.attack_slow_amount.clamp(0.0, 1.0) * 100.0) as u8
            {
                return Err("A stronger slow already holds that target.".to_string());
            }
            if e.is_pet() {
                Ok(ResolvedTarget::Pet(target))
            } else {
                Ok(ResolvedTarget::Mob(target))
            }
        }
        "ALLY" => {
            let raw = target_id.unwrap_or(0);
            if raw == 0 || raw == caster_id {
                return Ok(ResolvedTarget::Player(caster_cid)); // self
            }
            if IdKind::of(raw) == IdKind::Pet {
                let Some(owner_id) = entity_in_reach(raw).and_then(|p| p.owner) else {
                    return unreachable();
                };
                // The PvP heal gate: no topping up a duel partner's pet,
                // unless they are a group-mate.
                let owner_cid = owner_id as ClientId;
                if owner_cid != caster_cid {
                    let hostile = connections.get(&owner_cid).is_some_and(attackable);
                    if hostile && !group_manager.same_group(caster_cid, owner_cid) {
                        return Err("You cannot heal that pet.".to_string());
                    }
                }
                return Ok(ResolvedTarget::Pet(raw));
            }
            match IdKind::of(raw) {
                IdKind::Bag => return Err("Cannot heal that target.".to_string()),
                IdKind::Enemy => return Err("Cannot heal enemies.".to_string()),
                IdKind::Player | IdKind::Pet => {}
            }
            let target_cid = raw as ClientId;
            let Some(t) = player_in_reach(target_cid) else {
                return unreachable();
            };
            if t.hp <= 0.0 {
                return Err("That target is no longer here.".to_string());
            }
            if attackable(t) && !group_manager.same_group(caster_cid, target_cid) {
                return Err("You cannot heal that player.".to_string());
            }
            Ok(ResolvedTarget::Player(target_cid))
        }
        "PET_CHARM" => {
            let Some(target) = target_id else {
                return Err("You need a target for that spell.".to_string());
            };
            if IdKind::of(target) != IdKind::Enemy {
                return Err("You cannot charm that.".to_string());
            }
            match entity_in_reach(target) {
                Some(e) if !e.is_pet() => Ok(ResolvedTarget::Mob(target)),
                _ => unreachable(),
            }
        }
        "CORPSE" => {
            let Some(corpse_id) = target_id else {
                return Err("No corpse targeted.".to_string());
            };
            let Some(corpse) = corpses.get(&corpse_id) else {
                return Err("That is not a corpse.".to_string());
            };
            if !(corpse.pos.distance_to(caster_pos) <= reach) {
                return Err("You are too far from the corpse.".to_string());
            }
            if corpse.resurrected {
                return Err("That corpse has already been resurrected.".to_string());
            }
            // The owner must be in the world to receive the offer.
            let owner_cid = corpse.owner_char as ClientId;
            if !connections.get(&owner_cid).is_some_and(|c| c.in_world) {
                return Err("Their spirit is not present.".to_string());
            }
            Ok(ResolvedTarget::Corpse(corpse_id, owner_cid))
        }
        "AOE" => {
            if spell.aoe_radius > 0.0 {
                Ok(ResolvedTarget::Nothing)
            } else {
                no_effect()
            }
        }
        "PET_SUMMON" => {
            let known = !spell.pet_type.is_empty()
                && pet_templates::scaled(&spell.pet_type, caster.level.max(1) as u32, 0).is_some();
            if known {
                Ok(ResolvedTarget::Nothing)
            } else {
                no_effect()
            }
        }
        "BIND" => {
            // Spell batch step 2 (user calls D1, D2 of 2026-10-05): a caster
            // binds THEMSELVES anywhere; a GROUP MEMBER only while that member
            // stands in a safe area. No target, a self target or a mob target
            // all mean "myself". The bind written is always the target's own
            // position, never a client value.
            let raw = target_id.unwrap_or(0);
            if raw == 0 || raw == caster_id || IdKind::of(raw) != IdKind::Player {
                return Ok(ResolvedTarget::Player(caster_cid));
            }
            let target_cid = raw as ClientId;
            if !group_manager.same_group(caster_cid, target_cid) {
                return Err("You can only bind yourself or a group member.".to_string());
            }
            let Some(t) = player_in_reach(target_cid) else {
                return unreachable();
            };
            if t.hp <= 0.0 || t.death_processed {
                return Err("That target is no longer here.".to_string());
            }
            if crate::world::safe_areas::containing(t.pos).is_none() {
                return Err(
                    "You can only bind another in a safe place, such as the town.".to_string(),
                );
            }
            Ok(ResolvedTarget::Player(target_cid))
        }
        "PORT" => {
            // Gate (port = "bind") always has somewhere to go, since every
            // character is bound from birth; a safe-area port needs one safe
            // area to exist. Anything else is a port the server has no
            // destination for (the zone ports) and costs nothing.
            match spell.port.as_str() {
                "bind" => Ok(ResolvedTarget::Nothing),
                "safe" if crate::world::safe_areas::nearest_arrival(caster_pos).is_some() => {
                    Ok(ResolvedTarget::Nothing)
                }
                _ => no_effect(),
            }
        }
        "PET_HEAL" => {
            // The caster's own pet, whatever is targeted (the spell names it).
            // Having no pet says nothing about anyone else, so it can be
            // said plainly.
            let own_pet = enemies
                .iter()
                .find(|(_, e)| e.owner == Some(caster_id) && e.is_alive());
            match own_pet {
                None => Err("You have no pet to mend.".to_string()),
                Some((id, p)) if in_reach(p.pos) => Ok(ResolvedTarget::Pet(*id)),
                Some(_) => unreachable(),
            }
        }
        "SUMMON_ITEM" => {
            // Summon Food / Drink: the item must exist and the whole batch
            // must fit, decided before the mana comes off (EQ refused a
            // summon into full bags; here it costs nothing).
            let Some(item) = items::lookup(&spell.summon_item) else {
                return no_effect();
            };
            let n = spells::summon_count(spell, &caster.class, caster.level);
            if !caster.inventory.can_accept(&[(item.path.clone(), n)]) {
                return Err("Your bags are full.".to_string());
            }
            Ok(ResolvedTarget::Nothing)
        }
        "SELF" => Ok(ResolvedTarget::Nothing),
        // No arm exists for anything else: a target type new to the data.
        _ => no_effect(),
    }
}

/// An arm found the target the pre-flight resolved a moment ago gone. The
/// pre-flight and the arm run back to back for one intent, so this cannot
/// happen; if it ever does it is a bug, logged as one, and the caster is
/// refunded and told rather than silently charged for nothing.
#[allow(clippy::too_many_arguments)]
pub(super) fn cast_resolution_lost(
    server: &mut RenetServer,
    connections: &mut HashMap<ClientId, PerConnection>,
    recipients: &[ClientId],
    caster_cid: ClientId,
    caster_id: u64,
    mana_cost: f32,
    hp_cost: f32,
    spell_name: &str,
    what: &str,
) {
    tracing::error!(
        caster = caster_id,
        spell = %spell_name,
        what,
        "cast target resolved by the pre-flight was gone at the arm; refunded"
    );
    refund_spell_cost(
        server,
        connections,
        recipients,
        caster_cid,
        caster_id,
        mana_cost,
        hp_cost,
    );
    handlers::send_refusal(server, caster_cid, TARGET_UNREACHABLE);
}

/// Tell an interrupted caster what the interrupt cost (spell batch step 3):
/// their true mana, privately, right behind the CastFail their client
/// answers with a local refund, so the bar settles on the charged value; and
/// one line saying how much. Nothing is sent when nothing was charged.
pub(super) fn tell_interrupt_charge(
    server: &mut RenetServer,
    caster_cid: ClientId,
    caster_id: u64,
    mp: f32,
    max_mp: f32,
    mana_charged: f32,
) {
    if mana_charged <= 0.0 {
        return;
    }
    handlers::fan_out_mana_update(server, &[caster_cid], caster_id, mp, max_mp);
    handlers::send_system_line(
        server,
        caster_cid,
        &format!(
            "Your concentration breaks: {} mana lost.",
            mana_charged.round() as i32
        ),
    );
}

/// Stop a bard's song: clear the state and tell them. The lingering effects
/// run out on their own (EQ's rule). Nothing happens when nothing is sung.
pub(super) fn stop_song(
    server: &mut RenetServer,
    connections: &mut HashMap<ClientId, PerConnection>,
    cid: ClientId,
) {
    let Some(c) = connections.get_mut(&cid) else {
        return;
    };
    let Some(song) = c.active_song.take() else {
        return;
    };
    c.song_next_pulse_at = None;
    let singer = c.char_id;
    handlers::send_system_line(server, cid, &format!("You stop singing {song}."));
    tracing::info!(singer, %song, "song stopped");
}

/// One pulse of a bard's song (the rework of 2026-10-20): the level-scaled
/// effects (`songs::pulse_effects`) as the ordinary buffs, each lingering
/// `SONG_LINGER_SECS` and refreshed by name on every pulse, fanned as a
/// snapshot when any landed. The ENEMY song's slow lands on the bard's
/// current target while it is a live mob in reach (out of reach, the pulse
/// is skipped; dead or gone, the song stops: EQ's invalid-target rule). A
/// PvP target is not a mob, so the song stops on one too (flagged).
#[allow(clippy::too_many_arguments)]
pub(super) fn song_pulse(
    server: &mut RenetServer,
    recipients: &[ClientId],
    connections: &mut HashMap<ClientId, PerConnection>,
    enemies: &mut HashMap<EntityId, Entity>,
    cid: ClientId,
    spell: &spells::Spell,
    now: Instant,
    terrain: &crate::world::terrain::Terrain,
) {
    let linger = crate::world::SONG_LINGER_SECS;
    let (effects, target, singer_pos) = {
        let Some(c) = connections.get(&cid) else {
            return;
        };
        (
            crate::world::songs::pulse_effects(spell, c.level, &c.class),
            c.current_target,
            c.pos,
        )
    };
    let name = spell.name.clone();
    let mut changed = false;
    if let Some(c) = connections.get_mut(&cid) {
        if effects.hot_hps > 0.0 {
            apply_buff(
                c,
                ActiveBuff::new_hot(name.clone(), effects.hot_hps, linger, now),
            );
            changed = true;
        }
        if effects.speed_mult > 1.0 {
            apply_buff(
                c,
                ActiveBuff::new_speed(name.clone(), effects.speed_mult, linger, now),
            );
            changed = true;
        }
        if effects.mp_regen_hps > 0.0 {
            apply_mp_regen_exclusive(
                c,
                ActiveBuff::new_mp_regen(name.clone(), effects.mp_regen_hps, linger, now),
            );
            changed = true;
        }
        if effects.haste > 0.0 {
            apply_buff(
                c,
                ActiveBuff::new_haste(name.clone(), effects.haste, linger, now),
            );
            changed = true;
        }
        if effects.accuracy > 0.0 || effects.crit > 0.0 {
            apply_buff(
                c,
                ActiveBuff::new_accuracy_crit(
                    name.clone(),
                    effects.accuracy,
                    effects.crit,
                    linger,
                    now,
                ),
            );
            changed = true;
        }
    }
    if changed {
        if let Some(c) = connections.get(&cid) {
            handlers::fan_out_buff_snapshot(server, recipients, c);
        }
    }
    if effects.attack_slow > 0.0 {
        let target_stands = match target.and_then(|t| enemies.get_mut(&t)) {
            Some(e) if e.is_alive() => {
                // A pulse out of reach or out of sight is skipped, the
                // song goes on (a targeted effect, the spell rule).
                if e.pos.distance_to(singer_pos) <= spell.reach()
                    && terrain.can_see(singer_pos, e.pos)
                {
                    e.apply_cc(ActiveCc::new_attack_slow(effects.attack_slow, linger));
                }
                true
            }
            _ => false,
        };
        if !target_stands {
            stop_song(server, connections, cid);
        }
    }
}

/// Refuse a cast privately and hand the caster the server's true mana. The
/// client spent its mana and started a cooldown when it sent the cast, so the
/// CastFail is its signal to undo that and the ManaUpdate the value to settle
/// on. Nobody else is told: a refusal is the caster's business, and peers'
/// copies of a refused bar run out on their own (accepted 2026-10-02). The
/// three ways a RUNNING cast ends that peers can see (an interrupt, silence,
/// mez) keep their fan to peers so the bar they drew cancels; they reach the
/// caster too. Spell batch step 0.
pub(super) fn refuse_cast(
    server: &mut RenetServer,
    connections: &HashMap<ClientId, PerConnection>,
    caster_cid: ClientId,
    caster_id: u64,
    line: &str,
) {
    handlers::fan_out_cast_fail(server, &[caster_cid], caster_id, line.to_string());
    if let Some(c) = connections.get(&caster_cid) {
        handlers::fan_out_mana_update(server, &[caster_cid], caster_id, c.mp, c.max_mp);
        // The caster's true buffs too: the client applies a self-buff at the
        // release, before the server has judged the cast, and a refusal left
        // it believing in a buff the server never held (a phantom Haste paced
        // its swings faster than the limiter allows; the 2026-10-30 review).
        // The snapshot's reconcile drops anything the server does not hold.
        handlers::fan_out_buff_snapshot(server, &[caster_cid], c);
    }
}

/// Give back a spell's cost after a cast is rejected downstream of the
/// deduction.
///
/// Mana (and any hp cost) comes off near the top of the CastSpell handler,
/// before the target-type match, so every rejection below that point used to
/// charge full price for nothing. Reporting the failure was only half the fix:
/// a 2026-08-24 playtest confirmed the player got "That target is too far away."
/// and stayed down the mana.
///
/// The unknown-spell arm higher up does NOT use this — the server never
/// deducted there, so it hands back its own untouched value instead.
#[allow(clippy::too_many_arguments)]
pub(super) fn refund_spell_cost(
    server: &mut RenetServer,
    connections: &mut HashMap<ClientId, PerConnection>,
    recipients: &[ClientId],
    caster_cid: ClientId,
    caster_id: u64,
    mana_cost: f32,
    hp_cost: f32,
) {
    let Some(cc) = connections.get_mut(&caster_cid) else {
        return;
    };
    cc.mp = (cc.mp + mana_cost).min(cc.max_mp);
    if hp_cost > 0.0 {
        cc.hp = (cc.hp + hp_cost).min(cc.max_hp);
    }
    regen::mark_dirty(cc);
    let (mp, max_mp, hp, max_hp) = (cc.mp, cc.max_mp, cc.hp, cc.max_hp);
    handlers::fan_out_mana_update(server, recipients, caster_id, mp, max_mp);
    if hp_cost > 0.0 {
        handlers::fan_out_health_update(server, recipients, caster_id, hp, max_hp);
    }
}

#[cfg(test)]
mod tests {
    // The cast pre-flight is the ONE home of target resolution: the arms act
    // on the `ResolvedTarget` it returns and carry no copy of its checks (the
    // 2026-10-02 review's finding, closed 2026-10-14). So the VARIANT it
    // returns is load-bearing, not just the accept/refuse: an ENEMY cast
    // resolving a rival's pet as a Mob or a flagged player as anything else
    // would send the arm down the wrong path, and an untargeted heal is a
    // self-heal only because ALLY resolves nothing to the caster.
    mod cast_pre_flight {
        use super::super::{cast_target_refusal, ResolvedTarget, Vec3f, TARGET_UNREACHABLE};
        use crate::world::{
            connection::PerConnection,
            corpses::Corpse,
            entity::{mint_pet_id, Entity},
            groups::GroupManager,
            pet_templates, spells,
        };
        use protocol::world::{Coins, LOOT_BAG_ID_BASE};
        use renet::ClientId;
        use std::{collections::HashMap, time::Instant};

        fn conn_at(char_id: i64, pos: Vec3f, pvp: bool) -> PerConnection {
            let spawn = crate::db::CharacterSpawn {
                hunger: 14400.0,
                thirst: 14400.0,
                alignment: None,
                factions: Vec::new(),
                char_id,
                account_id: char_id,
                name: format!("Char{char_id}"),
                race: "Human".into(),
                class: "Cleric".into(),
                level: 20,
                xp: 0,
                xp_to_next: 100,
                strength: 10,
                dexterity: 10,
                agility: 10,
                intelligence: 10,
                wisdom: 10,
                charisma: 10,
                constitution: 10,
                max_hp: 100.0,
                max_mp: 100.0,
                max_stamina: 100.0,
                hp: 100.0,
                mp: 100.0,
                stamina: 100.0,
                coins: Coins::ZERO,
                bank_coins: Coins::ZERO,
                zone: None,
                pos: (pos.x, pos.y, pos.z),
                yaw: 0.0,
                completed_quests: Vec::new(),
                active_quests: Vec::new(),
                bind: None,
            };
            let mut c = PerConnection::from_spawn(spawn, Instant::now());
            c.in_world = true;
            c.pos = pos;
            c.pvp_override_on = pvp;
            c
        }

        /// A live skeleton at `pos`; with an owner it is that owner's pet,
        /// in the pet id partition like a real one.
        fn mob_at(pos: Vec3f, owner: Option<u64>) -> Entity {
            let template = pet_templates::lookup("skeleton").expect("skeleton template");
            let mut e = Entity::from_spawn(0, pos, template, Instant::now());
            if let Some(owner_id) = owner {
                e.id = mint_pet_id();
                e.owner = Some(owner_id);
            }
            e
        }

        #[test]
        fn resolves_each_target_kind_to_the_variant_its_arm_consumes() {
            let at = |x: f32| Vec3f { x, y: 0.0, z: 0.0 };
            let caster: ClientId = 1;
            let friend: ClientId = 2;
            let rival: ClientId = 3;
            let mut connections = HashMap::new();
            connections.insert(caster, conn_at(1, at(0.0), true));
            connections.insert(friend, conn_at(2, at(5.0), false));
            connections.insert(rival, conn_at(3, at(10.0), true));

            let mut enemies = HashMap::new();
            let mut add = |e: Entity| {
                let id = e.id;
                enemies.insert(id, e);
                id
            };
            let mob_id = add(mob_at(at(12.0), None));
            let own_pet_id = add(mob_at(at(3.0), Some(caster)));
            let rival_pet_id = add(mob_at(at(14.0), Some(rival)));
            // Just past the 30 m default reach (it was 26 m when hostile spells reached 25).
            let far_mob_id = add(mob_at(at(31.0), None));

            let corpse_id = LOOT_BAG_ID_BASE + 7;
            let mut corpses = HashMap::new();
            corpses.insert(
                corpse_id,
                Corpse::new(
                    corpse_id,
                    friend as i64,
                    "Char2".into(),
                    String::new(),
                    at(8.0),
                    Vec::new(),
                    Coins::ZERO,
                    0,
                    false,
                    Instant::now(),
                ),
            );
            let groups = GroupManager::new();
            let resolve = |name: &str, target: Option<u64>| {
                let spell = spells::lookup(name).unwrap_or_else(|| panic!("{name} in spells.toml"));
                cast_target_refusal(
                    spell,
                    target,
                    caster,
                    caster,
                    at(0.0),
                    &connections,
                    &enemies,
                    &corpses,
                    &groups,
                    crate::world::terrain::compiled(),
                )
            };
            let refused = |line: &str| Err(line.to_string());

            // ENEMY: a mob, a rival's pet (its owner attackable), a flagged
            // player; never one's own pet, nothing out of reach, nothing
            // untargeted.
            assert_eq!(
                resolve("Fireball", Some(mob_id)),
                Ok(ResolvedTarget::Mob(mob_id))
            );
            assert_eq!(
                resolve("Fireball", Some(rival_pet_id)),
                Ok(ResolvedTarget::Pet(rival_pet_id))
            );
            assert_eq!(
                resolve("Fireball", Some(rival)),
                Ok(ResolvedTarget::Player(rival))
            );
            assert_eq!(
                resolve("Fireball", Some(own_pet_id)),
                refused("You cannot attack that pet.")
            );
            assert_eq!(
                resolve("Fireball", Some(far_mob_id)),
                refused(TARGET_UNREACHABLE)
            );
            assert_eq!(
                resolve("Fireball", None),
                refused("You need a target for that spell.")
            );
            // ALLY: nothing targeted (or oneself) is a self-heal; a player; a pet.
            assert_eq!(resolve("Heal", None), Ok(ResolvedTarget::Player(caster)));
            assert_eq!(
                resolve("Heal", Some(caster)),
                Ok(ResolvedTarget::Player(caster))
            );
            assert_eq!(
                resolve("Heal", Some(friend)),
                Ok(ResolvedTarget::Player(friend))
            );
            assert_eq!(
                resolve("Heal", Some(own_pet_id)),
                Ok(ResolvedTarget::Pet(own_pet_id))
            );
            // PET_HEAL: the caster's own pet, whatever the client targeted.
            assert_eq!(
                resolve("Warder's Mend", Some(mob_id)),
                Ok(ResolvedTarget::Pet(own_pet_id))
            );
            // PET_CHARM: a mob, never a pet (a pet id is outside the enemy
            // partition, so the partition check answers before the lookup).
            assert_eq!(
                resolve("Charm", Some(mob_id)),
                Ok(ResolvedTarget::Mob(mob_id))
            );
            assert_eq!(
                resolve("Charm", Some(rival_pet_id)),
                refused("You cannot charm that.")
            );
            // CORPSE: the corpse and its owner, who is in the world.
            assert_eq!(
                resolve("Resurrection (Minor)", Some(corpse_id)),
                Ok(ResolvedTarget::Corpse(corpse_id, friend))
            );
            // BIND: oneself; a group member only (none here).
            assert_eq!(
                resolve("Bind Affinity", None),
                Ok(ResolvedTarget::Player(caster))
            );
            assert_eq!(
                resolve("Bind Affinity", Some(friend)),
                refused("You can only bind yourself or a group member.")
            );
            // SELF, AOE, PET_SUMMON, PORT: nothing to resolve.
            for name in ["Breeze", "Inferno", "Summon Skeleton", "Gate"] {
                assert_eq!(resolve(name, None), Ok(ResolvedTarget::Nothing), "{name}");
            }
        }
    }
}
