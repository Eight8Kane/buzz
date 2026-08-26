//! The relay-touching operations an agent performs, over either a local key +
//! relay ([`LocalBackend`]) or a keyless broker host ([`BrokerBackend`]).
//!
//! Both implement [`AgentBackend`] and speak the broker's vocabulary, so the
//! command layer depends on the trait and never on which side holds the key.
//! Selection is by provisioning: a broker endpoint + credential picks the
//! keyless path; a local key picks the relay path. This is the "seam that
//! covers both" from the keyless plan, done as one abstraction rather than
//! scattered conditionals.

use nostr::Event;
use uuid::Uuid;

use buzz_sdk::broker::{
    ActionArgs, ActionOutcome, BrokerClientExt, BrokerError, BrokerRequest, BrokerResult,
    ChannelReadArgs, EventPublished, MessagePage, MessagePostArgs, MessageReplyArgs, PubkeyHex,
};
use buzz_sdk::ThreadRef;

use crate::broker_client::HttpBrokerClient;
use crate::client::BuzzClient;
use crate::error::CliError;

/// The operations an agent performs, in the broker's vocabulary.
///
/// A closed set today — read a channel, post, reply — mirroring the contract's
/// first slice. Adding one is a change here *and* to the contract, deliberately.
#[allow(async_fn_in_trait)] // dispatched through the `Backend` enum, never `dyn`.
pub trait AgentBackend {
    async fn channel_read(&self, args: ChannelReadArgs) -> Result<MessagePage, CliError>;
    async fn message_post(&self, args: MessagePostArgs) -> Result<EventPublished, CliError>;
    async fn message_reply(&self, args: MessageReplyArgs) -> Result<EventPublished, CliError>;
}

/// Keyless backend: no key, no relay route. Every operation is a broker request.
pub struct BrokerBackend {
    client: HttpBrokerClient,
}

impl BrokerBackend {
    #[must_use]
    pub fn new(client: HttpBrokerClient) -> Self {
        Self { client }
    }

    /// Freeze one action, send it, and unwrap the host's verdict to its outcome.
    async fn run(&self, args: ActionArgs) -> Result<ActionOutcome, CliError> {
        let request = BrokerRequest::new(Uuid::new_v4().to_string(), args)
            .and_then(BrokerRequest::prepare)
            .map_err(|e| CliError::Other(format!("broker request: {e}")))?;
        let validated = self
            .client
            .execute(&request)
            .await
            .map_err(|e| CliError::Other(format!("broker transport: {e}")))?;
        match validated.into_envelope().result {
            BrokerResult::Succeeded { outcome } => Ok(outcome),
            BrokerResult::Failed { error } => Err(broker_verdict("failed", &error)),
            BrokerResult::Indeterminate { error } => Err(broker_verdict("indeterminate", &error)),
        }
    }
}

impl AgentBackend for BrokerBackend {
    async fn channel_read(&self, args: ChannelReadArgs) -> Result<MessagePage, CliError> {
        match self.run(ActionArgs::ChannelRead(args)).await? {
            ActionOutcome::ChannelRead(page) => Ok(page),
            _ => Err(unexpected_outcome("channel.read")),
        }
    }

    async fn message_post(&self, args: MessagePostArgs) -> Result<EventPublished, CliError> {
        match self.run(ActionArgs::MessagePost(args)).await? {
            ActionOutcome::MessagePost(published) => Ok(published),
            _ => Err(unexpected_outcome("message.post")),
        }
    }

    async fn message_reply(&self, args: MessageReplyArgs) -> Result<EventPublished, CliError> {
        match self.run(ActionArgs::MessageReply(args)).await? {
            ActionOutcome::MessageReply(published) => Ok(published),
            _ => Err(unexpected_outcome("message.reply")),
        }
    }
}

/// Local backend: holds the key and talks to the relay directly. Preserves
/// today's behavior by reusing the shared `buzz_sdk` builders the CLI already
/// signs and submits, so there is no parallel message-construction path.
pub struct LocalBackend {
    client: BuzzClient,
}

impl LocalBackend {
    #[must_use]
    pub fn new(client: BuzzClient) -> Self {
        Self { client }
    }

    /// Compute the outcome from the locally-signed event, then submit it.
    async fn publish(&self, event: Event) -> Result<EventPublished, CliError> {
        let published = EventPublished {
            event_id: event.id.to_hex(),
            kind: u32::from(event.kind.as_u16()),
            created_at: event.created_at.as_secs(),
        };
        self.client.submit_event(event).await?;
        Ok(published)
    }
}

impl AgentBackend for LocalBackend {
    async fn channel_read(&self, args: ChannelReadArgs) -> Result<MessagePage, CliError> {
        let mut filter = serde_json::json!({
            "kinds": [9, 40002, 40008, 45001, 45003],
            "#h": [args.channel_id],
            "limit": args.effective_limit(),
        });
        if args.mentions_only {
            filter["#p"] = serde_json::json!([self.client.keys().public_key().to_hex()]);
        }
        if let Some(root) = &args.root_event_id {
            filter["#e"] = serde_json::json!([root]);
        }

        let raw = self.client.query(&filter).await?;
        let values: Vec<serde_json::Value> =
            serde_json::from_str(&raw).map_err(|e| CliError::Other(format!("parse read: {e}")))?;
        let messages = values
            .into_iter()
            .filter_map(|v| serde_json::from_value::<Event>(v).ok())
            .map(buzz_sdk::broker::BrokerMessage)
            .collect();
        // Local paging differs from a host's; a cursor is a host concept, so a
        // local read is a single window with no continuation. Fuller paging is
        // deferred until the local path is retrofitted onto the trait.
        Ok(MessagePage {
            messages,
            next_cursor: None,
        })
    }

    async fn message_post(&self, args: MessagePostArgs) -> Result<EventPublished, CliError> {
        let channel = Uuid::parse_str(&args.channel_id)
            .map_err(|e| CliError::Other(format!("channel id: {e}")))?;
        let mentions: Vec<&str> = args.mentions.iter().map(PubkeyHex::as_str).collect();
        let builder = buzz_sdk::build_message(channel, &args.content, None, &mentions, false, &[])
            .map_err(|e| CliError::Other(format!("build_message: {e}")))?;
        let event = self.client.sign_event(builder)?;
        self.publish(event).await
    }

    async fn message_reply(&self, args: MessageReplyArgs) -> Result<EventPublished, CliError> {
        let channel = Uuid::parse_str(&args.channel_id)
            .map_err(|e| CliError::Other(format!("channel id: {e}")))?;
        let parent = nostr::EventId::from_hex(&args.reply_to_event_id)
            .map_err(|e| CliError::Other(format!("reply target: {e}")))?;
        // Direct reply: root == parent. Nested-thread root derivation (the
        // host's job under the broker) is deferred for the local path.
        let thread_ref = ThreadRef {
            root_event_id: parent,
            parent_event_id: parent,
        };
        let mentions: Vec<&str> = args.mentions.iter().map(PubkeyHex::as_str).collect();
        let builder = buzz_sdk::build_message(
            channel,
            &args.content,
            Some(&thread_ref),
            &mentions,
            false,
            &[],
        )
        .map_err(|e| CliError::Other(format!("build_message: {e}")))?;
        let event = self.client.sign_event(builder)?;
        self.publish(event).await
    }
}

/// A runtime-selected backend. Implements [`AgentBackend`] by dispatch, so
/// commands hold one value and never branch on custody.
pub enum Backend {
    Local(Box<LocalBackend>),
    Broker(BrokerBackend),
}

impl Backend {
    /// Keyless: talk to a broker `base_url` with `credential`.
    #[must_use]
    pub fn broker(base_url: impl Into<String>, credential: impl Into<String>) -> Self {
        Self::Broker(BrokerBackend::new(HttpBrokerClient::new(
            base_url, credential,
        )))
    }

    /// Local: hold the key and talk to the relay.
    #[must_use]
    pub fn local(client: BuzzClient) -> Self {
        Self::Local(Box::new(LocalBackend::new(client)))
    }
}

impl AgentBackend for Backend {
    async fn channel_read(&self, args: ChannelReadArgs) -> Result<MessagePage, CliError> {
        match self {
            Self::Local(b) => b.channel_read(args).await,
            Self::Broker(b) => b.channel_read(args).await,
        }
    }

    async fn message_post(&self, args: MessagePostArgs) -> Result<EventPublished, CliError> {
        match self {
            Self::Local(b) => b.message_post(args).await,
            Self::Broker(b) => b.message_post(args).await,
        }
    }

    async fn message_reply(&self, args: MessageReplyArgs) -> Result<EventPublished, CliError> {
        match self {
            Self::Local(b) => b.message_reply(args).await,
            Self::Broker(b) => b.message_reply(args).await,
        }
    }
}

fn broker_verdict(status: &str, error: &BrokerError) -> CliError {
    CliError::Other(format!(
        "broker {status}: {} [{}]",
        error.message,
        error.code.as_str()
    ))
}

fn unexpected_outcome(action: &str) -> CliError {
    CliError::Other(format!(
        "broker returned an outcome that is not for {action}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::Arc;

    use axum::body::{Body, Bytes};
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::Response;
    use axum::routing::post;
    use axum::Router;
    use nostr::{EventBuilder, Keys, Kind};
    use tokio::net::TcpListener;

    const CHANNEL: &str = "5df7dfa8-e919-43df-8efd-f1dcb8af7071";
    const EVENT_ID: &str = "cacf5f811cc8ef3f4af3f92cc222f92a86cdf6a26728a144c8e63b74ab6db359";

    type Responder = Arc<dyn Fn(&str, &str) -> (StatusCode, String) + Send + Sync>;

    /// Spawn a broker host that echoes the request's `requestId`/`action` into
    /// whatever `f` builds, so correlation always holds.
    async fn spawn_host<F>(f: F) -> BrokerBackend
    where
        F: Fn(&str, &str) -> (StatusCode, String) + Send + Sync + 'static,
    {
        let responder: Responder = Arc::new(f);
        let app = Router::new()
            .route(
                "/v1/action",
                post(|State(r): State<Responder>, body: Bytes| async move {
                    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let rid = v.get("requestId").and_then(|x| x.as_str()).unwrap_or("");
                    let action = v.get("action").and_then(|x| x.as_str()).unwrap_or("");
                    let (status, out) = r(rid, action);
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(out))
                        .unwrap()
                }),
            )
            .with_state(responder);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        BrokerBackend::new(HttpBrokerClient::new(format!("http://{addr}"), "cred"))
    }

    fn succeeded(rid: &str, action: &str, outcome: serde_json::Value) -> (StatusCode, String) {
        let body = serde_json::json!({
            "type": "broker_result",
            "protocolVersion": 1,
            "requestId": rid,
            "status": "succeeded",
            "action": action,
            "outcome": outcome,
        });
        (StatusCode::OK, body.to_string())
    }

    #[tokio::test]
    async fn broker_post_returns_the_published_event() {
        let backend = spawn_host(|rid, action| {
            succeeded(
                rid,
                action,
                serde_json::json!({ "eventId": EVENT_ID, "kind": 9, "createdAt": 1_700_000_000u64 }),
            )
        })
        .await;

        let published = backend
            .message_post(MessagePostArgs {
                channel_id: CHANNEL.into(),
                content: "hello".into(),
                mentions: Vec::new(),
            })
            .await
            .expect("published");

        assert_eq!(published.event_id, EVENT_ID);
        assert_eq!(published.kind, 9);
    }

    #[tokio::test]
    async fn broker_reply_returns_the_published_event() {
        let backend = spawn_host(|rid, action| {
            succeeded(
                rid,
                action,
                serde_json::json!({ "eventId": EVENT_ID, "kind": 9, "createdAt": 1_700_000_000u64 }),
            )
        })
        .await;

        let published = backend
            .message_reply(MessageReplyArgs {
                channel_id: CHANNEL.into(),
                reply_to_event_id: EVENT_ID.into(),
                content: "on it".into(),
                mentions: Vec::new(),
            })
            .await
            .expect("published");

        assert_eq!(published.event_id, EVENT_ID);
    }

    #[tokio::test]
    async fn broker_read_returns_a_page_of_signed_events() {
        // A real signed event, so the strict event reader accepts it.
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(9), "hi")
            .sign_with_keys(&keys)
            .unwrap();
        let event_json = serde_json::to_value(&event).unwrap();

        let backend = spawn_host(move |rid, action| {
            succeeded(
                rid,
                action,
                serde_json::json!({ "messages": [event_json.clone()] }),
            )
        })
        .await;

        let page = backend
            .channel_read(ChannelReadArgs::channel(CHANNEL))
            .await
            .expect("page");

        assert_eq!(page.messages.len(), 1);
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn broker_failure_is_surfaced_as_an_error() {
        let backend = spawn_host(|rid, _action| {
            let body = serde_json::json!({
                "type": "broker_result",
                "protocolVersion": 1,
                "requestId": rid,
                "status": "failed",
                "error": { "code": "unauthorized", "message": "not permitted" },
            });
            (StatusCode::OK, body.to_string())
        })
        .await;

        let err = backend
            .message_post(MessagePostArgs {
                channel_id: CHANNEL.into(),
                content: "hello".into(),
                mentions: Vec::new(),
            })
            .await
            .expect_err("a failure");

        assert!(err.to_string().contains("unauthorized"));
    }
}
