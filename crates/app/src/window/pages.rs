// SPDX-License-Identifier: GPL-3.0-or-later
//! The page sorter (DESIGN.md §9), a view of the sidebar: page thumbnails of the current tab's
//! note to jump to, reorder, insert blank pages, delete them or change their paper. Every
//! change goes through the note's history, so it undoes like any other edit.
//!
//! The Pages toggle next to the sidebar button (Ctrl+P) switches the sidebar to the pages,
//! and a back button returns. Tapping a thumbnail shows that page; scrolling the note moves the
//! selection. A right click or a hold on a thumbnail opens its context menu. Each thumbnail
//! has a small bookmark toggle in its corner, and a header toggle shows only the bookmarked
//! pages (a `FilterListModel` over all of them; the grid's positions are then not page
//! numbers, so pages are always found by id).
//! A selection toggle in each thumbnail's other corner selects pages (a set of ids, apart from
//! the grid's own single selection, which is the current page). While any are selected the
//! bottom bar holds the selection tools instead: select all, invert, and select the pages
//! before or after the page last selected. Only pages shown can be selected. The context menu
//! of a selected page acts on all of them while several are: paper, move, bookmark, delete.
//! The sorter is bound to the selected tab while the view is on screen and dropped, with its
//! thumbnails, when the view is hidden or the sidebar closes.
//!
//! Thumbnails are whole-page renders from the window's render threads under a document id
//! of their own, asked for only for the cells on screen and kept in a small LRU. A thumbnail
//! remembers the page's ink generation and spec it was drawn from, and is drawn again once
//! the note is edited (a short while after the last edit, not per stroke).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use adw::subclass::prelude::*;
use cera_core::{Background, PageId, PageSpec, Paper, PaperKind};
use cera_render::tile::{DocId, Layer, Scale, TileKey, WHOLE_PAGE_PX};
use cera_render::worker::{JobSource, Renderer, TileJob, TileResult};
use gtk::{gdk, gio, glib};

use super::CeraWindow;
use crate::canvas::Canvas;
use crate::note::SharedNote;

/// Thumbnail width in logical pixels.
const THUMB_WIDTH: f64 = 110.0;
/// Memory for thumbnail textures; about 50 A4 thumbnails at 2× scale.
const THUMB_BUDGET: usize = 16 << 20;
/// Thumbnails are brought up to date this long after the last edit.
const REFRESH_DELAY: Duration = Duration::from_millis(400);

#[cfg(target_endian = "little")]
const PIXEL_FORMAT: gdk::MemoryFormat = gdk::MemoryFormat::B8g8r8a8Premultiplied;
#[cfg(target_endian = "big")]
const PIXEL_FORMAT: gdk::MemoryFormat = gdk::MemoryFormat::A8r8g8b8Premultiplied;

/// What a thumbnail was drawn from: the page's ink generation and its spec.
type Version = (u64, PageSpec);

/// The sidebar view's widgets; built once per window.
pub(super) struct PagesPanel {
    stack: gtk::Stack,
    grid: gtk::GridView,
    actions: gio::SimpleActionGroup,
    /// Show only bookmarked pages; kept across notes.
    only_marks: Rc<Cell<bool>>,
    title: adw::WindowTitle,
    /// The page actions, or the selection tools while pages are selected.
    bars: gtk::Stack,
}

/// A cell on screen.
struct BoundCell {
    picture: gtk::Picture,
    mark: gtk::Button,
    check: gtk::Button,
}

pub(super) struct Sorter {
    me: std::rc::Weak<Sorter>,
    canvas: Canvas,
    note: SharedNote,
    renderer: Rc<Renderer>,
    doc: DocId,
    device_scale: f64,
    /// Paper for inserted pages.
    paper: Paper,
    store: gio::ListStore,
    filter: gtk::CustomFilter,
    filtered: gtk::FilterListModel,
    only_marks: Rc<Cell<bool>>,
    selection: gtk::SingleSelection,
    /// Pages selected with their toggles, and the one selected last.
    checked: RefCell<HashSet<PageId>>,
    anchor: Cell<Option<PageId>>,
    thumbs: RefCell<HashMap<PageId, (gdk::Texture, usize, Version)>>,
    /// Asked-for thumbnails and what they will be drawn from.
    pending: RefCell<HashMap<PageId, Version>>,
    lru: RefCell<VecDeque<PageId>>,
    thumb_bytes: Cell<usize>,
    /// Cells on screen, by the page they show.
    bound: RefCell<HashMap<PageId, BoundCell>>,
    requested: Cell<bool>,
    refresh: RefCell<Option<glib::SourceId>>,
    /// Set while the sorter itself moves the note or the selection, so neither follows the
    /// other back.
    syncing: Cell<bool>,
    handlers: RefCell<Vec<glib::SignalHandlerId>>,
}

impl CeraWindow {
    /// Builds the sidebar's pages view and adds it next to the files.
    pub(super) fn setup_pages(&self) {
        let imp = self.imp();
        let grid = gtk::GridView::builder()
            .min_columns(1)
            .max_columns(4)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&grid)
            .build();
        let empty = adw::StatusPage::builder()
            .icon_name("view-grid-symbolic")
            .title("No note open")
            .css_classes(["compact"])
            .build();
        let no_marks = adw::StatusPage::builder()
            .icon_name("user-bookmarks-symbolic")
            .title("No bookmarks")
            .description("Bookmark a page with the button on its thumbnail")
            .css_classes(["compact"])
            .build();
        let stack = gtk::Stack::new();
        stack.add_named(&scroller, Some("grid"));
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&no_marks, Some("no-marks"));
        let only_marks = Rc::new(Cell::new(false));
        let filter_button = gtk::ToggleButton::builder()
            .icon_name("user-bookmarks-symbolic")
            .tooltip_text("Show only bookmarked pages")
            .build();
        filter_button.connect_toggled(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[strong]
            only_marks,
            move |b| {
                only_marks.set(b.is_active());
                let sorter = win.imp().sorter.borrow().clone();
                if let Some(sorter) = sorter {
                    sorter.filter.changed(gtk::FilterChange::Different);
                    sorter.prune_checks();
                    sorter.follow_page(true);
                    win.update_pages_view();
                }
            }
        ));

        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Files")
            .action_name("win.pages")
            .build();
        let title = adw::WindowTitle::new("Pages", "");
        let header = adw::HeaderBar::builder().title_widget(&title).build();
        header.pack_start(&back);
        header.pack_end(&filter_button);
        let bars = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        bars.add_named(&action_bar(), Some("actions"));
        bars.add_named(&selection_bar(), Some("selection"));

        let root = adw::ToolbarView::new();
        root.add_top_bar(&header);
        root.add_bottom_bar(&bars);
        root.set_content(Some(&stack));
        let actions = self.pages_actions();
        root.insert_action_group("pages", Some(&actions));
        imp.sidebar_stack.add_named(&root, Some("pages"));

        let _ = imp.pages.set(PagesPanel {
            stack,
            grid,
            actions,
            only_marks,
            title,
            bars,
        });

        let pages = gio::ActionEntry::builder("pages")
            .state(false.to_variant())
            .activate(|win: &Self, action, _| {
                let on = action.state().and_then(|s| s.get::<bool>()) != Some(true);
                action.set_state(&on.to_variant());
                if on {
                    win.imp().split_view.set_show_sidebar(true);
                }
                win.sync_pages();
            })
            .build();
        self.add_action_entries([pages]);
        // Hiding the sidebar leaves the pages too; it comes back on the files.
        imp.split_view.connect_show_sidebar_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |split| {
                if !split.shows_sidebar() {
                    win.change_action_state("pages", &false.to_variant());
                }
                win.sync_pages();
            }
        ));
    }

    fn pages_shown(&self) -> bool {
        self.action_state("pages").and_then(|s| s.get::<bool>()) == Some(true)
    }

    /// Shows the files or the pages in the sidebar, and binds the sorter to the selected
    /// tab's note while the pages are on screen (or drops it).
    pub(super) fn sync_pages(&self) {
        let imp = self.imp();
        let Some(panel) = imp.pages.get() else { return };
        let shown = self.pages_shown();
        imp.sidebar_stack
            .set_visible_child_name(if shown { "pages" } else { "files" });
        let canvas = (shown && imp.split_view.shows_sidebar())
            .then(|| self.canvas())
            .flatten()
            .filter(|c| c.note().is_some());
        let current = imp.sorter.borrow().as_ref().map(|s| s.canvas.clone());
        if current == canvas {
            self.update_pages_actions();
            return;
        }
        if let Some(old) = imp.sorter.take() {
            self.drop_sorter(&old);
        }
        panel.grid.set_model(None::<&gtk::SelectionModel>);
        panel.grid.set_factory(None::<&gtk::ListItemFactory>);
        match canvas {
            Some(canvas) => {
                let sorter = self.new_sorter(canvas);
                panel.grid.set_factory(Some(&sorter.factory()));
                panel.grid.set_model(Some(&sorter.selection));
                *imp.sorter.borrow_mut() = Some(sorter.clone());
                sorter.follow_page(true);
            }
            None => panel.stack.set_visible_child_name("empty"),
        }
        self.update_pages_view();
        self.update_pages_actions();
    }

    /// The grid, or a status page when there is no note or (filtered) no bookmark.
    fn update_pages_view(&self) {
        let imp = self.imp();
        let Some(panel) = imp.pages.get() else { return };
        let name = match imp.sorter.borrow().as_ref() {
            None => "empty",
            Some(s) if s.filtered.n_items() == 0 && panel.only_marks.get() => "no-marks",
            Some(_) => "grid",
        };
        panel.stack.set_visible_child_name(name);
        self.update_pages_actions();
    }

    fn new_sorter(&self, canvas: Canvas) -> Rc<Sorter> {
        let imp = self.imp();
        let note = canvas.note().expect("checked by the caller");
        let doc = imp.next_doc.get();
        imp.next_doc.set(doc + 1);
        let renderer = self.renderer();
        {
            let n = note.borrow();
            renderer.open_doc(
                doc,
                n.file
                    .assets()
                    .filter_map(|a| Some((a.id, n.file.asset(a.id)?))),
            );
        }
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let only_marks = imp.pages.get().expect("set up").only_marks.clone();
        let filter = {
            let note = Rc::downgrade(&note);
            let only_marks = only_marks.clone();
            gtk::CustomFilter::new(move |item| {
                if !only_marks.get() {
                    return true;
                }
                let (Some(note), Some(id)) = (
                    note.upgrade(),
                    item.downcast_ref::<glib::BoxedAnyObject>()
                        .map(|b| *b.borrow::<PageId>()),
                ) else {
                    return true;
                };
                let note = note.borrow();
                note.doc
                    .page_index(id)
                    .and_then(|i| note.doc.page(i).ok())
                    .is_some_and(|p| p.bookmark().is_some())
            })
        };
        let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
        let selection = gtk::SingleSelection::new(Some(filtered.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let sorter = Rc::new_cyclic(|me| Sorter {
            me: me.clone(),
            canvas: canvas.clone(),
            note,
            renderer,
            doc,
            device_scale: f64::from(self.scale_factor()),
            paper: self.new_page_paper(),
            store,
            filter,
            filtered,
            only_marks,
            selection,
            checked: RefCell::default(),
            anchor: Cell::new(None),
            thumbs: RefCell::default(),
            pending: RefCell::default(),
            lru: RefCell::default(),
            thumb_bytes: Cell::new(0),
            bound: RefCell::default(),
            requested: Cell::new(false),
            refresh: RefCell::default(),
            syncing: Cell::new(false),
            handlers: RefCell::default(),
        });
        let weak = Rc::downgrade(&sorter);
        imp.sinks.borrow_mut().insert(
            doc,
            Rc::new(move |result| {
                if let Some(sorter) = weak.upgrade() {
                    sorter.thumbnail_ready(result);
                }
            }),
        );
        sorter.reload();

        // A tap on a thumbnail (or the arrow keys) shows that page.
        let weak = Rc::downgrade(&sorter);
        sorter.selection.connect_selected_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                win.update_pages_actions();
                let Some(sorter) = weak.upgrade() else { return };
                if sorter.syncing.get() {
                    return;
                }
                if let Some(index) = sorter.selected()
                    && index as u32 != sorter.canvas.current_page()
                {
                    sorter.syncing.set(true);
                    sorter.canvas.show_page(index);
                    sorter.syncing.set(false);
                }
            }
        ));
        let weak = Rc::downgrade(&sorter);
        let changed = canvas.connect_local("changed", false, move |_| {
            if let Some(sorter) = weak.upgrade() {
                sorter.refresh_soon();
            }
            None
        });
        let weak = Rc::downgrade(&sorter);
        let page = canvas.connect_notify_local(Some("current-page"), move |_, _| {
            if let Some(sorter) = weak.upgrade() {
                sorter.follow_page(false);
            }
        });
        sorter.handlers.borrow_mut().extend([changed, page]);
        sorter
    }

    fn drop_sorter(&self, sorter: &Sorter) {
        self.imp().sinks.borrow_mut().remove(&sorter.doc);
        sorter.renderer.close_doc(sorter.doc);
        if let Some(source) = sorter.refresh.take() {
            source.remove();
        }
        for handler in sorter.handlers.take() {
            sorter.canvas.disconnect(handler);
        }
        tracing::debug!(
            bytes = sorter.thumb_bytes.get(),
            "page sorter released its thumbnails"
        );
    }

    fn pages_actions(&self) -> gio::SimpleActionGroup {
        let group = gio::SimpleActionGroup::new();
        let with_sorter = |name: &str, f: fn(&Sorter, Option<String>)| {
            let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
            action.connect_activate(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |_, param| {
                    let sorter = win.imp().sorter.borrow().clone();
                    if let Some(sorter) = sorter {
                        f(&sorter, param.and_then(|p| p.get::<String>()));
                    }
                }
            ));
            group.add_action(&action);
        };
        with_sorter("insert", |s, p| s.insert(p.as_deref() == Some("after")));
        with_sorter("paper", |s, p| {
            if let Some(index) = s.selected() {
                s.set_paper_of(&[index], paper_kind(p));
            }
        });
        with_sorter("move", |s, p| {
            s.move_by(p.and_then(|p| p.parse::<i64>().ok()).unwrap_or(0))
        });
        with_sorter("delete", |s, _| s.delete());
        with_sorter("go", |s, p| s.select_end(p.as_deref() == Some("last")));
        with_sorter("bookmark", |s, _| s.toggle_bookmark());
        with_sorter("select", |s, p| s.select(p.as_deref()));
        // The same, for the selected pages.
        with_sorter("selected-paper", |s, p| {
            s.set_paper_of(&s.checked_indices(), paper_kind(p))
        });
        with_sorter("selected-move", |s, p| {
            s.move_checked(p.and_then(|p| p.parse::<i64>().ok()).unwrap_or(0))
        });
        with_sorter("selected-bookmark", |s, p| {
            s.bookmark_checked(p.as_deref() == Some("on"))
        });
        with_sorter("selected-delete", |s, _| s.delete_checked());
        group
    }

    fn update_pages_actions(&self) {
        let Some(panel) = self.imp().pages.get() else {
            return;
        };
        let (any, several, shown, checked, count) =
            self.imp()
                .sorter
                .borrow()
                .as_ref()
                .map_or((false, false, false, 0, 0), |s| {
                    (
                        s.selected().is_some(),
                        s.store.n_items() > 1,
                        s.filtered.n_items() > 0,
                        s.checked.borrow().len(),
                        s.store.n_items() as usize,
                    )
                });
        panel
            .bars
            .set_visible_child_name(if checked > 0 { "selection" } else { "actions" });
        panel.title.set_subtitle(&if checked > 0 {
            format!("{checked} selected")
        } else {
            String::new()
        });
        for (name, enabled) in [
            ("insert", any),
            ("bookmark", any),
            ("paper", any),
            ("move", any && several),
            ("delete", any && several),
            ("go", shown),
            ("select", shown),
            ("selected-paper", checked > 0),
            ("selected-bookmark", checked > 0),
            ("selected-move", checked > 0 && several),
            // A note keeps at least one page.
            ("selected-delete", checked > 0 && checked < count),
        ] {
            if let Some(action) = panel
                .actions
                .lookup_action(name)
                .and_downcast::<gio::SimpleAction>()
            {
                action.set_enabled(enabled);
            }
        }
    }

    /// Paper for new pages: the vault's choice for new notes.
    pub(super) fn new_page_paper(&self) -> Paper {
        self.imp()
            .vault
            .borrow()
            .as_ref()
            .map(|v| v.config.new_note.page_spec().paper)
            .unwrap_or_else(|| crate::vault::NewNote::default().page_spec().paper)
    }
}

/// Insert, paper, move and delete, for the selected page.
fn action_bar() -> gtk::CenterBox {
    let insert_menu = gio::Menu::new();
    insert_menu.append(Some("Insert Page _Before"), Some("pages.insert::before"));
    insert_menu.append(Some("Insert Page _After"), Some("pages.insert::after"));
    let left = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    left.append(
        &gtk::MenuButton::builder()
            .icon_name("cera-page-add-symbolic")
            .tooltip_text("Insert page")
            .menu_model(&insert_menu)
            .build(),
    );
    left.append(
        &gtk::MenuButton::builder()
            .icon_name("view-grid-symbolic")
            .tooltip_text("Paper")
            .menu_model(&paper_menu("paper"))
            .build(),
    );
    let middle = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    middle.append(&bar_button(
        "go-top-symbolic",
        "First page",
        "pages.go::first",
    ));
    middle.append(&bar_button(
        "go-bottom-symbolic",
        "Last page",
        "pages.go::last",
    ));
    let delete = bar_button("user-trash-symbolic", "Delete page", "pages.delete::");
    delete.add_css_class("destructive-action");
    bar(&left, &middle, &delete)
}

/// Select all, invert, select before or after the page selected last, and clear.
fn selection_bar() -> gtk::CenterBox {
    let left = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    left.append(&bar_button(
        "cera-select-all-symbolic",
        "Select all",
        "pages.select::all",
    ));
    left.append(&bar_button(
        "cera-select-invert-symbolic",
        "Invert selection",
        "pages.select::invert",
    ));
    let middle = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    middle.append(&bar_button(
        "cera-select-before-symbolic",
        "Select pages before",
        "pages.select::before",
    ));
    middle.append(&bar_button(
        "cera-select-after-symbolic",
        "Select pages after",
        "pages.select::after",
    ));
    let clear = bar_button(
        "window-close-symbolic",
        "Clear selection",
        "pages.select::none",
    );
    bar(&left, &middle, &clear)
}

/// A button of the bottom bar. Detailed names ("pages.move::1") set the action's target too.
fn bar_button(icon: &str, tooltip: &str, action: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .build();
    button.set_detailed_action_name(action);
    button
}

fn bar(
    start: &impl IsA<gtk::Widget>,
    center: &impl IsA<gtk::Widget>,
    end: &impl IsA<gtk::Widget>,
) -> gtk::CenterBox {
    let bar = gtk::CenterBox::new();
    bar.set_start_widget(Some(start));
    bar.set_center_widget(Some(center));
    bar.set_end_widget(Some(end));
    bar.set_margin_top(6);
    bar.set_margin_bottom(6);
    bar.set_margin_start(6);
    bar.set_margin_end(6);
    bar
}

fn paper_kind(name: Option<String>) -> PaperKind {
    match name.as_deref() {
        Some("ruled") => PaperKind::Ruled,
        Some("grid") => PaperKind::Grid,
        Some("dots") => PaperKind::Dots,
        _ => PaperKind::Blank,
    }
}

/// Where the pages at `selected` go when moved one step earlier (`by < 0`) or later: each
/// trades places with the unselected neighbour on that side, so a block moves as one and pages
/// already at that end stay. Returns the moves, `(from, to)`, in the order to make them.
fn step_moves(selected: &[usize], count: usize, by: i64) -> Vec<(usize, usize)> {
    let mut sel = vec![false; count];
    for &i in selected.iter().filter(|&&i| i < count) {
        sel[i] = true;
    }
    let mut moves = Vec::new();
    if by < 0 {
        for i in 1..count {
            if sel[i] && !sel[i - 1] {
                sel.swap(i, i - 1);
                moves.push((i, i - 1));
            }
        }
    } else if by > 0 {
        for i in (0..count.saturating_sub(1)).rev() {
            if sel[i] && !sel[i + 1] {
                sel.swap(i, i + 1);
                moves.push((i, i + 1));
            }
        }
    }
    moves
}

/// A blank page the size of `spec`: its own paper, or `paper` if it shows a PDF.
pub(super) fn blank_like(spec: &PageSpec, paper: Paper) -> PageSpec {
    match spec.background {
        Background::Pdf { .. } => PageSpec::new((spec.width, spec.height), paper),
        Background::None => PageSpec {
            background: Background::None,
            ..*spec
        },
    }
}

impl Sorter {
    fn factory(self: &Rc<Self>) -> gtk::SignalListItemFactory {
        let factory = gtk::SignalListItemFactory::new();
        let weak = Rc::downgrade(self);
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            let picture = gtk::Picture::builder()
                .content_fit(gtk::ContentFit::Contain)
                .can_shrink(true)
                .css_classes(["page-thumb"])
                .build();
            // A small bookmark toggle in the thumbnail's top right corner.
            let mark = gtk::Button::builder()
                .icon_name("user-bookmarks-symbolic")
                .halign(gtk::Align::End)
                .valign(gtk::Align::Start)
                .css_classes(["flat", "circular", "thumb-mark"])
                .build();
            mark.connect_clicked(glib::clone!(
                #[weak]
                item,
                #[strong]
                weak,
                move |_| {
                    if let (Some(sorter), Some(id)) = (weak.upgrade(), page_of(&item)) {
                        sorter.toggle_bookmark_of(id);
                    }
                }
            ));
            // And a selection toggle in the top left corner.
            let check = gtk::Button::builder()
                .icon_name("object-select-symbolic")
                .halign(gtk::Align::Start)
                .valign(gtk::Align::Start)
                .css_classes(["flat", "circular", "thumb-check"])
                .build();
            check.connect_clicked(glib::clone!(
                #[weak]
                item,
                #[strong]
                weak,
                move |_| {
                    if let (Some(sorter), Some(id)) = (weak.upgrade(), page_of(&item)) {
                        sorter.toggle_check(id);
                    }
                }
            ));
            let thumb = gtk::Overlay::builder()
                .child(&picture)
                .halign(gtk::Align::Center)
                .build();
            thumb.add_overlay(&mark);
            thumb.add_overlay(&check);
            let label = gtk::Label::builder().css_classes(["caption"]).build();
            let cell = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .margin_top(6)
                .margin_bottom(6)
                .build();
            cell.append(&thumb);
            cell.append(&label);
            item.set_child(Some(&cell));
            add_menu_gestures(item, &cell);
        });
        let weak = Rc::downgrade(self);
        factory.connect_bind(move |_, item| {
            let Some(sorter) = weak.upgrade() else { return };
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            let Some(id) = page_of(item) else { return };
            let Some(cell) = item.child() else { return };
            let thumb = cell
                .first_child()
                .and_downcast::<gtk::Overlay>()
                .expect("thumbnail");
            let picture = thumb
                .child()
                .and_downcast::<gtk::Picture>()
                .expect("picture");
            let mark = picture
                .next_sibling()
                .and_downcast::<gtk::Button>()
                .expect("bookmark toggle");
            let check = mark
                .next_sibling()
                .and_downcast::<gtk::Button>()
                .expect("selection toggle");
            let label = thumb
                .next_sibling()
                .and_downcast::<gtk::Label>()
                .expect("label");
            let index = sorter.note.borrow().doc.page_index(id).unwrap_or(0);
            label.set_label(&(index + 1).to_string());
            show_mark(&mark, sorter.is_bookmarked(id));
            show_check(&check, sorter.checked.borrow().contains(&id));
            sorter.bind(
                id,
                BoundCell {
                    picture,
                    mark,
                    check,
                },
            );
        });
        let weak = Rc::downgrade(self);
        factory.connect_unbind(move |_, item| {
            let Some(sorter) = weak.upgrade() else { return };
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            let Some(id) = page_of(item) else { return };
            // Only this cell's entry: when the store is refilled, another cell may already
            // have bound the same page (a new item for it) before this one lets go.
            let picture = item
                .child()
                .and_then(|c| c.first_child())
                .and_downcast::<gtk::Overlay>()
                .and_then(|t| t.child());
            let mut bound = sorter.bound.borrow_mut();
            if bound
                .get(&id)
                .is_some_and(|c| picture.as_ref() == Some(c.picture.upcast_ref()))
            {
                bound.remove(&id);
            }
        });
        factory
    }

    fn is_bookmarked(&self, id: PageId) -> bool {
        let note = self.note.borrow();
        note.doc
            .page_index(id)
            .and_then(|i| note.doc.page(i).ok())
            .is_some_and(|p| p.bookmark().is_some())
    }

    /// The selected page's index in the note.
    fn selected(&self) -> Option<usize> {
        let id = self
            .selection
            .selected_item()
            .and_downcast::<glib::BoxedAnyObject>()
            .map(|b| *b.borrow::<PageId>())?;
        self.note.borrow().doc.page_index(id)
    }

    /// Where page `index` is in the grid, if it is shown.
    fn position_of(&self, index: usize) -> Option<u32> {
        let id = self.note.borrow().doc.page(index).ok()?.id();
        if !self.only_marks.get() {
            return Some(index as u32);
        }
        (0..self.filtered.n_items()).find(|&i| {
            self.filtered
                .item(i)
                .and_downcast::<glib::BoxedAnyObject>()
                .is_some_and(|b| *b.borrow::<PageId>() == id)
        })
    }

    /// Selects page `index` (nothing if it is filtered out), which shows it in the note.
    fn select_page(&self, index: usize) {
        let index = index.min((self.store.n_items() as usize).saturating_sub(1));
        self.selection.set_selected(
            self.position_of(index)
                .unwrap_or(gtk::INVALID_LIST_POSITION),
        );
    }

    /// Selects page `index` without moving the note.
    fn select_quietly(&self, index: u32) {
        self.syncing.set(true);
        self.select_page(index as usize);
        self.syncing.set(false);
    }

    /// The selection follows the note's current page, scrolled into view.
    fn follow_page(&self, initial: bool) {
        if self.syncing.get() {
            return;
        }
        let page = self.canvas.current_page();
        if !initial && self.selected() == Some(page as usize) {
            return;
        }
        self.select_quietly(page);
        if let Some(grid) = self
            .canvas
            .root()
            .and_downcast::<CeraWindow>()
            .and_then(|w| w.imp().pages.get().map(|p| p.grid.clone()))
            && let Some(position) = self.position_of(page as usize)
        {
            grid.scroll_to(position, gtk::ListScrollFlags::NONE, None);
        }
    }

    /// Fills the grid from the note, if its pages changed, and selects the current page.
    fn reload(&self) {
        let ids: Vec<PageId> = self
            .note
            .borrow()
            .doc
            .pages()
            .iter()
            .map(|p| p.id())
            .collect();
        let same = ids.len() == self.store.n_items() as usize
            && ids.iter().enumerate().all(|(i, id)| {
                self.store
                    .item(i as u32)
                    .and_downcast::<glib::BoxedAnyObject>()
                    .is_some_and(|b| *b.borrow::<PageId>() == *id)
            });
        if !same {
            let items: Vec<glib::BoxedAnyObject> =
                ids.into_iter().map(glib::BoxedAnyObject::new).collect();
            self.syncing.set(true);
            self.store.splice(0, self.store.n_items(), &items);
            self.syncing.set(false);
            self.prune_checks();
        }
        self.select_quietly(self.canvas.current_page());
    }

    /// After an edit: once editing pauses, the page list, bookmarks and stale thumbnails.
    fn refresh_soon(&self) {
        if let Some(source) = self.refresh.take() {
            source.remove();
        }
        let me = self.me.clone();
        let source = glib::timeout_add_local_once(REFRESH_DELAY, move || {
            let Some(sorter) = me.upgrade() else { return };
            sorter.refresh.take();
            sorter.reload();
            sorter.marks_changed();
            sorter.send_jobs();
        });
        *self.refresh.borrow_mut() = Some(source);
    }

    /// The page's ink generation and spec now.
    fn version(&self, index: usize) -> Option<Version> {
        let note = self.note.borrow();
        let page = note.doc.page(index).ok()?;
        Some((self.canvas.ink_generation(page.id()), *page.spec()))
    }

    fn selected_bookmarked(&self) -> bool {
        let Some(index) = self.selected() else {
            return false;
        };
        self.note
            .borrow()
            .doc
            .page(index)
            .is_ok_and(|p| p.bookmark().is_some())
    }

    /// Selects the first or last page shown (only bookmarked ones while filtered), which
    /// shows it in the note, and scrolls its thumbnail into view.
    fn select_end(&self, last: bool) {
        let count = self.filtered.n_items();
        if count == 0 {
            return;
        }
        let position = if last { count - 1 } else { 0 };
        self.selection.set_selected(position);
        if let Some(grid) = self
            .canvas
            .root()
            .and_downcast::<CeraWindow>()
            .and_then(|w| w.imp().pages.get().map(|p| p.grid.clone()))
        {
            grid.scroll_to(position, gtk::ListScrollFlags::NONE, None);
        }
    }

    fn toggle_bookmark(&self) {
        if let Some(index) = self.selected() {
            let on = !self.selected_bookmarked();
            self.canvas.set_page_bookmarked(index, on);
            self.marks_changed();
        }
    }

    fn toggle_bookmark_of(&self, id: PageId) {
        let Some(index) = self.note.borrow().doc.page_index(id) else {
            return;
        };
        self.canvas
            .set_page_bookmarked(index, !self.is_bookmarked(id));
        self.marks_changed();
    }

    /// After bookmarks changed: the toggles on screen, and the filter if it is on.
    fn marks_changed(&self) {
        for (&id, cell) in self.bound.borrow().iter() {
            show_mark(&cell.mark, self.is_bookmarked(id));
        }
        if self.only_marks.get() {
            self.filter.changed(gtk::FilterChange::Different);
            self.prune_checks();
            self.follow_page(true);
        }
        if let Some(win) = self.canvas.root().and_downcast::<CeraWindow>() {
            win.update_pages_view();
        }
    }

    /// The pages shown, in order.
    fn shown(&self) -> Vec<PageId> {
        (0..self.filtered.n_items())
            .filter_map(|i| {
                self.filtered
                    .item(i)
                    .and_downcast::<glib::BoxedAnyObject>()
                    .map(|b| *b.borrow::<PageId>())
            })
            .collect()
    }

    fn toggle_check(&self, id: PageId) {
        let on = !self.checked.borrow_mut().remove(&id);
        if on {
            self.checked.borrow_mut().insert(id);
            self.anchor.set(Some(id));
        }
        self.checks_changed();
    }

    /// Selects all pages shown, inverts the selection, or adds the pages before or after the
    /// one selected last (if that one was let go, before the last selected page or after the
    /// first); anything else clears it.
    fn select(&self, how: Option<&str>) {
        let shown = self.shown();
        {
            let mut checked = self.checked.borrow_mut();
            match how {
                Some("all") => checked.extend(&shown),
                Some("invert") => {
                    for id in shown {
                        if !checked.remove(&id) {
                            checked.insert(id);
                        }
                    }
                }
                Some(side @ ("before" | "after")) => {
                    let after = side == "after";
                    let index: HashMap<PageId, usize> =
                        shown.iter().enumerate().map(|(i, &id)| (id, i)).collect();
                    let anchor = self
                        .anchor
                        .get()
                        .filter(|a| checked.contains(a))
                        .and_then(|a| index.get(&a).copied())
                        .or_else(|| {
                            let at = checked.iter().filter_map(|id| index.get(id).copied());
                            if after { at.min() } else { at.max() }
                        });
                    let Some(anchor) = anchor else { return };
                    let range = if after {
                        &shown[anchor..]
                    } else {
                        &shown[..=anchor]
                    };
                    checked.extend(range);
                }
                _ => checked.clear(),
            }
        }
        self.checks_changed();
    }

    /// Lets go of selected pages that are no longer shown: deleted, or filtered out.
    fn prune_checks(&self) {
        if self.checked.borrow().is_empty() {
            return;
        }
        let shown: HashSet<PageId> = self.shown().into_iter().collect();
        let before = self.checked.borrow().len();
        self.checked.borrow_mut().retain(|id| shown.contains(id));
        if self.checked.borrow().len() != before {
            self.checks_changed();
        }
    }

    /// After the selection changed: the toggles on screen, and the bottom bar.
    fn checks_changed(&self) {
        {
            let checked = self.checked.borrow();
            for (id, cell) in self.bound.borrow().iter() {
                show_check(&cell.check, checked.contains(id));
            }
        }
        if let Some(win) = self.canvas.root().and_downcast::<CeraWindow>() {
            win.update_pages_actions();
        }
    }

    fn insert(&self, after: bool) {
        let Some(index) = self.selected() else { return };
        let at = if after { index + 1 } else { index };
        let paper = self.paper;
        let done = self.canvas.edit_pages(|note| {
            let crate::note::Note { history, doc, .. } = note;
            let spec = blank_like(doc.page(index)?.spec(), paper);
            history.insert_page(doc, at, spec)
        });
        if done.is_some() {
            self.reload();
            self.select_page(at);
        }
    }

    fn delete(&self) {
        let Some(index) = self.selected() else { return };
        if self.store.n_items() <= 1 {
            return;
        }
        let done = self.canvas.edit_pages(|note| {
            let crate::note::Note { history, doc, .. } = note;
            history.remove_page(doc, index)
        });
        if done.is_some() {
            self.reload();
            // The page that took its place, or the new last page.
            self.select_page(index);
        }
    }

    fn move_by(&self, by: i64) {
        let Some(index) = self.selected() else { return };
        let count = self.store.n_items() as i64;
        let to = (index as i64 + by).clamp(0, count - 1) as usize;
        if to == index {
            return;
        }
        let done = self.canvas.edit_pages(|note| {
            let crate::note::Note { history, doc, .. } = note;
            history.move_page(doc, index, to)
        });
        if done.is_some() {
            self.reload();
            self.select_page(to);
        }
    }

    /// Gives the pages at `indices` paper of `kind`, as one undo step.
    fn set_paper_of(&self, indices: &[usize], kind: PaperKind) {
        if indices.is_empty() {
            return;
        }
        let defaults = self.paper;
        let done = self.canvas.edit_pages(|note| {
            let crate::note::Note { history, doc, .. } = note;
            history.begin_group();
            let result = indices.iter().try_for_each(|&index| {
                let old = *doc.page(index)?.spec();
                let paper = Paper {
                    kind,
                    spacing: if old.paper.spacing > 0.0 {
                        old.paper.spacing
                    } else {
                        defaults.spacing
                    },
                    line: if old.paper.line.a > 0 {
                        old.paper.line
                    } else {
                        defaults.line
                    },
                    fill: old.paper.fill,
                };
                history.set_page_spec(doc, index, PageSpec { paper, ..old })
            });
            history.end_group();
            result
        });
        if done.is_some() {
            // The thumbnails are stale now (their spec changed); draw them at once.
            self.send_jobs();
        }
    }

    /// The selected pages' indices in the note, in order.
    fn checked_indices(&self) -> Vec<usize> {
        let note = self.note.borrow();
        let mut indices: Vec<usize> = self
            .checked
            .borrow()
            .iter()
            .filter_map(|&id| note.doc.page_index(id))
            .collect();
        indices.sort_unstable();
        indices
    }

    fn all_checked_bookmarked(&self) -> bool {
        self.checked
            .borrow()
            .iter()
            .all(|&id| self.is_bookmarked(id))
    }

    /// Deletes the selected pages as one undo step, unless that would leave no page.
    fn delete_checked(&self) {
        let indices = self.checked_indices();
        let Some(&first) = indices.first() else {
            return;
        };
        if indices.len() >= self.store.n_items() as usize {
            return;
        }
        let done = self.canvas.edit_pages(|note| {
            let crate::note::Note { history, doc, .. } = note;
            history.begin_group();
            // From the back, so the indices still hold.
            let result = indices
                .iter()
                .rev()
                .try_for_each(|&index| history.remove_page(doc, index));
            history.end_group();
            result
        });
        if done.is_some() {
            // Lets go of the deleted pages, and shows the page that took the first one's place.
            self.reload();
            self.select_page(first);
        }
    }

    /// Moves the selected pages one step earlier or later as one undo step (see
    /// `step_moves`); the current page stays current wherever it goes.
    fn move_checked(&self, by: i64) {
        let moves = step_moves(&self.checked_indices(), self.store.n_items() as usize, by);
        if moves.is_empty() {
            return;
        }
        let current = self
            .selection
            .selected_item()
            .and_downcast::<glib::BoxedAnyObject>()
            .map(|b| *b.borrow::<PageId>());
        let done = self.canvas.edit_pages(|note| {
            let crate::note::Note { history, doc, .. } = note;
            history.begin_group();
            let result = moves
                .iter()
                .try_for_each(|&(from, to)| history.move_page(doc, from, to));
            history.end_group();
            result
        });
        if done.is_some() {
            self.reload();
            let index = current.and_then(|id| self.note.borrow().doc.page_index(id));
            if let Some(index) = index {
                self.select_page(index);
            }
        }
    }

    /// Bookmarks the selected pages, or removes their bookmarks.
    fn bookmark_checked(&self, on: bool) {
        for index in self.checked_indices() {
            self.canvas.set_page_bookmarked(index, on);
        }
        self.marks_changed();
    }

    fn bind(&self, id: PageId, cell: BoundCell) {
        let size = {
            let note = self.note.borrow();
            note.doc
                .page_index(id)
                .and_then(|i| note.doc.page(i).ok())
                .map(|p| (p.spec().width, p.spec().height))
        };
        if let Some((w, h)) = size {
            let height = THUMB_WIDTH * f64::from(h) / f64::from(w.max(1.0));
            cell.picture
                .set_size_request(THUMB_WIDTH as i32, height.round() as i32);
        }
        match self.thumbs.borrow().get(&id) {
            Some((texture, ..)) => cell.picture.set_paintable(Some(texture)),
            None => cell.picture.set_paintable(None::<&gdk::Paintable>),
        }
        self.bound.borrow_mut().insert(id, cell);
        self.request_soon();
    }

    /// Asks for the missing or stale thumbnails of the cells on screen once binding settles.
    fn request_soon(&self) {
        if self.requested.replace(true) {
            return;
        }
        let me = self.me.clone();
        glib::idle_add_local_once(move || {
            if let Some(sorter) = me.upgrade() {
                sorter.requested.set(false);
                sorter.send_jobs();
            }
        });
    }

    fn send_jobs(&self) {
        let bound: Vec<PageId> = self.bound.borrow().keys().copied().collect();
        let mut jobs = Vec::new();
        let mut pending = HashMap::new();
        for id in bound {
            let Some(index) = self.note.borrow().doc.page_index(id) else {
                continue;
            };
            let Some(version) = self.version(index) else {
                continue;
            };
            let fresh = self
                .thumbs
                .borrow()
                .get(&id)
                .is_some_and(|(.., v)| *v == version);
            if fresh {
                continue;
            }
            let spec = version.1;
            let Ok(strokes) = self
                .note
                .borrow_mut()
                .doc
                .load_ink(index)
                .map(<[_]>::to_vec)
            else {
                continue;
            };
            // Small enough to be one tile.
            let px_per_pt = (THUMB_WIDTH * self.device_scale / f64::from(spec.width))
                .min(f64::from(WHOLE_PAGE_PX) / f64::from(spec.width.max(spec.height)));
            jobs.push(TileJob {
                key: TileKey {
                    doc: self.doc,
                    page: id,
                    layer: Layer::Page,
                    scale: Scale::new(px_per_pt),
                    tx: 0,
                    ty: 0,
                },
                page_size: (spec.width, spec.height),
                generation: version.0,
                source: JobSource::Page { spec, strokes },
            });
            pending.insert(id, version);
        }
        *self.pending.borrow_mut() = pending;
        self.renderer.set_jobs(self.doc, jobs);
    }

    fn thumbnail_ready(&self, result: TileResult) {
        let Ok(p) = result.pixels else { return };
        let id = result.key.page;
        // Drawn from what was last asked for, or an older request that was overtaken.
        let Some(version) = self
            .pending
            .borrow()
            .get(&id)
            .copied()
            .filter(|v| v.0 == result.generation)
        else {
            return;
        };
        self.pending.borrow_mut().remove(&id);
        let texture: gdk::Texture = gdk::MemoryTexture::new(
            p.width as i32,
            p.height as i32,
            PIXEL_FORMAT,
            &p.bytes,
            p.stride,
        )
        .upcast();
        let bytes = p.bytes.len();
        self.forget_thumbnail(id);
        self.thumbs
            .borrow_mut()
            .insert(id, (texture.clone(), bytes, version));
        self.lru.borrow_mut().push_back(id);
        self.thumb_bytes.set(self.thumb_bytes.get() + bytes);
        // Evict the oldest thumbnails that are not on screen.
        while self.thumb_bytes.get() > THUMB_BUDGET {
            let victim = {
                let lru = self.lru.borrow();
                let bound = self.bound.borrow();
                lru.iter().copied().find(|v| !bound.contains_key(v))
            };
            match victim {
                Some(v) => self.forget_thumbnail(v),
                None => break,
            }
        }
        if let Some(cell) = self.bound.borrow().get(&id) {
            cell.picture.set_paintable(Some(&texture));
        }
    }

    fn forget_thumbnail(&self, id: PageId) {
        if let Some((_, bytes, _)) = self.thumbs.borrow_mut().remove(&id) {
            self.thumb_bytes.set(self.thumb_bytes.get() - bytes);
            self.lru.borrow_mut().retain(|&v| v != id);
        }
    }
}

/// A right click, or a hold of a finger or the stylus, on a thumbnail selects its page (which
/// shows it) and opens the page's context menu there.
fn add_menu_gestures(item: &gtk::ListItem, cell: &gtk::Box) {
    let open = {
        let item = item.downgrade();
        let cell = cell.downgrade();
        move |x: f64, y: f64, touch: bool| {
            let (Some(item), Some(cell)) = (item.upgrade(), cell.upgrade()) else {
                return;
            };
            let position = item.position();
            if position == gtk::INVALID_LIST_POSITION {
                return;
            }
            if let Some(grid) = cell
                .ancestor(gtk::GridView::static_type())
                .and_downcast::<gtk::GridView>()
                && let Some(selection) = grid.model()
            {
                selection.select_item(position, true);
            }
            let Some(sorter) = cell
                .root()
                .and_downcast::<CeraWindow>()
                .and_then(|w| w.imp().sorter.borrow().clone())
            else {
                return;
            };
            // On one of several selected pages, the menu acts on all of them.
            let checked = sorter.checked.borrow().len();
            let menu = if checked > 1
                && page_of(&item).is_some_and(|id| sorter.checked.borrow().contains(&id))
            {
                selection_menu(checked, sorter.all_checked_bookmarked())
            } else {
                page_menu(sorter.selected_bookmarked())
            };
            popup_menu(&menu, &cell, x, y, touch);
        }
    };
    let open = Rc::new(open);
    let right = gtk::GestureClick::builder()
        .button(gdk::BUTTON_SECONDARY)
        .build();
    right.connect_pressed(glib::clone!(
        #[strong]
        open,
        move |g, _, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            open(x, y, false);
        }
    ));
    cell.add_controller(right);
    let hold = gtk::GestureLongPress::new();
    hold.connect_pressed(move |g, x, y| {
        let mouse = g
            .current_event_device()
            .is_none_or(|d| d.source() == gdk::InputSource::Mouse);
        g.set_state(gtk::EventSequenceState::Claimed);
        open(x, y, !mouse);
    });
    cell.add_controller(hold);
}

/// The paper kinds, for the action `pages.<action>`.
fn paper_menu(action: &str) -> gio::Menu {
    let paper = gio::Menu::new();
    for (label, kind) in [
        ("_Blank", "blank"),
        ("_Ruled", "ruled"),
        ("_Grid", "grid"),
        ("_Dots", "dots"),
    ] {
        paper.append(Some(label), Some(&format!("pages.{action}::{kind}")));
    }
    paper
}

/// The page's actions: insert, paper, move, bookmark, delete. They act on the selected page,
/// which is the one pressed.
fn page_menu(bookmarked: bool) -> gio::Menu {
    let menu = gio::Menu::new();
    let insert = gio::Menu::new();
    insert.append(Some("Insert Page _Before"), Some("pages.insert::before"));
    insert.append(Some("Insert Page _After"), Some("pages.insert::after"));
    menu.append_section(None, &insert);
    let arrange = gio::Menu::new();
    arrange.append_submenu(Some("_Paper"), &paper_menu("paper"));
    arrange.append(Some("Move _Earlier"), Some("pages.move::-1"));
    arrange.append(Some("Move _Later"), Some("pages.move::1"));
    arrange.append(
        Some(if bookmarked {
            "Remove Book_mark"
        } else {
            "Book_mark"
        }),
        Some("pages.bookmark::"),
    );
    menu.append_section(None, &arrange);
    let delete = gio::Menu::new();
    delete.append(Some("_Delete Page"), Some("pages.delete::"));
    menu.append_section(None, &delete);
    menu
}

/// The actions for `count` selected pages: paper, move, bookmark (or remove the bookmarks if
/// all have one), clear the selection, delete.
fn selection_menu(count: usize, bookmarked: bool) -> gio::Menu {
    let menu = gio::Menu::new();
    let arrange = gio::Menu::new();
    arrange.append_submenu(Some("_Paper"), &paper_menu("selected-paper"));
    arrange.append(Some("Move _Earlier"), Some("pages.selected-move::-1"));
    arrange.append(Some("Move _Later"), Some("pages.selected-move::1"));
    if bookmarked {
        arrange.append(
            Some("Remove Book_marks"),
            Some("pages.selected-bookmark::off"),
        );
    } else {
        arrange.append(Some("Book_mark Pages"), Some("pages.selected-bookmark::on"));
    }
    menu.append_section(None, &arrange);
    let selection = gio::Menu::new();
    selection.append(Some("_Clear Selection"), Some("pages.select::none"));
    menu.append_section(None, &selection);
    let delete = gio::Menu::new();
    delete.append(
        Some(&format!("_Delete {count} Pages")),
        Some("pages.selected-delete::"),
    );
    menu.append_section(None, &delete);
    menu
}

/// Opens `menu` at the press on `cell`.
fn popup_menu(menu: &gio::Menu, cell: &gtk::Box, x: f64, y: f64, touch: bool) {
    let popover = gtk::PopoverMenu::from_model(Some(menu));
    popover.set_parent(cell);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    if touch {
        // Above the press, out from under the hand, with taller rows.
        popover.set_position(gtk::PositionType::Top);
        popover.add_css_class("touch-menu");
    } else {
        popover.set_position(gtk::PositionType::Bottom);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
    }
    super::toolbar::touch_scroll(&popover);
    popover.connect_closed(|m| {
        let m = m.clone();
        glib::idle_add_local_once(move || m.unparent());
    });
    popover.popup();
}

/// The bookmark toggle shows the page's state: accent-coloured when bookmarked.
fn show_mark(mark: &gtk::Button, on: bool) {
    if on {
        mark.add_css_class("bookmarked");
        mark.set_tooltip_text(Some("Remove bookmark"));
    } else {
        mark.remove_css_class("bookmarked");
        mark.set_tooltip_text(Some("Bookmark"));
    }
}

/// The selection toggle shows whether the page is selected: an empty ring, or a check on the
/// accent colour.
fn show_check(check: &gtk::Button, on: bool) {
    if on {
        check.add_css_class("checked");
        check.set_tooltip_text(Some("Deselect"));
    } else {
        check.remove_css_class("checked");
        check.set_tooltip_text(Some("Select"));
    }
}

fn page_of(item: &gtk::ListItem) -> Option<PageId> {
    item.item()
        .and_downcast::<glib::BoxedAnyObject>()
        .map(|b| *b.borrow::<PageId>())
}

#[cfg(test)]
mod tests {
    use super::step_moves;

    /// Applies the moves to `0..count` and returns the new order.
    fn apply(selected: &[usize], count: usize, by: i64) -> Vec<usize> {
        let mut order: Vec<usize> = (0..count).collect();
        for (from, to) in step_moves(selected, count, by) {
            let page = order.remove(from);
            order.insert(to, page);
        }
        order
    }

    #[test]
    fn a_block_moves_as_one() {
        assert_eq!(apply(&[2, 3], 5, -1), [0, 2, 3, 1, 4]);
        assert_eq!(apply(&[2, 3], 5, 1), [0, 1, 4, 2, 3]);
    }

    #[test]
    fn scattered_pages_each_move_one_step() {
        assert_eq!(apply(&[1, 3], 5, -1), [1, 0, 3, 2, 4]);
        assert_eq!(apply(&[1, 3], 5, 1), [0, 2, 1, 4, 3]);
    }

    #[test]
    fn pages_at_the_end_stay() {
        assert_eq!(apply(&[0, 1, 3], 5, -1), [0, 1, 3, 2, 4]);
        assert_eq!(apply(&[1, 3, 4], 5, 1), [0, 2, 1, 3, 4]);
        assert!(step_moves(&[0, 1], 2, -1).is_empty());
        assert!(step_moves(&[0], 3, 0).is_empty());
    }
}
