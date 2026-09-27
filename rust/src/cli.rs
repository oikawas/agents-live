use clap::Parser;
use std::path::PathBuf;

fn default_codex_home() -> String {
    std::env::var("CODEX_HOME").unwrap_or_else(|_| {
        let home = dirs_home();
        format!("{}/.codex", home)
    })
}

fn default_claude_home() -> String {
    std::env::var("CLAUDE_HOME").unwrap_or_else(|_| {
        let home = dirs_home();
        format!("{}/.claude", home)
    })
}

fn dirs_home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
}

#[derive(Parser, Debug, Clone)]
#[command(
    version,
    about = "Continuously show live Codex and Claude Code main and subagent status.",
    disable_help_flag = false
)]
pub struct Args {
    /// Codex data directory
    #[arg(long, default_value_t = default_codex_home())]
    pub codex_home: String,

    /// Claude Code data directory
    #[arg(long, global = true, default_value_t = default_claude_home())]
    pub claude_home: String,

    /// show or hide Codex main and spawned agents
    #[arg(long, default_value = "on", value_parser = ["on", "off"])]
    pub codex: String,

    /// show or hide Claude Code main and spawned agents
    #[arg(long, default_value = "on", value_parser = ["on", "off"])]
    pub claude: String,

    /// refresh interval in seconds
    #[arg(long, default_value_t = 0.7)]
    pub interval: f64,

    /// print one snapshot and exit
    #[arg(long, default_value_t = false)]
    pub once: bool,

    /// show only one Codex PID
    #[arg(long)]
    pub process: Option<i64>,

    /// hide completed subagents (alias: --hide-done-children)
    #[arg(long, alias = "hide-done-children", default_value_t = false)]
    pub hide_done: bool,

    /// show or hide the all-sessions-idle banner
    #[arg(long = "all-idle-banner", default_value = "on", value_parser = ["on", "off"])]
    pub all_idle_banner: String,

    /// background color for the all-sessions-idle banner
    #[arg(long = "all-idle-bgcolor", default_value = "blue", value_parser = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"])]
    pub all_idle_bgcolor: String,

    /// fill the terminal with the configured all-idle color when all sessions are idle
    #[arg(long = "all-idle-red-screen", default_value_t = false)]
    pub all_idle_red_screen: bool,

    /// show each source's Weekly nn% figure in reverse video
    #[arg(long = "weekly-reverse", default_value_t = false)]
    pub weekly_reverse: bool,

    /// omit the blank line between PID group blocks and source sections
    #[arg(long = "compact-groups", default_value = "on", value_parser = ["on", "off"])]
    pub compact_groups: String,

    /// include internal guardian threads
    #[arg(long = "show-guardians", default_value_t = false)]
    pub show_guardians: bool,

    /// order PID groups by creation time or root session name
    #[arg(long = "group-sort", default_value = "created", value_parser = ["created", "session-name"])]
    pub group_sort: String,

    /// show Codex's PID groups before Claude Code's, or vice versa
    #[arg(long = "group-order", default_value = "codex-first", value_parser = ["codex-first", "claude-first"])]
    pub group_order: String,

    /// terminal color palette
    #[arg(long, default_value = "cool", value_parser = ["cool", "codex", "cyberpunk"])]
    pub theme: String,

    /// ANSI color mode
    #[arg(long, default_value = "auto", value_parser = ["auto", "always", "never"])]
    pub color: String,

    /// show cumulative token usage in thread rows
    #[arg(long, default_value = "on", value_parser = ["on", "off"])]
    pub tokens: String,

    /// keep narrow thread rows to one compact line without model details
    #[arg(long = "no-wrap", default_value_t = false)]
    pub no_wrap: bool,

    #[command(subcommand)]
    pub command: Option<Subcommand>,
}

/// Without a subcommand, agents-live runs the dashboard.
#[derive(clap::Subcommand, Debug, Clone)]
pub enum Subcommand {
    /// Hook Claude Code's statusLine so the dashboard can show Claude's
    /// weekly and 5-hour limits (use --uninstall to undo)
    InstallStatusline {
        /// restore the statusLine configuration from before the install
        #[arg(long)]
        uninstall: bool,
    },
    /// Run as Claude Code's statusLine command (set up by install-statusline):
    /// cache the rate limits from stdin, then run ORIGINAL_COMMAND with the
    /// same stdin
    Statusline {
        /// the statusLine command that was configured before the install
        #[arg(allow_hyphen_values = true)]
        original_command: Option<String>,
    },
}

impl Args {
    pub fn codex_home_path(&self) -> PathBuf {
        shellexpand_home(&self.codex_home)
    }
    pub fn claude_home_path(&self) -> PathBuf {
        shellexpand_home(&self.claude_home)
    }

    /// The Claude home to spell out in an installed statusLine command:
    /// None for the default ~/.claude, which the hook finds on its own.
    pub fn explicit_claude_home(&self) -> Option<String> {
        let path = self.claude_home_path();
        if path == PathBuf::from(dirs_home()).join(".claude") {
            None
        } else {
            Some(std::path::absolute(&path).unwrap_or(path).to_string_lossy().to_string())
        }
    }
}

fn shellexpand_home(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        PathBuf::from(dirs_home()).join(rest)
    } else if value == "~" {
        PathBuf::from(dirs_home())
    } else {
        PathBuf::from(value)
    }
}
