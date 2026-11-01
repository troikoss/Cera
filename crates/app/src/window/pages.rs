//! The page sorter (DESIGN.md §9): a grid of page thumbnails to reorder pages, insert blank
//! ones, delete them or change their paper. Every change goes through the note's history, so
//! it undoes like any other edit.
//!
//! Thumbnails are whole-page renders from the window's render threads under a document id
//! of their own, asked for only for the cells on screen and kept in a small LRU.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

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
const THUMB_WIDTH: f64 = 120.0;
/// Memory for thumbnail textures; about 50 A4 thumbnails at 2× scale.
const THUMB_BUDGET: usize = 16 << 20;

#[cfg(target_endian = "little")]
const PIXEL_FORMAT: gdk::MemoryFormat = gdk::MemoryFormat::B8g8r8a8Premultiplied;
#[cfg(target_endian = "big")]
const PIXEL_FORMAT: gdk::MemoryFormat = gdk::MemoryFormat::A8r8g8b8Premultiplied;

struct Sorter {
    me: std::rc::Weak<Sorter>,
    canvas: Canvas,
    note: SharedNote,
    renderer: Rc<Renderer>,
    doc: DocId,
    device_scale: f64,
    /// Paper for inserted pages.
    paper: Paper,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    thumbs: RefCell<HashMap<PageId, (gdk::Texture, usize)>>,
    lru: RefCell<VecDeque<PageId>>,
    thumb_bytes: Cell<usize>,
    /// Cells on screen, by the page they show.
    bound: RefCell<HashMap<PageId, gtk::Picture>>,
    requested: Cell<bool>,
}

impl CeraWindow {
    pub(super) fn show_pages(&self) {
        let Some(canvas) = self.canvas() else { return };
        let Some(note) = canvas.note() else { return };
        canvas.clear_selection();
        let imp = self.imp();
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
        let selection = gtk::SingleSelection::new(Some(store.clone()));
        let sorter = Rc::new_cyclic(|me| Sorter {
            me: me.clone(),
            canvas,
            note,
            renderer,
            doc,
            device_scale: f64::from(self.scale_factor()),
            paper: self.new_page_paper(),
            store,
            selection,
            thumbs: RefCell::default(),
            lru: RefCell::default(),
            thumb_bytes: Cell::new(0),
            bound: RefCell::default(),
            requested: Cell::new(false),
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
        sorter.reload(Some(sorter.canvas.current_page()));
        let dialog = sorter.build_dialog();
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[strong]
            sorter,
            move |_| {
                win.imp().sinks.borrow_mut().remove(&sorter.doc);
                sorter.renderer.close_doc(sorter.doc);
                win.update_note_actions();
            }
        ));
        dialog.present(Some(self));
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
    fn build_dialog(self: &Rc<Self>) -> adw::Dialog {
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            let picture = gtk::Picture::builder()
                .content_fit(gtk::ContentFit::Contain)
                .can_shrink(true)
                .halign(gtk::Align::Center)
                .css_classes(["page-thumb"])
                .build();
            let label = gtk::Label::builder().css_classes(["caption"]).build();
            let cell = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .margin_top(6)
                .margin_bottom(6)
                .build();
            cell.append(&picture);
            cell.append(&label);
            item.set_child(Some(&cell));
        });
        let weak = Rc::downgrade(self);
        factory.connect_bind(move |_, item| {
            let Some(sorter) = weak.upgrade() else { return };
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            let Some(id) = item
                .item()
                .and_downcast::<glib::BoxedAnyObject>()
                .map(|b| *b.borrow::<PageId>())
            else {
                return;
            };
            let Some(cell) = item.child() else { return };
            let picture = cell
                .first_child()
                .and_downcast::<gtk::Picture>()
                .expect("picture");
            let label = picture
                .next_sibling()
                .and_downcast::<gtk::Label>()
                .expect("label");
            label.set_label(&(item.position() + 1).to_string());
            sorter.bind(id, &picture);
        });
        let weak = Rc::downgrade(self);
        factory.connect_unbind(move |_, item| {
            let Some(sorter) = weak.upgrade() else { return };
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            if let Some(id) = item
                .item()
                .and_downcast::<glib::BoxedAnyObject>()
                .map(|b| *b.borrow::<PageId>())
            {
                sorter.bound.borrow_mut().remove(&id);
            }
        });
        let grid = gtk::GridView::builder()
            .model(&self.selection)
            .factory(&factory)
            .min_columns(2)
            .max_columns(8)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&grid)
            .build();

        let go = gtk::Button::builder()
            .label("_Go to Page")
            .use_underline(true)
            .css_classes(["suggested-action"])
            .build();
        let header = adw::HeaderBar::new();
        header.pack_start(&go);

        let insert_menu = gio::Menu::new();
        insert_menu.append(Some("Insert Page _Before"), Some("pages.insert::before"));
        insert_menu.append(Some("Insert Page _After"), Some("pages.insert::after"));
        let paper_menu = gio::Menu::new();
        for (label, kind) in [
            ("_Blank", "blank"),
            ("_Ruled", "ruled"),
            ("_Grid", "grid"),
            ("_Dots", "dots"),
        ] {
            paper_menu.append(Some(label), Some(&format!("pages.paper::{kind}")));
        }
        // Detailed names ("pages.move::1") set the action's target too.
        let button = |icon: &str, tooltip: &str, action: &str| {
            let button = gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tooltip)
                .build();
            button.set_detailed_action_name(action);
            button
        };
        let bar = gtk::CenterBox::new();
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
                .menu_model(&paper_menu)
                .build(),
        );
        let middle = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        middle.append(&button(
            "go-previous-symbolic",
            "Move earlier",
            "pages.move::-1",
        ));
        middle.append(&button("go-next-symbolic", "Move later", "pages.move::1"));
        let delete = button("user-trash-symbolic", "Delete page", "pages.delete");
        delete.add_css_class("destructive-action");
        bar.set_start_widget(Some(&left));
        bar.set_center_widget(Some(&middle));
        bar.set_end_widget(Some(&delete));
        bar.set_margin_top(6);
        bar.set_margin_bottom(6);
        bar.set_margin_start(6);
        bar.set_margin_end(6);

        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.add_bottom_bar(&bar);
        view.set_content(Some(&scroller));
        let dialog = adw::Dialog::builder()
            .title("Pages")
            .content_width(720)
            .content_height(640)
            .child(&view)
            .build();
        dialog.insert_action_group("pages", Some(&self.actions()));

        let go_to = glib::clone!(
            #[weak(rename_to = sorter)]
            self,
            #[weak]
            dialog,
            move || {
                if let Some(index) = sorter.selected() {
                    sorter.canvas.show_page(index);
                }
                dialog.close();
            }
        );
        let go_to = Rc::new(go_to);
        go.connect_clicked(glib::clone!(
            #[strong]
            go_to,
            move |_| go_to()
        ));
        grid.connect_activate(move |_, _| go_to());
        dialog
    }

    fn actions(self: &Rc<Self>) -> gio::SimpleActionGroup {
        let group = gio::SimpleActionGroup::new();
        let insert = gio::SimpleAction::new("insert", Some(glib::VariantTy::STRING));
        insert.connect_activate(glib::clone!(
            #[weak(rename_to = sorter)]
            self,
            move |_, param| {
                let after = param.and_then(|p| p.get::<String>()).as_deref() == Some("after");
                sorter.insert(after);
            }
        ));
        let paper = gio::SimpleAction::new("paper", Some(glib::VariantTy::STRING));
        paper.connect_activate(glib::clone!(
            #[weak(rename_to = sorter)]
            self,
            move |_, param| {
                let kind = match param.and_then(|p| p.get::<String>()).as_deref() {
                    Some("ruled") => PaperKind::Ruled,
                    Some("grid") => PaperKind::Grid,
                    Some("dots") => PaperKind::Dots,
                    _ => PaperKind::Blank,
                };
                sorter.set_paper(kind);
            }
        ));
        let move_page = gio::SimpleAction::new("move", Some(glib::VariantTy::STRING));
        move_page.connect_activate(glib::clone!(
            #[weak(rename_to = sorter)]
            self,
            move |_, param| {
                let by = param
                    .and_then(|p| p.get::<String>())
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                sorter.move_by(by);
            }
        ));
        let delete = gio::SimpleAction::new("delete", None);
        delete.connect_activate(glib::clone!(
            #[weak(rename_to = sorter)]
            self,
            move |_, _| sorter.delete()
        ));
        for action in [&insert, &paper, &move_page, &delete] {
            group.add_action(action);
        }
        let update = glib::clone!(
            #[weak(rename_to = sorter)]
            self,
            #[weak]
            move_page,
            #[weak]
            delete,
            move || {
                let count = sorter.store.n_items() as usize;
                let selected = sorter.selected();
                delete.set_enabled(selected.is_some() && count > 1);
                move_page.set_enabled(selected.is_some() && count > 1);
            }
        );
        update();
        self.selection.connect_selected_notify(move |_| update());
        group
    }

    fn selected(&self) -> Option<usize> {
        let i = self.selection.selected();
        (i != gtk::INVALID_LIST_POSITION).then_some(i as usize)
    }

    /// Fills the grid from the note and selects `select`.
    fn reload(&self, select: Option<u32>) {
        let items: Vec<glib::BoxedAnyObject> = self
            .note
            .borrow()
            .doc
            .pages()
            .iter()
            .map(|p| glib::BoxedAnyObject::new(p.id()))
            .collect();
        self.store.splice(0, self.store.n_items(), &items);
        if let Some(i) = select {
            self.selection
                .set_selected(i.min(self.store.n_items().saturating_sub(1)));
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
            self.reload(Some(at as u32));
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
            self.reload(Some(index.saturating_sub(usize::from(index > 0)) as u32));
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
            self.reload(Some(to as u32));
        }
    }

    fn set_paper(&self, kind: PaperKind) {
        let Some(index) = self.selected() else { return };
        let defaults = self.paper;
        let id = self.canvas.edit_pages(|note| {
            let crate::note::Note { history, doc, .. } = note;
            let page = doc.page(index)?;
            let (id, old) = (page.id(), *page.spec());
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
            history.set_page_spec(doc, index, PageSpec { paper, ..old })?;
            Ok(id)
        });
        if let Some(id) = id {
            self.forget_thumbnail(id);
            self.reload(Some(index as u32));
        }
    }

    fn bind(&self, id: PageId, picture: &gtk::Picture) {
        let size = {
            let note = self.note.borrow();
            note.doc
                .page_index(id)
                .and_then(|i| note.doc.page(i).ok())
                .map(|p| (p.spec().width, p.spec().height))
        };
        if let Some((w, h)) = size {
            let height = THUMB_WIDTH * f64::from(h) / f64::from(w.max(1.0));
            picture.set_size_request(THUMB_WIDTH as i32, height.round() as i32);
        }
        match self.thumbs.borrow().get(&id) {
            Some((texture, _)) => picture.set_paintable(Some(texture)),
            None => picture.set_paintable(None::<&gdk::Paintable>),
        }
        self.bound.borrow_mut().insert(id, picture.clone());
        if !self.thumbs.borrow().contains_key(&id) {
            self.request_soon();
        }
    }

    /// Asks for the missing thumbnails of the cells on screen once binding settles.
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
        let wanted: Vec<PageId> = self
            .bound
            .borrow()
            .keys()
            .copied()
            .filter(|id| !self.thumbs.borrow().contains_key(id))
            .collect();
        let mut jobs = Vec::new();
        let mut note = self.note.borrow_mut();
        for id in wanted {
            let Some(index) = note.doc.page_index(id) else {
                continue;
            };
            let Ok(spec) = note.doc.page(index).map(|p| *p.spec()) else {
                continue;
            };
            let Ok(strokes) = note.doc.load_ink(index) else {
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
                generation: 0,
                source: JobSource::Page {
                    spec,
                    strokes: strokes.to_vec(),
                },
            });
        }
        drop(note);
        self.renderer.set_jobs(self.doc, jobs);
    }

    fn thumbnail_ready(&self, result: TileResult) {
        let Ok(p) = result.pixels else { return };
        let id = result.key.page;
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
            .insert(id, (texture.clone(), bytes));
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
        if let Some(picture) = self.bound.borrow().get(&id) {
            picture.set_paintable(Some(&texture));
        }
    }

    fn forget_thumbnail(&self, id: PageId) {
        if let Some((_, bytes)) = self.thumbs.borrow_mut().remove(&id) {
            self.thumb_bytes.set(self.thumb_bytes.get() - bytes);
            self.lru.borrow_mut().retain(|&v| v != id);
        }
    }
}
