//! Track 13.1 — inventory persistence roundtrip. Validates the
//! `character_items` schema added by `0002_inventory.sql` plus the
//! `db::save_inventory` / `db::load_inventory` helpers.
//!
//! Doesn't exercise the world tick or wire layer; the unit tests in
//! `world::inventory` cover the in-memory shape, and the world
//! integration tests cover the dispatch + fan-out. This file is
//! about "does it survive a process restart."

use projectdawn_server::{db, db::InventoryRow};
use tempfile::TempDir;

async fn fresh_pool() -> (sqlx::SqlitePool, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("inv_test.db");
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let pool = db::open(&url).await.expect("open pool");
    db::migrate(&pool).await.expect("migrate");
    (pool, tmp)
}

#[tokio::test]
async fn save_and_load_roundtrip() {
    let (pool, _tmp) = fresh_pool().await;
    let account_id = db::create_account(&pool, "tester", "hunter2!", None)
        .await
        .expect("create account");
    let char_id = db::create_character(&pool, account_id, "Persist", "Human", "Warrior")
        .await
        .expect("create character");

    let rows = vec![
        InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: "res://items/cloth.tres".into(),
            count: 5,
        },
        InventoryRow {
            location: "base".into(),
            slot: 3,
            item_path: "res://items/iron.tres".into(),
            count: 12,
        },
    ];
    db::save_inventory(&pool, char_id, &rows)
        .await
        .expect("save");

    let loaded = db::load_inventory(&pool, char_id)
        .await
        .expect("load");
    assert_eq!(loaded.len(), 2);
    let cloth = loaded
        .iter()
        .find(|r| r.item_path == "res://items/cloth.tres")
        .expect("cloth present");
    assert_eq!(cloth.slot, 0);
    assert_eq!(cloth.count, 5);
    let iron = loaded
        .iter()
        .find(|r| r.item_path == "res://items/iron.tres")
        .expect("iron present");
    assert_eq!(iron.slot, 3);
    assert_eq!(iron.count, 12);
}

#[tokio::test]
async fn save_overwrites_previous_snapshot() {
    let (pool, _tmp) = fresh_pool().await;
    let account_id = db::create_account(&pool, "overw", "hunter2!", None)
        .await
        .unwrap();
    let char_id = db::create_character(&pool, account_id, "Overwriter", "Human", "Warrior")
        .await
        .unwrap();

    // First snapshot.
    db::save_inventory(
        &pool,
        char_id,
        &[InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: "res://items/cloth.tres".into(),
            count: 5,
        }],
    )
    .await
    .unwrap();

    // Second snapshot — different item in the same slot. The atomic
    // delete + insert in `save_inventory` should leave only the
    // second item's row.
    db::save_inventory(
        &pool,
        char_id,
        &[InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: "res://items/iron.tres".into(),
            count: 1,
        }],
    )
    .await
    .unwrap();

    let loaded = db::load_inventory(&pool, char_id).await.unwrap();
    assert_eq!(loaded.len(), 1, "first snapshot must be wiped");
    assert_eq!(loaded[0].item_path, "res://items/iron.tres");
    assert_eq!(loaded[0].count, 1);
}

#[tokio::test]
async fn load_for_fresh_character_is_empty() {
    let (pool, _tmp) = fresh_pool().await;
    let account_id = db::create_account(&pool, "fresh", "hunter2!", None)
        .await
        .unwrap();
    let char_id = db::create_character(&pool, account_id, "Fresh", "Human", "Warrior")
        .await
        .unwrap();

    let loaded = db::load_inventory(&pool, char_id).await.unwrap();
    assert!(
        loaded.is_empty(),
        "freshly-created characters have no inventory rows"
    );
}

#[tokio::test]
async fn cascade_delete_on_character_removes_inventory() {
    let (pool, _tmp) = fresh_pool().await;
    let account_id = db::create_account(&pool, "deleter", "hunter2!", None)
        .await
        .unwrap();
    let char_id = db::create_character(&pool, account_id, "Doomed", "Human", "Warrior")
        .await
        .unwrap();
    db::save_inventory(
        &pool,
        char_id,
        &[InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: "res://items/cloth.tres".into(),
            count: 5,
        }],
    )
    .await
    .unwrap();

    // Hard-delete the character row to exercise the ON DELETE CASCADE
    // clause in the migration. The soft-delete path used in
    // production (`db::delete_character`) only flips `deleted_at`
    // and renames; inventory survives a soft-delete on purpose.
    sqlx::query("DELETE FROM characters WHERE id = ?1")
        .bind(char_id)
        .execute(&pool)
        .await
        .unwrap();

    let loaded = db::load_inventory(&pool, char_id).await.unwrap();
    assert!(loaded.is_empty(), "inventory rows must cascade with the character");
}

/// Coins persist across "restart": save_coins → load_character returns the
/// same four stacks. Guards the playtest bug where the in-session wallet
/// (vendor buys, dev grants) silently reset to the stale DB row on next
/// login because nothing ever wrote coins back.
#[tokio::test]
async fn coins_save_and_load_roundtrip() {
    let (pool, _tmp) = fresh_pool().await;
    let account_id = db::create_account(&pool, "coiner", "hunter2!", None)
        .await
        .expect("create account");
    let char_id = db::create_character(&pool, account_id, "Moneybags", "Human", "Warrior")
        .await
        .expect("create character");

    let wallet = protocol::world::Coins {
        platinum: 1,
        gold: 5,
        silver: 5,
        copper: 4000,
    };
    db::save_coins(&pool, char_id, wallet).await.expect("save coins");

    let spawn = db::load_character(&pool, char_id).await.expect("load character");
    assert_eq!(spawn.coins, wallet, "wallet must round-trip exactly, per-tier");
}
