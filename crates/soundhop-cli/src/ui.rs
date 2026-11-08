//! Cross-platform terminal output helpers (`CP1252` / UTF-8 safe).
//! Automatically falls back to ASCII symbols when on non-UTF8 Windows consoles or when
//! `--no-color` / `NO_COLOR` is active.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

static NO_COLOR: AtomicBool = AtomicBool::new(false);

pub fn set_no_color(disable: bool) {
    NO_COLOR.store(disable, Ordering::Relaxed);
}

pub fn use_color() -> bool {
    if NO_COLOR.load(Ordering::Relaxed) {
        return false;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false) {
        return false;
    }
    io::stdout().is_terminal()
}

pub fn supports_unicode() -> bool {
    #[cfg(target_os = "windows")]
    {
        if std::env::var_os("WT_SESSION").is_some()
            || std::env::var("TERM_PROGRAM").is_ok()
            || std::env::var("PYTHONUTF8").is_ok()
        {
            return true;
        }
        false
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}

pub fn sym_check() -> &'static str {
    if supports_unicode() {
        "✓"
    } else {
        "[OK]"
    }
}

pub fn sym_cross() -> &'static str {
    if supports_unicode() {
        "✗"
    } else {
        "[X]"
    }
}

pub fn sym_warn() -> &'static str {
    if supports_unicode() {
        "!"
    } else {
        "[!]"
    }
}

pub fn sym_bullet() -> &'static str {
    if supports_unicode() {
        "●"
    } else {
        "*"
    }
}

pub fn sym_arrow() -> &'static str {
    if supports_unicode() {
        "→"
    } else {
        "->"
    }
}

pub fn safe_print(msg: &str) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{msg}");
}

pub fn safe_eprint(msg: &str) {
    let mut err = io::stderr().lock();
    let _ = writeln!(err, "{msg}");
}

pub fn info(msg: &str) {
    if use_color() {
        safe_print(&format!("\x1b[36m{}\x1b[0m {}", sym_bullet(), msg));
    } else {
        safe_print(&format!("{} {}", sym_bullet(), msg));
    }
}

pub fn ok(msg: &str) {
    if use_color() {
        safe_print(&format!("\x1b[32m{}\x1b[0m {}", sym_check(), msg));
    } else {
        safe_print(&format!("{} {}", sym_check(), msg));
    }
}

pub fn warning(msg: &str) {
    if use_color() {
        safe_eprint(&format!("\x1b[33m{}\x1b[0m {}", sym_warn(), msg));
    } else {
        safe_eprint(&format!("{} {}", sym_warn(), msg));
    }
}

pub fn error(msg: &str) {
    if use_color() {
        safe_eprint(&format!("\x1b[31m{}\x1b[0m {}", sym_cross(), msg));
    } else {
        safe_eprint(&format!("{} {}", sym_cross(), msg));
    }
}

pub fn header(title: &str) {
    if use_color() {
        safe_print(&format!("\n\x1b[1;34m=== {title} ===\x1b[0m"));
    } else {
        safe_print(&format!("\n=== {title} ==="));
    }
}

pub fn print_json<T: serde::Serialize>(value: &T) {
    if let Ok(s) = serde_json::to_string_pretty(value) {
        safe_print(&s);
    }
}
