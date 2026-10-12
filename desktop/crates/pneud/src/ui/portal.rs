//! Portal method replies accept requests; only Response completes them.
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use std::{collections::HashMap, time::Duration};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// Response waits belong to supervised consumers, never shared action gates.
pub type Interaction = futures_util::future::BoxFuture<'static, Result<()>>;

/// A small per-consumer budget: reserve capacity before issuing a request.
/// Dropping the supervisor aborts waits and drops their portal Requests (Close).
pub(crate) struct Interactions(tokio::task::JoinSet<()>);
impl Interactions {
    pub const LIMIT: usize = 8;
    pub fn new() -> Self {
        Self(tokio::task::JoinSet::new())
    }
    pub fn available(&mut self) -> bool {
        while self.0.try_join_next().is_some() {}
        self.0.len() < Self::LIMIT
    }
    pub fn push(&mut self, wait: impl std::future::Future<Output = ()> + Send + 'static) {
        assert!(
            self.available(),
            "interaction capacity must be reserved before issuance"
        );
        self.0.spawn(Box::pin(wait));
    }
    pub async fn shutdown(&mut self) {
        self.0.abort_all();
        while self.0.join_next().await.is_some() {}
    }
}

pub(crate) struct Request {
    proxy: zbus::Proxy<'static>,
    responses: zbus::proxy::SignalStream<'static>,
    owners: zbus::proxy::SignalStream<'static>,
    owner: zbus::names::OwnedUniqueName,
    pub token: String,
    complete: bool,
}
impl Request {
    pub async fn new(connection: &zbus::Connection) -> Result<Self> {
        tokio::time::timeout(Duration::from_secs(5), Self::subscribe(connection)).await?
    }
    async fn subscribe(connection: &zbus::Connection) -> Result<Self> {
        let token = format!("pneu{}", crate::random_hex(8));
        let sender = connection
            .unique_name()
            .context("portal bus name")?
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");
        let service = zbus::Proxy::new_owned(
            connection.clone(),
            "org.freedesktop.portal.Desktop",
            format!("/org/freedesktop/portal/desktop/request/{sender}/{token}"),
            "org.freedesktop.portal.Request",
        )
        .await?;
        let dbus = zbus::fdo::DBusProxy::new(connection).await?;
        let name = service.destination().clone();
        let owner = match dbus.get_name_owner(name.clone()).await {
            Ok(owner) => owner,
            Err(zbus::fdo::Error::NameHasNoOwner(_)) => {
                dbus.start_service_by_name(name.clone().try_into()?, 0)
                    .await?;
                dbus.get_name_owner(name.clone()).await?
            }
            Err(e) => return Err(e.into()),
        };
        // Subscribe to LOSS by this exact owner, rather than only comparing the
        // latest owner. Releasing/reacquiring on the same connection still ends
        // old requests. Recheck after subscribing to close the setup race.
        let bus = zbus::Proxy::new_owned(
            connection.clone(),
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await?;
        let owners = bus
            .receive_signal_with_args(
                "NameOwnerChanged",
                &[(0, "org.freedesktop.portal.Desktop"), (1, owner.as_str())],
            )
            .await?;
        ensure!(
            dbus.get_name_owner(name).await? == owner,
            "portal service lost ownership"
        );
        // Both request and method proxies stay with the issuing unique name;
        // Close and Response must never target/come from its replacement.
        let proxy = zbus::Proxy::new_owned(
            connection.clone(),
            owner.clone(),
            service.path().to_owned(),
            "org.freedesktop.portal.Request",
        )
        .await?;
        // Subscribe before calling the method: a portal may emit before replying.
        let responses = proxy.receive_signal("Response").await?;
        Ok(Self {
            proxy,
            responses,
            owners,
            owner,
            token,
            complete: false,
        })
    }
    pub async fn method_proxy(&self, interface: &'static str) -> Result<zbus::Proxy<'static>> {
        Ok(zbus::Proxy::new_owned(
            self.proxy.connection().clone(),
            self.owner.clone(),
            "/org/freedesktop/portal/desktop",
            interface,
        )
        .await?)
    }
    pub async fn response(
        mut self,
        returned: OwnedObjectPath,
    ) -> Result<Option<HashMap<String, OwnedValue>>> {
        ensure!(
            returned.as_str() == self.proxy.path().as_str(),
            "portal returned an unexpected request path"
        );
        let signal = tokio::select! {
            biased;
            owner = self.owners.next() => {
                owner.context("portal ownership stream ended")?;
                anyhow::bail!("portal service lost ownership");
            }
            signal = self.responses.next() => signal.context("portal response stream ended")?,
        };
        let (code, values): (u32, HashMap<String, OwnedValue>) = signal.body().deserialize()?;
        self.complete = true;
        match code {
            0 => Ok(Some(values)),
            1 => Ok(None), // User cancellation is a completed no-op.
            _ => anyhow::bail!("portal request failed (response {code})"),
        }
    }
}
impl Drop for Request {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        let proxy = self.proxy.clone();
        tokio::spawn(Box::pin(async move {
            let _ =
                tokio::time::timeout(Duration::from_secs(2), proxy.call::<_, _, ()>("Close", &()))
                    .await;
        }));
    }
}

/// Method acceptance must be prompt; the Response may require user interaction.
pub(crate) async fn accept<F>(call: F) -> Result<OwnedObjectPath>
where
    F: std::future::Future<Output = zbus::Result<OwnedObjectPath>>,
{
    Ok(tokio::time::timeout(Duration::from_secs(5), call).await??)
}

pub(crate) async fn with_fallback<P, L, F>(portal: P, launch: L) -> Result<()>
where
    P: std::future::Future<Output = Result<()>>,
    L: FnOnce() -> F,
    F: std::future::Future<Output = Result<()>>,
{
    // Success and cancellation both return Ok; failures/timeouts use fallback.
    if portal.await.is_ok() {
        return Ok(());
    }
    tokio::time::timeout(Duration::from_secs(10), launch()).await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn acceptance_is_bounded_but_interactive_wait_is_not() -> Result<()> {
        assert!(accept(std::future::pending()).await.is_err());
        let start = tokio::time::Instant::now();
        with_fallback(
            async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(())
            },
            || async { panic!("interaction must not use fallback") },
        )
        .await?;
        assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(60));
        Ok(())
    }
}
