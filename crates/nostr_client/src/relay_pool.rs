use async_channel::Receiver;
use bevy::prelude::*;
use bevy::tasks::IoTaskPool;
use nostr_sdk::{Client, Filter, Kind, RelayMessage, RelayPoolNotification};
use protocol::RelayPoolReady;

use crate::{
    announcement::{NOSTR_KIND_SERVER_ANNOUNCEMENT, SERVER_ANNOUNCEMENT_IDENTIFIER},
    plugin::NostrClientConfig,
};

#[derive(Resource, Clone)]
pub struct RelayPool {
    pub client: Client,
    pub ready_rx: Receiver<()>,
}

impl RelayPool {
    /// Returns a generic event-query client backed by this relay pool.
    pub fn event_client(&self) -> crate::events::NostrEventClient {
        crate::events::NostrEventClient::from_client(self.client.clone())
    }
}

pub fn relay_pool_ready(pool: Res<RelayPoolReady>) -> bool {
    pool.0
}

pub fn spawn_relay_pool(mut commands: Commands, config: Res<NostrClientConfig>) {
    let (ready_tx, ready_rx) = async_channel::bounded(1);
    let client = nostr_sdk::Client::default();

    commands.insert_resource(RelayPool {
        client: client.clone(),
        ready_rx,
    });

    let relays = config.relays.clone();
    IoTaskPool::get()
        .spawn(async move {
            debug!(
                relay_count = relays.len(),
                "starting Nostr relay setup task"
            );

            for relay in relays {
                match client.add_relay(relay.clone()).await {
                    Ok(_) => debug!(%relay, "added Nostr relay"),
                    Err(error) => warn!(%relay, %error, "failed to add Nostr relay"),
                }
            }

            let mut notifications = client.notifications();
            client.connect().await;

            let filter = Filter::new()
                .kind(Kind::Custom(NOSTR_KIND_SERVER_ANNOUNCEMENT))
                .identifier(SERVER_ANNOUNCEMENT_IDENTIFIER)
                .limit(1);
            let subscription = match client.subscribe(filter, None).await {
                Ok(subscription) => subscription,
                Err(error) => {
                    warn!(%error, "failed to start Nostr readiness subscription");
                    return;
                }
            };
            let subscription_id = subscription.val;
            debug!(%subscription_id, "started Nostr readiness subscription");

            loop {
                match notifications.recv().await {
                    Ok(RelayPoolNotification::Message {
                        relay_url,
                        message: RelayMessage::EndOfStoredEvents(id),
                    }) if id.as_ref() == &subscription_id => {
                        debug!(%relay_url, %subscription_id, "received Nostr readiness EOSE");
                        let _ = ready_tx.send(()).await;
                        break;
                    }
                    Ok(RelayPoolNotification::Shutdown) => {
                        debug!("Nostr relay pool shut down before readiness EOSE");
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        warn!(%error, "Nostr relay notification listener ended before readiness");
                        break;
                    }
                }
            }
        })
        .detach();
}

pub fn poll_relay_pool_ready(mut ready: ResMut<RelayPoolReady>, pool: Option<Res<RelayPool>>) {
    let Some(pool) = pool else {
        trace!("poll_relay_pool_ready: RelayPool not inserted yet");
        return;
    };

    while pool.ready_rx.try_recv().is_ok() {
        if !ready.0 {
            info!("Nostr relay pool reached EOSE on at least one relay");
        }
        ready.0 = true;
    }
}

pub fn shutdown_relay_pool(mut exit: MessageReader<AppExit>, pool: Option<Res<RelayPool>>) {
    if exit.read().next().is_none() {
        return;
    }

    let Some(pool) = pool else {
        trace!("shutdown_relay_pool: RelayPool absent, nothing to shut down");
        return;
    };

    let client = pool.client.clone();
    IoTaskPool::get()
        .spawn(async move {
            client.shutdown().await;
        })
        .detach();
}
