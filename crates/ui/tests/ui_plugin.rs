use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use lightyear::prelude::client::*;
use lightyear::prelude::{Controlled, PeerId, Predicted, RemoteId};
use protocol::transition::ClientTransitionState;
use protocol::*;
use std::time::Duration;
use ui::*;

fn add_ui_test_plugin(app: &mut App) {
    app.add_plugins(MinimalPlugins);
    app.add_plugins(StatesPlugin);
    app.add_plugins(bevy::input::InputPlugin);
    app.add_plugins(bevy::picking::InteractionPlugin);
    app.add_plugins(bevy::picking::PickingPlugin);
    app.add_message::<bevy::asset::AssetEvent<bevy::text::Font>>();
    app.init_resource::<Assets<bevy::image::Image>>();
    app.init_resource::<Assets<bevy::text::Font>>();
    app.init_resource::<Assets<bevy::image::TextureAtlasLayout>>();
    app.init_state::<AppState>();
    app.init_resource::<ClientTransitionState>();
    app.add_plugins(UiPlugin);
}

fn enter_app_ready(app: &mut App) {
    app.update();
    app.world_mut()
        .resource_mut::<NextState<AppState>>()
        .set(AppState::Ready);
    app.update();
    app.update();
}

fn enter_main_menu(app: &mut App) {
    enter_app_ready(app);
    app.world_mut()
        .resource_mut::<NextState<ClientState>>()
        .set(ClientState::MainMenu);
    app.update();
}

#[test]
fn test_ui_plugin_initializes_state() {
    let mut app = App::new();
    add_ui_test_plugin(&mut app);

    app.update();

    // Verify state is initialized
    let state = app.world().resource::<State<ClientState>>();
    assert_eq!(*state.get(), ClientState::Loading);
}

#[test]
fn app_ready_transitions_loading_to_login() {
    let mut app = App::new();
    add_ui_test_plugin(&mut app);

    enter_app_ready(&mut app);

    let state = app.world().resource::<State<ClientState>>();
    assert_eq!(*state.get(), ClientState::Login);
}

#[test]
fn pre_ready_connection_does_not_enter_ingame() {
    let mut app = App::new();
    add_ui_test_plugin(&mut app);

    let client = app
        .world_mut()
        .spawn((Name::new("Test Client"), Client::default()))
        .id();
    app.update();

    app.world_mut()
        .entity_mut(client)
        .insert((Connected, RemoteId(PeerId::Netcode(0))));
    app.update();

    let state = app.world().resource::<State<ClientState>>();
    assert_eq!(*state.get(), ClientState::Loading);
}

#[test]
fn test_main_menu_spawns_buttons() {
    let mut app = App::new();
    add_ui_test_plugin(&mut app);
    enter_main_menu(&mut app);

    app.update();

    // Verify server list container exists
    let mut server_list_query = app
        .world_mut()
        .query_filtered::<Entity, With<ServerListContainer>>();
    assert_eq!(
        server_list_query.iter(app.world()).count(),
        1,
        "Should have one server list container"
    );

    // Verify Quit button exists
    let mut quit_query = app.world_mut().query_filtered::<Entity, With<QuitButton>>();
    assert_eq!(
        quit_query.iter(app.world()).count(),
        1,
        "Should have one Quit button"
    );
}

#[test]
fn test_ingame_state_spawns_hud() {
    let mut app = App::new();
    add_ui_test_plugin(&mut app);

    // Setup dummy client entity (needed for button interactions)
    app.world_mut()
        .spawn((Name::new("Test Client"), Client::default()));

    // Transition to InGame state
    app.world_mut()
        .resource_mut::<NextState<ClientState>>()
        .set(ClientState::InGame);
    app.update();

    // Verify Main Menu button exists
    let mut main_menu_query = app
        .world_mut()
        .query_filtered::<Entity, With<MainMenuButton>>();
    assert_eq!(
        main_menu_query.iter(app.world()).count(),
        1,
        "Should have one Main Menu button"
    );

    // Verify Quit button exists
    let mut quit_query = app.world_mut().query_filtered::<Entity, With<QuitButton>>();
    assert_eq!(
        quit_query.iter(app.world()).count(),
        1,
        "Should have one Quit button"
    );
}

#[test]
fn test_disconnection_returns_to_main_menu() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(StatesPlugin);
    app.add_plugins(bevy::input::InputPlugin);
    app.add_plugins(bevy::picking::InteractionPlugin);
    app.add_plugins(bevy::picking::PickingPlugin);
    app.add_message::<bevy::asset::AssetEvent<bevy::text::Font>>();
    app.init_resource::<Assets<bevy::image::Image>>();
    app.init_resource::<Assets<bevy::text::Font>>();
    app.init_resource::<Assets<bevy::image::TextureAtlasLayout>>();
    app.add_plugins(ClientPlugins {
        tick_duration: Duration::from_secs_f64(1.0 / FIXED_TIMESTEP_HZ),
    });
    app.add_plugins(ProtocolPlugin);
    app.init_state::<AppState>();
    app.init_resource::<ClientTransitionState>();
    app.add_plugins(UiPlugin);
    app.insert_resource(
        nostr_client::generate_encrypted_identity("passphrase")
            .unwrap()
            .0,
    );

    // Setup client entity
    app.world_mut()
        .spawn((Name::new("Client"), Client::default()));

    // Set to InGame state
    app.world_mut()
        .resource_mut::<NextState<ClientState>>()
        .set(ClientState::InGame);
    app.update();

    // Verify in InGame state
    let state = app.world().resource::<State<ClientState>>();
    assert_eq!(*state.get(), ClientState::InGame);

    // Trigger disconnection
    let client_entity = {
        let mut query = app.world_mut().query_filtered::<Entity, With<Client>>();
        query.single(app.world()).unwrap()
    };
    app.world_mut()
        .entity_mut(client_entity)
        .insert(Disconnected::default());
    app.update();

    // Verify returned to MainMenu
    let state = app.world().resource::<State<ClientState>>();
    assert_eq!(*state.get(), ClientState::MainMenu);
}

#[test]
fn test_connecting_state_spawns_cancel_button() {
    let mut app = App::new();
    add_ui_test_plugin(&mut app);

    // Setup dummy client entity (needed for Connecting state)
    app.world_mut()
        .spawn((Name::new("Test Client"), Client::default()));

    app.update();

    // Transition to Connecting state
    app.world_mut()
        .resource_mut::<NextState<ClientState>>()
        .set(ClientState::Connecting);
    app.update();

    // Verify Cancel button exists
    let mut cancel_query = app
        .world_mut()
        .query_filtered::<Entity, With<CancelButton>>();
    assert_eq!(
        cancel_query.iter(app.world()).count(),
        1,
        "Should have one Cancel button"
    );
}

#[test]
fn ingame_hud_spawns_map_switch_button() {
    let mut app = App::new();
    add_ui_test_plugin(&mut app);

    app.world_mut()
        .spawn((Name::new("Test Client"), Client::default()));

    app.world_mut()
        .resource_mut::<NextState<ClientState>>()
        .set(ClientState::InGame);
    app.update();

    let mut query = app
        .world_mut()
        .query_filtered::<Entity, With<MapSwitchButton>>();
    assert_eq!(
        query.iter(app.world()).count(),
        1,
        "Should have one MapSwitchButton"
    );
}

#[test]
fn map_switch_button_label_shows_homebase_when_on_overworld() {
    let mut app = App::new();
    add_ui_test_plugin(&mut app);

    app.world_mut()
        .spawn((Name::new("Test Client"), Client::default()));

    app.world_mut()
        .resource_mut::<NextState<ClientState>>()
        .set(ClientState::InGame);
    app.update();

    app.world_mut().spawn((
        CharacterMarker,
        Predicted,
        Controlled,
        MapInstanceId::Overworld,
    ));
    app.update();

    let button_entity = app
        .world_mut()
        .query_filtered::<Entity, With<MapSwitchButton>>()
        .single(app.world())
        .expect("MapSwitchButton should exist");
    let children = app.world().get::<Children>(button_entity).unwrap();
    let text = app.world().get::<Text>(children[0]).unwrap();
    assert_eq!(
        text.0, "Homebase",
        "Button should say 'Homebase' when player is on Overworld"
    );
}

#[test]
fn map_switch_button_label_shows_overworld_when_on_homebase() {
    let mut app = App::new();
    add_ui_test_plugin(&mut app);

    app.world_mut()
        .spawn((Name::new("Test Client"), Client::default()));

    app.world_mut()
        .resource_mut::<NextState<ClientState>>()
        .set(ClientState::InGame);
    app.update();

    app.world_mut().spawn((
        CharacterMarker,
        Predicted,
        Controlled,
        MapInstanceId::Homebase {
            owner: NostrPublicKey([42; 32]),
        },
    ));
    app.update();

    let button_entity = app
        .world_mut()
        .query_filtered::<Entity, With<MapSwitchButton>>()
        .single(app.world())
        .expect("MapSwitchButton should exist");
    let children = app.world().get::<Children>(button_entity).unwrap();
    let text = app.world().get::<Text>(children[0]).unwrap();
    assert_eq!(
        text.0, "Overworld",
        "Button should say 'Overworld' when player is on Homebase"
    );
}

#[test]
fn main_menu_refreshes_when_server_list_changes() {
    let mut app = App::new();
    add_ui_test_plugin(&mut app);
    app.world_mut()
        .spawn((Name::new("Test Client"), Client::default()));

    enter_main_menu(&mut app);
    app.update();

    let mut query = app
        .world_mut()
        .query_filtered::<Entity, With<ServerListEntryButton>>();
    assert_eq!(
        query.iter(app.world()).count(),
        0,
        "MainMenu should start without server entry buttons"
    );

    let (identity, _) = nostr_client::generate_encrypted_identity("passphrase").unwrap();
    app.world_mut()
        .resource_mut::<nostr_client::announcement::ServerList>()
        .entries
        .push(nostr_client::announcement::ServerListEntry {
            pubkey: identity.public_key(),
            addr: "127.0.0.1:6001".parse().unwrap(),
            cert_digest: "digest-from-announcement".to_string(),
            display_name: "Late Server".to_string(),
            received_at: Duration::ZERO,
        });
    app.update();

    let mut query = app
        .world_mut()
        .query_filtered::<Entity, With<ServerListEntryButton>>();
    assert_eq!(
        query.iter(app.world()).count(),
        1,
        "MainMenu should render a server entry after ServerList changes"
    );
}
#[test]
fn server_list_entry_sets_connection_config() {
    let mut app = App::new();
    add_ui_test_plugin(&mut app);
    app.world_mut()
        .spawn((Name::new("Test Client"), Client::default()));
    let (identity, _) = nostr_client::generate_encrypted_identity("passphrase").unwrap();
    let pubkey = identity.public_key();
    app.insert_resource(identity);
    app.world_mut()
        .resource_mut::<nostr_client::announcement::ServerList>()
        .entries
        .push(nostr_client::announcement::ServerListEntry {
            pubkey,
            addr: "127.0.0.1:6001".parse().unwrap(),
            cert_digest: "digest-from-announcement".to_string(),
            display_name: "Listed Server".to_string(),
            received_at: Duration::ZERO,
        });
    let full_pubkey = pubkey.to_string();
    enter_main_menu(&mut app);
    app.update();

    let button = {
        let mut query = app
            .world_mut()
            .query_filtered::<Entity, With<ServerListEntryButton>>();
        query
            .single(app.world())
            .expect("ServerListEntryButton should exist")
    };
    let children = app.world().get::<Children>(button).unwrap();
    let label = app.world().get::<Text>(children[0]).unwrap();
    assert!(label.0.contains("Listed Server"));
    assert!(label.0.contains("Server ID:"));
    assert!(!label.0.contains(&full_pubkey));
    app.world_mut()
        .entity_mut(button)
        .insert(Interaction::Pressed);
    app.update();

    let config = app.world().resource::<UiClientConfig>();
    assert_eq!(config.server_addr, "127.0.0.1:6001".parse().unwrap());
    assert_eq!(config.certificate_digest, "digest-from-announcement");
    assert_eq!(
        config.client_id,
        nostr_client::client_id_from_public_key(&pubkey)
    );
}

#[test]
fn test_state_cleanup() {
    let mut app = App::new();
    add_ui_test_plugin(&mut app);

    // Setup dummy client entity (needed for Connecting state)
    app.world_mut()
        .spawn((Name::new("Test Client"), Client::default()));
    enter_main_menu(&mut app);

    app.update();

    // Verify main menu UI exists
    let mut main_menu_ui = app.world_mut().query_filtered::<Entity, With<QuitButton>>();
    assert_eq!(main_menu_ui.iter(app.world()).count(), 1);

    // Transition to Connecting state
    app.world_mut()
        .resource_mut::<NextState<ClientState>>()
        .set(ClientState::Connecting);
    app.update();

    // Verify main menu UI is despawned
    let mut main_menu_ui = app.world_mut().query_filtered::<Entity, With<QuitButton>>();
    assert_eq!(
        main_menu_ui.iter(app.world()).count(),
        0,
        "Main menu UI should be despawned"
    );

    // Verify connecting UI exists
    let mut connecting_ui = app
        .world_mut()
        .query_filtered::<Entity, With<CancelButton>>();
    assert_eq!(
        connecting_ui.iter(app.world()).count(),
        1,
        "Connecting UI should exist"
    );
}
