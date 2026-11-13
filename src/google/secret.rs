//! Refresh-token storage in the Secret Service (`org.freedesktop.secrets`) via `oo7`.
//!
//! `oo7` is async; every call here is blocking and drives its own throw-away current-thread
//! runtime on a dedicated thread, so it is safe from sync code, from `spawn_blocking`, and even
//! from inside another tokio runtime (no nested `block_on` panic).

use std::future::Future;

use anyhow::{anyhow, Context, Result};
use oo7::Keyring;

use crate::app::KEYRING_SERVICE;

const LABEL: &str = "Google Calendar refresh token";

fn attributes() -> [(&'static str, &'static str); 2] {
    [("service", KEYRING_SERVICE), ("kind", "refresh_token")]
}

/// Run a future to completion on a fresh current-thread runtime, on a fresh OS thread.
/// The thread has no ambient runtime context, so `block_on` cannot panic as "nested".
/// The future is built inside the thread, so it need not be `Send`.
fn run<Fut, T>(make: impl FnOnce() -> Fut + Send) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
    T: Send,
{
    std::thread::scope(|s| {
        s.spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("starting secret-service runtime")?
                .block_on(make())
        })
        .join()
        .map_err(|_| anyhow!("secret-service thread panicked"))?
    })
}

fn unavailable(e: oo7::Error) -> anyhow::Error {
    anyhow!("Secret Service (keyring) unavailable or locked: {e}")
}

pub fn store_refresh_token(token: &str) -> Result<()> {
    run(|| async move {
        let keyring = Keyring::new().await.map_err(unavailable)?;
        keyring.unlock().await.map_err(unavailable)?;
        keyring
            .create_item(LABEL, &attributes(), token, true)
            .await
            .map_err(|e| anyhow!("storing refresh token in the keyring: {e}"))
    })
}

pub fn load_refresh_token() -> Result<Option<String>> {
    run(|| async {
        let keyring = Keyring::new().await.map_err(unavailable)?;
        keyring.unlock().await.map_err(unavailable)?;
        let items = keyring.search_items(&attributes()).await.map_err(unavailable)?;
        let Some(item) = items.first() else { return Ok(None) };
        let secret = item.secret().await.map_err(unavailable)?;
        let token = std::str::from_utf8(secret.as_bytes()).context("stored refresh token is not UTF-8")?;
        Ok(Some(token.trim().to_owned()).filter(|t| !t.is_empty()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_is_safe_inside_a_tokio_runtime() {
        let outer = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let v = outer.block_on(async { run(|| async { Ok(41 + 1) }) }).unwrap();
        assert_eq!(v, 42);
        // …and from a spawn_blocking thread.
        let v = outer
            .block_on(async { tokio::task::spawn_blocking(|| run(|| async { Ok("ok") })).await.unwrap() })
            .unwrap();
        assert_eq!(v, "ok");
    }

    #[test]
    fn run_propagates_errors() {
        let e = run::<_, ()>(|| async { Err(anyhow!("boom")) }).unwrap_err();
        assert_eq!(e.to_string(), "boom");
    }
}
