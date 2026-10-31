//! Writing a connection's state to SQLite: the 60 s checkpoint, and the
//! store flush every immediate save goes through. Most mutations only mark a
//! store dirty and wait for the checkpoint; the ones that move an item or coin
//! to another character or onto a durable record (a drop, a corpse, a corpse
//! loot, a quest reward) flush first, so a crash cannot leave it in two places.

use super::connection::PerConnection;
use crate::db;
use crate::error::AuthResult;
use sqlx::SqlitePool;

/// Write every dirty item and coin store of `conn` in ONE transaction and
/// clear their flags only once it commits. A no-op when nothing is dirty.
///
/// Items and coins move BETWEEN these stores (a bank deposit touches the
/// inventory and the vault, a vendor sale the inventory and the wallet), so
/// they are never written separately: a crash between two writes either loses
/// the item or duplicates it, and a dupe is the worse half, because losses get
/// reported and dupes get exploited quietly. For the same reason an immediate
/// save must never write one store alone while another is dirty (a quest
/// reward saving the inventory while a bank withdrawal sits unsaved left the
/// item in both). See db::save_stores_atomic.
pub async fn persist_dirty_stores(pool: &SqlitePool, conn: &mut PerConnection) -> AuthResult<()> {
    if !conn.any_store_dirty() && conn.item_events.is_empty() {
        return Ok(());
    }
    let inv_rows = conn.inventory_dirty.then(|| conn.inventory.to_rows());
    let bank_rows = conn.bank_items_dirty.then(|| conn.bank_items.to_rows());
    let acct_rows = conn
        .account_bank_items_dirty
        .then(|| conn.account_bank_items.to_rows());
    db::save_stores_atomic(
        pool,
        conn.char_id,
        conn.account_id,
        inv_rows.as_deref(),
        conn.coins_dirty.then_some(conn.coins),
        conn.bank_dirty.then_some(conn.bank_coins),
        bank_rows.as_deref(),
        acct_rows.as_deref(),
        &conn.item_events,
    )
    .await?;
    // All-or-nothing: the transaction committed, so every flag it covered is
    // clean. Clearing them individually would reopen the very window this closes.
    // The ledger rows it wrote leave the queue with them (world/ledger.rs).
    conn.inventory_dirty = false;
    conn.coins_dirty = false;
    conn.bank_dirty = false;
    conn.bank_items_dirty = false;
    conn.account_bank_items_dirty = false;
    conn.item_events.clear();
    Ok(())
}

/// Write everything about `conn` that has changed since it was last saved:
/// position, resources (HP, mana, stamina, XP, level, hunger, alignment), the
/// item and coin stores (in one transaction), passive skill scores, faction
/// standings and quest progress.
/// Each part clears its own dirty flag only when its write lands, so a failed
/// part is simply still dirty and a later call retries it.
///
/// Returns true when nothing is left unsaved. Every way a connection is saved
/// goes through here: the 60 s checkpoint, logout, the retry queue for a
/// logout whose save failed, and the graceful shutdown. `when` names the
/// caller in the log lines.
pub async fn flush_connection(pool: &SqlitePool, conn: &mut PerConnection, when: &str) -> bool {
    if conn.is_dirty_for_persist() {
        let zone = conn.zone.as_deref();
        match db::checkpoint_position(pool, conn.char_id, zone, conn.pos.into_tuple(), conn.yaw)
            .await
        {
            Ok(()) => conn.mark_persisted(),
            Err(e) => {
                tracing::warn!(char_id = conn.char_id, when, error = %e, "position save failed");
            }
        }
    }
    if conn.is_dirty_for_resource_persist() {
        match db::checkpoint_resources(
            pool,
            conn.char_id,
            conn.hp,
            conn.mp,
            conn.stamina,
            conn.xp,
            conn.xp_to_next,
            conn.level,
            conn.hunger_secs,
            conn.thirst_secs,
            conn.alignment,
        )
        .await
        {
            Ok(()) => conn.mark_resources_persisted(),
            Err(e) => {
                tracing::warn!(char_id = conn.char_id, when, error = %e, "resource save failed");
            }
        }
    }
    if let Err(e) = persist_dirty_stores(pool, conn).await {
        tracing::warn!(char_id = conn.char_id, when, error = %e, "store save failed");
    }
    // Passive skill scores: the full set is rewritten when any advance landed
    // since the last save (one delete plus at most ~21 inserts).
    if conn.skills_dirty {
        let mut rows: Vec<db::SkillRow> = Vec::new();
        for (kind, map) in [
            ("weapon", &conn.weapon_skills),
            ("armor", &conn.armor_skills),
            ("casting", &conn.casting_skills),
        ] {
            for (key, score) in map {
                rows.push(db::SkillRow { kind: kind.to_string(), key: key.clone(), score: *score });
            }
        }
        match db::save_skills(pool, conn.char_id, &rows).await {
            Ok(()) => conn.skills_dirty = false,
            Err(e) => {
                tracing::warn!(char_id = conn.char_id, when, error = %e, "skill save failed");
            }
        }
    }
    // Faction standings moved by a kill or a quest since the last save.
    if conn.factions_dirty {
        let rows = conn.faction_rows();
        match db::save_factions(pool, conn.char_id, &rows).await {
            Ok(()) => conn.factions_dirty = false,
            Err(e) => {
                tracing::warn!(char_id = conn.char_id, when, error = %e, "faction save failed");
            }
        }
    }
    flush_quests(pool, conn, when).await;
    !has_unsaved_state(conn)
}

/// Write the quest progress touched since the last save, reconciled to the
/// database: upsert a quest that is still active, delete the row of one that
/// is not (abandoned or completed). A failed write stays dirty for the next
/// call. The tick loop runs this every tick for every connection (counted
/// kills are worth more than the writes), and `flush_connection` runs it too.
pub async fn flush_quests(pool: &SqlitePool, conn: &mut PerConnection, when: &str) {
    let dirty_quests: Vec<String> = conn.quests_dirty.iter().cloned().collect();
    for quest_id in dirty_quests {
        let result = match conn.active_quests.get(&quest_id) {
            Some(progress) => db::save_quest_progress(pool, conn.char_id, &quest_id, progress).await,
            None => db::delete_active_quest(pool, conn.char_id, &quest_id).await,
        };
        match result {
            Ok(()) => {
                conn.quests_dirty.remove(&quest_id);
            }
            Err(e) => {
                tracing::warn!(char_id = conn.char_id, when, quest_id = %quest_id, error = %e, "quest save failed");
            }
        }
    }
}

/// Is anything about `conn` still waiting to be written?
pub fn has_unsaved_state(conn: &PerConnection) -> bool {
    conn.is_dirty_for_persist()
        || conn.is_dirty_for_resource_persist()
        || conn.any_store_dirty()
        || !conn.item_events.is_empty()
        || conn.skills_dirty
        || conn.factions_dirty
        || !conn.quests_dirty.is_empty()
}

/// The 60 s checkpoint: flush every connection. A part that fails stays dirty
/// and is retried at the next checkpoint.
pub async fn checkpoint_dirty(pool: &SqlitePool, conns: &mut [&mut PerConnection]) {
    for conn in conns.iter_mut() {
        flush_connection(pool, conn, "checkpoint").await;
    }
}
