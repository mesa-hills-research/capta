//! The systems Capta has no backend for, where listing and opening fail as unsupported.

use crate::{Device, Error, ErrorKind, OnAudio, OnError, Opened, Options};

fn unsupported() -> Error {
    Error::new(
        ErrorKind::Unsupported,
        "Capta records only on Linux, macOS and Windows",
    )
}

pub(crate) fn devices() -> Result<Vec<Device>, Error> {
    Err(unsupported())
}

pub(crate) fn open(_: &Options, _: OnAudio, _: OnError) -> Result<Opened, Error> {
    Err(unsupported())
}

/// Never made, since nothing opens.
pub(crate) struct Stream;
