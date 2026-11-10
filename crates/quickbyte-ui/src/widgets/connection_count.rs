//! The "how many connections" picker, shared by Add Download and Options so the
//! two cannot drift apart — port of `Controls/ConnectionCountBox.cs`.
//!
//! A fixed list rather than a 1–32 spinner: a spinner presents 32 answers and no
//! opinion about any of them, and the values it offers that this does not — 3,
//! 5, 13 — are not meaningfully different from their neighbours.
//! `DownloadSettings::CONNECTION_CHOICES` holds the list.

use egui::{ComboBox, Response, Ui};
use quickbyte_core::models::DownloadSettings;

/// The value the box shows for a stored count. A number written by an older
/// build — or by hand into settings.json — snaps to the nearest listed value at
/// or below it, so the box can never show "nothing selected" (which a WinForms
/// combo read back as one connection, silently, for every download).
pub fn normalize(stored: i32) -> i32 {
    DownloadSettings::nearest_connection_choice(stored)
}

/// The drop-down. `value` is normalised first, then edited in place.
pub fn show(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    value: &mut i32,
) -> Response {
    show_width(ui, id_salt, value, 56.0)
}

/// [`show`] at a given width (Options lines it up with its spinners).
pub fn show_width(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    value: &mut i32,
    width: f32,
) -> Response {
    *value = normalize(*value);
    let mut changed = false;
    let mut response = ComboBox::from_id_salt(id_salt)
        .width(width)
        .selected_text(value.to_string())
        .show_ui(ui, |ui| {
            for choice in DownloadSettings::CONNECTION_CHOICES {
                if ui
                    .selectable_value(value, choice, choice.to_string())
                    .changed()
                {
                    changed = true;
                }
            }
        })
        .response;
    if changed {
        response.mark_changed();
    }
    response
}

#[cfg(test)]
mod tests {
    //! Port of `ConnectionCountBoxTests.cs`.
    use super::*;

    #[test]
    fn every_listed_value_round_trips() {
        for choice in DownloadSettings::CONNECTION_CHOICES {
            assert_eq!(normalize(choice), choice);
        }
    }

    #[test]
    fn a_fresh_box_offers_the_default_rather_than_the_first_entry() {
        assert_eq!(
            normalize(DownloadSettings::DEFAULT_CONNECTIONS),
            DownloadSettings::DEFAULT_CONNECTIONS
        );
    }

    #[test]
    fn a_number_from_the_old_spinner_snaps_down_to_a_listed_one() {
        for (stored, expected) in [(5, 4), (3, 2), (31, 24), (12, 8)] {
            assert_eq!(normalize(stored), expected);
        }
    }

    #[test]
    fn a_value_off_either_end_still_leaves_something_selected() {
        for stored in [0, -7, 9999] {
            assert!(DownloadSettings::CONNECTION_CHOICES.contains(&normalize(stored)));
        }
    }
}
