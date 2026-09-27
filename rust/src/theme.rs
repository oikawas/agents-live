//! ANSI color palettes. Mirrors THEMES / CLAUDE_THEME / ALL_IDLE_BG_STYLES /
//! EFFORT_STYLES in the Python `agents-live` script.

use std::collections::HashMap;

pub const RESET: &str = "\x1b[0m";
pub const REVERSE: &str = "\x1b[7m";
pub const NO_REVERSE: &str = "\x1b[27m";
pub const HIDE_CURSOR: &str = "\x1b[?25l";
pub const SHOW_CURSOR: &str = "\x1b[?25h";
pub const CLEAR: &str = "\x1b[H\x1b[2J";

pub type Palette = HashMap<&'static str, &'static str>;

fn base_palette(
    header: &'static str,
    border: &'static str,
    run: &'static str,
    idle: &'static str,
    done: &'static str,
    start: &'static str,
    stop: &'static str,
    name: &'static str,
    dim: &'static str,
) -> Palette {
    let mut p = Palette::new();
    p.insert("header", header);
    // The top "AGENTS LIVE" line belongs to neither source, so it stays a
    // neutral white in every theme instead of the Codex header hue.
    p.insert("title", "\x1b[1;97m");
    p.insert("weekly_high", "\x1b[1;30;48;5;46m");
    p.insert("weekly_critical", "\x1b[1;97;48;5;196m");
    p.insert("effort_high", "\x1b[1;30;48;5;117m");
    p.insert("effort_xhigh", "\x1b[1;30;48;5;226m");
    p.insert("effort_ultra", "\x1b[1;97;48;5;196m");
    p.insert("model_highlight", "\x1b[1;30;48;5;46m");
    p.insert("border", border);
    p.insert("run", run);
    p.insert("idle", idle);
    p.insert("done", done);
    p.insert("start", start);
    p.insert("stop", stop);
    p.insert("name", name);
    p.insert("dim", dim);
    p
}

pub fn theme(name: &str) -> Palette {
    match name {
        "codex" => base_palette(
            "\x1b[1;38;5;48m",
            "\x1b[38;5;36m",
            "\x1b[1;38;5;84m",
            "\x1b[1;38;5;87m",
            "\x1b[38;5;102m",
            "\x1b[1;38;5;179m",
            "\x1b[1;38;5;203m",
            "\x1b[1;38;5;159m",
            "\x1b[38;5;102m",
        ),
        "cyberpunk" => base_palette(
            "\x1b[1;38;5;51m",
            "\x1b[38;5;93m",
            "\x1b[1;38;5;118m",
            "\x1b[1;38;5;51m",
            "\x1b[38;5;99m",
            "\x1b[1;38;5;208m",
            "\x1b[1;38;5;201m",
            "\x1b[1;38;5;213m",
            "\x1b[38;5;99m",
        ),
        _ => base_palette(
            "\x1b[1;38;5;117m",
            "\x1b[38;5;67m",
            "\x1b[1;38;5;151m",
            "\x1b[1;38;5;117m",
            "\x1b[38;5;102m",
            "\x1b[1;38;5;180m",
            "\x1b[1;38;5;167m",
            "\x1b[1;38;5;189m",
            "\x1b[38;5;102m",
        ),
    }
}

#[allow(dead_code)]
pub const THEME_NAMES: &[&str] = &["cool", "codex", "cyberpunk"];

pub fn claude_theme() -> Palette {
    base_palette(
        "\x1b[1;38;5;208m",
        "\x1b[38;5;166m",
        "\x1b[1;38;5;209m",
        "\x1b[1;38;5;180m",
        "\x1b[38;5;137m",
        "\x1b[1;38;5;214m",
        "\x1b[1;38;5;160m",
        "\x1b[1;38;5;223m",
        "\x1b[38;5;138m",
    )
}

pub fn all_idle_bg_style(color: &str) -> &'static str {
    match color {
        "black" => "\x1b[1;97;48;5;16m",
        "red" => "\x1b[1;97;48;5;196m",
        "green" => "\x1b[1;30;48;5;46m",
        "yellow" => "\x1b[1;30;48;5;226m",
        "magenta" => "\x1b[1;30;48;5;201m",
        "cyan" => "\x1b[1;30;48;5;51m",
        "white" => "\x1b[1;30;48;5;231m",
        _ => "\x1b[1;97;48;5;21m", // blue (default)
    }
}

#[allow(dead_code)]
pub const ALL_IDLE_BG_COLOR_NAMES: &[&str] = &[
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

/// Models whose name is highlighted in the model column. Matched against the
/// exact model string, so a differently-suffixed variant is left unhighlighted
/// rather than being guessed at.
pub fn model_style(model: &str) -> Option<&'static str> {
    match model {
        "gpt-6-astra" => Some("model_highlight"),
        _ => None,
    }
}

pub fn effort_style(effort: &str) -> Option<&'static str> {
    match effort {
        "high" => Some("effort_high"),
        "xhigh" => Some("effort_xhigh"),
        "ultra" | "max" => Some("effort_ultra"),
        _ => None,
    }
}
