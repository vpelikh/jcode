//! The desktop's theme choice at boot is the whole bug: on macOS and Windows
//! the `system_prefers_dark` probe is silent (it only knows the Linux portals),
//! so a System-preference window would open light even when the desktop asks
//! for dark. The window reports the real system theme through winit, and
//! `Model::resolve_theme_from_system` applies it. These tests lock that seam.

use crate::Model;
use crate::theme::ThemeMode;

/// A System-preference window follows a dark desktop on first show.
#[test]
fn a_system_window_follows_a_dark_desktop_at_boot() {
    let mut model = Model::default();
    model.theme_preference = ThemeMode::System;
    model.theme = crate::theme::Theme::print_light();
    assert!(model.resolve_theme_from_system(Some(true)));
    assert_eq!(model.theme.mode, ThemeMode::Dark);
}

/// ...and a light desktop stays light.
#[test]
fn a_system_window_follows_a_light_desktop_at_boot() {
    let mut model = Model::default();
    model.theme_preference = ThemeMode::System;
    model.theme = crate::theme::Theme::print_dark();
    assert!(model.resolve_theme_from_system(Some(false)));
    assert_eq!(model.theme.mode, ThemeMode::Light);
}

/// A platform winit cannot answer for (Wayland/x11) leaves the existing
/// resolution intact; the Linux probe keeps running the show.
#[test]
fn an_unknown_system_theme_leaves_the_resolution_alone() {
    let mut model = Model::default();
    model.theme_preference = ThemeMode::System;
    model.theme = crate::theme::Theme::print_light();
    assert!(!model.resolve_theme_from_system(None));
    assert_eq!(model.theme.mode, ThemeMode::Light);
}

/// An explicit preference beats the desktop: the user overriding the system
/// must keep winning, even when the desktop would say otherwise. This is what
/// makes the boot re-resolution safe for people who chose `dark` or `light`.
#[test]
fn an_explicit_preference_is_never_overridden() {
    for (preference, resolved, told_dark) in [
        (ThemeMode::Dark, ThemeMode::Dark, false),
        (ThemeMode::Light, ThemeMode::Light, true),
    ] {
        let mut model = Model::default();
        model.theme_preference = preference;
        model.theme = crate::theme::Theme::for_mode(preference, false);
        assert!(
            !model.resolve_theme_from_system(Some(told_dark)),
            "{preference:?} must not report a change (told {told_dark})"
        );
        assert_eq!(
            model.theme.mode, resolved,
            "{preference:?} must not follow the desktop (told {told_dark})"
        );
    }
}

/// A System window that already resolved to what the desktop now reports does
/// not claim a change, so the caller can skip the redundant redraw.
#[test]
fn a_noop_resolution_reports_no_change() {
    let mut model = Model::default();
    model.theme_preference = ThemeMode::System;
    model.theme = crate::theme::Theme::print_dark();
    assert!(!model.resolve_theme_from_system(Some(true)));
    assert_eq!(model.theme.mode, ThemeMode::Dark);
}
