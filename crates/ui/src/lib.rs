pub mod components;
pub mod login;
pub mod state;
pub mod widgets;

use bevy::ecs::message::MessageWriter;
use bevy::prelude::*;
use bevy_ui_text_input::TextInputPlugin;
pub use components::*;
use lightyear::netcode::Key;
use lightyear::prelude::{client::*, Controlled, Replicated};
use lightyear::prelude::{Authentication, MessageSender, PeerAddr, Predicted};
use lightyear::webtransport::client::WebTransportClientIo;
use nostr_client::announcement::ServerList;
use nostr_client::{LoginError, NostrKeys, StoredEncryptedIdentity};
use protocol::map::{MapChannel, MapSwitchTarget, PlayerMapSwitchRequest};
use protocol::{
    CharacterMarker, DummyTarget, MapInstanceId, PendingTransition, PRIVATE_KEY, PROTOCOL_ID,
};
pub use state::{ClientState, MapTransitionState};
use std::net::SocketAddr;

/// Lightweight client config for UI - mirrors essential fields from client::ClientNetworkConfig
/// This exists to avoid circular dependency between client and ui crates.
/// The main.rs is responsible for syncing this with ClientNetworkConfig.
#[derive(Clone, Resource)]
pub struct UiClientConfig {
    pub server_addr: SocketAddr,
    pub client_id: u64,
    pub certificate_digest: String,
    pub protocol_id: u64,
    pub private_key: [u8; 32],
}

impl Default for UiClientConfig {
    fn default() -> Self {
        Self {
            server_addr: SocketAddr::from(([127, 0, 0, 1], 5001)),
            client_id: 0,
            protocol_id: PROTOCOL_ID,
            certificate_digest: String::new(),
            private_key: PRIVATE_KEY,
        }
    }
}

/// Plugin that manages UI and client state
pub struct UiPlugin;

impl Plugin for UiPlugin {
    fn build(&self, app: &mut App) {
        // Initialize resources
        app.init_resource::<UiClientConfig>();
        app.add_plugins(TextInputPlugin);
        app.init_resource::<LoginError>();
        app.init_resource::<StoredEncryptedIdentity>();
        app.init_resource::<ServerList>();
        app.add_message::<nostr_client::SaveEncryptedIdentity>();

        // Initialize state management
        app.init_state::<ClientState>();

        app.add_sub_state::<MapTransitionState>();
        app.add_systems(
            OnEnter(MapTransitionState::Transitioning),
            setup_transition_loading_screen,
        );
        app.add_systems(
            Update,
            update_loading_screen_text
                .run_if(in_state(MapTransitionState::Transitioning))
                .run_if(resource_exists::<protocol::transition::ClientTransitionState>),
        );

        app.add_systems(
            OnEnter(protocol::AppState::Ready),
            enter_login_after_app_ready,
        );
        app.add_systems(OnEnter(ClientState::Login), login::setup_login_screen);
        app.add_systems(
            Update,
            login::login_button_interaction.run_if(in_state(ClientState::Login)),
        );
        // State transition systems
        app.add_systems(
            OnEnter(ClientState::Connecting),
            on_entering_connecting_state,
        );
        app.add_observer(on_client_disconnected);
        app.add_observer(on_client_connected);

        // Main menu
        app.add_systems(OnEnter(ClientState::MainMenu), setup_main_menu);
        app.add_systems(
            Update,
            main_menu_button_interaction.run_if(in_state(ClientState::MainMenu)),
        );
        app.add_systems(
            Update,
            refresh_main_menu_server_list
                .run_if(in_state(ClientState::MainMenu))
                .run_if(resource_changed::<ServerList>),
        );

        // Connecting screen
        app.add_systems(OnEnter(ClientState::Connecting), setup_connecting_screen);
        app.add_systems(
            Update,
            connecting_screen_interaction.run_if(in_state(ClientState::Connecting)),
        );

        // In-game HUD
        app.add_systems(OnEnter(ClientState::InGame), setup_ingame_hud);
        app.add_systems(
            Update,
            (
                ingame_button_interaction,
                map_switch_button_interaction,
                update_map_switch_button_label,
            )
                .run_if(in_state(ClientState::InGame)),
        );

        trace!("UiPlugin initialized");
    }
}

fn enter_login_after_app_ready(
    current_state: Res<State<ClientState>>,
    mut next_state: ResMut<NextState<ClientState>>,
) {
    if *current_state.get() == ClientState::Loading {
        info!("Startup gates complete, entering login");
        next_state.set(ClientState::Login);
    }
}

fn on_entering_connecting_state(
    mut commands: Commands,
    client_query: Query<Entity, With<Client>>,
    config: Res<UiClientConfig>,
) {
    trace!("Entering Connecting state, triggering connection...");
    let client_entity = client_query.single().expect("Client entity should exist");

    // Create fresh authentication with new token
    let auth = Authentication::Manual {
        server_addr: config.server_addr,
        client_id: config.client_id,
        private_key: Key::from(config.private_key),
        protocol_id: config.protocol_id,
    };

    // Insert fresh connection components so server-list selection can replace startup defaults.
    commands.entity(client_entity).insert((
        PeerAddr(config.server_addr),
        NetcodeClient::new(auth, NetcodeConfig::default()).expect("Failed to create NetcodeClient"),
        WebTransportClientIo {
            certificate_digest: config.certificate_digest.clone(),
        },
    ));

    commands.trigger(Connect {
        entity: client_entity,
    });
}

fn on_client_disconnected(
    _trigger: On<Add, Disconnected>,
    mut next_state: ResMut<NextState<ClientState>>,
    current_state: Res<State<ClientState>>,
    identity: Option<Res<NostrKeys>>,
) {
    match *current_state.get() {
        ClientState::Loading | ClientState::Login | ClientState::MainMenu => {}
        _ => {
            if identity.is_some() {
                info!("Client disconnected, returning to main menu");
                next_state.set(ClientState::MainMenu);
            } else {
                info!("Client disconnected without identity, returning to login");
                next_state.set(ClientState::Login);
            }
        }
    }
}

fn on_client_connected(
    _trigger: On<Add, Connected>,
    mut next_state: ResMut<NextState<ClientState>>,
    current_state: Res<State<ClientState>>,
) {
    if *current_state.get() != ClientState::Connecting {
        warn!(
            state = ?current_state.get(),
            "Client connected outside Connecting state; ignoring InGame transition"
        );
        return;
    }

    info!("Client connected, transitioning to InGame state");
    next_state.set(ClientState::InGame);
}

fn setup_main_menu(mut commands: Commands, server_list: Res<ServerList>) {
    trace!("Setting up main menu UI");

    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(20.0),
                ..default()
            },
            BackgroundColor(Color::srgb(0.1, 0.1, 0.1)),
            DespawnOnExit(ClientState::MainMenu),
        ))
        .with_children(|parent| {
            // Title
            parent.spawn((
                Text::new("Lightyear Client"),
                TextFont {
                    font_size: 60.0,
                    ..default()
                },
                TextColor(Color::WHITE),
            ));

            parent
                .spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: Val::Px(12.0),
                        ..default()
                    },
                    ServerListContainer,
                ))
                .with_children(|parent| {
                    spawn_server_list_buttons(parent, &server_list);
                });
            // Quit Button
            parent
                .spawn((
                    Button,
                    Node {
                        width: Val::Px(200.0),
                        height: Val::Px(65.0),
                        border: UiRect::all(Val::Px(5.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    BorderColor::all(Color::WHITE),
                    BackgroundColor(Color::srgb(0.2, 0.2, 0.2)),
                    QuitButton,
                ))
                .with_children(|parent| {
                    parent.spawn((
                        Text::new("Quit"),
                        TextFont {
                            font_size: 33.0,
                            ..default()
                        },
                        TextColor(Color::WHITE),
                    ));
                });
        });
}

fn spawn_server_list_buttons(
    parent: &mut bevy::ecs::hierarchy::ChildSpawnerCommands,
    server_list: &ServerList,
) {
    for entry in &server_list.entries {
        widgets::spawn_server_entry_button(
            parent,
            &entry.menu_label(),
            ServerListEntryButton(entry.clone()),
        );
    }
}

fn refresh_main_menu_server_list(
    mut commands: Commands,
    server_list: Res<ServerList>,
    container_query: Query<Entity, With<ServerListContainer>>,
    entry_query: Query<Entity, With<ServerListEntryButton>>,
) {
    let Ok(container) = container_query.single() else {
        trace!("refresh_main_menu_server_list: server list container not found");
        return;
    };

    for entity in &entry_query {
        commands.entity(entity).despawn();
    }

    commands.entity(container).with_children(|parent| {
        spawn_server_list_buttons(parent, &server_list);
    });
}

fn main_menu_button_interaction(
    mut next_state: ResMut<NextState<ClientState>>,
    mut exit_writer: MessageWriter<AppExit>,
    mut config: ResMut<UiClientConfig>,
    identity: Option<Res<NostrKeys>>,
    entry_query: Query<(&Interaction, &ServerListEntryButton), Changed<Interaction>>,
    quit_query: Query<&Interaction, (Changed<Interaction>, With<QuitButton>)>,
) {
    for (interaction, entry) in &entry_query {
        if *interaction == Interaction::Pressed {
            let identity = identity
                .as_ref()
                .expect("NostrKeys must exist before server selection");
            config.server_addr = entry.0.addr;
            config.certificate_digest = entry.0.cert_digest.clone();
            config.client_id = nostr_client::client_id_from_public_key(&identity.public_key());
            info!(
                pubkey = %identity.public_key(),
                server = %entry.0.pubkey,
                addr = %entry.0.addr,
                "selected Nostr server"
            );
            next_state.set(ClientState::Connecting);
        }
    }

    // Handle Quit button
    for interaction in quit_query.iter() {
        if *interaction == Interaction::Pressed {
            trace!("Quit button pressed");
            exit_writer.write(AppExit::Success);
        }
    }
}

fn setup_connecting_screen(mut commands: Commands) {
    trace!("Setting up connecting screen UI");

    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(20.0),
                ..default()
            },
            BackgroundColor(Color::srgb(0.1, 0.1, 0.1)),
            DespawnOnExit(ClientState::Connecting),
        ))
        .with_children(|parent| {
            // Connecting message
            parent.spawn((
                Text::new("Connecting to server..."),
                TextFont {
                    font_size: 40.0,
                    ..default()
                },
                TextColor(Color::WHITE),
            ));

            // Cancel Button
            parent
                .spawn((
                    Button,
                    Node {
                        width: Val::Px(200.0),
                        height: Val::Px(65.0),
                        border: UiRect::all(Val::Px(5.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    BorderColor::all(Color::WHITE),
                    BackgroundColor(Color::srgb(0.2, 0.2, 0.2)),
                    CancelButton,
                ))
                .with_children(|parent| {
                    parent.spawn((
                        Text::new("Cancel"),
                        TextFont {
                            font_size: 33.0,
                            ..default()
                        },
                        TextColor(Color::WHITE),
                    ));
                });
        });
}

fn connecting_screen_interaction(
    mut commands: Commands,
    mut next_state: ResMut<NextState<ClientState>>,
    client_query: Query<Entity, With<Client>>,
    cancel_query: Query<&Interaction, (Changed<Interaction>, With<CancelButton>)>,
) {
    for interaction in cancel_query.iter() {
        if *interaction == Interaction::Pressed {
            trace!("Cancel button pressed, disconnecting...");

            let client_entity = client_query.single().expect("Client entity should exist");
            // Trigger disconnection
            commands.trigger(Disconnect {
                entity: client_entity,
            });

            // Return to main menu (observer will also handle this, but explicit is clearer)
            next_state.set(ClientState::MainMenu);
        }
    }
}

fn setup_ingame_hud(mut commands: Commands) {
    trace!("Setting up in-game HUD");

    // Top-right corner HUD
    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                justify_content: JustifyContent::End,
                align_items: AlignItems::Start,
                padding: UiRect::all(Val::Px(20.0)),
                flex_direction: FlexDirection::Row,
                column_gap: Val::Px(10.0),
                ..default()
            },
            DespawnOnExit(ClientState::InGame),
        ))
        .with_children(|parent| {
            // Map Switch Button
            parent
                .spawn((
                    Button,
                    Node {
                        width: Val::Px(150.0),
                        height: Val::Px(50.0),
                        border: UiRect::all(Val::Px(3.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    BorderColor::all(Color::WHITE),
                    BackgroundColor(Color::srgba(0.2, 0.2, 0.2, 0.8)),
                    MapSwitchButton,
                ))
                .with_children(|parent| {
                    parent.spawn((
                        Text::new("Homebase"),
                        TextFont {
                            font_size: 24.0,
                            ..default()
                        },
                        TextColor(Color::WHITE),
                    ));
                });

            // Main Menu Button
            parent
                .spawn((
                    Button,
                    Node {
                        width: Val::Px(150.0),
                        height: Val::Px(50.0),
                        border: UiRect::all(Val::Px(3.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    BorderColor::all(Color::WHITE),
                    BackgroundColor(Color::srgba(0.2, 0.2, 0.2, 0.8)),
                    MainMenuButton,
                ))
                .with_children(|parent| {
                    parent.spawn((
                        Text::new("Main Menu"),
                        TextFont {
                            font_size: 24.0,
                            ..default()
                        },
                        TextColor(Color::WHITE),
                    ));
                });

            // Quit Button
            parent
                .spawn((
                    Button,
                    Node {
                        width: Val::Px(150.0),
                        height: Val::Px(50.0),
                        border: UiRect::all(Val::Px(3.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    BorderColor::all(Color::WHITE),
                    BackgroundColor(Color::srgba(0.2, 0.2, 0.2, 0.8)),
                    QuitButton,
                ))
                .with_children(|parent| {
                    parent.spawn((
                        Text::new("Quit"),
                        TextFont {
                            font_size: 24.0,
                            ..default()
                        },
                        TextColor(Color::WHITE),
                    ));
                });
        });
}

fn ingame_button_interaction(
    mut commands: Commands,
    mut next_state: ResMut<NextState<ClientState>>,
    mut exit_writer: MessageWriter<AppExit>,
    client_query: Query<Entity, With<Client>>,
    main_menu_query: Query<&Interaction, (Changed<Interaction>, With<MainMenuButton>)>,
    quit_query: Query<
        &Interaction,
        (
            Changed<Interaction>,
            With<QuitButton>,
            Without<MainMenuButton>,
        ),
    >,
) {
    // Handle Main Menu button
    for interaction in main_menu_query.iter() {
        if *interaction == Interaction::Pressed {
            trace!("Main Menu button pressed, disconnecting...");

            let client_entity = client_query.single().expect("Client entity should exist");
            // Trigger disconnection
            commands.trigger(Disconnect {
                entity: client_entity,
            });

            // Return to main menu (observer will also handle this)
            next_state.set(ClientState::MainMenu);
        }
    }

    // Handle Quit button
    for interaction in quit_query.iter() {
        if *interaction == Interaction::Pressed {
            trace!("Quit button pressed");
            exit_writer.write(AppExit::Success);
        }
    }
}

fn map_switch_button_interaction(
    switch_query: Query<&Interaction, (Changed<Interaction>, With<MapSwitchButton>)>,
    player_query: Query<
        &MapInstanceId,
        (
            With<Predicted>,
            With<Replicated>,
            With<CharacterMarker>,
            With<Controlled>,
            Without<DummyTarget>,
        ),
    >,
    mut senders: Query<&mut MessageSender<PlayerMapSwitchRequest>>,
    client_transition: Res<protocol::transition::ClientTransitionState>,
) {
    if client_transition.phase != protocol::transition::TransitionPhase::Idle {
        return;
    }

    for interaction in &switch_query {
        if *interaction != Interaction::Pressed {
            continue;
        }

        let current_map = player_query
            .single()
            .expect("Predicted player must exist when pressing map switch button");

        let target = match current_map {
            MapInstanceId::Overworld => MapSwitchTarget::Homebase,
            MapInstanceId::Homebase { .. } => MapSwitchTarget::Overworld,
        };

        trace!("Map switch button pressed, requesting {target:?}");
        for mut sender in &mut senders {
            sender.send::<MapChannel>(PlayerMapSwitchRequest {
                target: target.clone(),
            });
        }
        // Loading screen now appears when MapTransitionStart arrives from server
    }
}

fn update_map_switch_button_label(
    player_query: Query<
        (&MapInstanceId, Option<&PendingTransition>),
        (With<Predicted>, With<CharacterMarker>, With<Controlled>),
    >,
    button_query: Query<&Children, With<MapSwitchButton>>,
    mut text_query: Query<&mut Text>,
) {
    let Ok((map_id, pending)) = player_query.single() else {
        return;
    };
    let Ok(children) = button_query.single() else {
        return;
    };

    let effective_map = pending.map(|p| &p.0).unwrap_or(map_id);
    let label = match effective_map {
        MapInstanceId::Overworld => "Homebase",
        MapInstanceId::Homebase { .. } => "Overworld",
    };

    for child in children.iter() {
        if let Ok(mut text) = text_query.get_mut(child) {
            text.0 = label.to_string();
        }
    }
}

/// Marker for the loading screen text entity.
#[derive(Component)]
struct LoadingScreenText;

fn setup_transition_loading_screen(mut commands: Commands) {
    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 1.0)),
            GlobalZIndex(100),
            DespawnOnExit(MapTransitionState::Transitioning),
        ))
        .with_children(|parent| {
            parent.spawn((
                LoadingScreenText,
                Text::new("Loading..."),
                TextFont {
                    font_size: 48.0,
                    ..default()
                },
                TextColor(Color::WHITE),
            ));
        });
}

fn update_loading_screen_text(
    state: Res<protocol::transition::ClientTransitionState>,
    mut text_query: Query<&mut Text, With<LoadingScreenText>>,
) {
    let Ok(mut text) = text_query.single_mut() else {
        return;
    };
    let phase_name = match state.phase {
        protocol::transition::TransitionPhase::Cleanup => "Cleanup",
        protocol::transition::TransitionPhase::Loading => "Loading",
        protocol::transition::TransitionPhase::Ready => "Syncing",
        protocol::transition::TransitionPhase::Complete
        | protocol::transition::TransitionPhase::Idle => "Loading",
    };
    *text = Text::new(format!("{phase_name}..."));
}
