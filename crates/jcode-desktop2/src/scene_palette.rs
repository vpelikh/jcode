//! Rendering for the command palette card.
//!
//! A centred card over the conversation, like the help overlay: the query line
//! on top, the filtered command list below, and a cream wash on the highlighted
//! row so the keyboard highlight and the mouse land on the same band.

use crate::text::ParagraphStyle;
use crate::{Model, layout, text};
use vello::Scene;
use vello::kurbo::{Affine, Rect, RoundedRect};

const VEIL_OPACITY: f32 = 0.18;
const CARD_OPACITY: f32 = 0.995;

pub fn draw_palette(
    scene: &mut Scene,
    text: &mut text::TextSystem,
    model: &Model,
    frame: &layout::Frame,
    scale: f64,
) {
    if !model.palette.is_open() {
        return;
    }
    let theme = &model.theme;
    scene.fill(
        vello::peniko::Fill::NonZero,
        Affine::scale(scale),
        theme.background.with_alpha(VEIL_OPACITY),
        None,
        &Rect::new(0.0, 0.0, frame.width, frame.height),
    );

    let rows = model.palette.rows();
    let card = frame.palette_card(rows.len());
    scene.fill(
        vello::peniko::Fill::NonZero,
        Affine::scale(scale),
        theme.field.with_alpha(CARD_OPACITY),
        None,
        &RoundedRect::from_rect(card, layout::PALETTE_RADIUS),
    );
    scene.stroke(
        &vello::kurbo::Stroke::new(layout::COMPOSER_BORDER),
        Affine::scale(scale),
        theme.field_border,
        None,
        &RoundedRect::from_rect(card, layout::PALETTE_RADIUS),
    );

    draw_query(scene, text, model, frame, scale);
    let source = rows.as_slice();
    draw_list(scene, text, model, source, frame, scale);
}

/// The query line: what the user has typed, or the instruction when empty.
fn draw_query(
    scene: &mut Scene,
    text: &mut text::TextSystem,
    model: &Model,
    frame: &layout::Frame,
    scale: f64,
) {
    let theme = &model.theme;
    let rows = model.palette.rows();
    let card = frame.palette_card(rows.len());
    let box_ = Rect::new(
        card.x0 + layout::PALETTE_TEXT_PAD,
        card.y0 + 4.0,
        card.x1 - layout::PALETTE_TEXT_PAD,
        card.y0 + layout::PALETTE_SEARCH_HEIGHT,
    );
    let query = model.palette.query();
    let (mut label, color) = match query.is_empty() {
        true => ("Type a command, then ↵".to_string(), theme.faint),
        false => (format!("{query}\u{2502}"), theme.text),
    };
    // A single-line field elides rather than wrapping into the list beneath it
    // (visual-checklist rule 3.3). The *displayed* label is elided only; the
    // underlying `palette::query()` keeps the whole string, so matching is
    // never truncated by what the row happens to show.
    let budget = (box_.width() / (f64::from(layout::CAPTION_SIZE) * 0.72)).max(1.0) as usize;
    label = crate::scene::elide(&label, budget);
    text.draw_paragraph_scaled(
        scene,
        &label,
        (box_.x0, box_.y0 + 4.0),
        (box_.width().max(1.0)) as f32,
        ParagraphStyle {
            font_size: layout::CAPTION_SIZE,
            color,
            letter_spacing_em: 0.05,
            ..Default::default()
        },
        scale,
    );
    // A hairline under the query distinguishes it from the list without
    // spending a whole row on the gap.
    scene.fill(
        vello::peniko::Fill::NonZero,
        Affine::scale(scale),
        theme.rule,
        None,
        &Rect::new(box_.x0, box_.y1 - 1.0, box_.x1, box_.y1),
    );
}

/// The filtered command list. The cursor's row carries a wash so it reads as
/// the chosen action; the hint (the chord it mirrors) trails the row's edge.
fn draw_list(
    scene: &mut Scene,
    text: &mut text::TextSystem,
    model: &Model,
    commands: &[&crate::palette::Command],
    frame: &layout::Frame,
    scale: f64,
) {
    let theme = &model.theme;
    let rows = commands.len();
    for (index, command) in commands.iter().enumerate() {
        let band = frame.palette_row(rows, index);
        if model.palette.cursor() == index {
            scene.fill(
                vello::peniko::Fill::NonZero,
                Affine::scale(scale),
                theme.wash,
                None,
                &RoundedRect::from_rect(band, layout::PALETTE_RADIUS / 2.0),
            );
        }
        let baseline = band.y0 + (band.height() - f64::from(layout::CAPTION_SIZE) * 1.4) / 2.0;
        let label_width = (band.width() * 0.62).max(1.0);
        // A row is exactly PALETTE_ROW_HEIGHT tall, so a label longer than its
        // 62% slot would wrap onto a second line and spill into the row below
        // (visual-checklist rule 3.3, same as the query line). Elide to fit.
        let label_chars =
            (label_width / (f64::from(layout::CAPTION_SIZE) * 0.72)).max(1.0) as usize;
        let label = crate::scene::elide(command.label, label_chars);
        text.draw_paragraph_scaled(
            scene,
            &label,
            (band.x0, baseline),
            label_width as f32,
            ParagraphStyle {
                font_size: layout::CAPTION_SIZE,
                color: if model.palette.cursor() == index {
                    theme.text
                } else {
                    theme.muted
                },
                ..Default::default()
            },
            scale,
        );
        // The hint trails the right edge, in faint ink so it never fights the
        // label: it is the chord the row teaches, not another option to weigh.
        let hint_width = (band.width() * 0.3).max(1.0) as f32;
        text.draw_paragraph_scaled(
            scene,
            command.hint,
            (band.x1 - hint_width as f64, baseline),
            hint_width,
            ParagraphStyle {
                font_size: layout::CAPTION_SIZE,
                color: theme.faint,
                align: text::Align::End,
                ..Default::default()
            },
            scale,
        );
    }
}
