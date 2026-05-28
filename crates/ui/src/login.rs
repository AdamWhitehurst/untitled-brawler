use bevy::prelude::*;
use bevy_ui_text_input::TextInputBuffer;
use nostr_client::{
    generate_encrypted_identity, import_encrypted_identity, unlock_identity, LoginError, NostrKeys,
    SaveEncryptedIdentity, StoredEncryptedIdentity,
};

use crate::{components::*, state::ClientState, widgets};

pub fn setup_login_screen(
    mut commands: Commands,
    stored: Res<StoredEncryptedIdentity>,
    error: Res<LoginError>,
) {
    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(16.0),
                ..default()
            },
            BackgroundColor(widgets::SCREEN_BG),
            DespawnOnExit(ClientState::Login),
        ))
        .with_children(|parent| {
            parent.spawn((
                Text::new("Nostr Login"),
                TextFont {
                    font_size: 60.0,
                    ..default()
                },
                TextColor(Color::WHITE),
            ));

            if let Some(message) = &error.0 {
                parent.spawn((
                    Text::new(message.clone()),
                    TextFont {
                        font_size: 24.0,
                        ..default()
                    },
                    TextColor(Color::srgb(1.0, 0.2, 0.2)),
                ));
            }

            widgets::spawn_text_input(parent, "passphrase", PassphraseInput, true);
            if stored.0.is_some() {
                widgets::spawn_button(parent, "Unlock", UnlockButton);
            } else {
                widgets::spawn_text_input(parent, "nsec1...", NsecInput, false);
                widgets::spawn_button(parent, "Generate", GenerateButton);
                widgets::spawn_button(parent, "Import", ImportButton);
            }
        });
}

pub fn login_button_interaction(
    mut commands: Commands,
    mut next_state: ResMut<NextState<ClientState>>,
    mut stored: ResMut<StoredEncryptedIdentity>,
    mut error: ResMut<LoginError>,
    generate: Query<&Interaction, (Changed<Interaction>, With<GenerateButton>)>,
    import: Query<&Interaction, (Changed<Interaction>, With<ImportButton>)>,
    unlock: Query<&Interaction, (Changed<Interaction>, With<UnlockButton>)>,
    passphrase: Query<&TextInputBuffer, With<PassphraseInput>>,
    nsec: Query<&TextInputBuffer, With<NsecInput>>,
    mut save_writer: MessageWriter<SaveEncryptedIdentity>,
) {
    let passphrase = passphrase
        .single()
        .expect("PassphraseInput must exist")
        .get_text();
    let result = if pressed(&generate) {
        generate_encrypted_identity(&passphrase)
    } else if pressed(&import) {
        let nsec = nsec.single().expect("NsecInput must exist").get_text();
        import_encrypted_identity(&nsec, &passphrase)
    } else if pressed(&unlock) {
        let encrypted = stored
            .0
            .as_ref()
            .expect("stored identity required for unlock");
        unlock_identity(encrypted, &passphrase).map(|identity| (identity, encrypted.clone()))
    } else {
        return;
    };

    match result {
        Ok((identity, encrypted)) => {
            commands.insert_resource::<NostrKeys>(identity);
            stored.0 = Some(encrypted.clone());
            save_writer.write(SaveEncryptedIdentity(encrypted));
            error.0 = None;
            next_state.set(ClientState::MainMenu);
        }
        Err(message) => {
            warn!(%message, "login failed");
            error.0 = Some(message);
        }
    }
}

fn pressed<M: Component>(query: &Query<&Interaction, (Changed<Interaction>, With<M>)>) -> bool {
    query
        .iter()
        .any(|interaction| *interaction == Interaction::Pressed)
}
