//! Bounded endpoint routing. OS file locks share capacity across Swarm processes
//! and release automatically after cancellation, failure, or process death.
use crate::{
    ModelPricing, Provider, ProviderError, ProviderStream, SendRequest, Session, SessionConfig,
};
use async_trait::async_trait;
use futures::{StreamExt, stream};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use swarm_core::{
    ProviderId, SessionId,
    budget::BillingMode,
    provider::{ProviderCapabilities, ProviderKind},
};

struct Endpoint {
    provider: Arc<dyn Provider>,
    slots: Vec<PathBuf>,
}
pub struct PoolProvider {
    id: ProviderId,
    endpoints: Arc<Vec<Endpoint>>,
    next: Arc<AtomicUsize>,
    cap: u32,
    global_slots: Vec<PathBuf>,
}

fn slots(key: &str, capacity: u32) -> Result<Vec<PathBuf>, ProviderError> {
    let root = std::env::var_os("SWARM_POOL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("swarm-provider-pool"));
    let hash: String = Sha256::digest(key.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let root = root.join(hash);
    std::fs::create_dir_all(&root).map_err(|e| ProviderError::Session(e.to_string()))?;
    Ok((0..capacity)
        .map(|i| root.join(format!("{i}.lock")))
        .collect())
}

fn try_slot(slots: &[PathBuf]) -> Result<Option<File>, ProviderError> {
    for path in slots {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| ProviderError::Session(e.to_string()))?;
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(error) => return Err(ProviderError::Session(format!("pool lock: {error}"))),
        }
    }
    Ok(None)
}

impl PoolProvider {
    pub fn new(
        id: ProviderId,
        endpoints: Vec<(String, u32, Arc<dyn Provider>)>,
        cap: u32,
    ) -> Result<Self, ProviderError> {
        if endpoints.is_empty() || cap == 0 || endpoints.iter().any(|(_, c, _)| *c == 0) {
            return Err(ProviderError::Session(
                "provider pool needs positive capacities and at least one endpoint".into(),
            ));
        }
        let mut keys = std::collections::HashSet::new();
        if endpoints
            .iter()
            .any(|(url, _, _)| !keys.insert(url.trim_end_matches('/').to_string()))
        {
            return Err(ProviderError::Session(
                "duplicate endpoint URLs do not add capacity".into(),
            ));
        }
        let mut identity: Vec<_> = keys.into_iter().collect();
        identity.sort();
        let global_slots = slots(&format!("pool:{}:{}", id, identity.join("|")), cap)?;
        let endpoints = endpoints
            .into_iter()
            .map(|(url, capacity, provider)| {
                Ok(Endpoint {
                    provider,
                    slots: slots(url.trim_end_matches('/'), capacity)?,
                })
            })
            .collect::<Result<Vec<_>, ProviderError>>()?;
        Ok(Self {
            id,
            endpoints: Arc::new(endpoints),
            next: Arc::new(AtomicUsize::new(0)),
            cap,
            global_slots,
        })
    }
}

#[async_trait]
impl Provider for PoolProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }
    fn model(&self) -> &str {
        self.endpoints[0].provider.model()
    }
    fn kind(&self) -> ProviderKind {
        self.endpoints[0].provider.kind()
    }
    fn billing(&self) -> BillingMode {
        self.endpoints[0].provider.billing()
    }
    fn pricing(&self) -> ModelPricing {
        self.endpoints[0].provider.pricing()
    }
    fn capabilities(&self) -> ProviderCapabilities {
        let mut caps = self.endpoints[0].provider.capabilities();
        caps.max_concurrency = Some(
            self.cap
                .min(self.endpoints.iter().map(|e| e.slots.len() as u32).sum()),
        );
        caps
    }
    async fn start_session(
        &self,
        config: SessionConfig,
    ) -> Result<Box<dyn Session>, ProviderError> {
        let mut sessions = Vec::new();
        for endpoint in self.endpoints.iter() {
            sessions.push(endpoint.provider.start_session(config.clone()).await?);
        }
        Ok(Box::new(PoolSession {
            id: self.id.clone(),
            session_id: SessionId::new(),
            model: self.model().into(),
            endpoints: self.endpoints.clone(),
            next: self.next.clone(),
            sessions,
            global_slots: self.global_slots.clone(),
        }))
    }
}

struct PoolSession {
    id: ProviderId,
    session_id: SessionId,
    model: String,
    endpoints: Arc<Vec<Endpoint>>,
    next: Arc<AtomicUsize>,
    sessions: Vec<Box<dyn Session>>,
    global_slots: Vec<PathBuf>,
}
#[async_trait]
impl Session for PoolSession {
    fn provider_id(&self) -> &ProviderId {
        &self.id
    }
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    fn model(&self) -> &str {
        &self.model
    }
    async fn stream(&self, request: &SendRequest) -> Result<ProviderStream, ProviderError> {
        let started = std::time::Instant::now();
        loop {
            if started.elapsed() > request.deadline.unwrap_or(Duration::from_secs(600)) {
                return Err(ProviderError::Timeout(started.elapsed()));
            }
            if let Some(global) = try_slot(&self.global_slots)? {
                let start = self.next.fetch_add(1, Ordering::Relaxed);
                for offset in 0..self.endpoints.len() {
                    let i = start.wrapping_add(offset) % self.endpoints.len();
                    if let Some(endpoint) = try_slot(&self.endpoints[i].slots)? {
                        let response = self.sessions[i].stream(request).await?;
                        return Ok(Box::pin(stream::unfold(
                            (response, global, endpoint),
                            |(mut response, global, endpoint)| async move {
                                response
                                    .next()
                                    .await
                                    .map(|item| (item, (response, global, endpoint)))
                            },
                        )));
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    async fn close(&self) -> Result<(), ProviderError> {
        for session in &self.sessions {
            session.close().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MockProvider, MockStep};
    use swarm_core::message::ChatMessage;

    #[tokio::test]
    async fn routes_across_endpoints_and_releases_capacity_after_drop() {
        let key = format!("pool-test-{}", SessionId::new());
        let a: Arc<dyn Provider> =
            Arc::new(MockProvider::unowned().with_script(vec![MockStep::text("a")]));
        let b: Arc<dyn Provider> =
            Arc::new(MockProvider::unowned().with_script(vec![MockStep::text("b")]));
        let pool = PoolProvider::new(
            ProviderId::new(&key).unwrap(),
            vec![(format!("{key}/a"), 1, a), (format!("{key}/b"), 1, b)],
            1,
        )
        .unwrap();
        let first = pool.start_session(SessionConfig::default()).await.unwrap();
        let second = pool.start_session(SessionConfig::default()).await.unwrap();
        let req = SendRequest::new(vec![ChatMessage::user("go")]);
        let held = first.stream(&req).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(80), second.stream(&req))
                .await
                .is_err(),
            "global cap must hold until stream drops"
        );
        drop(held);
        let response = second.send(req).await.unwrap();
        assert_eq!(
            response.text, "b",
            "the next endpoint receives the next request"
        );
    }

    #[test]
    fn independent_pool_instances_share_endpoint_locks() {
        let key = format!("shared-{}", SessionId::new());
        let a = slots(&key, 1).unwrap();
        let b = slots(&key, 1).unwrap();
        let held = try_slot(&a).unwrap().unwrap();
        assert!(try_slot(&b).unwrap().is_none());
        drop(held);
        assert!(try_slot(&b).unwrap().is_some());
    }
}
