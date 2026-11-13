use crate::{commit_view::CommitView, picker_prompt, project_diff::ProjectDiff};
use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use editor::Editor;
use futures::{StreamExt as _, channel::oneshot, future::Shared};
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, FontStyle,
    FontWeight, HighlightStyle, Hsla, KeyContext, ScrollStrategy, SharedString, StyledText,
    Subscription, Task, UnderlineStyle, UniformListScrollHandle, WeakEntity, Window, actions,
    uniform_list,
};
use picker::{Picker, PickerDelegate};
use project::{Project, git_store::Repository};
use settings::Settings as _;
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use task::{Shell, TaskContext, TaskVariables, VariableName};
use theme_settings::ThemeSettings;
use ui::{HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{
    ModalView, Workspace,
    item::{Item, ItemEvent},
};

pub const JJ_COMMAND_TASK_TAG: &str = "jj-command";
pub(crate) const CUSTOM_JJ_COMMANDS_DOCS_SLUG: &str = "tasks#custom-jj-commands";

const FIELD_SEPARATOR: char = '\u{1f}';
const MARKER_END: char = '\u{1e}';
const MARKER_FIELD_COUNT: usize = 9;

// Prefixed to the user's own `templates.log` so every revision's first line
// carries its identity. The log view strips it back out before rendering.
const MARKER_TEMPLATE: &str = concat!(
    r#""\x1f" ++ change_id"#,
    r#" ++ "\x1f" ++ change_id.shortest()"#,
    r#" ++ "\x1f" ++ commit_id"#,
    r#" ++ "\x1f" ++ commit_id.shortest()"#,
    r#" ++ "\x1f" ++ if(current_working_copy, "1")"#,
    r#" ++ "\x1f" ++ if(divergent, "1")"#,
    r#" ++ "\x1f" ++ if(immutable, "1")"#,
    r#" ++ "\x1f" ++ parents.len()"#,
    r#" ++ "\x1f" ++ local_bookmarks.map(|b| b.name()).join(" ")"#,
    r#" ++ "\x1e""#,
);

actions!(
    jj_log,
    [
        /// Opens the jj log tab for the current project.
        Open,
        /// Reloads the jj log.
        Refresh,
        /// Moves the cursor to the first line of the next revision.
        SelectNextRevision,
        /// Moves the cursor to the first line of the previous revision.
        SelectPreviousRevision,
        /// Moves the cursor to the working-copy revision.
        SelectWorkingCopy,
        /// Moves the cursor half a page down.
        ScrollDown,
        /// Moves the cursor half a page up.
        ScrollUp,
        /// Starts or ends a line-wise selection covering several revisions.
        ToggleVisualSelection,
        /// Toggles `--stat` diff stats under each revision.
        ToggleStat,
        /// Prompts for a revset to show instead of the default log revset.
        ChangeRevset,
        /// Runs `jj edit` on the revision under the cursor.
        Edit,
        /// Runs `jj edit --ignore-immutable` on the revision under the cursor.
        EditIgnoreImmutable,
        /// Edits the description of the revision under the cursor.
        Describe,
        /// Saves the description being edited.
        ConfirmDescribe,
        /// Opens the diff of the revision under the cursor.
        ViewDiff,
        /// Creates a new change on top of the selected revisions.
        New,
        /// Creates a new change after the selected revisions (`jj new -A`).
        NewAfter,
        /// Creates a new change after the selected revisions, ignoring immutability.
        NewAfterIgnoreImmutable,
        /// Runs `jj undo`.
        Undo,
        /// Runs `jj redo`.
        Redo,
        /// Abandons the selected revisions.
        Abandon,
        /// Runs `jj git fetch`, prompting for a remote when there are several.
        Fetch,
        /// Pushes the bookmarks on the revision under the cursor.
        Push,
        /// Prompts for any local bookmark and pushes it.
        PushAnyBookmark,
        /// Creates or moves a bookmark to the revision under the cursor.
        SetBookmark,
        /// Deletes bookmarks on the revision under the cursor.
        DeleteBookmark,
        /// Starts rebasing the selected revisions; pick a destination next.
        Rebase,
        /// Starts squashing the selected revisions; pick a destination next.
        Squash,
        /// Squashes the revision under the cursor into its parent.
        QuickSquash,
        /// Starts duplicating the selected revisions; pick a destination next.
        Duplicate,
        /// Rebases or duplicates onto, or squashes into, the revision under the cursor.
        PlaceOnto,
        /// Like `PlaceOnto`, ignoring immutability.
        PlaceOntoIgnoreImmutable,
        /// Rebases or duplicates after the revision under the cursor.
        PlaceAfter,
        /// Like `PlaceAfter`, ignoring immutability.
        PlaceAfterIgnoreImmutable,
        /// Rebases or duplicates before the revision under the cursor.
        PlaceBefore,
        /// Like `PlaceBefore`, ignoring immutability.
        PlaceBeforeIgnoreImmutable,
        /// Picks a custom jj command task to run against the revision under the cursor.
        RunCommand,
    ]
);

/// Runs the custom jj command task with the given label against the revision under the cursor.
#[derive(Clone, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = jj_log)]
#[serde(deny_unknown_fields)]
pub struct RunCommandByName {
    pub task_name: String,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Open, window, cx| {
            open(workspace, window, cx);
        });
    })
    .detach();
}

fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let project = workspace.project().clone();
    let Some(start_path) = jj_search_path(&project, cx) else {
        show_workspace_error(workspace, "jj log: no local folder is open", cx);
        return;
    };
    let environment = project.update(cx, |project, cx| {
        project.environment().update(cx, |environment, cx| {
            environment.local_directory_environment(&Shell::System, start_path.clone(), cx)
        })
    });

    let workspace_handle = workspace.weak_handle();
    cx.spawn_in(window, async move |workspace, cx| {
        let runner = JjRunner {
            root: start_path,
            environment,
        };
        let root = match runner.run(&["workspace", "root"]).await {
            Ok(output) => PathBuf::from(output.stdout.trim()),
            Err(error) => {
                workspace.update(cx, |workspace, cx| {
                    show_workspace_error(workspace, format!("jj log: {error:#}"), cx);
                })?;
                return anyhow::Ok(());
            }
        };
        let runner = JjRunner {
            root: root.into(),
            environment: runner.environment,
        };
        workspace.update_in(cx, |workspace, window, cx| {
            let existing = workspace
                .items_of_type::<JjLog>(cx)
                .find(|log| log.read(cx).runner.root == runner.root);
            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
                return;
            }
            let project = workspace.project().clone();
            let log = cx.new(|cx| JjLog::new(runner, project, workspace_handle, window, cx));
            workspace.add_item_to_active_pane(Box::new(log), None, true, window, cx);
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn jj_search_path(project: &Entity<Project>, cx: &App) -> Option<Arc<Path>> {
    let project = project.read(cx);
    if project.is_via_remote_server() {
        return None;
    }
    if let Some(repository) = project.active_repository(cx) {
        return Some(repository.read(cx).work_directory_abs_path.clone());
    }
    project
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path())
}

fn show_workspace_error(
    workspace: &mut Workspace,
    message: impl Into<std::borrow::Cow<'static, str>>,
    cx: &mut Context<Workspace>,
) {
    struct JjLogError;
    workspace.show_toast(
        workspace::Toast::new(
            workspace::notifications::NotificationId::unique::<JjLogError>(),
            message,
        ),
        cx,
    );
}

#[derive(Clone)]
struct JjRunner {
    root: Arc<Path>,
    environment: Shared<Task<Option<HashMap<String, String>>>>,
}

struct JjOutput {
    stdout: String,
    stderr: String,
}

impl JjRunner {
    async fn run(&self, args: &[&str]) -> Result<JjOutput> {
        let environment = self.environment.clone().await.unwrap_or_default();
        let path_variable = environment
            .get("PATH")
            .cloned()
            .or_else(|| std::env::var("PATH").ok());
        let jj = which::which_in("jj", path_variable, &self.root)
            .context("couldn't find `jj` on PATH")?;
        let output = util::command::new_command(jj)
            .args(args)
            .current_dir(&self.root)
            .envs(&environment)
            // Commands run here have no terminal, so a command that wants an
            // editor would hang forever. Make it fail instead.
            .env("JJ_EDITOR", "false")
            .output()
            .await
            .context("failed to run jj")?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if output.status.success() {
            Ok(JjOutput { stdout, stderr })
        } else {
            let message = stderr.trim();
            if message.is_empty() {
                Err(anyhow!("jj {} failed: {}", args.join(" "), output.status))
            } else {
                Err(anyhow!("{message}"))
            }
        }
    }

    async fn run_owned(&self, args: Vec<String>) -> Result<JjOutput> {
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        self.run(&args).await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnsiColor {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AnsiStyle {
    foreground: Option<AnsiColor>,
    background: Option<AnsiColor>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
}

#[derive(Debug, PartialEq)]
struct LogLine {
    text: SharedString,
    styles: Vec<(Range<usize>, AnsiStyle)>,
    revision_index: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
struct Revision {
    change_id: String,
    change_id_short: String,
    commit_id: String,
    commit_id_short: String,
    is_working_copy: bool,
    is_divergent: bool,
    is_immutable: bool,
    parent_count: usize,
    bookmarks: Vec<String>,
    first_line: usize,
}

impl Revision {
    // Change IDs survive the working-copy snapshot that every jj command
    // takes first, so `@` keeps resolving even if files changed since the log
    // was drawn. A divergent change ID is ambiguous, so use the commit ID.
    fn revset(&self) -> &str {
        if self.is_divergent {
            &self.commit_id
        } else {
            &self.change_id
        }
    }

    fn short_id(&self) -> &str {
        if self.is_divergent {
            &self.commit_id_short
        } else {
            &self.change_id_short
        }
    }
}

#[derive(Debug, Default, PartialEq)]
struct ParsedLog {
    lines: Vec<LogLine>,
    revisions: Vec<Revision>,
}

fn parse_log_output(output: &str) -> ParsedLog {
    let mut parsed = ParsedLog::default();
    let mut current_revision = None;
    for raw_line in output.lines() {
        let (line, marker) = split_marker(raw_line);
        if let Some(revision) = marker.and_then(|marker| parse_marker(marker, parsed.lines.len())) {
            current_revision = Some(parsed.revisions.len());
            parsed.revisions.push(revision);
        }
        let (text, styles) = parse_ansi(&line);
        parsed.lines.push(LogLine {
            text: text.into(),
            styles,
            revision_index: current_revision,
        });
    }
    parsed
}

fn split_marker(raw_line: &str) -> (std::borrow::Cow<'_, str>, Option<&str>) {
    let Some(start) = raw_line.find(FIELD_SEPARATOR) else {
        return (raw_line.into(), None);
    };
    let fields_start = start + FIELD_SEPARATOR.len_utf8();
    let Some(end) = raw_line[fields_start..]
        .find(MARKER_END)
        .map(|offset| fields_start + offset)
    else {
        return (raw_line.into(), None);
    };
    let line = format!(
        "{}{}",
        &raw_line[..start],
        &raw_line[end + MARKER_END.len_utf8()..]
    );
    (line.into(), Some(&raw_line[fields_start..end]))
}

fn parse_marker(marker: &str, first_line: usize) -> Option<Revision> {
    let (plain, _) = parse_ansi(marker);
    let fields = plain.split(FIELD_SEPARATOR).collect::<Vec<_>>();
    let [
        change_id,
        change_id_short,
        commit_id,
        commit_id_short,
        is_working_copy,
        is_divergent,
        is_immutable,
        parent_count,
        bookmarks,
    ] = <[&str; MARKER_FIELD_COUNT]>::try_from(fields).ok()?;
    Some(Revision {
        change_id: change_id.to_string(),
        change_id_short: change_id_short.to_string(),
        commit_id: commit_id.to_string(),
        commit_id_short: commit_id_short.to_string(),
        is_working_copy: !is_working_copy.is_empty(),
        is_divergent: !is_divergent.is_empty(),
        is_immutable: !is_immutable.is_empty(),
        // An unreadable count is treated as zero parents, which keeps the
        // diff action on the read-only commit view.
        parent_count: parent_count.parse().unwrap_or(0),
        bookmarks: bookmarks
            .split_whitespace()
            .map(ToString::to_string)
            .collect(),
        first_line,
    })
}

fn parse_ansi(input: &str) -> (String, Vec<(Range<usize>, AnsiStyle)>) {
    let mut text = String::with_capacity(input.len());
    let mut styles = Vec::new();
    let mut style = AnsiStyle::default();
    let mut run_start = 0;
    let mut chars = input.chars().peekable();

    while let Some(character) = chars.next() {
        if character != '\x1b' {
            text.push(character);
            continue;
        }
        if chars.peek() != Some(&'[') {
            continue;
        }
        chars.next();
        let mut parameters = String::new();
        let mut final_byte = None;
        for character in chars.by_ref() {
            if ('\x40'..='\x7e').contains(&character) {
                final_byte = Some(character);
                break;
            }
            parameters.push(character);
        }
        if final_byte != Some('m') {
            continue;
        }
        let new_style = apply_sgr(style, &parameters);
        if new_style != style {
            if text.len() > run_start && style != AnsiStyle::default() {
                styles.push((run_start..text.len(), style));
            }
            run_start = text.len();
            style = new_style;
        }
    }
    if text.len() > run_start && style != AnsiStyle::default() {
        styles.push((run_start..text.len(), style));
    }
    (text, styles)
}

fn apply_sgr(mut style: AnsiStyle, parameters: &str) -> AnsiStyle {
    let codes = parameters
        .split(';')
        .map(|code| code.parse::<u16>().unwrap_or(0))
        .collect::<Vec<_>>();
    let mut index = 0;
    while let Some(&code) = codes.get(index) {
        index += 1;
        match code {
            0 => style = AnsiStyle::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            30..=37 => style.foreground = Some(AnsiColor::Indexed((code - 30) as u8)),
            39 => style.foreground = None,
            40..=47 => style.background = Some(AnsiColor::Indexed((code - 40) as u8)),
            49 => style.background = None,
            90..=97 => style.foreground = Some(AnsiColor::Indexed((code - 90 + 8) as u8)),
            100..=107 => style.background = Some(AnsiColor::Indexed((code - 100 + 8) as u8)),
            38 | 48 => {
                let color = match codes.get(index) {
                    Some(5) => {
                        let color = codes
                            .get(index + 1)
                            .map(|&value| AnsiColor::Indexed(value.min(255) as u8));
                        index += 2;
                        color
                    }
                    Some(2) => {
                        let channel =
                            |offset: usize| codes.get(index + offset).map(|&v| v.min(255) as u8);
                        let color = channel(1)
                            .zip(channel(2))
                            .zip(channel(3))
                            .map(|((red, green), blue)| AnsiColor::Rgb(red, green, blue));
                        index += 4;
                        color
                    }
                    _ => None,
                };
                if code == 38 {
                    style.foreground = color;
                } else {
                    style.background = color;
                }
            }
            _ => {}
        }
    }
    style
}

fn resolve_ansi_color(color: AnsiColor, cx: &App) -> Hsla {
    match color {
        AnsiColor::Indexed(index) => terminal::get_color_at_index(index as usize, cx.theme()),
        AnsiColor::Rgb(red, green, blue) => terminal::rgba_color(red, green, blue),
    }
}

fn highlight_style(style: &AnsiStyle, cx: &App) -> HighlightStyle {
    HighlightStyle {
        color: style.foreground.map(|color| resolve_ansi_color(color, cx)),
        background_color: style.background.map(|color| resolve_ansi_color(color, cx)),
        font_weight: style.bold.then_some(FontWeight::BOLD),
        font_style: style.italic.then_some(FontStyle::Italic),
        underline: style.underline.then(|| UnderlineStyle {
            thickness: px(1.),
            ..Default::default()
        }),
        fade_out: style.dim.then_some(0.4),
        ..Default::default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlacementKind {
    Rebase,
    Squash,
    Duplicate,
}

impl PlacementKind {
    fn label(self) -> &'static str {
        match self {
            PlacementKind::Rebase => "rebase",
            PlacementKind::Squash => "squash",
            PlacementKind::Duplicate => "duplicate",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Placement {
    Onto,
    After,
    Before,
}

struct Status {
    message: SharedString,
    is_error: bool,
}

pub struct JjLog {
    runner: JjRunner,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    lines: Vec<LogLine>,
    revisions: Vec<Revision>,
    cursor: usize,
    visual_anchor: Option<usize>,
    placement: Option<(PlacementKind, Vec<Revision>)>,
    show_stat: bool,
    revset: Option<String>,
    loaded: bool,
    status: Option<Status>,
    refresh_task: Option<Task<()>>,
    refresh_queued: bool,
    command_task: Option<Task<()>>,
    _op_heads_watch: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl JjLog {
    fn new(
        runner: JjRunner,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let subscriptions = vec![cx.on_focus_in(&focus_handle, window, |this, _, cx| {
            this.refresh(cx);
        })];
        let fs = project.read(cx).fs().clone();
        let op_heads_watch = cx.spawn({
            let root = runner.root.clone();
            async move |this, cx| {
                let Some(op_heads) = op_heads_dir(fs.as_ref(), &root).await else {
                    log::warn!("jj log: couldn't find the operation heads for {root:?}");
                    return;
                };
                let (mut events, _watcher) = fs.watch(&op_heads, Duration::from_millis(100)).await;
                while events.next().await.is_some() {
                    if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                        break;
                    }
                }
            }
        });

        let mut this = Self {
            runner,
            project,
            workspace,
            focus_handle,
            scroll_handle: UniformListScrollHandle::new(),
            lines: Vec::new(),
            revisions: Vec::new(),
            cursor: 0,
            visual_anchor: None,
            placement: None,
            show_stat: true,
            revset: None,
            loaded: false,
            status: None,
            refresh_task: None,
            refresh_queued: false,
            command_task: None,
            _op_heads_watch: op_heads_watch,
            _subscriptions: subscriptions,
        };
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_task.is_some() {
            self.refresh_queued = true;
            return;
        }
        let runner = self.runner.clone();
        let show_stat = self.show_stat;
        let revset = self.revset.clone();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let output = cx
                .background_spawn(async move {
                    let log_template = runner
                        .run(&["config", "get", "templates.log"])
                        .await
                        .map(|output| output.stdout.trim().to_string())
                        .unwrap_or_else(|_| "builtin_log_compact".to_string());
                    let template = format!("{MARKER_TEMPLATE} ++ ({log_template})");
                    let mut args = vec![
                        "log".to_string(),
                        "--no-pager".to_string(),
                        "--color=always".to_string(),
                        "-T".to_string(),
                        template,
                    ];
                    if show_stat {
                        args.push("--stat".to_string());
                    }
                    if let Some(revset) = revset {
                        args.push("-r".to_string());
                        args.push(revset);
                    }
                    let output = runner.run_owned(args).await?;
                    anyhow::Ok(parse_log_output(&output.stdout))
                })
                .await;
            this.update(cx, |this, cx| {
                this.refresh_task = None;
                match output {
                    Ok(parsed) => this.apply_log(parsed, cx),
                    Err(error) => this.set_status(format!("{error:#}"), true, cx),
                }
                if std::mem::take(&mut this.refresh_queued) {
                    this.refresh(cx);
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn apply_log(&mut self, parsed: ParsedLog, cx: &mut Context<Self>) {
        let previous = self.revision_at(self.cursor).map(|revision| {
            (
                revision.change_id.clone(),
                self.cursor.saturating_sub(revision.first_line),
            )
        });
        self.lines = parsed.lines;
        self.revisions = parsed.revisions;

        let restored = if !self.loaded {
            self.revisions
                .iter()
                .find(|revision| revision.is_working_copy)
                .map(|revision| revision.first_line)
        } else {
            previous.and_then(|(change_id, offset)| {
                let index = self
                    .revisions
                    .iter()
                    .position(|revision| revision.change_id == change_id)?;
                let start = self.revisions[index].first_line;
                let end = self.revision_end_line(index);
                Some((start + offset).min(end.saturating_sub(1)))
            })
        };
        self.loaded = true;
        self.cursor = restored
            .unwrap_or(self.cursor)
            .min(self.lines.len().saturating_sub(1));
        if self
            .visual_anchor
            .is_some_and(|anchor| anchor >= self.lines.len())
        {
            self.visual_anchor = None;
        }
        self.scroll_handle
            .scroll_to_item(self.cursor, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn revision_end_line(&self, revision_index: usize) -> usize {
        self.revisions
            .get(revision_index + 1)
            .map_or(self.lines.len(), |next| next.first_line)
    }

    fn revision_at(&self, line: usize) -> Option<&Revision> {
        let index = self.lines.get(line)?.revision_index?;
        self.revisions.get(index)
    }

    fn set_status(
        &mut self,
        message: impl Into<SharedString>,
        is_error: bool,
        cx: &mut Context<Self>,
    ) {
        self.status = Some(Status {
            message: message.into(),
            is_error,
        });
        cx.notify();
    }

    fn move_cursor(&mut self, line: usize, cx: &mut Context<Self>) {
        let Some(last_line) = self.lines.len().checked_sub(1) else {
            return;
        };
        self.cursor = line.min(last_line);
        self.scroll_handle
            .scroll_to_item(self.cursor, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(self.cursor + 1, cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_cursor(self.cursor.saturating_sub(1), cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(0, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(usize::MAX, cx);
    }

    fn select_next_revision(
        &mut self,
        _: &SelectNextRevision,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = self
            .revisions
            .iter()
            .find(|revision| revision.first_line > self.cursor)
            .map(|revision| revision.first_line);
        if let Some(line) = next {
            self.move_cursor(line, cx);
        }
    }

    fn select_previous_revision(
        &mut self,
        _: &SelectPreviousRevision,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self
            .revisions
            .iter()
            .rev()
            .find(|revision| revision.first_line < self.cursor)
            .map(|revision| revision.first_line);
        if let Some(line) = previous {
            self.move_cursor(line, cx);
        }
    }

    fn select_working_copy(
        &mut self,
        _: &SelectWorkingCopy,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line = self
            .revisions
            .iter()
            .find(|revision| revision.is_working_copy)
            .map(|revision| revision.first_line);
        match line {
            Some(line) => self.move_cursor(line, cx),
            None => self.set_status("The working copy isn't in this revset", true, cx),
        }
    }

    fn visible_line_count(&self, cx: &App) -> usize {
        let height = self
            .scroll_handle
            .0
            .borrow()
            .base_handle
            .bounds()
            .size
            .height;
        let line_height = self.line_height(cx);
        if line_height <= px(0.) {
            return 1;
        }
        ((height / line_height) as usize).max(1)
    }

    fn scroll_down(&mut self, _: &ScrollDown, _: &mut Window, cx: &mut Context<Self>) {
        let step = (self.visible_line_count(cx) / 2).max(1);
        self.move_cursor(self.cursor.saturating_add(step), cx);
    }

    fn scroll_up(&mut self, _: &ScrollUp, _: &mut Window, cx: &mut Context<Self>) {
        let step = (self.visible_line_count(cx) / 2).max(1);
        self.move_cursor(self.cursor.saturating_sub(step), cx);
    }

    fn toggle_visual_selection(
        &mut self,
        _: &ToggleVisualSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.visual_anchor = match self.visual_anchor {
            Some(_) => None,
            None => {
                self.status = None;
                Some(self.cursor)
            }
        };
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.visual_anchor.take().is_some() {
            cx.notify();
        } else if let Some((kind, _)) = self.placement.take() {
            self.set_status(format!("Canceled {}", kind.label()), false, cx);
        } else {
            cx.propagate();
        }
    }

    fn refresh_action(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh(cx);
    }

    fn toggle_stat(&mut self, _: &ToggleStat, _: &mut Window, cx: &mut Context<Self>) {
        self.show_stat = !self.show_stat;
        self.refresh(cx);
    }

    /// The revision under the cursor, or every revision touched by the visual
    /// selection (which this also ends).
    fn take_selected_revisions(&mut self, cx: &mut Context<Self>) -> Vec<Revision> {
        let range = match self.visual_anchor.take() {
            Some(anchor) => {
                cx.notify();
                anchor.min(self.cursor)..anchor.max(self.cursor) + 1
            }
            None => self.cursor..self.cursor + 1,
        };
        let mut revisions: Vec<Revision> = Vec::new();
        for line in range {
            if let Some(revision) = self.revision_at(line)
                && !revisions
                    .iter()
                    .any(|selected| selected.commit_id == revision.commit_id)
            {
                revisions.push(revision.clone());
            }
        }
        if revisions.is_empty() {
            self.set_status("No revision under the cursor", true, cx);
        }
        revisions
    }

    fn revision_under_cursor(&mut self, cx: &mut Context<Self>) -> Option<Revision> {
        let revision = self.revision_at(self.cursor).cloned();
        if revision.is_none() {
            self.set_status("No revision under the cursor", true, cx);
        }
        revision
    }

    fn run_command(
        &mut self,
        args: Vec<String>,
        success_message: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.run_command_then(args, success_message, cx, |_, _| {});
    }

    fn run_command_then(
        &mut self,
        args: Vec<String>,
        success_message: Option<String>,
        cx: &mut Context<Self>,
        on_success: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        if self.command_task.is_some() {
            self.set_status("A jj command is still running", true, cx);
            return;
        }
        self.set_status(format!("Running jj {}…", args.join(" ")), false, cx);
        let runner = self.runner.clone();
        self.command_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { runner.run_owned(args).await })
                .await;
            this.update(cx, |this, cx| {
                this.command_task = None;
                match result {
                    Ok(output) => {
                        // jj leads with the summary (e.g. "Rebased 1 commits"),
                        // so the first line is the one worth showing.
                        let message = success_message.unwrap_or_else(|| {
                            output
                                .stderr
                                .lines()
                                .map(str::trim)
                                .find(|line| !line.is_empty())
                                .unwrap_or("Done")
                                .to_string()
                        });
                        this.set_status(message, false, cx);
                        on_success(this, cx);
                    }
                    Err(error) => this.set_status(format!("{error:#}"), true, cx),
                }
                this.refresh(cx);
            })
            .log_err();
        }));
    }

    fn edit(&mut self, _: &Edit, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_revision(false, cx);
    }

    fn edit_ignore_immutable(
        &mut self,
        _: &EditIgnoreImmutable,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit_revision(true, cx);
    }

    fn edit_revision(&mut self, ignore_immutable: bool, cx: &mut Context<Self>) {
        let Some(revision) = self.revision_under_cursor(cx) else {
            return;
        };
        let mut args = vec!["edit".to_string()];
        if ignore_immutable {
            args.push("--ignore-immutable".to_string());
        }
        args.push(revision.revset().to_string());
        self.run_command(args, Some(format!("Editing `{}`", revision.short_id())), cx);
    }

    fn new_change(&mut self, _: &New, _: &mut Window, cx: &mut Context<Self>) {
        let revisions = self.take_selected_revisions(cx);
        if revisions.is_empty() {
            return;
        }
        let mut args = vec!["new".to_string()];
        args.extend(
            revisions
                .iter()
                .map(|revision| revision.revset().to_string()),
        );
        self.run_command(args, None, cx);
    }

    fn new_after(&mut self, _: &NewAfter, _: &mut Window, cx: &mut Context<Self>) {
        self.new_change_after(false, cx);
    }

    fn new_after_ignore_immutable(
        &mut self,
        _: &NewAfterIgnoreImmutable,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_change_after(true, cx);
    }

    fn new_change_after(&mut self, ignore_immutable: bool, cx: &mut Context<Self>) {
        let revisions = self.take_selected_revisions(cx);
        if revisions.is_empty() {
            return;
        }
        let mut args = vec!["new".to_string()];
        for revision in &revisions {
            args.push("-A".to_string());
            args.push(revision.revset().to_string());
        }
        if ignore_immutable {
            args.push("--ignore-immutable".to_string());
        }
        self.run_command(args, None, cx);
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        self.run_command(vec!["undo".to_string()], None, cx);
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        self.run_command(vec!["redo".to_string()], None, cx);
    }

    fn abandon(&mut self, _: &Abandon, _: &mut Window, cx: &mut Context<Self>) {
        let revisions = self.take_selected_revisions(cx);
        if revisions.is_empty() {
            return;
        }
        let mut args = vec!["abandon".to_string()];
        args.extend(
            revisions
                .iter()
                .map(|revision| revision.revset().to_string()),
        );
        self.run_command(args, None, cx);
    }

    fn quick_squash(&mut self, _: &QuickSquash, _: &mut Window, cx: &mut Context<Self>) {
        let Some(revision) = self.revision_under_cursor(cx) else {
            return;
        };
        self.run_command(
            vec![
                "squash".to_string(),
                "-r".to_string(),
                revision.revset().to_string(),
                "-u".to_string(),
                "--ignore-immutable".to_string(),
            ],
            Some(format!(
                "Squashed `{}` into its parent",
                revision.short_id()
            )),
            cx,
        );
    }

    fn start_placement(&mut self, kind: PlacementKind, cx: &mut Context<Self>) {
        let revisions = self.take_selected_revisions(cx);
        if revisions.is_empty() {
            return;
        }
        let names = revisions
            .iter()
            .map(|revision| revision.short_id())
            .collect::<Vec<_>>()
            .join(", ");
        self.set_status(
            format!("{} {names}: pick a destination", capitalize(kind.label())),
            false,
            cx,
        );
        self.placement = Some((kind, revisions));
    }

    fn rebase(&mut self, _: &Rebase, _: &mut Window, cx: &mut Context<Self>) {
        self.start_placement(PlacementKind::Rebase, cx);
    }

    fn squash(&mut self, _: &Squash, _: &mut Window, cx: &mut Context<Self>) {
        self.start_placement(PlacementKind::Squash, cx);
    }

    fn duplicate(&mut self, _: &Duplicate, _: &mut Window, cx: &mut Context<Self>) {
        self.start_placement(PlacementKind::Duplicate, cx);
    }

    fn place(&mut self, placement: Placement, ignore_immutable: bool, cx: &mut Context<Self>) {
        let Some((kind, sources)) = self.placement.clone() else {
            return;
        };
        let Some(destination) = self.revision_under_cursor(cx) else {
            return;
        };
        let flag = match placement {
            Placement::Onto => "-o",
            Placement::After => "-A",
            Placement::Before => "-B",
        };
        let source_revset = sources
            .iter()
            .map(|revision| revision.revset())
            .collect::<Vec<_>>()
            .join(" | ");
        let mut args = match kind {
            PlacementKind::Rebase => vec![
                "rebase".to_string(),
                "-r".to_string(),
                source_revset,
                flag.to_string(),
                destination.revset().to_string(),
            ],
            PlacementKind::Duplicate => vec![
                "duplicate".to_string(),
                source_revset,
                flag.to_string(),
                destination.revset().to_string(),
            ],
            PlacementKind::Squash => {
                if placement != Placement::Onto {
                    return;
                }
                vec![
                    "squash".to_string(),
                    "-f".to_string(),
                    source_revset,
                    "-u".to_string(),
                    "-t".to_string(),
                    destination.revset().to_string(),
                ]
            }
        };
        if ignore_immutable {
            args.push("--ignore-immutable".to_string());
        }
        self.run_command_then(args, None, cx, |this, cx| {
            this.placement = None;
            cx.notify();
        });
    }

    fn place_onto(&mut self, _: &PlaceOnto, _: &mut Window, cx: &mut Context<Self>) {
        self.place(Placement::Onto, false, cx);
    }

    fn place_onto_ignore_immutable(
        &mut self,
        _: &PlaceOntoIgnoreImmutable,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.place(Placement::Onto, true, cx);
    }

    fn place_after(&mut self, _: &PlaceAfter, _: &mut Window, cx: &mut Context<Self>) {
        self.place(Placement::After, false, cx);
    }

    fn place_after_ignore_immutable(
        &mut self,
        _: &PlaceAfterIgnoreImmutable,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.place(Placement::After, true, cx);
    }

    fn place_before(&mut self, _: &PlaceBefore, _: &mut Window, cx: &mut Context<Self>) {
        self.place(Placement::Before, false, cx);
    }

    fn place_before_ignore_immutable(
        &mut self,
        _: &PlaceBeforeIgnoreImmutable,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.place(Placement::Before, true, cx);
    }

    fn view_diff(&mut self, _: &ViewDiff, window: &mut Window, cx: &mut Context<Self>) {
        let Some(revision) = self.revision_under_cursor(cx) else {
            return;
        };
        let root = self.runner.root.clone();
        let repository = self
            .project
            .read(cx)
            .git_store()
            .read(cx)
            .repositories()
            .values()
            .find(|repository| repository.read(cx).work_directory_abs_path == root)
            .cloned();
        let Some(repository) = repository else {
            self.set_status(
                "Viewing a diff needs a colocated git repository at the jj workspace root",
                true,
                cx,
            );
            return;
        };
        // In a colocated repository jj points git's HEAD at `@-` and keeps the
        // index matching it, so git's uncommitted changes are exactly `@`'s
        // changes, shown on the real, editable files. With several parents,
        // HEAD can only name one of them, so merges keep the commit view.
        let show_working_copy = revision.is_working_copy && revision.parent_count == 1;
        if show_working_copy {
            repository.update(cx, |repository, cx| repository.set_as_active_repository(cx));
        }
        let workspace = self.workspace.clone();
        // Adding or activating a tab reads every item in the pane, this log
        // included, which panics while this log is still being updated.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    if show_working_copy {
                        open_working_copy_diff(workspace, repository, window, cx);
                    } else {
                        open_commit_diff(workspace, repository, revision.commit_id, window, cx);
                    }
                })
                .log_err();
        });
    }

    fn describe(&mut self, _: &Describe, window: &mut Window, cx: &mut Context<Self>) {
        let Some(revision) = self.revision_under_cursor(cx) else {
            return;
        };
        let runner = self.runner.clone();
        let workspace = self.workspace.clone();
        let this = cx.weak_entity();
        cx.spawn_in(window, async move |_, cx| {
            let description = cx
                .background_spawn({
                    let revset = revision.revset().to_string();
                    async move {
                        runner
                            .run(&["log", "--no-graph", "-r", &revset, "-T", "description"])
                            .await
                    }
                })
                .await?
                .stdout;
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    DescribeModal::new(this, revision, description, window, cx)
                });
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn save_description(&mut self, revision: &Revision, message: String, cx: &mut Context<Self>) {
        self.run_command(
            vec![
                "describe".to_string(),
                "-r".to_string(),
                revision.revset().to_string(),
                "-m".to_string(),
                message,
            ],
            Some(format!("Described `{}`", revision.short_id())),
            cx,
        );
    }

    fn fetch(&mut self, _: &Fetch, window: &mut Window, cx: &mut Context<Self>) {
        let runner = self.runner.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let remotes = cx
                .background_spawn(async move { runner.run(&["git", "remote", "list"]).await })
                .await?
                .stdout
                .lines()
                .filter_map(|line| line.split_whitespace().next())
                .map(|name| SharedString::from(name.to_string()))
                .collect::<Vec<_>>();
            let remote = if remotes.len() > 1 {
                let choice = workspace
                    .update_in(cx, |_, window, cx| {
                        picker_prompt::prompt(
                            "Select remote to fetch from",
                            remotes.clone(),
                            workspace.clone(),
                            window,
                            cx,
                        )
                    })?
                    .await;
                let Some(remote) = choice.and_then(|index| remotes.get(index).cloned()) else {
                    return anyhow::Ok(());
                };
                Some(remote)
            } else {
                None
            };
            this.update(cx, |this, cx| {
                let mut args = vec!["git".to_string(), "fetch".to_string()];
                if let Some(remote) = remote {
                    args.push("--remote".to_string());
                    args.push(remote.to_string());
                }
                this.run_command(args, None, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn push(&mut self, _: &Push, window: &mut Window, cx: &mut Context<Self>) {
        let Some(revision) = self.revision_under_cursor(cx) else {
            return;
        };
        match revision.bookmarks.as_slice() {
            [] => self.set_status("No bookmark on this revision", true, cx),
            [bookmark] => self.push_bookmark(Some(bookmark.clone()), cx),
            bookmarks => {
                let mut options = bookmarks
                    .iter()
                    .map(|bookmark| SharedString::from(bookmark.clone()))
                    .collect::<Vec<_>>();
                options.push("[All]".into());
                self.prompt_then(
                    "Which bookmark do you want to push?",
                    options.clone(),
                    window,
                    cx,
                    move |this, index, cx| {
                        let bookmark = revision.bookmarks.get(index).cloned();
                        this.push_bookmark(bookmark, cx);
                    },
                );
            }
        }
    }

    /// `None` pushes every bookmark (`--all`), as jj.nvim's "[All]" choice does.
    fn push_bookmark(&mut self, bookmark: Option<String>, cx: &mut Context<Self>) {
        let mut args = vec!["git".to_string(), "push".to_string()];
        match bookmark {
            Some(bookmark) => {
                args.push("-b".to_string());
                args.push(bookmark);
            }
            None => args.push("--all".to_string()),
        }
        self.run_command(args, None, cx);
    }

    fn push_any_bookmark(
        &mut self,
        _: &PushAnyBookmark,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let runner = self.runner.clone();
        cx.spawn_in(window, async move |this, cx| {
            let bookmarks = cx
                .background_spawn(async move { list_local_bookmarks(&runner).await })
                .await?;
            this.update_in(cx, |this, window, cx| {
                if bookmarks.is_empty() {
                    this.set_status("No bookmarks to push", true, cx);
                    return;
                }
                let options = bookmarks.iter().cloned().map(SharedString::from).collect();
                this.prompt_then(
                    "Select bookmark to push",
                    options,
                    window,
                    cx,
                    move |this, index, cx| {
                        if let Some(bookmark) = bookmarks.get(index).cloned() {
                            this.push_bookmark(Some(bookmark), cx);
                        }
                    },
                );
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn set_bookmark(&mut self, _: &SetBookmark, window: &mut Window, cx: &mut Context<Self>) {
        let Some(revision) = self.revision_under_cursor(cx) else {
            return;
        };
        let runner = self.runner.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let bookmarks = cx
                .background_spawn(async move { list_local_bookmarks(&runner).await })
                .await?;
            let options = bookmarks.iter().cloned().map(SharedString::from).collect();
            let choice = workspace
                .update_in(cx, |workspace, window, cx| {
                    text_prompt(
                        "Move a bookmark here, or type a new name",
                        options,
                        "Create bookmark",
                        workspace,
                        window,
                        cx,
                    )
                })?
                .await;
            this.update(cx, |this, cx| {
                let revset = revision.revset().to_string();
                match choice {
                    Some(PromptChoice::Option(index)) => {
                        if let Some(bookmark) = bookmarks.get(index) {
                            this.run_command(
                                vec![
                                    "bookmark".to_string(),
                                    "move".to_string(),
                                    bookmark.clone(),
                                    "--to".to_string(),
                                    revset,
                                    "-B".to_string(),
                                ],
                                None,
                                cx,
                            );
                        }
                    }
                    Some(PromptChoice::Custom(name)) => this.run_command(
                        vec![
                            "bookmark".to_string(),
                            "create".to_string(),
                            name,
                            "-r".to_string(),
                            revset,
                        ],
                        None,
                        cx,
                    ),
                    None => {}
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn delete_bookmark(&mut self, _: &DeleteBookmark, window: &mut Window, cx: &mut Context<Self>) {
        let Some(revision) = self.revision_under_cursor(cx) else {
            return;
        };
        if revision.bookmarks.is_empty() {
            self.set_status("No bookmark on this revision", true, cx);
            return;
        }
        let mut options = revision
            .bookmarks
            .iter()
            .map(|bookmark| SharedString::from(bookmark.clone()))
            .collect::<Vec<_>>();
        options.push("[All]".into());
        self.prompt_then(
            "Which bookmark do you want to delete?",
            options,
            window,
            cx,
            move |this, index, cx| {
                let bookmarks = match revision.bookmarks.get(index) {
                    Some(bookmark) => vec![bookmark.clone()],
                    None => revision.bookmarks.clone(),
                };
                let mut args = vec!["bookmark".to_string(), "delete".to_string()];
                args.extend(bookmarks);
                this.run_command(args, None, cx);
            },
        );
    }

    fn change_revset(&mut self, _: &ChangeRevset, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let choice = workspace
                .update_in(cx, |workspace, window, cx| {
                    text_prompt(
                        "Enter a revset",
                        vec!["Default log revset".into()],
                        "Show",
                        workspace,
                        window,
                        cx,
                    )
                })?
                .await;
            this.update(cx, |this, cx| {
                match choice {
                    Some(PromptChoice::Option(_)) => this.revset = None,
                    Some(PromptChoice::Custom(revset)) => this.revset = Some(revset),
                    None => return,
                }
                this.loaded = false;
                this.refresh(cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn prompt_then(
        &mut self,
        prompt: &str,
        options: Vec<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_choice: impl FnOnce(&mut Self, usize, &mut Context<Self>) + 'static,
    ) {
        let choice = picker_prompt::prompt(prompt, options, self.workspace.clone(), window, cx);
        cx.spawn(async move |this, cx| {
            if let Some(index) = choice.await {
                this.update(cx, |this, cx| on_choice(this, index, cx))?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn task_context(&self, revision: &Revision) -> TaskContext {
        let mut task_variables = TaskVariables::from_iter([
            (VariableName::JjChangeId, revision.change_id.clone()),
            (
                VariableName::JjChangeIdShort,
                revision.change_id_short.clone(),
            ),
            (VariableName::JjCommitId, revision.commit_id.clone()),
            (
                VariableName::JjCommitIdShort,
                revision.commit_id_short.clone(),
            ),
            (VariableName::JjBookmarks, revision.bookmarks.join(" ")),
            (
                VariableName::JjRepositoryPath,
                self.runner.root.to_string_lossy().into_owned(),
            ),
        ]);
        // Custom git command tasks work here too: in a colocated repository
        // jj's commit IDs are git SHAs.
        task_variables.insert(VariableName::GitSha, revision.commit_id.clone());
        task_variables.insert(VariableName::GitShaShort, revision.commit_id_short.clone());
        TaskContext {
            cwd: Some(self.runner.root.to_path_buf()),
            task_variables,
            ..TaskContext::default()
        }
    }

    fn resolved_commands(
        &self,
        revision: &Revision,
        cx: &App,
    ) -> Vec<(project::TaskSourceKind, task::ResolvedTask)> {
        let task_inventory = self
            .project
            .read(cx)
            .task_store()
            .read(cx)
            .task_inventory()
            .cloned();
        let Some(task_inventory) = task_inventory else {
            return Vec::new();
        };
        task_inventory
            .read(cx)
            .resolve_global_tasks_with_tag(JJ_COMMAND_TASK_TAG, &self.task_context(revision))
    }

    fn schedule_command(
        &mut self,
        command: (project::TaskSourceKind, task::ResolvedTask),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (task_source_kind, resolved_task) = command;
        let workspace = self.workspace.clone();
        // A task revealed in the center pane adds a tab next to this log,
        // which reads this log and panics while it is still being updated.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.schedule_resolved_task(
                        task_source_kind,
                        resolved_task,
                        false,
                        window,
                        cx,
                    );
                })
                .log_err();
        });
    }

    fn run_custom_command(&mut self, _: &RunCommand, window: &mut Window, cx: &mut Context<Self>) {
        let Some(revision) = self.revision_under_cursor(cx) else {
            return;
        };
        let mut commands = self.resolved_commands(&revision, cx);
        if commands.is_empty() {
            let docs_url = release_channel::docs_url(CUSTOM_JJ_COMMANDS_DOCS_SLUG, cx);
            self.set_status(
                format!(
                    "No tasks tagged \"{JJ_COMMAND_TASK_TAG}\" in your global tasks.json; see {docs_url}"
                ),
                true,
                cx,
            );
            return;
        }
        let options = commands
            .iter()
            .map(|(_, task)| SharedString::from(task.display_label().to_string()))
            .collect::<Vec<_>>();
        let prompt = format!("Run a jj command on {}", revision.short_id());
        let choice =
            text_prompt_without_custom(&prompt, options, self.workspace.clone(), window, cx);
        cx.spawn_in(window, async move |this, cx| {
            if let Some(index) = choice.await
                && index < commands.len()
            {
                let command = commands.swap_remove(index);
                this.update_in(cx, |this, window, cx| {
                    this.schedule_command(command, window, cx)
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn run_custom_command_by_name(
        &mut self,
        action: &RunCommandByName,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(revision) = self.revision_under_cursor(cx) else {
            return;
        };
        let command = self
            .resolved_commands(&revision, cx)
            .into_iter()
            .find(|(_, task)| {
                task.original_task().label == action.task_name
                    || task.display_label() == action.task_name
            });
        match command {
            Some(command) => self.schedule_command(command, window, cx),
            None => self.set_status(
                format!(
                    "No task labeled \"{}\" tagged \"{JJ_COMMAND_TASK_TAG}\"",
                    action.task_name
                ),
                true,
                cx,
            ),
        }
    }

    fn line_height(&self, cx: &App) -> Pixels {
        let settings = ThemeSettings::get_global(cx);
        settings.buffer_font_size(cx) * settings.line_height()
    }

    fn key_context(&self, cx: &App) -> KeyContext {
        let mut key_context = KeyContext::new_with_defaults(cx.platform_info().os);
        key_context.add("JjLog");
        let mode = match &self.placement {
            Some((kind, _)) => kind.label(),
            None => "normal",
        };
        key_context.set("jj_mode", mode);
        if self.visual_anchor.is_some() {
            key_context.add("jj_visual");
        }
        key_context
    }

    fn render_line(&self, index: usize, line_height: Pixels, cx: &App) -> AnyElement {
        let Some(line) = self.lines.get(index) else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors();
        let in_visual_selection = self.visual_anchor.is_some_and(|anchor| {
            (anchor.min(self.cursor)..=anchor.max(self.cursor)).contains(&index)
        });
        let revision = line
            .revision_index
            .and_then(|revision_index| self.revisions.get(revision_index));
        let is_placement_source = self.placement.as_ref().is_some_and(|(_, sources)| {
            revision.is_some_and(|revision| {
                sources
                    .iter()
                    .any(|source| source.change_id == revision.change_id)
            })
        });
        let is_cursor_revision = revision.is_some()
            && revision.map(|revision| &revision.commit_id)
                == self
                    .revision_at(self.cursor)
                    .map(|revision| &revision.commit_id);

        let background = if index == self.cursor {
            Some(colors.element_selected)
        } else if in_visual_selection {
            Some(colors.element_selection_background)
        } else if is_placement_source {
            Some(colors.ghost_element_active)
        } else if is_cursor_revision {
            Some(colors.ghost_element_hover)
        } else {
            None
        };

        let highlights = line
            .styles
            .iter()
            .map(|(range, style)| (range.clone(), highlight_style(style, cx)))
            .collect::<Vec<_>>();
        let text = StyledText::new(line.text.clone()).with_highlights(highlights);

        div()
            .id(("jj-log-line", index))
            .h(line_height)
            .w_full()
            .px_2()
            .flex_none()
            .whitespace_nowrap()
            .when_some(background, |this, background| this.bg(background))
            .child(text)
            .into_any_element()
    }
}

/// Shows the uncommitted changes view, reusing an open one as it is.
///
/// `ProjectDiff::deploy_at` also reuses an open view but first moves its cursor
/// to the top. When the first file is collapsed, that lands inside the file's
/// hidden lines and leaves vim in visual mode over them, so the open view is
/// activated here without moving anything.
fn open_working_copy_diff(
    workspace: &mut Workspace,
    repository: Entity<Repository>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(existing) = workspace.items_of_type::<ProjectDiff>(cx).next() else {
        ProjectDiff::deploy_at(workspace, None, window, cx);
        return;
    };
    let shows_repository = existing
        .read(cx)
        .repo(cx)
        .is_some_and(|current| current == repository);
    if !shows_repository {
        existing.update(cx, |project_diff, cx| {
            project_diff.set_repo(Some(repository), cx)
        });
    }
    workspace.activate_item(&existing, true, true, window, cx);
}

/// Shows a commit, reusing an open view of the same commit so its collapsed
/// files stay collapsed. A commit ID always names the same diff, so an open
/// view is never stale.
fn open_commit_diff(
    workspace: &mut Workspace,
    repository: Entity<Repository>,
    commit_id: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<CommitView>(cx).find(|view| {
        let view = view.read(cx);
        view.commit_sha().as_ref() == commit_id
            && view.shows_whole_commit()
            && view.repository() == &repository
    });
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    CommitView::open(
        commit_id,
        repository.downgrade(),
        workspace.weak_handle(),
        Default::default(),
        window,
        cx,
    );
}

fn capitalize(text: &str) -> String {
    let mut characters = text.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

async fn list_local_bookmarks(runner: &JjRunner) -> Result<Vec<String>> {
    let output = runner
        .run(&["bookmark", "list", "-T", r#"if(!remote, name ++ "\n")"#])
        .await?;
    let mut bookmarks = output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    bookmarks.dedup();
    Ok(bookmarks)
}

/// jj stores operation heads under the repo directory, which a secondary
/// workspace references through a `.jj/repo` file instead of containing it.
async fn op_heads_dir(fs: &dyn fs::Fs, root: &Path) -> Option<PathBuf> {
    let jj_dir = root.join(".jj");
    let repo = jj_dir.join("repo");
    let repo = if fs.is_file(&repo).await {
        let contents = fs.load(&repo).await.log_err()?;
        let path = PathBuf::from(contents.trim());
        if path.is_absolute() {
            path
        } else {
            jj_dir.join(path)
        }
    } else {
        repo
    };
    Some(repo.join("op_heads").join("heads"))
}

impl EventEmitter<ItemEvent> for JjLog {}

impl Focusable for JjLog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for JjLog {
    type Event = ItemEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitGraph))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        let name = self
            .runner
            .root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("jj log: {name}").into()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.runner.root.to_string_lossy().into_owned().into())
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for JjLog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let font = settings.buffer_font.clone();
        let font_size = settings.buffer_font_size(cx);
        let line_height = self.line_height(cx);
        let colors = cx.theme().colors();
        let line_count = self.lines.len();

        let mode_label = match &self.placement {
            Some((kind, _)) => {
                let hint = match kind {
                    PlacementKind::Squash => "into",
                    PlacementKind::Rebase | PlacementKind::Duplicate => "onto / after / before",
                };
                Some(format!(
                    "{} — pick a destination ({hint}), esc to cancel",
                    kind.label().to_uppercase()
                ))
            }
            None if self.visual_anchor.is_some() => Some("VISUAL".to_string()),
            None => None,
        };

        v_flex()
            .key_context(self.key_context(cx))
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .font(font)
            .text_size(font_size)
            .text_color(colors.terminal_foreground)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::select_next_revision))
            .on_action(cx.listener(Self::select_previous_revision))
            .on_action(cx.listener(Self::select_working_copy))
            .on_action(cx.listener(Self::scroll_down))
            .on_action(cx.listener(Self::scroll_up))
            .on_action(cx.listener(Self::toggle_visual_selection))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::refresh_action))
            .on_action(cx.listener(Self::toggle_stat))
            .on_action(cx.listener(Self::change_revset))
            .on_action(cx.listener(Self::edit))
            .on_action(cx.listener(Self::edit_ignore_immutable))
            .on_action(cx.listener(Self::describe))
            .on_action(cx.listener(Self::view_diff))
            .on_action(cx.listener(Self::new_change))
            .on_action(cx.listener(Self::new_after))
            .on_action(cx.listener(Self::new_after_ignore_immutable))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::abandon))
            .on_action(cx.listener(Self::fetch))
            .on_action(cx.listener(Self::push))
            .on_action(cx.listener(Self::push_any_bookmark))
            .on_action(cx.listener(Self::set_bookmark))
            .on_action(cx.listener(Self::delete_bookmark))
            .on_action(cx.listener(Self::rebase))
            .on_action(cx.listener(Self::squash))
            .on_action(cx.listener(Self::quick_squash))
            .on_action(cx.listener(Self::duplicate))
            .on_action(cx.listener(Self::place_onto))
            .on_action(cx.listener(Self::place_onto_ignore_immutable))
            .on_action(cx.listener(Self::place_after))
            .on_action(cx.listener(Self::place_after_ignore_immutable))
            .on_action(cx.listener(Self::place_before))
            .on_action(cx.listener(Self::place_before_ignore_immutable))
            .on_action(cx.listener(Self::run_custom_command))
            .on_action(cx.listener(Self::run_custom_command_by_name))
            .child(
                uniform_list(
                    "jj-log-lines",
                    line_count,
                    cx.processor(move |this, range: Range<usize>, _window, cx| {
                        range
                            .map(|index| this.render_line(index, line_height, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll_handle)
                .flex_1()
                .w_full(),
            )
            .child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .h(line_height)
                    .px_2()
                    .gap_2()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .bg(colors.status_bar_background)
                    .when_some(mode_label, |this, label| {
                        this.child(
                            div()
                                .flex_none()
                                .text_color(colors.text_accent)
                                .font_weight(FontWeight::BOLD)
                                .child(label),
                        )
                    })
                    .when_some(self.status.as_ref(), |this, status| {
                        this.child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_color(if status.is_error {
                                    colors.terminal_ansi_red
                                } else {
                                    colors.text_muted
                                })
                                .child(status.message.clone()),
                        )
                    }),
            )
    }
}

struct DescribeModal {
    editor: Entity<Editor>,
    log: WeakEntity<JjLog>,
    revision: Revision,
}

impl DescribeModal {
    fn new(
        log: WeakEntity<JjLog>,
        revision: Revision,
        description: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_text(description.trim_end(), window, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        Self {
            editor,
            log,
            revision,
        }
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let message = self.editor.read(cx).text(cx);
        let revision = self.revision.clone();
        self.log
            .update(cx, |log, cx| log.save_description(&revision, message, cx))
            .log_err();
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for DescribeModal {}
impl ModalView for DescribeModal {}

impl Focusable for DescribeModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for DescribeModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let font = settings.buffer_font.clone();
        let line_height = settings.buffer_font_size(cx) * settings.line_height();
        v_flex()
            .key_context("JjDescribe")
            .elevation_3(cx)
            .w(rems(40.))
            .p_2()
            .gap_2()
            .on_action(cx.listener(|this, _: &ConfirmDescribe, _, cx| this.confirm(cx)))
            .on_action(cx.listener(|this, _: &workspace::Save, _, cx| this.confirm(cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(
                Label::new(format!("Describe {}", self.revision.short_id()))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .h(line_height * 12.)
                    .font(font)
                    .p_1()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_sm()
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Label::new("Save")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(KeyBinding::for_action_in(
                        &ConfirmDescribe,
                        &self.editor.focus_handle(cx),
                        cx,
                    )),
            )
    }
}

enum PromptChoice {
    Option(usize),
    Custom(String),
}

/// A picker over `options` that can also accept whatever the user typed.
fn text_prompt(
    placeholder: &str,
    options: Vec<SharedString>,
    custom_label: &'static str,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Option<PromptChoice>> {
    let (sender, receiver) = oneshot::channel();
    let delegate = TextPromptDelegate {
        placeholder: placeholder.into(),
        options,
        matches: Vec::new(),
        query: String::new(),
        selected_index: 0,
        custom_label: Some(custom_label),
        sender: Some(sender),
    };
    workspace.toggle_modal(window, cx, |window, cx| {
        TextPrompt::new(delegate, window, cx)
    });
    cx.background_spawn(async move { receiver.await.ok() })
}

fn text_prompt_without_custom(
    placeholder: &str,
    options: Vec<SharedString>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Task<Option<usize>> {
    let (sender, receiver) = oneshot::channel();
    let delegate = TextPromptDelegate {
        placeholder: placeholder.into(),
        options,
        matches: Vec::new(),
        query: String::new(),
        selected_index: 0,
        custom_label: None,
        sender: Some(sender),
    };
    workspace
        .update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                TextPrompt::new(delegate, window, cx)
            });
        })
        .log_err();
    cx.background_spawn(async move {
        match receiver.await.ok()? {
            PromptChoice::Option(index) => Some(index),
            PromptChoice::Custom(_) => None,
        }
    })
}

struct TextPrompt {
    picker: Entity<Picker<TextPromptDelegate>>,
    _subscription: Subscription,
}

impl TextPrompt {
    fn new(delegate: TextPromptDelegate, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let picker =
            cx.new(|cx| Picker::uniform_list(delegate, window, cx).initial_width(rems(34.)));
        let subscription = cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent));
        Self {
            picker,
            _subscription: subscription,
        }
    }
}

impl ModalView for TextPrompt {}
impl EventEmitter<DismissEvent> for TextPrompt {}

impl Focusable for TextPrompt {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for TextPrompt {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

struct TextPromptDelegate {
    placeholder: Arc<str>,
    options: Vec<SharedString>,
    matches: Vec<StringMatch>,
    query: String,
    selected_index: usize,
    custom_label: Option<&'static str>,
    sender: Option<oneshot::Sender<PromptChoice>>,
}

impl TextPromptDelegate {
    fn shows_custom_entry(&self) -> bool {
        let query = self.query.trim();
        self.custom_label.is_some()
            && !query.is_empty()
            && !self.options.iter().any(|option| option.as_ref() == query)
    }
}

impl PickerDelegate for TextPromptDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "jj prompt"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        self.placeholder.clone()
    }

    fn match_count(&self) -> usize {
        self.matches.len() + usize::from(self.shows_custom_entry())
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates = self
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| StringMatchCandidate::new(index, option))
            .collect::<Vec<_>>();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                fuzzy::match_strings(
                    &candidates,
                    &query,
                    true,
                    true,
                    10000,
                    &Default::default(),
                    cx.background_executor().clone(),
                )
                .await
            };
            picker
                .update(cx, |picker, _| {
                    let delegate = &mut picker.delegate;
                    delegate.matches = matches;
                    delegate.query = query;
                    delegate.selected_index = 0;
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        let choice = match self.matches.get(self.selected_index) {
            Some(hit) => PromptChoice::Option(hit.candidate_id),
            None if self.shows_custom_entry() => {
                PromptChoice::Custom(self.query.trim().to_string())
            }
            None => return,
        };
        if let Some(sender) = self.sender.take() {
            sender.send(choice).ok();
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let item = ListItem::new(("jj-prompt-item", index))
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected);
        match self.matches.get(index) {
            Some(hit) => Some(item.child(HighlightedLabel::new(
                hit.string.clone(),
                hit.positions.clone(),
            ))),
            None => {
                let label = self.custom_label?;
                Some(item.child(Label::new(format!("{label} “{}”", self.query.trim()))))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ansi_runs() {
        let (text, styles) =
            parse_ansi("\x1b[1m\x1b[38;5;2m@\x1b[0m  plain \x1b[38;5;13mv\x1b[39m");
        assert_eq!(text, "@  plain v");
        assert_eq!(
            styles,
            vec![
                (
                    0..1,
                    AnsiStyle {
                        foreground: Some(AnsiColor::Indexed(2)),
                        bold: true,
                        ..Default::default()
                    }
                ),
                (
                    9..10,
                    AnsiStyle {
                        foreground: Some(AnsiColor::Indexed(13)),
                        ..Default::default()
                    }
                ),
            ]
        );
    }

    #[test]
    fn test_parse_ansi_truecolor_and_bright() {
        let (text, styles) = parse_ansi("\x1b[38;2;10;20;30;4mx\x1b[24;91my\x1b[m");
        assert_eq!(text, "xy");
        assert_eq!(
            styles,
            vec![
                (
                    0..1,
                    AnsiStyle {
                        foreground: Some(AnsiColor::Rgb(10, 20, 30)),
                        underline: true,
                        ..Default::default()
                    }
                ),
                (
                    1..2,
                    AnsiStyle {
                        foreground: Some(AnsiColor::Indexed(9)),
                        ..Default::default()
                    }
                ),
            ]
        );
    }

    #[test]
    fn test_parse_log_output_assigns_lines_to_revisions() {
        let output = concat!(
            "\x1b[1m\x1b[38;5;2m@\x1b[0m  \x1f\x1b[38;5;5mvxqo\x1b[39m\x1f\x1b[1mv\x1b[0m\x1fcdae\x1fc\x1f1\x1f\x1f\x1f1\x1f\x1evxqopwrx me\n",
            "│  (no description set)\n",
            "◆  \x1fuzqr\x1fu\x1f8e47\x1f8\x1f\x1f\x1f1\x1f2\x1fmain feature\x1euzqrlmow main\n",
            "│  gpui: a commit\n",
            "~\n",
        );
        let parsed = parse_log_output(output);
        assert_eq!(
            parsed
                .lines
                .iter()
                .map(|line| (line.text.as_ref(), line.revision_index))
                .collect::<Vec<_>>(),
            vec![
                ("@  vxqopwrx me", Some(0)),
                ("│  (no description set)", Some(0)),
                ("◆  uzqrlmow main", Some(1)),
                ("│  gpui: a commit", Some(1)),
                ("~", Some(1)),
            ]
        );
        assert_eq!(
            parsed.revisions,
            vec![
                Revision {
                    change_id: "vxqo".into(),
                    change_id_short: "v".into(),
                    commit_id: "cdae".into(),
                    commit_id_short: "c".into(),
                    is_working_copy: true,
                    is_divergent: false,
                    is_immutable: false,
                    parent_count: 1,
                    bookmarks: Vec::new(),
                    first_line: 0,
                },
                Revision {
                    change_id: "uzqr".into(),
                    change_id_short: "u".into(),
                    commit_id: "8e47".into(),
                    commit_id_short: "8".into(),
                    is_working_copy: false,
                    is_divergent: false,
                    is_immutable: true,
                    parent_count: 2,
                    bookmarks: vec!["main".into(), "feature".into()],
                    first_line: 2,
                },
            ]
        );
        assert_eq!(parsed.lines[0].styles[0].0, 0..1);
    }

    #[test]
    fn test_divergent_revision_uses_commit_id() {
        let revision = parse_marker("x\x1fx\x1fabc\x1fa\x1f\x1f1\x1f\x1f1\x1f", 0).unwrap();
        assert_eq!(revision.revset(), "abc");
        assert_eq!(revision.short_id(), "a");
    }

    #[test]
    fn test_unreadable_parent_count_is_zero() {
        let revision = parse_marker("x\x1fx\x1fabc\x1fa\x1f1\x1f\x1f\x1fmany\x1f", 0).unwrap();
        assert_eq!(revision.parent_count, 0);
        assert!(parse_marker("x\x1fx\x1fabc\x1fa\x1f1\x1f\x1f\x1f", 0).is_none());
    }
}
