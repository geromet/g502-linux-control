pub mod apply;
pub mod config;
pub mod dpi;
pub mod input;
pub mod ratbag;
pub mod restore;

use std::{fmt::Display, sync::OnceLock};

/// Log to stderr. Under systemd/journald a `<N>` prefix sets the priority, so
/// `journalctl -p warning` works. On a terminal we print plain text.
pub fn log(level: u8, msg: impl Display) {
    static JOURNAL: OnceLock<bool> = OnceLock::new();
    if *JOURNAL.get_or_init(|| std::env::var_os("JOURNAL_STREAM").is_some()) {
        eprintln!("<{level}>{msg}");
    } else {
        eprintln!("{msg}");
    }
}

#[macro_export]
macro_rules! info { ($($a:tt)*) => { $crate::log(6, format_args!($($a)*)) } }
#[macro_export]
macro_rules! warn { ($($a:tt)*) => { $crate::log(4, format_args!($($a)*)) } }
#[macro_export]
macro_rules! error { ($($a:tt)*) => { $crate::log(3, format_args!($($a)*)) } }
