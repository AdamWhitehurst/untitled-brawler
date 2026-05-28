use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::{
    Event, EventBuilder, Filter, JsonUtil, Keys, Kind, PublicKey, SecretKey, SingleLetterTag, Tag,
    TagKind,
};
use protocol::NostrPublicKey;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Nostr event kind used by generic event helpers.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum NostrEventKind {
    Custom(u16),
}

impl From<NostrEventKind> for Kind {
    fn from(value: NostrEventKind) -> Self {
        match value {
            NostrEventKind::Custom(kind) => Kind::Custom(kind),
        }
    }
}

impl From<Kind> for NostrEventKind {
    fn from(value: Kind) -> Self {
        match value {
            Kind::Custom(kind) => NostrEventKind::Custom(kind),
            other => NostrEventKind::Custom(other.as_u16()),
        }
    }
}

/// Single Nostr tag name/value pair used by map-agnostic verification helpers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct NostrTag {
    pub name: String,
    pub value: String,
}

impl NostrTag {
    /// Creates a tag with a single value.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// Unsigned Nostr event draft for tests and map-specific signing callers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NostrEventDraft {
    pub kind: NostrEventKind,
    pub content: String,
    pub tags: Vec<NostrTag>,
}

impl NostrEventDraft {
    /// Signs this draft with a Nostr secret key and returns raw event JSON.
    pub fn sign_with_secret(&self, secret: SecretKey) -> Result<String, NostrEventError> {
        let keys = Keys::new(secret);
        self.sign_with_keys(&keys)
    }

    /// Signs this draft with existing Nostr keys and returns raw event JSON.
    pub fn sign_with_keys(&self, keys: &Keys) -> Result<String, NostrEventError> {
        let mut builder = EventBuilder::new(Kind::from(self.kind), self.content.clone());
        for tag in &self.tags {
            builder = builder.tag(Tag::custom(
                TagKind::custom(tag.name.clone()),
                [tag.value.clone()],
            ));
        }
        let event = builder
            .sign_with_keys(keys)
            .map_err(|error| NostrEventError::Invalid(error.to_string()))?;
        Ok(event.as_json())
    }
}

/// Verified event data exposed without leaking nostr-sdk types into map crates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedNostrEvent {
    pub kind: NostrEventKind,
    pub pubkey: NostrPublicKey,
    pub content: String,
    pub tags: Vec<NostrTag>,
    pub raw_json: String,
}

/// Generic event query shape used by map-specific stores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NostrEventQuery {
    pub author: Option<NostrPublicKey>,
    pub kind: Option<NostrEventKind>,
    pub tags: Vec<(String, String)>,
    pub limit: usize,
    pub timeout: Duration,
}

impl Default for NostrEventQuery {
    fn default() -> Self {
        Self {
            author: None,
            kind: None,
            tags: Vec::new(),
            limit: 100,
            timeout: Duration::from_secs(5),
        }
    }
}

impl NostrEventQuery {
    /// Creates a default event query.
    pub fn new() -> Self {
        Self::default()
    }

    /// Restricts the query to an event author.
    pub fn author(mut self, author: NostrPublicKey) -> Self {
        self.author = Some(author);
        self
    }

    /// Restricts the query to an event kind.
    pub fn kind(mut self, kind: NostrEventKind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// Restricts the query to a tag value.
    pub fn tag(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.tags.push((name.into(), value.into()));
        self
    }

    /// Sets the result limit.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Sets the relay query timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Query-capable generic Nostr event client.
#[derive(Clone)]
pub struct NostrEventClient {
    source: Arc<NostrEventSource>,
}

enum NostrEventSource {
    Sdk(nostr_sdk::Client),
    #[cfg(any(test, feature = "test-fixtures"))]
    Static(Vec<String>),
}

impl NostrEventClient {
    /// Wraps a nostr-sdk client.
    pub fn from_client(client: nostr_sdk::Client) -> Self {
        Self {
            source: Arc::new(NostrEventSource::Sdk(client)),
        }
    }

    /// Creates an in-memory event client for deterministic tests.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn from_events(events: Vec<String>) -> Self {
        Self {
            source: Arc::new(NostrEventSource::Static(events)),
        }
    }

    /// Queries events and returns raw event JSON strings.
    pub async fn query(&self, query: NostrEventQuery) -> Result<Vec<String>, NostrEventError> {
        match self.source.as_ref() {
            NostrEventSource::Sdk(client) => query_sdk(client, query).await,
            #[cfg(any(test, feature = "test-fixtures"))]
            NostrEventSource::Static(events) => query_static(events, &query),
        }
    }
}

impl From<nostr_sdk::Client> for NostrEventClient {
    fn from(client: nostr_sdk::Client) -> Self {
        Self::from_client(client)
    }
}

/// Generic event verification failures.
#[derive(Debug, Error)]
pub enum NostrEventError {
    #[error("invalid Nostr event: {0}")]
    Invalid(String),
    #[error("Nostr query failed: {0}")]
    Query(String),
}

/// Publishes a signed event through a relay-backed client.
pub async fn publish_event(
    client: &NostrEventClient,
    event_json: String,
) -> Result<(), NostrEventError> {
    let event = Event::from_json(event_json)
        .map_err(|error| NostrEventError::Invalid(error.to_string()))?;
    event
        .verify()
        .map_err(|error| NostrEventError::Invalid(error.to_string()))?;
    match client.source.as_ref() {
        NostrEventSource::Sdk(client) => crate::compat::await_network(client.send_event(&event))
            .await
            .map(|_| ())
            .map_err(|error| NostrEventError::Query(error.to_string())),
        #[cfg(any(test, feature = "test-fixtures"))]
        NostrEventSource::Static(_) => Err(NostrEventError::Query(
            "static NostrEventClient cannot publish events".to_string(),
        )),
    }
}

/// Verifies raw Nostr event JSON and exposes generic event data.
pub fn verify_event_json(event_json: &str) -> Result<VerifiedNostrEvent, NostrEventError> {
    let event = Event::from_json(event_json)
        .map_err(|error| NostrEventError::Invalid(error.to_string()))?;
    event
        .verify()
        .map_err(|error| NostrEventError::Invalid(error.to_string()))?;
    Ok(verified_from_event(&event, event_json.to_string()))
}

fn verified_from_event(event: &Event, raw_json: String) -> VerifiedNostrEvent {
    let tags = event
        .tags
        .iter()
        .filter_map(|tag| {
            let fields = tag.as_slice();
            Some(NostrTag::new(fields.first()?, fields.get(1)?))
        })
        .collect();
    VerifiedNostrEvent {
        kind: event.kind.into(),
        pubkey: NostrPublicKey(*event.pubkey.as_bytes()),
        content: event.content.clone(),
        tags,
        raw_json,
    }
}

async fn query_sdk(
    client: &nostr_sdk::Client,
    query: NostrEventQuery,
) -> Result<Vec<String>, NostrEventError> {
    let mut filter = Filter::new();
    if let Some(author) = query.author {
        let public_key = PublicKey::from_byte_array(author.0);
        filter = filter.author(public_key);
    }
    if let Some(kind) = query.kind {
        filter = filter.kind(Kind::from(kind));
    }
    if query.limit > 0 {
        filter = filter.limit(query.limit);
    }
    for (name, value) in query.tags {
        let mut chars = name.chars();
        let Some(tag_char) = chars.next() else {
            return Err(NostrEventError::Invalid(
                "empty tag name in query".to_string(),
            ));
        };
        if chars.next().is_some() {
            return Err(NostrEventError::Invalid(format!(
                "nostr filter supports single-letter indexed tags, got {name}"
            )));
        }
        let tag = SingleLetterTag::from_char(tag_char)
            .map_err(|error| NostrEventError::Invalid(error.to_string()))?;
        filter = filter.custom_tag(tag, value);
    }
    let events = crate::compat::await_network(client.fetch_events(filter, query.timeout))
        .await
        .map_err(|error| NostrEventError::Query(error.to_string()))?;
    Ok(events.into_iter().map(|event| event.as_json()).collect())
}

#[cfg(any(test, feature = "test-fixtures"))]
fn query_static(
    events: &[String],
    query: &NostrEventQuery,
) -> Result<Vec<String>, NostrEventError> {
    let mut matches = Vec::new();
    for event_json in events {
        let verified = verify_event_json(event_json)?;
        if let Some(author) = query.author {
            if verified.pubkey != author {
                continue;
            }
        }
        if let Some(kind) = query.kind {
            if verified.kind != kind {
                continue;
            }
        }
        if query.tags.iter().any(|(name, value)| {
            !verified
                .tags
                .iter()
                .any(|tag| tag.name == *name && tag.value == *value)
        }) {
            continue;
        }
        matches.push(event_json.clone());
        if query.limit > 0 && matches.len() >= query.limit {
            break;
        }
    }
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_event() -> (String, NostrPublicKey) {
        let secret = SecretKey::generate();
        let keys = Keys::new(secret.clone());
        let owner = NostrPublicKey(*keys.public_key().as_bytes());
        let draft = NostrEventDraft {
            kind: NostrEventKind::Custom(30079),
            content: "hello".to_string(),
            tags: vec![NostrTag::new("m", "map"), NostrTag::new("x", "hash")],
        };
        (draft.sign_with_secret(secret).unwrap(), owner)
    }

    #[test]
    fn events_verify_signed_event_json() {
        let (event_json, owner) = signed_event();
        let event = verify_event_json(&event_json).expect("valid event");
        assert_eq!(event.pubkey, owner);
        assert_eq!(event.kind, NostrEventKind::Custom(30079));
        assert_eq!(event.content, "hello");
        assert!(event.tags.contains(&NostrTag::new("m", "map")));
    }

    #[test]
    fn events_verify_rejects_tampered_signature() {
        let (event_json, _) = signed_event();
        let mut value: serde_json::Value = serde_json::from_str(&event_json).unwrap();
        value["content"] = serde_json::Value::String("goodbye".to_string());
        let tampered = serde_json::to_string(&value).unwrap();
        assert!(matches!(
            verify_event_json(&tampered),
            Err(NostrEventError::Invalid(_))
        ));
    }

    #[test]
    fn events_static_client_filters_by_author_kind_and_tag() {
        let (event_json, owner) = signed_event();
        let client = NostrEventClient::from_events(vec![event_json.clone()]);
        let events = bevy::tasks::block_on(
            client.query(
                NostrEventQuery::new()
                    .author(owner)
                    .kind(NostrEventKind::Custom(30079))
                    .tag("m", "map"),
            ),
        )
        .expect("static query");
        assert_eq!(events, vec![event_json]);
    }
}
