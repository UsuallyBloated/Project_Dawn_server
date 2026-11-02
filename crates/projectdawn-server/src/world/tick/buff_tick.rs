//! Buffs over time (steps 4s, 5a and 5a-pet): a bard's active song pulses, then every
//! player's and pet's heal-over-time, mana regen, damage over time and expiry, with the
//! snapshots that tell the clients.

use super::*;

pub(super) fn run_buff_tick(world: &mut WorldState, _q: &mut Intents, now: Instant, dt: f32, in_world_recipients_now: &[ClientId]) {
    let WorldState { server, connections, enemies, .. } = world;
    // 4s. Bard song pulses (the rework of 2026-10-20): every singer whose
    //     next pulse is due gets the song's effects again, the ordinary
    //     buffs lingering SONG_LINGER_SECS, and the ENEMY song's slow on
    //     the bard's current target. A dead or absent singer sings
    //     nothing (death clears the song; a reap drops the connection).
    let due_singers: Vec<(ClientId, String)> = connections
        .iter()
        .filter(|(_, c)| c.ready && c.in_world && c.hp > 0.0 && !c.death_processed)
        .filter(|(_, c)| c.song_next_pulse_at.is_some_and(|t| now >= t))
        .filter_map(|(id, c)| c.active_song.clone().map(|s| (*id, s)))
        .collect();
    for (cid, song) in due_singers {
        let Some(spell) = spells::lookup(&song) else { continue };
        if let Some(c) = connections.get_mut(&cid) {
            c.song_next_pulse_at = Some(now + Duration::from_secs_f32(crate::world::SONG_PULSE_SECS));
        }
        song_pulse(&mut *server, &in_world_recipients_now, &mut *connections, &mut *enemies, cid, spell, now);
    }

    // 5a. Track 6 sub-task 4a buff tick — process HoT / MP regen
    //     for each connection's active_buffs. Decrements
    //     remaining, applies per-tick effects, removes expired
    //     buffs, and fans BuffSnapshot when the set changes. Runs
    //     BEFORE regen so HoT increments land in the same tick as
    //     the regen-driven HealthUpdate fan-out — one ManaUpdate
    //     / HealthUpdate per affected resource per tick at most.
    let mut buff_snapshot_dirty: Vec<u64> = Vec::new();
    // Whose resurrection sickness wore off this tick: they get EQ's line.
    let mut res_sickness_ended: Vec<ClientId> = Vec::new();
    // PvP DoT ticks dealt this pass: (bearer, caster, spell, amount,
    // dmg_type, hp after, max_hp), fanned once the borrow ends.
    let mut pvp_dot_ticks: Vec<(u64, u64, String, i32, DamageType, f32, f32)> = Vec::new();
    for conn in connections.values_mut().filter(|c| c.ready) {
        let mut snapshot_changed = false;
        let mut i = 0;
        while i < conn.active_buffs.len() {
            let buff = &mut conn.active_buffs[i];
            if buff.remaining.is_finite() {
                buff.remaining -= dt;
            }
            let expired = buff.remaining <= 0.0 && buff.remaining.is_finite();
            match buff.effect {
                buffs::BuffEffect::Hot { hps } => {
                    if !expired && hps > 0.0 && conn.hp < conn.max_hp {
                        buff.tick_acc += hps * dt;
                        if buff.tick_acc >= 1.0 {
                            let heal = buff.tick_acc.floor();
                            buff.tick_acc -= heal;
                            conn.hp = (conn.hp + heal).min(conn.max_hp);
                            regen::mark_dirty(conn);
                        }
                    }
                }
                // A PvP DoT (spell batch step 5): whole ticks of dps x
                // DOT_TICK_SECS, the same cadence as a mob's. The hit
                // and the caster's line fan after the loop; the death
                // sweep below handles a bearer brought to 0.
                // Ticks are counted off the elapsed time (`tick_acc`
                // holds the count), so the last one lands on the frame
                // the buff expires, whichever accumulator crosses first.
                buffs::BuffEffect::Dot { dps, caster, dmg_type, duration } => {
                    if dps > 0.0 && conn.hp > 0.0 && !conn.death_processed {
                        let total = (duration / crate::world::DOT_TICK_SECS).floor();
                        let elapsed = (duration - buff.remaining.max(0.0)).max(0.0);
                        let due = (elapsed / crate::world::DOT_TICK_SECS).floor().min(total);
                        if buff.tick_acc < due {
                            buff.tick_acc += 1.0;
                            let amount = (dps * crate::world::DOT_TICK_SECS).round().max(0.0) as i32;
                            // The buff borrow ends here; the bearer is
                            // mutated below.
                            let name = buff.name.clone();
                            if amount > 0 {
                                conn.hp = (conn.hp - amount as f32).max(0.0);
                                conn.note_damage_taken(now);
                                regen::mark_dirty(conn);
                                pvp_dot_ticks.push((
                                    conn.char_id as u64,
                                    caster,
                                    name,
                                    amount,
                                    dmg_type,
                                    conn.hp,
                                    conn.max_hp,
                                ));
                            }
                        }
                    }
                }
                buffs::BuffEffect::MpRegen { mps } => {
                    if !expired && mps > 0.0 && conn.mp < conn.max_mp {
                        buff.tick_acc += mps * dt;
                        if buff.tick_acc >= 1.0 {
                            let gain = buff.tick_acc.floor();
                            buff.tick_acc -= gain;
                            conn.mp = (conn.mp + gain).min(conn.max_mp);
                            regen::mark_dirty(conn);
                        }
                    }
                }
                buffs::BuffEffect::LichForm { lich_mp_regen } => {
                    // Lich Form is a passive toggle — regen.rs
                    // skips natural HP regen when this is
                    // present, and we add the MP/sec here.
                    if !expired && lich_mp_regen > 0.0 && conn.mp < conn.max_mp {
                        buff.tick_acc += lich_mp_regen * dt;
                        if buff.tick_acc >= 1.0 {
                            let gain = buff.tick_acc.floor();
                            buff.tick_acc -= gain;
                            conn.mp = (conn.mp + gain).min(conn.max_mp);
                            regen::mark_dirty(conn);
                        }
                    }
                }
                buffs::BuffEffect::StatBuff { .. } => {
                    // Stat buffs are duration-only — no per-tick
                    // effect. Deltas were applied at cast time
                    // (apply_stat_buff); the un-apply happens
                    // below in the expire branch.
                }
                buffs::BuffEffect::Speed { .. }
                | buffs::BuffEffect::Haste { .. }
                | buffs::BuffEffect::DamageShield { .. }
                | buffs::BuffEffect::AccuracyCrit { .. }
                | buffs::BuffEffect::Mez
                | buffs::BuffEffect::Root
                | buffs::BuffEffect::Snare { .. }
                | buffs::BuffEffect::AttackSlow { .. }
                | buffs::BuffEffect::Silence
                | buffs::BuffEffect::EvadeAll
                | buffs::BuffEffect::Stealth
                | buffs::BuffEffect::Stun
                | buffs::BuffEffect::Feigned => {
                    // Track 6 sub-task 4c/4d — duration-only
                    // buffs. Effects applied at read sites
                    // (movement integration / damage-shield
                    // path / calc_swing / intent gating); the
                    // tick just decrements remaining.
                }
                buffs::BuffEffect::Absorb { pool } => {
                    // Absorb's "duration" is infinite by design
                    // (consumed by damage, not by time). If the
                    // pool reached zero via consume_absorb in
                    // step 4h / 4ha, the expire branch below
                    // would have removed it. This match arm is
                    // a safety check — if a stale entry with
                    // pool <= 0 lingers, force-expire it here.
                    if pool <= 0.0 {
                        // Force the buff to expire by zeroing
                        // its remaining. Next iteration removes.
                        buff.remaining = 0.0;
                        // Avoid the is_finite gate failing the
                        // expired check.
                    }
                }
            }
            if expired {
                // Track 6 sub-task 4b — stat buffs need their
                // deltas undone before the entry is removed.
                // Other effect kinds were already accounted for
                // by the tick body.
                if let buffs::BuffEffect::StatBuff {
                    strength, agility, intelligence, wisdom, constitution,
                    max_hp_delta, max_mp_delta,
                } = conn.active_buffs[i].effect
                {
                    buffs::undo_stat_deltas(
                        conn,
                        strength, agility, intelligence, wisdom, constitution,
                        max_hp_delta, max_mp_delta,
                    );
                    regen::mark_dirty(conn);
                }
                if conn.active_buffs[i].name == buffs::RES_SICKNESS_NAME {
                    res_sickness_ended.push(conn.char_id as ClientId);
                }
                // The swing-rate limit keeps counting an expired Haste
                // for a moment, until the client has heard of it.
                if matches!(conn.active_buffs[i].effect, buffs::BuffEffect::Haste { .. }) {
                    conn.haste_lapsed = Some((conn.haste_total(), now));
                }
                conn.active_buffs.remove(i);
                snapshot_changed = true;
            } else {
                i += 1;
            }
        }
        if snapshot_changed {
            buff_snapshot_dirty.push(conn.char_id as u64);
        }
    }
    // PvP DoT ticks: everyone sees the hit and the bar, the caster
    // alone gets the named line (spell batch step 5).
    if !pvp_dot_ticks.is_empty() {
        let recipients: Vec<ClientId> = connections
            .iter()
            .filter(|(_, c)| c.in_world)
            .map(|(id, _)| *id)
            .collect();
        for (bearer, caster, name, amount, dmg_type, hp_after, max_hp) in pvp_dot_ticks {
            handlers::fan_out_hit(&mut *server, &recipients, caster, bearer, amount, false, dmg_type);
            handlers::fan_out_health_update(&mut *server, &recipients, bearer, hp_after, max_hp);
            let caster_cid = caster as ClientId;
            if connections.get(&caster_cid).is_some_and(|c| c.in_world) {
                handlers::fan_out_proc_triggered(
                    &mut *server,
                    &[caster_cid],
                    caster,
                    bearer,
                    name,
                    amount,
                    false,
                    dmg_type,
                );
            }
        }
    }
    // Fan BuffSnapshot for any connection whose buff set changed
    // this tick (expirations only — applies fanned inline at the
    // cast site). Collect first to drop the mut borrow before
    // re-borrowing immutably.
    if !buff_snapshot_dirty.is_empty() {
        let recipients: Vec<ClientId> = connections
            .iter()
            .filter(|(_, c)| c.in_world)
            .map(|(id, _)| *id)
            .collect();
        for id in buff_snapshot_dirty {
            if let Some(conn) = connections.get(&(id as ClientId)) {
                fan_out_server_buff_snapshot(&mut *server, &recipients, conn);
            }
        }
        for cid in res_sickness_ended {
            handlers::fan_out_chat_message(
                &mut *server,
                &[cid],
                "",
                protocol::world::ChatChannel::System,
                "You feel your strength return.",
            );
        }
    }

    // 5a-pet. Track 13 — pet buff tick. HoT heal + stat-buff expiry
    // for each pet's active_buffs. Collect the changed pets first, then
    // fan HealthUpdate / BuffSnapshot once the mut borrow drops.
    let mut pet_buff_changed: Vec<(EntityId, bool, bool)> = Vec::new();
    for entity in enemies.values_mut() {
        if entity.active_buffs.is_empty() {
            continue;
        }
        let (hp_changed, set_changed) = entity.tick_buffs(dt);
        if hp_changed || set_changed {
            pet_buff_changed.push((entity.id, hp_changed, set_changed));
        }
    }
    if !pet_buff_changed.is_empty() {
        let recipients: Vec<ClientId> = connections
            .iter()
            .filter(|(_, c)| c.in_world)
            .map(|(id, _)| *id)
            .collect();
        for (pet_id, hp_changed, set_changed) in pet_buff_changed {
            if let Some(pet) = enemies.get(&pet_id) {
                if hp_changed {
                    handlers::fan_out_health_update(
                        &mut *server, &recipients, pet_id, pet.hp, pet.max_hp,
                    );
                }
                if set_changed {
                    fan_out_pet_buff_snapshot(&mut *server, &recipients, pet);
                }
            }
        }
    }
}
