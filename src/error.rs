use std::fmt;

/// What went wrong, for callers that act on it or word their own message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// There's no microphone, or none of the name asked for.
    NoDevice,
    /// No sound server is running, or PipeWire runs without its PulseAudio service.
    NoServer,
    /// Another program has the device.
    Busy,
    /// The sound server or the system refused access to the microphone.
    PermissionDenied,
    /// The sound server went away or ended the recording, as when the device is unplugged.
    Disconnected,
    /// The sound server stopped answering or sending audio.
    Stalled,
    /// The sound server sent something that can't be read.
    Protocol,
    /// The options ask for something the system can't do, such as a network sound server.
    Unsupported,
    /// The operating system failed a call.
    Io,
}

/// An error from opening or running a capture. It displays as a lowercase phrase, so callers
/// can put it in a sentence of their own ("Couldn't open the microphone ({error}).").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    detail: String,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Error {
            kind,
            detail: detail.into(),
        }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for Error {}
