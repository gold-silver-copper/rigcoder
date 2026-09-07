//! The terminal UI: a Bevy app with the agent plugin and a ratatui screen
//! drawn over crossterm by bevy_ratatui. The UI never touches the bus
//! directly: it reads `Transcript`, and hands prompts to an exclusive system
//! that calls `rigcoder::submit`.
//!
//! Keys: Enter sends, Alt+Enter (or Ctrl+J) inserts a newline, Esc stops the
//! run in flight, Ctrl+C / Ctrl+Q quits, Up/Down/PageUp/PageDown scroll,
//! End follows the stream again.

use std::{path::PathBuf, time::Duration};

use bevy::{
    app::{AppExit, ScheduleRunnerPlugin},
    prelude::*,
};
use bevy_ratatui::{RatatuiContext, RatatuiPlugins, event::KeyMessage};
use ratatui::{
    crossterm::event::{KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use rigcoder::{Conversation, Event, ModelChoice, RigcoderPlugin, Transcript, Workspace};

const COLLAPSED_RESULT_LINES: usize = 6;

#[derive(Resource, Default)]
struct Ui {
    input: String,
    /// A prompt the user sent, waiting for the exclusive system.
    outbox: Option<String>,
    stop: bool,
    /// Transcript scroll offset in rendered lines; `follow` pins it to the end.
    scroll: usize,
    follow: bool,
    frame: usize,
    displayed_approval: Option<String>,
    approval_view: Option<ApprovalView>,
}

fn main() -> anyhow::Result<()> {
    let workspace = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("a current directory"))
        .canonicalize()?;
    // The terminal is the screen: logs go to a file, if asked for at all.
    if let Ok(path) = std::env::var("RIGCODER_LOG") {
        let file = std::fs::File::create(path)?;
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(file)
            .with_ansi(false)
            .init();
    }
    App::new()
        .add_plugins((
            MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f32(
                1. / 30.,
            ))),
            RatatuiPlugins::default(),
            RigcoderPlugin::live(workspace, ModelChoice::from_env(), 200),
        ))
        .insert_resource(Ui {
            follow: true,
            ..default()
        })
        .insert_resource(rigcoder::steer::Steer {
            auto_approve: false,
            hold: vec![
                r"(^|[;&|]\s*)(rm\s+-[a-zA-Z]*r|git\s+(push|reset\s+--hard|clean)|sudo)\b"
                    .to_owned(),
            ],
            ..Default::default()
        })
        .insert_resource(rigcoder::approval::ApprovalMode::Ask)
        .add_systems(PreUpdate, keys)
        .add_systems(Update, (deliver, draw).chain())
        .run();
    Ok(())
}

fn keys(
    mut messages: MessageReader<KeyMessage>,
    mut ui: ResMut<Ui>,
    conversation: Res<Conversation>,
    mut approvals: ResMut<rigcoder::steer::Approvals>,
    mut exit: MessageWriter<AppExit>,
) {
    let busy = conversation.is_busy();
    for message in messages.read() {
        let key = &message.0;
        if key.kind == KeyEventKind::Release {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c' | 'q') if ctrl => {
                exit.write_default();
            }
            KeyCode::Char('j') if ctrl => ui.input.push('\n'),
            KeyCode::Enter if alt => ui.input.push('\n'),
            KeyCode::Enter => {
                let prompt = ui.input.trim().to_owned();
                if !busy && !prompt.is_empty() {
                    ui.input.clear();
                    ui.outbox = Some(prompt);
                    ui.follow = true;
                }
            }
            KeyCode::Esc if busy => ui.stop = true,
            KeyCode::Char('y') if !approvals.pending.is_empty() && ui.input.is_empty() => {
                if let Some(id) = &ui.displayed_approval {
                    approvals.decide(id, true);
                }
            }
            KeyCode::Char('n') if !approvals.pending.is_empty() && ui.input.is_empty() => {
                if let Some(id) = &ui.displayed_approval {
                    approvals.decide(id, false);
                }
            }
            KeyCode::Backspace => {
                ui.input.pop();
            }
            KeyCode::Char(c) if !ctrl => ui.input.push(c),
            KeyCode::Up => scroll_by(&mut ui, -1),
            KeyCode::Down => scroll_by(&mut ui, 1),
            KeyCode::PageUp => scroll_by(&mut ui, -20),
            KeyCode::PageDown => scroll_by(&mut ui, 20),
            KeyCode::Home => {
                ui.scroll = 0;
                ui.follow = false;
            }
            KeyCode::End => ui.follow = true,
            _ => {}
        }
    }
}

fn scroll_by(ui: &mut Ui, delta: i32) {
    ui.follow = false;
    ui.scroll = ui.scroll.saturating_add_signed(delta as isize);
}

/// The exclusive step: send what the UI queued, or stop the run.
fn deliver(world: &mut World) {
    let (prompt, stop) = {
        let mut ui = world.resource_mut::<Ui>();
        (ui.outbox.take(), std::mem::take(&mut ui.stop))
    };
    if stop {
        rigcoder::cancel(world, "stopped from the UI");
    }
    if let Some(prompt) = prompt {
        rigcoder::submit(world, &prompt);
    }
}

fn draw(
    mut context: ResMut<RatatuiContext>,
    mut ui: ResMut<Ui>,
    transcript: Res<Transcript>,
    conversation: Res<Conversation>,
    workspace: Res<Workspace>,
    model: Res<ModelChoice>,
    approvals: Res<rigcoder::approval::Approvals>,
) -> Result {
    ui.frame = ui.frame.wrapping_add(1);
    let busy = conversation.is_busy();
    let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][(ui.frame / 3) % 10];
    let input_lines = ui.input.lines().count().clamp(1, 8) as u16 + 2;
    let request = approvals.pending.front();
    let displayed = request.map(|request| request.operation_id.clone());
    if displayed != ui.displayed_approval {
        ui.follow = displayed.is_none();
        ui.scroll = 0;
    }
    context.draw(|frame| {
        let [status, body, input, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(input_lines),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let state = if busy {
            Span::styled(format!("{spinner} working"), Style::new().fg(Color::Yellow))
        } else {
            Span::styled("idle", Style::new().fg(Color::Green))
        };
        frame.render_widget(
            Line::from(vec![
                Span::styled(
                    " rigcoder ",
                    Style::new().add_modifier(Modifier::BOLD).reversed(),
                ),
                Span::raw(format!(" {}  ", *model)),
                Span::styled(workspace.root.display().to_string(), Style::new().dim()),
                Span::raw("  "),
                state,
            ]),
            status,
        );

        let inner = Rect {
            width: body.width.saturating_sub(2),
            height: body.height.saturating_sub(2),
            ..body
        };
        if let Some(request) = request {
            if ui
                .approval_view
                .as_ref()
                .is_none_or(|view| view.id != request.operation_id || view.width != inner.width)
            {
                ui.approval_view = Some(ApprovalView {
                    id: request.operation_id.clone(),
                    width: inner.width,
                    lines: wrap_review(&request.terminal_preview(), inner.width),
                });
            }
            let total = ui.approval_view.as_ref().unwrap().lines.len();
            let max_scroll = total.saturating_sub(usize::from(inner.height));
            if ui.follow || ui.scroll > max_scroll {
                ui.scroll = max_scroll;
            }
            let visible: Vec<_> = ui
                .approval_view
                .as_ref()
                .unwrap()
                .lines
                .iter()
                .skip(ui.scroll)
                .take(usize::from(inner.height))
                .map(|line| Line::from(line.as_str()))
                .collect();
            let title = format!(
                " approval: y applies, n denies  {}/{} ",
                ui.scroll
                    .saturating_add(usize::from(inner.height))
                    .min(total),
                total
            );
            frame.render_widget(
                Paragraph::new(visible).block(Block::new().borders(Borders::ALL).title(title)),
                body,
            );
        } else {
            ui.approval_view = None;
            let paragraph =
                Paragraph::new(render_transcript(&transcript.events)).wrap(Wrap { trim: false });
            let total = paragraph.line_count(inner.width).min(usize::from(u16::MAX));
            let max_scroll = total.saturating_sub(usize::from(inner.height));
            if ui.follow || ui.scroll > max_scroll {
                ui.scroll = max_scroll;
            }
            let title = format!(
                " transcript  {}/{} ",
                ui.scroll
                    .saturating_add(usize::from(inner.height))
                    .min(total),
                total
            );
            frame.render_widget(
                paragraph
                    .scroll((ui.scroll as u16, 0))
                    .block(Block::new().borders(Borders::ALL).title(title)),
                body,
            );
        }

        let prompt_title = if request.is_some() {
            " Review the operation above; y approves, n denies (empty input) "
        } else if busy {
            " Esc stops the run "
        } else {
            " prompt "
        };
        frame.render_widget(
            Paragraph::new(ui.input.as_str())
                .wrap(Wrap { trim: false })
                .block(Block::new().borders(Borders::ALL).title(prompt_title)),
            input,
        );
        frame.render_widget(
            Line::from(vec![
                Span::styled(" Enter", Style::new().bold()),
                Span::raw(" send  "),
                Span::styled("Alt+Enter", Style::new().bold()),
                Span::raw(" newline  "),
                Span::styled("↑↓ PgUp PgDn", Style::new().bold()),
                Span::raw(" scroll  "),
                Span::styled("End", Style::new().bold()),
                Span::raw(" follow  "),
                Span::styled("Ctrl+C", Style::new().bold()),
                Span::raw(" quit"),
            ])
            .dim(),
            help,
        );
    })?;
    ui.displayed_approval = displayed;
    Ok(())
}

struct ApprovalView {
    id: String,
    width: u16,
    lines: Vec<String>,
}

// Pre-wrap once per operation/terminal width, then draw only the viewport.
// usize offsets make the complete review accessible beyond ratatui's u16 scroll.
fn wrap_review(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    for source in text.split('\n') {
        let span = Span::raw(source);
        let mut line = String::new();
        let mut used = 0;
        for grapheme in span.styled_graphemes(Style::default()) {
            let cells = Span::raw(grapheme.symbol).width();
            if used + cells > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push_str(grapheme.symbol);
            used += cells;
        }
        lines.push(line);
    }
    lines
}

/// The transcript as styled lines: prompts, streamed answers, tool calls
/// with a short argument preview, results collapsed to their first lines.
fn render_transcript(events: &[Event]) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if events.is_empty() {
        lines.push(Line::from(
            "The agent can read, edit, list and search files, and run bash in the workspace. Type a task and press Enter.",
        ).dim());
    }
    for event in events {
        match event {
            Event::User { text } => {
                lines.push(Line::from(Span::styled(
                    "you",
                    Style::new().fg(Color::Cyan).bold(),
                )));
                for l in text.lines() {
                    lines.push(Line::from(Span::styled(
                        l.to_owned(),
                        Style::new().fg(Color::Cyan),
                    )));
                }
                lines.push(Line::default());
            }
            Event::Assistant { text } => {
                for l in text.lines() {
                    lines.push(Line::from(l.to_owned()));
                }
                lines.push(Line::default());
            }
            Event::ToolCall { name, args } => {
                lines.push(Line::from(vec![
                    Span::styled(format!("▶ {name} "), Style::new().fg(Color::Yellow).bold()),
                    Span::styled(one_line(args, 120), Style::new().fg(Color::Yellow).dim()),
                ]));
            }
            Event::ToolResult { name, output, ok } => {
                let color = if *ok { Color::DarkGray } else { Color::Red };
                let mark = if *ok { "✓" } else { "✗" };
                lines.push(Line::from(Span::styled(
                    format!("{mark} {name}"),
                    Style::new().fg(color),
                )));
                let shown: Vec<&str> = output.lines().take(COLLAPSED_RESULT_LINES).collect();
                for l in &shown {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", one_line(l, 160)),
                        Style::new().fg(color),
                    )));
                }
                let hidden = output.lines().count().saturating_sub(shown.len());
                if hidden > 0 {
                    lines.push(Line::from(Span::styled(
                        format!("  … {hidden} more line(s)"),
                        Style::new().fg(color).italic(),
                    )));
                }
            }
            Event::Settled { .. } => {
                lines.push(Line::from(Span::styled("─".repeat(40), Style::new().dim())));
                lines.push(Line::default());
            }
            Event::Failed(reason) => {
                lines.push(Line::from(Span::styled(
                    format!("run failed: {reason}"),
                    Style::new().fg(Color::Red).bold(),
                )));
                lines.push(Line::default());
            }
            Event::Denied { name, reason } => {
                lines.push(Line::from(Span::styled(
                    format!("⛔ {name}: {reason}"),
                    Style::new().fg(Color::Red),
                )));
            }
            Event::Retrying {
                attempt, wait_secs, ..
            } => {
                lines.push(Line::from(Span::styled(
                    format!("↻ provider error, retrying in {wait_secs}s (attempt {attempt})"),
                    Style::new().fg(Color::Yellow),
                )));
            }
            Event::Held { name, args } => {
                lines.push(Line::from(Span::styled(
                    format!("⏸ {name} {} (y approve / n deny)", one_line(args, 120)),
                    Style::new().fg(Color::Magenta),
                )));
            }
            Event::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                lines.push(Line::from(Span::styled(
                    format!("tokens: {input_tokens} in, {output_tokens} out"),
                    Style::new().dim(),
                )));
            }
        }
    }
    for line in &mut lines {
        for span in &mut line.spans {
            span.content = rigcoder::approval::terminal_text(&span.content).into();
        }
    }
    Text::from(lines)
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.replace('\n', "⏎");
    let mut out: String = flat.chars().take(max).collect();
    if out.chars().count() < flat.chars().count() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod approval_tests {
    use super::*;

    #[test]
    fn large_and_wrapped_reviews_keep_the_tail_accessible() {
        let text = format!("{}{}tail", "a\nb\nc\n".repeat(40_000), "x".repeat(160_000));
        let lines = wrap_review(&text, 2);
        assert!(lines.len() > usize::from(u16::MAX));
        assert_eq!(lines[lines.len() - 2..], ["ta", "il"]);
        let mut ui = Ui {
            scroll: 65_535,
            ..Default::default()
        };
        scroll_by(&mut ui, 20);
        assert_eq!(ui.scroll, 65_555);
        assert_eq!(lines.concat(), text.replace('\n', ""));
    }
}
