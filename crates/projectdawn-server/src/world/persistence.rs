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
/// the server now mutates resources every regen tick.
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
    }
}
