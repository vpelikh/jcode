//! The command palette gesture on the `App`: opening, driving, and committing
//! a palette row.
//!
//! Mirror of `app_resume.rs`: the pure state lives in [`crate::palette`], the
//! drawing in the scene, and this is the only place the two meet. A committed
//! row re-dispatches the same `Action` the keyboard would, so the palette is a
//! router onto the existing actions rather than a second set of behaviors.

use crate::{App, keymap};

impl App {
    /// Open or shut the command palette.
    ///
    /// Opening it clears any other overlay so a menu cannot sit under the
    /// palette and fight it for the keyboard: the palette is the single
    /// discoverable entry point, so it must always win pointer and key focus
    /// for the instant it is open.
    pub(crate) fn toggle_palette(&mut self) {
        if self.model.palette.is_open() {
            self.model.palette.close();
        } else {
            self.model.help_open = false;
            self.model.panel.close();
            self.model.model_picker.close();
            self.model.resume.close();
            self.model.overview.abort();
            self.model.palette.open();
        }
        self.request_redraw();
    }

    /// A key went down while the palette owns the keyboard. Returns false when
    /// the chord asked the app to quit, mirroring [`Self::apply`].
    pub(crate) fn palette_keydown(
        &mut self,
        logical_key: &winit::keyboard::Key,
        typed: Option<&str>,
    ) -> bool {
        use winit::keyboard::NamedKey;
        match logical_key {
            winit::keyboard::Key::Named(named) => match named {
                NamedKey::ArrowDown | NamedKey::Tab => self.model.palette.next(),
                NamedKey::ArrowUp => self.model.palette.prev(),
                NamedKey::Enter => return self.palette_commit(),
                NamedKey::Escape => {
                    self.model.palette.close();
                    self.request_redraw();
                    return true;
                }
                NamedKey::Backspace => self.model.palette.backspace(),
                // A bare space types into the query; a modified Space (Ctrl/Cmd/
                // Alt) is a chord, not input — matching how the resume overlay
                // treats it — so it is not swallowed as a literal space.
                NamedKey::Space
                    if !self.modifiers.control_key()
                        && !self.modifiers.super_key()
                        && !self.modifiers.alt_key() =>
                {
                    self.model.palette.type_char(' ');
                }
                _ => {}
            },
            winit::keyboard::Key::Character(_text) => {
                // The chord that opened the palette (Ctrl/Cmd+P) also closes
                // it. Escape is handled in the named arm; this is the fallback
                // so the opening chord is a toggle even mid-gesture.
                if keymap::resolve(logical_key, self.modifiers)
                    == Some(keymap::Action::TogglePalette)
                {
                    self.model.palette.close();
                    self.request_redraw();
                    return true;
                }
                // Everything else types into the query. `typed` is None for
                // chord-only presses, which the named match has already
                // handled.
                if let Some(ch) = typed.and_then(|t| t.chars().next()) {
                    self.model.palette.type_char(ch);
                }
            }
            _ => {}
        }
        self.model.palette.clamp();
        self.request_redraw();
        true
    }

    /// Run the highlighted command and close the palette.
    pub(crate) fn palette_commit(&mut self) -> bool {
        let Some(command) = self.model.palette.selected().map(|c| c.action) else {
            self.model.palette.close();
            self.request_redraw();
            return true;
        };
        self.model.palette.close();
        self.request_redraw();
        self.apply(command, None)
    }
}
