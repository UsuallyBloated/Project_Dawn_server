//! Spell casts (step 4ha): each CastSpell judged and applied by its target type. The
//! rules it is judged by (the target pre-flight, refusals, interrupts, songs) live in
//! `cast_rules`.

use super::*;

pub(super) fn apply_casts(world: &mut WorldState, q: &mut Intents, now: Instant, in_world_recipients_now: &[ClientId]) {
    let WorldState { server, connections, aoi, enemies, loot_bags, group_manager, corpses, .. } = world;
    let Intents { cast_spell_intents, bind_intents, .. } = q;
    // 4ha. Apply player → server spell-cast intents. Server resolves
    //      the spell in spells.toml, validates the cast-time gate
    //      (Track 10) + mana cost + target, and applies
    //      authoritative damage (ENEMY/AOE) or heal (SELF/ALLY).
    if !cast_spell_intents.is_empty() {
        for intent in cast_spell_intents.drain(..) {
            let caster_cid = intent.caster as ClientId;
            let Some(spell) = spells::lookup(&intent.spell_name) else {
                tracing::info!(
                    char_id = intent.caster,
                    spell = %intent.spell_name,
                    "unknown spell name — server-side cast dropped"
                );
                // The client spends mana, starts the cooldown and applies
                // local buffs BEFORE it sends the cast, so a spell the server
                // has never heard of drains the bar and does nothing at all.
                // Roughly 32 spells exist client-side but not in spells.toml,
                // so this fires in ordinary play and reads as the spell being
                // broken rather than missing.
                //
                // The server never deducted anything here, so its own mana is
                // still correct: sending it back corrects the client's
                // optimistic spend. Same principle as correcting a rejected
                // MoveItem's slots — answer with the truth.
                if let Some(conn) = connections.get(&caster_cid) {
                    let (id, mp, max_mp) = (conn.char_id as u64, conn.mp, conn.max_mp);
                    handlers::fan_out_mana_update(&mut *server, &[caster_cid], id, mp, max_mp);
                }
                handlers::send_refusal(
                    &mut *server,
                    caster_cid,
                    "That spell fizzles — it isn't known here yet.",
                );
                continue;
            };
            // Spell batch step 0: a corpse casts nothing. The death sweep
            // runs after this step in the tick, so a cast whose bar ended
            // in the tick the caster's hp hit zero used to complete: a
            // self-heal would have revived them past the death penalty,
            // and Gate would have moved the corpse.
            if connections
                .get(&caster_cid)
                .map_or(true, |c| c.hp <= 0.0 || c.death_processed)
            {
                tracing::info!(
                    char_id = intent.caster,
                    spell = %spell.name,
                    "CastSpell rejected — caster is dead"
                );
                refuse_cast(&mut *server, &connections, caster_cid, intent.caster, "You are dead.");
                continue;
            }
            // Phase 1 exploit gate — class/level eligibility. Any cast that
            // reaches here names a real spell; verify the caster's class is in
            // the spell's `classes` and the caster meets `min_level`. A legit
            // client only offers spells your class knows, so this rejects a
            // forged CastSpell (the audit's "any class can cast any spell it can
            // name"). Checked before the mana / cooldown / skill side effects so
            // a rejected forgery costs nothing to retry against. Every spell in
            // spells.toml has a non-empty `classes` and a `min_level`, so no
            // legit spell trips this on missing data. This is the one home of
            // the rule: the CORPSE arm no longer keeps a copy.
            {
                // The class the caster casts as (alignment.rs): a Paladin
                // at Evil has the Fallen kit and not the Paladin's.
                let caster_cl = connections
                    .get(&caster_cid)
                    .map(|c| (c.effective_class(), c.level));
                let class_ok = caster_cl
                    .as_ref()
                    .map(|(class, _)| spell.classes.iter().any(|cl| cl == class))
                    .unwrap_or(false);
                let level_ok = caster_cl
                    .as_ref()
                    .map(|(class, level)| *level >= spell.required_level(class))
                    .unwrap_or(false);
                if !(class_ok && level_ok) {
                    let (class, level) = caster_cl.unwrap_or_default();
                    let reason = if !class_ok {
                        "Your class cannot cast that."
                    } else {
                        "You are not high enough level for that spell."
                    };
                    tracing::info!(
                        char_id = intent.caster,
                        spell = %spell.name,
                        class = %class,
                        level,
                        required_level = spell.required_level(&class),
                        class_ok,
                        "CastSpell rejected — class/level not eligible"
                    );
                    refuse_cast(&mut *server, &connections, caster_cid, intent.caster, reason);
                    continue;
                }
            }
            // The memorized bar (client-authority audit fix 5): a spell in
            // no gem is refused before anything is spent. Until the server
            // held the bar, only the client knew what was memorized.
            let memorized = connections
                .get(&caster_cid)
                .is_some_and(|c| crate::world::spell_gems::is_memorized(&c.spell_gems, &spell.name));
            if !memorized {
                tracing::info!(
                    char_id = intent.caster,
                    spell = %spell.name,
                    "CastSpell rejected — spell not memorized"
                );
                refuse_cast(&mut *server, &connections, caster_cid, intent.caster, "You have not memorized that spell.");
                continue;
            }
            // Track 10 — cast-time gate. Instant casts (cast_time
            // == 0) skip; for timed casts we require a matching
            // CastStartBroadcast on file with enough wall time
            // elapsed. The 100 ms tolerance absorbs network jitter
            // — packet loss can push arrival past the cast bar
            // ending, but a forged CastSpell sent the same tick as
            // CastStart will fall well short.
            if spell.cast_time > 0.0 {
                use crate::world::cast_gate::{self, CastTiming};
                let required_ms = (spell.cast_time * 1000.0) as u128;
                let name_matches = intent.cast_name_at_dispatch == spell.name;
                let elapsed_ms = intent
                    .cast_set_at_at_dispatch
                    .map(|set_at| now.duration_since(set_at).as_millis());
                let timing = if name_matches {
                    cast_gate::cast_timing(elapsed_ms, required_ms)
                } else {
                    CastTiming::NotReady
                };
                match timing {
                    CastTiming::Ready => {}
                    CastTiming::NotReady => {
                        tracing::info!(
                            char_id = intent.caster,
                            spell = %spell.name,
                            cast_time = spell.cast_time,
                            in_flight = %intent.cast_name_at_dispatch,
                            "CastSpell rejected — cast-time gate (no matching CastStart or too early)"
                        );
                        refuse_cast(&mut *server, &connections, caster_cid, intent.caster, "cast not ready");
                        continue;
                    }
                    CastTiming::Held => {
                        // Spell batch step 0: a finished bar kept in hand
                        // and released later (a pre-cast heal fired the
                        // instant it is needed) is refused, and the stale
                        // bar is dropped so the next cast starts clean.
                        tracing::info!(
                            char_id = intent.caster,
                            spell = %spell.name,
                            elapsed_ms = elapsed_ms.unwrap_or(0),
                            "CastSpell rejected — held past the bar"
                        );
                        if let Some(cc) = connections.get_mut(&caster_cid) {
                            cc.cast_spell_name.clear();
                            cc.cast_total_duration = 0.0;
                            cc.cast_set_at = None;
                        }
                        refuse_cast(&mut *server, &connections, caster_cid, intent.caster, "That cast has lapsed.");
                        continue;
                    }
                }
                // Spell batch step 0: an interrupt that landed after this
                // bar began beats the cast, even in the same tick. The
                // gate above reads the cache as it was at dispatch, which
                // also meant a hit processed earlier in this tick had
                // already cleared the live cache and fanned "interrupted",
                // and the cast still went through: both won.
                let interrupted_at = connections
                    .get(&caster_cid)
                    .and_then(|c| c.cast_interrupted_at);
                if cast_gate::beaten_by_interrupt(interrupted_at, intent.cast_set_at_at_dispatch) {
                    tracing::info!(
                        char_id = intent.caster,
                        spell = %spell.name,
                        "CastSpell rejected — interrupted after the bar began"
                    );
                    refuse_cast(&mut *server, &connections, caster_cid, intent.caster, "interrupted (hit during cast)");
                    continue;
                }
                // Track 17.2 — movement-during-cast gate. Compare
                // current caster pos to the snapshot taken at
                // CastStart. >MAX_CAST_MOVE_DISTANCE cancels the
                // cast.
                //
                // CORRECTED 2026-08-25: an earlier version of this
                // comment claimed "the client cancels its own cast on
                // movement already". It does not — the 08-24 audit
                // found no client-side movement cancel at all (the
                // interrupt path early-returns in launcher mode, and no
                // keybind/ESC/cancel-button exists). This gate is the
                // ONLY line of defence, for honest and forged clients
                // alike, so do not weaken it on the assumption there is
                // a client backstop.
                //
                // Threshold is generous (5 m, vs ~4 m max coast
                // from STALE_MOVE_THRESHOLD) so a player who
                // stopped just before the cast doesn't get pinged
                // for the half-second of server-side drift their
                // last Move authorized. A continuously-moving
                // forged client easily clears the gate during a
                // multi-second cast.
                const MAX_CAST_MOVE_DISTANCE: f32 = 5.0;
                if let Some(caster_conn_now) = connections.get(&caster_cid) {
                    let dx = caster_conn_now.pos.x - intent.cast_start_pos_at_dispatch.x;
                    let dz = caster_conn_now.pos.z - intent.cast_start_pos_at_dispatch.z;
                    let moved = (dx * dx + dz * dz).sqrt();
                    // A song is sung on the move (Selos' is a running
                    // song; EQ never interrupted a song for movement),
                    // so the gate passes songs through.
                    if moved > MAX_CAST_MOVE_DISTANCE && !spell.is_song {
                        tracing::info!(
                            char_id = intent.caster,
                            spell = %spell.name,
                            moved,
                            "CastSpell rejected — moved during cast"
                        );
                        // Clear cast cache so the next cast isn't
                        // gated by this stale in-flight state.
                        if let Some(cc) = connections.get_mut(&caster_cid) {
                            cc.cast_spell_name.clear();
                            cc.cast_total_duration = 0.0;
                            cc.cast_set_at = None;
                        }
                        refuse_cast(&mut *server, &connections, caster_cid, intent.caster, "interrupted (moved)");
                        continue;
                    }
                }
            }
            // Track 17.2 — per-player per-spell cooldown gate.
            // Independent of cast-time (an instant cast still
            // honours the spell's cooldown). Cleanup of expired
            // entries happens lazily on the next lookup.
            if let Some(caster_conn_now) = connections.get(&caster_cid) {
                if let Some(ready_at) = caster_conn_now.spell_cooldowns.get(&spell.name) {
                    if now < *ready_at {
                        let remaining = ready_at.duration_since(now).as_secs_f32();
                        tracing::info!(
                            char_id = intent.caster,
                            spell = %spell.name,
                            remaining_secs = remaining,
                            "CastSpell rejected — on cooldown"
                        );
                        refuse_cast(&mut *server, &connections, caster_cid, intent.caster, "Spell is on cooldown.");
                        continue;
                    }
                }
                // The global cooldown (spell batch step 4). A timed cast
                // was already refused at its CastStart; this is the
                // instant cast's gate, and the backstop for a forged
                // CastSpell with no bar. Songs are exempt.
                if !spell.is_song {
                    if let Some(until) = caster_conn_now.gcd_until {
                        if now < until {
                            tracing::info!(
                                char_id = intent.caster,
                                spell = %spell.name,
                                remaining_ms = until.duration_since(now).as_millis(),
                                "CastSpell rejected — inside the global cooldown"
                            );
                            refuse_cast(&mut *server, &connections, caster_cid, intent.caster, "You cannot cast again yet.");
                            continue;
                        }
                    }
                }
            }
            // Resolve caster's snapshot (immutable) — we need pos
            // for range / AOE. mp deduction lands later under a
            // mutable borrow.
            let Some(caster_conn) = connections.get(&caster_cid) else {
                continue;
            };
            if caster_conn.mp < spell.mana_cost {
                tracing::info!(
                    char_id = intent.caster,
                    spell = %spell.name,
                    mp = caster_conn.mp,
                    cost = spell.mana_cost,
                    "spell cast rejected — insufficient mana"
                );
                // Tell the caster so they see "Cast failed: Not enough
                // mana." instead of a silent no-op behind the optimistic
                // local "You cast X" line, with the mana they really have.
                refuse_cast(&mut *server, &connections, caster_cid, intent.caster, "Not enough mana.");
                continue;
            }
            let caster_pos = caster_conn.pos;
            let caster_max_mp = caster_conn.max_mp;
            let (caster_mp_now, caster_hp_now, caster_max_hp_now) =
                (caster_conn.mp, caster_conn.hp, caster_conn.max_hp);
            let mana_cost = spell.mana_cost;
            let hp_cost = spell.hp_cost;
            let dmg_type = spells::parse_damage_type(&spell.damage_type);
            // A spell does exactly its listed number, as in classic EQ
            // (the parity audit of 2026-10-20); the caster's INT feeds
            // only the crit roll (spell_power.rs). The one scaling left
            // is the off-side penalty on a Paladin's or Shadow Knight's
            // spells (alignment.rs), applied once here so every arm
            // below reads the penalised numbers.
            let caster_int = caster_conn.intelligence;
            let side = crate::world::alignment::off_side_multiplier(&caster_conn.class, caster_conn.alignment);
            let penalised;
            let spell = if side < 1.0 {
                penalised = crate::world::alignment::penalise(spell, side);
                &penalised
            } else {
                spell
            };

            // Cast pre-flight: presence, reach, the PvP gates and "the
            // server has no effect for this spell", decided before the mana
            // comes off, the cooldown is stamped or the casting skill rolls
            // (see `cast_target_refusal`). A refused cast costs nothing, so
            // nothing has to be handed back. The CLIENT, though, spent the
            // mana and started its own cooldown when it sent the cast: the
            // private CastFail is its signal to undo that, and the
            // ManaUpdate carries the server's untouched value.
            let resolved = match cast_target_refusal(
                spell,
                intent.target_id,
                intent.caster,
                caster_cid,
                caster_pos,
                &connections,
                &enemies,
                &corpses,
                &group_manager,
            ) {
                Ok(resolved) => resolved,
                Err(line) => {
                    tracing::info!(
                        char_id = intent.caster,
                        spell = %spell.name,
                        target = ?intent.target_id,
                        reason = %line,
                        caster_pos = ?caster_pos,
                        target_pos = ?intent.target_id.and_then(|t| enemies.get(&t).map(|e| (e.pos, e.hp, e.is_alive()))),
                        "CastSpell refused — target pre-flight"
                    );
                    // Clear the cast cache so this refused cast's in-flight
                    // state cannot gate the next one.
                    if let Some(cc) = connections.get_mut(&caster_cid) {
                        cc.cast_spell_name.clear();
                        cc.cast_total_duration = 0.0;
                        cc.cast_set_at = None;
                    }
                    handlers::fan_out_cast_fail(&mut *server, &[caster_cid], intent.caster, line);
                    handlers::fan_out_mana_update(
                        &mut *server,
                        &[caster_cid],
                        intent.caster,
                        caster_mp_now,
                        caster_max_mp,
                    );
                    if hp_cost > 0.0 {
                        handlers::fan_out_health_update(
                            &mut *server,
                            &[caster_cid],
                            intent.caster,
                            caster_hp_now,
                            caster_max_hp_now,
                        );
                    }
                continue;
                }
            };

            // The fizzle of an INSTANT spell (a timed one rolled at its
            // CastStart): EQ's roll on the casting skill against the
            // spell's level, after the target checks, before the mana.
            // A fizzle burns a quarter of the mana, starts the gem's
            // refresh (the global cooldown) and trains nothing.
            if spell.cast_time <= 0.0 && caster_conn.fizzles && crate::world::cast_gate::roll_fizzle(caster_conn, spell) {
                let (mp_after, max_mp) = {
                    let cc = connections.get_mut(&caster_cid).expect("checked");
                    let burned = crate::world::cast_gate::fizzle_mana(spell.mana_cost, cc.mp);
                    cc.mp -= burned;
                    cc.gcd_until = Some(now + crate::world::GLOBAL_COOLDOWN);
                    cc.cast_spell_name.clear();
                    cc.cast_total_duration = 0.0;
                    cc.cast_set_at = None;
                    regen::mark_dirty(cc);
                    tracing::info!(char_id = intent.caster, spell = %spell.name, burned, "CastSpell fizzled");
                    (cc.mp, cc.max_mp)
                };
                handlers::fan_out_cast_fail(
                    &mut *server,
                    &[caster_cid],
                    intent.caster,
                    crate::world::cast_gate::FIZZLE_LINE.to_string(),
                );
                handlers::fan_out_mana_update(&mut *server, &[caster_cid], intent.caster, mp_after, max_mp);
                continue;
            }

            // Deduct mana (+ optional hp_cost for blood / fallen
            // spells). Both are caster-side; target-side effects
            // come next. Track 10 — also clear the cast cache so a
            // CastSpell sent without a follow-up CastComplete (or
            // before it arrives) doesn't leave stale state that
            // gates the next timed cast.
            // Track 17.2 — stamp the per-spell cooldown so the
            // next cast of the same spell is rejected until it
            // expires. Skipped when cooldown == 0 (most damage
            // spells; cooldown matters mainly for utility / heals).
            let (new_mp, new_hp_after_cost, max_hp) = {
                let cc = connections.get_mut(&caster_cid).expect("checked");
                cc.mp = (cc.mp - mana_cost).max(0.0);
                // Alignment (2026-10-23): a cast leans its school's way by
                // a hundred-thousandth of its mana, through the momentum
                // rule (the user's equation).
                handlers::move_alignment(&mut *server, caster_cid, cc, crate::world::alignment::spell_delta(spell));
                if hp_cost > 0.0 {
                    cc.hp = (cc.hp - hp_cost).max(0.0);
                }
                // Casting stands you, as a swing does (spell batch step 0):
                // no seated regen through an instant cast. A timed cast
                // already stood at its CastStart.
                cc.is_sitting = false;
                cc.cast_spell_name.clear();
                cc.cast_total_duration = 0.0;
                cc.cast_set_at = None;
                if spell.cooldown > 0.0 {
                    cc.spell_cooldowns.insert(
                        spell.name.clone(),
                        now + Duration::from_secs_f32(spell.cooldown),
                    );
                }
                // The global cooldown starts when a cast is accepted
                // (spell batch step 4); a song starts none.
                if !spell.is_song {
                    cc.gcd_until = Some(now + crate::world::GLOBAL_COOLDOWN);
                }
                regen::mark_dirty(cc);
                (cc.mp, cc.hp, cc.max_hp)
            };
            handlers::fan_out_mana_update(
                &mut *server,
                &in_world_recipients_now,
                intent.caster,
                new_mp,
                caster_max_mp,
            );
            // A hostile cast from the shadows ends a Hide (active
            // skills), as a swing does.
            if matches!(spell.target_type.as_str(), "ENEMY" | "AOE" | "PET_CHARM") {
                let stealth_broken = connections
                    .get_mut(&caster_cid)
                    .is_some_and(|c| buffs::break_stealth(&mut c.active_buffs));
                if stealth_broken {
                    if let Some(c) = connections.get(&caster_cid) {
                        handlers::fan_out_buff_snapshot(&mut *server, &in_world_recipients_now, c);
                    }
                }
            }
            if hp_cost > 0.0 {
                handlers::fan_out_health_update(
                    &mut *server,
                    &in_world_recipients_now,
                    intent.caster,
                    new_hp_after_cost,
                    max_hp,
                );
            }

            // Track 18.1 — casting skill advance. Discipline
            // comes from the GDScript-mirrored DISCIPLINE map
            // (base spell name lookup, Rank suffixes stripped).
            // Channeling advances separately when an interrupt
            // is survived; that path is server-side-pending and
            // lands when the cast-interrupt formula is ported.
            let discipline = skills::discipline_for_spell(&spell.name).to_string();
            if let Some(cc) = connections.get_mut(&caster_cid) {
                if let Some(new_score) =
                    skills::try_advance(cc, skills::Skill::Casting, &discipline)
                {
                    handlers::send_skill_progress_update(
                        &mut *server,
                        caster_cid,
                        skills::Skill::Casting.as_protocol(),
                        discipline.clone(),
                        new_score,
                    );
                }
            }

            // Bard songs (the rework of 2026-10-20): a song's cast is its
            // sing. It starts the one active song, which the server
            // pulses from here (step 4s), or, sung again, stops it; any
            // other cast stops a song (EQ's rule). The ENEMY song's
            // target was checked by the pre-flight; its slow lands on
            // the pulse, so the per-target-type arms are skipped.
            if spell.is_song {
                let same = connections
                    .get(&caster_cid)
                    .is_some_and(|c| c.active_song.as_deref() == Some(spell.name.as_str()));
                if same {
                    stop_song(&mut *server, &mut *connections, caster_cid);
                } else {
                    if let Some(cc) = connections.get_mut(&caster_cid) {
                        cc.active_song = Some(spell.name.clone());
                        cc.song_next_pulse_at = Some(now + Duration::from_secs_f32(crate::world::SONG_PULSE_SECS));
                    }
                    handlers::send_system_line(&mut *server, caster_cid, &format!("You begin singing {}.", spell.name));
                    tracing::info!(singer = intent.caster, song = %spell.name, "song started");
                    song_pulse(&mut *server, &in_world_recipients_now, &mut *connections, &mut *enemies, caster_cid, spell, now);
                }
                continue;
            }
            if connections.get(&caster_cid).is_some_and(|c| c.active_song.is_some()) {
                stop_song(&mut *server, &mut *connections, caster_cid);
            }

            match spell.target_type.as_str() {
                "CORPSE" => {
                    // Corpse / resurrection Slice 3 — a res spell targets a
                    // corpse; offer the res to its owner, who summons + gets an
                    // xp refund on accept. Mana was already deducted above
                    // (consistent with the rest of the cast pipeline).
                    let ResolvedTarget::Corpse(corpse_id, owner_cid) = resolved else {
                        cast_resolution_lost(
                            &mut *server,
                            &mut *connections,
                            &in_world_recipients_now,
                            caster_cid,
                            intent.caster,
                            mana_cost,
                            hp_cost,
                            &spell.name,
                            "corpse",
                        );
                        continue;
                    };
                    tracing::info!(char_id = intent.caster, spell = %spell.name, corpse_id, "resurrection cast received");
                    let xp_percent = spell.res_xp_percent.round() as u32;
                    let caster_name = connections.get(&caster_cid).map(|c| c.name.clone()).unwrap_or_default();
                    // Record the pending offer on the owner so the accept can't
                    // forge the refund %, then send the offer privately.
                    if let Some(owner_conn) = connections.get_mut(&owner_cid) {
                        owner_conn.pending_res_offer = Some((corpse_id, xp_percent));
                    }
                    handlers::send_resurrect_offer(&mut *server, owner_cid, corpse_id, caster_name, xp_percent);
                    tracing::info!(char_id = intent.caster, corpse_id, xp_percent, "resurrection offered");
                }
                "SUMMON_ITEM" => {
                    // Summon Food / Drink (food slice 2): conjure the
                    // spell's item into the caster's bags, the count
                    // scaling with level; the pre-flight checked the
                    // item and the room. Into the bags rather than onto
                    // the cursor (EQ landed summoned items on the
                    // cursor; flagged, not built). The placement block
                    // ends the connection borrow before any refund.
                    let placement = {
                        let Some(conn) = connections.get_mut(&caster_cid) else {
                            continue;
                        };
                        let n = spells::summon_count(spell, &conn.class, conn.level);
                        match conn.inventory.add_item_locating(&spell.summon_item, n) {
                            Ok((touched, leftover)) => {
                                let placed = n - leftover;
                                if placed > 0 {
                                    conn.inventory_dirty = true;
                                    conn.ledger(ledger::item_event(
                                        ItemAction::Summon,
                                        &spell.summon_item,
                                        placed,
                                        i64::from(placed),
                                        Some(spell.name.clone()),
                                    ));
                                }
                                let deltas: Vec<(String, u32, Option<(String, u32)>)> = touched
                                    .iter()
                                    .map(|(loc, slot)| (loc.clone(), *slot, conn.inventory.peek_at(loc, *slot)))
                                    .collect();
                                Ok((placed, leftover, deltas))
                            }
                            Err(e) => Err(e),
                        }
                    };
                    match placement {
                        Ok((placed, leftover, deltas)) if placed > 0 => {
                            for (loc, slot, payload) in deltas {
                                handlers::send_slot_delta(&mut *server, caster_cid, loc, slot, payload);
                            }
                            let item_name = items::lookup(&spell.summon_item)
                                .map(|i| i.name.clone())
                                .unwrap_or_else(|| spell.summon_item.clone());
                            handlers::fan_out_chat_message(
                                &mut *server,
                                &[caster_cid],
                                "",
                                protocol::world::ChatChannel::System,
                                &format!("You conjure {} x{}.", item_name, placed),
                            );
                            tracing::info!(
                                char_id = intent.caster,
                                spell = %spell.name,
                                item = %spell.summon_item,
                                qty = placed,
                                leftover,
                                "SUMMON_ITEM applied"
                            );
                        }
                        other => {
                            // The pre-flight found room a moment ago, so
                            // this is the placer disagreeing with its own
                            // probe; say so, refund, and refuse.
                            tracing::warn!(
                                char_id = intent.caster,
                                spell = %spell.name,
                                outcome = ?other.as_ref().map(|(p, l, _)| (*p, *l)),
                                "SUMMON_ITEM placed nothing after the pre-flight accepted it; refunded"
                            );
                            refund_spell_cost(
                                &mut *server,
                                &mut *connections,
                                &in_world_recipients_now,
                                caster_cid,
                                intent.caster,
                                mana_cost,
                                hp_cost,
                            );
                            handlers::send_refusal(&mut *server, caster_cid, "Your bags are full.");
                            continue;
                        }
                    }
                }
                "SELF" => {
                    tracing::info!(
                        char_id = intent.caster,
                        spell = %spell.name,
                        absorb = spell.absorb_amount,
                        "SELF cast received"
                    );
                    // Heal the caster (or damage in the rare "self
                    // damage" case). base_damage is treated as a
                    // self-damage; heal_amount as a heal.
                    let heal = spell.heal_amount;
                    let dmg = spell.base_damage;
                    if heal > 0.0 || dmg > 0.0 {
                        let (final_hp, max_hp) = {
                            let cc = connections.get_mut(&caster_cid).expect("checked");
                            cc.hp = (cc.hp + heal - dmg).clamp(0.0, cc.max_hp);
                            regen::mark_dirty(cc);
                            (cc.hp, cc.max_hp)
                        };
                        handlers::fan_out_health_update(
                            &mut *server,
                            &in_world_recipients_now,
                            intent.caster,
                            final_hp,
                            max_hp,
                        );
                        tracing::info!(
                            char_id = intent.caster,
                            spell = %spell.name,
                            heal,
                            dmg,
                            final_hp,
                            "spell self effect applied"
                        );
                    }
                    // Track 6 — apply caster buffs. Lich Form is a
                    // SELF-only toggle handled inline; the rest of the
                    // beneficial buffs (HoT / MP-regen / speed / haste /
                    // shield / absorb / accuracy / primary stat) flow
                    // through the shared apply_player_spell_buffs so the
                    // SELF and ALLY arms stay in lockstep.
                    let mut buff_changed = false;
                    if spell.is_lich_form {
                        // Toggle semantics — second cast clears.
                        let cc = connections.get_mut(&caster_cid).expect("checked");
                        let was_active = buffs::is_lich_form_active(&cc.active_buffs);
                        cc.active_buffs.retain(|b| !matches!(
                            b.effect,
                            buffs::BuffEffect::LichForm { .. }
                        ));
                        if !was_active {
                            cc.active_buffs.push(ActiveBuff::new_lich_form(
                                spell.name.clone(),
                                spell.lich_mp_regen,
                                now,
                            ));
                        }
                        buff_changed = true;
                    }
                    if apply_player_spell_buffs(
                        connections.get_mut(&caster_cid).expect("checked"),
                        spell,
                        now,
                    ) {
                        buff_changed = true;
                    }
                    if buff_changed {
                        if let Some(cc) = connections.get(&caster_cid) {
                            handlers::fan_out_buff_snapshot(
                                &mut *server,
                                &in_world_recipients_now,
                                cc,
                            );
                        }
                    }
                }
                "ALLY" => {
                    // The pre-flight resolved the target (a pet, a player,
                    // or oneself) and decided reach, liveness and the PvP
                    // heal gates; the arm only applies.
                    let (pet_target, target_cid) = match resolved {
                        ResolvedTarget::Pet(id) => (Some(id), caster_cid),
                        ResolvedTarget::Player(cid) => (None, cid),
                        _ => {
                            cast_resolution_lost(
                                &mut *server,
                                &mut *connections,
                                &in_world_recipients_now,
                                caster_cid,
                                intent.caster,
                                mana_cost,
                                hp_cost,
                                &spell.name,
                                "ally",
                            );
                            continue;
                        }
                    };
                    if let Some(raw_target) = pet_target {
                        let heal = spell.heal_amount;
                        if heal > 0.0 {
                            if let Some(pet) = enemies.get_mut(&raw_target) {
                                pet.hp = (pet.hp + heal).min(pet.max_hp);
                                let new_hp = pet.hp;
                                let max_hp = pet.max_hp;
                                handlers::fan_out_health_update(
                                    &mut *server,
                                    &in_world_recipients_now,
                                    raw_target,
                                    new_hp,
                                    max_hp,
                                );
                                tracing::info!(
                                    char_id = intent.caster,
                                    target = raw_target,
                                    spell = %spell.name,
                                    heal,
                                    "ALLY pet heal applied"
                                );
                            }
                        }
                        // Track 13 — apply the pet-relevant buff set
                        // (HoT / speed / haste / damage shield / stat)
                        // to the pet and fan a BuffSnapshot under the
                        // pet id.
                        let mut pet_buff_changed = false;
                        if let Some(pet) = enemies.get_mut(&raw_target) {
                            pet_buff_changed = apply_pet_spell_buffs(pet, spell, now);
                        }
                        if pet_buff_changed {
                            if let Some(pet) = enemies.get(&raw_target) {
                                // Valor-style buffs grow max_hp — fan a
                                // HealthUpdate so the bar shows the new cap.
                                if spell.max_hp_buff != 0.0 {
                                    handlers::fan_out_health_update(
                                        &mut *server,
                                        &in_world_recipients_now,
                                        raw_target,
                                        pet.hp,
                                        pet.max_hp,
                                    );
                                }
                                fan_out_pet_buff_snapshot(
                                    &mut *server,
                                    &in_world_recipients_now,
                                    pet,
                                );
                                tracing::info!(
                                    char_id = intent.caster,
                                    target = raw_target,
                                    spell = %spell.name,
                                    "ALLY pet buff applied"
                                );
                            }
                        }
                        continue;
                    }
                    let target_entity_id = target_cid as u64;
                    let heal = spell.heal_amount;
                    if heal > 0.0 {
                        let (final_hp, max_hp) = {
                            let tc =
                                connections.get_mut(&target_cid).expect("checked");
                            tc.hp = (tc.hp + heal).min(tc.max_hp);
                            regen::mark_dirty(tc);
                            (tc.hp, tc.max_hp)
                        };
                        handlers::fan_out_health_update(
                            &mut *server,
                            &in_world_recipients_now,
                            target_entity_id,
                            final_hp,
                            max_hp,
                        );
                        tracing::info!(
                            char_id = intent.caster,
                            target = target_entity_id,
                            spell = %spell.name,
                            heal,
                            "ALLY heal applied"
                        );
                    }
                    // Apply HoT + every beneficial buff to the ally,
                    // same set the SELF arm grants the caster. Stat
                    // buffs mutate the target conn's effective stats
                    // (combat math reads them); the fanned BuffSnapshot
                    // lets the target's client reconstruct the buff
                    // locally (BuffManager.reconcile_with_server_snapshot).
                    let ally_buff_changed = apply_player_spell_buffs(
                        connections.get_mut(&target_cid).expect("checked"),
                        spell,
                        now,
                    );
                    if ally_buff_changed {
                        if let Some(tc) = connections.get(&target_cid) {
                            handlers::fan_out_buff_snapshot(
                                &mut *server,
                                &in_world_recipients_now,
                                tc,
                            );
                        }
                        tracing::info!(
                            char_id = intent.caster,
                            target = target_entity_id,
                            spell = %spell.name,
                            "ALLY buff applied"
                        );
                    }
                }
                "ENEMY" => {
                    // The pre-flight resolved the target and decided
                    // presence, reach, liveness and the PvP gates (for a
                    // player, and for another player's pet); the arm only
                    // applies.
                    let (target_id, pvp_target) = match resolved {
                        ResolvedTarget::Player(cid) => (cid as u64, Some(cid)),
                        ResolvedTarget::Mob(id) | ResolvedTarget::Pet(id) => (id, None),
                        _ => {
                            cast_resolution_lost(
                                &mut *server,
                                &mut *connections,
                                &in_world_recipients_now,
                                caster_cid,
                                intent.caster,
                                mana_cost,
                                hp_cost,
                                &spell.name,
                                "enemy",
                            );
                            continue;
                        }
                    };
                    if let Some(target_cid) = pvp_target {
                        // Track 6 sub-task 4c — absorb pool +
                        // damage shield apply on PvP spell hit
                        // too. Armor reduction is skipped for
                        // spells (matches GDScript wrapping).
                        let shield_back: f32;
                        let shield_back_name: Option<String>;
                        let mut absorb_strip_idx: Option<usize> = None;
                        // The spell crit (spell_power.rs), before the absorb
                        // shield takes its share, as on the client.
                        let flat = spell.base_damage.max(0.0) as i32;
                        let (rolled, crit) = if flat > 0 {
                            crate::world::spell_power::roll_hit(flat, caster_int, &mut rand::thread_rng())
                        } else {
                            (0, false)
                        };
                        let (final_hp, max_hp, applied) = {
                            let tc = connections.get_mut(&target_cid).expect("checked");
                            let mut dmg = rolled;
                            let (after_absorb, exhausted) =
                                buffs::consume_absorb(&mut tc.active_buffs, dmg);
                            dmg = after_absorb;
                            if exhausted {
                                absorb_strip_idx = tc.active_buffs.iter().position(|b| {
                                    matches!(b.effect, buffs::BuffEffect::Absorb { .. })
                                });
                            }
                            shield_back = buffs::damage_shield_total(&tc.active_buffs);
                            shield_back_name = buffs::first_damage_shield_name(&tc.active_buffs).map(|s| s.to_string());
                            tc.hp = (tc.hp - dmg as f32).max(0.0);
                            regen::mark_dirty(tc);
                            tc.note_damage_taken(now); // camp breaks on damage
                            (tc.hp, tc.max_hp, dmg)
                        };
                        if let Some(idx) = absorb_strip_idx {
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
                        handlers::fan_out_hit(
                            &mut *server,
                            &in_world_recipients_now,
                            intent.caster,
                            target_id,
                            applied,
                            crit,
                            dmg_type,
                        );
                        handlers::fan_out_health_update(
                            &mut *server,
                            &in_world_recipients_now,
                            target_id,
                            final_hp,
                            max_hp,
                        );
                        tracing::info!(
                            char_id = intent.caster,
                            target = target_id,
                            spell = %spell.name,
                            applied,
                            "PvP spell applied"
                        );
                        // Track 22.E — PvP spell damage interrupts
                        // the target's cast (mirror of the PvP
                        // melee interrupt wired in Track 19A).
                        if let Some(tc) = connections.get_mut(&target_cid) {
                            let outcome = roll_cast_interrupt(tc, now, applied);
                            let (tc_mp, tc_max_mp) = (tc.mp, tc.max_mp);
                            match &outcome {
                                InterruptOutcome::Interrupted { spell_name, mana_charged } => {
                                    handlers::fan_out_cast_fail(
                                        &mut *server,
                                        &in_world_recipients_now,
                                        target_id,
                                        "interrupted (hit during cast)".to_string(),
                                    );
                                    tell_interrupt_charge(&mut *server, target_cid, target_id, tc_mp, tc_max_mp, *mana_charged);
                                    tracing::info!(
                                        caster = target_id,
                                        spell = %spell_name,
                                        mana_charged,
                                        "PvP cast interrupted by incoming spell damage"
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
                        // Damage shield reflects to caster.
                        if shield_back > 0.0 {
                            if let Some(att) = connections.get_mut(&caster_cid) {
                                if att.hp > 0.0 {
                                    let reflect_dmg = shield_back as i32;
                                    att.hp = (att.hp - shield_back).max(0.0);
                                    regen::mark_dirty(att);
                                    att.note_damage_taken(now); // camp breaks on damage
                                    let new_att_hp = att.hp;
                                    let att_max = att.max_hp;
                                    handlers::fan_out_health_update(
                                        &mut *server,
                                        &in_world_recipients_now,
                                        intent.caster,
                                        new_att_hp,
                                        att_max,
                                    );
                                    if let Some(name) = shield_back_name.as_ref() {
                                        handlers::fan_out_damage_shield_trigger(
                                            &mut *server,
                                            &in_world_recipients_now,
                                            target_id,
                                            intent.caster,
                                            reflect_dmg,
                                            name.clone(),
                                        );
                                    }
                                    // Track 22.E — shield reflect
                                    // damage also interrupts the
                                    // caster (their own thorns
                                    // hits their cast). Note this
                                    // is rarely meaningful since
                                    // CastSpell already cleared
                                    // cast_spell_name above when
                                    // the cast resolved — left
                                    // here for the case where a
                                    // future spell type leaves
                                    // the cache populated.
                                    let outcome = roll_cast_interrupt(att, now, reflect_dmg);
                                    let (att_mp, att_max_mp) = (att.mp, att.max_mp);
                                    if let InterruptOutcome::Interrupted { spell_name, mana_charged } = &outcome {
                                        handlers::fan_out_cast_fail(
                                            &mut *server,
                                            &in_world_recipients_now,
                                            intent.caster,
                                            "interrupted (hit during cast)".to_string(),
                                        );
                                        tell_interrupt_charge(&mut *server, caster_cid, intent.caster, att_mp, att_max_mp, *mana_charged);
                                        tracing::info!(
                                            char_id = intent.caster,
                                            spell = %spell_name,
                                            mana_charged,
                                            "PvP caster interrupted by own shield reflect"
                                        );
                                    }
                                }
                            }
                        }
                        // Track 6 sub-task 4d — CC application on
                        // PvP spell. Mez, Root, Snare, AttackSlow,
                        // Silence, Dispel land on the target's
                        // active_buffs after damage. Refresh
                        // semantics (same-name re-cast renews
                        // duration via apply_buff).
                        let mut cc_changed = false;
                        // A PvP DoT rides the same buff list (spell
                        // batch step 5), ticking in the buff sweep.
                        if spell.dot_dps > 0.0 && spell.dot_duration > 0.0 {
                            if let Some(tc) = connections.get_mut(&target_cid) {
                                apply_buff(
                                    tc,
                                    ActiveBuff::new_dot(
                                        spell.name.clone(),
                                        spell.dot_dps,
                                        intent.caster,
                                        dmg_type,
                                        spell.dot_duration,
                                        now,
                                    ),
                                );
                                cc_changed = true;
                            }
                        }
                        if spell.cc_duration > 0.0 {
                            if let Some(tc) = connections.get_mut(&target_cid) {
                                apply_buff(tc, ActiveBuff::new_mez(spell.name.clone(), spell.cc_duration, now));
                                cc_changed = true;
                            }
                        }
                        if spell.root_duration > 0.0 {
                            if let Some(tc) = connections.get_mut(&target_cid) {
                                apply_buff(tc, ActiveBuff::new_root(spell.name.clone(), spell.root_duration, now));
                                cc_changed = true;
                            }
                        }
                        if spell.slow_amount > 0.0 && spell.slow_duration > 0.0 {
                            if let Some(tc) = connections.get_mut(&target_cid) {
                                apply_buff(tc, ActiveBuff::new_snare(spell.name.clone(), spell.slow_amount, spell.slow_duration, now));
                                cc_changed = true;
                            }
                        }
                        if spell.attack_slow_amount > 0.0 && spell.attack_slow_duration > 0.0 {
                            if let Some(tc) = connections.get_mut(&target_cid) {
                                apply_buff(tc, ActiveBuff::new_attack_slow(spell.name.clone(), spell.attack_slow_amount, spell.attack_slow_duration, now));
                                cc_changed = true;
                            }
                        }
                        if spell.silence_duration > 0.0 {
                            if let Some(tc) = connections.get_mut(&target_cid) {
                                apply_buff(tc, ActiveBuff::new_silence(spell.name.clone(), spell.silence_duration, now));
                                cc_changed = true;
                            }
                        }
                        if spell.is_dispel {
                            // Strip one non-CC buff from target.
                            // Stat buffs need their deltas undone
                            // first.
                            if let Some(tc) = connections.get_mut(&target_cid) {
                                if let Some(idx) = buffs::first_dispellable_index(&tc.active_buffs) {
                                    if let buffs::BuffEffect::StatBuff {
                                        strength, agility, intelligence, wisdom, constitution,
                                        max_hp_delta, max_mp_delta,
                                    } = tc.active_buffs[idx].effect
                                    {
                                        buffs::undo_stat_deltas(
                                            tc,
                                            strength, agility, intelligence, wisdom, constitution,
                                            max_hp_delta, max_mp_delta,
                                        );
                                        regen::mark_dirty(tc);
                                    }
                                    tc.active_buffs.remove(idx);
                                    cc_changed = true;
                                }
                            }
                        }
                        if cc_changed {
                            if let Some(tc) = connections.get(&target_cid) {
                                handlers::fan_out_buff_snapshot(
                                    &mut *server,
                                    &in_world_recipients_now,
                                    tc,
                                );
                            }
                        }
                        continue;
                    }
                    // A mob, or another player's pet the pre-flight
                    // admitted: apply spell damage via the shared helper
                    // (Track 9; the same path AOE uses per victim).
                    apply_spell_damage_to_enemy(
                        &mut *server,
                        &in_world_recipients_now,
                        &mut *connections,
                        &mut *enemies,
                        &mut *loot_bags,
                        &mut *aoi,
                        &group_manager,
                        intent.caster,
                        target_id,
                        spell,
                        dmg_type,
                        now,
                    );
                    // Visibility log for PvP spell-on-pet. The
                    // pet-target path doesn't reach the
                    // "PvP spell applied" trace at line ~3002 (which
                    // is in the player-target arm), so without this
                    // line a spell-on-pet hit is invisible in
                    // server.log and hard to triage.
                    if IdKind::of(target_id) == IdKind::Pet {
                        tracing::info!(
                            char_id = intent.caster,
                            target = target_id,
                            spell = %spell.name,
                            "PvP spell applied vs pet"
                        );
                    }
                    // Track 11.3 — refresh pet target inheritance
                    // on every single-target damage spell too, so
                    // a Necromancer casting Bone Shards at an
                    // enemy directs the skeleton onto it.
                    if let Some(att) = connections.get_mut(&caster_cid) {
                        att.last_attacked_enemy = Some(target_id);
                        att.last_attacked_at = Some(now);
                    }
                }
                "AOE" => {
                    // Track 9 — server-authoritative AOE damage.
                    // Search the caster's AOI neighbourhood (3×3
                    // cells around the caster, 360 m on a side at
                    // CELL_SIZE=120), then filter by spell radius.
                    // AOE spell radii top out around 6 m today, so
                    // this is a tiny working set in practice.
                    let radius = spell.aoe_radius;
                    let radius_sq = radius * radius;
                    let caster_cell = aoi::cell_for(caster_pos.x, caster_pos.z);
                    let visible = aoi.entities_visible_from(caster_cell);
                    let mut victims: Vec<EntityId> = Vec::new();
                    for id in visible {
                        // Eligible AOE victims: world enemies
                        // [ENEMY_ID_BASE, LOOT_BAG_ID_BASE) and pets
                        // [PET_ID_BASE, …). Players (< ENEMY_ID_BASE) and
                        // loot bags ([LOOT_BAG_ID_BASE, PET_ID_BASE)) are
                        // excluded. Pets are included so AOE hits them like
                        // melee/single-target does — the owner PvP gate
                        // below decides whether each pet is actually hit.
                        if !matches!(IdKind::of(id), IdKind::Enemy | IdKind::Pet) {
                            continue;
                        }
                        if let Some(entity) = enemies.get(&id) {
                            if !entity.is_alive() {
                                continue;
                            }
                            // Pet PvP gate (mirrors the single-target
                            // melee/spell paths): a player-owned pet is
                            // only a valid AOE victim if the caster
                            // could attack its owner directly — and
                            // never the caster's own pet. NPC mobs
                            // (owner None) are always eligible.
                            if let Some(owner_id) = entity.owner {
                                let owner_cid = owner_id as ClientId;
                                let allowed = owner_cid != caster_cid
                                    && match (
                                        connections.get(&caster_cid),
                                        connections.get(&owner_cid),
                                    ) {
                                        (Some(a), Some(o)) => combat::can_attack(
                                            a, o,
                                            a.zone.as_deref(),
                                            o.zone.as_deref(),
                                        ),
                                        _ => false,
                                    };
                                if !allowed {
                                    continue;
                                }
                            }
                            let dx = entity.pos.x - caster_pos.x;
                            let dy = entity.pos.y - caster_pos.y;
                            let dz = entity.pos.z - caster_pos.z;
                            if dx * dx + dy * dy + dz * dz <= radius_sq {
                                victims.push(id);
                            }
                        }
                    }
                    let mut hits = 0;
                    for victim in victims {
                        if apply_spell_damage_to_enemy(
                            &mut *server,
                            &in_world_recipients_now,
                            &mut *connections,
                            &mut *enemies,
                            &mut *loot_bags,
                            &mut *aoi,
                            &group_manager,
                            intent.caster,
                            victim,
                            spell,
                            dmg_type,
                            now,
                        ) {
                            hits += 1;
                        }
                    }
                    tracing::info!(
                        char_id = intent.caster,
                        spell = %spell.name,
                        radius,
                        hits,
                        "AOE spell applied"
                    );
                }
                "PET_SUMMON" => {
                    // Track 11 — spawn a player-owned pet at the
                    // caster's position. One pet per owner: any
                    // existing pet for this caster is despawned
                    // first (mark Dead + EntityDespawn fan-out;
                    // corpse cleanup phase removes it from the
                    // map and AOI next tick).
                    let pet_type = spell.pet_type.clone();
                    // Owner-derived level (pet interim A): manual
                    // summons take the EQ re-summon gamble — a 0..=2
                    // roll off owner - 1 — so paying mana again can
                    // land a better pet. The skeleton's tier cap
                    // lives in `pet_templates::scaled`.
                    let Some(owner_level) = connections
                        .get(&(intent.caster as ClientId))
                        .map(|c| c.level.max(1) as u32)
                    else {
                        // Caster vanished between queueing the intent and
                        // resolution — skip, like the other resolver arms,
                        // rather than spawning an ownerless level-1 pet.
                        tracing::debug!(
                            char_id = intent.caster,
                            spell = %spell.name,
                            "PET_SUMMON dropped — caster no longer connected"
                        );
                        continue;
                    };
                    let variance = rand::thread_rng().gen_range(0..=2u32);
                    let owner_id = intent.caster;
                    // Helper handles the scaled template, spawn offset,
                    // despawn-existing, AOI insert, fan-out, and map
                    // insert. None = unknown pet_type.
                    let Some((pet_id, pet_level)) = summon_scaled_pet(
                        &mut *server,
                        &in_world_recipients_now,
                        &mut *enemies,
                        &mut *aoi,
                        owner_id,
                        owner_level,
                        caster_pos,
                        &pet_type,
                        variance,
                        1.0,
                        now,
                    ) else {
                        // The pre-flight checked the pet type is known
                        // (`pet_templates::scaled` at variance 0), so a
                        // miss here is the template vanishing between.
                        cast_resolution_lost(
                            &mut *server,
                            &mut *connections,
                            &in_world_recipients_now,
                            caster_cid,
                            intent.caster,
                            mana_cost,
                            hp_cost,
                            &spell.name,
                            "pet template",
                        );
                        continue;
                    };
                    tracing::info!(
                        owner = owner_id,
                        pet_id,
                        spell = %spell.name,
                        pet_type = %pet_type,
                        level = pet_level,
                        owner_level,
                        "pet summoned"
                    );
                }
                "PET_CHARM" => {
                    // Track 12 Piece C — convert the targeted
                    // enemy into a player-owned pet for
                    // `spell.duration` seconds. Re-keys the id
                    // partition: EntityDespawn the old enemy id,
                    // PetSpawn a fresh pet id at the same pos
                    // with the same hp/max_hp, owner = caster.
                    // When the charm ends (the sweep below, or the
                    // owner leaving) the mob is handed back as the
                    // enemy it was, hostile to its charmer; see
                    // `return_charmed_mob`.
                    let ResolvedTarget::Mob(target_id) = resolved else {
                        cast_resolution_lost(
                            &mut *server,
                            &mut *connections,
                            &in_world_recipients_now,
                            caster_cid,
                            intent.caster,
                            mana_cost,
                            hp_cost,
                            &spell.name,
                            "charm",
                        );
                        continue;
                    };
                    let owner_id = intent.caster;
                    // Look up the target; copy out the stats we
                    // need then remove it from the map + AOI.
                    let extracted: Option<(crate::world::zones::MobTemplate, f32, f32, Vec3f, f32, usize, Vec3f)> = enemies
                        .get(&target_id)
                        .filter(|e| e.is_alive() && !e.is_pet())
                        .map(|e| (e.mob.clone(), e.hp, e.max_hp, e.pos, e.yaw, e.spawn_point_idx, e.spawn_pos));
                    let Some((mob, hp, max_hp, pos, yaw, slot_idx, home_pos)) = extracted else {
                        cast_resolution_lost(
                            &mut *server,
                            &mut *connections,
                            &in_world_recipients_now,
                            caster_cid,
                            intent.caster,
                            mana_cost,
                            hp_cost,
                            &spell.name,
                            "charm victim",
                        );
                        continue;
                    };
                    // Despawn the old enemy id from AOI + fan.
                    let enemy_cell = aoi::cell_for(pos.x, pos.z);
                    aoi.remove(target_id, enemy_cell);
                    let despawn_visible = aoi.entities_visible_from(enemy_cell);
                    for &recipient in in_world_recipients_now.iter() {
                        if despawn_visible.contains(&recipient) {
                            handlers::send_entity_despawn(&mut *server, recipient, target_id);
                        }
                    }
                    // The camp slot stays OCCUPIED for the charm's
                    // life. It used to be freed here, so the camp
                    // respawned a replacement while the charmed mob
                    // still existed; now that the mob comes back
                    // when the charm ends, freeing it would let a
                    // charm-and-wait add a mob to the camp per cast.
                    // The slot rides on the pet: the return puts the
                    // mob back into it, and a pet that dies charmed
                    // frees it through the corpse sweep like any
                    // death.
                    enemies.remove(&target_id);
                    // Build the fresh pet entity. Override hp/
                    // max_hp/yaw from the original so the charm
                    // preserves the mob's current state, and carry
                    // its slot and home so it can be given back.
                    let mut pet = Entity::from_pet_summon(owner_id, pos, mob, now);
                    pet.max_hp = max_hp;
                    pet.hp = hp;
                    pet.yaw = yaw;
                    pet.spawn_point_idx = slot_idx;
                    pet.spawn_pos = home_pos;
                    let duration = if spell.duration > 0.0 { spell.duration } else { 30.0 };
                    pet.charm_expires_at = Some(now + std::time::Duration::from_secs_f32(duration));
                    let pet_id = pet.id;
                    let pet_cell = aoi::cell_for(pet.pos.x, pet.pos.z);
                    aoi.insert(pet_id, pet_cell);
                    let spawn_visible = aoi.entities_visible_from(pet_cell);
                    let pet_recipients: Vec<ClientId> = in_world_recipients_now
                        .iter()
                        .copied()
                        .filter(|id| spawn_visible.contains(id))
                        .collect();
                    if !pet_recipients.is_empty() {
                        // Same creature, new id: the caster keeps it targeted.
                        handlers::fan_out_pet_spawn(&mut *server, &pet_recipients, &pet, Some(target_id));
                    }
                    enemies.insert(pet_id, pet);
                    tracing::info!(
                        owner = owner_id,
                        old_enemy_id = target_id,
                        new_pet_id = pet_id,
                        duration_secs = duration,
                        spell = %spell.name,
                        "enemy charmed into pet"
                    );
                }
                "PET_HEAL" => {
                    // Mends the caster's own pet, whatever is targeted;
                    // the pre-flight found it alive and in reach. Warder's
                    // Mend was the one client-only pet spell (ported
                    // 2026-10-07 after the tester asked for none).
                    let ResolvedTarget::Pet(pet_id) = resolved else {
                        cast_resolution_lost(
                            &mut *server,
                            &mut *connections,
                            &in_world_recipients_now,
                            caster_cid,
                            intent.caster,
                            mana_cost,
                            hp_cost,
                            &spell.name,
                            "own pet",
                        );
                        continue;
                    };
                    let heal = spell.heal_amount.max(0.0);
                    if let Some(pet) = enemies.get_mut(&pet_id) {
                        pet.hp = (pet.hp + heal).min(pet.max_hp);
                        let (new_hp, max_hp) = (pet.hp, pet.max_hp);
                        handlers::fan_out_health_update(
                            &mut *server,
                            &in_world_recipients_now,
                            pet_id,
                            new_hp,
                            max_hp,
                        );
                        tracing::info!(
                            char_id = intent.caster,
                            pet_id,
                            spell = %spell.name,
                            heal,
                            "PET_HEAL applied"
                        );
                    }
                }
                "BIND" => {
                    // Spell batch step 2: bind the target (self, or the
                    // group member the pre-flight admitted) where THEY
                    // stand, and persist it like the Soul Binder does.
                    let ResolvedTarget::Player(bound_cid) = resolved else {
                        cast_resolution_lost(
                            &mut *server,
                            &mut *connections,
                            &in_world_recipients_now,
                            caster_cid,
                            intent.caster,
                            mana_cost,
                            hp_cost,
                            &spell.name,
                            "bind",
                        );
                        continue;
                    };
                    let Some(bc) = connections.get_mut(&bound_cid) else {
                        cast_resolution_lost(
                            &mut *server,
                            &mut *connections,
                            &in_world_recipients_now,
                            caster_cid,
                            intent.caster,
                            mana_cost,
                            hp_cost,
                            &spell.name,
                            "bind target",
                        );
                        continue;
                    };
                    let pos = bc.pos.into_tuple();
                    bc.bind = Some(crate::db::BindPoint { pos });
                    bind_intents.push((bc.char_id, bc.zone.clone(), pos));
                    tracing::info!(
                        char_id = intent.caster,
                        bound = bound_cid as u64,
                        x = pos.0, y = pos.1, z = pos.2,
                        "BIND applied"
                    );
                    handlers::send_system_line(&mut *server, bound_cid, "Your soul is bound to this place.");
                    if bound_cid != caster_cid {
                        handlers::send_system_line(&mut *server, caster_cid, "Their soul is bound to this place.");
                    }
                }
                "PORT" => {
                    // Spell batch step 2: the destination is the server's
                    // (the caster's bind, or the nearest safe area's
                    // arrival), never the client's. Evacuate also moves the
                    // caster's alive, present group members within friendly
                    // reach; a corpse or a linkdead body stays. Everyone
                    // moved is wiped from every mob's tables first, so no
                    // train follows them, and their pets come along.
                    let caster_pos_now = connections
                        .get(&caster_cid)
                        .map(|c| c.pos)
                        .unwrap_or(caster_pos);
                    let dest = match spell.port.as_str() {
                        "bind" => connections
                            .get(&caster_cid)
                            .and_then(|c| c.bind)
                            .map(|b| Vec3f::from_tuple(b.pos))
                            .unwrap_or(crate::world::STARTER_SPAWN),
                        _ => crate::world::safe_areas::nearest_arrival(caster_pos_now)
                            .unwrap_or(crate::world::STARTER_SPAWN),
                    };
                    let mut movers: Vec<ClientId> = vec![caster_cid];
                    if spell.port_group {
                        if let Some(g) = group_manager.group_of(caster_cid) {
                            for &m in &g.members {
                                if m == caster_cid {
                                    continue;
                                }
                                let eligible = connections.get(&m).is_some_and(|c| {
                                    c.in_world
                                        && c.linkdead_since.is_none()
                                        && c.hp > 0.0
                                        && !c.death_processed
                                        && c.pos.distance_to(caster_pos_now) <= spell.reach()
                                });
                                if eligible {
                                    movers.push(m);
                                }
                            }
                        }
                    }
                    let mover_ids: Vec<EntityId> = movers.iter().map(|m| *m as EntityId).collect();
                    for mob_id in wipe_hate(&mut *enemies, &mover_ids) {
                        handlers::fan_out_entity_target(
                            &mut *server,
                            &in_world_recipients_now,
                            mob_id,
                            None,
                        );
                    }
                    for bearer in clear_player_dots_from(&mut *connections, &mover_ids) {
                        if let Some(conn) = connections.get(&bearer) {
                            handlers::fan_out_buff_snapshot(&mut *server, &in_world_recipients_now, conn);
                        }
                    }
                    for &m in &movers {
                        if let Some(c) = connections.get_mut(&m) {
                            c.pos = dest;
                            // The fight is over for the mover: without this
                            // the pet pre-pass (step 4i) re-inherits the
                            // owner's last target for up to 10 s and the
                            // pet walks straight back out of town to it.
                            c.last_attacked_enemy = None;
                            c.last_attacked_at = None;
                            regen::mark_dirty(c);
                        }
                        handlers::send_teleport(&mut *server, m, dest);
                        // The mover's summoned pets come along (user call
                        // D4), set to follow so a parked one does not stand
                        // alone across the zone. Their AOI cell changes
                        // right here, so the players who gain or lose sight
                        // of them are told now rather than never. A CHARMED
                        // mob is not a pet the mover owns: it is released
                        // where it stands and walks home, as when its
                        // charmer logs out. Carrying it would put a camp
                        // mob (a named one, even) inside the safe area to
                        // turn hostile when the charm ends.
                        let pets: Vec<EntityId> = enemies
                            .iter()
                            .filter(|(_, e)| e.owner == Some(m as EntityId) && e.is_alive())
                            .map(|(id, _)| *id)
                            .collect();
                        for pet_id in pets {
                            if enemies.get(&pet_id).is_some_and(|e| e.charm_expires_at.is_some()) {
                                if let Some(pet) = enemies.remove(&pet_id) {
                                    let mob_id = return_charmed_mob(
                                        &mut *server,
                                        &mut *aoi,
                                        &mut *enemies,
                                        &in_world_recipients_now,
                                        pet,
                                        None,
                                        now,
                                    );
                                    tracing::info!(
                                        owner = m,
                                        pet_id,
                                        mob_id,
                                        "charm broken by the owner porting away — mob returned"
                                    );
                                }
                                continue;
                            }
                            let (old_cell, new_cell) = {
                                let Some(pet) = enemies.get_mut(&pet_id) else { continue };
                                let old_cell = aoi::cell_for(pet.pos.x, pet.pos.z);
                                pet.pos = Vec3f { x: dest.x + 1.5, y: dest.y, z: dest.z };
                                // last_bcast_pos is left where it was, so the
                                // move fans on the next tick instead of
                                // waiting for the keepalive.
                                pet.target = None;
                                pet.stance = entity::PetStance::Follow;
                                (old_cell, aoi::cell_for(pet.pos.x, pet.pos.z))
                            };
                            if new_cell != old_cell {
                                let (gained, lost) = aoi.update(pet_id, old_cell, new_cell);
                                if let Some(pet) = enemies.get(&pet_id) {
                                    fan_entity_cell_crossing(&mut *server, &aoi, &connections, pet, &gained, &lost);
                                }
                            }
                        }
                    }
                    tracing::info!(
                        char_id = intent.caster,
                        spell = %spell.name,
                        port = %spell.port,
                        moved = movers.len(),
                        x = dest.x, z = dest.z,
                        "PORT applied"
                    );
                }
                "NONE" | _ => {
                    // A target type with no arm. Every type in today's
                    // spells.toml has one (BIND and PORT gained theirs in
                    // spell batch step 2), and the pre-flight refuses an
                    // unknown type before the mana comes off, so this is
                    // reached only by a new type added to the data ahead of
                    // its arm. Mana is gone and nothing happens, which is
                    // indistinguishable from a bug. Say so, and give the cost
                    // back. Raised to info! as well: a player-visible
                    // failure should not be invisible at the default log
                    // level.
                    tracing::info!(
                        char_id = intent.caster,
                        spell = %spell.name,
                        target_type = %spell.target_type,
                        "spell target_type not yet processed server-side; mana deducted only"
                    );
                    refund_spell_cost(
                        &mut *server,
                        &mut *connections,
                        &in_world_recipients_now,
                        caster_cid,
                        intent.caster,
                        mana_cost,
                        hp_cost,
                    );
                    handlers::send_refusal(
                        &mut *server,
                        caster_cid,
                        "That magic has no effect here yet.",
                    );
                }
            }
        }
    }
}
