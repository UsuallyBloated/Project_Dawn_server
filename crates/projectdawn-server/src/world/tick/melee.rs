//! Melee (steps 4g, the active skills, and 4h): skill uses and auto-attack swings, paced
//! by the swing-rate limit (`swing`) and resolved by `combat`.

use super::*;

pub(super) fn apply_active_skills(world: &mut WorldState, q: &mut Intents, now: Instant, in_world_recipients_now: &[ClientId]) {
    let WorldState { server, connections, enemies, .. } = world;
    let Intents { attack_intents, use_skill_intents, .. } = q;
    // 4g. Active skills (client-authority audit item 6). The handler
    //     checked the skill, the class, the crowd control, the cooldown
    //     and the stamina; here the target is checked, the stamina and
    //     the cooldown are charged, and then a SELF skill's effect is
    //     applied or a damage skill becomes a swing for step 4h with the
    //     skill's multiplier and effect riding it. A refusal costs nothing
    //     and answers with SkillFail, so the client drops the cooldown it
    //     started.
    if !use_skill_intents.is_empty() {
        for intent in use_skill_intents.drain(..) {
            let user_cid = intent.user as ClientId;
            let Some(skill) = active_skills::lookup(&intent.skill_name) else { continue };
            let Some(user) = connections.get(&user_cid) else { continue };
            // A dead user uses nothing (as a dead caster casts nothing).
            if !user.in_world || user.hp <= 0.0 || user.death_processed {
                continue;
            }
            // Checked again against this tick's state: two UseSkills in
            // one datagram passed the handler on the same stamina.
            if active_skills::on_cooldown(user.skill_cooldowns.get(&skill.name).copied(), now) {
                handlers::send_skill_fail(&mut *server, user_cid, &skill.name, "You cannot use that yet.");
                continue;
            }
            if user.stamina < skill.stamina_cost {
                handlers::send_skill_fail(&mut *server, user_cid, &skill.name, "You are too tired.");
                continue;
            }
            let user_pos = user.pos;
            let user_zone = user.zone.clone();
            let effect = skill.effect();

            if skill.is_damage() {
                // The reach of the main-hand weapon, as the swing it
                // becomes will check again (step 4h fans a Miss for a
                // target out of reach; the skill refuses before any cost).
                let weapon_path = user.equipped_weapon_path(false);
                let ranged = items::lookup(&weapon_path).is_some_and(|w| w.is_ranged);
                let Some(target_id) = intent.target_id else {
                    handlers::send_skill_fail(&mut *server, user_cid, &skill.name, "You have no target.");
                    continue;
                };
                // Presence, liveness and reach share ONE line, so an id
                // probed from across the world learns nothing (the
                // 2026-10-02 rule: no refusal is an is-online or
                // PvP-flag oracle); the PvP gate answers only for a
                // target in reach, which is on the user's screen anyway.
                // Pets inherit their owner's flag, as for a plain swing.
                const NOT_HERE: &str = "That target is not here.";
                let check: Result<(), &str> = if IdKind::of(target_id) == IdKind::Player {
                    if target_id == intent.user {
                        Err("You cannot use that on yourself.")
                    } else {
                        let allowed = if ranged { RANGED_ATTACK_RANGE } else { 3.0 * ATTACK_RANGE_TOLERANCE };
                        match connections.get(&(target_id as ClientId)) {
                            Some(t) if t.in_world && t.hp > 0.0 && t.pos.distance_to(user_pos) <= allowed => {
                                if combat::can_attack(user, t, user_zone.as_deref(), t.zone.as_deref()) {
                                    Ok(())
                                } else {
                                    Err("Unable to attack that target.")
                                }
                            }
                            _ => Err(NOT_HERE),
                        }
                    }
                } else {
                    match enemies.get(&target_id) {
                        Some(e) if e.is_alive() => {
                            let allowed = if ranged { RANGED_ATTACK_RANGE } else { e.melee_range() * ATTACK_RANGE_TOLERANCE };
                            if e.pos.distance_to(user_pos) > allowed {
                                Err(NOT_HERE)
                            } else {
                                match e.owner {
                                    None => Ok(()),
                                    Some(owner) if owner as ClientId == user_cid => Err("You cannot attack your own pet."),
                                    Some(owner) => match connections.get(&(owner as ClientId)) {
                                        Some(o) if combat::can_attack(user, o, user_zone.as_deref(), o.zone.as_deref()) => Ok(()),
                                        _ => Err("Unable to attack that target."),
                                    },
                                }
                            }
                        }
                        _ => Err(NOT_HERE),
                    }
                };
                if let Err(line) = check {
                    handlers::send_skill_fail(&mut *server, user_cid, &skill.name, line);
                    continue;
                }
            }

            // A SELF skill whose effect still holds is refused, not
            // refreshed: a Holy Shield re-pressed on cooldown would refill
            // its pool for 10 stamina (the pool lasts until spent, as the
            // absorb spells do), and a second Hide while hidden buys
            // nothing.
            let holding: Option<&str> = match effect {
                active_skills::Effect::AbsorbShield
                    if user.active_buffs.iter().any(|b| matches!(b.effect, buffs::BuffEffect::Absorb { pool } if pool > 0.0)) =>
                {
                    Some("Your shield still holds.")
                }
                active_skills::Effect::Stealth if buffs::is_hidden(&user.active_buffs) => Some("You are already hidden."),
                _ => None,
            };
            if let Some(line) = holding {
                handlers::send_skill_fail(&mut *server, user_cid, &skill.name, line);
                continue;
            }

            // Charged: the stamina (mark_dirty has the regen loop fan it,
            // a spend past 5% of the bar on the next tick, so a
            // group-mate's panel moves with it), the cooldown, and the
            // user stands. A damage skill also marks them in combat; a
            // Hide does not.
            if let Some(u) = connections.get_mut(&user_cid) {
                u.stamina = (u.stamina - skill.stamina_cost).max(0.0);
                regen::mark_dirty(u);
                u.skill_cooldowns.insert(skill.name.clone(), now + skill.cooldown_duration());
                u.is_sitting = false;
                if skill.is_damage() {
                    u.last_attack_at = Some(now);
                }
            }
            tracing::info!(user = intent.user, skill = %skill.name, cost = skill.stamina_cost, "skill used");

            if skill.is_damage() {
                attack_intents.push(AttackIntent {
                    attacker: intent.user,
                    target_id: intent.target_id.unwrap_or(0),
                    is_offhand: false,
                    dmg_type: DamageType::Physical,
                    skill: Some(SkillSwing {
                        mult: skill.damage_multiplier,
                        effect,
                        stun_duration: skill.stun_duration,
                    }),
                });
                continue;
            }

            // SELF skills: a buff on the user, or a feign death.
            let mut snapshot_changed = false;
            match effect {
                active_skills::Effect::EvadeBoost => {
                    if let Some(u) = connections.get_mut(&user_cid) {
                        u.active_buffs.retain(|b| b.name != skill.name);
                        u.active_buffs.push(ActiveBuff::new_evade_all(skill.name.clone(), skill.effect_duration, now));
                        snapshot_changed = true;
                    }
                }
                active_skills::Effect::AbsorbShield => {
                    if let Some(u) = connections.get_mut(&user_cid) {
                        u.active_buffs.retain(|b| b.name != skill.name);
                        u.active_buffs.push(ActiveBuff::new_absorb(skill.name.clone(), skill.absorb_amount, now));
                        snapshot_changed = true;
                    }
                }
                active_skills::Effect::Stealth => {
                    if let Some(u) = connections.get_mut(&user_cid) {
                        u.active_buffs.retain(|b| b.name != skill.name);
                        u.active_buffs.push(ActiveBuff::new_stealth(skill.name.clone(), skill.stealth_duration, now));
                        snapshot_changed = true;
                    }
                    handlers::send_system_line(&mut *server, user_cid, "You have hidden yourself from view.");
                }
                active_skills::Effect::FeignDeath => {
                    let roll: f32 = rand::thread_rng().gen();
                    if active_skills::feign_death_succeeds(roll) {
                        // Every mob forgets the user (their DoTs end with
                        // their hate, so a feigned player is paid nothing
                        // by what they left ticking), and the mobs that
                        // lost their target are fanned as such.
                        for mob_id in wipe_hate(&mut *enemies, &[intent.user]) {
                            handlers::fan_out_entity_target(&mut *server, &in_world_recipients_now, mob_id, None);
                        }
                        // And they lie unnoticed until they stand (any
                        // movement), swing, use a skill or cast at a mob,
                        // or two minutes pass; otherwise a mob with an
                        // aggro radius would turn straight back on them.
                        if let Some(u) = connections.get_mut(&user_cid) {
                            u.active_buffs.retain(|b| !matches!(b.effect, buffs::BuffEffect::Feigned));
                            u.active_buffs.push(ActiveBuff::new_feigned(
                                "Feigned Death".to_string(),
                                active_skills::FEIGN_DEATH_HOLD_SECS,
                                now,
                            ));
                            snapshot_changed = true;
                        }
                        handlers::send_system_line(&mut *server, user_cid, "You feign death successfully.");
                        tracing::info!(user = intent.user, "feign death succeeded");
                    } else {
                        handlers::send_system_line(&mut *server, user_cid, "Your feign death fails!");
                        tracing::info!(user = intent.user, "feign death failed");
                    }
                }
                _ => {}
            }
            if snapshot_changed {
                if let Some(u) = connections.get(&user_cid) {
                    handlers::fan_out_buff_snapshot(&mut *server, &in_world_recipients_now, u);
                }
            }
        }
    }
}

pub(super) fn apply_attacks(world: &mut WorldState, q: &mut Intents, now: Instant, in_world_recipients_now: &[ClientId]) {
    let WorldState { server, connections, aoi, enemies, loot_bags, group_manager, .. } = world;
    let Intents { attack_intents, pet_command_intents, .. } = q;
    // 4h. Apply player → server attack intents. The handler queued
    //     these without touching the enemies map; here we run the
    //     server-authoritative damage formula against the attacker's
    //     PerConnection + weapon path, validate the target (alive,
    //     in range), apply damage, and fan out Hit/Miss + HealthUpdate
    //     + (if HP hit zero) EntityDied. A range mismatch or dead
    //     target produces a Miss broadcast so the attacker sees
    //     their swing landed even if cheaty.
    if !attack_intents.is_empty() && !in_world_recipients_now.is_empty() {
        for intent in attack_intents.drain(..) {
            // ClientId is renet's u64 alias and we minted it as char_id,
            // so the attacker's char_id (also u64 on the wire) is the
            // map key directly.
            let attacker_cid = intent.attacker as ClientId;
            // A swing forces the attacker to STAND (you can't fight seated)
            // and marks them in combat, so the seated regen bonus stops (see
            // regen::sitting_bonus_applies). Closes the attack-while-seated
            // behaviour + the latent free-in-combat-regen exploit.
            if let Some(a) = connections.get_mut(&attacker_cid) {
                a.is_sitting = false;
                a.last_attack_at = Some(now);
            }
            let Some(attacker_conn) = connections.get(&attacker_cid) else {
                // Attacker disconnected between sending and apply.
                continue;
            };
            let attacker_pos = attacker_conn.pos;
            let attacker_zone = attacker_conn.zone.clone();
            // Phase 1 exploit gate — weapon_path trust (audit finding 5). The
            // Attack message carries the weapon the client CLAIMS to swing; a
            // modified client could name a heavier weapon (more damage), a
            // ranged one (more reach), or one that trains a skill it hasn't
            // equipped. Ignore the wire field and read the SERVER's equipment
            // map: slot 0 = main hand, slot 1 = off hand (protocol EquipSlot
            // order, items.rs). Equipping only ever happens through the
            // server-side EquipItem intent, so this map is authoritative and
            // in sync for anything actually worn. An empty slot is an unarmed
            // swing — items::lookup("") falls through to the 1-4 fist /
            // hand_to_hand path in calc_swing + the skill lookup below, so
            // bare-handed combat still works. is_offhand now only selects the
            // slot, so a client can't fabricate an off-hand swing it isn't
            // geared for (empty slot 1 -> fists, not the main-hand weapon).
            let server_weapon_path: String =
                attacker_conn.equipped_weapon_path(intent.is_offhand);
            // Phase 1 exploit gate — off-hand swings require an actual off-hand
            // weapon. A stock client only sends is_offhand=true when
            // dual-wielding a real slot-1 weapon (combat.gd `_is_dual_wielding`);
            // an off-hand Attack with an EMPTY slot 1 is a forged free second
            // damage stream (it would otherwise resolve to a 1-4 fist swing x the
            // off-hand mult). A bare hand is not an off-hand weapon — reject it.
            // Dual wield is earned (user, 2026-10-19): no off-hand swing
            // before the class's level (skills::dual_wield_level), and
            // never for a class without it. A Monk fights with both
            // fists, so with BOTH hands empty a Monk's off hand punches;
            // everyone else still needs a real off-hand weapon. And no
            // off hand swings beside a two-handed main hand (2026-10-29;
            // the equip refuses the pairing, this is the backstop). One
            // home for the rule: skills::offhand_swing_refusal.
            if intent.is_offhand {
                if let Some(why) = skills::offhand_swing_refusal(
                    &attacker_conn.class,
                    attacker_conn.level,
                    &attacker_conn.equipped_weapon_path(false),
                    &server_weapon_path,
                ) {
                    tracing::info!(
                        attacker = intent.attacker,
                        class = %attacker_conn.class,
                        level = attacker_conn.level,
                        "Attack rejected — off-hand swing {why}"
                    );
                    continue;
                }
            }
            // Phase 1 exploit gate — melee swing-rate limit. No swing timer
            // existed, so a modified client could spam Attack for an
            // attack-speed hack. Player auto-attack is client-paced, so the
            // server floors how fast the SAME hand may swing. Keyed PER HAND
            // (main = slot 0, off = slot 1) because dual-wield emits two
            // independent Attack streams that legitimately co-fire in one tick
            // — a single combined timer would false-throttle a legit
            // dual-wielder. The client sends an Attack for every swing (the
            // server rolls the miss, combat.rs) and has no burst / catch-up
            // (combat.gd), so arrivals are already >= its swing interval; we
            // only reject "too fast", never "too slow".
            // Dropped silently (no Miss fan-out — that would reward the spammer
            // and desync their local timer). The timestamp advances only on an
            // ACCEPTED swing (below, after calc_swing), so a spammer can't walk
            // the window forward with rejected swings. The pace is the
            // player's own: the weapon and the haste the server holds for
            // them (the client-authority audit's Phase 4), not the
            // fully-hasted allowance everyone used to get.
            let hand = intent.is_offhand as usize;
            let base_delay = items::lookup(&server_weapon_path)
                .map(|w| w.weapon_delay)
                .unwrap_or(FIST_WEAPON_DELAY);
            let haste_now = attacker_conn.limiter_haste(now);
            let haste = carried_haste(haste_now, attacker_conn.last_swing_haste[hand]);
            let min_interval =
                min_swing_interval_secs(honest_swing_pace(base_delay, intent.is_offhand, haste));
            // A skill's swing is outside the auto-attack limiter: its own
            // cooldown paces it (step 4g charged it), and it neither reads
            // nor advances the hand's timer.
            if intent.skill.is_none() && swing_too_fast(attacker_conn.last_swing_at[hand], now, min_interval) {
                tracing::info!(
                    attacker = intent.attacker,
                    is_offhand = intent.is_offhand,
                    weapon_delay = base_delay,
                    haste,
                    min_interval,
                    "Attack rejected — swing-rate limit (too fast)"
                );
                continue;
            }
            // Track 6 sub-task 2: server computes the damage roll.
            // Client-supplied amount is ignored — even a malicious
            // client can't claim 999 damage anymore.
            let swing = combat::calc_swing(
                attacker_conn,
                &server_weapon_path,
                intent.is_offhand,
            );
            // An active skill's swing: the ordinary roll times its
            // multiplier; Truesight Strike never misses.
            let swing = skill_swing(swing, intent.skill.as_ref());
            // Swing accepted at a legal cadence — advance THIS hand's timer.
            // Stamped here on cadence-acceptance (before the range / hit / PvP
            // resolution below) because the client paces by swing attempt, not
            // by landed hit; the attacker_conn immutable borrow ends at
            // calc_swing above, so this get_mut is clear.
            if intent.skill.is_none() {
                if let Some(a) = connections.get_mut(&attacker_cid) {
                    a.last_swing_at[hand] = Some(now);
                    stamp_swing_haste(&mut a.last_swing_haste[hand], haste_now);
                }
            }
            // The first swing from the shadows ends a Hide, skill or not.
            let stealth_broken = connections
                .get_mut(&attacker_cid)
                .is_some_and(|a| buffs::break_stealth(&mut a.active_buffs));
            if stealth_broken {
                if let Some(a) = connections.get(&attacker_cid) {
                    handlers::fan_out_buff_snapshot(&mut *server, &in_world_recipients_now, a);
                }
            }

            // Track 6 sub-task 3: player-target branch (PvP). The
            // attack-id partition has player char_ids below
            // ENEMY_ID_BASE; anything in that range is a peer. We
            // resolve, gate via combat::can_attack (which today
            // requires both sides flipped /pvp on), apply HP delta,
            // and fan Hit + HealthUpdate. Self-attack guarded;
            // dying via PvP routes through the regular client-side
            // PlayerDeath flow (which fires DeathBroadcast on its
            // own) until sub-task 4 lifts death detection server-
            // authoritative.
            if IdKind::of(intent.target_id) == IdKind::Player {
                if intent.target_id == intent.attacker {
                    continue;
                }
                let target_cid = intent.target_id as ClientId;
                let target_zone = connections
                    .get(&target_cid)
                    .and_then(|c| c.zone.clone());
                let allowed_pvp = match (
                    connections.get(&attacker_cid),
                    connections.get(&target_cid),
                ) {
                    (Some(a), Some(t)) => combat::can_attack(
                        a, t,
                        attacker_zone.as_deref(),
                        target_zone.as_deref(),
                    ),
                    _ => false,
                };
                let target_in_range_alive = connections
                    .get(&target_cid)
                    .map(|t| {
                        t.in_world
                            && t.hp > 0.0
                            && t.pos.distance_to(attacker_pos) <= match items::lookup(
                                &server_weapon_path,
                            ) {
                                Some(w) if w.is_ranged => RANGED_ATTACK_RANGE,
                                _ => 3.0 * ATTACK_RANGE_TOLERANCE,
                            }
                    })
                    .unwrap_or(false);
                if !allowed_pvp {
                    // Named only when in reach and alive (on the
                    // attacker's screen anyway); a far or absent id gets
                    // the generic line, so a swing at an id is no
                    // char-id-to-name map (review, 2026-10-19).
                    let name = connections
                        .get(&target_cid)
                        .filter(|_| target_in_range_alive)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| "that target".to_string());
                    handlers::fan_out_chat_message(
                        &mut *server,
                        &[attacker_cid],
                        "",
                        protocol::world::ChatChannel::System,
                        &format!("Unable to attack {}.", name),
                    );
                    tracing::debug!(
                        attacker = intent.attacker,
                        target = intent.target_id,
                        "PvP not authorized"
                    );
                    continue;
                }
                if !target_in_range_alive {
                    handlers::fan_out_miss(
                        &mut *server,
                        &in_world_recipients_now,
                        intent.attacker,
                        intent.target_id,
                    );
                    continue;
                }
                // Evade (active skills): every melee swing at the bearer
                // is deflected while it holds, except Truesight Strike's.
                let target_evading = connections
                    .get(&target_cid)
                    .is_some_and(|t| buffs::is_evading(&t.active_buffs));
                let truesight = intent.skill.as_ref().is_some_and(|s| s.effect == active_skills::Effect::Truesight);
                if target_evading && !truesight {
                    handlers::fan_out_evade(&mut *server, &in_world_recipients_now, intent.attacker, intent.target_id);
                    continue;
                }
                // Seated-combat rule (classic EQ, user call 2026-10-14):
                // a sitting target takes the top of the attacker's band
                // with no armor; the hit then stands them
                // (note_damage_taken). Decided here, after the gates, so
                // a refused swing never re-rolls.
                let target_seated = connections
                    .get(&target_cid)
                    .is_some_and(|t| t.is_sitting);
                let swing = if target_seated {
                    connections
                        .get(&attacker_cid)
                        .map(|a| skill_swing(combat::calc_swing_max(a, &server_weapon_path, intent.is_offhand), intent.skill.as_ref()))
                        .unwrap_or(swing)
                } else {
                    swing
                };
                // Warder's Fury commands the warder whether the blow lands
                // or not (the /pet attack path PvP-gates it again).
                if intent.skill.as_ref().is_some_and(|s| s.effect == active_skills::Effect::WarderFury) {
                    pet_command_intents.push(PetCommandI {
                        owner: intent.attacker,
                        command: protocol::world::pet_command::ATTACK,
                        target_id: Some(intent.target_id),
                    });
                }
                // The server's miss roll (combat.rs); a seated target is
                // never missed.
                if swing.missed {
                    handlers::fan_out_miss(
                        &mut *server,
                        &in_world_recipients_now,
                        intent.attacker,
                        intent.target_id,
                    );
                    continue;
                }
                // Apply damage. Armor reduction matches the
                // GDScript Combat.receive_player_damage:
                //   reduction = armor / (armor + ARMOR_DR_DIVISOR)
                // with ARMOR_DR_DIVISOR = 100 (`combat::mitigated`).
                // Track 6 sub-task 4c: absorb pool consumed before HP
                // deduction; damage shield reflects damage back to
                // attacker after.
                let raw_swing = swing.amount;
                let shield_to_attacker_pvp: f32;
                let shield_name_pvp: Option<String>;
                let mut absorb_strip_pvp: Option<usize> = None;
                let (new_hp, max_hp, amount, target_armor) = {
                    let target_conn = connections.get_mut(&target_cid).expect("checked");
                    let mut amount =
                        combat::mitigated(swing.amount, target_conn.equipped_armor, target_seated);
                    let (after_absorb, exhausted) =
                        buffs::consume_absorb(&mut target_conn.active_buffs, amount);
                    amount = after_absorb;
                    if exhausted {
                        absorb_strip_pvp = target_conn.active_buffs.iter().position(|b| {
                            matches!(b.effect, buffs::BuffEffect::Absorb { .. })
                        });
                    }
                    shield_to_attacker_pvp =
                        buffs::damage_shield_total(&target_conn.active_buffs);
                    shield_name_pvp = buffs::first_damage_shield_name(&target_conn.active_buffs).map(|s| s.to_string());
                    target_conn.hp = (target_conn.hp - amount as f32).max(0.0);
                    regen::mark_dirty(target_conn);
                    target_conn.note_damage_taken(now); // camp breaks on damage
                    (target_conn.hp, target_conn.max_hp, amount, target_conn.equipped_armor)
                };
                // Strip exhausted absorb buff + fan snapshot.
                if let Some(idx) = absorb_strip_pvp {
                    if let Some(tc) = connections.get_mut(&target_cid) {
                        if idx < tc.active_buffs.len() {
                            tc.active_buffs.remove(idx);
                        }
                    }
                    if let Some(tc) = connections.get(&target_cid) {
                        handlers::fan_out_buff_snapshot(
                            &mut *server,
                            &in_world_recipients_now,
                            tc,
                        );
                    }
                }
                // Damage shield reflects damage to the attacker
                // (also a player here). Skip if the attacker has
                // since disconnected.
                if shield_to_attacker_pvp > 0.0 {
                    if let Some(att) = connections.get_mut(&attacker_cid) {
                        if att.hp > 0.0 {
                            let dmg = shield_to_attacker_pvp;
                            let reflect_dmg = dmg as i32;
                            att.hp = (att.hp - dmg).max(0.0);
                            regen::mark_dirty(att);
                            att.note_damage_taken(now); // camp breaks on damage
                            let new_att_hp = att.hp;
                            let att_max = att.max_hp;
                            handlers::fan_out_health_update(
                                &mut *server,
                                &in_world_recipients_now,
                                intent.attacker,
                                new_att_hp,
                                att_max,
                            );
                            if let Some(name) = shield_name_pvp.as_ref() {
                                handlers::fan_out_damage_shield_trigger(
                                    &mut *server,
                                    &in_world_recipients_now,
                                    intent.target_id,
                                    intent.attacker,
                                    reflect_dmg,
                                    name.clone(),
                                );
                            }
                            // Track 22.E — reflected damage also
                            // interrupts an in-flight cast. Classic
                            // EQ: any incoming damage rolls the
                            // channeling check, regardless of source.
                            let outcome = roll_cast_interrupt(att, now, reflect_dmg);
                            let (att_mp, att_max_mp) = (att.mp, att.max_mp);
                            match &outcome {
                                InterruptOutcome::Interrupted { spell_name, mana_charged } => {
                                    handlers::fan_out_cast_fail(
                                        &mut *server,
                                        &in_world_recipients_now,
                                        intent.attacker,
                                        "interrupted (hit during cast)".to_string(),
                                    );
                                    tell_interrupt_charge(&mut *server, attacker_cid, intent.attacker, att_mp, att_max_mp, *mana_charged);
                                    tracing::info!(
                                        caster = intent.attacker,
                                        spell = %spell_name,
                                        mana_charged,
                                        "PvP cast interrupted by damage shield reflect"
                                    );
                                }
                                InterruptOutcome::Survived {
                                    advanced_to: Some(new_score),
                                } => {
                                    handlers::send_skill_progress_update(
                                        &mut *server,
                                        attacker_cid,
                                        skills::Skill::Casting.as_protocol(),
                                        "channeling".to_string(),
                                        *new_score,
                                    );
                                }
                                InterruptOutcome::Survived { advanced_to: None }
                                | InterruptOutcome::NotCasting => {}
                            }
                        }
                    }
                }
                tracing::info!(
                    attacker = intent.attacker,
                    target = intent.target_id,
                    raw_swing,
                    target_armor,
                    applied = amount,
                    target_hp = new_hp,
                    "PvP attack applied"
                );
                // Track 19A — PvP hit can interrupt a cast too.
                // Re-borrow target_conn now that the damage block
                // has released it; same helper as the enemy arm.
                if let Some(tc) = connections.get_mut(&target_cid) {
                    let interrupt_outcome = roll_cast_interrupt(tc, now, amount as i32);
                    let (tc_mp, tc_max_mp) = (tc.mp, tc.max_mp);
                    match &interrupt_outcome {
                        InterruptOutcome::Interrupted { spell_name, mana_charged } => {
                            handlers::fan_out_cast_fail(
                                &mut *server,
                                &in_world_recipients_now,
                                intent.target_id,
                                "interrupted (hit during cast)".to_string(),
                            );
                            tell_interrupt_charge(&mut *server, target_cid, intent.target_id, tc_mp, tc_max_mp, *mana_charged);
                            tracing::info!(
                                caster = intent.target_id,
                                spell = %spell_name,
                                mana_charged,
                                "PvP cast interrupted by incoming damage"
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
                }
                handlers::fan_out_hit(
                    &mut *server,
                    &in_world_recipients_now,
                    intent.attacker,
                    intent.target_id,
                    amount,
                    swing.crit,
                    intent.dmg_type,
                );
                handlers::fan_out_health_update(
                    &mut *server,
                    &in_world_recipients_now,
                    intent.target_id,
                    new_hp,
                    max_hp,
                );
                // The skill's effect on a landed PvP hit: Shield Bash
                // stuns the player (no move, swing, cast or skill; the
                // name shows on their frame).
                if let Some(sk) = intent.skill.as_ref() {
                    if sk.effect == active_skills::Effect::Stun {
                        let mut interrupted: Option<String> = None;
                        if let Some(tc) = connections.get_mut(&target_cid) {
                            tc.active_buffs.retain(|b| !matches!(b.effect, buffs::BuffEffect::Stun));
                            tc.active_buffs.push(ActiveBuff::new_stun("Stunned".to_string(), sk.stun_duration, now));
                            // A stun ends a running cast outright (the
                            // hit's own channeling roll may have let it
                            // through); no mana charge, since a stun is
                            // not damage.
                            if !tc.cast_spell_name.is_empty() {
                                interrupted = Some(tc.cast_spell_name.clone());
                                tc.cast_spell_name.clear();
                                tc.cast_total_duration = 0.0;
                                tc.cast_set_at = None;
                                tc.cast_interrupted_at = Some(now);
                            }
                        }
                        if let Some(spell) = interrupted {
                            handlers::fan_out_cast_fail(
                                &mut *server,
                                &in_world_recipients_now,
                                intent.target_id,
                                "interrupted (stunned)".to_string(),
                            );
                            tracing::info!(caster = intent.target_id, spell = %spell, "PvP cast interrupted by a stun");
                        }
                        if let Some(tc) = connections.get(&target_cid) {
                            handlers::fan_out_buff_snapshot(&mut *server, &in_world_recipients_now, tc);
                        }
                    }
                }
                continue;
            }

            // Pet PvP gate. Pets inherit their owner's PvP flag —
            // a player can't damage another player's pet unless
            // can_attack between the players would already allow
            // a direct hit. NPC-owned pets (none today, but
            // charm/PetSummon paths exist) and regular mobs have
            // `entity.owner == None` and fall through. Look up
            // owner first via an immutable borrow so the
            // mutable damage borrow below remains valid.
            if IdKind::of(intent.target_id) == IdKind::Pet {
                let pet_owner = enemies.get(&intent.target_id).and_then(|e| e.owner);
                if let Some(owner_id) = pet_owner {
                    let owner_cid = owner_id as ClientId;
                    let allowed = if owner_cid == attacker_cid {
                        false  // attacking your own pet — defensive guard
                    } else {
                        match (
                            connections.get(&attacker_cid),
                            connections.get(&owner_cid),
                        ) {
                            (Some(a), Some(o)) => combat::can_attack(
                                a, o,
                                a.zone.as_deref(),
                                o.zone.as_deref(),
                            ),
                            _ => false,
                        }
                    };
                    if !allowed {
                        // Resolve "Owner's PetName." for the chat
                        // line. Owner name lives on the connection;
                        // pet display name lives on the Entity's mob
                        // template. Either may be missing if the
                        // owner just disconnected or the pet's
                        // template lookup is degraded — fall back
                        // to a generic message in that case.
                        let owner_name = connections
                            .get(&(owner_id as ClientId))
                            .map(|c| c.name.clone())
                            .unwrap_or_default();
                        let pet_name = enemies
                            .get(&intent.target_id)
                            .map(|p| p.mob.name.clone())
                            .unwrap_or_default();
                        let line = if !owner_name.is_empty() && !pet_name.is_empty() {
                            format!("Unable to attack {}'s {}.", owner_name, pet_name)
                        } else if !pet_name.is_empty() {
                            format!("Unable to attack the {}.", pet_name)
                        } else {
                            "Unable to attack that target.".to_string()
                        };
                        handlers::fan_out_chat_message(
                            &mut *server,
                            &[attacker_cid],
                            "",
                            protocol::world::ChatChannel::System,
                            &line,
                        );
                        tracing::debug!(
                            attacker = intent.attacker,
                            target = intent.target_id,
                            owner = owner_id,
                            "PvP not authorized vs pet"
                        );
                        continue;
                    }
                }
            }
            let Some(entity) = enemies.get_mut(&intent.target_id) else {
                handlers::fan_out_miss(
                    &mut *server,
                    &in_world_recipients_now,
                    intent.attacker,
                    intent.target_id,
                );
                continue;
            };
            if !entity.is_alive() {
                handlers::fan_out_miss(
                    &mut *server,
                    &in_world_recipients_now,
                    intent.attacker,
                    intent.target_id,
                );
                continue;
            }
            let dist = entity.pos.distance_to(attacker_pos);
            // Track 6 sub-task 2 (fix): ranged weapons use a much
            // larger range. Without this branch, bows at >2.7m
            // produce silent Miss broadcasts even though the swing
            // visually fired. Lookup is by weapon_path; an empty or
            // unknown path uses the melee envelope.
            let allowed = match items::lookup(&server_weapon_path) {
                Some(w) if w.is_ranged => RANGED_ATTACK_RANGE,
                _ => entity.melee_range() * ATTACK_RANGE_TOLERANCE,
            };
            if dist > allowed {
                tracing::debug!(
                    attacker = intent.attacker,
                    target = intent.target_id,
                    dist,
                    allowed,
                    weapon = %server_weapon_path,
                    "attack out of range, fanning Miss"
                );
                handlers::fan_out_miss(
                    &mut *server,
                    &in_world_recipients_now,
                    intent.attacker,
                    intent.target_id,
                );
                continue;
            }
            // Warder's Fury commands the warder at the target whether the
            // user's own blow lands or not (the command is not the blow).
            if intent.skill.as_ref().is_some_and(|s| s.effect == active_skills::Effect::WarderFury) {
                pet_command_intents.push(PetCommandI {
                    owner: intent.attacker,
                    command: protocol::world::pet_command::ATTACK,
                    target_id: Some(intent.target_id),
                });
            }
            // The server's miss roll (combat.rs). A miss still angers the
            // mob (one point of hate, as any swing at it does in EQ),
            // keeps the pet's assist target, and trains the weapon skill
            // the client used to train on every swing; it deals nothing.
            if swing.missed {
                *entity.aggro.entry(intent.attacker).or_insert(0.0) += 1.0;
                *entity.threat.entry(intent.attacker).or_insert(0.0) += 1.0;
                handlers::fan_out_miss(
                    &mut *server,
                    &in_world_recipients_now,
                    intent.attacker,
                    intent.target_id,
                );
                if let Some(att) = connections.get_mut(&attacker_cid) {
                    att.last_attacked_enemy = Some(intent.target_id);
                    att.last_attacked_at = Some(now);
                }
                train_swing_skills(
                    &mut *server,
                    &mut *connections,
                    attacker_cid,
                    &combat::weapon_skill_key(&server_weapon_path),
                    intent.is_offhand,
                );
                continue;
            }
            let amount = swing.amount.max(0);
            entity.hp = (entity.hp - amount as f32).max(0.0);
            *entity.aggro.entry(intent.attacker).or_insert(0.0) += amount as f32;
            // Track 12 Piece A2 — also tracks per-actual-attacker
            // threat for the AI re-target check. Mirrors aggro
            // for a player attacker (no pet remapping here).
            *entity.threat.entry(intent.attacker).or_insert(0.0) += amount as f32;
            // Track 11.3 — record the attacker's last hit on an
            // enemy so their pet (if any) can inherit the target
            // on its next AI tick. Decayed by the pet's AI
            // resolution step; refreshed on every swing.
            let target_for_pet = intent.target_id;
            if let Some(att) = connections.get_mut(&attacker_cid) {
                att.last_attacked_enemy = Some(target_for_pet);
                att.last_attacked_at = Some(now);
            }
            // Track 18.1: the weapon skill (and defense) advance on every
            // swing at a mob, hit or miss (`train_swing_skills`).
            train_swing_skills(
                &mut *server,
                &mut *connections,
                attacker_cid,
                &combat::weapon_skill_key(&server_weapon_path),
                intent.is_offhand,
            );
            // The skill's effect on a landed hit: Shield Bash stuns the
            // mob (frozen like mez, but damage does not wake it; the AI
            // sweep fans "Stunned" on its frame).
            if let Some(sk) = intent.skill.as_ref() {
                if sk.effect == active_skills::Effect::Stun {
                    entity.apply_cc(ActiveCc::new_stun(sk.stun_duration));
                    tracing::info!(attacker = intent.attacker, target = intent.target_id,
                        secs = sk.stun_duration, "mob stunned");
                }
            }
            // PD_W0025 — server-authoritative weapon proc. Roll the equipped
            // weapon's proc_chance on this landed swing; on a proc, fold the
            // proc_damage into THIS swing's resolution (same entity borrow +
            // aggro, and the death/loot/xp block below), so a proc killing
            // blow is credited/looted correctly with no duplicate cascade, and
            // announce it via ProcTriggered so the client renders the named
            // "<proc> for N" hit. Only fires if the main swing didn't already
            // kill (no proc-on-a-corpse). Flat 5% proc crit (1.5-2.0x) mirrors
            // the old client roll; no elemental resist (the server has no
            // enemy-resist model — enemies take raw damage). This replaces the
            // old client-sent proc Attack (which double-hit and is now dropped
            // by the swing-rate limit).
            if entity.hp > 0.0 {
                if let Some(w) = items::lookup(&server_weapon_path) {
                    if w.proc_chance > 0.0 && w.proc_damage > 0 {
                        let mut rng = rand::thread_rng();
                        if rng.gen::<f32>() < w.proc_chance {
                            let proc_crit = rng.gen::<f32>() < 0.05;
                            let proc_dmg = (if proc_crit {
                                (w.proc_damage as f32 * rng.gen_range(1.5..=2.0)) as i32
                            } else {
                                w.proc_damage
                            })
                            .max(1);
                            entity.hp = (entity.hp - proc_dmg as f32).max(0.0);
                            *entity.aggro.entry(intent.attacker).or_insert(0.0) +=
                                proc_dmg as f32;
                            *entity.threat.entry(intent.attacker).or_insert(0.0) +=
                                proc_dmg as f32;
                            // Private to the attacker — the proc's named
                            // number/flash/log is their flavor; the mob's HP
                            // drop is already fanned to everyone via the
                            // HealthUpdate below.
                            handlers::fan_out_proc_triggered(
                                &mut *server,
                                &[attacker_cid],
                                intent.attacker,
                                intent.target_id,
                                w.proc_name.clone(),
                                proc_dmg,
                                proc_crit,
                                spells::proc_damage_type_to_wire(w.proc_damage_type),
                            );
                        }
                    }
                }
            }
            handlers::fan_out_hit(
                &mut *server,
                &in_world_recipients_now,
                intent.attacker,
                intent.target_id,
                amount,
                swing.crit,
                intent.dmg_type,
            );
            handlers::fan_out_health_update(
                &mut *server,
                &in_world_recipients_now,
                entity.id,
                entity.hp,
                entity.max_hp,
            );
            if entity.hp <= 0.0 {
                entity.transition(EnemyState::Dead, now);
                handlers::fan_out_entity_died(
                    &mut *server,
                    &in_world_recipients_now,
                    entity.id,
                );
                // Warder respawn — if a player kill (PvP or
                // accidental) drops a Beast Master warder, schedule
                // the same retreat-and-respawn the enemy-kill path
                // at line ~5011 uses. Without this the pet stays
                // dead until the owner re-zones.
                if crate::world::pet_templates::is_warder_template(&entity.mob.name) {
                    if let Some(owner_id) = entity.owner {
                        const WARDER_RETREAT_SECS: f32 = 15.0;
                        let due = now
                            + std::time::Duration::from_secs_f32(WARDER_RETREAT_SECS);
                        let owner_cid = owner_id as ClientId;
                        if let Some(conn) = connections.get_mut(&owner_cid) {
                            conn.warder_respawn_at = Some(due);
                            tracing::info!(
                                owner = owner_cid as u64,
                                killer = intent.attacker,
                                retreat_secs = WARDER_RETREAT_SECS,
                                "warder retreating after player kill",
                            );
                        }
                    }
                }
                // Kill credit: pick the top damager from the aggro
                // table and send a private XpGained. Solo-only
                // semantics — the legacy enet GroupManager path
                // splits XP locally and is out of scope for the
                // server's renet view. HashMap iteration order is
                // non-deterministic, so max_by with the partial_cmp
                // tiebreak is stable enough for the single-attacker
                // case (only one entry).
                if let Some((&credit_id, _)) = entity
                    .aggro
                    .iter()
                    .max_by(|a, b| {
                        a.1.partial_cmp(b.1)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                {
                    // EQ quadratic per-kill award from the mob's level (see
                    // progression::kill_xp), split across the killer's
                    // group + quest credit via the shared helper (same
                    // path as spell and pet kills).
                    let base_xp = crate::world::progression::kill_xp(entity.mob.level as i32, crate::world::progression::ZEM_NORMAL);
                    let mob_name = entity.mob.name.clone();
                    // Owned victims (pets / charmed mobs) grant no quest
                    // credit — the warder is literally named "Wolf".
                    let victim_owned = entity.owner.is_some();
                    let victim_pos = entity.pos;
                    let faction_kill = FactionKill::of(entity);
                    award_kill(
                        &mut *server,
                        &mut *connections,
                        &group_manager,
                        credit_id,
                        base_xp,
                        &mob_name,
                        victim_owned,
                        victim_pos,
                        faction_kill,
                    );
                }
                // Roll loot from the mob's archetype table; spawn
                // a server-owned bag at the death pos if any
                // stacks landed. Empty rolls produce no bag at all
                // (matches the GDScript behaviour where the local
                // Loot autoload simply returns without instantiating
                // a node).
                // Loot ownership: the top damager (kill-creditor) owns
                // the corpse; their group shares rights, resolved at
                // loot time (see loot::LootBag::can_loot). Filter to the
                // player id range — if a pet/enemy was the top damager
                // the bag falls back to public rather than locking out.
                let owner_cid_opt: Option<ClientId> = entity
                    .aggro
                    .iter()
                    .max_by(|a, b| {
                        a.1.partial_cmp(b.1)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(&id, _)| id)
                    .filter(|&id| IdKind::of(id) == IdKind::Player)
                    .map(|id| id as ClientId);
                // An owned victim (a pet, a charmed mob) drops nothing,
                // as it pays no XP: a warder is "Wolf" with a wolf's
                // table and returns free every 15 s, so two flagged
                // accounts could farm its bag (review of spell batch
                // step 5; this path had always rolled it).
                let owned_victim = entity.owner.is_some();
                let loot_items = if owned_victim {
                    Vec::new()
                } else {
                    loot::roll_for_mob(&entity.mob.name).unwrap_or_default()
                };
                let loot_coins = if owned_victim {
                    protocol::world::Coins::ZERO
                } else {
                    loot::roll_coin_for_mob(&entity.mob.name, entity.mob.level)
                };
                if !loot_items.is_empty() || loot_coins != protocol::world::Coins::ZERO {
                    let stacks_for_log = loot_items.len();
                    let bag = LootBag::new(
                        entity.pos,
                        loot_items,
                        loot_coins,
                        entity.mob.name.clone(),
                        owner_cid_opt,
                        now,
                    );
                    let bag_id = bag.id;
                    // Track 7: add bag to AOI; fan LootBagSpawn only
                    // to players who can see the bag's cell.
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
                        mob = %entity.mob.name,
                        bag_id,
                        stacks = stacks_for_log,
                        "loot bag spawned"
                    );
                }
            }
        }
    }
}
