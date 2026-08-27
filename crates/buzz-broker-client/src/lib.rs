//! Shared HTTP transport for the agent broker.
//!
//! Implements [`buzz_sdk::broker::BrokerClient`], the transport primitive the
//! contract crate deliberately omits: frozen request bytes out, one envelope
//! back. Callers use [`buzz_sdk::broker::BrokerClientExt::execute`], which adds
//! correlation checks; this type never interprets a verdict.

use buzz_sdk::broker::{
    BrokerClient, BrokerFuture, BrokerResponse, BrokerTransportError, Dispatch, PreparedRequest,
    BROKER_ACTION_PATH, BROKER_CREDENTIAL_HEADER,
};

/// A broker host endpoint plus the agent's bearer credential.
///
/// Holds no key and knows nothing of the relay: its whole authority is the
/// opaque credential it replays on every request.
pub struct HttpBrokerClient {
    base_url: String,
    credential: String,
    http: reqwest::Client,
}

impl HttpBrokerClient {
    /// A client posting to `base_url` with `credential` as its bearer token.
    pub fn new(base_url: impl Into<String>, credential: impl Into<String>) -> Self {
        Self::with_client(base_url, credential, reqwest::Client::new())
    }

    /// As [`Self::new`], reusing an existing reqwest client and its pool.
    pub fn with_client(
        base_url: impl Into<String>,
        credential: impl Into<String>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            credential: credential.into(),
            http,
        }
    }
}

impl BrokerClient for HttpBrokerClient {
    fn send<'a>(&'a self, request: &'a PreparedRequest, _dispatch: Dispatch) -> BrokerFuture<'a> {
        Box::pin(async move {
            let url = format!(
                "{}{BROKER_ACTION_PATH}",
                self.base_url.trim_end_matches('/')
            );
            let response = self
                .http
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(
                    BROKER_CREDENTIAL_HEADER,
                    format!("Bearer {}", self.credential),
                )
                .body(request.body().to_vec())
                .send()
                .await
                .map_err(|e| BrokerTransportError::Unreachable(e.to_string()))?;

            let status = response.status().as_u16();
            let body = response
                .bytes()
                .await
                .map_err(|e| BrokerTransportError::Unreachable(e.to_string()))?;

            // Parse an envelope whatever the status. Only its absence makes the
            // status meaningful, and then only as operator detail.
            serde_json::from_slice::<BrokerResponse>(&body).map_err(|e| {
                BrokerTransportError::NoEnvelope {
                    status,
                    detail: e.to_string(),
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, Bytes};
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::Response;
    use axum::routing::post;
    use axum::Router;
    use buzz_sdk::broker::{
        ActionArgs, BrokerClientExt, BrokerRequest, BrokerResult, MessagePostArgs,
    };
    use tokio::net::TcpListener;

    const CHANNEL: &str = "5df7dfa8-e919-43df-8efd-f1dcb8af7071";
    const CRED: &str = "test-cred";

    /// Spawn a broker that answers `/v1/action` with a fixed `(status, body)`
    /// and records the `Authorization` header it saw.
    async fn spawn(status: StatusCode, body: String) -> (String, Arc<Mutex<Option<String>>>) {
        let seen_auth = Arc::new(Mutex::new(None));
        type S = (Arc<(StatusCode, String)>, Arc<Mutex<Option<String>>>);
        let state: S = (Arc::new((status, body)), seen_auth.clone());

        let app = Router::new()
            .route(
                BROKER_ACTION_PATH,
                post(
                    |State((canned, seen)): State<S>, headers: HeaderMap, _body: Bytes| async move {
                        *seen.lock().unwrap() = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned);
                        Response::builder()
                            .status(canned.0)
                            .header("content-type", "application/json")
                            .body(Body::from(canned.1.clone()))
                            .unwrap()
                    },
                ),
            )
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen_auth)
    }

    fn post_request() -> PreparedRequest {
        let args = ActionArgs::MessagePost(MessagePostArgs {
            channel_id: CHANNEL.to_string(),
            content: "hi".to_string(),
            mentions: Vec::new(),
        });
        BrokerRequest::new("req-post-1", args)
            .unwrap()
            .prepare()
            .unwrap()
    }

    #[tokio::test]
    async fn success_round_trips_and_sends_bearer_credential() {
        let req = post_request();
        let body = format!(
            r#"{{"type":"broker_result","protocolVersion":1,"requestId":"{}","status":"succeeded","action":"message.post","outcome":{{"eventId":"{}","kind":9,"createdAt":1700000000}}}}"#,
            req.request_id(),
            "a".repeat(64),
        );
        let (base, seen_auth) = spawn(StatusCode::OK, body).await;

        let client = HttpBrokerClient::new(base, CRED);
        let validated = client.execute(&req).await.expect("a verdict");

        assert!(matches!(validated.result(), BrokerResult::Succeeded { .. }));
        assert_eq!(
            seen_auth.lock().unwrap().as_deref(),
            Some("Bearer test-cred")
        );
    }

    #[tokio::test]
    async fn rejected_credential_is_a_verdict_not_a_transport_error() {
        let req = post_request();
        let body = format!(
            r#"{{"type":"broker_result","protocolVersion":1,"requestId":"{}","status":"failed","error":{{"code":"unauthenticated","message":"nope"}}}}"#,
            req.request_id(),
        );
        let (base, _) = spawn(StatusCode::OK, body).await;

        let client = HttpBrokerClient::new(base, CRED);
        let validated = client.execute(&req).await.expect("a verdict");

        assert!(matches!(validated.result(), BrokerResult::Failed { .. }));
    }

    #[tokio::test]
    async fn non_envelope_response_is_a_transport_error() {
        let req = post_request();
        let (base, _) = spawn(StatusCode::BAD_GATEWAY, "upstream boom".to_string()).await;

        let client = HttpBrokerClient::new(base, CRED);
        let err = client.execute(&req).await.expect_err("no envelope");

        assert!(matches!(
            err,
            BrokerTransportError::NoEnvelope { status: 502, .. }
        ));
    }

    #[tokio::test]
    async fn unreachable_host_is_a_transport_error() {
        let req = post_request();
        let client = HttpBrokerClient::new("http://127.0.0.1:1", CRED);
        let err = client.execute(&req).await.expect_err("unreachable");

        assert!(matches!(err, BrokerTransportError::Unreachable(_)));
    }
}
