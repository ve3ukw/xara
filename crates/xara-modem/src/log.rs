use std::fmt;
use std::sync::OnceLock;

static LOG: OnceLock<fn(bool, fmt::Arguments)> = OnceLock::new();

pub fn set_log(f: fn(bool, fmt::Arguments)) {
    let _ = LOG.set(f);
}

pub(crate) fn line(console: bool, args: fmt::Arguments) {
    match LOG.get() {
        Some(f) => f(console, args),
        None if console => eprintln!("{args}"),
        None => {}
    }
}
