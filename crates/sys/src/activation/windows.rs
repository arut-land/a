use super::{Home, Listener, io};

pub(super) const SUPPORTED: bool = false;

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows uses per-user self-spawn; no socket-activation service is installed",
    )
}

pub(super) fn install(_: &Home, _: bool, _: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    Err(unsupported())
}
pub(super) fn uninstall(_: &Home, _: bool) -> io::Result<()> {
    Err(unsupported())
}
pub(super) fn listener(_: &Home) -> io::Result<Listener> {
    Err(unsupported())
}
