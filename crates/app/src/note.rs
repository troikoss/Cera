// SPDX-License-Identifier: GPL-3.0-or-later
//! The note open in a window: its file, document and undo history, shared by the window
//! (saving, undo buttons) and the canvas (drawing, erasing).

use std::cell::RefCell;
use std::rc::Rc;

use cera_core::{Document, History, NoteFile};
use cera_render::tile::DocId;

pub struct Note {
    pub id: DocId,
    pub file: NoteFile,
    pub doc: Document,
    pub history: History,
}

pub type SharedNote = Rc<RefCell<Note>>;

impl Note {
    pub fn new(id: DocId, file: NoteFile, doc: Document) -> SharedNote {
        Rc::new(RefCell::new(Self {
            id,
            file,
            doc,
            history: History::default(),
        }))
    }

    /// Appends unsaved changes to the file; returns whether there were any.
    pub fn save(&mut self) -> cera_core::Result<bool> {
        self.file.save(&mut self.doc)
    }

    pub fn name(&self) -> String {
        self.file
            .path()
            .file_stem()
            .map_or_else(|| "Note".into(), |s| s.to_string_lossy().into_owned())
    }
}
