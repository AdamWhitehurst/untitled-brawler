use bevy::prelude::*;
use leafwing_input_manager::prelude::*;

/// Client-local physical input vocabulary populated by Leafwing before ownership filtering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Reflect)]
pub enum RawClientActions {
    Move,
    CameraYaw,
    CameraRotateLeft,
    CameraRotateRight,
    Jump,
    Ability1,
    Ability2,
    Ability3,
    Ability4,
    PlaceVoxel,
    RemoveVoxel,
    Delete,
    DevTogglePhysics,
    DevToggleInspector,
    DevToggleWorldInspector,
    DevToggleSpawnPanel,
    PublishHomebase,
}

impl Actionlike for RawClientActions {
    fn input_control_kind(&self) -> InputControlKind {
        match self {
            Self::Move => InputControlKind::DualAxis,
            Self::CameraYaw => InputControlKind::Axis,
            _ => InputControlKind::Button,
        }
    }
}

/// Builds the client-local raw input map for routed ownership-sensitive inputs.
pub fn raw_client_input_map() -> InputMap<RawClientActions> {
    InputMap::default()
        .with(RawClientActions::Ability1, KeyCode::Digit1)
        .with(RawClientActions::Ability2, KeyCode::Digit2)
        .with(RawClientActions::Ability3, KeyCode::Digit3)
        .with(RawClientActions::Ability4, KeyCode::Digit4)
        .with_dual_axis(RawClientActions::Move, GamepadStick::LEFT)
        .with_dual_axis(RawClientActions::Move, VirtualDPad::wasd())
        .with(RawClientActions::CameraRotateLeft, KeyCode::KeyQ)
        .with(RawClientActions::CameraRotateRight, KeyCode::KeyE)
        .with(RawClientActions::Jump, KeyCode::Space)
        .with(RawClientActions::Jump, GamepadButton::South)
        .with(RawClientActions::PlaceVoxel, MouseButton::Left)
        .with(RawClientActions::RemoveVoxel, MouseButton::Right)
        .with(RawClientActions::Delete, KeyCode::Delete)
        .with(RawClientActions::DevTogglePhysics, KeyCode::F3)
        .with(RawClientActions::DevToggleInspector, KeyCode::F4)
        .with(RawClientActions::DevToggleWorldInspector, KeyCode::F5)
        .with(RawClientActions::DevToggleSpawnPanel, KeyCode::F6)
        .with(RawClientActions::PublishHomebase, KeyCode::F7)
}
