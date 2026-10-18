//! The Memos access token, stored in the desktop keyring.
//!
//! A Memos token is password-equivalent — anything the web UI can do, it can
//! do — so it must not land in `settings.json` next to the window geometry.
//! It goes to the freedesktop Secret Service (GNOME Keyring) instead, which
//! also makes it revocable from one place if it ever leaks.
//!
//! Every function here is a blocking D-Bus round trip that can additionally
//! stall on an unlock prompt when the login keyring is locked, so all of
//! them must be called from a worker thread — `ui.rs` reads the token inside
//! the same `gio::spawn_blocking` closure that performs the request, which
//! also keeps the secret out of the long-lived `App` struct.
//!
//! Under Flatpak this needs `--talk-name=org.freedesktop.secrets`.

/// Keyring attributes identifying the entry. Both are part of its identity:
/// changing either orphans previously saved tokens.
const SERVICE: &str = "no.oyzmo.Bindle";
const ACCOUNT: &str = "memos-token";

fn entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, ACCOUNT).map_err(|err| err.to_string())
}

/// The saved token, or None when none has been stored (or the keyring is
/// unavailable — an absent secret and an unreachable daemon are the same
/// thing to the caller: there is no token to send).
pub fn load_token() -> Option<String> {
    entry().ok()?.get_password().ok().filter(|token| !token.is_empty())
}

pub fn store_token(token: &str) -> Result<(), String> {
    entry()?.set_password(token).map_err(|err| err.to_string())
}

/// Forget the token. Succeeds when there was nothing to forget.
pub fn clear_token() -> Result<(), String> {
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}
