//! Terminal setup and teardown, and the thread that reads input.

use std::io::{Stdout, Write, stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, bounded};
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

/// Write an escape sequence straight to the terminal, for things ratatui
/// doesn't cover, like OSC 52 clipboard writes.
/// Set the mouse pointer shape with OSC 22, using CSS cursor names.
/// Terminals that don't know the sequence ignore it.
pub fn set_pointer(shape: &str) {
    write_raw(&format!("\x1b]22;{shape}\x1b\\"));
}

pub fn write_raw(s: &str) {
    let mut out = stdout();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

enum Control {
    Pause,
    Resume,
}

/// The input thread reads terminal events and forwards them. It can be
/// paused while another program (such as an editor) owns the terminal.
pub struct Input {
    control: Sender<Control>,
    ack: Receiver<()>,
}

impl Input {
    pub fn start(forward: impl Fn(crossterm::event::Event) + Send + 'static) -> Input {
        let (control, control_rx) = bounded::<Control>(1);
        let (ack_tx, ack) = bounded::<()>(1);
        std::thread::Builder::new()
            .name("input".into())
            .spawn(move || {
                loop {
                    if let Ok(Control::Pause) = control_rx.try_recv() {
                        let _ = ack_tx.send(());
                        // Wait until we're told to resume.
                        while !matches!(control_rx.recv(), Ok(Control::Resume) | Err(_)) {}
                        continue;
                    }
                    match crossterm::event::poll(Duration::from_millis(50)) {
                        Ok(true) => match crossterm::event::read() {
                            Ok(ev) => forward(ev),
                            Err(_) => break,
                        },
                        Ok(false) => {}
                        Err(_) => break,
                    }
                }
            })
            .ok();
        Input { control, ack }
    }

    pub fn pause(&self) {
        if self.control.send(Control::Pause).is_ok() {
            let _ = self.ack.recv_timeout(Duration::from_secs(1));
        }
    }

    pub fn resume(&self) {
        let _ = self.control.send(Control::Resume);
    }
}
