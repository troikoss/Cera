// SPDX-License-Identifier: GPL-3.0-or-later
use crate::model::PageId;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("not a Cera note")]
    NotANote,
    #[error(
        "written by a newer version of Cera ({what} version {found}, this build reads up to {supported})"
    )]
    UnsupportedVersion {
        what: &'static str,
        found: u32,
        supported: u32,
    },
    #[error("damaged note: {0}")]
    Corrupt(String),
    #[error("the note is read-only")]
    ReadOnly,
    #[error("no page at index {0}")]
    PageOutOfRange(usize),
    #[error("no page with id {0}")]
    UnknownPage(PageId),
    #[error("no stroke at index {0}")]
    StrokeOutOfRange(usize),
    #[error("undo history does not match the document")]
    HistoryMismatch,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) fn corrupt(what: impl Into<String>) -> Error {
    Error::Corrupt(what.into())
}
