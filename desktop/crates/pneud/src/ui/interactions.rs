use anyhow::Result;
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
