//! A vault: somewhere else logins are kept, which a plugin brings
//! (`oak_sdk::Vault`). The vault alone holds the Claude logins it keeps
//! and renews them (refresh tokens are replaced at every renewal, so two
//! holders of one login would log each other out); Oak asks it for an
//! access token whenever the one it has is close to expiring. Without a
//! plugin there is none, and every login is this machine's.

use super::*;
use std::sync::Arc;

/// The vault, when a plugin brought one and it is connected.
pub(crate) fn vault() -> Option<&'static Arc<dyn oak_sdk::Vault>> {
    crate::plugins::vault().filter(|v| v.connected())
}

/// Connected to a vault.
pub fn vault_on() -> bool {
    vault().is_some()
}

/// What the vault is called in sentences (its plugin names it: "the team server").
pub fn vault_name() -> String {
    crate::plugins::vault().map(|v| v.name()).filter(|n| !n.is_empty()).unwrap_or_else(|| "the vault".into())
}

/// Sign-ins go to the vault.
pub(crate) fn signing_in_remotely() -> bool {
    vault().is_some_and(|v| v.takes_new())
}

/// `e` is the vault's own trouble (not an answer about one account).
pub(crate) fn vault_failed(e: &str) -> bool {
    crate::plugins::vault().is_some_and(|v| v.failed(e))
}

/// `e` says the vault couldn't be reached at all.
pub(crate) fn vault_unreachable(e: &str) -> bool {
    crate::plugins::vault().is_some_and(|v| v.unreachable(e))
}

/// `e` (from `vault_add`) says the vault can't keep that kind of login.
pub(crate) fn vault_cannot_keep(e: &str) -> bool {
    crate::plugins::vault().is_some_and(|v| v.cannot_keep(e))
}

const NO_VAULT: &str = "No vault is connected.";

/// The vault's entries of `kind`.
pub(crate) fn vault_list(kind: &str) -> Result<Vec<Value>, String> {
    vault().ok_or(NO_VAULT)?.list(kind)
}

/// Gives the vault a login of `kind`; what it says about it.
pub(crate) fn vault_add(kind: &str, login: &Value) -> Result<Value, String> {
    vault().ok_or(NO_VAULT)?.add(kind, login)
}

/// Takes entry `id` of `kind` out of the vault, for every machine.
pub(crate) fn vault_remove(kind: &str, id: &str) -> Result<(), String> {
    vault().ok_or(NO_VAULT)?.remove(kind, id)
}

/// A token for the vault's entry `id` of `kind` (None: it has no such
/// entry any more).
pub(crate) fn vault_token(kind: &str, id: &str, refused: Option<&str>) -> Result<Option<Value>, String> {
    vault().ok_or(NO_VAULT)?.token(kind, id, refused).map(|v| (!v.is_null()).then_some(v))
}

/// The vault connected or disconnected, or its logins changed
/// (`oak_sdk::Host::vault_changed`): its accounts join this machine's
/// list, or leave it.
pub fn vault_changed() {
    if vault_on() {
        let _ = sync_vault();
        super::sync_other_logins();
        note_sign_in();
    } else {
        forget_vault();
    }
    ACCOUNTS_CHANGED.store(true, Ordering::Relaxed);
}

/// Set by `vault_changed`, for the window to fetch the models and the
/// accounts' usage again.
static ACCOUNTS_CHANGED: AtomicBool = AtomicBool::new(false);

/// Whether the vault's accounts changed since this was last asked.
pub fn take_accounts_changed() -> bool {
    ACCOUNTS_CHANGED.swap(false, Ordering::Relaxed)
}

/// The vault is gone: its accounts go from this machine (they stay in
/// the vault).
fn forget_vault() {
    let mut a = ACCOUNTS.lock().unwrap();
    let before = a.list.len();
    a.list.retain(|s| !s.remote());
    if a.get(a.current).is_none() {
        a.current = a.list.first().map_or(0, |s| s.id);
    }
    if a.list.len() != before {
        a.save();
    }
    drop(a);
    // The Codex logins and the keys from the vault go too.
    crate::codex::forget_remote();
    crate::openrouter::KEYS.forget_remote();
    crate::nodeclub::KEYS.forget_remote();
    crate::freebuff::forget_remote();
}

/// Brings the list in step with the vault's: its new accounts join, ones
/// removed there (by another machine) go.
pub fn sync_vault() -> Result<(), String> {
    let list = vault_list("claude")?;
    let field = |x: &Value, k: &str| x[k].as_str().unwrap_or("").to_string();
    let ids: Vec<String> = list.iter().map(|x| field(x, "id")).filter(|id| !id.is_empty()).collect();
    let mut a = ACCOUNTS.lock().unwrap();
    let before = a.list.len();
    a.list.retain(|s| s.remote_id().is_none_or(|r| ids.iter().any(|id| id == r)));
    let mut changed = a.list.len() != before;
    let mut added = false;
    for x in &list {
        let id = field(x, "id");
        if id.is_empty() || a.list.iter().any(|s| s.remote_id() == Some(id.as_str())) {
            continue;
        }
        let n = a.push(Auth::Remote { id, access: String::new(), expires_at: 0 });
        let s = a.get_mut(n).unwrap();
        (s.email, s.name, s.plan) = (field(x, "email"), field(x, "name"), field(x, "plan"));
        added = true;
    }
    changed |= added;
    if a.get(a.current).is_none() {
        let now = now_ms();
        a.current = pick(&a.list, u64::MAX, now).or(a.list.first().map(|s| s.id)).unwrap_or(0);
        changed = true;
    }
    if changed {
        a.save();
    }
    drop(a);
    if added {
        note_sign_in();
    }
    Ok(())
}

/// An access token for the vault's account `rid`, and when it expires:
/// the one it has, or a new one when `refused` (turned down by Anthropic)
/// is still its current one.
pub(crate) fn remote_token(rid: &str, refused: Option<&str>) -> Result<(String, i64), String> {
    let v = vault_token("claude", rid, refused)?.ok_or_else(|| format!("That account is no longer in {}.", vault_name()))?;
    let access = v["access_token"].as_str().unwrap_or("").to_string();
    let expires_at = v["expires_at"].as_i64().unwrap_or(0);
    if access.is_empty() {
        return Err(format!("{} sent no token.", capital(&vault_name())));
    }
    Ok((access, expires_at))
}

/// `s` with its first letter a capital.
pub(crate) fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// The vault's answer about an account it took: its id there, and who it is.
fn upload(auth: &Auth) -> Result<(String, String, String, String), String> {
    let Auth::Oauth { access, refresh, expires_at } = auth else {
        return Err(format!("Only a Claude sign-in can go to {} (not an API key).", vault_name()));
    };
    let v = vault_add("claude", &json!({"refresh_token": refresh, "access_token": access, "expires_at": expires_at}))?;
    let field = |k: &str| v[k].as_str().unwrap_or("").to_string();
    if field("id").is_empty() {
        return Err(format!("{} didn't say which account it took.", capital(&vault_name())));
    }
    Ok((field("id"), field("email"), field("name"), field("plan")))
}

/// A fresh sign-in goes to the vault; this machine keeps its access token
/// only.
pub(crate) fn add_remote(auth: Auth) -> Result<(), String> {
    let (rid, email, name, plan) = upload(&auth)?;
    let Auth::Oauth { access, expires_at, .. } = auth else { unreachable!() };
    let mut a = ACCOUNTS.lock().unwrap();
    let remote = Auth::Remote { id: rid.clone(), access, expires_at };
    let id = match a.list.iter().position(|s| s.remote_id() == Some(rid.as_str())) {
        Some(i) => a.list[i].id,
        None => a.push(remote.clone()),
    };
    let s = a.get_mut(id).unwrap();
    s.auth = remote;
    (s.email, s.name, s.plan, s.limited_until, s.unreachable_until) = (email, name, plan, 0, 0);
    if a.get(a.current).is_none_or(|c| !c.remote()) {
        a.current = id;
    }
    a.save();
    drop(a);
    note_sign_in();
    Ok(())
}

/// Moves login `id` on this machine to the vault, which from then on is
/// the only one to renew it; this machine keeps its access token.
pub fn move_to_vault(id: u64) -> Result<(), String> {
    // No renewal of this login may start while it moves.
    let _renewing = REFRESHING.lock().unwrap();
    let auth = ACCOUNTS.lock().unwrap().get(id).map(|s| s.auth.clone()).ok_or("That account is gone.")?;
    let (rid, email, name, plan) = upload(&auth)?;
    let Auth::Oauth { access, expires_at, .. } = auth else { unreachable!() };
    let mut a = ACCOUNTS.lock().unwrap();
    let remote = Auth::Remote { id: rid.clone(), access, expires_at };
    // Already listed from the vault (a sync came first): that entry takes
    // the token, and the login here goes.
    if let Some(i) = a.list.iter().position(|s| s.remote_id() == Some(rid.as_str())) {
        let keep = a.list[i].id;
        a.list[i].auth = remote;
        a.list.retain(|s| s.id != id);
        if a.current == id {
            a.current = keep;
        }
    } else if let Some(s) = a.get_mut(id) {
        s.auth = remote;
        (s.email, s.name, s.plan, s.unreachable_until) = (email, name, plan, 0);
    }
    a.save();
    Ok(())
}
