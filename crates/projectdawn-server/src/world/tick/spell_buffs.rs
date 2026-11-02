//! Putting a spell's buffs on a player or a pet (the stacking and exclusivity rules), and the buff snapshots that tell the client and its peers.

use super::*;

/// Track 6 sub-task 4a — apply an active buff to a connection.
/// Re-cast of a same-named buff refreshes the duration (matches
/// `autoloads/buff_manager.gd::add_hot`'s behaviour). Caller is
/// responsible for fanning a BuffSnapshot afterwards.
pub(super) fn apply_buff(conn: &mut PerConnection, buff: ActiveBuff) {
    if let Some(existing) = conn
        .active_buffs
        .iter_mut()
        .find(|b| b.name == buff.name)
    {
        existing.effect = buff.effect;
        existing.remaining = buff.remaining;
        existing.tick_acc = 0.0;
        existing.applied_at = buff.applied_at;
    } else {
        conn.active_buffs.push(buff);
    }
}

/// Track 6 sub-task 4b — apply a stat buff, mutating the
/// connection's effective stats. Refresh semantics for re-cast: undo
/// the existing same-named buff's deltas, then apply the new ones.
/// This keeps total stat additions from doubling on refresh.
pub(super) fn apply_stat_buff(conn: &mut PerConnection, buff: ActiveBuff) {
    // Pull deltas out of the incoming buff.
    let (str_d, agi_d, int_d, wis_d, con_d, hp_d, mp_d) = match buff.effect {
        buffs::BuffEffect::StatBuff {
            strength, agility, intelligence, wisdom, constitution,
            max_hp_delta, max_mp_delta,
        } => (strength, agility, intelligence, wisdom, constitution, max_hp_delta, max_mp_delta),
        _ => return,
    };
    // If an existing same-named stat buff is present, undo its
    // deltas before removing it; preserves the invariant that
    // conn.strength etc. = base + sum(active stat buffs).
    if let Some(idx) = conn.active_buffs.iter().position(|b| b.name == buff.name) {
        if let buffs::BuffEffect::StatBuff {
            strength, agility, intelligence, wisdom, constitution,
            max_hp_delta, max_mp_delta,
        } = conn.active_buffs[idx].effect
        {
            buffs::undo_stat_deltas(
                conn, strength, agility, intelligence, wisdom, constitution,
                max_hp_delta, max_mp_delta,
            );
        }
        conn.active_buffs.remove(idx);
    }
    buffs::apply_stat_deltas(conn, str_d, agi_d, int_d, wis_d, con_d, hp_d, mp_d);
    conn.active_buffs.push(buff);
}

/// Track 6 sub-task 4a fix — apply an MP-regen buff with exclusive
/// semantics. The client's `BuffManager._mp_regen_buff` is a single
/// slot, so casting Clarity after Breeze replaces rather than stacks.
/// Mirror that here by purging any existing MpRegen entry before
/// pushing the new one. Lich Form's MP regen is a separate
/// `BuffEffect::LichForm` variant and isn't touched.
pub(super) fn apply_mp_regen_exclusive(conn: &mut PerConnection, buff: ActiveBuff) {
    conn.active_buffs
        .retain(|b| !matches!(b.effect, buffs::BuffEffect::MpRegen { .. }));
    conn.active_buffs.push(buff);
}

/// "Latest wins" exclusive apply for DamageShield (Thorns / Spellshield)
/// and Absorb (Rune / Primal Bond). The client's BuffManager tracks
/// these as single slots, so the server matches by purging any
/// existing variant of the same effect kind before pushing the new
/// buff. Keeps both renderings (local buff bar + target HUD)
/// in agreement: one shield, one absorb at a time.
pub(super) fn apply_damage_shield_exclusive(conn: &mut PerConnection, buff: ActiveBuff) {
    conn.active_buffs
        .retain(|b| !matches!(b.effect, buffs::BuffEffect::DamageShield { .. }));
    conn.active_buffs.push(buff);
}

pub(super) fn apply_absorb_exclusive(conn: &mut PerConnection, buff: ActiveBuff) {
    conn.active_buffs
        .retain(|b| !matches!(b.effect, buffs::BuffEffect::Absorb { .. }));
    conn.active_buffs.push(buff);
}

/// Apply every beneficial buff a spell grants to one recipient — the
/// caster for a SELF spell, the targeted ally for an ALLY spell. Covers
/// HoT, MP-regen, speed, haste, damage shield, absorb, accuracy/crit, and
/// primary stat buffs. Lich Form and the self heal/damage are NOT here:
/// Lich is a SELF-only toggle and the heal lands before this in each arm.
/// Returns true if the active set changed so the caller fans a
/// BuffSnapshot. `mark_dirty` is called for stat/MP changes so the next
/// regen tick fans the updated max HP/MP caps.
pub(super) fn apply_player_spell_buffs(conn: &mut PerConnection, spell: &spells::Spell, now: Instant) -> bool {
    let mut changed = false;
    if spell.hot_hps > 0.0 && spell.hot_duration > 0.0 {
        apply_buff(
            conn,
            ActiveBuff::new_hot(spell.name.clone(), spell.hot_hps, spell.hot_duration, now),
        );
        changed = true;
    }
    if spell.mp_regen_hps > 0.0 && spell.mp_regen_duration > 0.0 {
        apply_mp_regen_exclusive(
            conn,
            ActiveBuff::new_mp_regen(
                spell.name.clone(),
                spell.mp_regen_hps,
                spell.mp_regen_duration,
                now,
            ),
        );
        changed = true;
    }
    if spell.move_speed_mult > 0.0 && spell.move_speed_duration > 0.0 {
        apply_buff(
            conn,
            ActiveBuff::new_speed(
                spell.name.clone(),
                spell.move_speed_mult,
                spell.move_speed_duration,
                now,
            ),
        );
        changed = true;
    }
    if spell.haste_amount > 0.0 && spell.haste_duration > 0.0 {
        apply_buff(
            conn,
            ActiveBuff::new_haste(spell.name.clone(), spell.haste_amount, spell.haste_duration, now),
        );
        changed = true;
    }
    if spell.damage_shield_amount > 0.0 && spell.damage_shield_duration > 0.0 {
        apply_damage_shield_exclusive(
            conn,
            ActiveBuff::new_damage_shield(
                spell.name.clone(),
                spell.damage_shield_amount,
                spell.damage_shield_duration,
                now,
            ),
        );
        changed = true;
    }
    if spell.absorb_amount > 0.0 {
        apply_absorb_exclusive(
            conn,
            ActiveBuff::new_absorb(spell.name.clone(), spell.absorb_amount, now),
        );
        changed = true;
    }
    if (spell.accuracy_buff > 0.0 || spell.crit_buff > 0.0) && spell.stat_buff_duration > 0.0 {
        apply_buff(
            conn,
            ActiveBuff::new_accuracy_crit(
                spell.name.clone(),
                spell.accuracy_buff,
                spell.crit_buff,
                spell.stat_buff_duration,
                now,
            ),
        );
        changed = true;
    }
    if spell.primary_stat_buff_duration > 0.0 {
        let any_nonzero = spell.str_buff != 0
            || spell.agi_buff != 0
            || spell.int_buff != 0
            || spell.wis_buff != 0
            || spell.con_buff != 0
            || spell.max_hp_buff != 0.0
            || spell.max_mp_buff != 0.0;
        if any_nonzero {
            apply_stat_buff(
                conn,
                ActiveBuff::new_stat_buff(
                    spell.name.clone(),
                    spell.str_buff,
                    spell.agi_buff,
                    spell.int_buff,
                    spell.wis_buff,
                    spell.con_buff,
                    spell.max_hp_buff,
                    spell.max_mp_buff,
                    spell.primary_stat_buff_duration,
                    now,
                ),
            );
            // max_hp / max_mp may have changed — mark resources dirty so
            // the next regen tick fans HealthUpdate / ManaUpdate.
            regen::mark_dirty(conn);
            changed = true;
        }
    }
    changed
}

/// Track 13 — apply the pet-relevant beneficial buffs a spell grants to a
/// pet (ALLY buff cast on it). Mirrors `apply_player_spell_buffs` minus
/// the effects pets have no model for: mp regen (no mana pool),
/// accuracy/crit (no crit model), absorb (self-only spells). StatBuff
/// mutates the pet's stats + max_hp via `Entity::apply_buff`. Returns true
/// if anything was added so the caller fans a BuffSnapshot.
pub(super) fn apply_pet_spell_buffs(pet: &mut Entity, spell: &spells::Spell, now: Instant) -> bool {
    let mut changed = false;
    if spell.hot_hps > 0.0 && spell.hot_duration > 0.0 {
        pet.apply_buff(ActiveBuff::new_hot(
            spell.name.clone(), spell.hot_hps, spell.hot_duration, now,
        ));
        changed = true;
    }
    if spell.move_speed_mult > 0.0 && spell.move_speed_duration > 0.0 {
        pet.apply_buff(ActiveBuff::new_speed(
            spell.name.clone(), spell.move_speed_mult, spell.move_speed_duration, now,
        ));
        changed = true;
    }
    if spell.haste_amount > 0.0 && spell.haste_duration > 0.0 {
        pet.apply_buff(ActiveBuff::new_haste(
            spell.name.clone(), spell.haste_amount, spell.haste_duration, now,
        ));
        changed = true;
    }
    if spell.damage_shield_amount > 0.0 && spell.damage_shield_duration > 0.0 {
        pet.apply_buff(ActiveBuff::new_damage_shield(
            spell.name.clone(), spell.damage_shield_amount, spell.damage_shield_duration, now,
        ));
        changed = true;
    }
    if spell.primary_stat_buff_duration > 0.0 {
        let any_nonzero = spell.str_buff != 0 || spell.agi_buff != 0 || spell.int_buff != 0
            || spell.wis_buff != 0 || spell.con_buff != 0 || spell.max_hp_buff != 0.0;
        if any_nonzero {
            pet.apply_buff(ActiveBuff::new_stat_buff(
                spell.name.clone(), spell.str_buff, spell.agi_buff, spell.int_buff,
                spell.wis_buff, spell.con_buff, spell.max_hp_buff, spell.max_mp_buff,
                spell.primary_stat_buff_duration, now,
            ));
            changed = true;
        }
    }
    changed
}

/// Track 6 sub-task 4a — rebuild conn.buff_snapshot from
/// active_buffs and fan a BuffSnapshot to in-world peers. Server is
/// authoritative on buff state now; the client-driven
/// BuffSnapshotBroadcast path is deprecated (kept as a no-op for one
/// release so transitional builds don't crash on the variant).
pub(super) fn fan_out_server_buff_snapshot(
    server: &mut renet::RenetServer,
    recipients: &[renet::ClientId],
    conn: &PerConnection,
) {
    let payload = buffs::snapshot_pairs(&conn.active_buffs);
    let msg = protocol::world::ServerWorldMsg::BuffSnapshot {
        target: conn.char_id as u64,
        buffs: payload,
    };
    let Ok(bytes) = bincode::serde::encode_to_vec(&msg, bincode::config::standard()) else {
        return;
    };
    for &recipient in recipients {
        server.send_message(recipient, CHANNEL_SYSTEM, bytes.clone());
    }
}

/// Track 13 — fan a BuffSnapshot for a pet's or a mob's status under its
/// id, so the owner + bystanders render its buff bar (the client routes
/// the snapshot by id partition). Mirror of `fan_out_server_buff_snapshot`.
/// Since spell batch step 5 the payload is `Entity::status_pairs`: buffs,
/// DoTs and crowd control, so a mob's target frame shows what is on it.
pub(super) fn fan_out_pet_buff_snapshot(
    server: &mut renet::RenetServer,
    recipients: &[renet::ClientId],
    pet: &Entity,
) {
    let payload = pet.status_pairs();
    let msg = protocol::world::ServerWorldMsg::BuffSnapshot {
        target: pet.id,
        buffs: payload,
    };
    let Ok(bytes) = bincode::serde::encode_to_vec(&msg, bincode::config::standard()) else {
        return;
    };
    for &recipient in recipients {
        server.send_message(recipient, CHANNEL_SYSTEM, bytes.clone());
    }
}