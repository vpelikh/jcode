use super::text::truncate_smart;
use super::{GitInfo, InfoWidgetData};
use crate::tui::color_support::rgb;
use ratatui::prelude::*;

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

    let branch_max = w.saturating_sub(2 + stats_len).max(4);
    let branch_display = truncate_smart(&info.branch, branch_max);
    parts.push(Span::styled(
        branch_display,
        Style::default()
            .fg(rgb(200, 200, 210))
            .add_modifier(Modifier::BOLD),
    ));

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

    let branch_display = truncate_smart(&info.branch, w.saturating_sub(12).max(6));
    parts.push(Span::styled(" ", Style::default().fg(rgb(240, 160, 60))));
    parts.push(Span::styled(
        branch_display,
        Style::default().fg(rgb(160, 160, 170)),
    ));

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
}
