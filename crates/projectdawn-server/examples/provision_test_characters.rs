//! Make a throwaway account with one character per class a playtest needs,
//! over the ordinary auth socket, the way the launcher would. Written for the
//! ported-spells checklist (2026-10-06): six classes at six levels, so the
//! tester logs in, picks a character, casts, and moves on.
//!
//!   cargo run -p projectdawn-server --example provision_test_characters -- \
//!       ws://100.93.108.112:8765 spelltest
//!
//! It registers the account with a random password (printed ONCE; it is not
//! stored anywhere), logs in, creates the characters, and prints the one
//! command the operator runs on the host to set their levels. New characters
//! are always level 1 and only the host can change that; the SQL is the same
//! thing the integration tests do (`set_char_level`): the server recomputes
//! the maxima from the level at login and clamps the stored values down to
//! them, so each character logs in at full health at its new level. Run the
//! SQL while none of them is logged in; the server never writes an offline
//! character.
//!
//! Nothing here is privileged: these are the public Register, Login and
//! CharCreate messages, and the account is an ordinary non-GM one.

use futures_util::{SinkExt, StreamExt};
use protocol::auth::{ClientAuthMsg, ErrorCode, ServerAuthMsg};
use rand::Rng;
use tokio_tungstenite::tungstenite::Message;

/// What the checklist needs: class, target level, and a few name candidates
/// (character names are unique world-wide, so a taken one falls through to
/// the next).
const WANTED: &[(&str, i32, &[&str])] = &[
    ("Sorcerer", 10, &["Embris", "Embrisa", "Embriel"]),
    ("Wizard", 10, &["Rimewind", "Rimewynd", "Rimevane"]),
    ("Enchanter", 20, &["Mirelle", "Mirella", "Mirelda"]),
    ("Bard", 10, &["Caderyn", "Caderin", "Cadryn"]),
    ("Beast Master", 4, &["Fennric", "Fennrick", "Fennrik"]),
    ("Paladin", 12, &["Aldous", "Aldouse", "Aldus"]),
];
/// Locked out of no class (see the client's `LOCKED_COMBOS`).
const RACE: &str = "Human";
const CLIENT_VERSION: &str = "0.1.0";
const PASSWORD_LEN: usize = 16;

type Ws = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (url, username) = match args.as_slice() {
        [url, username] => (url.clone(), username.clone()),
        _ => {
            eprintln!("usage: provision_test_characters <ws://host:8765> <account name>");
            std::process::exit(2);
        }
    };

    let password = random_password();
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await?;

    match rpc(&mut ws, ClientAuthMsg::Register {
        username: username.clone(),
        password: password.clone(),
        email: None,
    })
    .await?
    {
        ServerAuthMsg::RegisterOk { account_id } => {
            println!("Registered account '{username}' (id {account_id}).");
        }
        ServerAuthMsg::Error { code: ErrorCode::NameTaken, .. } => {
            anyhow::bail!(
                "an account named '{username}' already exists; its password is not known \
                 here, so pick another account name"
            );
        }
        other => anyhow::bail!("register: unexpected reply {other:?}"),
    }

    let session = match rpc(&mut ws, ClientAuthMsg::Login {
        username: username.clone(),
        password: password.clone(),
        client_version: CLIENT_VERSION.into(),
    })
    .await?
    {
        ServerAuthMsg::LoginOk { session_token, .. } => session_token,
        other => anyhow::bail!("login: unexpected reply {other:?}"),
    };

    let mut made: Vec<(String, &str, i32, i64)> = Vec::new();
    for (class, level, candidates) in WANTED {
        let mut landed = None;
        for name in *candidates {
            match rpc(&mut ws, ClientAuthMsg::CharCreate {
                session_token: session.clone(),
                name: (*name).into(),
                race: RACE.into(),
                class: (*class).into(),
            })
            .await?
            {
                ServerAuthMsg::CharCreated { char_id } => {
                    landed = Some(((*name).to_string(), char_id));
                    break;
                }
                ServerAuthMsg::Error { code: ErrorCode::NameTaken, .. } => {
                    println!("  '{name}' is taken, trying the next name");
                }
                other => anyhow::bail!("create {name} ({class}): unexpected reply {other:?}"),
            }
        }
        let Some((name, char_id)) = landed else {
            anyhow::bail!("every candidate name for the {class} was taken; add more to WANTED");
        };
        println!("  created {name} ({RACE} {class}, id {char_id}), wanted at level {level}");
        made.push((name, class, *level, char_id));
    }
    let _ = rpc(&mut ws, ClientAuthMsg::Logout { session_token: session }).await;
    // A proper close handshake, or the server logs the dropped socket as a
    // protocol error (it did, on the first run).
    let _ = ws.close(None).await;

    println!();
    println!("Log in with  account: {username}   password: {password}");
    println!("(The password is shown this once and stored nowhere. reset_password can replace it.)");
    println!();
    println!("Characters (all level 1 until the host command below runs):");
    for (name, class, level, _) in &made {
        println!("  {name:<10} {class:<13} wanted level {level}");
    }
    println!();
    println!("On the R720, with none of them logged in, run:");
    let updates: Vec<String> = made
        .iter()
        .map(|(name, _, level, _)| {
            format!(
                "UPDATE characters SET level = {level}, hp = 99999, mp = 99999, stamina = 99999 \
                 WHERE name = '{name}';"
            )
        })
        .collect();
    println!(
        "sudo -u projectdawn sqlite3 /opt/projectdawn/world.db \"{}\"",
        updates.join(" ")
    );
    println!();
    println!("Then check with:");
    println!(
        "sudo -u projectdawn sqlite3 /opt/projectdawn/world.db \"SELECT name, class, level FROM characters WHERE account_id = (SELECT id FROM accounts WHERE username = '{username}');\""
    );
    Ok(())
}

async fn rpc(ws: &mut Ws, msg: ClientAuthMsg) -> anyhow::Result<ServerAuthMsg> {
    ws.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
    loop {
        let frame = ws
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("auth socket closed"))??;
        match frame {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => anyhow::bail!("unexpected frame: {other:?}"),
        }
    }
}

fn random_password() -> String {
    const CHARS: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..PASSWORD_LEN)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}
