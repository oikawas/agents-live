mod cli;
mod claude;
mod codex;
mod model;
mod render;
mod theme;
mod usage_tracker;

use clap::Parser;
use crossterm::event::{self, Event, KeyCode};
use render::Dashboard;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static RUNNING: AtomicBool = AtomicBool::new(true);

/// Put stdin into cbreak mode: only `ICANON`/`ECHO` are cleared, everything
/// else (crucially `OPOST`/`ONLCR`) is left as the terminal had it. Mirrors
/// Python's `tty.setcbreak()` exactly, on purpose -- `crossterm::terminal::
/// enable_raw_mode()` instead applies a full `cfmakeraw()`-equivalent,
/// which also clears `OPOST`. With `OPOST` off, the plain `\n` this
/// renderer joins lines with no longer gets translated to `\r\n` by the
/// tty driver, so each redrawn line starts wherever the previous one ended
/// horizontally instead of at column 0 -- a staircase of misaligned,
/// wrapped rows. Restoring the saved termios on exit mirrors Python's
/// `termios.tcsetattr(fd, TCSADRAIN, saved)` in its `finally` block.
#[cfg(unix)]
fn enable_cbreak(fd: i32) -> Option<libc::termios> {
    unsafe {
        let mut original: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut original) != 0 {
            return None;
        }
        let mut mode = original;
        mode.c_lflag &= !(libc::ECHO | libc::ICANON);
        mode.c_cc[libc::VMIN] = 1;
        mode.c_cc[libc::VTIME] = 0;
        if libc::tcsetattr(fd, libc::TCSAFLUSH, &mode) != 0 {
            return None;
        }
        Some(original)
    }
}

#[cfg(unix)]
fn restore_termios(fd: i32, original: &libc::termios) {
    unsafe {
        libc::tcsetattr(fd, libc::TCSADRAIN, original);
    }
}

#[cfg(not(unix))]
fn enable_cbreak(_fd: i32) -> Option<()> {
    None
}

#[cfg(not(unix))]
fn restore_termios(_fd: i32, _original: &()) {}

/// Wait up to `timeout` seconds for a quit key (`q`/`Q`, bare Esc). Only
/// polls the keyboard when `raw_mode_active`; otherwise this is a plain
/// sleep. Mirrors `wait_for_quit`, but relies on crossterm's own escape-
/// sequence parsing (rather than a raw single-byte read) to tell a bare Esc
/// keypress apart from an arrow key's longer escape sequence. crossterm's
/// event reader only needs the fd in non-canonical/no-echo mode (which our
/// own cbreak setup above already provides) -- it does not require
/// crossterm's own `enable_raw_mode()` to have been called.
fn wait_for_quit(timeout: Duration, raw_mode_active: bool) -> bool {
    if !raw_mode_active {
        std::thread::sleep(timeout);
        return false;
    }
    match event::poll(timeout) {
        Ok(true) => match event::read() {
            Ok(Event::Key(key)) => matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q')),
            _ => false,
        },
        _ => false,
    }
}

/// The Claude statusLine subcommands; they never start the dashboard.
fn run_subcommand(args: &cli::Args, command: &cli::Subcommand) -> i32 {
    use claude::statusline;
    let claude_home = args.claude_home_path();
    match command {
        cli::Subcommand::Statusline { original_command } => {
            let mut stdin = Vec::new();
            let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut stdin);
            statusline::run_hook(
                &claude_home,
                original_command.as_deref().unwrap_or(""),
                &stdin,
                &mut std::io::stdout(),
                &mut std::io::stderr(),
            )
        }
        cli::Subcommand::InstallStatusline { uninstall } => {
            let result = if *uninstall {
                statusline::uninstall(&claude_home)
            } else {
                match std::env::current_exe() {
                    Ok(exe) => statusline::install(&claude_home, &exe, args.explicit_claude_home().as_deref()),
                    Err(e) => Err(format!("cannot locate the agents-live executable: {}", e)),
                }
            };
            match result {
                Ok(messages) => {
                    for line in messages {
                        println!("{}", line);
                    }
                    0
                }
                Err(message) => {
                    eprintln!("agents-live: {}", message);
                    1
                }
            }
        }
    }
}

fn main() {
    let args = cli::Args::parse();
    if let Some(command) = &args.command {
        std::process::exit(run_subcommand(&args, command));
    }
    if args.interval < 0.2 {
        eprintln!("--interval must be at least 0.2 seconds");
        std::process::exit(1);
    }
    let interval = Duration::from_secs_f64(args.interval);
    let once = args.once;

    let mut dashboard = Dashboard::new(args);

    let _ = ctrlc::set_handler(|| {
        RUNNING.store(false, Ordering::SeqCst);
    });

    let stdout_is_tty = std::io::stdout().is_terminal();
    let stdin_is_tty = std::io::stdin().is_terminal();
    let interactive = stdout_is_tty && !once;
    let mut raw_mode_active = false;
    let mut saved_termios = None;
    if interactive && stdin_is_tty {
        saved_termios = enable_cbreak(libc::STDIN_FILENO);
        raw_mode_active = saved_termios.is_some();
    }
    if interactive {
        print!("{}", theme::HIDE_CURSOR);
        let _ = std::io::stdout().flush();
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        while RUNNING.load(Ordering::SeqCst) {
            let (threads, states) = dashboard.snapshot();
            if !RUNNING.load(Ordering::SeqCst) {
                break;
            }
            let output = dashboard.render(&threads, &states);
            if interactive {
                print!("{}{}", theme::CLEAR, output);
                let _ = std::io::stdout().flush();
            } else {
                println!("{}", output);
                let _ = std::io::stdout().flush();
            }
            if once {
                break;
            }
            if wait_for_quit(interval, raw_mode_active) {
                break;
            }
        }
    }));

    if let Some(saved) = &saved_termios {
        restore_termios(libc::STDIN_FILENO, saved);
    }
    if interactive {
        print!("{}\n", theme::SHOW_CURSOR);
        let _ = std::io::stdout().flush();
    }

    if let Err(err) = result {
        std::panic::resume_unwind(err);
    }
}
