// SPDX-License-Identifier: GPL-3.0-or-later
//! Page bookmarks (DESIGN.md §9): Ctrl+D toggles the current page's bookmark; the page
//! sorter in the sidebar has a toggle on each thumbnail and can show only bookmarked pages.
//! Bookmarks are saved in the note's manifest and become the outline of an exported PDF.

use adw::prelude::*;
use gtk::gio;

use super::CeraWindow;

impl CeraWindow {
    pub(super) fn setup_bookmarks(&self) {
        let bookmark = gio::ActionEntry::builder("bookmark")
            .state(false.to_variant())
            .activate(|win: &Self, action, _| {
                let on = !action
                    .state()
                    .and_then(|s| s.get::<bool>())
                    .unwrap_or(false);
                if let Some(canvas) = win.canvas() {
                    canvas.set_bookmarked(on);
                }
                win.sync_bookmark();
            })
            .build();
        self.add_action_entries([bookmark]);
    }

    /// Keeps the bookmark action's state on the current page.
    pub(super) fn sync_bookmark(&self) {
        let on = self.canvas().is_some_and(|c| c.is_bookmarked());
        self.change_action_state("bookmark", &on.to_variant());
    }
}
