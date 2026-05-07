//! Hackable CLI table renderer.
//!
//! `babel ls` and `babel ls-sessions` describe different things: live terminal
//! panes vs durable conversation history. They still need the same output
//! mechanics: indicator columns, harness swatches, active-row underline, width
//! allocation, and middle snips. This module keeps those mechanics declarative
//! so new status columns are one `ColumnSpec` instead of another hand-rolled
//! print chain.

use console::{colors_enabled, Style, Term};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

#[derive(Clone, Debug)]
pub struct ColumnSpec {
    pub _key: &'static str,
    pub label: &'static str,
    pub min: usize,
    pub max: Option<usize>,
    pub flex: u16,
    pub align: Align,
    pub hidden: bool,
    pub snip: bool,
}

impl ColumnSpec {
    pub fn fixed(_key: &'static str, label: &'static str, width: usize, align: Align) -> Self {
        Self {
            _key,
            label,
            min: width,
            max: Some(width),
            flex: 0,
            align,
            hidden: false,
            snip: true,
        }
    }

    pub fn fit(
        _key: &'static str,
        label: &'static str,
        min: usize,
        max: usize,
        align: Align,
    ) -> Self {
        Self {
            _key,
            label,
            min,
            max: Some(max),
            flex: 0,
            align,
            hidden: false,
            snip: true,
        }
    }

    pub fn flex(_key: &'static str, label: &'static str, min: usize, flex: u16) -> Self {
        Self {
            _key,
            label,
            min,
            max: None,
            flex,
            align: Align::Left,
            hidden: false,
            snip: true,
        }
    }

    pub fn hidden(mut self, hidden: bool) -> Self {
        self.hidden = hidden;
        self
    }

    pub fn snip(mut self, snip: bool) -> Self {
        self.snip = snip;
        self
    }
}

#[derive(Clone, Debug)]
pub struct TableCell {
    pub text: String,
    pub style: Style,
    pub truecolor: Option<TruecolorStyle>,
}

impl TableCell {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            truecolor: None,
        }
    }

    pub fn truecolor(text: impl Into<String>, style: TruecolorStyle) -> Self {
        Self {
            text: text.into(),
            style: Style::new(),
            truecolor: Some(style),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TruecolorStyle {
    pub fg: (u8, u8, u8),
    pub bold: bool,
    pub dim: bool,
}

impl TruecolorStyle {
    pub fn new(fg: (u8, u8, u8)) -> Self {
        Self {
            fg,
            bold: false,
            dim: false,
        }
    }

    pub fn bold(mut self, bold: bool) -> Self {
        self.bold = bold;
        self
    }

    pub fn dim(mut self, dim: bool) -> Self {
        self.dim = dim;
        self
    }

    fn apply_to(self, text: String, underlined: bool) -> String {
        if !colors_enabled() {
            return text;
        }

        let (r, g, b) = self.fg;
        let mut codes = vec![format!("38;2;{r};{g};{b}")];
        if self.bold {
            codes.push("1".to_string());
        }
        if self.dim {
            codes.push("2".to_string());
        }
        if underlined {
            codes.push("4".to_string());
        }
        format!("\x1b[{}m{text}\x1b[0m", codes.join(";"))
    }
}

#[derive(Clone, Debug)]
pub struct TableRow {
    pub cells: Vec<TableCell>,
    pub running: bool,
}

impl TableRow {
    pub fn new(cells: Vec<TableCell>, running: bool) -> Self {
        Self { cells, running }
    }
}

#[derive(Clone, Debug)]
pub struct TableOptions {
    pub gap: usize,
    pub headers: bool,
    pub terminal_width: Option<usize>,
}

impl Default for TableOptions {
    fn default() -> Self {
        Self {
            gap: 2,
            headers: false,
            terminal_width: Some(Term::stdout().size().1 as usize),
        }
    }
}

pub fn print_table(columns: &[ColumnSpec], rows: &[TableRow], options: &TableOptions) {
    let active_columns: Vec<(usize, &ColumnSpec)> = columns
        .iter()
        .enumerate()
        .filter(|(_, column)| !column.hidden)
        .collect();
    if active_columns.is_empty() {
        return;
    }

    let widths = allocate_widths(&active_columns, rows, options);
    if options.headers {
        print_header(&active_columns, &widths, options.gap);
    }
    for row in rows {
        print_row(row, &active_columns, &widths, options.gap);
    }
}

fn allocate_widths(
    columns: &[(usize, &ColumnSpec)],
    rows: &[TableRow],
    options: &TableOptions,
) -> Vec<usize> {
    let mut widths = columns
        .iter()
        .map(|(cell_idx, column)| {
            let label_width = display_width(column.label);
            let content_width = rows
                .iter()
                .filter_map(|row| row.cells.get(*cell_idx))
                .map(|cell| display_width(&cell.text))
                .max()
                .unwrap_or(0);
            let natural = column.min.max(label_width).max(content_width);
            column.max.map(|max| natural.min(max)).unwrap_or(natural)
        })
        .collect::<Vec<_>>();

    let gap_total = options.gap.saturating_mul(columns.len().saturating_sub(1));
    let Some(limit) = options.terminal_width else {
        return widths;
    };
    let width_total = widths.iter().sum::<usize>() + gap_total;
    if width_total <= limit {
        let spare = limit - width_total;
        grow_flex_columns(columns, &mut widths, spare);
    } else {
        let excess = width_total - limit;
        shrink_columns(columns, &mut widths, excess);
    }
    widths
}

fn grow_flex_columns(columns: &[(usize, &ColumnSpec)], widths: &mut [usize], spare: usize) {
    let flex_indices = columns
        .iter()
        .enumerate()
        .filter_map(|(idx, (_, column))| (column.flex > 0).then_some(idx))
        .collect::<Vec<_>>();
    let total_flex = flex_indices
        .iter()
        .map(|idx| columns[*idx].1.flex as usize)
        .sum::<usize>();
    if spare == 0 || total_flex == 0 {
        return;
    }
    let mut remaining = spare;
    for (pos, idx) in flex_indices.iter().copied().enumerate() {
        let add = (spare * columns[idx].1.flex as usize) / total_flex;
        let add = if pos == flex_indices.len() - 1 {
            remaining
        } else {
            add.min(remaining)
        };
        widths[idx] += add;
        remaining = remaining.saturating_sub(add);
    }
}

fn shrink_columns(columns: &[(usize, &ColumnSpec)], widths: &mut [usize], mut excess: usize) {
    for idx in columns
        .iter()
        .enumerate()
        .filter_map(|(idx, (_, column))| (column.flex > 0).then_some(idx))
        .collect::<Vec<_>>()
    {
        if excess == 0 {
            return;
        }
        let min = columns[idx].1.min;
        let take = widths[idx].saturating_sub(min).min(excess);
        widths[idx] -= take;
        excess -= take;
    }

    for idx in (0..columns.len()).rev() {
        if excess == 0 {
            return;
        }
        let min = columns[idx].1.min;
        let take = widths[idx].saturating_sub(min).min(excess);
        widths[idx] -= take;
        excess -= take;
    }
}

fn print_header(columns: &[(usize, &ColumnSpec)], widths: &[usize], gap: usize) {
    let dim = Style::new().dim();
    for (idx, (_, column)) in columns.iter().enumerate() {
        if idx > 0 {
            print!("{}", " ".repeat(gap));
        }
        print!(
            "{}",
            dim.apply_to(pad_cell(column.label, widths[idx], column.align))
        );
    }
    println!();
}

fn print_row(row: &TableRow, columns: &[(usize, &ColumnSpec)], widths: &[usize], gap: usize) {
    let gap_text = " ".repeat(gap);
    let gap_style = if row.running {
        Style::new().underlined()
    } else {
        Style::new()
    };
    for (idx, (cell_idx, column)) in columns.iter().enumerate() {
        if idx > 0 {
            print!("{}", gap_style.apply_to(&gap_text));
        }
        let Some(cell) = row.cells.get(*cell_idx) else {
            print!("{}", " ".repeat(widths[idx]));
            continue;
        };
        let text = fit_cell(&cell.text, widths[idx], column.align, column.snip);
        if let Some(truecolor) = cell.truecolor {
            print!("{}", truecolor.apply_to(text, row.running));
            continue;
        }
        let style = if row.running {
            cell.style.clone().underlined()
        } else {
            cell.style.clone()
        };
        print!("{}", style.apply_to(text));
    }
    println!();
}

fn fit_cell(text: &str, width: usize, align: Align, snip: bool) -> String {
    let clipped = if snip {
        middle_truncate(text, width)
    } else {
        take_width_from_start(text, width)
    };
    pad_cell(&clipped, width, align)
}

fn pad_cell(text: &str, width: usize, align: Align) -> String {
    let used = display_width(text);
    if used >= width {
        return text.to_string();
    }
    let pad = " ".repeat(width - used);
    match align {
        Align::Left => format!("{text}{pad}"),
        Align::Right => format!("{pad}{text}"),
    }
}

fn middle_truncate(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    const MARKER: &str = " ⌿ ";
    let marker_width = display_width(MARKER);
    if width <= marker_width + 2 {
        return take_width_from_start(text, width);
    }
    let keep = width - marker_width;
    let head_width = keep / 2;
    let tail_width = keep.saturating_sub(head_width);
    format!(
        "{}{}{}",
        take_width_from_start(text, head_width),
        MARKER,
        take_width_from_end(text, tail_width)
    )
}

fn take_width_from_start(text: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out
}

fn take_width_from_end(text: &str, max_width: usize) -> String {
    let mut chars = Vec::new();
    let mut width = 0;
    for ch in text.chars().rev() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        chars.push(ch);
        width += ch_width;
    }
    chars.into_iter().rev().collect()
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}
