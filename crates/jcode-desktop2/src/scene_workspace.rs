//! Composition of the live session page with cached neighboring previews.
//!
//! The focused model is rendered through the existing full scene builder and
//! remains the only interactive page. Other columns use the same builder with a
//! read-only model made from their cached `Peek` transcript. All pages are then
//! appended into one Vello scene at the camera positions supplied by
//! [`crate::workspace`], each clipped to a rounded window with its own border
//! ring, so where one session ends and the next begins is legible at a glance.

use crate::{paint, scene, scene_file_tree, strip, workspace, Model};
use vello::kurbo::{Affine, Rect, RoundedRect, Stroke};
use vello::Scene;

/// Corner radius of a session page, in logical units. Soft enough to read as
/// a window, square enough that the transcript inside does not lose its
/// margins to the curve.
const PAGE_CORNER: f64 = 10.0;
/// Ring weight around an unfocused page, and around the focused one. The
/// focused ring is the compositor's focus border: it is the whole signal for
/// "this is the page your keys go to", so it is unmistakably heavier.
const PAGE_RING: f64 = 1.0;
const PAGE_RING_FOCUS: f64 = 2.0;

/// Build the actual window scene. The overview keeps the legacy full-window
/// path: it is already a view of every session, and nesting that spatial
/// navigator inside one workspace column would make both modes worse.
pub fn build_workspace_scene(
    output: &mut Scene,
    painter: &mut paint::Painter,
    model: &Model,
    size: (u32, u32),
    scale: f64,
) {
    if model.overview.is_visible() {
        scene::build_scene(output, painter, model, size, scale);
        scene_file_tree::draw(output, painter, model, size, scale);
        return;
    }
    let entries = model.strips.panels();
    let columns = workspace::placement(
        &model.strips,
        &model.workspace,
        model.session_id.as_deref(),
        (f64::from(size.0), f64::from(size.1)),
        workspace::GAP * scale,
    );
    // A lone page at rest is the legacy full-window layout: chrome around a
    // window with no neighbors would be a picture frame on a wall with one
    // painting.
    if columns.len() <= 1 && !model.workspace.is_animating() {
        scene::build_scene(output, painter, model, size, scale);
        scene_file_tree::draw(output, painter, model, size, scale);
        return;
    }

    // The gutters belong to the workspace, not to any session. A quiet wash
    // makes the page boundaries legible without adding permanent chrome.
    output.fill(
        vello::peniko::Fill::NonZero,
        Affine::IDENTITY,
        model.theme.wash,
        None,
        &Rect::new(0.0, 0.0, f64::from(size.0), f64::from(size.1)),
    );

    let inset = workspace::VERTICAL_INSET * scale;
    let page_height = (f64::from(size.1) - inset * 2.0).max(1.0) as u32;
    let viewport = (f64::from(size.0), f64::from(size.1));

    // Neighbors first, then the live page, so the focused ring is never
    // washed over by a neighbor's edge antialiasing.
    let mut ordered: Vec<&workspace::Column> = columns
        .iter()
        .filter(|column| column.is_visible(viewport))
        .collect();
    ordered.sort_by_key(|column| column.focused);
    for column in ordered {
        let width = column.width.round().max(1.0) as u32;
        let mut child = Scene::new();
        // The focused column is the live page: placement anchors focus to the
        // attached session, falling back to the strip's focus during the
        // frames before an attach resolves, exactly when the live model's
        // "attaching" status is the honest thing to show.
        if column.focused {
            scene::build_scene(&mut child, painter, model, (width, page_height), scale);
        } else {
            let Some(entry) = entries.get(column.index) else {
                continue;
            };
            let retained = retained_session_model(model, entry);
            scene::build_scene(&mut child, painter, &retained, (width, page_height), scale);
        }

        let page = RoundedRect::new(
            column.x,
            inset + column.y,
            column.x + column.width,
            inset + column.y + f64::from(page_height),
            PAGE_CORNER * scale,
        );
        // The page is clipped to its rounded window so nothing it draws (a
        // wide code block, a selection band) can bleed into the gutter or
        // onto a neighbor: the boundary is a wall, not a suggestion.
        output.push_layer(
            vello::peniko::Fill::NonZero,
            vello::peniko::Mix::Normal,
            1.0,
            Affine::IDENTITY,
            &page,
        );
        output.append(
            &child,
            Some(Affine::translate((column.x, inset + column.y))),
        );
        output.pop_layer();

        // The ring sits outside the clip so it stays crisp at every corner.
        // A focused page takes the accent like every other focused surface, so
        // "the keys will land in this page" reads the same way everywhere.
        let (color, weight) = if column.focused {
            (model.theme.accent, PAGE_RING_FOCUS)
        } else {
            (model.theme.rule, PAGE_RING)
        };
        output.stroke(
            &Stroke::new(weight * scale),
            Affine::IDENTITY,
            color,
            None,
            &page,
        );
    }
    scene_file_tree::draw(output, painter, model, size, scale);
}

/// Make the immutable scene input for one retained session. This deliberately
/// does not clone the live model wholesale: doing so would copy every cached
/// Peek transcript on every frame and would leak the focused page's draft,
/// selection, overlays, and animation state into its neighbors.
fn retained_session_model(source: &Model, entry: &strip::Panel) -> Model {
    let cached = source.peeks.get(&entry.session_id);
    let loading = cached.is_none();
    let transcript = cached.cloned().unwrap_or_default();
    let mut session_strip = source.strips.clone();
    session_strip.focus_session(&entry.session_id);

    Model {
        theme: source.theme,
        theme_preference: source.theme_preference,
        meta: source.meta.clone(),
        status: String::new(),
        session_id: Some(entry.session_id.clone()),
        transcript,
        editor: crate::editor::Editor::default(),
        caret: crate::caret::Caret::default(),
        busy: false,
        activity: crate::activity::Activity::default(),
        focused: false,
        scroll: 0.0,
        selection: None,
        notice: loading.then(|| "loading session…".to_string()),
        attachments: 0,
        attachment_previews: Vec::new(),
        attachment_preview: None,
        failure: None,
        donut: None,
        spin: crate::donut::Spin::default(),
        hint: source.hint,
        stream: crate::stream::Stream::default(),
        smooth: crate::scroll::Smooth::default(),
        strips: session_strip,
        workspace: workspace::Workspace::default(),
        overview: crate::overview::Overview::default(),
        peeks: crate::overview::Peeks::default(),
        resume: crate::resume::Picker::default(),
        help_open: false,
        working_dir: entry.working_dir.clone(),
        file_tree: crate::file_tree::FileTree::default(),
        model: None,
        model_picker: crate::model_picker::Picker::default(),
        boot: crate::boot::Boot::default(),
        progress_clock: None,
        settings: source.settings,
        panel: crate::settings::Panel::default(),
        mem: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{Message, Transcript};

    fn transcript(messages: impl IntoIterator<Item = Message>) -> Transcript {
        let mut transcript = Transcript::default();
        for message in messages {
            transcript.push(message);
        }
        transcript
    }

    #[test]
    fn retained_page_uses_the_complete_cached_transcript() {
        let mut source = Model::default();
        let entry = strip::Panel::new("neighbor", Some("/work/neighbor"));
        source.strips = strip::Strips::build(
            vec![strip::Panel::new("live", Some("/work/live")), entry.clone()],
            Some("live"),
        );
        let transcript = transcript([
            Message::user("a long question that must remain a paragraph"),
            Message::assistant("a complete markdown answer\n\nwith a second paragraph"),
        ]);
        source.peeks.insert("neighbor", transcript.clone());

        let retained = retained_session_model(&source, &entry);

        assert_eq!(retained.transcript, transcript);
        assert_eq!(retained.session_id.as_deref(), Some("neighbor"));
        assert_eq!(retained.working_dir.as_deref(), Some("/work/neighbor"));
        assert_eq!(retained.strips.focused_session(), Some("neighbor"));
        assert_eq!(source.strips.focused_session(), Some("live"));
    }

    #[test]
    fn retained_page_is_a_read_only_settled_shell() {
        let mut source = Model::default();
        let entry = strip::Panel::new("neighbor", None);
        source.editor.insert_str("unsent live draft");
        source.focused = true;
        source.busy = true;
        source.stream = crate::stream::Stream::pinned(0.25);
        source.notice = Some("live notice".into());
        source.overview.open(Some("neighbor"));
        source.peeks.insert(
            "neighbor",
            transcript([Message::assistant("cached answer")]),
        );

        let retained = retained_session_model(&source, &entry);

        assert!(retained.editor.is_empty());
        assert!(!retained.focused);
        assert!(!retained.busy);
        assert!(!retained.stream.is_animating());
        assert_eq!(retained.notice, None);
        assert!(!retained.overview.is_visible());
        assert_eq!(source.editor.text(), "unsent live draft");
        assert!(source.stream.is_animating());
        assert!(source.overview.is_visible());
    }

    #[test]
    fn missing_peek_still_gets_the_full_loading_shell() {
        let source = Model::default();
        let retained = retained_session_model(&source, &strip::Panel::new("pending", None));

        assert!(retained.transcript.is_empty());
        assert_eq!(retained.notice.as_deref(), Some("loading session…"));
        assert!(!retained.focused);
    }

    #[test]
    fn retained_page_builds_with_the_actual_session_scene() {
        let mut source = Model::default();
        let entry = strip::Panel::new("neighbor", Some("/work/neighbor"));
        source.session_id = Some("live".into());
        source.strips = strip::Strips::build(
            vec![
                strip::Panel::new("live", Some("/work/neighbor")),
                entry.clone(),
            ],
            Some("live"),
        );
        source
            .transcript
            .push(Message::assistant("focused session stays live"));
        source.peeks.insert(
            "neighbor",
            transcript([
                Message::user("question with **formatting**"),
                Message::assistant("answer with `code` and\n\nmultiple paragraphs"),
            ]),
        );
        let mut painter = paint::Painter::default();
        let mut output = Scene::new();

        build_workspace_scene(&mut output, &mut painter, &source, (1000, 720), 1.0);
    }

    /// A vertical row slide draws both rows without panicking, including the
    /// departing sessions that only exist as peeks.
    #[test]
    fn a_row_slide_builds_with_both_rows() {
        let mut source = Model::default();
        source.session_id = Some("b1".into());
        source.strips = strip::Strips::build(
            vec![
                strip::Panel::new("a1", Some("/w/jcode")),
                strip::Panel::new("a2", Some("/w/jcode")),
                strip::Panel::new("b1", Some("/w/site")),
            ],
            Some("b1"),
        );
        source.peeks.insert("a1", transcript([Message::user("q")]));
        source.workspace.begin_row_change(
            workspace::Direction::Down,
            vec!["a1".into(), "a2".into()],
            0,
        );

        let mut painter = paint::Painter::default();
        let mut output = Scene::new();
        build_workspace_scene(&mut output, &mut painter, &source, (1000, 720), 1.0);
    }

    /// A workspace-active row (focused session alongside a neighbor) with a
    /// file tree present must still draw the focused session's top-left
    /// sessions button *right of* the explorer, not underneath it. This is the
    /// integration split the layout reservation exists to serve: the page is
    /// clipped to its column and translated by `workspace::placement`, so the
    /// explorer would otherwise sit over it here too.
    #[test]
    #[ignore = "requires a GPU"]
    fn workspace_row_keeps_the_focused_sessions_button_clear_of_the_explorer() {
        let mut source = Model {
            working_dir: Some("/tmp".into()),
            ..Model::default()
        };
        // The icon is checked against a luminance floor, so the palette must
        // be deterministic: `Model::default()` reads the developer's saved
        // theme, which would make this GPU test fail on a dark-theme machine.
        source.theme = crate::theme::Theme::print_light();
        source.theme_preference = crate::theme::ThemeMode::Light;
        source.file_tree.sync_root(Some("/tmp"));
        source.session_id = Some("live".into());
        source.strips = strip::Strips::build(
            vec![
                strip::Panel::new("live", Some("/tmp")),
                strip::Panel::new("neighbor", Some("/tmp")),
            ],
            Some("live"),
        );
        source
            .transcript
            .push(crate::transcript::Message::assistant("hello"));
        source
            .peeks
            .insert("neighbor", transcript([Message::assistant("side")]));

        let size = (1400u32, 900u32);
        let mut painter = crate::paint::Painter::default();
        let mut output = Scene::new();
        build_workspace_scene(&mut output, &mut painter, &source, size, 1.0);
        let Ok(pixels) = crate::capture::capture_scene_to_rgba(&output, size.0, size.1) else {
            eprintln!("skipping: no GPU");
            return;
        };
        let luma = |x: u32, y: u32| {
            let i = ((y * size.0 + x) * 4) as usize;
            (0.2126 * pixels[i] as f64
                + 0.7152 * pixels[i + 1] as f64
                + 0.0722 * pixels[i + 2] as f64)
                / 255.0
        };
        let clip = |v: f64| v.max(0.0) as u32;

        // The focused column's origin from the same placement the render uses,
        // and its native page size (the render builds the child scene at the
        // column width, not the whole window).
        let column = workspace::placement(
            &source.strips,
            &source.workspace,
            source.session_id.as_deref(),
            (f64::from(size.0), f64::from(size.1)),
            workspace::GAP,
        )
        .into_iter()
        .find(|c| c.focused)
        .unwrap();
        let page_height = (f64::from(size.1) - workspace::VERTICAL_INSET * 2.0).max(1.0) as u32;
        let frame = crate::App::frame_for_model((column.width as u32, page_height), 1.0, &source);
        let button = frame.sessions();
        // The focused page reserves the sidebar, so its top-left chrome sits
        // clear of the explorer even after the column is placed in the window.
        assert!(
            button.x0 >= crate::file_tree::WIDTH,
            "focused page reserves no room for the explorer"
        );
        let window_x = column.x + button.x0;
        let mid_y = column.y + workspace::VERTICAL_INSET + button.y0 + button.height() / 2.0;
        let dark =
            (clip(window_x)..=clip(window_x + button.width())).any(|x| luma(x, clip(mid_y)) < 0.9);
        assert!(
            dark,
            "the focused sessions button was not drawn clear of the explorer"
        );
    }
}
