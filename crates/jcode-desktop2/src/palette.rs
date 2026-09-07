//! The command palette: a fuzzy type-to-act surface over the desktop's real
//! actions.
//!
//! The app has a large chord map (see `keymap`) but no single discoverable
//! entry point, so a new user must already know the keys. The palette is that
//! entry: one modal (`Cmd/Ctrl+P`), a query line, and a filtered list of
//! actions the app can actually execute. It does not invent a second command
//! grammar — it dispatches the same `Action`s the keyboard already reaches, so
//! the palette and the chord map can never disagree about what a row does.
//!
//! This module is pure. It owns the command registry, the query, and the
//! filtering; the renderer draws it and the App commits a row, mirroring how
//! `resume` separates state from drawing.

use crate::keymap::Action;

/// One command the palette can run. `label` is what the user reads and types
/// against; `hint` is the chord/description shown beside it; dispatching the
/// row re-uses the same `Action` the keyboard would.
#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    pub label: &'static str,
    /// The chord or short description shown on the row's trailing edge, so
    /// the palette also *teaches* the shortcut behind the command.
    pub hint: &'static str,
    pub action: Action,
}

/// The desktop's genuine command surface. Kept deliberately short and honest:
/// each row must resolve to an action the app really implements, so a novice
/// is never pointed at something that silently does nothing.
pub const COMMANDS: &[Command] = &[
    Command { label: "New session", hint: "Ctrl/Cmd+T", action: Action::SessionNew },
    Command { label: "Resume a saved session", hint: "Ctrl/Cmd+R", action: Action::ToggleResume },
    Command { label: "Choose model", hint: "Ctrl/Cmd+M", action: Action::ToggleModelPicker },
    Command { label: "Settings", hint: "Ctrl/Cmd+,", action: Action::ToggleSettings },
    Command { label: "Toggle dark / light theme", hint: "Ctrl+Shift+D", action: Action::ToggleTheme },
    Command { label: "Step through a reply's reasoning", hint: "Ctrl+Shift+R", action: Action::CycleReasoningDisplay },
    Command { label: "Help", hint: "F1", action: Action::ToggleHelp },
];

/// The palette's modal state: whether it is open, the query, and where the
/// highlight sits. Pure, so the whole surface is testable without a window.
#[derive(Clone, Debug, Default)]
pub struct Palette {
    open: bool,
    query: String,
    cursor: usize,
}

impl Palette {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The rows shown for the current query, in order. Rebuilt per call: the
    /// registry is tiny and the query usually changes every keystroke, so a
    /// cache would be one more thing that can disagree with the highlight.
    #[allow(dead_code)] // consumed by the renderer and the App's row hit-test
    pub fn rows(&self) -> Vec<&'static Command> {
        let query = self.query.trim();
        let mut matched: Vec<&'static Command> = COMMANDS
            .iter()
            .filter(|command| {
                query.is_empty()
                    || jcode_fuzzy::fuzzy_match(query, command.label).is_some()
            })
            .collect();
        if !query.is_empty() {
            // Order by fuzzy score, best match first, so typing converges on
            // the intended command rather than the first row of the registry.
            matched.sort_by_key(|command| {
                let score = jcode_fuzzy::fuzzy_score(query, command.label).unwrap_or(i32::MIN);
                std::cmp::Reverse(score)
            });
        }
        matched
    }

    /// Filtered row at `index`, if any (clamped to the list).
    #[allow(dead_code)]
    pub fn row(&self, index: usize) -> Option<&'static Command> {
        self.rows().get(index).copied()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn clamp(&mut self) {
        let len = self.rows().len();
        self.cursor = self.cursor.min(len.saturating_sub(1));
    }

    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.cursor = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn next(&mut self) {
        let len = self.rows().len();
        if len > 0 {
            self.cursor = (self.cursor + 1) % len;
        }
    }

    pub fn prev(&mut self) {
        let len = self.rows().len();
        if len > 0 {
            self.cursor = (self.cursor + len - 1) % len;
        }
    }

    /// Append `ch` to the query, keeping the highlight on the first row.
    pub fn type_char(&mut self, ch: char) {
        if !ch.is_control() {
            self.query.push(ch);
            self.cursor = 0;
        }
    }

    /// Backspace one char from the query, keeping the highlight on the first
    /// row.
    pub fn backspace(&mut self) {
        self.query.pop();
        self.cursor = 0;
    }

    /// The command the highlight is on, ready to run.
    pub fn selected(&self) -> Option<&'static Command> {
        self.row(self.cursor)
    }

    /// Point the highlight at `index` (pointer click), clamped to the list.
    pub fn select_row(&mut self, index: usize) -> bool {
        if index < self.rows().len() {
            self.cursor = index;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_lists_every_command_in_registry_order() {
        let mut palette = Palette::default();
        palette.open();
        assert_eq!(palette.rows().len(), COMMANDS.len());
        assert_eq!(
            palette.rows().iter().map(|c| c.label).collect::<Vec<_>>(),
            COMMANDS.iter().map(|c| c.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn typing_funnels_to_a_command_and_reorders_by_score() {
        let mut palette = Palette::default();
        palette.open();
        for ch in "theme".chars() {
            palette.type_char(ch);
        }
        let rows = palette.rows();
        assert!(
            rows.iter().any(|c| c.label == "Toggle dark / light theme"),
            "theme command vanished from the filtered set"
        );
        // Score ordering puts the closest label first.
        assert_eq!(rows[0].label, "Toggle dark / light theme");
    }

    #[test]
    fn backspace_widens_again_and_clearing_restores_the_full_list() {
        let mut palette = Palette::default();
        palette.open();
        palette.type_char('n');
        assert!(palette.rows().len() < COMMANDS.len());
        palette.backspace();
        assert_eq!(palette.rows().len(), COMMANDS.len());
    }

    #[test]
    fn cursor_wraps_and_clamps_to_the_filtered_list() {
        let mut palette = Palette::default();
        palette.open();
        let len = palette.rows().len();
        // Cursor starts on the first row. Moving down past the last wraps to 0.
        for _ in 0..len.saturating_sub(1) {
            palette.next();
        }
        assert_eq!(palette.cursor(), len - 1, "cursor should rest on the last row");
        palette.next();
        assert_eq!(palette.cursor(), 0, "next past the last row wraps to the top");
        palette.prev();
        assert_eq!(palette.cursor(), len - 1, "prev from the top wraps to the last row");
    }

    #[test]
    fn selected_tracks_the_cursor_and_survives_a_narrowing_query() {
        let mut palette = Palette::default();
        palette.open();
        for ch in "theme".chars() {
            palette.type_char(ch);
        }
        palette.clamp();
        assert_eq!(palette.selected().unwrap().label, "Toggle dark / light theme");
    }
}