//! Auth-channel messages (JSON over WebSocket).
//!
//! Wire format: `{ "type": "<Variant>", ... fields ... }`. Session tokens
//! are 32 random bytes, hex-encoded as 64-char lowercase strings.

use serde::{Deserialize, Serialize};

// ─── Client → Server ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientAuthMsg {
    Register {
        username: String,
        password: String,
        #[serde(default)]
        email: Option<String>,
    },
    Login {
        username: String,
        password: String,
        client_version: String,
    },
    CharList {
        session_token: String,
    },
    CharCreate {
        session_token: String,
        name: String,
        race: String,
        class: String,
    },
    CharDelete {
        session_token: String,
        char_id: i64,
    },
    Logout {
        session_token: String,
    },
    /// Launcher → auth, after the user picks Play. Auth validates the
    /// session and char ownership, then mints a renet `ConnectToken`
    /// signed with the shared netcode private key. The launcher hands
    /// the returned bytes to the game .exe (via temp file, not CLI arg —
    /// CLI args are visible in `ps`/Task Manager).
    RequestWorldToken {
        session_token: String,
        char_id: i64,
    },
}

// ─── Server → Client ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharacterSummary {
    pub id: i64,
    pub name: String,
    pub race: String,
    pub class: String,
    pub level: i32,
    pub zone: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerAuthMsg {
    RegisterOk {
        account_id: i64,
    },
    LoginOk {
        session_token: String,
        account_id: i64,
        is_gm: bool,
        world_endpoint: String,
        characters: Vec<CharacterSummary>,
    },
    CharList {
        characters: Vec<CharacterSummary>,
    },
    CharCreated {
        char_id: i64,
    },
    CharDeleted,
    LogoutOk,
    /// Reply to `RequestWorldToken`. `token_bytes` is the serialized renet
    /// `ConnectToken` (single-use, expires in ~30 s). `world_endpoint` is
    /// where the game .exe should aim its UDP socket. `expires_at_unix`
    /// lets the launcher abort early if it sat on the token too long.
    WorldConnectToken {
        token_bytes: Vec<u8>,
        world_endpoint: String,
        expires_at_unix: i64,
    },
    Error {
        code: ErrorCode,
        msg: String,
    },
}

/// Stable string codes the launcher branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Username already taken on Register.
    NameTaken,
    /// Username/password invalid on Login.
    AuthFailed,
    /// `client_version` < server's `min_client_version`.
    VersionMismatch,
    /// Session token expired or unknown.
    SessionExpired,
    /// Account is_banned.
    Banned,
    /// Validation failure (e.g. username too short, bad characters).
    InvalidInput,
    /// Resource not found (char_id doesn't belong to account, etc.).
    NotFound,
    /// Unhandled server error — bug to fix.
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_roundtrip() {
        let msg = ClientAuthMsg::Login {
            username: "tester".into(),
            password: "hunter2".into(),
            client_version: "0.1.0".into(),
        };
        let encoded = serde_json::to_string(&msg).unwrap();
        assert!(encoded.contains(r#""type":"Login""#));
        let decoded: ClientAuthMsg = serde_json::from_str(&encoded).unwrap();
        match decoded {
            ClientAuthMsg::Login { username, .. } => assert_eq!(username, "tester"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn error_code_snake_case() {
        let err = ServerAuthMsg::Error {
            code: ErrorCode::NameTaken,
            msg: "taken".into(),
        };
        let encoded = serde_json::to_string(&err).unwrap();
        assert!(encoded.contains(r#""code":"name_taken""#));
    }
}
