use super::*;

/// Below this many todos we always render an exact 1:1 pip per todo,
/// even if the panel is a bit narrow, so small lists are never normalized.
const EXACT_PIP_FLOOR: usize = 12;

/// Map swarm plan items into the todo-widget model so the persistent info
/// widget renders live plan state (this is the durable surface backing the
/// transient 3s "Swarm plan synced" status notice).
///
/// Plan statuses use the scheduler vocabulary (`queued`, `ready`, `running`,
/// `running_stale`, `done`, `failed`, `stopped`, `crashed`, ...) while the todo
/// renderer only distinguishes `in_progress`/`completed`/`cancelled`/other.
/// Without normalization, `running` plan tasks render as open `○` items and
/// sort *after* completed work, so large plans hide all live activity behind
/// the "+N more" footer.
pub(crate) fn swarm_plan_todos(items: &[crate::plan::PlanItem]) -> Vec<crate::todo::TodoItem> {
    items
        .iter()
        .map(|item| crate::todo::TodoItem {
            content: item.content.clone(),
            status: normalize_plan_status_for_todo(&item.status),
            priority: item.priority.clone(),
            id: item.id.clone(),
            group: None,
            blocked_by: item.blocked_by.clone(),
            assigned_to: item.assigned_to.clone(),
            confidence: None,
            completion_confidence: None,
            confidence_history: Vec::new(),
        })
        .collect()
}

/// Collapse the scheduler's status vocabulary onto the todo renderer's:
/// active → `in_progress` (▶ amber, sorts first), terminal success →
/// `completed` (✓), terminal failure → `cancelled` (✗), runnable →
/// `pending` (○). Statuses the todo renderer already understands (and any
/// arbitrary strings) pass through unchanged. Blocked items still get their
/// ⊳ marker from `blocked_by`.
fn normalize_plan_status_for_todo(status: &str) -> String {
    match status {
        "running" | "running_stale" => "in_progress".to_string(),
        "done" => "completed".to_string(),
        "failed" | "stopped" | "crashed" => "cancelled".to_string(),
        "queued" | "ready" | "todo" | "blocked" => "pending".to_string(),
        other => other.to_string(),
    }
}

fn todo_confidence_weight(priority: &str) -> u32 {
    match priority {
        "high" => 3,
        "medium" => 2,
        _ => 1,
    }
}

fn todo_display_confidence(todo: &crate::todo::TodoItem) -> Option<crate::todo::ConfidenceState> {
    if todo.status == "completed" {
        todo.completion_confidence.or(todo.confidence)
    } else {
        todo.confidence
    }
}

fn aggregate_todo_confidence<'a>(
    todos: impl IntoIterator<Item = &'a crate::todo::TodoItem>,
) -> Option<crate::todo::ConfidenceState> {
    let mut weighted_sum = 0u32;
    let mut total_weight = 0u32;
    for todo in todos.into_iter().filter(|todo| todo.status != "cancelled") {
        let Some(state) = todo_display_confidence(todo) else {
            continue;
        };
        let weight = todo_confidence_weight(&todo.priority);
        weighted_sum += u32::from(state.legacy_score()) * weight;
        total_weight += weight;
    }
    if total_weight == 0 {
        None
    } else {
        Some(crate::todo::ConfidenceState::from_legacy_score(
            ((weighted_sum + total_weight / 2) / total_weight) as u8,
        ))
    }
}

fn confidence_style(state: Option<crate::todo::ConfidenceState>) -> Style {
    use crate::todo::ConfidenceState;
    let color = match state {
        Some(ConfidenceState::Validated | ConfidenceState::Verified) => rgb(100, 180, 100),
        Some(ConfidenceState::Plausible) => rgb(220, 190, 100),
        Some(ConfidenceState::Speculative) => rgb(220, 120, 100),
        None => rgb(100, 100, 110),
    };
    Style::default().fg(color)
}

fn confidence_label(state: Option<crate::todo::ConfidenceState>) -> String {
    state
        .map(|state| state.as_str().to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Find the goal assessment recorded for a todo group (`None` = the
/// ungrouped/flat list). Group labels are compared after trimming, matching
/// how the todo tool normalizes them.
fn goal_for_group<'a>(
    goals: &'a [crate::todo::TodoGoal],
    group: Option<&str>,
) -> Option<&'a crate::todo::TodoGoal> {
    let key = group.map(str::trim).filter(|group| !group.is_empty());
    goals.iter().find(|goal| {
        goal.group
            .as_deref()
            .map(str::trim)
            .filter(|group| !group.is_empty())
            == key
    })
}

/// Color for a closed feedback loop score: green when progress has a credible
/// metric to iterate against, red when it is low (below the reframe-nudge
/// threshold), amber in between.
fn loop_style(state: crate::todo::FeedbackLoopState) -> Style {
    use crate::todo::FeedbackLoopState;
    let color = if state >= FeedbackLoopState::Closed {
        rgb(100, 180, 100)
    } else if state >= FeedbackLoopState::Strong {
        rgb(220, 190, 100)
    } else {
        rgb(220, 120, 100)
    };
    Style::default().fg(color)
}

/// Append a compact suffix describing a goal's feedback-loop assessments.
fn push_goal_loop_suffix(spans: &mut Vec<Span<'static>>, goal: &crate::todo::TodoGoal) {
    if goal.closed_feedback_loop.is_none()
        && goal.feedback_loop_relevance.is_none()
        && goal.feedback_loop_coverage.is_none()
        && goal.feedback_loop_traceability.is_none()
    {
        return;
    }
    spans.push(Span::styled(" · ", Style::default().fg(rgb(80, 80, 90))));
    spans.push(Span::styled(
        "loop ",
        Style::default().fg(rgb(140, 140, 150)),
    ));
    let mut separator = false;
    if let Some(state) = goal.closed_feedback_loop {
        spans.push(Span::styled(state.as_str().to_string(), loop_style(state)));
        separator = true;
    }
    for value in [
        goal.feedback_loop_relevance.map(|state| state.as_str()),
        goal.feedback_loop_coverage.map(|state| state.as_str()),
        goal.feedback_loop_traceability.map(|state| state.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        if separator {
            spans.push(Span::styled("/", Style::default().fg(rgb(80, 80, 90))));
        }
        spans.push(Span::styled(
            value.to_string(),
            Style::default().fg(rgb(140, 140, 150)),
        ));
        separator = true;
    }
}

/// Display width of the suffix `push_goal_loop_suffix` would render for this
/// goal (0 when it renders nothing), so header truncation can reserve room.
fn goal_loop_suffix_width(goal: &crate::todo::TodoGoal) -> u16 {
    let states = [
        goal.closed_feedback_loop.map(|state| state.as_str()),
        goal.feedback_loop_relevance.map(|state| state.as_str()),
        goal.feedback_loop_coverage.map(|state| state.as_str()),
        goal.feedback_loop_traceability.map(|state| state.as_str()),
    ];
    let values: Vec<&str> = states.into_iter().flatten().collect();
    if values.is_empty() {
        0
    } else {
        3 + "loop ".len() as u16
            + values.iter().map(|value| value.len() as u16).sum::<u16>()
            + values.len().saturating_sub(1) as u16
    }
}

fn todo_confidence_suffix_width(todo: &crate::todo::TodoItem) -> u16 {
    3 + confidence_label(todo_display_confidence(todo)).len() as u16
}

fn push_todo_confidence_suffix(spans: &mut Vec<Span<'static>>, todo: &crate::todo::TodoItem) {
    let score = todo_display_confidence(todo);
    spans.push(Span::styled(" · ", Style::default().fg(rgb(80, 80, 90))));
    spans.push(Span::styled(
        confidence_label(score),
        confidence_style(score),
    ));
}

/// Build a compact pip-dot status meter for a set of todos.
///
/// Each todo becomes one pip: green filled = completed, amber filled = in_progress,
/// hollow = pending/blocked. We render an exact 1:1 pip per todo whenever
/// the list is small enough to fit in `width_pips` columns; only larger
/// lists collapse to a proportional summary so the footprint stays small.
fn push_todo_pips(spans: &mut Vec<Span<'static>>, data: &InfoWidgetData, width_pips: usize) {
    let total = data.todos.len();
    if total == 0 || width_pips == 0 {
        return;
    }

    let done_color = rgb(100, 180, 100);
    let active_color = rgb(255, 200, 100);
    let open_color = rgb(90, 90, 105);

    let completed = data
        .todos
        .iter()
        .filter(|t| t.status == "completed")
        .count();
    let in_progress = data
        .todos
        .iter()
        .filter(|t| t.status == "in_progress")
        .count();
    let open = total.saturating_sub(completed + in_progress);

    spans.push(Span::raw("  "));

    // Prefer exact 1:1 pips. Allow it whenever the list fits the available
    // width, plus a generous floor so typical lists never get normalized
    // just because the panel is a little narrow.
    let exact_threshold = width_pips.max(EXACT_PIP_FLOOR);

    if total <= exact_threshold {
        // One pip per todo, in status order: done, active, open.
        for _ in 0..completed {
            spans.push(Span::styled("●", Style::default().fg(done_color)));
        }
        for _ in 0..in_progress {
            spans.push(Span::styled("●", Style::default().fg(active_color)));
        }
        for _ in 0..open {
            spans.push(Span::styled("○", Style::default().fg(open_color)));
        }
    } else {
        // Collapse proportionally to width_pips.
        let max_pips = width_pips.max(1);
        let scale = |count: usize| -> usize {
            ((count as f64 / total as f64) * max_pips as f64).round() as usize
        };
        let mut done_pips = scale(completed);
        let mut active_pips = scale(in_progress);
        // Ensure at least one active pip if any work is in progress.
        if in_progress > 0 && active_pips == 0 {
            active_pips = 1;
        }
        // Ensure at least one done pip if anything is completed.
        if completed > 0 && done_pips == 0 {
            done_pips = 1;
        }
        let used = (done_pips + active_pips).min(max_pips);
        let open_pips = max_pips.saturating_sub(used);
        let done_pips = done_pips.min(max_pips);
        let active_pips = active_pips.min(max_pips.saturating_sub(done_pips));

        for _ in 0..done_pips {
            spans.push(Span::styled("●", Style::default().fg(done_color)));
        }
        for _ in 0..active_pips {
            spans.push(Span::styled("●", Style::default().fg(active_color)));
        }
        for _ in 0..open_pips {
            spans.push(Span::styled("○", Style::default().fg(open_color)));
        }
    }
}

fn aggregate_confidence_suffix_width(score: Option<crate::todo::ConfidenceState>) -> u16 {
    match score {
        Some(score) => 3 + "confidence ".len() as u16 + confidence_label(Some(score)).len() as u16,
        None => 0,
    }
}

fn push_aggregate_confidence_suffix(
    spans: &mut Vec<Span<'static>>,
    score: Option<crate::todo::ConfidenceState>,
) {
    let Some(score) = score else {
        return;
    };
    spans.push(Span::styled(" · ", Style::default().fg(rgb(100, 100, 110))));
    spans.push(Span::styled(
        "confidence ",
        Style::default().fg(rgb(140, 140, 150)),
    ));
    spans.push(Span::styled(
        confidence_label(Some(score)),
        confidence_style(Some(score)),
    ));
}

/// Append the aggregate-confidence suffix only when it fits within `available`
/// remaining columns. The suffix is informational and is dropped (never
/// clipped) when the group header is too narrow, so it cannot overflow the box.
fn push_aggregate_confidence_suffix_if_fits(
    spans: &mut Vec<Span<'static>>,
    score: Option<crate::todo::ConfidenceState>,
    available: u16,
) {
    if aggregate_confidence_suffix_width(score) == 0
        || aggregate_confidence_suffix_width(score) > available
    {
        return;
    }
    push_aggregate_confidence_suffix(spans, score);
}

/// Total display width of a row's spans, used to decide how much room remains
/// for optional suffixes before the line can overflow `inner.width`.
fn spans_width(spans: &[Span<'static>]) -> u16 {
    use unicode_width::UnicodeWidthStr;
    spans.iter().map(|s| s.content.width() as u16).sum()
}

/// Normalize a todo's group label, treating empty/whitespace as ungrouped.
fn todo_group_key(todo: &crate::todo::TodoItem) -> Option<String> {
    todo.group
        .as_deref()
        .map(str::trim)
        .filter(|group| !group.is_empty())
        .map(|group| group.to_string())
}

/// Partition todos into ordered groups, preserving the order groups first
/// appear. Ungrouped items collapse into a trailing `None` bucket. Returns
/// `None` when no todo declares a group, so callers fall back to the flat list.
fn grouped_todos(
    todos: &[crate::todo::TodoItem],
) -> Option<Vec<(Option<String>, Vec<&crate::todo::TodoItem>)>> {
    if !todos.iter().any(|todo| todo_group_key(todo).is_some()) {
        return None;
    }
    let mut groups: Vec<(Option<String>, Vec<&crate::todo::TodoItem>)> = Vec::new();
    for todo in todos {
        let key = todo_group_key(todo);
        if let Some(entry) = groups.iter_mut().find(|(existing, _)| *existing == key) {
            entry.1.push(todo);
        } else {
            groups.push((key, vec![todo]));
        }
    }
    // Keep the ungrouped bucket last; sort_by_key is stable so named groups
    // retain their first-seen order.
    groups.sort_by_key(|(key, _)| key.is_none());
    Some(groups)
}

fn status_sort_rank(status: &str) -> u8 {
    match status {
        "in_progress" => 0,
        "pending" => 1,
        "completed" => 2,
        "cancelled" => 3,
        _ => 4,
    }
}

fn sort_todos_by_status<'a>(todos: &[&'a crate::todo::TodoItem]) -> Vec<&'a crate::todo::TodoItem> {
    let mut sorted: Vec<&crate::todo::TodoItem> = todos.to_vec();
    sorted.sort_by(|a, b| status_sort_rank(&a.status).cmp(&status_sort_rank(&b.status)));
    sorted
}

fn push_group_header(
    lines: &mut Vec<Line<'static>>,
    name: &str,
    items: &[&crate::todo::TodoItem],
    goal: Option<&crate::todo::TodoGoal>,
    inner: Rect,
) {
    let total = items.len();
    let completed = items.iter().filter(|t| t.status == "completed").count();
    let counter = format!(" {}/{}", completed, total);
    let confidence = aggregate_todo_confidence(items.iter().copied());
    let confidence_width = aggregate_confidence_suffix_width(confidence);
    let loop_width = goal.map(goal_loop_suffix_width).unwrap_or(0);
    let max_name = inner
        .width
        .saturating_sub(counter.len() as u16 + confidence_width + loop_width)
        .max(4) as usize;
    let highlight = items.iter().any(|t| t.status == "in_progress");
    let name_style = if highlight {
        Style::default().fg(rgb(255, 210, 130)).bold()
    } else {
        Style::default().fg(rgb(170, 175, 205)).bold()
    };
    // Group names wrap instead of truncating so the full label always shows.
    let name_lines = wrap_text_width(name, max_name);
    for (i, name_line) in name_lines.iter().enumerate() {
        let mut spans = if i == 0 {
            vec![
                Span::styled(name_line.clone(), name_style),
                Span::styled(counter.clone(), Style::default().fg(rgb(120, 120, 140))),
            ]
        } else {
            vec![Span::styled(name_line.clone(), name_style)]
        };
        if i == 0 {
            // The name is already sized to reserve room, but when the widget is
            // very narrow the reserved confidence/loop suffixes alone can exceed
            // inner.width (the name clamps to a minimum). Guard each optional
            // suffix so the group header never overflows its box; the suffix is
            // informational and is dropped rather than clipped.
            let avail = inner.width.saturating_sub(spans_width(&spans));
            push_aggregate_confidence_suffix_if_fits(&mut spans, confidence, avail);
            if let Some(goal) = goal {
                let avail = inner.width.saturating_sub(spans_width(&spans));
                if goal_loop_suffix_width(goal) <= avail {
                    push_goal_loop_suffix(&mut spans, goal);
                }
            }
        }
        lines.push(Line::from(spans));
    }
}

/// Render one todo, wrapping its content across as many lines as needed so the
/// full text always displays (no ellipsis truncation). Continuation pawns of a
/// wrapped item repeat its status icon indented so the multi-line item stays
/// visually attributed. `indent` is the leading-space depth used when items sit
/// under a group header.
fn push_todo_item_line(
    lines: &mut Vec<Line<'static>>,
    todo: &crate::todo::TodoItem,
    inner: Rect,
    show_priority_marker: bool,
    indent: usize,
) {
    let is_blocked = !todo.blocked_by.is_empty();
    let (icon, status_color) = if is_blocked && todo.status != "completed" {
        ("⊳", rgb(180, 140, 100))
    } else {
        match todo.status.as_str() {
            "completed" => ("✓", rgb(100, 180, 100)),
            "in_progress" => ("▶", rgb(255, 200, 100)),
            "cancelled" => ("✗", rgb(120, 80, 80)),
            _ => ("○", rgb(120, 120, 130)),
        }
    };

    let priority_marker = if show_priority_marker {
        match todo.priority.as_str() {
            "high" => ("!", rgb(255, 120, 100)),
            _ => ("", rgb(120, 120, 130)),
        }
    } else {
        ("", rgb(120, 120, 130))
    };

    let suffix = if is_blocked && todo.status != "completed" {
        " (blocked)"
    } else {
        ""
    };

    let text_color = if todo.status == "completed" {
        rgb(100, 100, 110)
    } else if is_blocked {
        rgb(120, 120, 130)
    } else if todo.status == "in_progress" {
        rgb(200, 200, 210)
    } else {
        rgb(160, 160, 170)
    };

    // Fixed prefix consumed by the status glyph, optional priority marker, the
    // per-item confidence suffix, and the blocked suffix.
    let prefix_width = indent
        + 2 // glyph + following space
        + priority_marker.0.len()
        + todo_confidence_suffix_width(todo) as usize;

    let content_width = inner
        .width
        .saturating_sub(prefix_width as u16)
        .saturating_sub(suffix.len() as u16)
        .max(1) as usize;

    let wrapped = wrap_text_width(&todo.content, content_width);

    // First line carries the status glyph + priority marker + content + suffix.
    let mut first_spans = Vec::new();
    if indent > 0 {
        first_spans.push(Span::raw(" ".repeat(indent)));
    }
    first_spans.push(Span::styled(
        format!("{} ", icon),
        Style::default().fg(status_color),
    ));
    if !priority_marker.0.is_empty() {
        first_spans.push(Span::styled(
            priority_marker.0,
            Style::default().fg(priority_marker.1),
        ));
    }
    first_spans.push(Span::styled(
        wrapped[0].clone(),
        Style::default().fg(text_color),
    ));
    push_todo_confidence_suffix(&mut first_spans, todo);
    if !suffix.is_empty() {
        first_spans.push(Span::styled(
            suffix.to_string(),
            Style::default().fg(rgb(100, 100, 110)),
        ));
    }
    lines.push(Line::from(first_spans));

    // Continuation lines: indent to align under the content start and repeat a
    // dim glyph so the wrapped rows stay visually attributed to the item.
    let cont_indent = indent + 2 + priority_marker.0.len();
    for chunk in &wrapped[1..] {
        let mut spans = Vec::new();
        spans.push(Span::raw(" ".repeat(cont_indent)));
        spans.push(Span::styled(
            format!("{} ", icon),
            Style::default().fg(status_color).dim(),
        ));
        spans.push(Span::styled(chunk.clone(), Style::default().fg(text_color)));
        lines.push(Line::from(spans));
    }
}

/// Render every todo partitioned by group (headers + item rows), with no line
/// budget and no "+N more" footer: the full list always renders. Returns the
/// rendered lines so height computation can mirror them exactly.
fn render_grouped_todo_lines(
    groups: &[(Option<String>, Vec<&crate::todo::TodoItem>)],
    goals: &[crate::todo::TodoGoal],
    inner: Rect,
    show_priority_marker: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (group, items) in groups {
        let header_name = group.as_deref().unwrap_or("Other");
        let goal = goal_for_group(goals, group.as_deref());
        push_group_header(&mut lines, header_name, items, goal, inner);
        for todo in sort_todos_by_status(items) {
            push_todo_item_line(&mut lines, todo, inner, show_priority_marker, 2);
        }
    }
    lines
}

/// Header label for the todo slot: "Plan" when the items are the shared
/// swarm plan projection, "Todos" for the session's own private list.
fn todos_widget_label(data: &InfoWidgetData) -> &'static str {
    if data.todos_are_swarm_plan {
        "Plan"
    } else {
        "Todos"
    }
}

/// Render todos widget content
pub(super) fn render_todos_widget(data: &InfoWidgetData, inner: Rect) -> Vec<Line<'static>> {
    if data.todos.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<Line> = Vec::new();
    let total = data.todos.len();
    let completed: usize = data
        .todos
        .iter()
        .filter(|t| t.status == "completed")
        .count();

    // Header with progress + inline pip meter
    let mut header = vec![
        Span::styled(
            format!("{} ", todos_widget_label(data)),
            Style::default().fg(rgb(180, 180, 190)).bold(),
        ),
        Span::styled(
            format!("{}/{}", completed, total),
            Style::default().fg(rgb(140, 140, 150)),
        ),
    ];
    let pip_budget = (inner.width.saturating_sub(12) / 2).clamp(0, 10) as usize;
    push_todo_pips(&mut header, data, pip_budget);
    push_aggregate_confidence_suffix(&mut header, aggregate_todo_confidence(&data.todos));

    // Grouped layout when any todo declares a group; otherwise the flat list.
    if let Some(groups) = grouped_todos(&data.todos) {
        lines.push(Line::from(header));
        lines.extend(render_grouped_todo_lines(&groups, &data.todo_goals, inner, false));
        return lines;
    }

    // Flat list: the whole list is one implicit goal, so its feedback-loop score
    // (if recorded) lives on the header line.
    if let Some(goal) = goal_for_group(&data.todo_goals, None) {
        push_goal_loop_suffix(&mut header, goal);
    }
    lines.push(Line::from(header));

    // Sort todos: in_progress first, then pending, then completed
    let mut sorted_todos: Vec<&crate::todo::TodoItem> = data.todos.iter().collect();
    sorted_todos.sort_by(|a, b| status_sort_rank(&a.status).cmp(&status_sort_rank(&b.status)));

    // Render every todo with no height cap and no "+N more" footer.
    for todo in sorted_todos {
        push_todo_item_line(&mut lines, todo, inner, false, 0);
    }

    lines
}

/// Number of rows the todos widget would render at `width` (wrapping long
/// items, no cap and no "+N more" footer). Used by page-layout height math so a
/// widget that shows the full list is sized to match.
pub(crate) fn todos_widget_line_count(data: &InfoWidgetData, width: u16) -> usize {
    render_todos_widget(data, Rect::new(0, 0, width, u16::MAX)).len()
}

pub(super) fn render_todos_expanded(data: &InfoWidgetData, inner: Rect) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    if data.todos.is_empty() {
        return lines;
    }

    // Calculate stats
    let total = data.todos.len();
    let completed: usize = data
        .todos
        .iter()
        .filter(|t| t.status == "completed")
        .count();

    // Header with progress + inline pip meter
    let mut header = vec![
        Span::styled(
            format!("{} ", todos_widget_label(data)),
            Style::default().fg(rgb(180, 180, 190)).bold(),
        ),
        Span::styled(
            format!("{}/{}", completed, total),
            Style::default().fg(rgb(140, 140, 150)),
        ),
    ];
    let pip_budget = (inner.width.saturating_sub(12) / 2).clamp(0, 14) as usize;
    push_todo_pips(&mut header, data, pip_budget);
    push_aggregate_confidence_suffix(&mut header, aggregate_todo_confidence(&data.todos));

    // Grouped layout when any todo declares a group; otherwise the flat list.
    if let Some(groups) = grouped_todos(&data.todos) {
        lines.push(Line::from(header));
        lines.extend(render_grouped_todo_lines(&groups, &data.todo_goals, inner, true));
        return lines;
    }

    // Flat list: the whole list is one implicit goal, so its feedback-loop score
    // (if recorded) lives on the header line.
    if let Some(goal) = goal_for_group(&data.todo_goals, None) {
        push_goal_loop_suffix(&mut header, goal);
    }
    lines.push(Line::from(header));

    // Sort todos: in_progress first, then pending, then completed
    let mut sorted_todos: Vec<&crate::todo::TodoItem> = data.todos.iter().collect();
    sorted_todos.sort_by(|a, b| status_sort_rank(&a.status).cmp(&status_sort_rank(&b.status)));

    // Render every todo with no height cap and no "+N more" footer.
    for todo in sorted_todos {
        push_todo_item_line(&mut lines, todo, inner, true, 0);
    }

    lines
}

pub(super) fn render_todos_compact(data: &InfoWidgetData, _inner: Rect) -> Vec<Line<'static>> {
    if data.todos.is_empty() {
        return Vec::new();
    }
    let total = data.todos.len();
    let mut completed = 0usize;
    let mut in_progress = 0usize;
    for todo in &data.todos {
        match todo.status.as_str() {
            "completed" => completed += 1,
            "in_progress" => in_progress += 1,
            _ => {}
        }
    }
    let pending = total.saturating_sub(completed);
    let mut summary = vec![
        Span::styled(
            format!("{} total", total),
            Style::default().fg(rgb(160, 160, 170)),
        ),
        Span::styled(" · ", Style::default().fg(rgb(100, 100, 110))),
        Span::styled(
            format!("{} active", in_progress),
            Style::default().fg(rgb(255, 200, 100)),
        ),
        Span::styled(" · ", Style::default().fg(rgb(100, 100, 110))),
        Span::styled(
            format!("{} open", pending),
            Style::default().fg(rgb(140, 140, 150)),
        ),
    ];
    push_aggregate_confidence_suffix(&mut summary, aggregate_todo_confidence(&data.todos));
    if let Some(goal) = goal_for_group(&data.todo_goals, None) {
        push_goal_loop_suffix(&mut summary, goal);
    }

    vec![
        Line::from(vec![Span::styled(
            todos_widget_label(data),
            Style::default().fg(rgb(180, 180, 190)).bold(),
        )]),
        Line::from(summary),
    ]
}
