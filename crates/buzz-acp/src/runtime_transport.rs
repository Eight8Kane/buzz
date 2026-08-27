//! Runtime event transport selected by provisioning.
//!
//! Local mode delegates to the existing authenticated relay client. Broker
//! mode polls the frozen `channel.read` action and deliberately provides no
//! relay publisher: relay-only housekeeping is disabled by broker-mode config.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use buzz_broker_client::HttpBrokerClient;
use buzz_sdk::broker::{
    ActionArgs, ActionOutcome, BrokerClientExt, BrokerErrorCode, BrokerRequest, BrokerResult,
    ChannelReadArgs, StorageAddressArgs,
};
use nostr::{Event, Keys};
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::config::ChannelFilter;
use crate::relay::{
    BuzzEvent, ChannelInfo, HarnessRelay, RelayError, RelayEventPublisher, RestClient,
};

pub enum RuntimeTransport {
    Local(HarnessRelay),
    Broker(BrokerRuntime),
}

pub struct BrokerRuntime {
    client: HttpBrokerClient,
    channel_ids: Vec<Uuid>,
    filters: HashMap<Uuid, ChannelFilter>,
    cursors: HashMap<Uuid, String>,
    pending: VecDeque<BuzzEvent>,
    seen_current: HashSet<String>,
    seen_previous: HashSet<String>,
    poll: tokio::time::Interval,
    placeholder_keys: Keys,
    terminal_error: Option<String>,
}

struct BrokerActionError {
    detail: String,
    code: Option<BrokerErrorCode>,
}

impl std::fmt::Display for BrokerActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl BrokerActionError {
    fn relay_error(self) -> RelayError {
        RelayError::Http(self.detail)
    }
}

impl BrokerRuntime {
    pub async fn connect(
        base_url: String,
        credential: String,
        channel_ids: Vec<Uuid>,
        poll_interval: Duration,
        placeholder_keys: Keys,
    ) -> Result<(Self, String), RelayError> {
        let client = HttpBrokerClient::new(base_url, credential);
        let outcome = execute(
            &client,
            ActionArgs::StorageAddress(StorageAddressArgs {
                slug: "core".into(),
            }),
        )
        .await
        .map_err(BrokerActionError::relay_error)?;
        let ActionOutcome::StorageAddress(address) = outcome else {
            return Err(RelayError::Http(
                "broker returned the wrong outcome for storage.address".into(),
            ));
        };
        let agent_pubkey = address.author_pubkey.as_str().to_string();
        let mut poll = tokio::time::interval(poll_interval);
        poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Ok((
            Self {
                client,
                channel_ids,
                filters: HashMap::new(),
                cursors: HashMap::new(),
                pending: VecDeque::new(),
                seen_current: HashSet::new(),
                seen_previous: HashSet::new(),
                poll,
                placeholder_keys,
                terminal_error: None,
            },
            agent_pubkey,
        ))
    }

    fn remember(&mut self, event_id: String) -> bool {
        if self.seen_current.contains(&event_id) || self.seen_previous.contains(&event_id) {
            return false;
        }
        self.seen_current.insert(event_id);
        if self.seen_current.len() >= 2_000 {
            self.seen_previous = std::mem::take(&mut self.seen_current);
        }
        true
    }

    async fn next_event(&mut self) -> Option<BuzzEvent> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            self.poll.tick().await;
            let subscriptions: Vec<(Uuid, bool)> = self
                .channel_ids
                .iter()
                .filter_map(|channel_id| {
                    self.filters
                        .get(channel_id)
                        .map(|filter| (*channel_id, filter.require_mention))
                })
                .collect();
            for (channel_id, mentions_only) in subscriptions {
                let result = execute(
                    &self.client,
                    ActionArgs::ChannelRead(ChannelReadArgs {
                        channel_id: channel_id.to_string(),
                        root_event_id: None,
                        mentions_only,
                        cursor: self.cursors.get(&channel_id).cloned(),
                        limit: Some(100),
                    }),
                )
                .await;
                let page = match result {
                    Ok(ActionOutcome::ChannelRead(page)) => page,
                    Ok(_) => {
                        tracing::warn!(%channel_id, "broker returned the wrong channel.read outcome");
                        continue;
                    }
                    Err(error) if error.code == Some(BrokerErrorCode::Unauthenticated) => {
                        tracing::error!(%channel_id, "broker credential was rejected: {error}");
                        self.terminal_error = Some(error.to_string());
                        return None;
                    }
                    Err(error) => {
                        tracing::warn!(%channel_id, "broker channel.read failed: {error}");
                        continue;
                    }
                };
                if let Some(cursor) = page.next_cursor {
                    self.cursors.insert(channel_id, cursor);
                }
                for message in page.messages {
                    if let Err(error) = message.verify() {
                        tracing::warn!(%channel_id, "broker returned an unverifiable event: {error}");
                        continue;
                    }
                    let event = message.0;
                    let addressed_channel = event
                        .tags
                        .iter()
                        .find(|tag| tag.kind().to_string() == "h")
                        .and_then(|tag| tag.content())
                        .and_then(|value| Uuid::parse_str(value).ok());
                    if addressed_channel != Some(channel_id) {
                        tracing::warn!(%channel_id, "broker returned an event for a different channel");
                        continue;
                    }
                    if self.remember(event.id.to_hex()) {
                        self.pending.push_back(BuzzEvent { channel_id, event });
                    }
                }
            }
        }
    }
}

async fn execute(
    client: &HttpBrokerClient,
    args: ActionArgs,
) -> Result<ActionOutcome, BrokerActionError> {
    let request = BrokerRequest::new(Uuid::new_v4().to_string(), args)
        .and_then(BrokerRequest::prepare)
        .map_err(|error| BrokerActionError {
            detail: format!("broker request: {error}"),
            code: None,
        })?;
    let response = client
        .execute(&request)
        .await
        .map_err(|error| BrokerActionError {
            detail: format!("broker transport: {error}"),
            code: None,
        })?;
    match response.into_envelope().result {
        BrokerResult::Succeeded { outcome } => Ok(outcome),
        BrokerResult::Failed { error } | BrokerResult::Indeterminate { error } => {
            Err(BrokerActionError {
                detail: format!(
                    "broker verdict: {} [{}]",
                    error.message,
                    error.code.as_str()
                ),
                code: Some(error.code),
            })
        }
    }
}

impl RuntimeTransport {
    pub fn local(relay: HarnessRelay) -> Self {
        Self::Local(relay)
    }

    pub async fn broker(
        base_url: String,
        credential: String,
        channel_ids: Vec<Uuid>,
        poll_interval: Duration,
        placeholder_keys: Keys,
    ) -> Result<(Self, String), RelayError> {
        let (runtime, pubkey) = BrokerRuntime::connect(
            base_url,
            credential,
            channel_ids,
            poll_interval,
            placeholder_keys,
        )
        .await?;
        Ok((Self::Broker(runtime), pubkey))
    }

    pub async fn set_startup_watermark(&self, timestamp: u64) -> Result<(), RelayError> {
        match self {
            Self::Local(relay) => relay.set_startup_watermark(timestamp).await,
            Self::Broker(_) => Ok(()),
        }
    }

    pub async fn subscribe_membership_notifications(&mut self) -> Result<(), RelayError> {
        match self {
            Self::Local(relay) => relay.subscribe_membership_notifications().await,
            Self::Broker(_) => Ok(()),
        }
    }

    pub async fn subscribe_observer_controls(&mut self) -> Result<(), RelayError> {
        match self {
            Self::Local(relay) => relay.subscribe_observer_controls().await,
            Self::Broker(_) => Ok(()),
        }
    }

    pub fn take_observer_control_rx(&mut self) -> Option<tokio::sync::mpsc::Receiver<Event>> {
        match self {
            Self::Local(relay) => relay.take_observer_control_rx(),
            Self::Broker(_) => None,
        }
    }

    pub async fn discover_channels(&self) -> Result<HashMap<Uuid, ChannelInfo>, RelayError> {
        match self {
            Self::Local(relay) => relay.discover_channels().await,
            Self::Broker(runtime) => Ok(runtime
                .channel_ids
                .iter()
                .map(|channel_id| {
                    (
                        *channel_id,
                        ChannelInfo {
                            name: channel_id.to_string(),
                            // The frozen broker contract does not expose channel
                            // metadata. Keep the type unknown so the runtime's
                            // author gate treats it as a DM (fail closed).
                            channel_type: "unknown".into(),
                            description: None,
                        },
                    )
                })
                .collect()),
        }
    }

    pub async fn subscribe_channel(
        &mut self,
        channel_id: Uuid,
        filter: ChannelFilter,
    ) -> Result<(), RelayError> {
        match self {
            Self::Local(relay) => relay.subscribe_channel(channel_id, filter).await,
            Self::Broker(runtime) => {
                runtime.filters.insert(channel_id, filter);
                Ok(())
            }
        }
    }

    pub async fn subscribe_channel_from(
        &mut self,
        channel_id: Uuid,
        filter: ChannelFilter,
        replay_since: Option<u64>,
    ) -> Result<(), RelayError> {
        match self {
            Self::Local(relay) => {
                relay
                    .subscribe_channel_from(channel_id, filter, replay_since)
                    .await
            }
            Self::Broker(runtime) => {
                runtime.filters.insert(channel_id, filter);
                Ok(())
            }
        }
    }

    pub async fn unsubscribe_channel(&mut self, channel_id: Uuid) -> Result<(), RelayError> {
        match self {
            Self::Local(relay) => relay.unsubscribe_channel(channel_id).await,
            Self::Broker(runtime) => {
                runtime.filters.remove(&channel_id);
                Ok(())
            }
        }
    }

    pub async fn next_event(&mut self) -> Option<BuzzEvent> {
        match self {
            Self::Local(relay) => relay.next_event().await,
            Self::Broker(runtime) => runtime.next_event().await,
        }
    }

    pub async fn reconnect(&mut self) -> Result<(), RelayError> {
        match self {
            Self::Local(relay) => relay.reconnect().await,
            Self::Broker(runtime) => match &runtime.terminal_error {
                Some(error) => Err(RelayError::Http(error.clone())),
                None => Ok(()),
            },
        }
    }

    pub fn event_publisher(&self) -> RelayEventPublisher {
        match self {
            Self::Local(relay) => relay.event_publisher(),
            Self::Broker(_) => RelayEventPublisher::disabled(),
        }
    }

    pub fn rest_client(&self) -> RestClient {
        match self {
            Self::Local(relay) => relay.rest_client(),
            Self::Broker(runtime) => RestClient {
                http: reqwest::Client::new(),
                base_url: "http://127.0.0.1:1".into(),
                keys: runtime.placeholder_keys.clone(),
                auth_tag_json: None,
            },
        }
    }

    pub fn build_typing_event(
        &self,
        channel_id: Uuid,
        root_event_id: Option<&str>,
        parent_event_id: Option<&str>,
    ) -> Result<Event, RelayError> {
        match self {
            Self::Local(relay) => {
                relay.build_typing_event(channel_id, root_event_id, parent_event_id)
            }
            Self::Broker(_) => Err(RelayError::Http(
                "typing indicators are disabled in broker mode".into(),
            )),
        }
    }

    pub fn try_publish_event(&self, event: Event) -> Result<(), RelayError> {
        match self {
            Self::Local(relay) => relay.try_publish_event(event),
            Self::Broker(_) => Err(RelayError::ConnectionClosed),
        }
    }

    pub async fn shutdown(self) {
        if let Self::Local(relay) = self {
            relay.shutdown().await;
        }
    }
}
