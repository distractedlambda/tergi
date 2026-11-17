//! Terminal setup and teardown.

use std::io::{Stdout, Write, stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub type Term = Terminal<CrosstermBackend<Stdout>>;

static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

pub fn setup() -> Result<Term> {
    enable_raw_mode()?;
    enter()?;
    let mut t = Terminal::new(CrosstermBackend::new(stdout()))?;
    t.clear()?;
    Ok(t)
}

fn enter() -> Result<()> {
    let mut out = stdout();
    execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        EnableFocusChange
    )?;
    // The kitty keyboard protocol (supported by ghostty, kitty, foot, WezTerm
    // and others) lets us tell Esc apart from Alt sequences without a delay,
    // and see keys like Ctrl+Enter.
    if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        KEYBOARD_ENHANCED.store(true, Ordering::Relaxed);
    }
    Ok(())
}

pub fn restore() {
    let mut out = stdout();
    set_pointer("default");
    if KEYBOARD_ENHANCED.swap(false, Ordering::Relaxed) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        out,
        DisableMouseCapture,
        DisableBracketedPaste,
        DisableFocusChange,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = disable_raw_mode();
    let _ = out.flush();
}

/// Put the terminal back after `restore`, for example after running an
/// editor.
pub fn resume(term: &mut Term) -> Result<()> {
    enable_raw_mode()?;
    enter()?;
    term.clear()?;
    Ok(())
}

pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        prev(info);
    }));
}

/// Set the mouse pointer shape with OSC 22, using CSS cursor names.
/// Terminals that don't know the sequence ignore it.
pub fn set_pointer(shape: &str) {
    write_raw(&format!("\x1b]22;{shape}\x1b\\"));
}

/// Copy text to the clipboard with OSC 52, which works over SSH too.
pub fn copy(text: &str) {
    write_raw(&format!(
        "\x1b]52;c;{}\x07",
        crate::util::base64(text.as_bytes())
    ));
}

/// Write an escape sequence straight to the terminal, for things ratatui
/// doesn't cover.
pub fn write_raw(s: &str) {
    let mut out = stdout();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}
