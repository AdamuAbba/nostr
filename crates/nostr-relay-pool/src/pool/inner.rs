// Copyright (c) 2022-2023 Yuki Kishimoto
// Copyright (c) 2023-2024 Rust Nostr Developers
// Distributed under the MIT software license

//! Relay Pool

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_utility::task;
use atomic_destructor::AtomicDestroyer;
use nostr_database::prelude::*;
use tokio::sync::{broadcast, RwLock};

use super::options::RelayPoolOptions;
use super::{Error, RelayPoolNotification};
use crate::relay::options::RelayOptions;
use crate::relay::Relay;
use crate::shared::SharedState;
use crate::RelayServiceFlags;

pub(super) type Relays = HashMap<RelayUrl, Relay>;

// Instead of wrap every field in an `Arc<T>`, which increases the number of atomic operations,
// put all fields that require an `Arc` here.
#[derive(Debug)]
pub(super) struct AtomicPrivateData {
    pub(super) relays: RwLock<Relays>,
    subscriptions: RwLock<HashMap<SubscriptionId, Vec<Filter>>>,
    shutdown: AtomicBool,
}

#[derive(Debug, Clone)]
pub struct InnerRelayPool {
    pub(super) state: SharedState,
    pub(super) atomic: Arc<AtomicPrivateData>,
    pub(super) notification_sender: broadcast::Sender<RelayPoolNotification>, // TODO: move to shared state?
    opts: RelayPoolOptions,
}

impl AtomicDestroyer for InnerRelayPool {
    fn on_destroy(&self) {
        let pool = self.clone();
        task::spawn(async move {
            match pool.shutdown().await {
                Ok(()) => tracing::debug!("Relay pool destroyed."),
                Err(e) => tracing::error!(error = %e, "Impossible to destroy pool."),
            }
        });
    }
}

impl InnerRelayPool {
    pub fn new(opts: RelayPoolOptions, state: SharedState) -> Self {
        let (notification_sender, _) = broadcast::channel(opts.notification_channel_size);

        Self {
            state,
            atomic: Arc::new(AtomicPrivateData {
                relays: RwLock::new(HashMap::new()),
                subscriptions: RwLock::new(HashMap::new()),
                shutdown: AtomicBool::new(false),
            }),
            notification_sender,
            opts,
        }
    }

    pub(super) fn is_shutdown(&self) -> bool {
        self.atomic.shutdown.load(Ordering::SeqCst)
    }

    pub async fn shutdown(&self) -> Result<(), Error> {
        if self.is_shutdown() {
            return Ok(());
        }

        // Disconnect and force remove all relays
        self.remove_all_relays(true).await?;

        // Send shutdown notification
        let _ = self
            .notification_sender
            .send(RelayPoolNotification::Shutdown);

        // Mark as shutdown
        self.atomic.shutdown.store(true, Ordering::SeqCst);

        Ok(())
    }

    pub async fn subscriptions(&self) -> HashMap<SubscriptionId, Vec<Filter>> {
        self.atomic.subscriptions.read().await.clone()
    }

    pub async fn subscription(&self, id: &SubscriptionId) -> Option<Vec<Filter>> {
        let subscriptions = self.atomic.subscriptions.read().await;
        subscriptions.get(id).cloned()
    }

    pub async fn save_subscription(&self, id: SubscriptionId, filters: Vec<Filter>) {
        let mut subscriptions = self.atomic.subscriptions.write().await;
        let current: &mut Vec<Filter> = subscriptions.entry(id).or_default();
        *current = filters;
    }

    pub(crate) async fn remove_subscription(&self, id: &SubscriptionId) {
        let mut subscriptions = self.atomic.subscriptions.write().await;
        subscriptions.remove(id);
    }

    pub(crate) async fn remove_all_subscriptions(&self) {
        let mut subscriptions = self.atomic.subscriptions.write().await;
        subscriptions.clear();
    }

    pub async fn add_relay<U>(
        &self,
        url: U,
        inherit_pool_subscriptions: bool,
        opts: RelayOptions,
    ) -> Result<bool, Error>
    where
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Convert into url
        let url: RelayUrl = url.try_into_url()?;

        // Check if the pool has been shutdown
        if self.is_shutdown() {
            return Err(Error::Shutdown);
        }

        // Get relays
        let mut relays = self.atomic.relays.write().await;

        // Check if map already contains url
        if relays.contains_key(&url) {
            return Ok(false);
        }

        // Check number fo relays and limit
        if let Some(max) = self.opts.max_relays {
            if relays.len() >= max {
                return Err(Error::TooManyRelays { limit: max });
            }
        }

        // Compose new relay
        let relay: Relay = Relay::internal_custom(url, self.state.clone(), opts);

        // Set notification sender
        relay
            .inner
            .set_notification_sender(self.notification_sender.clone())?;

        // Set relay subscriptions
        if inherit_pool_subscriptions {
            let subscriptions = self.subscriptions().await;
            for (id, filters) in subscriptions.into_iter() {
                relay.inner.update_subscription(id, filters, false).await;
            }
        }

        // Insert relay into map
        relays.insert(relay.url().clone(), relay);

        Ok(true)
    }

    fn internal_remove_relay(
        &self,
        relays: &mut Relays,
        url: RelayUrl,
        force: bool,
    ) -> Result<(), Error> {
        // Remove relay
        let relay = relays.remove(&url).ok_or(Error::RelayNotFound)?;

        // If NOT force, check if has `GOSSIP` flag
        if !force {
            let flags = relay.flags();
            if flags.has_any(RelayServiceFlags::GOSSIP) {
                // Remove READ, WRITE and DISCOVERY flags
                flags.remove(
                    RelayServiceFlags::READ
                        | RelayServiceFlags::WRITE
                        | RelayServiceFlags::DISCOVERY,
                );

                // Re-insert
                relays.insert(url, relay);
                return Ok(());
            }
        }

        // Disconnect
        relay.disconnect();

        Ok(())
    }

    pub async fn remove_relay<U>(&self, url: U, force: bool) -> Result<(), Error>
    where
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Convert into url
        let url: RelayUrl = url.try_into_url()?;

        // Acquire write lock
        let mut relays = self.atomic.relays.write().await;

        // Remove
        self.internal_remove_relay(&mut relays, url, force)
    }

    pub async fn remove_all_relays(&self, force: bool) -> Result<(), Error> {
        // Acquire write lock
        let mut relays = self.atomic.relays.write().await;

        // Collect all relay urls
        let urls: Vec<RelayUrl> = relays.keys().cloned().collect();

        // Iter urls and remove relays
        for url in urls.into_iter() {
            // TODO: don't propagate error here, it will never return error
            self.internal_remove_relay(&mut relays, url, force)?;
        }

        Ok(())
    }
}
