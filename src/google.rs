//! Google Calendar. CONTRACT STUB — replaced by the google implementation.
use crate::{config::Config, db::Db};

/// Interactive OAuth (loopback + PKCE), stores refresh token in Secret Service, then syncs calendar list.
pub fn login() -> anyhow::Result<()> { anyhow::bail!("stub") }

/// Incremental sync of calendar list + events of all calendars. Returns Ok(false) when not
/// configured (no client_id) or not logged in (no refresh token); Ok(true) after a sync.
pub fn sync_all(_db: &Db, _cfg: &Config) -> anyhow::Result<bool> { Ok(false) }
