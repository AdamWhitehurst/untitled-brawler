use bevy::prelude::*;
use lightyear::prelude::client::*;
use lightyear::prelude::*;
use protocol::{PRIVATE_KEY, PROTOCOL_ID};
use std::net::SocketAddr;

#[derive(Clone, Resource)]
pub struct ClientNetworkConfig {
    pub client_addr: SocketAddr,
    pub server_addr: SocketAddr,
    /// Lightyear netcode client id. Set from the logged-in Nostr public key before connecting.
    pub client_id: u64,
    pub protocol_id: u64,
    pub private_key: [u8; 32],
    pub certificate_digest: String,
    pub token_expire_secs: i32,
}

impl Default for ClientNetworkConfig {
    fn default() -> Self {
        Self {
            client_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            server_addr: SocketAddr::from(([127, 0, 0, 1], 5001)),
            client_id: 0,
            protocol_id: PROTOCOL_ID,
            private_key: PRIVATE_KEY,
            certificate_digest: String::new(),
            token_expire_secs: 30,
        }
    }
}

#[derive(Default)]
pub struct ClientNetworkPlugin {
    pub config: ClientNetworkConfig,
}


impl Plugin for ClientNetworkPlugin {
    fn build(&self, app: &mut App) {
        let config = self.config.clone();
        app.insert_resource(config.clone());
        app.add_systems(Startup, move |commands: Commands| {
            spawn_client_entity(commands, config.clone());
        });
        app.add_observer(on_connected);
        app.add_observer(on_disconnected);
    }
}

fn spawn_client_entity(mut commands: Commands, config: ClientNetworkConfig) {
    let netcode_client = crate::netcode::build_netcode_client(&config);
    let webtransport_io = crate::webtransport::build_io(&config);

    commands.spawn((
        Name::new("Client"),
        Client::default(),
        LocalAddr(config.client_addr),
        PeerAddr(config.server_addr),
        Link::new(None),
        ReplicationReceiver::default(),
        PredictionManager::default(),
        netcode_client,
        webtransport_io,
    ));
}

fn on_connected(trigger: On<Add, Connected>) {
    info!("Client {:?} connected!", trigger.entity);
}

fn on_disconnected(trigger: On<Add, Disconnected>) {
    info!("Client {:?} disconnected!", trigger.entity);
}
