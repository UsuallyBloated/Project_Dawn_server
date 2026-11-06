//! The 20 Hz world loop. `run` owns every connection, mob, bag and corpse
//! exclusively (no locking) and drives the renet transport. Each tick it builds an
//! `Intents`, drains the clients' messages into it, and runs the steps below in a fixed
//! order, one function each, every one reading and writing the `WorldState` (state.rs).
//! The step numbers in the comments (3, 4a, 4h, 5b ...) are the order they were added,
//! not the order they run; the calls in `run` are the order.

use super::{
    active_skills,
    aoi::{self, AoiGrid},
    buffs::{self, ActiveBuff},
    clock, combat,
    connection::{PerConnection, Vec3f},
    entity::{self, ActiveCc, EnemyState, Entity, HitIntent},
    groups::{self, GroupManager},
    handlers::{self, Outcome},
    hunger, inventory, items,
    ledger::{self, ItemAction},
    loot::{self, LootBag},
    persistence, pet_templates, regen, skills,
    spawn_points::Spawner,
    spells, Stop, ATTACK_RANGE_TOLERANCE, BAN_SWEEP_INTERVAL, CAMP_SECS, CHANNEL_POSITION,
    CHANNEL_SYSTEM, CHECKPOINT_INTERVAL, ENEMY_DESPAWN_LINGER_SECS, ENEMY_FAN_REPORT_INTERVAL,
    GROUP_SHARE_RANGE, INSPECT_RANGE, KICK_FLUSH_GRACE, LINKDEAD_SECS, LOOT_BAG_LINGER_SECS,
    LOOT_PICKUP_RANGE, RANGED_ATTACK_RANGE, RESTART_FOR_ADDRESS_CHANGE, RUN_SPEED,
    STALE_MOVE_THRESHOLD, TICK_DT,
};
use crate::{db, Config};
use protocol::world::{DamageType, EntityId, IdKind, KickCode};
use rand::Rng;
use renet::{ClientId, RenetServer, ServerEvent};
use renet_netcode::NetcodeServerTransport;
use sqlx::SqlitePool;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

// The world's state and one tick's queues.
mod state;
use state::*;

// The steps, in the order `run` calls them.
mod banking;
mod binds_and_res;
mod buff_tick;
mod camp;
mod casting;
mod death_sweep;
mod enemy_ai;
mod fanout;
mod grouping;
mod intake;
mod inventory_ops;
mod looting;
mod melee;
mod movement;
mod pet_commands;
mod position_fan;
mod quest_steps;
mod regen_tick;
mod session;
mod spawning;
mod trading;
mod upkeep;
mod vendors;

// The rules several steps share.
mod cast_rules;
mod kills;
mod pet_life;
mod spell_buffs;
mod swing;

// The shared rules by their bare names: every step does `use super::*`.
use cast_rules::{
    cast_resolution_lost, cast_target_refusal, refund_spell_cost, refuse_cast, roll_cast_interrupt,
    song_pulse, stop_song, tell_interrupt_charge, InterruptOutcome, ResolvedTarget,
};
use fanout::{despawn_recipients, fan_entity_cell_crossing, say_recipients};
use kills::{
    apply_spell_damage_to_enemy, award_kill, clear_player_dots_from, kill_enemy, wipe_hate,
    FactionKill,
};
use pet_life::{despawn_owned_pets, pet_owner_cid, return_charmed_mob, summon_scaled_pet};
use session::{shut_down_world, PENDING_SAVE_GIVE_UP, PENDING_SAVE_RETRY};
use spell_buffs::{
    apply_buff, apply_mp_regen_exclusive, apply_pet_spell_buffs, apply_player_spell_buffs,
    apply_stat_buff, fan_out_pet_buff_snapshot,
};
use swing::{
    carried_haste, honest_swing_pace, min_swing_interval_secs, skill_swing, stamp_swing_haste,
    swing_too_fast, train_swing_skills, FIST_WEAPON_DELAY,
};
use trading::trade_locked;

/// Wall-clock unix seconds (the logout stamp for NO RENT items; the ledger's
/// rows are stamped by the same clock).
fn unix_now_secs() -> i64 {
    ledger::unix_now_secs()
}

pub async fn run(
    cfg: Arc<Config>,
    pool: SqlitePool,
    server: RenetServer,
    mut transport: NetcodeServerTransport,
    stop: tokio::sync::watch::Receiver<Stop>,
) -> anyhow::Result<()> {
    // Track 18.1 — build the spell discipline lookup once at startup
    // so casting-skill advance dispatch is a HashMap hit per cast.
    skills::init_discipline_map();

    let connections: HashMap<ClientId, PerConnection> = HashMap::new();
    // Track 7 — AOI spatial index. Tracks which grid cell each in_world
    // entity (player, enemy, loot bag) occupies so position broadcasts
    // and EntitySpawn/Despawn can be filtered to the 3×3 neighbourhood
    // instead of broadcasting to every in_world client.
    let mut aoi = AoiGrid::new();
    let mut interval = tokio::time::interval(TICK_DT);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let last_checkpoint = Instant::now();
    // Characters whose logout save did not fully land (see reap_connection):
    // retried every PENDING_SAVE_RETRY, their accounts' relogins refused
    // until the save lands, and flushed again at shutdown.
    let pending_saves: Vec<PendingSave> = Vec::new();
    let last_pending_retry = Instant::now();
    // Bandwidth counter for the enemy Position stream (step 6b): sends in the
    // current report window, logged every ENEMY_FAN_REPORT_INTERVAL.
    let enemy_pos_sent: u64 = 0;
    let last_fan_report = Instant::now();
    // The loop's own timings (world/health.rs): each tick's work, each
    // checkpoint and the steps the slope rule refused, reported as the
    // `world health` line.
    let tick_health = super::health::TickHealth::default();
    let mut tick_started: Option<Instant> = None;
    let last_health_report = Instant::now();
    let last_ban_sweep = Instant::now();
    // The item ledger's daily sweep (world/ledger.rs): None until it has run
    // once, so the first tick runs it at boot.
    let last_ledger_sweep: Option<Instant> = None;
    let last_time_broadcast = Instant::now();
    // The ban sweep's query runs on its own task and reports back here, so
    // the world loop never waits on the database for it.
    let (ban_tx, ban_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<i64>>();
    let ban_sweep_in_flight = false;

    // Track 5 sub-task 1B — server-authoritative enemies. The spawner owns
    // respawn timers per authored spawn point; `enemies` holds the live
    // instances. AI ticking + position/HP fan-out land in 1C; this commit
    // wires spawn lifecycle + EnemySpawn fan-out only (mobs stand idle).
    let spawner = Spawner::new(Instant::now());
    let enemies: HashMap<EntityId, Entity> = HashMap::new();
    // Track 5 sub-task 4 — server-owned loot bags. Rolled and spawned
    // in step 4h's on-death branch; expire after LOOT_BAG_LINGER_SECS
    // in step 4k. 4B will add the LootItem / LootAll handlers that
    // remove items mid-life.
    let loot_bags: HashMap<EntityId, LootBag> = HashMap::new();
    // Track 6 sub-task 5 — server-authoritative group state.
    // Ephemeral; lives only for the tick loop's lifetime. Disconnect
    // removes the member; one-member-left groups dissolve.
    let group_manager = GroupManager::new();
    // PD_W0028 — server-owned trade sessions (escrow by locking; see
    // world/trade.rs). Ephemeral like groups: a crash or restart drops every
    // session and every item is exactly where it always was.
    let trade_manager = super::trade::TradeManager::new();
    // The ground (the height model, world/terrain.rs): the compiled terrain,
    // or the ramp or ridge the integration harness hands in through the
    // config. Everything that stands or moves is pinned to it from here on.
    let terrain: Arc<super::terrain::Terrain> = cfg
        .terrain
        .clone()
        .unwrap_or_else(|| Arc::new(super::terrain::compiled().clone()));

    // Corpse / resurrection Slice 1 — server-owned player corpses (persisted
    // LootBags). Load every persisted corpse BEFORE the loop (so before any login
    // is accepted, no first-player race), seed the AOI, and advance the bag-id
    // minter past the highest loaded id so a fresh bag/corpse can't reuse a
    // loaded corpse's id after this restart.
    let mut corpses: HashMap<EntityId, super::corpses::Corpse> = HashMap::new();
    match db::load_corpses(&pool).await {
        Ok(rows) => {
            let boot = Instant::now();
            let mut max_id: EntityId = 0;
            for r in rows {
                let id = r.corpse_id as EntityId;
                // Re-pinned at load: the ground may have changed since the
                // corpse was saved, and a corpse in the air or underground
                // would be out of every reach.
                let pos = terrain.ground_vec(super::connection::Vec3f::from_tuple(r.pos));
                let items: Vec<super::loot::LootItemStack> = r
                    .items
                    .into_iter()
                    .map(
                        |(item_path, count, equip_slot)| super::loot::LootItemStack {
                            item_path,
                            count,
                            equip_slot,
                        },
                    )
                    .collect();
                aoi.insert(id, aoi::cell_for(pos.x, pos.z));
                max_id = max_id.max(id);
                corpses.insert(
                    id,
                    super::corpses::Corpse::new(
                        id,
                        r.char_id,
                        r.owner_name,
                        r.zone,
                        pos,
                        items,
                        r.coins,
                        r.lost_xp,
                        r.resurrected,
                        boot,
                    ),
                );
            }
            if IdKind::of(max_id) == IdKind::Bag {
                super::loot::reserve_bag_ids_through(max_id);
            }
            tracing::info!(count = corpses.len(), "loaded persisted corpses");
        }
        Err(e) => tracing::error!(error = %e, "load_corpses failed; starting with no corpses"),
    }

    let mut world = WorldState {
        cfg,
        pool,
        server,
        connections,
        aoi,
        last_checkpoint,
        pending_saves,
        last_pending_retry,
        enemy_pos_sent,
        last_fan_report,
        tick_health,
        last_health_report,
        last_ban_sweep,
        last_ledger_sweep,
        last_time_broadcast,
        ban_tx,
        ban_rx,
        ban_sweep_in_flight,
        spawner,
        enemies,
        loot_bags,
        group_manager,
        trade_manager,
        corpses,
        terrain,
    };

    loop {
        // The previous tick's work ends here, before the wait for the next:
        // measured at the top so every path out of the body (a `continue`
        // included) is counted.
        if let Some(started) = tick_started {
            world.tick_health.record_tick(started.elapsed());
        }
        interval.tick().await;
        let now = Instant::now();
        let mut q = Intents::default();
        tick_started = Some(now);
        let dt = TICK_DT.as_secs_f32();

        // 0. A stop request (SIGTERM from systemd or Ctrl-C; or the address
        //    watch, when the advertised name moved) ends the world cleanly:
        //    everyone is saved, everyone is told why, then the loop returns.
        //    Checked at the top of a tick, so the state saved is whole, never
        //    half a tick.
        let stop_request = *stop.borrow();
        if stop_request != Stop::Run {
            let unsaved = shut_down_world(
                &mut world.server,
                &mut transport,
                &mut world.connections,
                &mut world.pending_saves,
                &world.pool,
                stop_request,
            )
            .await;
            if unsaved > 0 {
                // A non-zero exit, so the journal and systemd both show it.
                anyhow::bail!("{unsaved} character(s) could not be saved at shutdown");
            }
            if stop_request == Stop::AddressChanged {
                // Also non-zero, on purpose: systemd runs `Restart=on-failure`,
                // and this exit exists to be restarted on the new address.
                anyhow::bail!("restarting for the new world address (everyone saved and told)");
            }
            return Ok(());
        }

        // 1. Advance renet's per-connection clocks, then the transport (UDP
        //    packets in, netcode handshakes, the clients' acks handed to renet).
        //    That is renet's documented order, and the one its RTT needs: an
        //    ack is timed against the connection's clock as the transport hands
        //    it over, and that clock moves nowhere but in `server.update`. With
        //    the transport first (until 2026-10-30), an ack read in the tick
        //    after its packet went out measured zero, so the `player link`
        //    line read rtt_ms=0.0 for everyone. An RTT now reads in whole
        //    ticks, rounded up: 50 ms is the floor. The same clock drives the
        //    reliable resends: a lost packet is resent only when the clock
        //    passes its resend time, and this call was missing until
        //    2026-10-12, so a lost reliable packet (an inventory delta, a chat
        //    line, a kick) was never resent; invisible on a lossless tailnet,
        //    not on the internet.
        world.server.update(TICK_DT);
        if let Err(e) = transport.update(TICK_DT, &mut world.server) {
            tracing::warn!(error = %e, "transport update error");
        }

        session::handle_transport_events(&mut world, &mut q, now, &transport).await;

        intake::drain_messages(&mut world, &mut q, now);
        session::drop_panicked(&mut world, &mut q, now);

        session::flush_gm_audit(&mut world, &mut q, now).await;

        session::reap_timed_out(&mut world, &mut q, now);

        session::reap_linkdead(&mut world, &mut q, now).await;

        // The players in the world, listed once: nothing after the session
        // steps above adds a connection, removes one or puts one in the
        // world, so the list holds for every fan-out in the rest of the tick.
        let in_world_recipients_now = session::in_world_ids(&world.connections);

        fanout::seed_joiners(&mut world, &mut q, now);

        fanout::fan_out_events(&mut world, &mut q, now, &in_world_recipients_now);

        spawning::run_spawning(&mut world, &mut q, now, &in_world_recipients_now);

        melee::apply_active_skills(&mut world, &mut q, now, &in_world_recipients_now);

        melee::apply_attacks(&mut world, &mut q, now, &in_world_recipients_now);

        casting::apply_casts(&mut world, &mut q, now, &in_world_recipients_now);

        grouping::apply_group_intents(&mut world, &mut q, now);

        pet_commands::apply_pet_commands(&mut world, &mut q, now);

        inventory_ops::apply_moves(&mut world, &mut q, now);

        trading::apply_trade_intents(&mut world, &mut q, now).await;

        trading::sweep_trades(&mut world, &mut q, now);

        inventory_ops::apply_splits(&mut world, &mut q, now);

        inventory_ops::apply_drops(&mut world, &mut q, now, &in_world_recipients_now).await;

        inventory_ops::apply_equips(&mut world, &mut q, now, &in_world_recipients_now);

        inventory_ops::apply_unequips(&mut world, &mut q, now, &in_world_recipients_now);

        inventory_ops::apply_destroys(&mut world, &mut q, now);

        inventory_ops::apply_consumables(&mut world, &mut q, now, &in_world_recipients_now);

        inventory_ops::apply_gm_gives(&mut world, &mut q, now);

        vendors::apply_buys(&mut world, &mut q, now);

        vendors::apply_sells(&mut world, &mut q, now);

        banking::apply_coin_bank(&mut world, &mut q, now);

        banking::apply_item_bank(&mut world, &mut q, now);

        enemy_ai::run_enemy_ai(&mut world, &mut q, now, dt, &in_world_recipients_now);

        enemy_ai::clean_up_enemy_corpses(&mut world, &mut q, now, &in_world_recipients_now).await;

        binds_and_res::persist_binds(&mut world, &mut q, now).await;

        binds_and_res::apply_res_accepts(&mut world, &mut q, now).await;

        quest_steps::apply_quest_turn_ins(&mut world, &mut q, now).await;

        quest_steps::apply_quest_lifecycle(&mut world, &mut q, now);

        looting::apply_loot_intents(&mut world, &mut q, now, &in_world_recipients_now).await;

        looting::expire_bags_and_corpses(&mut world, &mut q, now, &in_world_recipients_now).await;

        movement::run_movement(&mut world, &mut q, now, dt, &in_world_recipients_now);

        buff_tick::run_buff_tick(&mut world, &mut q, now, dt, &in_world_recipients_now);

        camp::camp_sweep(&mut world, &mut q, now);

        death_sweep::run_death_sweep(&mut world, &mut q, now, &in_world_recipients_now).await;

        regen_tick::run_regen(&mut world, &mut q, now, dt);

        position_fan::fan_out_positions(&mut world, &mut q, now, &in_world_recipients_now);

        upkeep::run_upkeep(&mut world, &mut q, now, &in_world_recipients_now).await;

        // 8. Push outbound packets to the network.
        transport.send_packets(&mut world.server);
    }
}
