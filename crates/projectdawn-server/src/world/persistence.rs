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
        if conn.inventory_dirty {
            let rows = conn.inventory.to_rows();
            match db::save_inventory(pool, conn.char_id, &rows).await {
                Ok(()) => conn.inventory_dirty = false,
                Err(e) => {
                    tracing::warn!(
                        char_id = conn.char_id,
                        error = %e,
                        "inventory checkpoint failed; will retry next interval"
                    );
                }
            }
        }
        if conn.coins_dirty {
            match db::save_coins(pool, conn.char_id, conn.coins).await {
                Ok(()) => conn.coins_dirty = false,
                Err(e) => {
                    tracing::warn!(
                        char_id = conn.char_id,
                        error = %e,
                        "coin checkpoint failed; will retry next interval"
                    );
                }
            }
        }
        if conn.bank_dirty {
            match db::save_bank(pool, conn.char_id, conn.bank_coins).await {
                Ok(()) => conn.bank_dirty = false,
                Err(e) => {
                    tracing::warn!(
                        char_id = conn.char_id,
                        error = %e,
                        "bank checkpoint failed; will retry next interval"
                    );
                }
            }
        }
        // Banker slice 2 — the two item vaults. Personal is char-keyed;
        // the account-shared vault is keyed on account_id.
        if conn.bank_items_dirty {
            let rows = conn.bank_items.to_rows();
            match db::save_bank_items(pool, conn.char_id, &rows).await {
                Ok(()) => conn.bank_items_dirty = false,
                Err(e) => {
                    tracing::warn!(
                        char_id = conn.char_id,
                        error = %e,
                        "bank-items checkpoint failed; will retry next interval"
                    );
                }
            }
        }
        if conn.account_bank_items_dirty {
            let rows = conn.account_bank_items.to_rows();
            match db::save_account_bank_items(pool, conn.account_id, &rows).await {
                Ok(()) => conn.account_bank_items_dirty = false,
                Err(e) => {
                    tracing::warn!(
                        account_id = conn.account_id,
                        error = %e,
                        "account-bank-items checkpoint failed; will retry next interval"
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
