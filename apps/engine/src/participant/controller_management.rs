//! Data-management commands share the participant owner and its room document.

use super::{AudioEffect, Command, CommandOutcome, Worker};
use crate::participant::management::{self, ManagementResult};

impl Worker {
    pub(super) fn apply_management_command(
        &mut self,
        command: Command,
    ) -> Result<CommandOutcome, String> {
        let name = command.name().to_owned();
        let data = match command {
            Command::CheckPendingSettings => {
                let active = self
                    .rooms
                    .as_ref()
                    .map(|room| room.session.snapshot_settings())
                    .transpose()?;
                self.observe_rooms()?;
                let mut changes = Vec::new();
                if self.unsaved {
                    changes.push(management::UnsavedChange {
                        kind: "configuration",
                        path: self.path.clone(),
                        detail: self.persistence_error.clone(),
                    });
                }
                if let Some(active) = active {
                    for (kind, status) in [
                        ("global", active.rule_files.global),
                        ("room", active.rule_files.room),
                    ] {
                        if status.outcome.unsaved() {
                            if let Some(path) = status.path {
                                changes.push(management::UnsavedChange {
                                    kind,
                                    path,
                                    detail: status.outcome.detail().map(str::to_owned),
                                });
                            }
                        }
                    }
                }
                ManagementResult::PendingChanges { changes }
            }
            Command::ReadRoomCard { path } => {
                let active = self
                    .rooms
                    .as_ref()
                    .map(|room| room.session.snapshot_settings())
                    .transpose()?;
                let document = match active {
                    Some(active)
                        if active
                            .profile
                            .as_ref()
                            .is_some_and(|(active, _)| management::same_path(active, &path)) =>
                    {
                        let (_, profile) = active.profile.expect("matched profile");
                        let error=active.profile_revision.is_none().then(||"This card has not been written yet. Restore its storage or export the effective settings before editing its target.".to_owned());
                        management::CardDocument {
                            path,
                            revision: active.profile_revision.unwrap_or_default(),
                            profile: Some(profile),
                            error,
                        }
                    }
                    _ => management::read_card(&path)?,
                };
                ManagementResult::Card { document }
            }
            Command::SaveRoomCard {
                path,
                target,
                expected_revision,
            } => {
                let current = self
                    .rooms
                    .as_ref()
                    .map(|room| room.session.snapshot_settings())
                    .transpose()?;
                let active = current
                    .and_then(|state| state.profile)
                    .is_some_and(|(active, _)| management::same_path(&active, &path));
                let document = if active {
                    self.rooms
                        .as_ref()
                        .ok_or("The room owner is unavailable.")?
                        .session
                        .update_card_target(path, target, expected_revision)?
                } else {
                    management::save_card(&path, target, &expected_revision)?
                };
                self.apply_room_command(Command::RefreshRooms)?;
                ManagementResult::Card { document }
            }
            Command::DeleteRoomCard {
                path,
                expected_revision,
            } => {
                let current = self
                    .rooms
                    .as_ref()
                    .map(|room| room.session.snapshot_settings())
                    .transpose()?;
                if current
                    .and_then(|state| state.profile)
                    .is_some_and(|(active, _)| management::same_path(&active, &path))
                {
                    return Err("Leave this room before deleting its active card.".into());
                }
                let selected = self.config.room.selected.as_ref().is_some_and(|s| {
                    let selected = match s {
                        crate::participant::RoomStartup::Join { profile_path }
                        | crate::participant::RoomStartup::Create { profile_path, .. } => {
                            profile_path
                        }
                    };
                    management::same_path(selected, &path)
                });
                management::delete_card(&self.path, &path, &expected_revision)?;
                if selected {
                    self.config.room.selected = None;
                    self.config.room.auto_enter = false;
                    self.persist();
                }
                self.apply_room_command(Command::RefreshRooms)?;
                ManagementResult::CardDeleted { path }
            }
            Command::ExportSettings { path } => {
                let active = self
                    .rooms
                    .as_ref()
                    .map(|room| room.session.snapshot_settings())
                    .transpose()?;
                self.observe_rooms()?;
                let config = self.effective_export_config();
                let mut preview =
                    management::export_effective(&self.path, &config, active, &path)?;
                if self.unsaved {
                    preview.warnings.push("This export also contains current local settings that were not saved to the original configuration.".into());
                }
                ManagementResult::Exported { path, preview }
            }
            _ => return Err("This command does not belong to settings management.".into()),
        };
        let mut outcome = self.outcome(
            &name,
            true,
            AudioEffect::NotApplicable,
            "The requested settings operation completed.",
        );
        outcome.management = Some(data);
        Ok(outcome)
    }
}
