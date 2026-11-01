// SPDX-License-Identifier: GPL-3.0-or-later
//! Page bookmarks (DESIGN.md §9): the header's bookmark menu toggles the current page's
//! bookmark (also Ctrl+D) and lists the bookmarked pages to jump to. Bookmarks are saved in
//! the note's manifest and become the outline of an exported PDF.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

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
        let go_to = gio::ActionEntry::builder("go-to-page")
            .parameter_type(Some(glib::VariantTy::UINT32))
            .activate(|win: &Self, _, param| {
                if let (Some(canvas), Some(index)) =
                    (win.canvas(), param.and_then(|p| p.get::<u32>()))
                {
                    canvas.show_page(index as usize);
                }
            })
            .build();
        self.add_action_entries([bookmark, go_to]);

        // The list is built each time the menu opens, so it never goes stale.
        self.imp()
            .bookmarks_button
            .set_create_popup_func(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |button| button.set_menu_model(Some(&win.bookmarks_menu()))
            ));
    }

    fn bookmarks_menu(&self) -> gio::Menu {
        let menu = gio::Menu::new();
        let toggle = gio::Menu::new();
        toggle.append(Some("_Bookmark this page"), Some("win.bookmark"));
        menu.append_section(None, &toggle);

        let list = gio::Menu::new();
        let marks = self.canvas().map(|c| c.bookmarks()).unwrap_or_default();
        if marks.is_empty() {
            // No action: shown greyed out.
            list.append(Some("No bookmarks yet"), None);
        }
        for (index, name) in marks {
            let number = index + 1;
            let label = if name.is_empty() {
                format!("Page {number}")
            } else {
                format!("{name} (page {number})")
            };
            let item = gio::MenuItem::new(Some(&label), None);
            item.set_action_and_target_value(
                Some("win.go-to-page"),
                Some(&(index as u32).to_variant()),
            );
            list.append_item(&item);
        }
        menu.append_section(Some("Bookmarks"), &list);
        menu
    }

    /// Shows whether the current page is bookmarked (check in the menu, filled button).
    pub(super) fn sync_bookmark(&self) {
        let canvas = self.canvas();
        let on = canvas.as_ref().is_some_and(|c| c.is_bookmarked());
        self.change_action_state("bookmark", &on.to_variant());
        let button = &self.imp().bookmarks_button;
        button.set_sensitive(canvas.is_some());
        if on {
            button.add_css_class("bookmarked");
        } else {
            button.remove_css_class("bookmarked");
        }
    }
}
