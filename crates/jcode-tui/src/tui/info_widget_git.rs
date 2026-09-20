use super::text::{truncate_smart, truncate_with_ellipsis};
use super::{GitInfo, InfoWidgetData};
use crate::tui::color_support::rgb;
use ratatui::prelude::*;

/// Max characters of the worktree name shown in the git widget before it is
/// truncated. Centralized so the full widget and its compact form agree on
/// exactly how wide a worktree marker can be. Sized to fit common worktree
/// names (e.g. `telegram-anti-censorship`) while still leaving room for the
/// branch and stats to share the line.
const WORKTREE_NAME_MAX: usize = 24;

/// Build the inline worktree marker (e.g. ` ⤷my-worktree`) used by both render
/// paths. Names longer than `WORKTREE_NAME_MAX` are shortened with an explicit
/// ellipsis so a cut name is never mistaken for a complete one.
fn worktree_marker(name: &str) -> String {
    format!(
        " ⤷{}",
        truncate_with_ellipsis(name, WORKTREE_NAME_MAX)
    )
}

pub(super) fn render_git_widget(data: &InfoWidgetData, inner: Rect) -> Vec<Line<'static>> {
    let Some(info) = &data.git_info else {
        return Vec::new();
    };
    if !info.is_interesting() {
        return Vec::new();
    }

    let w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    let mut parts: Vec<Span> = Vec::new();
    parts.push(Span::styled(" ", Style::default().fg(rgb(240, 160, 60))));

    let mut stats_len = 0usize;
    if info.ahead > 0 {
        stats_len += format!(" ↑{}", info.ahead).chars().count();
    }
    if info.behind > 0 {
        stats_len += format!(" ↓{}", info.behind).chars().count();
    }
    if info.modified > 0 {
        stats_len += format!(" ~{}", info.modified).chars().count();
    }
    if info.staged > 0 {
        stats_len += format!(" +{}", info.staged).chars().count();
    }
    if info.untracked > 0 {
        stats_len += format!(" ?{}", info.untracked).chars().count();
    }
    let worktree_mark = info
        .worktree
        .as_deref()
        .map(worktree_marker)
        .unwrap_or_default();
    stats_len += worktree_mark.chars().count();

    let branch_max = w.saturating_sub(2 + stats_len).max(4);
    let branch_display = truncate_smart(&info.branch, branch_max);
    parts.push(Span::styled(
        branch_display,
        Style::default()
            .fg(rgb(200, 200, 210))
            .add_modifier(Modifier::BOLD),
    ));

    if info.worktree.is_some() {
        parts.push(Span::styled(
            worktree_mark,
            Style::default().fg(rgb(120, 160, 220)),
        ));
    }

    if info.modified > 0 {
        parts.push(Span::styled(
            format!(" ~{}", info.modified),
            Style::default().fg(rgb(240, 200, 80)),
        ));
    }
    if info.staged > 0 {
        parts.push(Span::styled(
            format!(" +{}", info.staged),
            Style::default().fg(rgb(100, 200, 100)),
        ));
    }
    if info.untracked > 0 {
        parts.push(Span::styled(
            format!(" ?{}", info.untracked),
            Style::default().fg(rgb(140, 140, 150)),
        ));
    }
    if info.ahead > 0 {
        parts.push(Span::styled(
            format!(" ↑{}", info.ahead),
            Style::default().fg(rgb(100, 200, 100)),
        ));
    }
    if info.behind > 0 {
        parts.push(Span::styled(
            format!(" ↓{}", info.behind),
            Style::default().fg(rgb(255, 140, 100)),
        ));
    }

    lines.push(Line::from(parts));

    let max_files = inner.height.saturating_sub(lines.len() as u16).min(5) as usize;
    for file in info.dirty_files.iter().take(max_files) {
        let display = truncate_smart(file, w.saturating_sub(4));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(display, Style::default().fg(rgb(140, 140, 155))),
        ]));
    }
    if info.dirty_files.len() > max_files {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("+{} more", info.dirty_files.len() - max_files),
                Style::default().fg(rgb(100, 100, 115)),
            ),
        ]));
    }

    lines
}

pub(super) fn render_git_compact(info: &GitInfo, width: u16) -> Vec<Line<'static>> {
    let w = width as usize;
    let mut parts: Vec<Span> = Vec::new();

    // Reserve room for the worktree marker so a long branch plus a linked
    // worktree still fits the widget width instead of overflowing silently.
    let worktree_mark = info.worktree.as_deref().map(worktree_marker);
    let worktree_budget = worktree_mark.as_deref().map_or(0, |m| m.chars().count());
    let branch_max = (w.saturating_sub(12 + worktree_budget)).max(6);
    let branch_display = truncate_smart(&info.branch, branch_max);
    parts.push(Span::styled(" ", Style::default().fg(rgb(240, 160, 60))));
    parts.push(Span::styled(
        branch_display,
        Style::default().fg(rgb(160, 160, 170)),
    ));

    if let Some(mark) = &worktree_mark {
        parts.push(Span::styled(
            mark.clone(),
            Style::default().fg(rgb(120, 160, 220)),
        ));
    }

    if info.ahead > 0 {
        parts.push(Span::styled(
            format!(" ↑{}", info.ahead),
            Style::default().fg(rgb(100, 200, 100)),
        ));
    }
    if info.behind > 0 {
        parts.push(Span::styled(
            format!(" ↓{}", info.behind),
            Style::default().fg(rgb(255, 140, 100)),
        ));
    }
    if info.modified > 0 {
        parts.push(Span::styled(
            format!(" ~{}", info.modified),
            Style::default().fg(rgb(240, 200, 80)),
        ));
    }
    if info.staged > 0 {
        parts.push(Span::styled(
            format!(" +{}", info.staged),
            Style::default().fg(rgb(100, 200, 100)),
        ));
    }
    if info.untracked > 0 {
        parts.push(Span::styled(
            format!(" ?{}", info.untracked),
            Style::default().fg(rgb(140, 140, 150)),
        ));
    }

    vec![Line::from(parts)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_info(branch: &str) -> GitInfo {
        GitInfo {
            branch: branch.to_string(),
            worktree: None,
            modified: 0,
            staged: 0,
            untracked: 0,
            ahead: 0,
            behind: 0,
            dirty_files: Vec::new(),
        }
    }

    /// `render_git_compact` must surface the working-directory branch in the
    /// rendered line. This is the widget-level proof that the scoped branch
    /// (e.g. a worktree's `feat/panel`, not the daemon CWD's `master`) actually
    /// appears in the drawn output.
    #[test]
    fn compact_render_contains_worktree_branch() {
        let info = git_info("feat/panel");
        let lines = render_git_compact(&info, 40);
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            rendered.contains("feat/panel"),
            "rendered git widget must contain the worktree branch, got: {rendered:?}"
        );
    }

    /// Rendering a long worktree branch into a narrow widget must never panic
    /// and must still surface a visible, non-empty slice of the branch (the
    /// truncation keeps the widget from blanking out on deep branch names).
    /// The branch line starts with the branch's own characters (e.g. `fea...`),
    /// so a prefix of the worktree branch remains visible.
    #[test]
    fn compact_render_handles_long_branch_narrow_width() {
        let info = git_info("feature/some-very-long-worktree-branch-name");
        let lines = render_git_compact(&info, 10);
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            rendered.contains('f') && rendered.contains("fea"),
            "long branch must still render a visible prefix of the branch, got: {rendered:?}"
        );
    }

    /// The worktree marker must be budgeted out of the branch allowance, so the
    /// with-worktree line is no wider than the worktree-less line plus the
    /// marker's own width. (Both paths share the same heuristic `w - 12`
    /// branch reserve and stats, so this isolates exactly what the worktree
    /// budgeting must achieve, without demanding an absolute fit the compact
    /// renderer never promised.)
    #[test]
    fn compact_render_budgets_worktree_marker() {
        let mut base = git_info("feature/some-very-long-worktree-branch-name");
        base.ahead = 2;
        base.behind = 1;
        base.modified = 3;
        base.staged = 4;
        base.untracked = 5;

        let mut with_wt = base.clone();
        with_wt.worktree = Some("a-linked-worktree".to_string());
        let marker_len = worktree_marker("a-linked-worktree").chars().count();

        for width in [20u16, 30, 40] {
            let base_line = &render_git_compact(&base, width)[0];
            let wt_line = &render_git_compact(&with_wt, width)[0];
            let base_w: usize = base_line.spans.iter().map(|s| s.content.chars().count()).sum();
            let wt_w: usize = wt_line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(
                wt_w <= base_w + marker_len,
                "adding a worktree marker must be offset by shrinking the branch: \
                 base {base_w}, with-worktree {wt_w}, marker {marker_len} at width {width}"
            );
        }
    }

    /// When there is no git info, the full widget must render nothing (empty
    /// output) rather than panic or emit a stray branch line.
    #[test]
    fn widget_render_empty_when_no_git_info() {
        let data = InfoWidgetData {
            git_info: None,
            ..Default::default()
        };
        assert!(
            render_git_widget(&data, Rect::new(0, 0, 40, 5)).is_empty(),
            "widget must render nothing when git_info is None"
        );
    }

    /// Rendering the full widget (not just compact) also surfaces the branch.
    #[test]
    fn widget_render_contains_branch_when_interesting() {
        let mut info = git_info("feat/panel");
        info.modified = 2; // is_interesting() requires some stat
        let data = InfoWidgetData {
            git_info: Some(info),
            ..Default::default()
        };
        let lines = render_git_widget(&data, Rect::new(0, 0, 40, 5));
        assert!(!lines.is_empty(), "widget must render when repo is dirty");
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            rendered.contains("feat/panel"),
            "rendered widget must contain the worktree branch, got: {rendered:?}"
        );
    }

    /// Realistically-long worktree names (like this repo's own
    /// `telegram-anti-censorship`, 24 chars) must be surfaced in full rather
    /// than clipped to an unreadable fragment.
    #[test]
    fn render_shows_realistic_long_worktree_names_in_full() {
        let mut info = git_info("feat/panel");
        info.worktree = Some("telegram-anti-censorship".to_string());
        info.modified = 2;
        let data = InfoWidgetData {
            git_info: Some(info),
            ..Default::default()
        };
        let lines = render_git_widget(&data, Rect::new(0, 0, 80, 5));
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            rendered.contains("telegram-anti-censorship"),
            "24-char worktree name must render in full, got: {rendered:?}"
        );
    }

    /// When GitInfo carries a linked-worktree name, both render paths must
    /// surface it so the user knows which worktree they are editing in.
    #[test]
    fn render_surfaces_linked_worktree_name() {
        let mut info = git_info("feat/panel");
        info.worktree = Some("wt-panel".to_string());
        info.modified = 2; // full widget requires some stat to be interesting

        // Compact path.
        let compact = render_git_compact(&info, 40);
        let compact_rendered: String = compact
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            compact_rendered.contains("wt-panel"),
            "compact render must surface the worktree name, got: {compact_rendered:?}"
        );

        // Full widget path.
        let data = InfoWidgetData {
            git_info: Some(info),
            ..Default::default()
        };
        let lines = render_git_widget(&data, Rect::new(0, 0, 60, 5));
        assert!(!lines.is_empty(), "widget must render when repo is dirty");
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            rendered.contains("wt-panel"),
            "full widget must surface the worktree name, got: {rendered:?}"
        );
    }

    /// A clean linked worktree (worktree set, zero stats) must not force the
    /// git widget to render: the widget is a *changes* status, so a spotless
    /// worktree should not light it up just because it is a worktree.
    #[test]
    fn clean_worktree_does_not_force_widget_render() {
        let info = git_info("feat/panel");
        // worktree set but is_interesting() false (all stats zero).
        let mut with_wt = info;
        with_wt.worktree = Some("wt-panel".to_string());
        assert!(!with_wt.is_interesting());
        let data = InfoWidgetData {
            git_info: Some(with_wt),
            ..Default::default()
        };
        // The full widget is gated on is_interesting(), so a clean worktree
        // (worktree set but no stats) must render nothing. This is the same
        // gate the compact path goes through at its call site.
        assert!(
            render_git_widget(&data, Rect::new(0, 0, 60, 5)).is_empty(),
            "a clean worktree must not render the git widget"
        );
    }

    /// A main checkout (no linked worktree) must not emit any worktree marker.
    #[test]
    fn render_omits_worktree_marker_when_not_a_linked_worktree() {
        let mut info = git_info("main");
        info.modified = 1;
        let data = InfoWidgetData {
            git_info: Some(info),
            ..Default::default()
        };
        let lines = render_git_widget(&data, Rect::new(0, 0, 40, 5));
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            !rendered.contains('⤷'),
            "main checkout must not show a worktree marker, got: {rendered:?}"
        );
    }
}
