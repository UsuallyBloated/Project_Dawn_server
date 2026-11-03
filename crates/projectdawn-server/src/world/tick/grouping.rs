//! Group intents (steps 4hb to 4hbc): invites, accepts, leaves, kicks, the loot mode,
//! auto-split notices and leadership, each fanned to the members.

use super::*;

pub(super) fn apply_group_intents(world: &mut WorldState, q: &mut Intents, _now: Instant) {
    let WorldState {
        server,
        connections,
        group_manager,
        ..
    } = world;
    let Intents {
        group_invite_intents,
        group_accept_intents,
        group_leave_intents,
        group_kick_intents,
        group_loot_mode_intents,
        group_pass_leadership_intents,
        autosplit_notice_intents,
        ..
    } = q;
    // 4hb. Track 6 sub-task 5 — group-intent processing.
    //      Invite: resolve target_name → char_id, record pending
    //      invite, forward GroupInvited to invitee.
    //      Accept: GroupManager.accept; fan GroupRoster to all
    //      members on success.
    //      Leave: GroupManager.leave; fan roster (or empty for
    //      dissolved) to remaining + the leaver.
    //      Kick: leader-only action; remove target; fan rosters.
    // Helper to fan the current roster of a group with names
    // looked up from the connections map. Empty members = group
    // dissolved (last-member signal).
    let fan_roster = |srv: &mut renet::RenetServer,
                      conns: &HashMap<ClientId, PerConnection>,
                      gm: &GroupManager,
                      gid: groups::GroupId,
                      also_notify_dissolved: Option<ClientId>| {
        if let Some(g) = gm.groups.get(&gid) {
            let members_with_names: Vec<(u64, String)> = g
                .members
                .iter()
                .filter_map(|m| conns.get(m).map(|c| (*m, c.name.clone())))
                .collect();
            let recipients: Vec<ClientId> = g.members.clone();
            handlers::fan_group_roster(
                srv,
                &recipients,
                gid,
                g.leader,
                members_with_names,
                g.loot_mode.to_u8(),
            );
            // Seed the group panel rows (playtest 2026-08-27: rows
            // build zeroed client-side and resources otherwise fan
            // only on CHANGE, so a full-HP idle member showed blank
            // bars until something moved). One shot of the ordinary
            // resource updates per member, sent to the group, fills
            // every row the moment the roster lands. Existing
            // message types; no protocol change.
            for m in &g.members {
                if let Some(c) = conns.get(m) {
                    handlers::fan_out_resources(srv, &recipients, c);
                }
            }
        } else if let Some(last) = also_notify_dissolved {
            // Group dissolved — send an empty roster to the
            // last member as a "your group dissolved" signal.
            handlers::fan_group_roster(srv, std::slice::from_ref(&last), gid, last, Vec::new(), 0);
        }
    };

    for intent in group_invite_intents.drain(..) {
        // Resolve target by name (case-insensitive). The
        // characters table has a NOCASE collation on name.
        let target_cid = connections
            .iter()
            .find(|(_, c)| c.name.eq_ignore_ascii_case(&intent.target_name))
            .map(|(id, _)| *id);
        let Some(target_cid) = target_cid else {
            tracing::debug!(
                inviter = intent.inviter,
                target = %intent.target_name,
                "GroupInvite — target offline or unknown"
            );
            // The client already printed "Invited X to your group." A
            // misspelled or offline name otherwise produces nothing at all,
            // on either side.
            handlers::send_refusal(
                &mut *server,
                intent.inviter as ClientId,
                &format!("{} isn't online.", intent.target_name),
            );
            continue;
        };
        if target_cid as u64 == intent.inviter {
            continue; // can't invite self
        }
        let from_name = connections
            .get(&(intent.inviter as ClientId))
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let _gid = group_manager.record_invite(intent.inviter as ClientId, target_cid);
        handlers::send_group_invited(&mut *server, target_cid, intent.inviter, from_name);
        tracing::info!(
            inviter = intent.inviter,
            invitee = target_cid as u64,
            "GroupInvite recorded; GroupInvited forwarded"
        );
    }

    for intent in group_accept_intents.drain(..) {
        let invitee_cid = intent.invitee as ClientId;
        let from_cid = intent.from as ClientId;
        if let Some(gid) = group_manager.accept(invitee_cid, from_cid) {
            tracing::info!(
                invitee = intent.invitee,
                inviter = intent.from,
                gid,
                "GroupAccept — invitee joined"
            );
            fan_roster(&mut *server, &connections, &group_manager, gid, None);
        } else {
            tracing::debug!(
                invitee = intent.invitee,
                inviter = intent.from,
                "GroupAccept rejected (no pending invite / already in a group)"
            );
        }
    }

    for intent in group_leave_intents.drain(..) {
        let cid = intent.member as ClientId;
        if let Some((gid, remaining, dissolved)) = group_manager.leave(cid) {
            tracing::info!(
                member = intent.member,
                gid,
                remaining = remaining.len(),
                dissolved,
                "GroupLeave processed"
            );
            // The leaver always gets an empty roster so their HUD clears.
            handlers::fan_group_roster(
                &mut *server,
                std::slice::from_ref(&cid),
                gid,
                cid,
                Vec::new(),
                0,
            );
            if dissolved {
                // Survivors (0 or 1) also need a dissolution notice
                // so their HUD clears.
                for m in &remaining {
                    handlers::fan_group_roster(
                        &mut *server,
                        std::slice::from_ref(m),
                        gid,
                        *m,
                        Vec::new(),
                        0,
                    );
                }
            } else {
                fan_roster(&mut *server, &connections, &group_manager, gid, None);
            }
        }
    }

    for intent in group_kick_intents.drain(..) {
        let leader_cid = intent.leader as ClientId;
        let Some(group) = group_manager.group_of(leader_cid) else {
            // Previously no log AND no reply, so a failed /kick was
            // invisible on both sides and untriageable from server.log.
            tracing::debug!(leader = intent.leader, "GroupKick — not in a group");
            handlers::send_refusal(&mut *server, leader_cid, "You aren't in a group.");
            continue;
        };
        if group.leader != leader_cid {
            // Only a forged client reaches this (the UI hides kick for
            // non-leaders), so log for triage but say nothing: a reply is an
            // oracle with no honest beneficiary.
            tracing::debug!(leader = intent.leader, "GroupKick — not the leader");
            continue;
        }
        let gid = group.id;
        // Resolve target name within the group's roster.
        let target_cid: Option<ClientId> = group.members.iter().copied().find(|m| {
            connections
                .get(m)
                .map(|c| c.name.eq_ignore_ascii_case(&intent.target_name))
                .unwrap_or(false)
        });
        let Some(target_cid) = target_cid else {
            // Had neither log nor reply: a misspelled /kick simply did
            // nothing, visibly or in server.log.
            tracing::debug!(
                leader = intent.leader,
                target = %intent.target_name,
                "GroupKick — no such member in the group"
            );
            handlers::send_refusal(
                &mut *server,
                leader_cid,
                &format!("{} isn't in your group.", intent.target_name),
            );
            continue;
        };
        if target_cid == leader_cid {
            // Reachable from the UI by typing your own name, unlike the
            // non-leader case, so it earns a reply.
            handlers::send_refusal(
                &mut *server,
                leader_cid,
                "Use /leave to leave your own group.",
            );
            continue;
        }
        if let Some((_gid, remaining, dissolved)) = group_manager.leave(target_cid) {
            tracing::info!(
                leader = intent.leader,
                kicked = target_cid as u64,
                gid,
                remaining = remaining.len(),
                dissolved,
                "GroupKick processed"
            );
            // Notify the kicked member their group ended (from their POV).
            handlers::fan_group_roster(
                &mut *server,
                std::slice::from_ref(&target_cid),
                gid,
                target_cid,
                Vec::new(),
                0,
            );
            if dissolved {
                // Survivors (0 or 1) also need a dissolution notice.
                for m in &remaining {
                    handlers::fan_group_roster(
                        &mut *server,
                        std::slice::from_ref(m),
                        gid,
                        *m,
                        Vec::new(),
                        0,
                    );
                }
            } else {
                fan_roster(&mut *server, &connections, &group_manager, gid, None);
            }
        }
    }

    // 4hbb. PD_W0014 — leader sets the group's loot distribution
    //       mode. Validate the sender is the leader, set the mode,
    //       and re-fan the roster so every member sees it.
    for intent in group_loot_mode_intents.drain(..) {
        let leader_cid = intent.leader as ClientId;
        let gid = match group_manager.group_of(leader_cid) {
            Some(g) if g.leader == leader_cid => g.id,
            _ => continue, // not grouped, or not the leader
        };
        if let Some(g) = group_manager.groups.get_mut(&gid) {
            g.loot_mode = groups::LootMode::from_u8(intent.mode);
        }
        tracing::info!(
            leader = intent.leader,
            gid,
            mode = intent.mode,
            "group loot mode set"
        );
        fan_roster(&mut *server, &connections, &group_manager, gid, None);
    }

    // 4hbb-bis. PD_W0014 — a player toggled /autosplit. Fan a one-line
    //       notice to their group-mates (transparency: it changes
    //       whether the group shares coin from that member's loots).
    //       The toggler already echoes locally (hud.gd), so they're
    //       excluded here; solo players get no notice at all.
    for intent in autosplit_notice_intents.drain(..) {
        let toggler_cid = intent.char_id as ClientId;
        let gid = match group_manager.group_of(toggler_cid) {
            Some(g) => g.id,
            None => continue, // solo → nobody to notify
        };
        let toggler_name = connections
            .get(&toggler_cid)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "Someone".to_string());
        let text = format!(
            "{} set auto-split {}.",
            toggler_name,
            if intent.on { "on" } else { "off" }
        );
        let members: Vec<ClientId> = match group_manager.groups.get(&gid) {
            Some(g) => g.members.clone(),
            None => continue,
        };
        for member_cid in members {
            if member_cid == toggler_cid {
                continue; // toggler echoed locally already
            }
            if connections.get(&member_cid).map_or(false, |c| c.in_world) {
                handlers::send_group_notice(&mut *server, member_cid, text.clone());
            }
        }
    }

    // 4hbc. PD_W0014 — leader hands leadership to a member. Validate
    //       (current leader → existing member) and re-fan the roster
    //       so both old and new leader see the change. Fixes the
    //       launcher-mode gap where pass-leadership was local-only.
    for intent in group_pass_leadership_intents.drain(..) {
        if let Some(gid) =
            group_manager.pass_leadership(intent.leader as ClientId, intent.new_leader as ClientId)
        {
            tracing::info!(
                from = intent.leader,
                to = intent.new_leader,
                gid,
                "group leadership passed"
            );
            fan_roster(&mut *server, &connections, &group_manager, gid, None);
        }
    }
}
