use std::{net::SocketAddr, time::Duration};

use async_channel::Receiver;
use bevy::{prelude::*, tasks::IoTaskPool};
use nostr_sdk::{
    Client, Event, EventBuilder, Filter, Kind, PublicKey, RelayMessage, RelayPoolNotification, Tag,
    Timestamp,
};
use serde::{Deserialize, Serialize};

use crate::{relay_pool::RelayPool, NostrKeys};

pub const NOSTR_KIND_SERVER_ANNOUNCEMENT: u16 = 30078;
pub const SERVER_ANNOUNCEMENT_VERSION: u32 = 1;
#[cfg(debug_assertions)]
pub const SERVER_ANNOUNCEMENT_TTL_SECS: u64 = 10;
#[cfg(not(debug_assertions))]
pub const SERVER_ANNOUNCEMENT_TTL_SECS: u64 = 60;
pub const SERVER_ANNOUNCEMENT_REPUBLISH_SECS: u64 = SERVER_ANNOUNCEMENT_TTL_SECS / 2;
pub(crate) const SERVER_ANNOUNCEMENT_IDENTIFIER: &str = "untitled-brawler";
const SERVER_ANNOUNCEMENT_EXPIRED: &str = "server announcement expired";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerAnnouncement {
    pub server_addr: SocketAddr,
    pub cert_digest: String,
    pub display_name: String,
    pub version: u32,
}

#[derive(Clone, Debug)]
pub struct ServerListEntry {
    pub pubkey: PublicKey,
    pub addr: SocketAddr,
    pub cert_digest: String,
    pub display_name: String,
    pub received_at: Duration,
}

impl ServerListEntry {
    pub fn menu_label(&self) -> String {
        format!(
            "{}\n{}\nServer ID: {}",
            self.display_name,
            self.addr,
            short_public_key(&self.pubkey)
        )
    }
}

fn short_public_key(public: &PublicKey) -> String {
    let hex = public.to_string();
    if hex.len() <= 18 {
        return hex;
    }

    format!("{}...{}", &hex[..8], &hex[hex.len() - 6..])
}

#[derive(Resource, Default, Clone, Debug)]
pub struct ServerList {
    pub entries: Vec<ServerListEntry>,
}

#[derive(Resource)]
pub struct ServerAnnouncementRx(pub Receiver<ServerListEntry>);

#[derive(Resource, Default, Clone, Copy, Debug)]
pub struct ServerAnnouncementSubscriptionStarted(pub bool);

pub fn server_announcement_builder(
    announcement: &ServerAnnouncement,
) -> Result<EventBuilder, serde_json::Error> {
    let content = serde_json::to_string(announcement)?;
    let expiration = Timestamp::now() + Duration::from_secs(SERVER_ANNOUNCEMENT_TTL_SECS);
    Ok(
        EventBuilder::new(Kind::Custom(NOSTR_KIND_SERVER_ANNOUNCEMENT), content)
            .tag(Tag::identifier(SERVER_ANNOUNCEMENT_IDENTIFIER))
            .tag(Tag::expiration(expiration)),
    )
}

pub async fn publish_server_announcement(
    client: Client,
    identity: NostrKeys,
    announcement: ServerAnnouncement,
) -> Result<String, String> {
    let event = server_announcement_builder(&announcement)
        .map_err(|error| format!("serialize announcement: {error}"))?
        .sign_with_keys(identity.keys())
        .map_err(|error| format!("sign announcement: {error}"))?;
    let output = client
        .send_event(&event)
        .await
        .map_err(|error| format!("publish announcement: {error}"))?;
    if output.success.is_empty() {
        return Err(format!(
            "publish announcement: no relay accepted event {}; failures={:?}",
            event.id, output.failed
        ));
    }
    Ok(event.id.to_string())
}

fn server_announcement_filter() -> Filter {
    Filter::new()
        .kind(Kind::Custom(NOSTR_KIND_SERVER_ANNOUNCEMENT))
        .identifier(SERVER_ANNOUNCEMENT_IDENTIFIER)
}

pub fn parse_server_announcement_event(event: &Event) -> Result<ServerListEntry, String> {
    if event.kind != Kind::Custom(NOSTR_KIND_SERVER_ANNOUNCEMENT) {
        return Err(format!("unexpected announcement kind {}", event.kind));
    }
    if event.tags.identifier() != Some(SERVER_ANNOUNCEMENT_IDENTIFIER) {
        return Err("server announcement identifier tag mismatch".to_string());
    }
    if event
        .tags
        .expiration()
        .is_some_and(|expiration| *expiration <= Timestamp::now())
    {
        return Err(SERVER_ANNOUNCEMENT_EXPIRED.to_string());
    }

    let announcement: ServerAnnouncement = serde_json::from_str(&event.content)
        .map_err(|error| format!("invalid announcement JSON: {error}"))?;
    if announcement.version != SERVER_ANNOUNCEMENT_VERSION {
        return Err(format!(
            "unsupported announcement version {}",
            announcement.version,
        ));
    }

    Ok(ServerListEntry {
        pubkey: event.pubkey,
        addr: announcement.server_addr,
        cert_digest: announcement.cert_digest,
        display_name: announcement.display_name,
        received_at: Duration::ZERO,
    })
}

pub fn poll_server_announcements(
    time: Res<Time<Real>>,
    mut list: ResMut<ServerList>,
    rx: Option<Res<ServerAnnouncementRx>>,
) {
    let Some(rx) = rx else {
        trace!("poll_server_announcements: subscription receiver not ready");
        return;
    };

    while let Ok(mut entry) = rx.0.try_recv() {
        entry.received_at = time.elapsed();
        if let Some(existing) = list
            .entries
            .iter_mut()
            .find(|existing| existing.pubkey == entry.pubkey)
        {
            *existing = entry;
        } else {
            list.entries.push(entry);
        }
    }

    let now = time.elapsed();
    let ttl = Duration::from_secs(SERVER_ANNOUNCEMENT_TTL_SECS);
    let before_len = list.entries.len();
    list.entries
        .retain(|entry| now.saturating_sub(entry.received_at) <= ttl);
    let removed = before_len.saturating_sub(list.entries.len());
    if removed > 0 {
        trace!(removed, "pruned stale Nostr server announcements");
    }
}

pub fn spawn_server_announcement_subscription(
    mut commands: Commands,
    mut started: ResMut<ServerAnnouncementSubscriptionStarted>,
    pool: Option<Res<RelayPool>>,
) {
    if started.0 {
        trace!("spawn_server_announcement_subscription: already started");
        return;
    }

    let Some(pool) = pool else {
        trace!("spawn_server_announcement_subscription: RelayPool not ready yet");
        return;
    };

    started.0 = true;
    let (tx, rx) = async_channel::unbounded();
    commands.insert_resource(ServerAnnouncementRx(rx));

    let client = pool.client.clone();
    IoTaskPool::get()
        .spawn(async move {
            let mut notifications = client.notifications();
            let filter = server_announcement_filter();
            let subscription = client
                .subscribe(filter, None)
                .await
                .expect("server announcement subscription must start");
            let subscription_id = subscription.val;
            debug!(%subscription_id, "started Nostr server announcement subscription");

            loop {
                match notifications.recv().await {
                    Ok(RelayPoolNotification::Message {
                        message:
                            RelayMessage::Event {
                                subscription_id: id,
                                event,
                            },
                        ..
                    }) if id.as_ref() == &subscription_id => {
                        match parse_server_announcement_event(event.as_ref()) {
                            Ok(entry) => {
                                let _ = tx.send(entry).await;
                            }
                            Err(error) if error == SERVER_ANNOUNCEMENT_EXPIRED => {
                                trace!("ignored expired server announcement event")
                            }
                            Err(error) => {
                                trace!(%error, "ignored non-matching server announcement event")
                            }
                        }
                    }
                    Ok(RelayPoolNotification::Shutdown) => {
                        debug!("Nostr relay pool shut down server announcement subscription");
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        warn!(%error, "Nostr server announcement subscription ended");
                        break;
                    }
                }
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use nostr_sdk::{Keys, SecretKey};

    use super::*;

    fn announcement() -> ServerAnnouncement {
        ServerAnnouncement {
            server_addr: SocketAddr::from((IpAddr::V4(Ipv4Addr::LOCALHOST), 5001)),
            cert_digest: "F03BA2AB0904DA5DEA7D3F5952FADED758248DF23306F0EF59296B7D0C25A016"
                .to_string(),
            display_name: "Test Server".to_string(),
            version: SERVER_ANNOUNCEMENT_VERSION,
        }
    }

    fn signed_event(keys: &Keys, announcement: &ServerAnnouncement) -> Event {
        server_announcement_builder(announcement)
            .unwrap()
            .sign_with_keys(keys)
            .unwrap()
    }

    #[test]
    fn announcement_builder_signs_kind_content_and_identifier() {
        let keys = Keys::new(SecretKey::generate());
        let event = signed_event(&keys, &announcement());

        assert_eq!(
            event.kind,
            Kind::Custom(NOSTR_KIND_SERVER_ANNOUNCEMENT.into())
        );
        assert!(event
            .tags
            .identifier()
            .is_some_and(|value| value == SERVER_ANNOUNCEMENT_IDENTIFIER));

        let content: ServerAnnouncement = serde_json::from_str(&event.content).unwrap();
        assert_eq!(content.server_addr, announcement().server_addr);
        assert_eq!(content.cert_digest, announcement().cert_digest);
        assert_eq!(content.display_name, announcement().display_name);
        assert_eq!(content.version, SERVER_ANNOUNCEMENT_VERSION);
        let expiration = event
            .tags
            .expiration()
            .expect("announcement must include NIP-40 expiration tag");
        assert!(expiration.as_secs() > Timestamp::now().as_secs());
    }

    #[test]
    fn parse_server_announcement_event_extracts_entry() {
        let keys = Keys::new(SecretKey::generate());
        let event = signed_event(&keys, &announcement());

        let entry = parse_server_announcement_event(&event).unwrap();

        assert_eq!(entry.pubkey, keys.public_key());
        assert_eq!(entry.addr, announcement().server_addr);
        assert_eq!(entry.cert_digest, announcement().cert_digest);
        assert_eq!(entry.display_name, announcement().display_name);
    }

    #[test]
    fn menu_label_uses_short_server_id() {
        let keys = Keys::new(SecretKey::generate());
        let entry = parse_server_announcement_event(&signed_event(&keys, &announcement())).unwrap();
        let full_pubkey = keys.public_key().to_string();

        let label = entry.menu_label();

        assert!(label.contains("Test Server"));
        assert!(label.contains("127.0.0.1:5001"));
        assert!(label.contains("Server ID:"));
        assert!(!label.contains(&full_pubkey));
    }

    #[test]
    fn server_announcement_filter_selects_identifier() {
        let value = serde_json::to_value(server_announcement_filter()).unwrap();
        assert_eq!(
            value.get("#d").and_then(|value| value.as_array()),
            Some(&vec![serde_json::Value::String(
                SERVER_ANNOUNCEMENT_IDENTIFIER.to_string(),
            )])
        );
    }

    fn app_with_announcement_poller(rx: Receiver<ServerListEntry>) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<ServerList>()
            .insert_resource(ServerAnnouncementRx(rx))
            .add_systems(Update, poll_server_announcements);
        app
    }

    #[test]
    fn parse_server_announcement_event_rejects_identifier_mismatch() {
        let keys = Keys::new(SecretKey::generate());
        let content = serde_json::to_string(&announcement()).unwrap();
        let event = EventBuilder::new(Kind::Custom(NOSTR_KIND_SERVER_ANNOUNCEMENT.into()), content)
            .tag(Tag::identifier("other"))
            .sign_with_keys(&keys)
            .unwrap();

        let error = parse_server_announcement_event(&event).unwrap_err();

        assert!(error.contains("identifier tag mismatch"));
    }

    #[test]
    fn parse_server_announcement_event_rejects_version_mismatch() {
        let keys = Keys::new(SecretKey::generate());
        let mut announcement = announcement();
        announcement.version = SERVER_ANNOUNCEMENT_VERSION + 1;
        let event = signed_event(&keys, &announcement);

        let error = parse_server_announcement_event(&event).unwrap_err();

        assert!(error.contains("unsupported announcement version"));
    }

    #[test]
    fn parse_server_announcement_event_rejects_expired_events() {
        let keys = Keys::new(SecretKey::generate());
        let content = serde_json::to_string(&announcement()).unwrap();
        let event = EventBuilder::new(Kind::Custom(NOSTR_KIND_SERVER_ANNOUNCEMENT), content)
            .tag(Tag::identifier(SERVER_ANNOUNCEMENT_IDENTIFIER))
            .tag(Tag::expiration(
                Timestamp::now() - Duration::from_secs(SERVER_ANNOUNCEMENT_TTL_SECS + 1),
            ))
            .sign_with_keys(&keys)
            .unwrap();

        let error = parse_server_announcement_event(&event).unwrap_err();

        assert!(error.contains("server announcement expired"));
    }

    #[test]
    fn poll_server_announcements_replaces_existing_pubkey_entry() {
        let keys = Keys::new(SecretKey::generate());
        let original =
            parse_server_announcement_event(&signed_event(&keys, &announcement())).unwrap();
        let mut updated_announcement = announcement();
        updated_announcement.server_addr =
            SocketAddr::from((IpAddr::V4(Ipv4Addr::LOCALHOST), 5002));
        updated_announcement.display_name = "Updated Server".to_string();
        let updated =
            parse_server_announcement_event(&signed_event(&keys, &updated_announcement)).unwrap();
        let other_keys = Keys::new(SecretKey::generate());
        let other =
            parse_server_announcement_event(&signed_event(&other_keys, &announcement())).unwrap();
        let (tx, rx) = async_channel::unbounded();
        tx.try_send(original).unwrap();
        tx.try_send(other).unwrap();
        tx.try_send(updated).unwrap();

        let mut app = app_with_announcement_poller(rx);
        app.update();

        let list = app.world().resource::<ServerList>();
        assert_eq!(list.entries.len(), 2);
        let entry = list
            .entries
            .iter()
            .find(|entry| entry.pubkey == keys.public_key())
            .unwrap();
        assert_eq!(entry.addr, updated_announcement.server_addr);
        assert_eq!(entry.display_name, updated_announcement.display_name);
    }
    #[test]
    fn poll_server_announcements_prunes_stale_entries() {
        let keys = Keys::new(SecretKey::generate());
        let mut stale =
            parse_server_announcement_event(&signed_event(&keys, &announcement())).unwrap();
        stale.received_at = Duration::ZERO;
        let (_tx, rx) = async_channel::unbounded();
        let mut app = app_with_announcement_poller(rx);
        app.world_mut()
            .resource_mut::<ServerList>()
            .entries
            .push(stale);
        app.world_mut()
            .resource_mut::<Time<Real>>()
            .advance_by(Duration::from_secs(SERVER_ANNOUNCEMENT_TTL_SECS + 1));
        app.update();

        let list = app.world().resource::<ServerList>();
        assert!(list.entries.is_empty());
    }
}
