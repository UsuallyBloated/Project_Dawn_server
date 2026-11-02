//! Natural regeneration (step 5b): hit points, mana and stamina, gated by food and
//! water; hunger and thirst drain, eating by itself, and the eating emote.

use super::*;

pub(super) fn run_regen(world: &mut WorldState, q: &mut Intents, now: Instant, dt: f32) {
    let WorldState { server, connections, trade_manager, .. } = world;
    let Intents { eat_emotes, .. } = q;
    // 5b. Track 6 regen tick — HP/MP/Stamina recovery, then fan
    //     `HealthUpdate` / `ManaUpdate` / `StaminaUpdate` for any
    //     connection whose values crossed the broadcast threshold.
    //     Runs for every ready connection (regardless of in_world)
    //     so a player in the lobby keeps regenerating between
    //     sessions, but only in_world peers receive the fan-outs.
    let mut regen_fanouts: Vec<u64> = Vec::new();
    for (client_id, conn) in connections.iter_mut().filter(|(_, c)| c.ready) {
        // Food and water, the EQ way (`hunger.rs`): the counters run
        // down for the living in the world, the stage lines are told
        // once each, and a famished character eats the first food (or
        // drinks the first drink) in slot order the tick it runs out.
        // EQ checks about every 46 s; checking every tick only trims the
        // lag, the rule is the same. The dead and the lobby do not drain.
        if conn.in_world && conn.hp > 0.0 && !conn.death_processed {
            // A character can log in hungry or out: say so once.
            if !conn.appetite_announced {
                conn.appetite_announced = true;
                for line in hunger::current_stage_lines(conn) {
                    handlers::send_system_line(&mut *server, *client_id, line);
                }
            }
            let entered = hunger::drain(conn, dt);
            // Eat before speaking: a character that runs out WITH bread in
            // the bags eats this tick and is never told it is out of food.
            let mut fed_now = [false, false];
            for (i, need) in [hunger::Need::Food, hunger::Need::Drink].into_iter().enumerate() {
                if !hunger::is_famished(conn, need) {
                    continue;
                }
                // Food offered in an open trade is not the character's to
                // eat until the trade ends.
                let Some((loc, slot, item)) = hunger::first_in_slot_order(
                    &conn.inventory,
                    need,
                    |l, s| trade_manager.is_locked(*client_id, l, s),
                ) else {
                    continue;
                };
                if conn.inventory.decrement_at(&loc, slot).is_err() {
                    continue;
                }
                conn.inventory_dirty = true;
                conn.ledger(ledger::item_event(
                    ItemAction::Eat,
                    &item.path,
                    1,
                    -1,
                    Some("hunger".to_string()),
                ));
                let (path, count) = match conn.inventory.peek_at(&loc, slot) {
                    Some((p, c)) => (Some(p), c),
                    None => (None, 0),
                };
                handlers::send_inventory_delta(&mut *server, *client_id, loc.clone(), slot, path, count);
                let added = hunger::feed(conn, need, hunger::fullness_of(item));
                fed_now[i] = true;
                eat_emotes.push((*client_id, hunger::eat_emote(&conn.name, &item.name, need)));
                let verb = if need == hunger::Need::Food { "eat" } else { "drink" };
                handlers::send_system_line(
                    &mut *server,
                    *client_id,
                    &format!("You {verb} a {}.", item.name),
                );
                tracing::info!(
                    char_id = conn.char_id,
                    item = %item.path,
                    %loc,
                    slot,
                    added_secs = added,
                    "auto-{verb}: the first in slot order"
                );
            }
            for (need, stage, line) in entered {
                let i = if need == hunger::Need::Food { 0 } else { 1 };
                if stage == hunger::Stage::Out && fed_now[i] {
                    continue;
                }
                handlers::send_system_line(&mut *server, *client_id, line);
            }
        }
        // The hunger window: to the owner, on each reason `update_due`
        // knows (dead or alive, so a death stops the bars).
        if conn.in_world && hunger::update_due(conn, now) {
            handlers::send_hunger_update(&mut *server, *client_id, conn);
            hunger::mark_sent(conn, now);
        }
        let result = regen::tick_one(conn, dt, now);
        if result.hp_fanout || result.mp_fanout || result.stamina_fanout {
            regen_fanouts.push(conn.char_id as u64);
        }
        // Meditate skill-up: while actually meditating (seated, out of combat,
        // mana below full) advance the `meditate` casting skill once per ~6 s
        // med-tick — server-authoritative, gated to that cadence via
        // `last_meditate_at`. `try_advance` returns None for classes that can't
        // train it (cap 0), and drives the sitting MP regen in regen.rs. Fans
        // a private SkillProgressUpdate on a gain so the client mirror keeps up.
        // No water, no meditate: the skill-up rides mana regeneration,
        // which a thirsty character does not have (the 2026-10-13 review
        // found a seated caster out of water rolling advances for ever).
        if conn.mp < conn.max_mp
            && regen::sitting_bonus_applies(conn, now)
            && regen::natural_regen_allowed(conn.hunger_secs, conn.thirst_secs).1
        {
            let due = conn
                .last_meditate_at
                .map_or(true, |t| now.duration_since(t) >= Duration::from_secs(6));
            if due {
                conn.last_meditate_at = Some(now);
                if let Some(new_score) =
                    skills::try_advance(conn, skills::Skill::Casting, "meditate")
                {
                    let cid = conn.char_id as ClientId;
                    handlers::send_skill_progress_update(
                        &mut *server,
                        cid,
                        skills::Skill::Casting.as_protocol(),
                        "meditate".to_string(),
                        new_score,
                    );
                }
            }
        }
    }
    // EQ's eating emote, to the people who could hear the eater say
    // something (the say channel's reach), never to the eater.
    for (eater_cid, text) in eat_emotes.drain(..) {
        let recipients = say_recipients(eater_cid, &connections);
        handlers::fan_out_chat_message(
            &mut *server,
            &recipients,
            "",
            protocol::world::ChatChannel::System,
            &text,
        );
    }
    if !regen_fanouts.is_empty() {
        let recipients: Vec<ClientId> = connections
            .iter()
            .filter(|(_, c)| c.in_world)
            .map(|(id, _)| *id)
            .collect();
        if !recipients.is_empty() {
            for id in regen_fanouts {
                if let Some(conn) = connections.get(&(id as ClientId)) {
                    handlers::fan_out_resources(&mut *server, &recipients, conn);
                }
            }
        }
    }
}
