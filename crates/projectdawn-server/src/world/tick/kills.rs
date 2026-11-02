//! A mob's death and what it pays (XP, quest credit, faction, loot), spell damage on a mob, and the hate and damage-over-time a death or a port wipes.

use super::*;

/// Award a kill to the creditor's group: split the XP pool (with
/// GROUP_XP_BONUS when grouped) among online members and count the kill
/// against each member's active quest objectives (PD_W0024 — the server
/// counts kills now; each increment fans a private `QuestProgress`, which
/// replaced the old `KillCredit`). EQ semantics: the group split applies
/// regardless of HOW the mob died — melee, spell, or pet — so all three kill
/// paths route here. Solo creditor = full base XP. Owned victims (pets /
/// charmed mobs) award nothing — no XP and no quest credit (the warder is
/// literally named "Wolf"). Non-player or fully-disconnected creditors award
/// nothing.
///
/// Eligibility (decided 2026-09-19): an XP share goes only to members who
/// are online, ALIVE, and within `GROUP_SHARE_RANGE` of the dying
/// enemy — the coin split's range rule, plus the alive filter, because a
/// corpse lying beside the mob would otherwise still collect under pure
/// proximity, and classic EQ pays a corpse nothing. Alive means
/// `!death_processed && hp > 0.0`: the death sweep runs at end of tick, so
/// a member dropped by the mob's last swing in the SAME tick as the kill
/// is a corpse by hp before the flag catches up. The kill-creditor is
/// What a kill does to standings and alignment (factions, 2026-10-23), read
/// off the dying entity before its borrow ends: its faction and alignment
/// tier, the hit it is worth, and the players on its hate list.
pub(super) struct FactionKill {
    pub(super) faction: Option<String>,
    pub(super) alignment: Option<String>,
    pub(super) hit: f32,
    pub(super) named: bool,
    pub(super) haters: Vec<ClientId>,
}

impl FactionKill {
    pub(super) fn of(entity: &Entity) -> Self {
        let named = entity.mob.named_id.is_some();
        Self {
            faction: entity.mob.faction.clone(),
            alignment: entity.mob.alignment.clone(),
            hit: crate::world::factions::kill_hit(entity.mob.level, named, entity.mob.faction_hit),
            named,
            haters: {
                // Everyone the mob hates: whoever damaged it (`aggro`) and
                // whoever it holds threat on (a miss adds a point; a healer's
                // hate rides the threat map), as EQ's hate list.
                let mut ids: Vec<ClientId> = entity
                    .aggro
                    .keys()
                    .chain(entity.threat.keys())
                    .filter(|&&id| id < protocol::world::ENEMY_ID_BASE)
                    .map(|&id| id as ClientId)
                    .collect();
                ids.sort_unstable();
                ids.dedup();
                ids
            },
        }
    }
}

/// exempt from the RANGE half only (a pet owner may legitimately direct a
/// kill from afar, and the coin analogue — the looter — always collects),
/// never from the alive half. The pool divides among eligible members only.
///
/// Quest kill-credit takes the same alive-and-in-range test since
/// 2026-10-05 (user: "Change the XP/journal credit distance to 200m"): at
/// 200 m the gate no longer costs a member chasing a runner their credit,
/// which was the reason journal ticks had stayed unranged at 30 m.
pub(super) fn award_kill(
    server: &mut RenetServer,
    connections: &mut HashMap<ClientId, PerConnection>,
    group_manager: &groups::GroupManager,
    credit_id: u64,
    base_xp: i32,
    mob_name: &str,
    victim_owned: bool,
    victim_pos: Vec3f,
    faction_kill: FactionKill,
) {
    if base_xp <= 0 || credit_id >= protocol::world::ENEMY_ID_BASE {
        return; // no reward, or the top damager wasn't a player
    }
    // Owned victims (player pets, charmed mobs) award NOTHING — no XP and no
    // quest credit. EQ semantics, and the XP half matters as much as the quest
    // half: a Beast Master's warder respawns free every ~15s, so pet kills
    // paying XP would be an infinite (and group-amplified) leveling loop.
    if victim_owned {
        return;
    }
    let credit_cid = credit_id as ClientId;
    // Factions (2026-10-23, EQ's rule): the kill's hits go to every player
    // on the mob's hate list, grouped or not, in range or not, so a healer
    // who never swung still answers for the kill once the mob hated them.
    if let Some(faction_id) = faction_kill.faction.as_deref() {
        let hits = crate::world::factions::kill_hits(faction_id, faction_kill.hit);
        for hater in &faction_kill.haters {
            if let Some(conn) = connections.get_mut(hater) {
                crate::world::handlers::apply_faction_hits(server, *hater, conn, &hits);
            }
        }
    }
    // Alignment: the kill is the creditor's act (events.md's table by the
    // victim's tier; a named mob counts as its tier's named row).
    if let Some(conn) = connections.get_mut(&credit_cid) {
        let delta = crate::world::alignment::kill_delta(faction_kill.alignment.as_deref(), faction_kill.named);
        crate::world::handlers::move_alignment(server, credit_cid, conn, delta);
    }
    // Alive = neither flagged dead NOR at 0 hp — the flag alone misses a
    // member the mob dropped in this same tick (death sweep runs at end
    // of tick).
    let alive = |c: &PerConnection| !c.death_processed && c.hp > 0.0;
    // XP share: alive + in range (creditor exempt from the range half only).
    let xp_eligible = |cid: ClientId, c: &PerConnection| -> bool {
        alive(c)
            && (cid == credit_cid
                || c.pos.distance_to(victim_pos) <= GROUP_SHARE_RANGE)
    };
    // The candidate set: the group, or the solo creditor. Liveness check on
    // the solo killer too — they may have disconnected (or died to a last
    // exchange of blows) between dealing top damage and the mob dying.
    let group_members: Vec<ClientId> = match group_manager.group_of(credit_cid) {
        Some(g) => g.members.iter().copied().collect(),
        None => vec![credit_cid],
    };
    let xp_members: Vec<ClientId> = group_members
        .iter()
        .filter(|&&m| connections.get(&m).map(|c| xp_eligible(m, c)).unwrap_or(false))
        .copied()
        .collect();
    // Quest kill-credit: the same alive-and-in-range test (see doc comment).
    let quest_members: Vec<ClientId> = xp_members.clone();
    if xp_members.is_empty() && quest_members.is_empty() {
        return;
    }
    // Server-authoritative xp/leveling (Slice 0): every member's share runs
    // through award_xp so leveling stays authoritative.
    let (pool, per_member) = if xp_members.is_empty() {
        (0, 0)
    } else {
        let pool = if xp_members.len() > 1 {
            ((base_xp as f32) * (1.0 + groups::GROUP_XP_BONUS)) as i32
        } else {
            base_xp
        };
        (pool, (pool / xp_members.len() as i32).max(1))
    };
    for m in &xp_members {
        if let Some(conn) = connections.get_mut(m) {
            crate::world::progression::award_xp(server, conn, per_member);
        }
    }
    // PD_W0024 — count the kill against each alive member's active quest
    // objectives (private; witnesses outside the group get none). Counts
    // clamp at the requirement, mirroring the client's old notify_kill;
    // each increment fans a private QuestProgress and marks the quest for
    // the end-of-tick persist flush.
    for m in &quest_members {
        if let Some(conn) = connections.get_mut(m) {
            let mut updates: Vec<(String, u32, i32)> = Vec::new();
            for (quest_id, progress) in conn.active_quests.iter_mut() {
                let Some(quest) = crate::world::quests::lookup(quest_id) else {
                    continue; // normalized at login; stay defensive anyway
                };
                for (i, obj) in quest.objectives.iter().enumerate() {
                    let Some(p) = progress.get_mut(i) else { continue };
                    if *p < obj.count && crate::world::quests::kill_matches(&obj.target, mob_name) {
                        *p += 1;
                        updates.push((quest_id.clone(), i as u32, *p));
                    }
                }
            }
            for (quest_id, index, count) in updates {
                conn.quests_dirty.insert(quest_id.clone());
                crate::world::handlers::send_quest_progress(server, *m, &quest_id, index, count);
            }
        }
    }
    tracing::info!(
        killer = credit_id,
        mob = %mob_name,
        base_xp,
        pool,
        per_member,
        members = xp_members.len(),
        "kill credit granted"
    );
}

/// The one kill step for a mob or pet brought to 0 HP by anything other
/// than a melee swing (spell batch step 5): a direct spell hit, a DoT tick,
/// a damage shield. Marks it dead, fans EntityDied, credits the kill to
/// whoever holds the most aggro (XP, the group split, quest credit),
/// schedules a warder's return, and drops the loot. Idempotent on an
/// entity that is already dead or gone. Before this existed the spell arm
/// carried the sequence inline and the two damage-shield sites skipped it,
/// so a mob finished off by Thorns paid nothing.
/// `source` names the killing blow in the log ("spell kill", "reflect
/// kill", ...), so a scan for one kind keeps working.
#[allow(clippy::too_many_arguments)]
pub(super) fn kill_enemy(
    server: &mut RenetServer,
    in_world_recipients: &[ClientId],
    connections: &mut HashMap<ClientId, PerConnection>,
    enemies: &mut HashMap<EntityId, Entity>,
    loot_bags: &mut HashMap<EntityId, LootBag>,
    aoi: &mut AoiGrid,
    group_manager: &groups::GroupManager,
    entity_id: EntityId,
    source: &str,
    now: Instant,
) {
    let (credit_id_opt, mob_xp, mob_level, death_pos, mob_name, warder_owner_opt, victim_owned, faction_kill) = {
        let Some(entity) = enemies.get_mut(&entity_id) else {
            return;
        };
        if !entity.is_alive() {
            return;
        }
        entity.transition(EnemyState::Dead, now);
        handlers::fan_out_entity_died(server, in_world_recipients, entity_id);
        let credit_id_opt = entity
            .aggro
            .iter()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(&id, _)| id);
        // EQ quadratic per-kill award, derived from the mob's level (not the
        // legacy flat mob.xp constant). See progression::kill_xp.
        let mob_xp = crate::world::progression::kill_xp(entity.mob.level as i32, crate::world::progression::ZEM_NORMAL);
        let warder_owner_opt: Option<EntityId> =
            if crate::world::pet_templates::is_warder_template(&entity.mob.name) {
                entity.owner
            } else {
                None
            };
        // Owned victims (pets, charmed mobs) must not grant quest kill
        // credit — the warder is literally named "Wolf", so a respawning
        // PvP warder would farm the wolf quest otherwise.
        let victim_owned = entity.owner.is_some();
        let faction_kill = FactionKill::of(entity);
        (credit_id_opt, mob_xp, entity.mob.level, entity.pos, entity.mob.name.clone(), warder_owner_opt, victim_owned, faction_kill)
    };

    // Warder respawn — mirrors the melee path. If the dying entity is a
    // Beast Master warder, schedule the owner's warder_respawn_at so the
    // warder-respawn sweep brings it back.
    if let Some(owner_id) = warder_owner_opt {
        const WARDER_RETREAT_SECS: f32 = 15.0;
        let due = now + std::time::Duration::from_secs_f32(WARDER_RETREAT_SECS);
        let owner_cid = owner_id as ClientId;
        if let Some(conn) = connections.get_mut(&owner_cid) {
            conn.warder_respawn_at = Some(due);
            tracing::info!(
                owner = owner_cid as u64,
                killer = ?credit_id_opt,
                retreat_secs = WARDER_RETREAT_SECS,
                "warder retreating after kill",
            );
        }
    }
    if let Some(credit_id) = credit_id_opt {
        // Every kill uses the same group XP split + quest credit as a melee
        // kill (EQ semantics: the split is method-agnostic).
        award_kill(
            server,
            connections,
            group_manager,
            credit_id,
            mob_xp,
            &mob_name,
            victim_owned,
            death_pos,
            faction_kill,
        );
    }
    // An owned victim (a pet, a charmed mob) drops nothing, as it pays no
    // XP: a warder is "Wolf" with a wolf's loot table and returns free every
    // 15 s, so two flagged accounts could farm its bag (the review of step 5
    // found the melee path had always rolled it).
    if victim_owned {
        return;
    }
    let loot_items = loot::roll_for_mob(&mob_name).unwrap_or_default();
    let loot_coins = loot::roll_coin_for_mob(&mob_name, mob_level);
    if !loot_items.is_empty() || loot_coins != protocol::world::Coins::ZERO {
        // Loot ownership: the kill-creditor owns the corpse; their group
        // shares rights, resolved at loot time.
        let owner_cid_opt = credit_id_opt
            .filter(|&id| id < protocol::world::ENEMY_ID_BASE)
            .map(|id| id as ClientId);
        let bag = LootBag::new(death_pos, loot_items, loot_coins, mob_name.clone(), owner_cid_opt, now);
        let bag_id = bag.id;
        let bag_cell = aoi::cell_for(bag.pos.x, bag.pos.z);
        aoi.insert(bag_id, bag_cell);
        let visible = aoi.entities_visible_from(bag_cell);
        let bag_recipients: Vec<ClientId> = in_world_recipients
            .iter()
            .copied()
            .filter(|id| visible.contains(id))
            .collect();
        if !bag_recipients.is_empty() {
            handlers::fan_out_loot_bag_spawn(server, &bag_recipients, &bag);
        }
        loot_bags.insert(bag.id, bag);
        tracing::info!(mob = %mob_name, bag_id, "loot bag spawned ({source})");
    }
}

/// Track 9 — apply a single spell hit to one enemy. Shared by the
/// single-target ENEMY arm and the AOE fan-out so the damage / death /
/// kill-credit / loot / CC sequence stays in one place. Caller is
/// responsible for range checks; this just applies the hit to the
/// `target_id` enemy if it exists and is alive.
///
/// Returns `true` if damage landed (entity existed and was alive at
/// entry), `false` otherwise.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_spell_damage_to_enemy(
    server: &mut RenetServer,
    in_world_recipients: &[ClientId],
    connections: &mut HashMap<ClientId, PerConnection>,
    enemies: &mut HashMap<EntityId, Entity>,
    loot_bags: &mut HashMap<EntityId, LootBag>,
    aoi: &mut AoiGrid,
    group_manager: &groups::GroupManager,
    caster_id: u64,
    target_id: EntityId,
    spell: &spells::Spell,
    dmg_type: DamageType,
    now: Instant,
) -> bool {
    // The spell crit (spell_power.rs), rolled per hit off the caster's INT;
    // a spell that deals no damage (a pure mez or root) has nothing to crit.
    let caster_int = connections
        .get(&(caster_id as ClientId))
        .map(|c| c.intelligence)
        .unwrap_or(0);
    let (died, damage_done) = {
        let Some(entity) = enemies.get_mut(&target_id) else {
            return false;
        };
        if !entity.is_alive() {
            return false;
        }
        let flat = spell.base_damage.max(0.0) as i32;
        let (dmg, crit) = if flat > 0 {
            crate::world::spell_power::roll_hit(flat, caster_int, &mut rand::thread_rng())
        } else {
            (0, false)
        };
        let hp_before = entity.hp;
        entity.hp = (entity.hp - dmg as f32).max(0.0);
        let damage_done = (hp_before - entity.hp).max(0.0);
        *entity.aggro.entry(caster_id).or_insert(0.0) += dmg as f32;
        // Track 12 Piece A2 — caster threat mirrors aggro for the
        // re-target check.
        *entity.threat.entry(caster_id).or_insert(0.0) += dmg as f32;
        let entity_id = entity.id;
        let entity_hp = entity.hp;
        let entity_max_hp = entity.max_hp;
        let died = entity.hp <= 0.0;

        handlers::fan_out_hit(
            server,
            in_world_recipients,
            caster_id,
            target_id,
            dmg,
            crit,
            dmg_type,
        );
        handlers::fan_out_health_update(
            server,
            in_world_recipients,
            entity_id,
            entity_hp,
            entity_max_hp,
        );

        if died {
            (true, damage_done)
        } else {
            if dmg > 0 {
                entity.clear_mez();
            }
            // Damage over time (spell batch step 5): the direct hit above
            // is the cast's own; the DoT ticks from here, credited to the
            // caster, replacing an earlier cast of the same spell.
            if spell.dot_dps > 0.0 && spell.dot_duration > 0.0 {
                entity.apply_dot(entity::ActiveDot {
                    name: spell.name.clone(),
                    caster: caster_id,
                    dps: spell.dot_dps,
                    dmg_type,
                    remaining: spell.dot_duration,
                    tick_acc: 0.0,
                    ticks_left: 0,
                });
            }
            if spell.cc_duration > 0.0 {
                entity.apply_cc(ActiveCc::new_mez(spell.cc_duration));
            }
            if spell.root_duration > 0.0 {
                entity.apply_cc(ActiveCc::new_root(spell.root_duration));
            }
            if spell.slow_amount > 0.0 && spell.slow_duration > 0.0 {
                entity.apply_cc(ActiveCc::new_snare(spell.slow_amount, spell.slow_duration));
            }
            if spell.attack_slow_amount > 0.0 && spell.attack_slow_duration > 0.0 {
                entity.apply_cc(ActiveCc::new_attack_slow(
                    spell.attack_slow_amount,
                    spell.attack_slow_duration,
                ));
            }
            (false, damage_done)
        }
    };

    // Track 14 follow-up — lifesteal. Spells with both base_damage
    // and heal_amount (e.g. Lifetap, Soul Drain, Exsanguinate) heal
    // the caster for `min(heal_amount, damage_done)`. Capping at
    // damage_done matches the GDScript reference and stops a low-HP
    // mob from over-healing the caster. Skipped when the caster
    // isn't a connected player (NPC casters don't have HP we track).
    if spell.heal_amount > 0.0 && damage_done > 0.0 {
        let heal = spell.heal_amount.min(damage_done);
        let caster_cid = caster_id as ClientId;
        if let Some(caster) = connections.get_mut(&caster_cid) {
            let prev_hp = caster.hp;
            caster.hp = (caster.hp + heal).min(caster.max_hp);
            if caster.hp != prev_hp {
                handlers::fan_out_health_update(
                    server,
                    in_world_recipients,
                    caster.char_id as u64,
                    caster.hp,
                    caster.max_hp,
                );
            }
        }
    }

    if died {
        kill_enemy(
            server,
            in_world_recipients,
            connections,
            enemies,
            loot_bags,
            aoi,
            group_manager,
            target_id,
            "spell kill",
            now,
        );
    }

    true
}

/// End every PvP DoT these casters have on any player (spell batch step 5):
/// the ledger rule that your DoTs end when you die, port or log out holds
/// for a flagged player's bar too, or a caster could DoT someone and log
/// out with no counterplay. Returns the bearers whose buff set changed, for
/// the caller to re-fan.
pub(super) fn clear_player_dots_from(connections: &mut HashMap<ClientId, PerConnection>, casters: &[EntityId]) -> Vec<ClientId> {
    let mut changed = Vec::new();
    for (cid, conn) in connections.iter_mut() {
        let before = conn.active_buffs.len();
        conn.active_buffs.retain(|b| {
            !matches!(b.effect, buffs::BuffEffect::Dot { caster, .. } if casters.contains(&caster))
        });
        if conn.active_buffs.len() != before {
            changed.push(*cid);
        }
    }
    changed
}

/// Remove these players from every mob's aggro and threat tables and from any
/// mob's target. A mob that loses its target leashes home and heals to full
/// (`entity.rs::tick_leash`). Shared by the death sweep and the port arm: a
/// player who Gates must not drag a train into town, and a mob that lost its
/// target pays nothing to "tag it, Gate out, let the damage finish it".
/// Returns the mobs whose target was cleared, for the caller to fan as
/// `EntityTarget`: the AI pass only reports a change it made itself
/// (`tick_ai` compares against the target it started with), so a wipe
/// here is invisible to clients unless the caller says so.
pub(super) fn wipe_hate(enemies: &mut HashMap<EntityId, Entity>, players: &[EntityId]) -> Vec<EntityId> {
    let mut dropped = Vec::new();
    for entity in enemies.values_mut() {
        for id in players {
            entity.aggro.remove(id);
            entity.threat.remove(id);
            // Their DoTs end with their hate (spell batch step 5): a
            // player who died or ported is paid nothing by what they left
            // ticking.
            entity.clear_dots_from(*id);
            if entity.target == Some(*id) {
                entity.target = None;
                dropped.push(entity.id);
            }
        }
    }
    dropped
}