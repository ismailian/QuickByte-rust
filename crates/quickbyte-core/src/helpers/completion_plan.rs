//! Port of `CompletionPlan.cs`: what should happen now that something finished —
//! the per-download touches, and the power action with what it would interrupt.
//!
//! Kept apart from the window that acts on it so the decision can be checked
//! without shutting a test machine down. The interesting part is
//! [`CompletionPlan::still_running`]: a "shut down when done" on one download
//! says nothing about the four others still transferring, and the countdown has
//! to be able to say so before it pauses them.

use uuid::Uuid;

use crate::enums::{CompletionActions, DownloadStatus, PowerAction};
use crate::helpers::download_actions;
use crate::models::{DownloadItem, DownloadQueue};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompletionPlan {
    pub reveal_folder: bool,
    pub play_sound: bool,
    pub power: PowerAction,
    pub still_running: usize,
}

impl CompletionPlan {
    pub const NOTHING: CompletionPlan =
        CompletionPlan { reveal_folder: false, play_sound: false, power: PowerAction::None, still_running: 0 };

    pub fn has_power_action(&self) -> bool {
        self.power != PowerAction::None
    }

    /// A download just completed.
    pub fn for_download<'a>(completed: &DownloadItem, all: impl IntoIterator<Item = &'a DownloadItem>) -> Self {
        if completed.status != DownloadStatus::Completed {
            return Self::NOTHING;
        }
        Self {
            reveal_folder: completed.on_completion.contains(CompletionActions::OPEN_FOLDER),
            play_sound: completed.on_completion.contains(CompletionActions::PLAY_SOUND),
            power: defined(completed.power_action_when_done),
            still_running: still_running_except(Some(completed.id), all),
        }
    }

    /// A queue's run ended. Only a run that `drained` the queue counts as
    /// finishing it: a run the user stopped, or one its own stop time cut short,
    /// has files left, and shutting the machine down on top of that would be the
    /// opposite of what was asked.
    pub fn for_queue<'a>(queue: &DownloadQueue, drained: bool, all: impl IntoIterator<Item = &'a DownloadItem>) -> Self {
        if !drained || defined(queue.when_finished) == PowerAction::None {
            return Self::NOTHING;
        }
        Self { reveal_folder: false, play_sound: false, power: queue.when_finished, still_running: still_running_except(None, all) }
    }
}

/// A value read from a hand-edited file that names no action is no action.
///
/// In Rust a [`PowerAction`] can only hold a defined value: the models read an
/// undefined number as `None` (see `models::lenient_power_action`), so this is
/// the identity, kept so the call sites read as the C# did.
pub fn defined(action: PowerAction) -> PowerAction {
    action
}

fn still_running_except<'a>(id: Option<Uuid>, all: impl IntoIterator<Item = &'a DownloadItem>) -> usize {
    all.into_iter().filter(|item| Some(item.id) != id && download_actions::is_active(item)).count()
}

#[cfg(test)]
mod tests {
    //! The cases that matter are the ones that must *not* turn a computer off: a
    //! queue that was stopped rather than emptied, a download that did not
    //! actually complete, a value in a hand-edited file that names no action at all.

    use super::*;

    fn completed(actions: CompletionActions, power: PowerAction) -> DownloadItem {
        DownloadItem { status: DownloadStatus::Completed, on_completion: actions, power_action_when_done: power, ..Default::default() }
    }

    #[test]
    fn a_completed_download_asks_for_what_it_was_set_to() {
        let item = completed(CompletionActions::OPEN_FOLDER | CompletionActions::PLAY_SOUND, PowerAction::ShutDown);
        let plan = CompletionPlan::for_download(&item, [&item]);
        assert!(plan.reveal_folder);
        assert!(plan.play_sound);
        assert_eq!(plan.power, PowerAction::ShutDown);
        assert_eq!(plan.still_running, 0);
    }

    #[test]
    fn a_download_that_did_not_complete_asks_for_nothing() {
        let mut item = completed(CompletionActions::OPEN_FOLDER, PowerAction::ShutDown);
        item.status = DownloadStatus::Failed;
        assert_eq!(CompletionPlan::for_download(&item, [&item]), CompletionPlan::NOTHING);
    }

    #[test]
    fn the_plan_counts_the_other_downloads_the_action_would_interrupt() {
        let done = completed(CompletionActions::NONE, PowerAction::Sleep);
        let others: Vec<DownloadItem> =
            [DownloadStatus::Downloading, DownloadStatus::Merging, DownloadStatus::Paused, DownloadStatus::Completed]
                .into_iter()
                .map(|status| DownloadItem { status, ..Default::default() })
                .collect();
        let plan = CompletionPlan::for_download(&done, others.iter().chain([&done]));
        assert_eq!(plan.still_running, 2);
    }

    #[test]
    fn a_power_action_no_version_of_the_app_knows_is_no_action() {
        let json = r#"{"Status":5,"PowerActionWhenDone":42}"#;
        let item: DownloadItem = serde_json::from_str(json).unwrap();
        assert_eq!(CompletionPlan::for_download(&item, [&item]).power, PowerAction::None);
        assert!(!CompletionPlan::for_download(&item, [&item]).has_power_action());
    }

    #[test]
    fn a_queue_that_worked_through_every_file_does_what_it_was_set_to() {
        let queue = DownloadQueue { when_finished: PowerAction::Hibernate, ..Default::default() };
        let plan = CompletionPlan::for_queue(&queue, true, []);
        assert_eq!(plan.power, PowerAction::Hibernate);
    }

    #[test]
    fn a_queue_stopped_before_it_was_done_never_shuts_anything_down() {
        // Stopped by the user, or cut off by its own stop time with files left:
        // turning the machine off on top of that is the opposite of what was asked.
        let queue = DownloadQueue { when_finished: PowerAction::ShutDown, ..Default::default() };
        assert!(!CompletionPlan::for_queue(&queue, false, []).has_power_action());
    }
}
