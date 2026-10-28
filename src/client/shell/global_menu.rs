use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClientGlobalMenuAction {
    Binding(crate::input::KeybindAction),
    WhatsNew,
    /// Index into the snapshot's endpoint command manifest (`ClientShellSnapshot::commands`).
    Command(usize),
}

pub(super) fn global_menu_attention(snapshot: &ClientShellSnapshot) -> bool {
    snapshot.update_available.is_some() || snapshot.integration_updates_available
}

pub(super) fn global_menu_item_has_badge(
    snapshot: &ClientShellSnapshot,
    action: ClientGlobalMenuAction,
) -> bool {
    (action == ClientGlobalMenuAction::WhatsNew && snapshot.update_available.is_some())
        || (action == ClientGlobalMenuAction::Binding(crate::input::KeybindAction::Settings)
            && snapshot.integration_updates_available)
}

/// Endpoint custom commands that opted into the global menu, in config order.
/// Commands this client cannot execute are left out. The label is the
/// command's description, or its key label when the description is missing.
fn global_menu_command_items(
    snapshot: &ClientShellSnapshot,
) -> impl Iterator<Item = (&str, ClientGlobalMenuAction)> {
    snapshot
        .commands
        .iter()
        .enumerate()
        .filter(|(_, command)| {
            command.menu && command.action != crate::protocol::ClientShellCommandAction::Unknown
        })
        .map(|(index, command)| {
            let label = command
                .description
                .as_deref()
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .unwrap_or(command.binding_label.as_str());
            (label, ClientGlobalMenuAction::Command(index))
        })
}

pub(super) fn global_menu_items(
    snapshot: &ClientShellSnapshot,
) -> Vec<(&str, ClientGlobalMenuAction)> {
    let mut items = vec![
        (
            "settings",
            ClientGlobalMenuAction::Binding(crate::input::KeybindAction::Settings),
        ),
        (
            "keybinds",
            ClientGlobalMenuAction::Binding(crate::input::KeybindAction::Help),
        ),
        (
            "reload config",
            ClientGlobalMenuAction::Binding(crate::input::KeybindAction::ReloadConfig),
        ),
    ];
    if snapshot.update_available.is_some() || snapshot.latest_release_notes_available {
        items.push((
            if snapshot.update_available.is_some() {
                "update ready"
            } else {
                "what's new"
            },
            ClientGlobalMenuAction::WhatsNew,
        ));
    }
    items.extend(global_menu_command_items(snapshot));
    items.push((
        "detach",
        ClientGlobalMenuAction::Binding(crate::input::KeybindAction::Detach),
    ));
    items
}

impl ClientShellState {
    pub(super) fn toggle_global_menu(&mut self) {
        if matches!(self.overlay, Some(ClientShellOverlay::GlobalMenu(_))) {
            self.overlay = None;
        } else {
            self.overlay = Some(ClientShellOverlay::GlobalMenu(ClientGlobalMenuOverlay {
                highlighted: 0,
            }));
        }
    }

    pub(super) fn move_global_menu_selection(&mut self, delta: isize) {
        let item_count = self
            .snapshot
            .as_deref()
            .map(global_menu_items)
            .map_or(0, |items| items.len());
        let Some(ClientShellOverlay::GlobalMenu(menu)) = self.overlay.as_mut() else {
            return;
        };
        menu.highlighted = (menu.highlighted as isize + delta)
            .clamp(0, item_count.saturating_sub(1) as isize) as usize;
    }

    /// The custom command binding a menu entry stands for. This is the same
    /// binding its key resolves to, so activating the entry runs the key's path.
    /// When this client has no key binding for the command (local keybindings
    /// against a remote endpoint, or a key dropped by a collision), the binding
    /// is rebuilt from the endpoint manifest entry, which carries the same ID.
    fn global_menu_command_binding(
        &self,
        index: usize,
    ) -> Option<crate::config::CustomCommandKeybind> {
        let command = self.snapshot.as_deref()?.commands.get(index)?;
        if let Some(binding) = self
            .config
            .keybinds
            .keybinds
            .custom_commands
            .iter()
            .find(|binding| binding.command == command.command_id)
        {
            return Some(binding.clone());
        }
        Some(crate::config::CustomCommandKeybind {
            bindings: crate::config::ActionKeybinds::from_labels(&command.binding_labels)
                .unwrap_or_default(),
            label: command.binding_label.clone(),
            command: command.command_id.clone(),
            action: command.action.try_into().ok()?,
            description: command.description.clone(),
            width: None,
            height: None,
            menu: command.menu,
        })
    }

    pub(super) fn activate_global_menu_item(
        &mut self,
        index: usize,
        outcome: &mut ClientShellInput,
    ) {
        let Some(action) = self.snapshot.as_deref().and_then(|snapshot| {
            global_menu_items(snapshot)
                .get(index)
                .map(|(_, action)| *action)
        }) else {
            return;
        };
        if action == ClientGlobalMenuAction::WhatsNew
            && self
                .snapshot
                .as_deref()
                .and_then(|snapshot| snapshot.release_notes.as_ref())
                .is_none()
        {
            return;
        }
        let command = match action {
            ClientGlobalMenuAction::Command(command) => {
                let Some(binding) = self.global_menu_command_binding(command) else {
                    return;
                };
                Some(binding)
            }
            ClientGlobalMenuAction::Binding(_) | ClientGlobalMenuAction::WhatsNew => None,
        };
        self.overlay = None;
        match (action, command) {
            (ClientGlobalMenuAction::Binding(binding), _) => {
                self.record_binding(crate::input::KeybindMatch::Action(binding), outcome)
            }
            (ClientGlobalMenuAction::WhatsNew, _) => self.open_release_notes(),
            (ClientGlobalMenuAction::Command(_), Some(binding)) => {
                self.record_binding(crate::input::KeybindMatch::Command(binding), outcome)
            }
            (ClientGlobalMenuAction::Command(_), None) => {}
        }
        outcome.repaint = true;
    }
}
