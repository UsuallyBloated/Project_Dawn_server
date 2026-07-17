//! Read-only offline viewer for Project Dawn's `world.db`.
//!
//! Prints a summary of every account and character (including SOFT-DELETED
//! characters, i.e. `characters.deleted_at IS NOT NULL` — deleted from the
//! player's view but still on disk) to the console, and writes a self-contained
//! HTML report you can open in any browser.
//!
//! The database is opened **read-only** (`?mode=ro`) and nothing is written to
//! it, so this is safe to run even while the live server is up.
//!
//! Run from the server repo root (where `world.db` lives):
//!   cargo run -p projectdawn-server --bin admin_report
//! Optional positional args: [db_path] [output_html]
//!   defaults: world.db, world_report.html
//!
//! This is an ops/inspection tool, not part of the game server process. It adds
//! no network surface and no live-server dependency.

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::FromRow;
use std::collections::HashMap;
use std::fmt::Write as _;

#[derive(FromRow)]
struct Account {
    id: i64,
    username: String,
    email: Option<String>,
    is_gm: bool,
    is_banned: bool,
    ban_reason: Option<String>,
    created_at: Option<String>,
    last_login: Option<String>,
}

#[derive(FromRow)]
struct Character {
    id: i64,
    account_id: i64,
    name: String,
    race: String,
    class: String,
    level: i64,
    coins: i64,
    alignment_score: i64,
    zone: Option<String>,
    created_at: Option<String>,
    last_played_at: Option<String>,
    /// Non-null once the character was deleted. The row lingers (soft delete)
    /// so the name stays reserved and the character can be inspected/restored.
    deleted_at: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let db_path = args.next().unwrap_or_else(|| "world.db".to_string());
    let out_path = args.next().unwrap_or_else(|| "world_report.html".to_string());

    // Read-only: never touches the DB. `immutable=1` also lets us read a DB a
    // running server holds open, without contending on its lock.
    let url = format!("sqlite://{db_path}?mode=ro&immutable=1");
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .with_context(|| format!("opening '{db_path}' read-only (does the file exist?)"))?;

    let accounts: Vec<Account> = sqlx::query_as(
        "SELECT id, username, email, is_gm, is_banned, ban_reason, created_at, last_login
         FROM accounts ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .context("querying accounts")?;

    // No `deleted_at IS NULL` filter — we WANT the soft-deleted rows here.
    let characters: Vec<Character> = sqlx::query_as(
        "SELECT id, account_id, name, race, class, level, coins, alignment_score,
                zone, created_at, last_played_at, deleted_at
         FROM characters ORDER BY account_id, id",
    )
    .fetch_all(&pool)
    .await
    .context("querying characters")?;

    let generated = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z").to_string();

    print_console_summary(&db_path, &accounts, &characters);

    let html = render_html(&db_path, &generated, &accounts, &characters);
    std::fs::write(&out_path, html).with_context(|| format!("writing '{out_path}'"))?;
    println!("\nHTML report written to: {out_path}");
    println!("Open it in a browser (double-click, or 'Open with' a browser).");

    Ok(())
}

// ─── Console output ──────────────────────────────────────────────────────────

fn print_console_summary(db_path: &str, accounts: &[Account], characters: &[Character]) {
    let deleted = characters.iter().filter(|c| c.deleted_at.is_some()).count();
    let active = characters.len() - deleted;
    let gm = accounts.iter().filter(|a| a.is_gm).count();
    let banned = accounts.iter().filter(|a| a.is_banned).count();

    let mut by_account: HashMap<i64, Vec<&Character>> = HashMap::new();
    for c in characters {
        by_account.entry(c.account_id).or_default().push(c);
    }

    println!("Project Dawn — {db_path} (read-only)");
    println!(
        "Accounts: {}   Characters: {} ({} active, {} deleted)   GM: {}   Banned: {}",
        accounts.len(),
        characters.len(),
        active,
        deleted,
        gm,
        banned
    );
    println!("{}", "-".repeat(72));

    for a in accounts {
        let mut badges = String::new();
        if a.is_gm {
            badges.push_str(" [GM]");
        }
        if a.is_banned {
            badges.push_str(" [BANNED]");
        }
        println!(
            "#{:<4} {}{}   created {}   last login {}",
            a.id,
            a.username,
            badges,
            a.created_at.as_deref().unwrap_or("?"),
            a.last_login.as_deref().unwrap_or("never"),
        );
        if let Some(reason) = a.ban_reason.as_deref().filter(|r| !r.is_empty()) {
            println!("        ban reason: {reason}");
        }
        match by_account.get(&a.id) {
            None => println!("        (no characters)"),
            Some(chars) => {
                for c in chars {
                    let status = match &c.deleted_at {
                        Some(when) => format!("DELETED {when}"),
                        None => "active".to_string(),
                    };
                    println!(
                        "        - {:<16} Lv{:<3} {} {}   zone:{}   {}c   [{}]",
                        c.name,
                        c.level,
                        c.race,
                        c.class,
                        c.zone.as_deref().unwrap_or("-"),
                        c.coins,
                        status,
                    );
                }
            }
        }
    }

    // Characters whose account is missing (shouldn't happen with the FK, but be honest).
    let account_ids: std::collections::HashSet<i64> = accounts.iter().map(|a| a.id).collect();
    let orphans: Vec<&Character> = characters
        .iter()
        .filter(|c| !account_ids.contains(&c.account_id))
        .collect();
    if !orphans.is_empty() {
        println!("{}", "-".repeat(72));
        println!("ORPHAN characters (no matching account): {}", orphans.len());
        for c in orphans {
            println!("        - {} (account_id {})", c.name, c.account_id);
        }
    }
}

// ─── HTML output ─────────────────────────────────────────────────────────────

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn cell(value: Option<&str>, fallback: &str) -> String {
    match value {
        Some(v) if !v.is_empty() => esc(v),
        _ => format!("<span class=\"muted\">{fallback}</span>"),
    }
}

fn render_html(
    db_path: &str,
    generated: &str,
    accounts: &[Account],
    characters: &[Character],
) -> String {
    let deleted = characters.iter().filter(|c| c.deleted_at.is_some()).count();
    let active = characters.len() - deleted;
    let gm = accounts.iter().filter(|a| a.is_gm).count();
    let banned = accounts.iter().filter(|a| a.is_banned).count();

    let mut by_account: HashMap<i64, Vec<&Character>> = HashMap::new();
    for c in characters {
        by_account.entry(c.account_id).or_default().push(c);
    }

    let mut h = String::new();
    h.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    h.push_str("<title>Project Dawn — Server Data</title>\n<style>\n");
    h.push_str(CSS);
    h.push_str("\n</style>\n</head>\n<body>\n");

    // Header + stat tiles.
    h.push_str("<header class=\"top\">\n");
    h.push_str("<h1>Project Dawn <span class=\"sub\">server data</span></h1>\n");
    let _ = write!(
        h,
        "<p class=\"meta\">Read-only snapshot of <code>{}</code> &middot; generated {}</p>\n",
        esc(db_path),
        esc(generated)
    );
    h.push_str("<div class=\"tiles\">\n");
    tile(&mut h, "Accounts", &accounts.len().to_string(), "");
    tile(&mut h, "Characters", &characters.len().to_string(), "");
    tile(&mut h, "Active", &active.to_string(), "ok");
    tile(&mut h, "Deleted", &deleted.to_string(), "del");
    tile(&mut h, "GM", &gm.to_string(), "gm");
    tile(&mut h, "Banned", &banned.to_string(), "ban");
    h.push_str("</div>\n");

    // Controls.
    h.push_str("<div class=\"controls\">\n");
    h.push_str("<input id=\"q\" type=\"search\" placeholder=\"Filter by account or character name…\" oninput=\"applyFilter()\">\n");
    h.push_str("<label class=\"chk\"><input id=\"hideDel\" type=\"checkbox\" onchange=\"applyFilter()\"> Hide deleted characters</label>\n");
    h.push_str("</div>\n");
    h.push_str("</header>\n");

    h.push_str("<main>\n");
    if accounts.is_empty() {
        h.push_str("<p class=\"empty\">No accounts in this database.</p>\n");
    }

    for a in accounts {
        let filter_key = a.username.to_lowercase();
        let _ = write!(h, "<section class=\"card\" data-name=\"{}\">\n", esc(&filter_key));
        h.push_str("<div class=\"card-head\">\n");
        let _ = write!(
            h,
            "<span class=\"acc-id\">#{}</span><span class=\"acc-name\">{}</span>\n",
            a.id,
            esc(&a.username)
        );
        if a.is_gm {
            h.push_str("<span class=\"badge gm\">GM</span>\n");
        }
        if a.is_banned {
            h.push_str("<span class=\"badge ban\">BANNED</span>\n");
        }
        h.push_str("</div>\n");

        h.push_str("<div class=\"acc-meta\">\n");
        let _ = write!(h, "<span>email: {}</span>\n", cell(a.email.as_deref(), "none"));
        let _ = write!(h, "<span>created: {}</span>\n", cell(a.created_at.as_deref(), "?"));
        let _ = write!(h, "<span>last login: {}</span>\n", cell(a.last_login.as_deref(), "never"));
        h.push_str("</div>\n");
        if let Some(reason) = a.ban_reason.as_deref().filter(|r| !r.is_empty()) {
            let _ = write!(h, "<div class=\"ban-reason\">ban reason: {}</div>\n", esc(reason));
        }

        match by_account.get(&a.id) {
            None => h.push_str("<p class=\"no-chars\">No characters.</p>\n"),
            Some(chars) => render_char_table(&mut h, chars),
        }
        h.push_str("</section>\n");
    }

    // Orphans.
    let account_ids: std::collections::HashSet<i64> = accounts.iter().map(|a| a.id).collect();
    let orphans: Vec<&Character> = characters
        .iter()
        .filter(|c| !account_ids.contains(&c.account_id))
        .collect();
    if !orphans.is_empty() {
        h.push_str("<section class=\"card orphan\" data-name=\"orphan\">\n");
        h.push_str("<div class=\"card-head\"><span class=\"acc-name\">Orphan characters</span><span class=\"badge ban\">NO ACCOUNT</span></div>\n");
        h.push_str("<p class=\"acc-meta\"><span>Characters whose account row is missing.</span></p>\n");
        render_char_table(&mut h, &orphans);
        h.push_str("</section>\n");
    }

    h.push_str("</main>\n");
    h.push_str("<footer>Read-only ops view. Generated by <code>admin_report</code> — no changes were written to the database.</footer>\n");
    h.push_str("<script>\n");
    h.push_str(JS);
    h.push_str("\n</script>\n</body>\n</html>\n");
    h
}

fn tile(h: &mut String, label: &str, value: &str, kind: &str) {
    let _ = write!(
        h,
        "<div class=\"tile {kind}\"><div class=\"tile-val\">{value}</div><div class=\"tile-label\">{label}</div></div>\n"
    );
}

fn render_char_table(h: &mut String, chars: &[&Character]) {
    h.push_str("<div class=\"table-wrap\">\n<table class=\"chars\">\n<thead><tr>");
    for col in [
        "Name", "ID", "Lvl", "Race", "Class", "Zone", "Coins", "Align", "Created", "Last played",
        "Status",
    ] {
        let _ = write!(h, "<th>{col}</th>");
    }
    h.push_str("</tr></thead>\n<tbody>\n");
    for c in chars {
        let deleted = c.deleted_at.is_some();
        let row_class = if deleted { " class=\"deleted\"" } else { "" };
        let name_key = c.name.to_lowercase();
        let _ = write!(h, "<tr{row_class} data-name=\"{}\" data-deleted=\"{}\">", esc(&name_key), deleted);
        let _ = write!(h, "<td class=\"cname\">{}</td>", esc(&c.name));
        let _ = write!(h, "<td class=\"muted mono\">{}</td>", c.id);
        let _ = write!(h, "<td>{}</td>", c.level);
        let _ = write!(h, "<td>{}</td>", esc(&c.race));
        let _ = write!(h, "<td>{}</td>", esc(&c.class));
        let _ = write!(h, "<td>{}</td>", cell(c.zone.as_deref(), "-"));
        let _ = write!(h, "<td>{}</td>", c.coins);
        let _ = write!(h, "<td>{}</td>", c.alignment_score);
        let _ = write!(h, "<td class=\"muted\">{}</td>", cell(c.created_at.as_deref(), "?"));
        let _ = write!(h, "<td>{}</td>", cell(c.last_played_at.as_deref(), "never"));
        match &c.deleted_at {
            Some(when) => {
                let _ = write!(h, "<td><span class=\"badge del\">DELETED</span> <span class=\"muted\">{}</span></td>", esc(when));
            }
            None => h.push_str("<td><span class=\"badge ok\">active</span></td>"),
        }
        h.push_str("</tr>\n");
    }
    h.push_str("</tbody>\n</table>\n</div>\n");
}

const CSS: &str = r#"
:root {
  --bg: #f6f7f9; --panel: #ffffff; --ink: #1c2128; --ink-soft: #5a6270;
  --line: #e2e5ea; --accent: #4a6da7; --ok: #2f855a; --del: #b23b3b;
  --gm: #8a5a00; --ban: #9b2226; --shadow: 0 1px 3px rgba(0,0,0,.08);
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #14171c; --panel: #1c2128; --ink: #e6e9ef; --ink-soft: #9aa4b2;
    --line: #2b313b; --accent: #7aa2d6; --ok: #4caf7f; --del: #e06a6a;
    --gm: #d9a441; --ban: #e06a6a; --shadow: 0 1px 3px rgba(0,0,0,.4);
  }
}
:root[data-theme="dark"] {
  --bg: #14171c; --panel: #1c2128; --ink: #e6e9ef; --ink-soft: #9aa4b2;
  --line: #2b313b; --accent: #7aa2d6; --ok: #4caf7f; --del: #e06a6a;
  --gm: #d9a441; --ban: #e06a6a; --shadow: 0 1px 3px rgba(0,0,0,.4);
}
* { box-sizing: border-box; }
body {
  margin: 0; background: var(--bg); color: var(--ink);
  font: 14px/1.5 -apple-system, "Segoe UI", Roboto, system-ui, sans-serif;
}
code { font-family: ui-monospace, "Cascadia Code", Consolas, monospace; font-size: .92em; }
.top { padding: 22px clamp(14px, 4vw, 40px) 10px; border-bottom: 1px solid var(--line); }
h1 { margin: 0 0 2px; font-size: 22px; font-weight: 650; }
h1 .sub { color: var(--ink-soft); font-weight: 400; font-size: 15px; }
.meta { margin: 0 0 16px; color: var(--ink-soft); font-size: 13px; }
.tiles { display: flex; flex-wrap: wrap; gap: 10px; margin-bottom: 16px; }
.tile {
  background: var(--panel); border: 1px solid var(--line); border-radius: 10px;
  padding: 10px 16px; min-width: 92px; box-shadow: var(--shadow);
}
.tile-val { font-size: 24px; font-weight: 680; line-height: 1.1; }
.tile-label { font-size: 11px; text-transform: uppercase; letter-spacing: .04em; color: var(--ink-soft); }
.tile.ok .tile-val { color: var(--ok); }
.tile.del .tile-val { color: var(--del); }
.tile.gm .tile-val { color: var(--gm); }
.tile.ban .tile-val { color: var(--ban); }
.controls { display: flex; flex-wrap: wrap; gap: 14px; align-items: center; }
#q {
  flex: 1 1 260px; padding: 8px 12px; border: 1px solid var(--line);
  border-radius: 8px; background: var(--panel); color: var(--ink); font-size: 14px;
}
.chk { font-size: 13px; color: var(--ink-soft); display: flex; align-items: center; gap: 6px; cursor: pointer; }
main { padding: 18px clamp(14px, 4vw, 40px) 40px; display: grid; gap: 14px; }
.empty, .no-chars { color: var(--ink-soft); }
.card {
  background: var(--panel); border: 1px solid var(--line); border-radius: 12px;
  padding: 14px 16px; box-shadow: var(--shadow);
}
.card.orphan { border-color: var(--ban); }
.card-head { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.acc-id { color: var(--ink-soft); font: .85em ui-monospace, monospace; }
.acc-name { font-size: 17px; font-weight: 640; }
.acc-meta { display: flex; flex-wrap: wrap; gap: 6px 20px; color: var(--ink-soft); font-size: 12.5px; margin: 6px 0 4px; }
.ban-reason { color: var(--ban); font-size: 12.5px; margin: 2px 0 6px; }
.badge {
  font-size: 10.5px; font-weight: 700; letter-spacing: .03em; text-transform: uppercase;
  padding: 2px 7px; border-radius: 999px; border: 1px solid transparent; white-space: nowrap;
}
.badge.gm { color: var(--gm); border-color: var(--gm); }
.badge.ban { color: #fff; background: var(--ban); }
.badge.ok { color: var(--ok); border-color: var(--ok); }
.badge.del { color: #fff; background: var(--del); }
.muted { color: var(--ink-soft); }
.mono { font-family: ui-monospace, "Cascadia Code", Consolas, monospace; font-size: 12px; }
.table-wrap { overflow-x: auto; }
table.chars { width: 100%; border-collapse: collapse; margin-top: 8px; font-size: 13px; }
.chars th {
  text-align: left; font-size: 11px; text-transform: uppercase; letter-spacing: .03em;
  color: var(--ink-soft); font-weight: 600; padding: 4px 10px; border-bottom: 1px solid var(--line);
}
.chars td { padding: 5px 10px; border-bottom: 1px solid var(--line); }
.chars tr:last-child td { border-bottom: none; }
.chars .cname { font-weight: 600; }
.chars tr.deleted td { color: var(--ink-soft); }
.chars tr.deleted .cname { text-decoration: line-through; }
footer { padding: 18px clamp(14px, 4vw, 40px) 40px; color: var(--ink-soft); font-size: 12px; border-top: 1px solid var(--line); }
"#;

const JS: &str = r#"
function applyFilter() {
  const q = document.getElementById('q').value.trim().toLowerCase();
  const hideDel = document.getElementById('hideDel').checked;
  document.querySelectorAll('table.chars tr[data-name]').forEach(function (tr) {
    const isDel = tr.getAttribute('data-deleted') === 'true';
    tr.style.display = (hideDel && isDel) ? 'none' : '';
  });
  document.querySelectorAll('section.card').forEach(function (card) {
    const acc = card.getAttribute('data-name') || '';
    let match = q === '' || acc.indexOf(q) !== -1;
    if (!match) {
      card.querySelectorAll('tr[data-name]').forEach(function (tr) {
        if ((tr.getAttribute('data-name') || '').indexOf(q) !== -1) match = true;
      });
    }
    card.style.display = match ? '' : 'none';
  });
}
"#;
