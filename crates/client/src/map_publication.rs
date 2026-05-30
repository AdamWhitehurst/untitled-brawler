//! Client-owned homebase publication.
//!
//! Under the Phase 5 "server encodes, client signs" model the client cannot
//! reproduce the server's authoritative save bytes from replication, so it asks
//! the server to prepare and authorize a publication. The server uploads the
//! payload blobs, signs an attestation, and returns an unsigned `NostrMapManifest`.
//! The client then signs that manifest event with the player's Nostr key and
//! publishes it to relays.

use bevy::prelude::*;
use bevy::tasks::{IoTaskPool, Task};
use lightyear::prelude::{MessageReceiver, MessageSender};
use nostr_client::events::NostrEventDraft;
use nostr_client::{NostrKeys, RelayPool};
use nostr_map_persistence::{
    build_signed_map_manifest_event, manifest_from_json, ManifestHash, MapManifestSigner,
    NostrManifestPublishStore, RemotePersistenceError,
};
use persistence::AsyncStore;
use protocol::map::{HomebaseAttestationRequest, HomebaseAttestationResponse, MapChannel};
use protocol::NostrPublicKey;

/// Signs homebase manifest events with the player's Nostr identity.
struct ClientManifestSigner<'a>(&'a NostrKeys);

impl MapManifestSigner for ClientManifestSigner<'_> {
    fn public_key(&self) -> NostrPublicKey {
        self.0.protocol_public_key()
    }

    fn sign_map_manifest_event(
        &self,
        draft: NostrEventDraft,
    ) -> Result<String, RemotePersistenceError> {
        self.0
            .sign_event(&draft)
            .map_err(RemotePersistenceError::from)
    }
}

/// In-flight relay publication tasks for signed homebase manifests.
#[derive(Resource, Default)]
struct PendingHomebasePublishes(Vec<Task<Result<(), String>>>);

/// Sends a homebase publication request when the player presses the publish key.
///
/// The request carries no payload: the server derives the owner from the
/// authenticated connection and publishes that player's own homebase.
fn request_homebase_publish_on_keypress(
    keys: Res<ButtonInput<KeyCode>>,
    mut senders: Query<&mut MessageSender<HomebaseAttestationRequest>>,
) {
    if !keys.just_pressed(KeyCode::F9) {
        return;
    }
    let mut requested = false;
    for mut sender in &mut senders {
        sender.send::<MapChannel>(HomebaseAttestationRequest);
        requested = true;
    }
    if requested {
        info!("requested homebase publication from server");
    } else {
        warn!("no MapChannel sender available to request homebase publication");
    }
}

/// Signs and publishes the unsigned manifest the server returns on a granted request.
fn handle_homebase_attestation_response(
    mut receivers: Query<&mut MessageReceiver<HomebaseAttestationResponse>>,
    identity: Res<NostrKeys>,
    relay_pool: Res<RelayPool>,
    mut pending: ResMut<PendingHomebasePublishes>,
) {
    for mut receiver in &mut receivers {
        for response in receiver.receive() {
            match response {
                HomebaseAttestationResponse::Rejected(reason) => {
                    warn!(%reason, "server rejected homebase publication");
                }
                HomebaseAttestationResponse::Granted {
                    unsigned_manifest_json,
                } => match sign_homebase_manifest(&identity, &unsigned_manifest_json) {
                    Ok((manifest_hash, event_json)) => {
                        let store = NostrManifestPublishStore {
                            client: relay_pool.event_client(),
                        };
                        pending.0.push(IoTaskPool::get().spawn(async move {
                            store
                                .save(&manifest_hash, &event_json)
                                .await
                                .map_err(|error| error.to_string())
                        }));
                        info!("signed homebase manifest; publishing to relays");
                    }
                    Err(error) => warn!(%error, "failed to sign homebase manifest"),
                },
            }
        }
    }
}

/// Deserializes the server's unsigned manifest and signs it with the player key.
fn sign_homebase_manifest(
    identity: &NostrKeys,
    unsigned_manifest_json: &str,
) -> Result<(ManifestHash, String), String> {
    let manifest = manifest_from_json(unsigned_manifest_json).map_err(|error| error.to_string())?;
    build_signed_map_manifest_event(&ClientManifestSigner(identity), manifest)
        .map_err(|error| error.to_string())
}

/// Drains completed relay publications and logs their outcome.
fn poll_homebase_publishes(mut pending: ResMut<PendingHomebasePublishes>) {
    let mut index = 0;
    while index < pending.0.len() {
        let Some(result) = bevy::tasks::futures::check_ready(&mut pending.0[index]) else {
            index += 1;
            continue;
        };
        let _ = pending.0.swap_remove(index);
        match result {
            Ok(()) => info!("published homebase manifest to relays"),
            Err(error) => error!(%error, "failed to publish homebase manifest"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_map_persistence::{
        compute_descriptor_root, manifest_to_json, HomebasePayloadScope,
        HomebasePublicationAttestation, NostrMapManifest, MAP_MANIFEST_SCHEMA_VERSION,
    };
    use protocol::MapInstanceId;

    fn unsigned_homebase_manifest_json(owner: NostrPublicKey) -> String {
        let map_id = MapInstanceId::Homebase { owner };
        let attestation = HomebasePublicationAttestation {
            owner,
            map_id: map_id.clone(),
            server_revision: 0,
            previous_manifest_hash: None,
            descriptor_root: compute_descriptor_root(&[]).unwrap(),
            payload_scope: HomebasePayloadScope::default(),
            expires_at: u64::MAX,
            server_pubkey: NostrPublicKey([9; 32]),
            server_signature: vec![1; 64],
        };
        let manifest = NostrMapManifest {
            map_id,
            owner,
            revision: 0,
            previous_hash: None,
            payloads: Vec::new(),
            schema_version: MAP_MANIFEST_SCHEMA_VERSION,
            descriptor_root: compute_descriptor_root(&[]).unwrap(),
            homebase_attestation: Some(attestation),
        };
        manifest_to_json(&manifest).unwrap()
    }

    #[test]
    fn homebase_publication_owner_signs_server_manifest() {
        let keys = NostrKeys::generate();
        let json = unsigned_homebase_manifest_json(keys.protocol_public_key());

        let (_, event_json) =
            sign_homebase_manifest(&keys, &json).expect("owner signs its homebase manifest");
        assert!(event_json.contains("\"content\""));
    }

    #[test]
    fn homebase_publication_non_owner_cannot_sign() {
        let owner = NostrKeys::generate();
        let other = NostrKeys::generate();
        let json = unsigned_homebase_manifest_json(owner.protocol_public_key());

        let result = sign_homebase_manifest(&other, &json);
        assert!(
            result.is_err(),
            "a client may not sign a manifest owned by another player"
        );
    }

    #[test]
    fn homebase_publication_rejects_malformed_json() {
        let keys = NostrKeys::generate();
        assert!(sign_homebase_manifest(&keys, "not json").is_err());
    }
}

/// Wires client-owned homebase publication request/sign/publish systems.
pub struct ClientMapPublicationPlugin;

impl Plugin for ClientMapPublicationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PendingHomebasePublishes>().add_systems(
            Update,
            (
                request_homebase_publish_on_keypress.run_if(resource_exists::<NostrKeys>),
                handle_homebase_attestation_response
                    .run_if(resource_exists::<NostrKeys>.and(resource_exists::<RelayPool>)),
                poll_homebase_publishes,
            ),
        );
    }
}
