//! The suggestion popup under the prompt: slash commands, `@` files, and
//! reverse history search.

use super::text::{pad_right, truncate, width};
use crate::style::{Color, role};

/// Rows shown at once; the window scrolls with the selection.
pub const WINDOW: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    Slash,
    Mention,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// What the row shows in the name column.
    pub label: String,
    /// Dim text after it.
    pub detail: String,
    /// What replaces the typed token when the row is accepted.
    pub insert: String,
    /// Accepting it with Enter must wait for the user to type something more
    /// (a command with a required argument).
    pub required: bool,
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub kind: MenuKind,
    pub items: Vec<MenuItem>,
    pub sel: usize,
    /// The text the matches were computed for, to notice when it changes.
    pub query: String,
    /// Byte offset in the buffer where the token being completed starts.
    pub token_start: usize,
}

impl Menu {
    pub fn new(kind: MenuKind, items: Vec<MenuItem>, query: String, token_start: usize) -> Self {
        Self { kind, items, sel: 0, query, token_start }
    }

    pub fn selected(&self) -> Option<&MenuItem> {
        self.items.get(self.sel)
    }

    pub fn step(&mut self, down: bool) {
        let count = self.items.len();
        if count == 0 {
            return;
        }
        self.sel = if down { (self.sel + 1) % count } else { (self.sel + count - 1) % count };
    }

    /// The first row of the visible window, keeping the selection in view.
    fn window_start(&self) -> usize {
        if self.items.len() <= WINDOW {
            0
        } else {
            self.sel.saturating_sub(WINDOW - 1).min(self.items.len() - WINDOW)
        }
    }

    pub fn render(&self, cols: usize, colors: Color) -> Vec<String> {
        let start = self.window_start();
        let shown = &self.items[start..(start + WINDOW).min(self.items.len())];
        let label_w = self
            .items
            .iter()
            .map(|item| width(&item.label))
            .max()
            .unwrap_or(0)
            .min(32);
        let mut rows = Vec::new();
        for (offset, item) in shown.iter().enumerate() {
            let selected = start + offset == self.sel;
            let label = pad_right(&truncate(&item.label, label_w), label_w);
            let room = cols.saturating_sub(label_w + 5);
            let detail = truncate(&item.detail, room);
            let row = if selected {
                role(colors, "select", &format!("  {label}  {detail}"))
            } else {
                format!("  {label}  {}", role(colors, "subtle", &detail))
            };
            rows.push(row);
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::text::strip_ansi;

    fn items(count: usize) -> Vec<MenuItem> {
        (0..count)
            .map(|n| MenuItem {
                label: format!("/cmd{n}"),
                detail: format!("does thing {n}"),
                insert: format!("/cmd{n}"),
                required: false,
            })
            .collect()
    }

    #[test]
    fn selection_wraps() {
        let mut menu = Menu::new(MenuKind::Slash, items(3), String::new(), 0);
        menu.step(false);
        assert_eq!(menu.sel, 2);
        menu.step(true);
        assert_eq!(menu.sel, 0);
        assert_eq!(menu.selected().unwrap().label, "/cmd0");
    }

    #[test]
    fn the_window_scrolls_with_the_selection() {
        let mut menu = Menu::new(MenuKind::Slash, items(20), String::new(), 0);
        assert_eq!(menu.render(80, Color::Plain).len(), WINDOW);
        for _ in 0..10 {
            menu.step(true);
        }
        let rows = menu.render(80, Color::Plain);
        assert_eq!(rows.len(), WINDOW);
        assert!(rows.iter().any(|row| row.contains("/cmd10")), "{rows:?}");
        assert!(!rows.iter().any(|row| row.contains("/cmd0 ")), "{rows:?}");
        menu.step(false);
        menu.step(false);
        assert!(menu.render(80, Color::Plain).iter().any(|row| row.contains("/cmd8")));
    }

    #[test]
    fn short_menus_show_every_row_aligned() {
        let menu = Menu::new(MenuKind::Slash, items(3), String::new(), 0);
        let rows = menu.render(60, Color::Plain);
        assert_eq!(rows.len(), 3);
        let column = rows[0].find("does").unwrap();
        assert!(rows.iter().all(|row| row.find("does") == Some(column)), "{rows:?}");
    }

    #[test]
    fn rows_fit_narrow_terminals() {
        let menu = Menu::new(MenuKind::Slash, items(5), String::new(), 0);
        for cols in [10, 20, 40] {
            for color in [Color::Plain, Color::Ansi] {
                for row in menu.render(cols, color) {
                    assert!(width(&row) <= cols.max(10), "{cols}: {:?}", strip_ansi(&row));
                }
            }
        }
    }

    #[test]
    fn the_selected_row_is_highlighted_only_in_colour() {
        let menu = Menu::new(MenuKind::Slash, items(2), String::new(), 0);
        let plain = menu.render(60, Color::Plain);
        let ansi = menu.render(60, Color::Ansi);
        assert_eq!(plain.iter().map(|row| strip_ansi(row)).collect::<Vec<_>>(), plain);
        assert!(ansi[0].contains("\x1b[1m"), "selected row is bold");
        assert!(!ansi[1].contains("\x1b[1m"));
    }
}
