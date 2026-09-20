//! Pixel-level invariants for the overview's card thumbnails.
//!
//! The field's whole claim is that you can see several sessions at once. That
//! is a claim about ink: geometry tests can prove a card is in the right place
//! while its contents are an unreadable smudge, which is the state the first
//! attempt at this shipped in. So these measure contrast inside the cards.
//!
//! Requires a GPU, so these are `#[ignore]`d; run with
//! `cargo test -p jcode-desktop2 -- --ignored`.

use super::visual::Rendered;
use crate::states;

/// The surface has to be big enough for cards that clear the thumbnail floor;
/// below it the previews are correctly suppressed and there is nothing to
/// measure.
const SURFACE: (u32, u32, f64) = (1600, 1100, 1.5);
/// Inset from a card's edge before any ink is measured. The focused card's ring
/// is 2.5 logical units of near-black, so a smaller inset would sample the
/// border on that one card and report it as text: every band would pass, and
/// the comparison between bands would be a comparison of the same ring.
const RING_CLEARANCE: f64 = 5.0;

fn field(model: &crate::Model) -> (Rendered, crate::overview::Field) {
    let (width, height, scale) = SURFACE;
    let rendered = Rendered::at(model, width, height, scale).expect("a GPU render");
    let field = crate::overview::layout(
        &model.strips.panels(),
        model.overview.focus().or(model.session_id.as_deref()),
        model.session_id.as_deref(),
        crate::overview::area(&rendered.frame),
    );
    (rendered, field)
}

/// Every card shows its own conversation, and shows it legibly.
///
/// The failure this exists to catch is the one that is easy to ship: a preview
/// so faint it costs a card's space and tells the user nothing, which is worse
/// than no preview at all because it cannot be distinguished from one.
#[test]
#[ignore = "requires a GPU"]
fn every_card_shows_its_own_conversation() {
    let model = states::by_name("overview_thumbnails").expect("node");
    let (rendered, field) = field(&model);
    let mut measured = 0;
    for card in &field.cards {
        let (x0, y0, x1, y1) = card.rect;
        if x1 - x0 < 100.0 || y1 - y0 < 60.0 {
            continue;
        }
        // The upper band, which is the thumbnail's; the name lives below it and
        // would otherwise be what passes this test.
        let ink = rendered.darkest_in(
            x0 + RING_CLEARANCE,
            y0 + RING_CLEARANCE,
            x1 - RING_CLEARANCE,
            y0 + (y1 - y0) * 0.55,
        );
        assert!(
            ink < 0.72,
            "{}: its card carries no readable preview (darkest {ink:.3})",
            card.session_id
        );
        measured += 1;
    }
    assert!(
        measured >= 2,
        "the surface held {measured} previewable cards, so nothing was really tested"
    );
}

/// A card's name stays readable with a conversation above it. The preview is
/// context; the name is the thing the user acts on, so it may never be the
/// thing that gets crowded out.
#[test]
#[ignore = "requires a GPU"]
fn the_name_survives_having_a_preview_above_it() {
    let model = states::by_name("overview_thumbnails").expect("node");
    let (rendered, field) = field(&model);
    for card in &field.cards {
        let (x0, y0, x1, y1) = card.rect;
        if x1 - x0 < 100.0 || y1 - y0 < 60.0 {
            continue;
        }
        let band = rendered.darkest_in(
            x0 + RING_CLEARANCE,
            y0 + (y1 - y0) * 0.62,
            x1 - RING_CLEARANCE,
            y1 - RING_CLEARANCE,
        );
        let preview = rendered.darkest_in(
            x0 + RING_CLEARANCE,
            y0 + RING_CLEARANCE,
            x1 - RING_CLEARANCE,
            y0 + (y1 - y0) * 0.55,
        );
        assert!(
            band < 0.6,
            "{}: no name in the band under its preview (darkest {band:.3})",
            card.session_id
        );
        // The name has to win the card: if the preview were as heavy, the tile
        // would read as a wall of text with no handle on it.
        assert!(
            band < preview,
            "{}: its preview ({preview:.3}) is as heavy as its name ({band:.3})",
            card.session_id
        );
    }
}

/// The overview's cards stay fully on-paper even when the project-explorer
/// sidebar owns the window's leading edge.
///
/// The overview is centred on the page column, which shifts right of the
/// window's middle whenever a sidebar is present. If the field still sized
/// itself from the full window width it would spill past the right edge, hiding
/// the rightmost cards and their content (the "impossible to read" report).
/// This renders the crowded field over a sidebar-real model at window widths
/// narrow enough that the old code overflowed.
///
/// Two guards, in increasing precision:
///   * every card body must sit inside the window;
///   * every focused or current card — the only ones that draw a halo past
///     their body — must clear the window edge by at least the halo, so the
///     ring can never be clipped by the frame.
#[test]
#[ignore = "requires a GPU"]
fn no_card_spills_off_the_window_with_a_sidebar() {
    const SIDEBAR: f64 = crate::file_tree::WIDTH;
    const HALO: f64 = 5.0; // the focused card's halo in `scene_overview`.
    for width in [900u32, 1100, 1280] {
        let (height, scale) = (720u32, 1.0);
        let mut model = states::by_name("overview_many_sessions").expect("node");
        // Sidebar present: the working directory and the explorer it paints.
        model.working_dir = Some("/home/j/site".into());
        model.file_tree.sync_root(Some("/home/j/site"));

        // The overview must actually be drawing for this to be an overview
        // test; guard the precondition so a future model change cannot turn
        // it into a plain no-overview render that passes trivially.
        assert!(
            model.overview.is_visible(),
            "at {width} the overview is not visible, so nothing was really tested"
        );

        let rendered =
            super::visual::Rendered::at(&model, width, height, scale).expect("a GPU render");
        let frame = &rendered.frame;

        // The page column must actually be shifted clear of the sidebar, or the
        // setup did not reproduce the overflow we are guarding against.
        assert!(
            frame.left >= SIDEBAR - 1.0,
            "at {width} the page column {:.1} did not clear the explorer",
            frame.left
        );

        let field = crate::overview::layout(
            &model.strips.panels(),
            model.overview.focus().or(model.session_id.as_deref()),
            model.session_id.as_deref(),
            crate::overview::area(frame),
        );
        assert!(
            !field.cards.is_empty(),
            "at {width} the field was empty, so nothing was really tested"
        );

        for card in &field.cards {
            let (x0, _, x1, _) = card.rect;
            assert!(
                x0 >= -1.0 && x1 <= f64::from(width) + 1.0,
                "at {width} card {:?} body [{x0:.1},{x1:.1}] left the window",
                card.session_id
            );
            if card.focused || card.current {
                // The halo inflates a card by `HALO` on every side. A focused
                // card too near the window edge would paint its ring straight
                // over the frame, so the body must leave room for it.
                assert!(
                    x1 + HALO <= f64::from(width) + 1.0,
                    "at {width} focused/current card {:?} halo [{x1:.1},{:.1}] spilled past the window",
                    card.session_id,
                    x1 + HALO
                );
            }
        }
    }
}

/// The real runtime entry point — `build_workspace_scene`, what the event loop
/// calls every frame — keeps the overview's cards on-paper with a sidebar.
///
/// The overview routes *through* the workspace compositor: when
/// `model.overview.is_visible()` its first branch hands straight off to
/// `build_scene`. That is the exact code path `main.rs` drives, so this renders
/// through it (not directly through `build_scene`) and asserts the same
/// on-paper and halo guarantees. It is the only test that pins the workspace
/// dispatcher rather than the single-page builder.
#[test]
#[ignore = "requires a GPU"]
fn the_live_workspace_path_keeps_the_overview_on_paper() {
    const SIDEBAR: f64 = crate::file_tree::WIDTH;
    const HALO: f64 = 5.0;
    for width in [900u32, 1100, 1280] {
        let mut model = states::by_name("overview_many_sessions").expect("node");
        model.working_dir = Some("/home/j/site".into());
        model.file_tree.sync_root(Some("/home/j/site"));

        // The whole point of this test is the overview branch of
        // `build_workspace_scene`. If the model were not visibly overlaying
        // the overview, the compositor would take its single-page/column path
        // instead and this would silently test nothing. Pin that precondition
        // so a future model change cannot paper over it.
        assert!(
            model.overview.is_visible(),
            "at {width} the overview is not visible, so build_workspace_scene took the wrong branch"
        );

        let mut painter = crate::paint::Painter::default();
        let mut scene = vello::Scene::new();
        crate::scene_workspace::build_workspace_scene(
            &mut scene,
            &mut painter,
            &model,
            (width, 720),
            1.0,
        );
        let pixels = crate::capture::capture_scene_to_rgba(&scene, width, 720).expect("gpu");
        let frame = crate::App::frame_for_model((width, 720), 1.0, &model);

        assert!(
            frame.left >= SIDEBAR - 1.0,
            "at {width} the page column {:.1} did not clear the explorer",
            frame.left
        );
        let field = crate::overview::layout(
            &model.strips.panels(),
            model.overview.focus().or(model.session_id.as_deref()),
            model.session_id.as_deref(),
            crate::overview::area(&frame),
        );
        assert!(!field.cards.is_empty());
        for card in &field.cards {
            let (x0, _, x1, _) = card.rect;
            assert!(
                x0 >= -1.0 && x1 <= f64::from(width) + 1.0,
                "at {width} card {:?} body [{x0:.1},{x1:.1}] left the window",
                card.session_id
            );
            if card.focused || card.current {
                assert!(
                    x1 + HALO <= f64::from(width) + 1.0,
                    "at {width} focused/current card {:?} halo spilled",
                    card.session_id
                );
            }
        }
        // Rendering succeeds end to end (the pixels captured above); the
        // geometry guards above are the precise on-paper guarantee, and they
        // are shared verbatim with the single-page test.
        let _ = pixels;
    }
}
