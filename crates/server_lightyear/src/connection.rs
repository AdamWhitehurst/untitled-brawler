use bevy::prelude::*;
use lightyear::prelude::server::*;
use lightyear::prelude::*;
use protocol::{PRIVATE_KEY, PROTOCOL_ID};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

const REPLICATION_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Resource)]
pub struct ServerNetworkConfig {
    pub bind_addr: IpAddr,
    pub port: u16,
    pub protocol_id: u64,
    pub private_key: [u8; 32],
    pub cert_pem_path: PathBuf,
    pub key_pem_path: PathBuf,
    pub replication_interval: Duration,
}

impl Default for ServerNetworkConfig {
    fn default() -> Self {
        Self {
            bind_addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 5001,
            protocol_id: PROTOCOL_ID,
            private_key: PRIVATE_KEY,
            cert_pem_path: PathBuf::new(),
            key_pem_path: PathBuf::new(),
            replication_interval: REPLICATION_INTERVAL,
        }
    }
}

#[derive(Default)]
pub struct ServerNetworkPlugin {
    pub config: ServerNetworkConfig,
}

impl Plugin for ServerNetworkPlugin {
    fn build(&self, app: &mut App) {
        let config = self.config.clone();
        app.insert_resource(config.clone());
        app.register_required_components_with::<ClientOf, ReplicationSender>(|| {
            ReplicationSender::new(REPLICATION_INTERVAL, SendUpdatesMode::SinceLastAck, false)
        });
        app.add_systems(Startup, move |commands: Commands| {
            start_server(commands, config.clone());
        });
    }
}

fn start_server(mut commands: Commands, config: ServerNetworkConfig) {
    let netcode = crate::netcode::build_netcode_server(&config);
    let webtransport_io = crate::webtransport::build_io(&config);

    let server = commands
        .spawn((
            Name::new("WebTransport Server"),
            Server::default(),
            netcode,
            LocalAddr(SocketAddr::from((config.bind_addr, config.port))),
            webtransport_io,
        ))
        .id();
    commands.trigger(Start { entity: server });
    info!(
        "WebTransport server listening on {}:{}",
        config.bind_addr, config.port
    );
}
