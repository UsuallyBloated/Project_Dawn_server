//! The item ledger (migration 0017, `world/ledger.rs`) at the database level:
//! rows ride the store transaction, the conservation query names a planted
//! dupe and a planted loss, and the daily fold keeps the books balanced while
//! it prunes. The world integration tests (`world_two_clients.rs`) cover the
//! sites that queue the rows; this file is about the table and its queries.
//! Every test here fails on a database without the table.

use projectdawn_server::db::{self, InventoryRow, ItemEventRow};
use tempfile::TempDir;

const BREAD: &str = "res://data/loot/items/bread_loaf.tres";

async fn fresh_pool() -> (sqlx::SqlitePool, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("ledger_test.db");
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let pool = db::open(&url).await.expect("open pool");
    db::migrate(&pool).await.expect("migrate");
    (pool, tmp)
}

async fn account_and_character(pool: &sqlx::SqlitePool, user: &str, name: &str) -> (i64, i64) {
    let account_id = db::create_account(pool, user, "hunter2!", None)
        .await
        .expect("create account");
    let char_id = db::create_character(pool, account_id, name, "Human", "Warrior")
        .await
        .expect("create character");
    (account_id, char_id)
}

fn row(char_id: i64, at: i64, action: &'static str, item: Option<&str>, count: i64, delta: i64, copper: i64) -> ItemEventRow {
    ItemEventRow {
        at,
        char_id,
        action,
        item_path: item.map(str::to_string),
        count,
        delta,
        copper,
        detail: None,
    }
}

fn bread(count: i32) -> Vec<InventoryRow> {
    vec![InventoryRow { location: "base".into(), slot: 0, item_path: BREAD.into(), count }]
}

async fn sums(pool: &sqlx::SqlitePool, char_id: i64) -> (i64, i64, i64) {
    let (rows, delta, copper): (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(delta), 0), COALESCE(SUM(copper), 0)
         FROM item_events WHERE char_id = ?1",
    )
    .bind(char_id)
    .fetch_one(pool)
    .await
    .expect("sums");
    (rows, delta, copper)
}

/// The rows are written by the same transaction as the stores, and once both
/// are in, the conservation query has nothing to say. Then a planted extra
/// stack (a dupe) and a wallet edited behind the ledger's back (a loss) are
/// each reported as exactly one mismatch naming the account and the item.
#[tokio::test]
async fn rows_ride_the_store_transaction_and_the_check_names_a_dupe_and_a_loss() {
    let (pool, _tmp) = fresh_pool().await;
    let (account_id, char_id) = account_and_character(&pool, "ledger", "Keeper").await;
    let coins = protocol::world::Coins::from_copper(100);

    db::save_stores_atomic(
        &pool,
        char_id,
        account_id,
        Some(&bread(3)),
        Some(coins),
        None,
        None,
        None,
        &[
            row(char_id, 1_000, "loot", Some(BREAD), 3, 3, 0),
            row(char_id, 1_001, "gm_coins", None, 0, 0, 100),
        ],
    )
    .await
    .expect("save with rows");
    assert_eq!(sums(&pool, char_id).await, (2, 3, 100), "both rows committed with the stores");
    assert!(db::ledger_mismatches(&pool).await.expect("check").is_empty(), "the books balance");

    // One loaf destroyed: the store shrinks and the row says why.
    db::save_stores_atomic(
        &pool,
        char_id,
        account_id,
        Some(&bread(2)),
        None,
        None,
        None,
        None,
        &[row(char_id, 1_002, "destroy", Some(BREAD), 1, -1, 0)],
    )
    .await
    .expect("save the destroy");
    assert!(db::ledger_mismatches(&pool).await.expect("check").is_empty());

    // A dupe: a stack the ledger never saw.
    sqlx::query("INSERT INTO character_items (char_id, location, slot, item_path, count) VALUES (?1, 'base', 5, ?2, 5)")
        .bind(char_id)
        .bind(BREAD)
        .execute(&pool)
        .await
        .expect("plant");
    let found = db::ledger_mismatches(&pool).await.expect("check");
    assert_eq!(
        found,
        vec![db::LedgerMismatch { account_id, item_path: Some(BREAD.into()), ledger: 2, held: 7 }],
        "the planted stack is the one mismatch"
    );
    sqlx::query("DELETE FROM character_items WHERE char_id = ?1 AND slot = 5")
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("unplant");

    // A loss: coin gone from the wallet with no row (every tier: 100 copper
    // was saved as one silver).
    sqlx::query("UPDATE characters SET platinum = 0, gold = 0, silver = 0, copper = 0 WHERE id = ?1")
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("drain");
    let found = db::ledger_mismatches(&pool).await.expect("check");
    assert_eq!(
        found,
        vec![db::LedgerMismatch { account_id, item_path: None, ledger: 100, held: 0 }],
        "the drained wallet is the one mismatch"
    );
}

/// The fold: rows older than the cutoff become one carry row per character
/// and item holding their sums, the old rows go, the newer stay, and the
/// books balance exactly as before.
#[tokio::test]
async fn the_fold_keeps_the_sums_and_drops_the_old_rows() {
    let (pool, _tmp) = fresh_pool().await;
    let (account_id, char_id) = account_and_character(&pool, "folder", "Folder").await;
    let cutoff = 1_000_000;
    db::save_stores_atomic(
        &pool,
        char_id,
        account_id,
        Some(&bread(1)),
        Some(protocol::world::Coins::from_copper(50)),
        None,
        None,
        None,
        &[
            row(char_id, cutoff - 500, "loot", Some(BREAD), 3, 3, 0),
            row(char_id, cutoff - 400, "destroy", Some(BREAD), 1, -1, 0),
            row(char_id, cutoff - 300, "gm_coins", None, 0, 0, 50),
            row(char_id, cutoff + 10, "drop", Some(BREAD), 1, -1, 0),
        ],
    )
    .await
    .expect("seed");
    assert!(db::ledger_mismatches(&pool).await.expect("check").is_empty(), "balanced before the fold");

    let (carried, deleted) = db::fold_item_events_before(&pool, cutoff).await.expect("fold");
    assert_eq!((carried, deleted), (2, 3), "one carry row for the bread, one for the coin; three old rows gone");

    assert_eq!(sums(&pool, char_id).await, (3, 1, 50), "the sums survive the fold");
    assert!(db::ledger_mismatches(&pool).await.expect("check").is_empty(), "balanced after the fold");
    let carries: Vec<(String, Option<String>, i64, i64, i64)> = sqlx::query_as(
        "SELECT action, item_path, at, delta, copper FROM item_events WHERE action = 'carry' ORDER BY item_path",
    )
    .fetch_all(&pool)
    .await
    .expect("carry rows");
    assert_eq!(
        carries,
        vec![
            ("carry".into(), None, cutoff, 0, 50),
            ("carry".into(), Some(BREAD.into()), cutoff, 2, 0),
        ],
        "each carry row is stamped at the cutoff and holds the folded sums"
    );

    // A second fold at the same cutoff finds nothing older and changes nothing.
    assert_eq!(db::fold_item_events_before(&pool, cutoff).await.expect("fold again"), (0, 0));
    assert_eq!(sums(&pool, char_id).await, (3, 1, 50));
}

/// The operator's read: newest first, with the character's name, and a row
/// whose character is gone still shows (the join is LEFT).
#[tokio::test]
async fn recent_rows_come_newest_first_with_the_characters_name() {
    let (pool, _tmp) = fresh_pool().await;
    let (account_id, char_id) = account_and_character(&pool, "reader", "Reader").await;
    db::save_stores_atomic(
        &pool,
        char_id,
        account_id,
        None,
        None,
        None,
        None,
        None,
        &[
            row(char_id, 10, "loot", Some(BREAD), 1, 1, 0),
            row(char_id, 20, "destroy", Some(BREAD), 1, -1, 0),
            row(99_999, 30, "destroy", Some(BREAD), 1, -1, 0),
        ],
    )
    .await
    .expect("rows");
    let recent = db::recent_item_events(&pool, 2).await.expect("recent");
    assert_eq!(recent.len(), 2);
    assert_eq!((recent[0].at, recent[0].char_name.as_deref()), (30, None), "a row outlives its character");
    assert_eq!((recent[1].at, recent[1].char_name.as_deref(), recent[1].action.as_str()), (20, Some("Reader"), "destroy"));
}
