//! Periodic position checkpoints. Inventory / quest mutations write
//! per-mutation elsewhere; this is the "where was I when the power went
//! out" safety net.

use super::connection::PerConnection;
use crate::db;
use sqlx::SqlitePool;

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
        // Items and coins move BETWEEN these stores, so they are written in one
        // transaction rather than five. A bank deposit touches inventory and the
        // vault; a vendor sale touches inventory and the wallet. Writing them
        // separately left a crash window that either loses the item (gone from
        // inventory, never arrived) or duplicates it (arrived and still in
        // inventory) — and a dupe is the worse half, because losses get reported
        // and dupes get exploited quietly. See db::save_stores_atomic.
        let any_store_dirty = conn.inventory_dirty
            || conn.coins_dirty
            || conn.bank_dirty
            || conn.bank_items_dirty
            || conn.account_bank_items_dirty;
        if any_store_dirty {
            let inv_rows = conn.inventory_dirty.then(|| conn.inventory.to_rows());
            let bank_rows = conn.bank_items_dirty.then(|| conn.bank_items.to_rows());
            let acct_rows = conn
                .account_bank_items_dirty
                .then(|| conn.account_bank_items.to_rows());
            match db::save_stores_atomic(
                pool,
                conn.char_id,
                conn.account_id,
                inv_rows.as_deref(),
                conn.coins_dirty.then_some(conn.coins),
                conn.bank_dirty.then_some(conn.bank_coins),
                bank_rows.as_deref(),
                acct_rows.as_deref(),
            )
            .await
            {
                Ok(()) => {
                    // All-or-nothing: the transaction committed, so every flag it
                    // covered is clean. Clearing them individually would reopen
                    // the very window this closes.
                    conn.inventory_dirty = false;
                    conn.coins_dirty = false;
                    conn.bank_dirty = false;
                    conn.bank_items_dirty = false;
                    conn.account_bank_items_dirty = false;
                }
                Err(e) => {
                    tracing::warn!(
                        char_id = conn.char_id,
                        error = %e,
                        "store checkpoint failed; will retry next interval"
                    );
                }
            }
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
    }
}
