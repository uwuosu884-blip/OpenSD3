//! Core format inspection, container readers, codecs, and sample extraction
//! for Superior Drummer 3 (`SL-*` / `.obw` / `soundstats.dat`) and Toontrack
//! `ttfs` archives, plus unified drum-kit (`kit.json` + `.sfz`) I/O.

pub mod kit;
pub mod library;
pub mod obw;
pub mod probe;
pub mod riff;
pub mod synthkit;
pub mod tpc;
pub mod ttbw;
pub mod ttfs;
pub mod ttpw;
pub mod wav;

use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Sd3Error {
    Io(io::Error),
    Format(String),
    Unsupported(String),
    Json(serde_json::Error),
}

impl fmt::Display for Sd3Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Sd3Error::Io(e) => write!(f, "I/O error: {e}"),
            Sd3Error::Format(msg) => write!(f, "format error: {msg}"),
            Sd3Error::Unsupported(msg) => write!(f, "unsupported format/codec: {msg}"),
            Sd3Error::Json(e) => write!(f, "JSON error: {e}"),
        }
    }
}

impl std::error::Error for Sd3Error {}

impl From<io::Error> for Sd3Error {
    fn from(e: io::Error) -> Self {
        Sd3Error::Io(e)
    }
}

impl From<serde_json::Error> for Sd3Error {
    fn from(e: serde_json::Error) -> Self {
        Sd3Error::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, Sd3Error>;
