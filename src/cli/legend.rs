//! Legend system for CLI output
//!
//! Provides consistent legends across all commands. Legends appear at the
//! bottom of output, showing only symbols that were actually used.
//!
//! # Design Principles
//!
//! 1. **Bottom placement** - Legends appear after data, not before
//! 2. **Conditional** - Only show symbols actually present in output
//! 3. **Subtle** - Dim styling so legends don't dominate
//! 4. **Consistent** - Same symbols mean the same thing everywhere
//!
//! # Symbol Reference
//!
//! ## Socket Status
//! - `●` green  = current socket (the kitty instance you're in)
//! - `○`        = other responsive socket
//! - `✗` red    = dead/unresponsive socket
//! - `⚠` red    = warning (window on non-current socket)
//!
//! ## Window State
//! - `▸`        = focused window
//! - `●` yellow = unread (new activity since last view)
//! - `*`        = focused pane (in raw pane listings)
//!
//! ## Activity State (what Claude is doing)
//! - `⚡` yellow = Thinking (generating response)
//! - `⚙` cyan   = ToolUse (executing a tool)
//! - `◆` green  = AwaitingInput (waiting for user)
//! - `○` dim    = Idle (at prompt, not active)
//! - `●` blue   = Unknown (can't determine state)

use console::{style, Style};
use std::collections::HashSet;

/// Symbols that can appear in legends
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LegendSymbol {
    // Socket status
    SocketCurrent,
    SocketOther,
    SocketDead,
    SocketWarning,

    // Window state
    Focused,
    Unread,
    FocusedPane,

    // Activity state
    ActivityThinking,
    ActivityToolUse,
    ActivityAwaitingInput,
    ActivityIdle,
    ActivityUnknown,
}

impl LegendSymbol {
    /// Get the symbol character(s)
    #[allow(dead_code)]
    pub fn symbol(&self) -> &'static str {
        match self {
            Self::SocketCurrent => "●",
            Self::SocketOther => "○",
            Self::SocketDead => "✗",
            Self::SocketWarning => "⚠",
            Self::Focused => "▸",
            Self::Unread => "●",
            Self::FocusedPane => "*",
            Self::ActivityThinking => "⚡",
            Self::ActivityToolUse => "⚙",
            Self::ActivityAwaitingInput => "◆",
            Self::ActivityIdle => "○",
            Self::ActivityUnknown => "●",
        }
    }

    /// Get styled symbol for legend display
    pub fn styled(&self) -> String {
        match self {
            Self::SocketCurrent => style("●").green().to_string(),
            Self::SocketOther => "○".to_string(),
            Self::SocketDead => style("✗").red().to_string(),
            Self::SocketWarning => style("⚠").red().to_string(),
            Self::Focused => "▸".to_string(),
            Self::Unread => style("●").yellow().to_string(),
            Self::FocusedPane => "*".to_string(),
            Self::ActivityThinking => style("⚡").yellow().to_string(),
            Self::ActivityToolUse => style("⚙").cyan().to_string(),
            Self::ActivityAwaitingInput => style("◆").green().to_string(),
            Self::ActivityIdle => Style::new().dim().apply_to("○").to_string(),
            Self::ActivityUnknown => style("●").blue().to_string(),
        }
    }

    /// Get description for legend
    pub fn description(&self) -> &'static str {
        match self {
            Self::SocketCurrent => "current socket",
            Self::SocketOther => "other socket",
            Self::SocketDead => "dead socket",
            Self::SocketWarning => "non-current socket",
            Self::Focused => "focused",
            Self::Unread => "unread",
            Self::FocusedPane => "focused pane",
            Self::ActivityThinking => "thinking",
            Self::ActivityToolUse => "tool use",
            Self::ActivityAwaitingInput => "awaiting input",
            Self::ActivityIdle => "idle",
            Self::ActivityUnknown => "unknown state",
        }
    }
}

/// Builder for collecting and printing legends
#[derive(Default)]
pub struct Legend {
    symbols: HashSet<LegendSymbol>,
}

impl Legend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a symbol to the legend
    #[allow(dead_code)]
    pub fn add(&mut self, symbol: LegendSymbol) -> &mut Self {
        self.symbols.insert(symbol);
        self
    }

    /// Add multiple symbols
    pub fn add_all(&mut self, symbols: &[LegendSymbol]) -> &mut Self {
        for s in symbols {
            self.symbols.insert(*s);
        }
        self
    }

    /// Check if any symbols were added
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    /// Print the legend (if non-empty)
    ///
    /// Output format: dim text showing symbol = description pairs
    pub fn print(&self) {
        if self.symbols.is_empty() {
            return;
        }

        let dim = Style::new().dim();

        // Group by category for cleaner output
        let mut parts = Vec::new();

        // Socket symbols (in logical order)
        let socket_order = [
            LegendSymbol::SocketCurrent,
            LegendSymbol::SocketOther,
            LegendSymbol::SocketDead,
            LegendSymbol::SocketWarning,
        ];
        for sym in socket_order {
            if self.symbols.contains(&sym) {
                parts.push(format!("{}={}", sym.styled(), sym.description()));
            }
        }

        // Window state symbols
        let window_order = [
            LegendSymbol::Focused,
            LegendSymbol::Unread,
            LegendSymbol::FocusedPane,
        ];
        for sym in window_order {
            if self.symbols.contains(&sym) {
                parts.push(format!("{}={}", sym.styled(), sym.description()));
            }
        }

        // Activity state symbols
        let activity_order = [
            LegendSymbol::ActivityThinking,
            LegendSymbol::ActivityToolUse,
            LegendSymbol::ActivityAwaitingInput,
            LegendSymbol::ActivityIdle,
            LegendSymbol::ActivityUnknown,
        ];
        for sym in activity_order {
            if self.symbols.contains(&sym) {
                parts.push(format!("{}={}", sym.styled(), sym.description()));
            }
        }

        if !parts.is_empty() {
            println!();
            println!("{}", dim.apply_to(parts.join("  ")));
        }
    }
}

/// Preset legends for common command patterns
impl Legend {
    /// Legend for ls command (activity states, focus, unread, socket warnings)
    pub fn for_ls() -> Self {
        let mut legend = Self::new();
        legend.add_all(&[
            LegendSymbol::Focused,
            LegendSymbol::Unread,
            LegendSymbol::ActivityThinking,
            LegendSymbol::ActivityToolUse,
            LegendSymbol::ActivityAwaitingInput,
            LegendSymbol::ActivityIdle,
            LegendSymbol::ActivityUnknown,
            LegendSymbol::SocketWarning,
        ]);
        legend
    }

    /// Legend for ls-panes command
    pub fn for_ls_panes() -> Self {
        let mut legend = Self::new();
        legend.add_all(&[
            LegendSymbol::SocketCurrent,
            LegendSymbol::SocketOther,
            LegendSymbol::FocusedPane,
        ]);
        legend
    }

    /// Legend for ls-sockets command
    pub fn for_ls_sockets() -> Self {
        let mut legend = Self::new();
        legend.add_all(&[
            LegendSymbol::SocketCurrent,
            LegendSymbol::SocketOther,
            LegendSymbol::SocketDead,
            LegendSymbol::Focused,
        ]);
        legend
    }

    /// Legend for ls-terminals command
    pub fn for_ls_terminals() -> Self {
        let mut legend = Self::new();
        legend.add_all(&[
            LegendSymbol::SocketCurrent,
            LegendSymbol::SocketOther,
            LegendSymbol::SocketDead,
        ]);
        legend
    }
}
