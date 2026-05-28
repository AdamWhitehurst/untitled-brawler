use bevy::prelude::*;
use bevy::tasks::IoTaskPool;
use nostr_client::{
    NostrKeys, RelayPool, ServerAnnouncement, SERVER_ANNOUNCEMENT_REPUBLISH_SECS,
    SERVER_ANNOUNCEMENT_VERSION,
};
use server_lightyear::ServerNetworkConfig;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub struct ServerAnnouncementPlugin;

impl Plugin for ServerAnnouncementPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            OnEnter(protocol::AppState::Ready),
            publish_announcement_on_ready,
        )
        .add_systems(
            Update,
            publish_announcement_periodically.run_if(in_state(protocol::AppState::Ready)),
        );
    }
}

fn publish_announcement_on_ready(
    pool: Res<RelayPool>,
    identity: Res<NostrKeys>,
    network: Res<ServerNetworkConfig>,
) {
    publish_announcement(&pool, &identity, &network);
}

fn publish_announcement_periodically(
    time: Res<Time>,
    mut last_publish: Local<Option<f64>>,
    pool: Res<RelayPool>,
    identity: Res<NostrKeys>,
    network: Res<ServerNetworkConfig>,
) {
    let now = time.elapsed_secs_f64();
    let Some(last) = *last_publish else {
        *last_publish = Some(now);
        return;
    };

    if now - last < SERVER_ANNOUNCEMENT_REPUBLISH_SECS as f64 {
        return;
    }

    *last_publish = Some(now);
    publish_announcement(&pool, &identity, &network);
}

fn publish_announcement(pool: &RelayPool, identity: &NostrKeys, network: &ServerNetworkConfig) {
    let client = pool.client.clone();
    let identity = identity.clone();
    let announcement = ServerAnnouncement {
        server_addr: announced_server_addr(network),
        cert_digest: load_cert_digest(),
        display_name: "Untitled Brawler Server".to_string(),
        version: SERVER_ANNOUNCEMENT_VERSION,
    };

    IoTaskPool::get()
        .spawn(async move {
            match nostr_client::announcement::publish_server_announcement(
                client,
                identity,
                announcement,
            )
            .await
            {
                Ok(event_id) => trace!(%event_id, "published Nostr server announcement"),
                Err(error) => panic!("failed to publish Nostr server announcement: {error}"),
            }
        })
        .detach();
}

fn load_cert_digest() -> String {
    include_str!("../../../certificates/digest.txt")
        .trim()
        .to_string()
}

fn announced_server_addr(network: &ServerNetworkConfig) -> SocketAddr {
    SocketAddr::from((announced_ip(network.bind_addr), network.port))
}

fn announced_ip(bind_addr: IpAddr) -> IpAddr {
    match bind_addr {
        IpAddr::V4(addr) if addr.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(addr) if addr.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        addr => addr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announced_server_addr_replaces_unspecified_ipv4_with_loopback() {
        let network = ServerNetworkConfig {
            bind_addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 5001,
            ..Default::default()
        };

        assert_eq!(
            announced_server_addr(&network),
            SocketAddr::from((Ipv4Addr::LOCALHOST, 5001))
        );
    }

    #[test]
    fn announced_server_addr_replaces_unspecified_ipv6_with_loopback() {
        let network = ServerNetworkConfig {
            bind_addr: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            port: 5001,
            ..Default::default()
        };

        assert_eq!(
            announced_server_addr(&network),
            SocketAddr::from((Ipv6Addr::LOCALHOST, 5001))
        );
    }

    #[test]
    fn announced_server_addr_preserves_specific_bind_address() {
        let network = ServerNetworkConfig {
            bind_addr: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
            port: 5001,
            ..Default::default()
        };

        assert_eq!(
            announced_server_addr(&network),
            SocketAddr::from((Ipv4Addr::new(192, 0, 2, 10), 5001))
        );
    }
}
