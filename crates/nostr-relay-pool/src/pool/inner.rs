// Copyright (c) 2022-2023 Yuki Kishimoto
// Copyright (c) 2023-2024 Rust Nostr Developers
// Distributed under the MIT software license

//! Relay Pool

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_utility::futures_util::{future, StreamExt};
use async_utility::task;
use atomic_destructor::AtomicDestroyer;
use nostr_database::prelude::*;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock, RwLockReadGuard};

use super::options::RelayPoolOptions;
use super::{Error, Output, RelayPoolNotification};
use crate::relay::options::{RelayOptions, ReqExitPolicy, SyncOptions};
use crate::relay::{FlagCheck, Reconciliation, Relay};
use crate::shared::SharedState;
use crate::stream::ReceiverStream;
use crate::{RelayServiceFlags, SubscribeOptions};

type Relays = HashMap<RelayUrl, Relay>;

// Instead of wrap every field in an `Arc<T>`, which increases the number of atomic operations,
// put all fields that require an `Arc` here.
#[derive(Debug)]
struct AtomicPrivateData {
    relays: RwLock<Relays>,
    subscriptions: RwLock<HashMap<SubscriptionId, Vec<Filter>>>,
    shutdown: AtomicBool,
}

#[derive(Debug, Clone)]
pub struct InnerRelayPool {
    pub(super) state: SharedState,
    atomic: Arc<AtomicPrivateData>,
    notification_sender: broadcast::Sender<RelayPoolNotification>, // TODO: move to shared state?
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

    pub async fn shutdown(&self) -> Result<(), Error> {
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

    #[inline]
    pub(super) fn is_shutdown(&self) -> bool {
        self.atomic.shutdown.load(Ordering::SeqCst)
    }

    pub fn notifications(&self) -> broadcast::Receiver<RelayPoolNotification> {
        self.notification_sender.subscribe()
    }

    pub async fn all_relays(&self) -> Relays {
        let relays = self.atomic.relays.read().await;
        relays.clone()
    }

    fn internal_relays_with_flag<'a>(
        &self,
        txn: &'a RwLockReadGuard<'a, Relays>,
        flag: RelayServiceFlags,
        check: FlagCheck,
    ) -> impl Iterator<Item = (&'a RelayUrl, &'a Relay)> + 'a {
        txn.iter().filter(move |(_, r)| r.flags().has(flag, check))
    }

    /// Get relays that has `READ` or `WRITE` flags
    pub async fn relays(&self) -> Relays {
        self.relays_with_flag(
            RelayServiceFlags::READ | RelayServiceFlags::WRITE,
            FlagCheck::Any,
        )
        .await
    }

    /// Get relays that have a certain [RelayServiceFlag] enabled
    pub async fn relays_with_flag(&self, flag: RelayServiceFlags, check: FlagCheck) -> Relays {
        let relays = self.atomic.relays.read().await;
        self.internal_relays_with_flag(&relays, flag, check)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Get relays with `READ` or `WRITE` relays
    async fn relay_urls(&self) -> Vec<RelayUrl> {
        let relays = self.atomic.relays.read().await;
        self.internal_relays_with_flag(
            &relays,
            RelayServiceFlags::READ | RelayServiceFlags::WRITE,
            FlagCheck::Any,
        )
        .map(|(k, ..)| k.clone())
        .collect()
    }

    async fn read_relay_urls(&self) -> Vec<RelayUrl> {
        let relays = self.atomic.relays.read().await;
        self.internal_relays_with_flag(&relays, RelayServiceFlags::READ, FlagCheck::All)
            .map(|(k, ..)| k.clone())
            .collect()
    }

    async fn write_relay_urls(&self) -> Vec<RelayUrl> {
        let relays = self.atomic.relays.read().await;
        self.internal_relays_with_flag(&relays, RelayServiceFlags::WRITE, FlagCheck::All)
            .map(|(k, ..)| k.clone())
            .collect()
    }

    fn internal_relay<'a>(
        &self,
        txn: &'a RwLockReadGuard<'a, Relays>,
        url: &RelayUrl,
    ) -> Result<&'a Relay, Error> {
        txn.get(url).ok_or(Error::RelayNotFound)
    }

    pub async fn relay<U>(&self, url: U) -> Result<Relay, Error>
    where
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        let url: RelayUrl = url.try_into_url()?;
        let relays = self.atomic.relays.read().await;
        self.internal_relay(&relays, &url).cloned()
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

    pub async fn get_or_add_relay(
        &self,
        url: RelayUrl,
        inherit_pool_subscriptions: bool,
        opts: RelayOptions,
    ) -> Result<Option<Relay>, Error> {
        match self.relay(&url).await {
            Ok(relay) => Ok(Some(relay)),
            Err(..) => {
                self.add_relay(url, inherit_pool_subscriptions, opts)
                    .await?;
                Ok(None)
            }
        }
    }

    async fn internal_remove_relay(
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
        relay.disconnect()?;

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
        self.internal_remove_relay(&mut relays, url, force).await
    }

    pub async fn remove_all_relays(&self, force: bool) -> Result<(), Error> {
        // Acquire write lock
        let mut relays = self.atomic.relays.write().await;

        // Collect all relay urls
        let urls: Vec<RelayUrl> = relays.keys().cloned().collect();

        // Iter urls and remove relays
        for url in urls.into_iter() {
            self.internal_remove_relay(&mut relays, url, force).await?;
        }

        Ok(())
    }

    pub async fn send_msg_to<I, U>(&self, urls: I, msg: ClientMessage) -> Result<Output<()>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        self.batch_msg_to(urls, vec![msg]).await
    }

    pub async fn batch_msg_to<I, U>(
        &self,
        urls: I,
        msgs: Vec<ClientMessage>,
    ) -> Result<Output<()>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Compose URLs
        let set: HashSet<RelayUrl> = urls
            .into_iter()
            .map(|u| u.try_into_url())
            .collect::<Result<_, _>>()?;

        // Check if urls set is empty
        if set.is_empty() {
            return Err(Error::NoRelaysSpecified);
        }

        // Lock with read shared access
        let relays = self.atomic.relays.read().await;

        if relays.is_empty() {
            return Err(Error::NoRelays);
        }

        // Check if urls set contains ONLY already added relays
        if !set.iter().all(|url| relays.contains_key(url)) {
            return Err(Error::RelayNotFound);
        }

        // Save events
        for msg in msgs.iter() {
            if let ClientMessage::Event(event) = msg {
                self.state.database().save_event(event).await?;
            }
        }

        let mut output: Output<()> = Output::default();

        // Batch messages and construct outputs
        for url in set.into_iter() {
            let relay: &Relay = self.internal_relay(&relays, &url)?;
            match relay.batch_msg(msgs.clone()) {
                Ok(..) => {
                    // Success, insert relay url in 'success' set result
                    output.success.insert(url);
                }
                Err(e) => {
                    output.failed.insert(url, e.to_string());
                }
            }
        }

        if output.success.is_empty() {
            return Err(Error::Failed);
        }

        Ok(output)
    }

    pub async fn send_event(&self, event: Event) -> Result<Output<EventId>, Error> {
        let urls: Vec<RelayUrl> = self.write_relay_urls().await;
        self.send_event_to(urls, event).await
    }

    pub async fn send_event_to<I, U>(&self, urls: I, event: Event) -> Result<Output<EventId>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Compose URLs
        let set: HashSet<RelayUrl> = urls
            .into_iter()
            .map(|u| u.try_into_url())
            .collect::<Result<_, _>>()?;

        // Check if urls set is empty
        if set.is_empty() {
            return Err(Error::NoRelaysSpecified);
        }

        // Lock with read shared access
        let relays = self.atomic.relays.read().await;

        if relays.is_empty() {
            return Err(Error::NoRelays);
        }

        // Check if urls set contains ONLY already added relays
        if !set.iter().all(|url| relays.contains_key(url)) {
            return Err(Error::RelayNotFound);
        }

        // Save event into database
        self.state.database().save_event(&event).await?;

        let mut urls: Vec<RelayUrl> = Vec::with_capacity(set.len());
        let mut futures = Vec::with_capacity(set.len());
        let mut output: Output<EventId> = Output {
            val: event.id,
            success: HashSet::new(),
            failed: HashMap::new(),
        };

        // Compose futures
        for url in set.into_iter() {
            let relay: &Relay = self.internal_relay(&relays, &url)?;
            let event: Event = event.clone();
            urls.push(url);
            futures.push(relay.send_event(event));
        }

        // Join futures
        let list = future::join_all(futures).await;

        // Iter results and construct output
        for (url, result) in urls.into_iter().zip(list.into_iter()) {
            match result {
                Ok(..) => {
                    // Success, insert relay url in 'success' set result
                    output.success.insert(url);
                }
                Err(e) => {
                    output.failed.insert(url, e.to_string());
                }
            }
        }

        if output.success.is_empty() {
            return Err(Error::Failed);
        }

        Ok(output)
    }

    pub async fn subscribe(
        &self,
        filters: Vec<Filter>,
        opts: SubscribeOptions,
    ) -> Result<Output<SubscriptionId>, Error> {
        let id: SubscriptionId = SubscriptionId::generate();
        let output: Output<()> = self.subscribe_with_id(id.clone(), filters, opts).await?;
        Ok(Output {
            val: id,
            success: output.success,
            failed: output.failed,
        })
    }

    pub async fn subscribe_with_id(
        &self,
        id: SubscriptionId,
        filters: Vec<Filter>,
        opts: SubscribeOptions,
    ) -> Result<Output<()>, Error> {
        // Check if isn't auto-closing subscription
        if !opts.is_auto_closing() {
            // Save subscription
            self.save_subscription(id.clone(), filters.clone()).await;
        }

        // Get relay urls
        let urls: Vec<RelayUrl> = self.read_relay_urls().await;

        // Subscribe
        self.subscribe_with_id_to(urls, id, filters, opts).await
    }

    pub async fn subscribe_to<I, U>(
        &self,
        urls: I,
        filters: Vec<Filter>,
        opts: SubscribeOptions,
    ) -> Result<Output<SubscriptionId>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        let id: SubscriptionId = SubscriptionId::generate();
        let output: Output<()> = self
            .subscribe_with_id_to(urls, id.clone(), filters, opts)
            .await?;
        Ok(Output {
            val: id,
            success: output.success,
            failed: output.failed,
        })
    }

    pub async fn subscribe_with_id_to<I, U>(
        &self,
        urls: I,
        id: SubscriptionId,
        filters: Vec<Filter>,
        opts: SubscribeOptions,
    ) -> Result<Output<()>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        let targets = urls.into_iter().map(|u| (u, filters.clone()));
        self.subscribe_targeted(id, targets, opts).await
    }

    pub async fn subscribe_targeted<I, U>(
        &self,
        id: SubscriptionId,
        targets: I,
        opts: SubscribeOptions,
    ) -> Result<Output<()>, Error>
    where
        I: IntoIterator<Item = (U, Vec<Filter>)>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Collect targets map
        let mut map: HashMap<RelayUrl, Vec<Filter>> = HashMap::new();
        for (url, filters) in targets.into_iter() {
            map.insert(url.try_into_url()?, filters);
        }

        // Check if urls set is empty
        if map.is_empty() {
            return Err(Error::NoRelaysSpecified);
        }

        // Lock with read shared access
        let relays = self.atomic.relays.read().await;

        // Check if relays map is empty
        if relays.is_empty() {
            return Err(Error::NoRelays);
        }

        // Check if urls set contains ONLY already added relays
        if !map.keys().all(|url| relays.contains_key(url)) {
            return Err(Error::RelayNotFound);
        }

        let mut urls: Vec<RelayUrl> = Vec::with_capacity(map.len());
        let mut futures = Vec::with_capacity(map.len());
        let mut output: Output<()> = Output::default();

        // Compose futures
        for (url, filters) in map.into_iter() {
            let relay: &Relay = self.internal_relay(&relays, &url)?;
            let id: SubscriptionId = id.clone();
            urls.push(url);
            futures.push(relay.subscribe_with_id(id, filters, opts));
        }

        // Join futures
        let list = future::join_all(futures).await;

        // Iter results and construct output
        for (url, result) in urls.into_iter().zip(list.into_iter()) {
            match result {
                Ok(..) => {
                    // Success, insert relay url in 'success' set result
                    output.success.insert(url);
                }
                Err(e) => {
                    output.failed.insert(url, e.to_string());
                }
            }
        }

        if output.success.is_empty() {
            return Err(Error::Failed);
        }

        Ok(output)
    }

    pub async fn unsubscribe(&self, id: SubscriptionId) {
        // Remove subscription from pool
        self.remove_subscription(&id).await;

        // Lock with read shared access
        let relays = self.atomic.relays.read().await;

        // TODO: use join_all and return `Output`?

        // Remove subscription from relays
        for relay in relays.values() {
            if let Err(e) = relay.unsubscribe(id.clone()).await {
                tracing::error!("{e}");
            }
        }
    }

    pub async fn unsubscribe_all(&self) {
        // Remove subscriptions from pool
        self.remove_all_subscriptions().await;

        // Lock with read shared access
        let relays = self.atomic.relays.read().await;

        // TODO: use join_all and return `Output`?

        // Unsubscribe relays
        for relay in relays.values() {
            if let Err(e) = relay.unsubscribe_all().await {
                tracing::error!("{e}");
            }
        }
    }

    pub async fn sync(
        &self,
        filter: Filter,
        opts: &SyncOptions,
    ) -> Result<Output<Reconciliation>, Error> {
        let urls: Vec<RelayUrl> = self.relay_urls().await;
        self.sync_with(urls, filter, opts).await
    }

    pub async fn sync_with<I, U>(
        &self,
        urls: I,
        filter: Filter,
        opts: &SyncOptions,
    ) -> Result<Output<Reconciliation>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Get items
        let items: Vec<(EventId, Timestamp)> = self
            .state
            .database()
            .negentropy_items(filter.clone())
            .await?;

        // Compose filters
        let mut filters: HashMap<Filter, Vec<(EventId, Timestamp)>> = HashMap::with_capacity(1);
        filters.insert(filter, items);

        // Reconcile
        let targets = urls.into_iter().map(|u| (u, filters.clone()));
        self.sync_targeted(targets, opts).await
    }

    pub async fn sync_targeted<I, U>(
        &self,
        targets: I,
        opts: &SyncOptions,
    ) -> Result<Output<Reconciliation>, Error>
    where
        I: IntoIterator<Item = (U, HashMap<Filter, Vec<(EventId, Timestamp)>>)>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Collect targets map
        // TODO: create hashmap with capacity
        let mut map: HashMap<RelayUrl, HashMap<Filter, Vec<(EventId, Timestamp)>>> = HashMap::new();
        for (url, value) in targets.into_iter() {
            map.insert(url.try_into_url()?, value);
        }

        // Check if urls set is empty
        if map.is_empty() {
            return Err(Error::NoRelaysSpecified);
        }

        // Lock with read shared access
        let relays = self.atomic.relays.read().await;

        // Check if empty
        if relays.is_empty() {
            return Err(Error::NoRelays);
        }

        // Check if urls set contains ONLY already added relays
        if !map.keys().all(|url| relays.contains_key(url)) {
            return Err(Error::RelayNotFound);
        }

        // TODO: shared reconciliation output to avoid to request duplicates?

        let mut urls: Vec<RelayUrl> = Vec::with_capacity(map.len());
        let mut futures = Vec::with_capacity(map.len());
        let mut output: Output<Reconciliation> = Output::default();

        // Compose futures
        for (url, filters) in map.into_iter() {
            let relay: &Relay = self.internal_relay(&relays, &url)?;
            urls.push(url);
            futures.push(relay.sync_multi(filters, opts));
        }

        // Join futures
        let list = future::join_all(futures).await;

        // Iter results and constructs output
        for (url, result) in urls.into_iter().zip(list.into_iter()) {
            match result {
                Ok(reconciliation) => {
                    // Success, insert relay url in 'success' set result
                    output.success.insert(url);
                    output.merge(reconciliation);
                }
                Err(e) => {
                    output.failed.insert(url, e.to_string());
                }
            }
        }

        // Check if sync failed (no success)
        if output.success.is_empty() {
            return Err(Error::NegentropyReconciliationFailed);
        }

        Ok(output)
    }

    pub async fn fetch_events(
        &self,
        filters: Vec<Filter>,
        timeout: Duration,
        policy: ReqExitPolicy,
    ) -> Result<Events, Error> {
        let urls: Vec<RelayUrl> = self.read_relay_urls().await;
        self.fetch_events_from(urls, filters, timeout, policy).await
    }

    pub async fn fetch_events_from<I, U>(
        &self,
        urls: I,
        filters: Vec<Filter>,
        timeout: Duration,
        policy: ReqExitPolicy,
    ) -> Result<Events, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        let mut events: Events = Events::new(&filters);

        // Stream events
        let mut stream = self
            .stream_events_from(urls, filters, timeout, policy)
            .await?;
        while let Some(event) = stream.next().await {
            events.insert(event);
        }

        Ok(events)
    }

    pub async fn stream_events(
        &self,
        filters: Vec<Filter>,
        timeout: Duration,
        policy: ReqExitPolicy,
    ) -> Result<ReceiverStream<Event>, Error> {
        let urls: Vec<RelayUrl> = self.read_relay_urls().await;
        self.stream_events_from(urls, filters, timeout, policy)
            .await
    }

    pub async fn stream_events_from<I, U>(
        &self,
        urls: I,
        filters: Vec<Filter>,
        timeout: Duration,
        policy: ReqExitPolicy,
    ) -> Result<ReceiverStream<Event>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        let targets = urls.into_iter().map(|u| (u, filters.clone()));
        self.stream_events_targeted(targets, timeout, policy).await
    }

    // TODO: change target type to `HashMap<Url, Vec<Filter>>`?
    pub async fn stream_events_targeted<I, U>(
        &self,
        targets: I,
        timeout: Duration,
        policy: ReqExitPolicy,
    ) -> Result<ReceiverStream<Event>, Error>
    where
        I: IntoIterator<Item = (U, Vec<Filter>)>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Collect targets map
        let mut map: HashMap<RelayUrl, Vec<Filter>> = HashMap::new();
        for (url, filters) in targets.into_iter() {
            map.insert(url.try_into_url()?, filters);
        }

        // Check if urls set is empty
        if map.is_empty() {
            return Err(Error::NoRelaysSpecified);
        }

        // Lock with read shared access
        let relays = self.atomic.relays.read().await;

        // Check if empty
        if relays.is_empty() {
            return Err(Error::NoRelays);
        }

        // Check if urls set contains ONLY already added relays
        if !map.keys().all(|url| relays.contains_key(url)) {
            return Err(Error::RelayNotFound);
        }

        // Drop
        drop(relays);

        // Create channel
        let (tx, rx) = mpsc::channel::<Event>(map.len() * 512);

        // Spawn
        let this = self.clone();
        task::spawn(async move {
            // Lock with read shared access
            let relays = this.atomic.relays.read().await;

            let ids: Mutex<HashSet<EventId>> = Mutex::new(HashSet::new());

            let mut urls: Vec<RelayUrl> = Vec::with_capacity(map.len());
            let mut futures = Vec::with_capacity(map.len());

            // Filter relays and start query
            for (url, filters) in map.into_iter() {
                match this.internal_relay(&relays, &url) {
                    Ok(relay) => {
                        urls.push(url);
                        futures.push(relay.fetch_events_with_callback(
                            filters,
                            timeout,
                            policy,
                            |event| async {
                                let mut ids = ids.lock().await;
                                if ids.insert(event.id) {
                                    drop(ids);
                                    let _ = tx.try_send(event);
                                }
                            },
                        ));
                    }
                    // TODO: remove this
                    Err(e) => tracing::error!("{e}"),
                }
            }

            // Join futures
            let list = future::join_all(futures).await;

            // Iter results
            for (url, result) in urls.into_iter().zip(list.into_iter()) {
                if let Err(e) = result {
                    tracing::error!(url = %url, error = %e, "Failed to stream events.");
                }
            }
        });

        // Return stream
        Ok(ReceiverStream::new(rx))
    }

    pub async fn connect(&self) {
        // Lock with read shared access
        let relays = self.atomic.relays.read().await;

        // Connect
        for relay in relays.values() {
            relay.connect()
        }
    }

    pub async fn wait_for_connection(&self, timeout: Duration) {
        // Lock with read shared access
        let relays = self.atomic.relays.read().await;

        // Compose futures
        let mut futures = Vec::with_capacity(relays.len());
        for relay in relays.values() {
            futures.push(relay.wait_for_connection(timeout));
        }

        // Join futures
        future::join_all(futures).await;
    }

    pub async fn try_connect(&self, timeout: Duration) -> Output<()> {
        // Lock with read shared access
        let relays = self.atomic.relays.read().await;

        let mut urls: Vec<RelayUrl> = Vec::with_capacity(relays.len());
        let mut futures = Vec::with_capacity(relays.len());
        let mut output: Output<()> = Output::default();

        // Filter only relays that can connect and compose futures
        for relay in relays.values().filter(|r| r.status().can_connect()) {
            urls.push(relay.url().clone());
            futures.push(relay.try_connect(timeout));
        }

        // TODO: use semaphore to limit number concurrent connections?

        // Join futures
        let list = future::join_all(futures).await;

        // Iterate results and compose output
        for (url, result) in urls.into_iter().zip(list.into_iter()) {
            match result {
                Ok(..) => {
                    output.success.insert(url);
                }
                Err(e) => {
                    output.failed.insert(url, e.to_string());
                }
            }
        }

        output
    }

    pub async fn disconnect(&self) -> Result<(), Error> {
        // Lock with read shared access
        let relays = self.atomic.relays.read().await;

        // Iter values and disconnect
        for relay in relays.values() {
            relay.disconnect()?;
        }

        Ok(())
    }

    pub async fn connect_relay<U>(&self, url: U) -> Result<(), Error>
    where
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Convert url
        let url: RelayUrl = url.try_into_url()?;

        // Lock with read shared access
        let relays = self.atomic.relays.read().await;

        // Get relay
        let relay: &Relay = self.internal_relay(&relays, &url)?;

        // Connect
        relay.connect();

        Ok(())
    }

    pub async fn try_connect_relay<U>(&self, url: U, timeout: Duration) -> Result<(), Error>
    where
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Convert url
        let url: RelayUrl = url.try_into_url()?;

        // Lock with read shared access
        let relays = self.atomic.relays.read().await;

        // Get relay
        let relay: &Relay = self.internal_relay(&relays, &url)?;

        // Try to connect
        relay.try_connect(timeout).await?;

        Ok(())
    }

    pub async fn disconnect_relay<U>(&self, url: U) -> Result<(), Error>
    where
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Convert url
        let url: RelayUrl = url.try_into_url()?;

        // Lock with read shared access
        let relays = self.atomic.relays.read().await;

        // Get relay
        let relay: &Relay = self.internal_relay(&relays, &url)?;

        // Disconnect
        relay.disconnect()?;

        Ok(())
    }
}
