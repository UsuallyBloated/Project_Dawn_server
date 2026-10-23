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
    let any_store_dirty = conn.inventory_dirty
        || conn.coins_dirty
        || conn.bank_dirty
        || conn.bank_items_dirty
        || conn.account_bank_items_dirty;
    if !any_store_dirty {
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
    )
    .await?;
    // All-or-nothing: the transaction committed, so every flag it covered is
    // clean. Clearing them individually would reopen the very window this closes.
    conn.inventory_dirty = false;
    conn.coins_dirty = false;
    conn.bank_dirty = false;
    conn.bank_items_dirty = false;
    conn.account_bank_items_dirty = false;
    Ok(())
}

/// Walk the connection map, writing dirty rows to SQLite. Called from the
/// tick loop on each `CHECKPOINT_INTERVAL` and on disconnect.
///
/// Each row is its own UPDATE — keeps lock contention with the auth
/// handlers' touch_session writes minimal under SQLite WAL. Track 6 added
/// the resource path (hp/mp/stamina/xp/level) alongside position, since
/// the server now mutates resources every regen tick. Track 13.1 added
/// the inventory path (delete + insert per-character) for the server-
/// side inventory snapshot.
pub async fn checkpoint_dirty(pool: &SqlitePool, conns: &mut [&mut PerConnection]) {
    for conn in conns.iter_mut() {
        if conn.is_dirty_for_persist() {
            let zone = conn.zone.as_deref();
            match db::checkpoint_position(
                pool,
                conn.char_id,
                zone,
                conn.pos.into_tuple(),
                conn.yaw,
            )
            .await
            {
                Ok(()) => conn.mark_persisted(),
                Err(e) => {
                    tracing::warn!(
                        char_id = conn.char_id,
                        error = %e,
                        "position checkpoint failed; will retry next interval"
                    );
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
                    tracing::warn!(
                        char_id = conn.char_id,
                        error = %e,
                        "resource checkpoint failed; will retry next interval"
                    );
                }
            }
        }
        if let Err(e) = persist_dirty_stores(pool, conn).await {
            tracing::warn!(
                char_id = conn.char_id,
                error = %e,
                "store checkpoint failed; will retry next interval"
            );
        }
        // Track 18.1 — passive skill scores. Rewrite the full set
        // when any advance landed since last persist; one delete +
        // ≤ 21 inserts per character is well within the SQLite WAL
        // budget at the 60 s checkpoint cadence.
        if conn.skills_dirty {
            let mut rows: Vec<db::SkillRow> = Vec::new();
            for (key, score) in &conn.weapon_skills {
                rows.push(db::SkillRow {
                    kind: "weapon".to_string(),
                    key: key.clone(),
                    score: *score,
                });
            }
            for (key, score) in &conn.armor_skills {
                rows.push(db::SkillRow {
                    kind: "armor".to_string(),
                    key: key.clone(),
                    score: *score,
                });
            }
            for (key, score) in &conn.casting_skills {
                rows.push(db::SkillRow {
                    kind: "casting".to_string(),
                    key: key.clone(),
                    score: *score,
                });
            }
            match db::save_skills(pool, conn.char_id, &rows).await {
                Ok(()) => conn.skills_dirty = false,
                Err(e) => {
                    tracing::warn!(
                        char_id = conn.char_id,
                        error = %e,
                        "skill checkpoint failed; will retry next interval"
                    );
                }
            }
        }
        // Faction standings moved by a kill or a quest since the last pass.
        if conn.factions_dirty {
            let rows = conn.faction_rows();
            match db::save_factions(pool, conn.char_id, &rows).await {
                Ok(()) => conn.factions_dirty = false,
                Err(e) => {
                    tracing::warn!(
                        char_id = conn.char_id,
                        error = %e,
                        "faction checkpoint failed; will retry next interval"
                    );
                }
            }
        }
    }
}
