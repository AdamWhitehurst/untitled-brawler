pub mod auth;
pub mod gameplay;
pub mod input;
pub mod map;
pub mod map_publication;
pub mod persistence;
pub mod transition;
pub mod world_object;

pub use client_lightyear::ClientNetworkConfig;
pub use lightyear::netcode::{Key, NetcodeClient};
pub use lightyear::prelude::client::NetcodeConfig;
pub use lightyear::prelude::Authentication;
pub use map::{current_placement_target, PlacementTarget};
