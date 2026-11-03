//! The message drain (step 3): every client's packets decoded and handed to
//! `handlers::handle_message`, whose outcomes fill this tick's `Intents` for the steps
//! after it.

use super::*;
use protocol::world::ClientWorldMsg;

pub(super) fn drain_messages(world: &mut WorldState, q: &mut Intents, now: Instant) {
    let WorldState {
        server,
        connections,
        ..
    } = world;
    // 3. Drain incoming application messages on each channel for each client.
    let client_ids: Vec<ClientId> = server.clients_id_iter().collect();
    for client_id in client_ids {
        // Skip clients whose Connected event is in the queue but whose
        // PerConnection row hasn't been built yet (load_character failed
        // and we already queued the kick).
        let Some(conn) = connections.get(&client_id) else {
            continue;
        };
        // Every line logged while this client's messages are handled (and
        // their outcomes queued) names who sent them.
        let _span = tracing::info_span!(
            "client",
            char_id = conn.char_id,
            account_id = conn.account_id
        )
        .entered();
        // A client's batch is all or nothing (`handle_batch`): if any of its
        // messages panics, the connection is put back as it was before the
        // batch and the outcomes are discarded, then `session::drop_panicked`
        // drops the client as a cut connection (the body lingers and is saved
        // at the reap, as for any unclean drop).
        let msgs = receive_batch(&mut *server, client_id);
        let conn = connections.get_mut(&client_id).expect("checked above");
        let Some(outcomes) = handle_batch(conn, client_id, msgs, |conn, msg| {
            handlers::handle_message(&mut *server, conn, client_id, msg, now)
        }) else {
            q.panicked.push(client_id);
            continue;
        };
        for outcome in outcomes {
            route_outcome(&mut *server, connections, q, client_id, outcome);
        }
    }
}

/// Every message a client has waiting this tick, decoded, in the order the
/// drain has always taken them (the system channel, then positions).
pub(super) fn receive_batch(server: &mut RenetServer, client_id: ClientId) -> Vec<ClientWorldMsg> {
    let mut msgs = Vec::new();
    for &channel in &[CHANNEL_SYSTEM, CHANNEL_POSITION] {
        while let Some(bytes) = server.receive_message(client_id, channel) {
            if let Some(msg) = handlers::decode_client(&bytes, client_id) {
                msgs.push(msg);
            }
        }
    }
    msgs
}

/// Handle one client's batch of messages, all or nothing. The connection is
/// copied before the first message; if any message panics (a bug, never a
/// rule), the copy is put back and `None` is returned, so the tick is as if
/// none of this client's messages had arrived. Nothing else can need undoing:
/// `handle_message` touches the sender's own connection and sends only to the
/// sender, and every move of an item or coin between players happens in the
/// steps after the drain, from the outcomes the caller discards. A handler
/// that reached any other state would break this.
pub(super) fn handle_batch<F>(
    conn: &mut PerConnection,
    client_id: ClientId,
    msgs: Vec<ClientWorldMsg>,
    mut handle: F,
) -> Option<Vec<Outcome>>
where
    F: FnMut(&mut PerConnection, ClientWorldMsg) -> Outcome,
{
    if msgs.is_empty() {
        return Some(Vec::new());
    }
    let before = conn.clone();
    let mut outcomes = Vec::with_capacity(msgs.len());
    for msg in msgs {
        match contain_panic(conn, client_id, |conn| handle(conn, msg)) {
            Some(outcome) => outcomes.push(outcome),
            None => {
                *conn = before;
                return None;
            }
        }
    }
    Some(outcomes)
}

/// Run one handler call with a panic caught. The log line carries no message
/// name: the panic's own report, just above it in the journal, names the
/// file and line.
fn contain_panic<F>(conn: &mut PerConnection, client_id: ClientId, handle: F) -> Option<Outcome>
where
    F: FnOnce(&mut PerConnection) -> Outcome,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(&mut *conn))) {
        Ok(outcome) => Some(outcome),
        Err(payload) => {
            let reason = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic with no message".to_string());
            tracing::error!(
                %client_id,
                char_id = conn.char_id,
                %reason,
                "handling a message panicked: the player's tick is undone and they drop as if cut off, the world carries on"
            );
            None
        }
    }
}

/// Put one handled message's outcome on this tick's queues.
fn route_outcome(
    server: &mut RenetServer,
    connections: &HashMap<ClientId, PerConnection>,
    q: &mut Intents,
    client_id: ClientId,
    outcome: Outcome,
) {
    let Intents {
        to_disconnect,
        newly_in_world,
        cast_fanouts,
        jump_fanouts,
        buff_fanouts,
        combat_fanouts,
        death_fanouts,
        player_target_fanouts,
        chat_fanouts,
        inspect_intents,
        attack_intents,
        use_skill_intents,
        cast_spell_intents,
        cast_overflow_told,
        group_invite_intents,
        group_accept_intents,
        group_leave_intents,
        group_kick_intents,
        group_loot_mode_intents,
        group_pass_leadership_intents,
        autosplit_notice_intents,
        loot_intents,
        loot_to_cursor_intents,
        resurrect_accept_intents,
        bind_intents,
        dev_spawn_intents,
        dev_despawn_intents,
        spell_gem_saves,
        complete_quest_intents,
        quest_lifecycle_intents,
        consider_intents,
        pet_command_intents,
        move_item_intents,
        trade_intents,
        split_stack_intents,
        drop_item_intents,
        destroy_item_intents,
        use_consumable_intents,
        gm_give_intents,
        equip_item_intents,
        unequip_item_intents,
        buy_item_intents,
        sell_item_intents,
        bank_deposit_intents,
        bank_withdraw_intents,
        bank_exchange_intents,
        bank_store_item_intents,
        bank_withdraw_item_intents,
        ..
    } = q;
    match outcome {
        Outcome::Disconnect => to_disconnect.push(client_id),
        Outcome::JustEnteredWorld => newly_in_world.push(client_id),
        Outcome::CastStartFanOut {
            spell_name,
            duration,
        } => {
            cast_fanouts.push((
                client_id,
                CastEvent::Start {
                    spell_name,
                    duration,
                },
            ));
        }
        Outcome::JumpFanOut => jump_fanouts.push(client_id),
        Outcome::CastCompleteFanOut { spell_name } => {
            cast_fanouts.push((client_id, CastEvent::Complete { spell_name }));
        }
        Outcome::CastFailFanOut { reason } => {
            cast_fanouts.push((client_id, CastEvent::Fail { reason }));
        }
        Outcome::BuffSnapshotFanOut => {
            if !buff_fanouts.contains(&client_id) {
                buff_fanouts.push(client_id);
            }
        }
        Outcome::HitFanOut {
            target,
            amount,
            crit,
            dmg_type,
        } => {
            combat_fanouts.push((
                client_id,
                CombatEvent::Hit {
                    target,
                    amount,
                    crit,
                    dmg_type,
                },
            ));
        }
        Outcome::MissFanOut { target } => {
            combat_fanouts.push((client_id, CombatEvent::Miss { target }));
        }
        Outcome::EvadeFanOut { target } => {
            combat_fanouts.push((client_id, CombatEvent::Evade { target }));
        }
        Outcome::DeathFanOut => {
            if !death_fanouts.contains(&client_id) {
                death_fanouts.push(client_id);
            }
        }
        Outcome::PlayerTargetFanOut { target } => {
            // Track 22.H — queue for the post-dispatch
            // sweep where in_world_recipients_now is
            // computed. Drain happens alongside the
            // other fan-outs.
            player_target_fanouts.push((client_id, target));
        }
        Outcome::ChatFanOut {
            channel,
            text,
            speaker,
            target_name,
        } => {
            chat_fanouts.push((client_id, channel, text, speaker, target_name));
        }
        Outcome::InspectIntent { target_char_id } => {
            inspect_intents.push((client_id, target_char_id));
        }
        Outcome::AttackIntent {
            attacker,
            target_id,
            is_offhand,
            dmg_type,
        } => {
            attack_intents.push(AttackIntent {
                attacker,
                target_id,
                is_offhand,
                dmg_type,
                skill: None,
            });
        }
        Outcome::UseSkillIntent {
            user,
            skill_name,
            target_id,
        } => {
            // One UseSkill per user per tick (the CastSpell
            // rule): the handler stamps no cooldown, so every
            // copy in a forged datagram would pass it and be
            // answered, a reply amplifier. An honest client
            // never sends two in one tick; extras are dropped.
            if use_skill_intents.iter().any(|i| i.user == user) {
                tracing::debug!(user, skill = %skill_name, "UseSkill dropped — one per tick");
            } else {
                use_skill_intents.push(UseSkillIntent {
                    user,
                    skill_name,
                    target_id,
                });
            }
        }
        Outcome::CastSpellIntent {
            caster,
            spell_name,
            target_id,
            cast_name_at_dispatch,
            cast_set_at_at_dispatch,
            cast_start_pos_at_dispatch,
        } => {
            // One CastSpell per caster per tick. Every CastSpell
            // in a batch reads the same CastStart cache (it is
            // cleared only when a cast resolves), so one cast
            // bar used to authorise any number of them in a
            // single datagram, and a refused cast is free, so
            // nothing else metered them. An honest client
            // cannot finish two casts inside 50 ms. The first
            // extra is answered (the client spent its mana
            // optimistically and needs the correction); the
            // rest of a flood are dropped in silence.
            if cast_spell_intents.iter().any(|i| i.caster == caster) {
                if !cast_overflow_told.contains(&client_id) {
                    cast_overflow_told.push(client_id);
                    handlers::fan_out_cast_fail(
                        &mut *server,
                        &[client_id],
                        caster,
                        "One spell at a time.".to_string(),
                    );
                    if let Some(c) = connections.get(&client_id) {
                        handlers::fan_out_mana_update(
                            &mut *server,
                            &[client_id],
                            caster,
                            c.mp,
                            c.max_mp,
                        );
                    }
                }
            } else {
                cast_spell_intents.push(CastSpellIntent {
                    caster,
                    spell_name,
                    target_id,
                    cast_name_at_dispatch,
                    cast_set_at_at_dispatch,
                    cast_start_pos_at_dispatch,
                });
            }
        }
        Outcome::PetCommandIntent {
            owner,
            command,
            target_id,
        } => {
            pet_command_intents.push(PetCommandI {
                owner,
                command,
                target_id,
            });
        }
        Outcome::MoveItemIntent {
            owner,
            src_location,
            src_slot,
            dst_location,
            dst_slot,
        } => {
            move_item_intents.push(MoveItemI {
                owner,
                src_location,
                src_slot,
                dst_location,
                dst_slot,
            });
        }
        Outcome::TradeRequestIntent { requester, target } => {
            trade_intents.push(TradeI::Request { requester, target });
        }
        Outcome::TradeOfferItemIntent {
            owner,
            window_slot,
            from_location,
            from_slot,
        } => {
            trade_intents.push(TradeI::OfferItem {
                owner,
                window_slot,
                from_location,
                from_slot,
            });
        }
        Outcome::TradeRetrieveItemIntent { owner, window_slot } => {
            trade_intents.push(TradeI::RetrieveItem { owner, window_slot });
        }
        Outcome::TradeOfferCoinsIntent { owner, coins } => {
            trade_intents.push(TradeI::OfferCoins { owner, coins });
        }
        Outcome::TradeAcceptIntent { owner } => {
            trade_intents.push(TradeI::Accept { owner });
        }
        Outcome::TradeCancelIntent { owner } => {
            trade_intents.push(TradeI::Cancel { owner });
        }
        Outcome::SplitStackIntent {
            owner,
            src_location,
            src_slot,
            dst_location,
            dst_slot,
            count,
        } => {
            split_stack_intents.push(SplitStackI {
                owner,
                src_location,
                src_slot,
                dst_location,
                dst_slot,
                count,
            });
        }
        Outcome::DropItemIntent {
            owner,
            location,
            slot,
            count,
        } => {
            drop_item_intents.push(DropItemI {
                owner,
                location,
                slot,
                count,
            });
        }
        Outcome::DestroyItemIntent {
            owner,
            location,
            slot,
            count,
        } => {
            destroy_item_intents.push(DestroyItemI {
                owner,
                location,
                slot,
                count,
            });
        }
        Outcome::UseConsumableIntent {
            owner,
            location,
            slot,
        } => {
            use_consumable_intents.push(UseConsumableI {
                owner,
                location,
                slot,
            });
        }
        Outcome::GmGiveIntent {
            owner,
            item_name,
            qty,
        } => {
            gm_give_intents.push(GmGiveI {
                owner,
                item_name,
                qty,
            });
        }
        Outcome::EquipItemIntent {
            owner,
            src_location,
            src_slot,
            equip_slot,
        } => {
            equip_item_intents.push(EquipItemI {
                owner,
                src_location,
                src_slot,
                equip_slot,
            });
        }
        Outcome::UnequipItemIntent {
            owner,
            equip_slot,
            dst_location,
            dst_slot,
        } => {
            unequip_item_intents.push(UnequipItemI {
                owner,
                equip_slot,
                dst_location,
                dst_slot,
            });
        }
        Outcome::BuyItemIntent {
            owner,
            vendor_id,
            item_name,
            qty,
        } => {
            buy_item_intents.push(BuyItemI {
                owner,
                vendor_id,
                item_name,
                qty,
            });
        }
        Outcome::SellItemIntent { owner, slot, qty } => {
            sell_item_intents.push(SellItemI { owner, slot, qty });
        }
        Outcome::BankDepositIntent { owner, coins } => {
            bank_deposit_intents.push(BankDepositI { owner, coins });
        }
        Outcome::BankWithdrawIntent { owner, coins } => {
            bank_withdraw_intents.push(BankWithdrawI { owner, coins });
        }
        Outcome::BankExchangeIntent {
            owner,
            from_tier,
            to_tier,
            qty,
        } => {
            bank_exchange_intents.push(BankExchangeI {
                owner,
                from_tier,
                to_tier,
                qty,
            });
        }
        Outcome::BankStoreItemIntent {
            owner,
            src_location,
            src_slot,
            shared,
        } => {
            bank_store_item_intents.push(BankStoreItemI {
                owner,
                src_location,
                src_slot,
                shared,
            });
        }
        Outcome::BankWithdrawItemIntent {
            owner,
            shared,
            vault_slot,
        } => {
            bank_withdraw_item_intents.push(BankWithdrawItemI {
                owner,
                shared,
                vault_slot,
            });
        }
        Outcome::GroupInviteIntent {
            inviter,
            target_name,
        } => {
            group_invite_intents.push(GroupInviteI {
                inviter,
                target_name,
            });
        }
        Outcome::GroupAcceptIntent { invitee, from } => {
            group_accept_intents.push(GroupAcceptI { invitee, from });
        }
        Outcome::GroupLeaveIntent { member } => {
            group_leave_intents.push(GroupLeaveI { member });
        }
        Outcome::GroupKickIntent {
            leader,
            target_name,
        } => {
            group_kick_intents.push(GroupKickI {
                leader,
                target_name,
            });
        }
        Outcome::SetGroupLootModeIntent { leader, mode } => {
            group_loot_mode_intents.push(GroupLootModeI { leader, mode });
        }
        Outcome::PassLeadershipIntent { leader, new_leader } => {
            group_pass_leadership_intents.push(GroupPassLeadershipI { leader, new_leader });
        }
        Outcome::AutosplitNoticeIntent { char_id, on } => {
            autosplit_notice_intents.push(AutosplitNoticeI { char_id, on });
        }
        Outcome::LootItemIntent {
            looter,
            bag_id,
            slot,
        } => {
            loot_intents.push(LootIntent {
                looter,
                bag_id,
                slot: Some(slot),
            });
        }
        Outcome::LootAllIntent { looter, bag_id } => {
            loot_intents.push(LootIntent {
                looter,
                bag_id,
                slot: None,
            });
        }
        Outcome::LootToCursorIntent { looter, bag_id } => {
            loot_to_cursor_intents.push((looter, bag_id));
        }
        Outcome::ResurrectAcceptIntent {
            responder,
            corpse_id,
            accept,
        } => {
            resurrect_accept_intents.push((responder, corpse_id, accept));
        }
        Outcome::BindIntent { char_id, zone, pos } => {
            bind_intents.push((char_id, zone, pos));
        }
        Outcome::RespawnTeleport { pos } => {
            // Snap the client to the bind point. Without this the
            // client keeps rendering the death site until the next
            // Position broadcast drags it back — the teleport IS
            // the respawn from the player's point of view.
            handlers::send_teleport(&mut *server, client_id, pos);
        }
        Outcome::DevSpawnMobIntent { pos, mob } => {
            dev_spawn_intents.push((pos, mob));
        }
        Outcome::DevDespawnEnemiesIntent { requester, pos } => {
            dev_despawn_intents.push((requester, pos));
        }
        Outcome::ConsiderIntent {
            requester,
            target_id,
        } => {
            consider_intents.push((requester, target_id));
        }
        Outcome::SpellGemsChanged {
            char_id,
            gems,
            seeded,
        } => {
            spell_gem_saves.push((char_id, gems, seeded));
        }
        Outcome::CompleteQuestIntent {
            responder,
            quest_id,
        } => {
            complete_quest_intents.push((responder, quest_id));
        }
        Outcome::AcceptQuestIntent {
            responder,
            quest_id,
        } => {
            quest_lifecycle_intents.push((responder, QuestLifecycle::Accept, quest_id));
        }
        Outcome::AbandonQuestIntent {
            responder,
            quest_id,
        } => {
            quest_lifecycle_intents.push((responder, QuestLifecycle::Abandon, quest_id));
        }
        Outcome::Continue => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::world::ChatChannel;

    const CLIENT: ClientId = 7;

    fn say(text: &str) -> ClientWorldMsg {
        ClientWorldMsg::Chat {
            channel: ChatChannel::Say,
            text: text.into(),
            target_name: None,
        }
    }

    /// A handler that changes the connection, as a real one does, and panics
    /// on the line "boom".
    fn handler(conn: &mut PerConnection, msg: ClientWorldMsg) -> Outcome {
        conn.hp -= 10.0;
        conn.gm_cmd_refused += 1;
        if let ClientWorldMsg::Chat { text, .. } = msg {
            if text == "boom" {
                panic!("a planted bug");
            }
        }
        Outcome::Continue
    }

    #[test]
    fn a_batch_without_a_panic_keeps_every_change() {
        let mut conn = PerConnection::from_spawn(crate::world::fuzzing::spawn(), Instant::now());
        let hp = conn.hp;
        let outcomes = handle_batch(&mut conn, CLIENT, vec![say("a"), say("b")], handler)
            .expect("no panic, so the outcomes come back");
        assert_eq!(outcomes.len(), 2);
        assert_eq!(conn.hp, hp - 20.0);
    }

    /// The whole batch is undone, the messages handled before the panic
    /// included: a dead player who panics stays dead, a GM's audit entries
    /// for commands that never took effect go with them.
    #[test]
    fn a_panic_puts_the_connection_back_as_it_was_before_the_batch() {
        let mut conn = PerConnection::from_spawn(crate::world::fuzzing::spawn(), Instant::now());
        conn.hp = 0.0;
        conn.death_processed = true;
        let before = format!("{conn:?}");
        let result = handle_batch(
            &mut conn,
            CLIENT,
            vec![say("a"), say("boom"), say("c")],
            handler,
        );
        assert!(result.is_none(), "a panic discards the batch");
        assert_eq!(
            format!("{conn:?}"),
            before,
            "every field is as it was before the batch"
        );
    }
}
