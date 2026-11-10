//! The C# `Enums/` folder. Persisted enums are serialized as their numbers
//! (`serde_repr`), exactly as System.Text.Json wrote them, so the discriminants
//! here are part of the file format: never reorder, only append.

use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

/// The user's choice of colours. `System` follows Windows' own app mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum AppTheme {
    #[default]
    System = 0,
    Light = 1,
    Dark = 2,
}

/// The small things a download can ask for when it finishes, beside a
/// [`PowerAction`]. Flags, because they combine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CompletionActions(pub i32);

impl CompletionActions {
    pub const NONE: Self = Self(0);
    /// Show the finished file in Explorer.
    pub const OPEN_FOLDER: Self = Self(1);
    /// Play the system notification sound — for a download someone is waiting on from across the room.
    pub const PLAY_SOUND: Self = Self(2);

    pub fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0 && flag.0 != 0
    }
    pub fn with(self, flag: Self, on: bool) -> Self {
        if on { Self(self.0 | flag.0) } else { Self(self.0 & !flag.0) }
    }
}

impl std::ops::BitOr for CompletionActions {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// What the user can ask of one connection of a running download, from the
/// details window's connection list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConnectionCommand {
    /// Stop this connection and keep its chunk; its slot stays held for it.
    Pause,
    /// Carry a paused or failed connection on from what its chunk already holds.
    Resume,
    /// Throw this connection's chunk away and fetch its range again from the start.
    Restart,
}

/// State of a single chunk connection, as displayed in the details window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum ConnectionStatus {
    #[default]
    Idle = 0,
    SendingRequest = 1,
    ReceivingData = 2,
    Finished = 3,
    Failed = 4,
    Paused = 5,
}

/// The sidebar category a download belongs to, derived from its status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum DownloadCategory {
    #[default]
    All = 0,
    InProgress = 1,
    Queued = 2,
    Completed = 3,
    Failed = 4,
    Paused = 5,
}

/// Lifecycle states of a download.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum DownloadStatus {
    #[default]
    Queued = 0,
    Connecting = 1,
    Downloading = 2,
    Paused = 3,
    Merging = 4,
    Completed = 5,
    Failed = 6,
    Cancelled = 7,
    /// The file is assembled and being checked against the checksum the user gave.
    /// Appended rather than placed beside `Merging`: statuses are persisted as
    /// numbers, and moving one would re-read every stored download.
    Verifying = 8,
}

impl DownloadStatus {
    /// Connecting, Downloading, Merging or Verifying — a service is running.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Connecting | Self::Downloading | Self::Merging | Self::Verifying)
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Connecting => "Connecting",
            Self::Downloading => "Downloading",
            Self::Paused => "Paused",
            Self::Merging => "Merging",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
            Self::Verifying => "Verifying",
        }
    }
}

impl std::fmt::Display for DownloadStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What to do with the computer — or with QuickByte — once a download or a
/// queue is finished. Core only records the choice; doing it (behind a
/// countdown) belongs to the shell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum PowerAction {
    #[default]
    None = 0,
    ExitApp = 1,
    Sleep = 2,
    Hibernate = 3,
    ShutDown = 4,
}

impl PowerAction {
    pub const ALL: [PowerAction; 5] = [Self::None, Self::ExitApp, Self::Sleep, Self::Hibernate, Self::ShutDown];
}

/// What a queue is doing right now. Never persisted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum QueueState {
    /// Not running. Either drained, never started, or waiting for its schedule.
    #[default]
    Idle,
    /// A runner is walking the queue, starting downloads as slots free up.
    Running,
}

/// The days of the week a queue's schedule fires on. A single integer (bit
/// `1 << weekday`, Sunday = 0) because it is persisted state read by two
/// processes, and an integer cannot deserialize half-populated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScheduleDays(pub i32);

impl ScheduleDays {
    pub const NONE: Self = Self(0);
    pub const SUNDAY: Self = Self(1 << 0);
    pub const MONDAY: Self = Self(1 << 1);
    pub const TUESDAY: Self = Self(1 << 2);
    pub const WEDNESDAY: Self = Self(1 << 3);
    pub const THURSDAY: Self = Self(1 << 4);
    pub const FRIDAY: Self = Self(1 << 5);
    pub const SATURDAY: Self = Self(1 << 6);
    pub const WEEKDAYS: Self = Self(0b011_1110);
    pub const WEEKEND: Self = Self(0b100_0001);
    pub const EVERY_DAY: Self = Self(0b111_1111);

    /// The flag for a chrono weekday (`DayOfWeek` numbering: Sunday = 0).
    pub fn from_weekday(day: chrono::Weekday) -> Self {
        Self(1 << day.num_days_from_sunday())
    }
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn with(self, flag: Self, on: bool) -> Self {
        if on { Self(self.0 | flag.0) } else { Self(self.0 & !flag.0) }
    }
}

impl std::ops::BitOr for ScheduleDays {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl std::ops::BitAnd for ScheduleDays {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}
