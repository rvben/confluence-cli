// THESIS: Confluence becomes a working proof where hierarchy, readable copy, and local corrections share one calm terminal sheet.
// OWN-WORLD: Pale galley stock, dark ink, blue-pencil focus, correction magenta, amber attention, fine rules, folio numbers, and restrained proof marks.
// STORY: Choose a space, scan its contents, unfold a page into a readable proof, inspect its outer-margin evidence, then review local sync changes before returning to the shell.
// FIRST VIEWPORT: A narrow contents folio sits left of a dominant galley while metadata and sync annotations occupy the outer margin; compact terminals reveal one complete surface at a time.
// FORM: Proof Desk, model-selected grounded direction; seed 8074cc2a. The selected page unfolds into a readable proof, and Review preserves its own folio position while the margin changes to diff evidence.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use tokio::sync::oneshot;

use crate::markdown::storage_to_markdown;
use crate::model::{
    AttachmentInfo, CommentInfo, ContentItem, ContentProperty, PlanActionKind, PlanItem,
    SpaceSummary, SyncPlan,
};
use crate::provider::ConfluenceProvider;
use crate::sync;

const MIN_WIDTH: u16 = 52;
const MIN_HEIGHT: u16 = 16;
const WIDE_WIDTH: u16 = 118;
const MEDIUM_WIDTH: u16 = 82;

#[derive(Clone, Debug)]
pub struct TuiOptions {
    pub profile: String,
    pub site: String,
    pub initial_space: Option<String>,
    pub review_path: Option<PathBuf>,
    pub delete_remote: bool,
    pub page_size: usize,
    pub color: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Mode {
    #[default]
    Browse,
    Review,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Browse => "BROWSE",
            Self::Review => "REVIEW",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum MarginTab {
    #[default]
    Metadata,
    Attachments,
    Comments,
    Properties,
}

impl MarginTab {
    fn label(self) -> &'static str {
        match self {
            Self::Metadata => "META",
            Self::Attachments => "FILE",
            Self::Comments => "NOTE",
            Self::Properties => "PROP",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum Overlay {
    #[default]
    None,
    Proof,
    Margin,
    Help,
    SpaceInput(String),
    PathInput(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    None,
    Quit,
    LoadWorkspace,
    LoadPage,
    Replan,
    Open,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NoticeKind {
    Quiet,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
struct Notice {
    kind: NoticeKind,
    text: String,
}

impl Notice {
    fn quiet(text: impl Into<String>) -> Self {
        Self::new(NoticeKind::Quiet, text)
    }

    fn success(text: impl Into<String>) -> Self {
        Self::new(NoticeKind::Success, text)
    }

    fn warning(text: impl Into<String>) -> Self {
        Self::new(NoticeKind::Warning, text)
    }

    fn error(text: impl Into<String>) -> Self {
        Self::new(NoticeKind::Error, text)
    }

    fn new(kind: NoticeKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: safe_text(&text.into()),
        }
    }
}

#[derive(Clone, Debug, Default)]
enum ResourceState<T> {
    #[default]
    Idle,
    Ready(Vec<T>),
    Failed(String),
}

#[derive(Clone, Debug)]
struct PageRow {
    item: ContentItem,
    depth: usize,
}

#[derive(Clone, Debug, Default)]
struct PageProof {
    item: Option<ContentItem>,
    markdown: String,
    attachments: ResourceState<AttachmentInfo>,
    comments: ResourceState<CommentInfo>,
    properties: ResourceState<ContentProperty>,
}

impl PageProof {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn matches(&self, id: &str) -> bool {
        self.item.as_ref().is_some_and(|item| item.id == id)
    }
}

struct App {
    profile: String,
    site: String,
    requested_space: Option<String>,
    page_size: usize,
    color: bool,
    mode: Mode,
    spaces: Vec<SpaceSummary>,
    active_space: Option<SpaceSummary>,
    pages: Vec<PageRow>,
    page_state: ListState,
    page_proof: PageProof,
    workspace_loaded: bool,
    workspace_loading: bool,
    page_loading: bool,
    pages_truncated: bool,
    review_path: Option<PathBuf>,
    delete_remote: bool,
    review_items: Vec<PlanItem>,
    review_state: ListState,
    review_error: Option<String>,
    overlay: Overlay,
    margin_tab: MarginTab,
    body_scroll: u16,
    overlay_scroll: u16,
    overlay_max_scroll: u16,
    notice: Notice,
}

impl App {
    fn new(options: TuiOptions) -> Self {
        let mode = if options.review_path.is_some() {
            Mode::Review
        } else {
            Mode::Browse
        };
        Self {
            profile: safe_text(&options.profile),
            site: compact_site(&options.site),
            requested_space: options.initial_space,
            page_size: options.page_size,
            color: options.color,
            mode,
            spaces: Vec::new(),
            active_space: None,
            pages: Vec::new(),
            page_state: ListState::default(),
            page_proof: PageProof::default(),
            workspace_loaded: false,
            workspace_loading: false,
            page_loading: false,
            pages_truncated: false,
            review_path: options.review_path,
            delete_remote: options.delete_remote,
            review_items: Vec::new(),
            review_state: ListState::default(),
            review_error: None,
            overlay: Overlay::None,
            margin_tab: MarginTab::Metadata,
            body_scroll: 0,
            overlay_scroll: 0,
            overlay_max_scroll: 0,
            notice: Notice::quiet(if mode == Mode::Review {
                "Reading the local sync proof..."
            } else {
                "Preparing the proof desk..."
            }),
        }
    }

    async fn load_workspace(&mut self, provider: &dyn ConfluenceProvider) {
        self.workspace_loading = true;
        self.notice = Notice::quiet("Indexing visible spaces...");
        match provider.list_spaces_window(self.page_size).await {
            Ok(window) => self.spaces = window.items,
            Err(error) => {
                self.workspace_loading = false;
                self.workspace_loaded = false;
                self.pages.clear();
                self.page_state.select(None);
                self.notice = Notice::error(format!(
                    "Could not load spaces: {error}. Press r to retry or q to quit."
                ));
                return;
            }
        }

        let chosen = self.requested_space.as_deref().and_then(|requested| {
            self.spaces
                .iter()
                .find(|space| space.key.eq_ignore_ascii_case(requested) || space.id == requested)
                .cloned()
        });
        let chosen = match (chosen, self.requested_space.as_deref()) {
            (Some(space), _) => Some(space),
            (None, Some(requested)) => match provider.get_space(requested).await {
                Ok(space) => Some(space),
                Err(error) => {
                    self.workspace_loading = false;
                    self.workspace_loaded = false;
                    self.pages.clear();
                    self.page_state.select(None);
                    self.notice = Notice::error(format!(
                        "Space `{}` could not be opened: {error}. Press s to choose another space.",
                        safe_text(requested)
                    ));
                    return;
                }
            },
            (None, None) => self.spaces.first().cloned(),
        };

        let Some(space) = chosen else {
            self.workspace_loading = false;
            self.workspace_loaded = true;
            self.active_space = None;
            self.pages.clear();
            self.page_state.select(None);
            self.notice = Notice::warning(
                "No visible spaces were returned. Press s to enter a space key or r to retry.",
            );
            return;
        };

        self.notice = Notice::quiet(format!("Reading the {} contents folio...", space.key));
        match provider
            .list_space_content_window(crate::model::ContentKind::Page, &space.key, self.page_size)
            .await
        {
            Ok(window) => {
                self.pages_truncated = window.has_more;
                self.pages = page_rows(window.items, self.page_size);
                self.active_space = Some(space);
                self.page_state
                    .select((!self.pages.is_empty()).then_some(0));
                self.page_proof.clear();
                self.body_scroll = 0;
                self.workspace_loaded = true;
                self.workspace_loading = false;
                self.notice = if self.pages.is_empty() {
                    Notice::warning(
                        "This space has no visible pages. Press s to choose another space.",
                    )
                } else if self.pages_truncated {
                    let scope = window.total.map_or_else(
                        || format!("the first {} pages", self.pages.len()),
                        |total| format!("the first {} of {total} pages", self.pages.len()),
                    );
                    Notice::warning(format!("Showing {scope}. Raise --page-size to see more."))
                } else {
                    Notice::success(format!(
                        "Indexed {} page{} in {}.",
                        self.pages.len(),
                        if self.pages.len() == 1 { "" } else { "s" },
                        self.active_space
                            .as_ref()
                            .map_or("this space", |space| space.key.as_str())
                    ))
                };
            }
            Err(error) => {
                self.workspace_loading = false;
                self.workspace_loaded = false;
                self.active_space = Some(space);
                self.pages.clear();
                self.page_state.select(None);
                self.notice = Notice::error(format!(
                    "Could not load this space's pages: {error}. Press r to retry or s to change space."
                ));
                return;
            }
        }

        if !self.pages.is_empty() {
            self.load_selected_page(provider).await;
        }
    }

    async fn load_selected_page(&mut self, provider: &dyn ConfluenceProvider) {
        let Some(id) = self.selected_page().map(|row| row.item.id.clone()) else {
            self.notice = Notice::warning("Select a page before opening its proof.");
            return;
        };
        self.page_loading = true;
        self.notice = Notice::quiet("Unfolding the selected page proof...");
        let item = match provider
            .get_content(crate::model::ContentKind::Page, &id, true)
            .await
        {
            Ok(item) => item,
            Err(error) => {
                self.page_loading = false;
                self.notice = Notice::error(format!(
                    "Could not read the selected page: {error}. Press Enter to retry."
                ));
                return;
            }
        };
        let markdown = storage_to_markdown(item.body_storage.as_deref().unwrap_or_default());
        self.page_proof = PageProof {
            item: Some(item),
            markdown,
            attachments: ResourceState::Idle,
            comments: ResourceState::Idle,
            properties: ResourceState::Idle,
        };

        let (attachments, comments, properties) = tokio::join!(
            provider.list_attachments(&id),
            provider.list_comments(&id),
            provider.list_properties(&id)
        );
        self.page_proof.attachments = match attachments {
            Ok(items) => ResourceState::Ready(items),
            Err(error) => ResourceState::Failed(safe_text(&error.to_string())),
        };
        self.page_proof.comments = match comments {
            Ok(items) => ResourceState::Ready(items),
            Err(error) => ResourceState::Failed(safe_text(&error.to_string())),
        };
        self.page_proof.properties = match properties {
            Ok(items) => ResourceState::Ready(items),
            Err(error) => ResourceState::Failed(safe_text(&error.to_string())),
        };
        self.page_loading = false;
        self.body_scroll = 0;
        self.notice = Notice::success("Page proof and margin evidence are ready.");
    }

    async fn load_review(&mut self) {
        let Some(path) = self.review_path.as_ref() else {
            self.review_items.clear();
            self.review_state.select(None);
            self.review_error = Some("No local sync directory selected.".into());
            return;
        };
        self.notice = Notice::quiet("Reading the local sync proof...");
        let path = path.clone();
        let delete_remote = self.delete_remote;
        let result = match launch_review_plan(path, delete_remote) {
            Ok(receiver) => receiver
                .await
                .map_err(|_| anyhow!("the local planner stopped without returning a result")),
            Err(error) => Err(error),
        };
        match result {
            Ok(Ok(plan)) => {
                self.review_items = plan.items;
                self.review_state
                    .select((!self.review_items.is_empty()).then_some(0));
                self.review_error = None;
                self.body_scroll = 0;
                let changed = self
                    .review_items
                    .iter()
                    .filter(|item| item.action != PlanActionKind::Noop)
                    .count();
                self.notice = if changed == 0 {
                    Notice::success("Local proof is clean; no changes detected.")
                } else {
                    Notice::warning(format!(
                        "{changed} local change{} ready for review; remote drift is checked by apply.",
                        if changed == 1 { " is" } else { "s are" }
                    ))
                };
            }
            Ok(Err(error)) => {
                self.review_items.clear();
                self.review_state.select(None);
                self.review_error = Some(safe_text(&error.to_string()));
                self.notice = Notice::error(
                    "Could not build the local sync proof. Press p to choose another directory.",
                );
            }
            Err(error) => {
                self.review_items.clear();
                self.review_state.select(None);
                self.review_error = Some("The local planner stopped unexpectedly.".into());
                self.notice = Notice::error(format!(
                    "The local planner stopped unexpectedly: {error}. Press r to retry."
                ));
            }
        }
    }

    fn selected_page(&self) -> Option<&PageRow> {
        self.page_state
            .selected()
            .and_then(|index| self.pages.get(index))
    }

    fn selected_review(&self) -> Option<&PlanItem> {
        self.review_state
            .selected()
            .and_then(|index| self.review_items.get(index))
    }

    fn move_selection(&mut self, delta: isize) {
        let (state, len) = match self.mode {
            Mode::Browse => (&mut self.page_state, self.pages.len()),
            Mode::Review => (&mut self.review_state, self.review_items.len()),
        };
        if len == 0 {
            return;
        }
        let current = state.selected().unwrap_or(0);
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(len - 1)
        };
        state.select(Some(next));
        self.body_scroll = 0;
    }

    fn move_overlay_scroll(&mut self, delta: isize) {
        if delta.is_negative() {
            self.overlay_scroll = self
                .overlay_scroll
                .saturating_sub(delta.unsigned_abs().min(usize::from(u16::MAX)) as u16);
        } else {
            self.overlay_scroll = self
                .overlay_scroll
                .saturating_add((delta as usize).min(usize::from(u16::MAX)) as u16)
                .min(self.overlay_max_scroll);
        }
    }

    fn reset_overlay_scroll(&mut self) {
        self.overlay_scroll = 0;
        self.overlay_max_scroll = 0;
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        match &mut self.overlay {
            Overlay::Help => match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                    self.overlay = Overlay::None;
                    self.reset_overlay_scroll();
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_overlay_scroll(1);
                    Action::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_overlay_scroll(-1);
                    Action::None
                }
                KeyCode::PageDown => {
                    self.move_overlay_scroll(8);
                    Action::None
                }
                KeyCode::PageUp => {
                    self.move_overlay_scroll(-8);
                    Action::None
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.overlay_scroll = 0;
                    Action::None
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.overlay_scroll = self.overlay_max_scroll;
                    Action::None
                }
                _ => Action::None,
            },
            Overlay::SpaceInput(buffer) | Overlay::PathInput(buffer) => match key.code {
                KeyCode::Esc => {
                    self.overlay = Overlay::None;
                    Action::None
                }
                KeyCode::Enter => self.commit_input(),
                KeyCode::Backspace => {
                    buffer.pop();
                    Action::None
                }
                KeyCode::Char(character)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !character.is_control() =>
                {
                    buffer.push(character);
                    Action::None
                }
                _ => Action::None,
            },
            Overlay::Proof => match key.code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
                    self.overlay = Overlay::None;
                    self.body_scroll = 0;
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.body_scroll = self.body_scroll.saturating_add(1);
                    Action::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.body_scroll = self.body_scroll.saturating_sub(1);
                    Action::None
                }
                KeyCode::PageDown => {
                    self.body_scroll = self.body_scroll.saturating_add(8);
                    Action::None
                }
                KeyCode::PageUp => {
                    self.body_scroll = self.body_scroll.saturating_sub(8);
                    Action::None
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.body_scroll = 0;
                    Action::None
                }
                KeyCode::Char('m') => {
                    self.overlay = Overlay::Margin;
                    self.reset_overlay_scroll();
                    Action::None
                }
                KeyCode::Char('o') if self.mode == Mode::Browse => Action::Open,
                KeyCode::Char('r') if self.mode == Mode::Browse => Action::LoadPage,
                _ => Action::None,
            },
            Overlay::Margin => match key.code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('m') => {
                    self.overlay = Overlay::None;
                    self.reset_overlay_scroll();
                    Action::None
                }
                KeyCode::Char('1') => {
                    self.margin_tab = MarginTab::Metadata;
                    self.reset_overlay_scroll();
                    Action::None
                }
                KeyCode::Char('2') => {
                    self.margin_tab = MarginTab::Attachments;
                    self.reset_overlay_scroll();
                    Action::None
                }
                KeyCode::Char('3') => {
                    self.margin_tab = MarginTab::Comments;
                    self.reset_overlay_scroll();
                    Action::None
                }
                KeyCode::Char('4') => {
                    self.margin_tab = MarginTab::Properties;
                    self.reset_overlay_scroll();
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_overlay_scroll(1);
                    Action::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_overlay_scroll(-1);
                    Action::None
                }
                KeyCode::PageDown => {
                    self.move_overlay_scroll(8);
                    Action::None
                }
                KeyCode::PageUp => {
                    self.move_overlay_scroll(-8);
                    Action::None
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.overlay_scroll = 0;
                    Action::None
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.overlay_scroll = self.overlay_max_scroll;
                    Action::None
                }
                _ => Action::None,
            },
            Overlay::None => self.handle_main_key(key),
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Action::None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                match self.mode {
                    Mode::Browse => self
                        .page_state
                        .select((!self.pages.is_empty()).then_some(0)),
                    Mode::Review => self
                        .review_state
                        .select((!self.review_items.is_empty()).then_some(0)),
                }
                self.body_scroll = 0;
                Action::None
            }
            KeyCode::End | KeyCode::Char('G') => {
                match self.mode {
                    Mode::Browse => self
                        .page_state
                        .select((!self.pages.is_empty()).then_some(self.pages.len() - 1)),
                    Mode::Review => self.review_state.select(
                        (!self.review_items.is_empty()).then_some(self.review_items.len() - 1),
                    ),
                }
                self.body_scroll = 0;
                Action::None
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match self.mode {
                Mode::Browse if self.selected_page().is_some() => {
                    self.overlay = Overlay::Proof;
                    if self
                        .selected_page()
                        .is_some_and(|row| self.page_proof.matches(&row.item.id))
                    {
                        Action::None
                    } else {
                        Action::LoadPage
                    }
                }
                Mode::Review if self.selected_review().is_some() => {
                    self.overlay = Overlay::Proof;
                    Action::None
                }
                _ => Action::None,
            },
            KeyCode::Tab => self.toggle_mode(),
            KeyCode::Char('r') => match self.mode {
                Mode::Browse => Action::LoadWorkspace,
                Mode::Review => Action::Replan,
            },
            KeyCode::Char('s') => {
                self.overlay = Overlay::SpaceInput(
                    self.active_space
                        .as_ref()
                        .map(|space| space.key.clone())
                        .unwrap_or_default(),
                );
                Action::None
            }
            KeyCode::Char('p') => {
                self.overlay = Overlay::PathInput(
                    self.review_path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
                );
                Action::None
            }
            KeyCode::Char('m') => {
                self.overlay = Overlay::Margin;
                self.reset_overlay_scroll();
                Action::None
            }
            KeyCode::Char('1') if self.mode == Mode::Browse => {
                self.margin_tab = MarginTab::Metadata;
                Action::None
            }
            KeyCode::Char('2') if self.mode == Mode::Browse => {
                self.margin_tab = MarginTab::Attachments;
                Action::None
            }
            KeyCode::Char('3') if self.mode == Mode::Browse => {
                self.margin_tab = MarginTab::Comments;
                Action::None
            }
            KeyCode::Char('4') if self.mode == Mode::Browse => {
                self.margin_tab = MarginTab::Properties;
                Action::None
            }
            KeyCode::Char('o') if self.mode == Mode::Browse => Action::Open,
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
                self.reset_overlay_scroll();
                Action::None
            }
            KeyCode::PageDown => {
                self.body_scroll = self.body_scroll.saturating_add(8);
                Action::None
            }
            KeyCode::PageUp => {
                self.body_scroll = self.body_scroll.saturating_sub(8);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn toggle_mode(&mut self) -> Action {
        match self.mode {
            Mode::Browse => {
                if self.review_path.is_none() {
                    self.overlay = Overlay::PathInput(String::new());
                    self.notice = Notice::quiet("Enter a pulled Markdown directory to review.");
                    Action::None
                } else {
                    self.mode = Mode::Review;
                    self.body_scroll = 0;
                    Action::Replan
                }
            }
            Mode::Review => {
                self.mode = Mode::Browse;
                self.body_scroll = 0;
                if self.workspace_loaded {
                    Action::None
                } else {
                    Action::LoadWorkspace
                }
            }
        }
    }

    fn commit_input(&mut self) -> Action {
        match std::mem::take(&mut self.overlay) {
            Overlay::SpaceInput(value) => {
                let value = value.trim();
                if value.is_empty() {
                    self.notice = Notice::warning("Enter a Confluence space key or ID.");
                    return Action::None;
                }
                self.requested_space = Some(value.to_string());
                self.mode = Mode::Browse;
                Action::LoadWorkspace
            }
            Overlay::PathInput(value) => {
                let value = value.trim();
                if value.is_empty() {
                    self.notice = Notice::warning("Enter a local Markdown sync directory.");
                    return Action::None;
                }
                self.review_path = Some(PathBuf::from(value));
                self.mode = Mode::Review;
                Action::Replan
            }
            overlay => {
                self.overlay = overlay;
                Action::None
            }
        }
    }

    fn open_selected(&mut self) {
        let url = match self.selected_browser_url() {
            Ok(url) => url,
            Err(message) => {
                self.notice = Notice::warning(message);
                return;
            }
        };
        match open::that(url) {
            Ok(()) => self.notice = Notice::success("Opened the selected page in Confluence."),
            Err(error) => {
                self.notice = Notice::error(format!("Could not open the browser: {error}"))
            }
        }
    }

    fn selected_browser_url(&self) -> std::result::Result<&str, &'static str> {
        let Some(selected) = self.selected_page() else {
            return Err("Select a page before opening it in Confluence.");
        };
        let url = selected.item.web_url.as_deref().or_else(|| {
            self.page_proof
                .matches(&selected.item.id)
                .then(|| self.page_proof.item.as_ref()?.web_url.as_deref())
                .flatten()
        });
        let Some(url) = url else {
            return Err("This page did not include a browser URL.");
        };
        let Ok(parsed) = reqwest::Url::parse(url) else {
            return Err("This page included an invalid browser URL.");
        };
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err("Confluence browser URLs must use http or https.");
        }
        Ok(url)
    }

    fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        let theme = Theme::new(self.color);
        frame.render_widget(Block::default().style(theme.canvas()), frame.area());
        if frame.area().width < MIN_WIDTH || frame.area().height < MIN_HEIGHT {
            self.render_too_small(frame, theme);
            return;
        }
        let footer_height = if frame.area().height < 20 { 2 } else { 3 };
        let areas = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(footer_height),
        ])
        .split(frame.area());
        self.render_header(frame, areas[0], theme);
        self.render_body(frame, areas[1], theme);
        self.render_footer(frame, areas[2], theme);

        match self.overlay.clone() {
            Overlay::Proof => self.render_proof_overlay(frame, theme),
            Overlay::Margin => self.render_margin_overlay(frame, theme),
            Overlay::Help => self.render_help(frame, theme),
            Overlay::SpaceInput(buffer) => render_input(
                frame,
                theme,
                "CHOOSE A SPACE",
                "Confluence space key or ID",
                &buffer,
            ),
            Overlay::PathInput(buffer) => render_input(
                frame,
                theme,
                "OPEN A LOCAL PROOF",
                "Markdown sync directory created by pull",
                &buffer,
            ),
            Overlay::None => {}
        }
    }

    fn render_too_small(&self, frame: &mut ratatui::Frame<'_>, theme: Theme) {
        let message = Paragraph::new(vec![
            Line::styled("THE PROOF DESK NEEDS MORE ROOM", theme.title()),
            Line::styled(
                format!("Resize to at least {MIN_WIDTH} columns x {MIN_HEIGHT} rows."),
                theme.muted(),
            ),
            Line::from(vec![
                Span::styled("q", theme.key()),
                Span::styled(" or Ctrl-C to return to the shell", theme.muted()),
            ]),
        ])
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true });
        frame.render_widget(message, frame.area());
    }

    fn render_header(&self, frame: &mut ratatui::Frame<'_>, area: Rect, theme: Theme) {
        let space = self
            .active_space
            .as_ref()
            .map(|space| space.key.as_str())
            .or(self.requested_space.as_deref())
            .unwrap_or("NO SPACE");
        let location = if area.width >= 90 {
            format!(
                "{}  /  {}  /  {}",
                self.profile,
                self.site,
                safe_text(space)
            )
        } else {
            format!(
                "{}  /  {}  /  {}",
                self.profile,
                compact_site(&self.site),
                safe_text(space)
            )
        };
        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(" CONFLUENCE ", theme.brand()),
                Span::styled(" PROOF DESK ", theme.title()),
                Span::styled(format!(" {} ", self.mode.label()), theme.mode()),
            ]),
            Line::styled(location, theme.muted()),
        ])
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(theme.rule()),
        );
        frame.render_widget(header, area);
    }

    fn render_body(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect, theme: Theme) {
        if self.mode == Mode::Browse && self.workspace_loading {
            let loading = Paragraph::new(vec![
                Line::styled("SETTING THE GALLEY", theme.title()),
                Line::styled(
                    "Reading spaces and arranging the page folio...",
                    theme.muted(),
                ),
            ])
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .title(" LIVE CONFLUENCE ")
                    .borders(Borders::ALL)
                    .border_style(theme.active_rule()),
            );
            frame.render_widget(loading, inset(area, 2, 2));
            return;
        }

        if area.width >= WIDE_WIDTH {
            let columns = Layout::horizontal([
                Constraint::Percentage(23),
                Constraint::Percentage(54),
                Constraint::Percentage(23),
            ])
            .split(area);
            self.render_index(frame, columns[0], theme);
            self.render_galley(frame, columns[1], theme, false);
            self.render_margin(frame, columns[2], theme);
        } else if area.width >= MEDIUM_WIDTH {
            let columns =
                Layout::horizontal([Constraint::Percentage(34), Constraint::Percentage(66)])
                    .split(area);
            self.render_index(frame, columns[0], theme);
            self.render_galley(frame, columns[1], theme, false);
        } else {
            self.render_index(frame, area, theme);
        }
    }

    fn render_index(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect, theme: Theme) {
        match self.mode {
            Mode::Browse => self.render_contents(frame, area, theme),
            Mode::Review => self.render_review_index(frame, area, theme),
        }
    }

    fn render_contents(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect, theme: Theme) {
        if !self.workspace_loaded && !self.workspace_loading {
            let body = Paragraph::new(vec![
                Line::styled("CONFLUENCE IS NOT ON THE DESK", theme.title()),
                Line::styled(&self.notice.text, theme.body()),
                Line::raw(""),
                Line::from(vec![
                    Span::styled("r", theme.key()),
                    Span::styled(" retry   ", theme.muted()),
                    Span::styled("s", theme.key()),
                    Span::styled(" choose space", theme.muted()),
                ]),
            ])
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .title(" CONTENTS FOLIO ")
                    .borders(Borders::ALL)
                    .border_style(theme.error()),
            );
            frame.render_widget(body, area);
            return;
        }
        if self.pages.is_empty() {
            let empty = Paragraph::new(vec![
                Line::styled("NO PAGES IN THIS FOLIO", theme.title()),
                Line::styled("Choose another space or reload this one.", theme.muted()),
                Line::from(vec![
                    Span::styled("s", theme.key()),
                    Span::styled(" choose space   ", theme.muted()),
                    Span::styled("r", theme.key()),
                    Span::styled(" reload", theme.muted()),
                ]),
            ])
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .title(" CONTENTS FOLIO ")
                    .borders(Borders::ALL)
                    .border_style(theme.rule()),
            );
            frame.render_widget(empty, area);
            return;
        }
        let items = self.pages.iter().enumerate().map(|(index, row)| {
            let indent = "  ".repeat(row.depth.min(8));
            let status = if row.item.status == "current" || row.item.status.is_empty() {
                ""
            } else {
                " *"
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:03} ", index + 1), theme.folio()),
                Span::raw(indent),
                Span::styled(safe_text(&row.item.title), theme.body()),
                Span::styled(status, theme.warning()),
            ]))
        });
        let title = self.active_space.as_ref().map_or_else(
            || " CONTENTS FOLIO ".to_string(),
            |space| format!(" CONTENTS  {}  /  {} ", space.key, self.pages.len()),
        );
        let list = List::new(items)
            .highlight_style(theme.selection())
            .highlight_symbol("> ")
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(theme.rule()),
            );
        frame.render_stateful_widget(list, area, &mut self.page_state);
    }

    fn render_review_index(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect, theme: Theme) {
        if let Some(error) = &self.review_error {
            let body = Paragraph::new(vec![
                Line::styled("LOCAL PROOF COULD NOT BE SET", theme.error()),
                Line::styled(error, theme.body()),
                Line::raw(""),
                Line::from(vec![
                    Span::styled("p", theme.key()),
                    Span::styled(" choose directory   ", theme.muted()),
                    Span::styled("r", theme.key()),
                    Span::styled(" retry", theme.muted()),
                ]),
            ])
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .title(" SYNC PROOF ")
                    .borders(Borders::ALL)
                    .border_style(theme.error()),
            );
            frame.render_widget(body, area);
            return;
        }
        let items = self.review_items.iter().enumerate().map(|(index, item)| {
            let (mark, style) = action_mark(&item.action, theme);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:03} ", index + 1), theme.folio()),
                Span::styled(format!("{mark} "), style),
                Span::styled(safe_text(&item.title), theme.body()),
            ]))
        });
        let list = List::new(items)
            .highlight_style(theme.selection())
            .highlight_symbol("> ")
            .block(
                Block::default()
                    .title(format!(
                        " SYNC PROOF  /  {} {} ",
                        self.review_items.len(),
                        if self.review_items.len() == 1 {
                            "MARK"
                        } else {
                            "MARKS"
                        }
                    ))
                    .borders(Borders::ALL)
                    .border_style(theme.rule()),
            );
        frame.render_stateful_widget(list, area, &mut self.review_state);
    }

    fn render_galley(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: Rect,
        theme: Theme,
        expanded: bool,
    ) {
        match self.mode {
            Mode::Browse => self.render_page_galley(frame, area, theme, expanded),
            Mode::Review => self.render_diff_galley(frame, area, theme, expanded),
        }
    }

    fn render_page_galley(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: Rect,
        theme: Theme,
        expanded: bool,
    ) {
        let Some(row) = self.selected_page() else {
            let empty = Paragraph::new("Select a page from the contents folio.")
                .style(theme.muted())
                .block(galley_block(" PAGE GALLEY ", theme, expanded));
            frame.render_widget(empty, area);
            return;
        };
        let loaded = self.page_proof.matches(&row.item.id);
        let title = format!(
            " GALLEY  {:03}  /  {}{} ",
            self.page_state.selected().unwrap_or(0) + 1,
            truncate_text(
                &row.item.title,
                usize::from(area.width.saturating_sub(24)).max(12)
            ),
            if expanded { "  /  ESC BACK" } else { "" }
        );
        let lines = if self.page_loading {
            vec![
                Line::styled("UNFOLDING PAGE PROOF", theme.title()),
                Line::styled(
                    "Reading the complete page and its margin evidence...",
                    theme.muted(),
                ),
            ]
        } else if loaded {
            markdown_lines(&self.page_proof.markdown, theme)
        } else {
            vec![
                Line::styled(safe_text(&row.item.title), theme.detail_title()),
                Line::raw(""),
                Line::styled(
                    "This folio entry is indexed but not unfolded.",
                    theme.body(),
                ),
                Line::from(vec![
                    Span::styled("enter", theme.key()),
                    Span::styled(" read the complete page proof", theme.muted()),
                ]),
            ]
        };
        let galley = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.body_scroll, 0))
            .block(galley_block(&title, theme, expanded));
        frame.render_widget(galley, area);
    }

    fn render_diff_galley(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: Rect,
        theme: Theme,
        expanded: bool,
    ) {
        let Some(item) = self.selected_review() else {
            let empty = Paragraph::new("Choose a local sync mark to inspect its proof.")
                .style(theme.muted())
                .block(galley_block(" CHANGE GALLEY ", theme, expanded));
            frame.render_widget(empty, area);
            return;
        };
        let title = format!(
            " CHANGE GALLEY  /  {}{} ",
            truncate_text(
                &item.title,
                usize::from(area.width.saturating_sub(26)).max(12)
            ),
            if expanded { "  /  ESC BACK" } else { "" }
        );
        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    action_label(&item.action),
                    action_style(&item.action, theme),
                ),
                Span::styled(format!("  {}", safe_text(&item.details)), theme.body()),
            ]),
            Line::styled(safe_text(&item.path.display().to_string()), theme.muted()),
            Line::raw(""),
        ];
        if let Some(diff) = item.diff.as_deref() {
            lines.extend(diff_lines(diff, theme));
        } else if item.action == PlanActionKind::Noop {
            lines.push(Line::styled(
                "NO CORRECTIONS ON THIS PROOF",
                theme.success(),
            ));
            lines.push(Line::styled(
                "The local Markdown body and tracked sync state agree.",
                theme.body(),
            ));
        } else {
            lines.push(Line::styled("NO BODY DIFF FOR THIS MARK", theme.title()));
            lines.push(Line::styled(
                "This action concerns hierarchy, metadata, or an attachment.",
                theme.body(),
            ));
        }
        let galley = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.body_scroll, 0))
            .block(galley_block(&title, theme, expanded));
        frame.render_widget(galley, area);
    }

    fn render_margin(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect, theme: Theme) {
        match self.mode {
            Mode::Browse => self.render_page_margin(frame, area, theme, false),
            Mode::Review => self.render_review_margin(frame, area, theme, false),
        }
    }

    fn render_page_margin(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        area: Rect,
        theme: Theme,
        expanded: bool,
    ) {
        let tabs = Line::from(vec![
            margin_tab_span(MarginTab::Metadata, self.margin_tab, theme),
            margin_tab_span(MarginTab::Attachments, self.margin_tab, theme),
            margin_tab_span(MarginTab::Comments, self.margin_tab, theme),
            margin_tab_span(MarginTab::Properties, self.margin_tab, theme),
        ]);
        let mut lines = Vec::new();
        let selected = self.selected_page().map(|row| &row.item);
        let proof_matches = selected.is_some_and(|item| self.page_proof.matches(&item.id));
        let detailed = selected
            .filter(|item| self.page_proof.matches(&item.id))
            .and(self.page_proof.item.as_ref());
        match self.margin_tab {
            MarginTab::Metadata => {
                if let Some(item) = detailed.or(selected) {
                    lines.extend(metadata_lines(item, theme));
                } else {
                    lines.push(Line::styled("No page selected.", theme.muted()));
                }
            }
            MarginTab::Attachments => {
                if proof_matches {
                    lines.extend(resource_lines(
                        &self.page_proof.attachments,
                        "FILES",
                        |item| {
                            format!(
                                "{}  {}",
                                item.title,
                                item.file_size
                                    .map(human_size)
                                    .unwrap_or_else(|| "size unknown".into())
                            )
                        },
                        theme,
                    ));
                } else {
                    lines.extend(resource_lines::<AttachmentInfo>(
                        &ResourceState::Idle,
                        "FILES",
                        |_| String::new(),
                        theme,
                    ));
                }
            }
            MarginTab::Comments => {
                if proof_matches {
                    lines.extend(resource_lines(
                        &self.page_proof.comments,
                        "NOTES",
                        |item| {
                            let author = item.author.as_deref().unwrap_or("unknown author");
                            let body = storage_to_markdown(&item.body_storage);
                            format!(
                                "{}  {}",
                                author,
                                truncate_text(&body.replace('\n', " "), 96)
                            )
                        },
                        theme,
                    ));
                } else {
                    lines.extend(resource_lines::<CommentInfo>(
                        &ResourceState::Idle,
                        "NOTES",
                        |_| String::new(),
                        theme,
                    ));
                }
            }
            MarginTab::Properties => {
                if proof_matches {
                    lines.extend(resource_lines(
                        &self.page_proof.properties,
                        "PROPERTIES",
                        |item| {
                            if is_sensitive_key(&item.key) {
                                format!("{}  [REDACTED]", item.key)
                            } else {
                                format!(
                                    "{}  {}",
                                    item.key,
                                    truncate_text(&redact_json_value(&item.value).to_string(), 72)
                                )
                            }
                        },
                        theme,
                    ));
                } else {
                    lines.extend(resource_lines::<ContentProperty>(
                        &ResourceState::Idle,
                        "PROPERTIES",
                        |_| String::new(),
                        theme,
                    ));
                }
            }
        }
        let title = if expanded {
            " PROOF MARGIN  /  1-4 VIEWS  /  ESC BACK "
        } else {
            " PROOF MARGIN  /  1-4 VIEWS "
        };
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(if expanded {
                theme.active_rule()
            } else {
                theme.rule()
            })
            .style(theme.canvas());
        if expanded {
            self.overlay_max_scroll = render_scrollable_overlay(
                frame,
                area,
                block,
                vec![tabs],
                lines,
                self.overlay_scroll,
                theme,
            );
            self.overlay_scroll = self.overlay_scroll.min(self.overlay_max_scroll);
        } else {
            lines.insert(0, Line::raw(""));
            lines.insert(0, tabs);
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(block),
                area,
            );
        }
    }

    fn render_review_margin(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        area: Rect,
        theme: Theme,
        expanded: bool,
    ) {
        let counts = review_counts(&self.review_items);
        let mut lines = vec![
            Line::styled("LOCAL PLAN ONLY", theme.success()),
            Line::styled(
                "No remote calls or writes occur in this view.",
                theme.body(),
            ),
            Line::raw(""),
            Line::from(vec![
                Span::styled("+ CREATE  ", theme.success()),
                Span::styled(
                    counts.get("create").copied().unwrap_or(0).to_string(),
                    theme.body(),
                ),
            ]),
            Line::from(vec![
                Span::styled("+ UPLOAD  ", theme.success()),
                Span::styled(
                    counts.get("upload").copied().unwrap_or(0).to_string(),
                    theme.body(),
                ),
            ]),
            Line::from(vec![
                Span::styled("~ UPDATE  ", theme.warning()),
                Span::styled(
                    counts.get("update").copied().unwrap_or(0).to_string(),
                    theme.body(),
                ),
            ]),
            Line::from(vec![
                Span::styled("> MOVE    ", theme.accent()),
                Span::styled(
                    counts.get("move").copied().unwrap_or(0).to_string(),
                    theme.body(),
                ),
            ]),
            if self.delete_remote {
                Line::from(vec![
                    Span::styled("- DELETE  ", theme.error()),
                    Span::styled(
                        counts.get("delete").copied().unwrap_or(0).to_string(),
                        theme.body(),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled("- DELETE  ", theme.folio()),
                    Span::styled("OFF", theme.body()),
                ])
            },
            Line::from(vec![
                Span::styled("= CLEAN   ", theme.muted()),
                Span::styled(
                    counts.get("clean").copied().unwrap_or(0).to_string(),
                    theme.body(),
                ),
            ]),
        ];
        if !self.delete_remote {
            lines.push(Line::styled(
                "Use --delete-remote to plan removals.",
                theme.muted(),
            ));
        }
        lines.push(Line::raw(""));
        if let Some(item) = self.selected_review() {
            lines.push(Line::styled("SELECTED MARK", theme.folio()));
            lines.push(Line::styled(
                action_label(&item.action),
                action_style(&item.action, theme),
            ));
            lines.push(Line::styled(safe_text(&item.details), theme.body()));
            if let Some(id) = item.content_id.as_deref() {
                lines.push(Line::styled(
                    format!("content  {}", safe_text(id)),
                    theme.muted(),
                ));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("REMOTE DRIFT", theme.folio()));
        lines.push(Line::styled(
            "Not checked here. `confluence apply` verifies remote versions and refuses drift.",
            theme.muted(),
        ));
        let title = if expanded {
            " REVIEW MARKS  /  ESC BACK "
        } else {
            " REVIEW MARKS "
        };
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(if expanded {
                theme.active_rule()
            } else {
                theme.rule()
            })
            .style(theme.canvas());
        if expanded {
            self.overlay_max_scroll = render_scrollable_overlay(
                frame,
                area,
                block,
                Vec::new(),
                lines,
                self.overlay_scroll,
                theme,
            );
            self.overlay_scroll = self.overlay_scroll.min(self.overlay_max_scroll);
        } else {
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(block),
                area,
            );
        }
    }

    fn render_proof_overlay(&self, frame: &mut ratatui::Frame<'_>, theme: Theme) {
        let width = frame.area().width.saturating_sub(4).min(112);
        let height = frame.area().height.saturating_sub(4).min(40);
        let area = centered_rect(width, height, frame.area());
        frame.render_widget(Clear, area);
        self.render_galley(frame, area, theme, true);
    }

    fn render_margin_overlay(&mut self, frame: &mut ratatui::Frame<'_>, theme: Theme) {
        let width = frame.area().width.saturating_sub(4).min(82);
        let height = frame.area().height.saturating_sub(4).min(34);
        let area = centered_rect(width, height, frame.area());
        frame.render_widget(Clear, area);
        match self.mode {
            Mode::Browse => self.render_page_margin(frame, area, theme, true),
            Mode::Review => self.render_review_margin(frame, area, theme, true),
        }
    }

    fn render_help(&mut self, frame: &mut ratatui::Frame<'_>, theme: Theme) {
        let area = centered_rect(74, 25, frame.area());
        frame.render_widget(Clear, area);
        let mut rows = vec![
            ("up/k, down/j", "Move through the current folio"),
            ("tab", "Switch between Browse and Review"),
            ("p", "Choose a pulled Markdown directory"),
        ];
        match self.mode {
            Mode::Browse => rows.splice(
                1..1,
                [
                    ("enter/right", "Unfold the selected page proof"),
                    ("s", "Choose a Confluence space"),
                ],
            ),
            Mode::Review => rows.splice(
                1..1,
                [
                    ("enter/right", "Inspect the selected local diff"),
                    ("s", "Choose a space and return to Browse"),
                ],
            ),
        };
        if self.mode == Mode::Browse {
            rows.extend([
                (
                    "1 / 2 / 3 / 4",
                    "Show metadata, files, notes, or properties",
                ),
                ("o", "Open the selected page in Confluence"),
            ]);
        }
        rows.extend([
            ("m", "Open the current proof margin"),
            (
                "r",
                if self.mode == Mode::Browse {
                    "Reload the current workspace"
                } else {
                    "Rebuild the local sync plan"
                },
            ),
            ("PgUp / PgDn", "Scroll the visible galley"),
            ("g / G", "Jump to the first or last mark"),
            ("esc", "Fold the current proof or margin"),
            ("q / Ctrl-C", "Return to the shell"),
        ]);
        let mut lines = vec![
            Line::styled(
                "The Proof Desk is keyboard-first and read-only.",
                theme.muted(),
            ),
            Line::raw(""),
        ];
        for (key, description) in rows {
            lines.push(Line::from(vec![
                Span::styled(format!("{key:<18}"), theme.key()),
                Span::styled(description, theme.body()),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Review never contacts or changes Confluence; apply remains a separate CLI command.",
            theme.success(),
        ));
        let block = Block::default()
            .title(" PROOF DESK MAP  /  ? OR ESC TO CLOSE ")
            .borders(Borders::ALL)
            .border_style(theme.active_rule())
            .style(theme.canvas());
        self.overlay_max_scroll = render_scrollable_overlay(
            frame,
            area,
            block,
            Vec::new(),
            lines,
            self.overlay_scroll,
            theme,
        );
        self.overlay_scroll = self.overlay_scroll.min(self.overlay_max_scroll);
    }

    fn render_footer(&self, frame: &mut ratatui::Frame<'_>, area: Rect, theme: Theme) {
        let notice_style = match self.notice.kind {
            NoticeKind::Quiet => theme.muted(),
            NoticeKind::Success => theme.success(),
            NoticeKind::Warning => theme.warning(),
            NoticeKind::Error => theme.error(),
        };
        let notice = truncate_text(&self.notice.text, usize::from(area.width.saturating_sub(2)));
        let hints = match &self.overlay {
            Overlay::Proof => {
                let mut spans = vec![
                    Span::styled("j/k", theme.key()),
                    Span::styled(" scroll  ", theme.muted()),
                    Span::styled("PgUp/PgDn", theme.key()),
                    Span::styled(" page  ", theme.muted()),
                    Span::styled("m", theme.key()),
                    Span::styled(" margin  ", theme.muted()),
                ];
                if self.mode == Mode::Browse && area.width >= 72 {
                    spans.extend([
                        Span::styled("o", theme.key()),
                        Span::styled(" open  ", theme.muted()),
                        Span::styled("r", theme.key()),
                        Span::styled(" reload  ", theme.muted()),
                    ]);
                }
                spans.extend([
                    Span::styled("esc", theme.key()),
                    Span::styled(" back", theme.muted()),
                ]);
                Line::from(spans)
            }
            Overlay::Margin if self.mode == Mode::Browse => Line::from(vec![
                Span::styled("j/k", theme.key()),
                Span::styled(" scroll  ", theme.muted()),
                Span::styled("1-4", theme.key()),
                Span::styled(" margin view  ", theme.muted()),
                Span::styled("m/esc", theme.key()),
                Span::styled(" back", theme.muted()),
            ]),
            Overlay::Margin => Line::from(vec![
                Span::styled("j/k", theme.key()),
                Span::styled(" scroll  ", theme.muted()),
                Span::styled("m/esc", theme.key()),
                Span::styled(" back to local plan", theme.muted()),
            ]),
            Overlay::Help => Line::from(vec![
                Span::styled("j/k", theme.key()),
                Span::styled(" scroll  ", theme.muted()),
                Span::styled("?/esc/q", theme.key()),
                Span::styled(" close map", theme.muted()),
            ]),
            Overlay::SpaceInput(_) | Overlay::PathInput(_) => Line::from(vec![
                Span::styled("enter", theme.key()),
                Span::styled(" open  ", theme.muted()),
                Span::styled("esc", theme.key()),
                Span::styled(" cancel", theme.muted()),
            ]),
            Overlay::None if self.mode == Mode::Review && area.width >= 80 => Line::from(vec![
                Span::styled("up/down", theme.key()),
                Span::styled(" move  ", theme.muted()),
                Span::styled("enter", theme.key()),
                Span::styled(" inspect diff  ", theme.muted()),
                Span::styled("tab", theme.key()),
                Span::styled(" browse  ", theme.muted()),
                Span::styled("p", theme.key()),
                Span::styled(" path  ", theme.muted()),
                Span::styled("r", theme.key()),
                Span::styled(" replan  ", theme.muted()),
                Span::styled("?", theme.key()),
                Span::styled(" map  ", theme.muted()),
                Span::styled("q", theme.key()),
                Span::styled(" quit", theme.muted()),
            ]),
            Overlay::None if area.width >= 92 => Line::from(vec![
                Span::styled("up/down", theme.key()),
                Span::styled(" move  ", theme.muted()),
                Span::styled("enter", theme.key()),
                Span::styled(" unfold  ", theme.muted()),
                Span::styled("tab", theme.key()),
                Span::styled(" browse/review  ", theme.muted()),
                Span::styled("s", theme.key()),
                Span::styled(" space  ", theme.muted()),
                Span::styled("p", theme.key()),
                Span::styled(" path  ", theme.muted()),
                Span::styled("m", theme.key()),
                Span::styled(" margin  ", theme.muted()),
                Span::styled("?", theme.key()),
                Span::styled(" map  ", theme.muted()),
                Span::styled("q", theme.key()),
                Span::styled(" quit", theme.muted()),
            ]),
            Overlay::None => Line::from(vec![
                Span::styled("up/down", theme.key()),
                Span::styled(" move  ", theme.muted()),
                Span::styled("enter", theme.key()),
                Span::styled(" unfold  ", theme.muted()),
                Span::styled("tab", theme.key()),
                Span::styled(" mode  ", theme.muted()),
                Span::styled("?", theme.key()),
                Span::styled(" map  ", theme.muted()),
                Span::styled("q", theme.key()),
                Span::styled(" quit", theme.muted()),
            ]),
        };
        let lines = if area.height >= 3 {
            vec![Line::styled(format!(" {notice}"), notice_style), hints]
        } else {
            vec![hints]
        };
        let footer = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(theme.rule()),
        );
        frame.render_widget(footer, area);
    }
}

fn launch_review_plan(
    path: PathBuf,
    delete_remote: bool,
) -> Result<oneshot::Receiver<Result<SyncPlan>>> {
    launch_review_plan_with(move || sync::plan_path(&path, false, delete_remote, true))
}

fn launch_review_plan_with<F>(job: F) -> Result<oneshot::Receiver<Result<SyncPlan>>>
where
    F: FnOnce() -> Result<SyncPlan> + Send + 'static,
{
    let (sender, receiver) = oneshot::channel();
    thread::Builder::new()
        .name("confluence-review-plan".into())
        .spawn(move || {
            let _ = sender.send(job());
        })
        .context("failed to start the local review planner")?;
    Ok(receiver)
}

pub async fn run(provider: &dyn ConfluenceProvider, options: TuiOptions) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(crate::output::typed_error_with_hint(
            crate::output::ErrorKind::TtyRequired,
            "the TUI requires an interactive terminal on stdin and stdout",
            "run `confluence --help` for scriptable commands, or launch `confluence tui` directly from a terminal",
        ));
    }

    enable_raw_mode().context("failed to enable terminal raw mode")?;
    let _restore = RestoreTerminal;
    execute!(io::stdout(), EnterAlternateScreen).context("failed to enter the alternate screen")?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("failed to initialize the terminal")?;
    terminal.clear().context("failed to clear the terminal")?;
    let events = EventReader::start();

    let mut app = App::new(options);
    if app.mode == Mode::Browse {
        app.workspace_loading = true;
        app.notice = Notice::quiet("Indexing visible spaces...");
    }
    terminal
        .draw(|frame| app.render(frame))
        .context("failed to draw the proof desk")?;
    match app.mode {
        Mode::Browse => {
            if complete_load_or_quit(app.load_workspace(provider), &events).await? {
                return Ok(());
            }
        }
        Mode::Review => {
            if complete_load_or_quit(app.load_review(), &events).await? {
                return Ok(());
            }
        }
    }

    let mut needs_draw = true;
    loop {
        if needs_draw {
            terminal
                .draw(|frame| app.render(frame))
                .context("failed to draw the proof desk")?;
            needs_draw = false;
        }
        let key = match events.recv_timeout(Duration::from_millis(500))? {
            None => continue,
            Some(Event::Key(key)) if key.kind == KeyEventKind::Press => key,
            Some(Event::Resize(_, _)) => {
                needs_draw = true;
                continue;
            }
            _ => continue,
        };
        needs_draw = true;
        match app.handle_key(key) {
            Action::None => {}
            Action::Quit => return Ok(()),
            Action::LoadWorkspace => {
                app.workspace_loading = true;
                app.notice = Notice::quiet("Indexing visible spaces...");
                terminal
                    .draw(|frame| app.render(frame))
                    .context("failed to draw the loading state")?;
                if complete_load_or_quit(app.load_workspace(provider), &events).await? {
                    return Ok(());
                }
            }
            Action::LoadPage => {
                app.page_loading = true;
                app.notice = Notice::quiet("Unfolding the selected page proof...");
                terminal
                    .draw(|frame| app.render(frame))
                    .context("failed to draw the page loading state")?;
                if complete_load_or_quit(app.load_selected_page(provider), &events).await? {
                    return Ok(());
                }
            }
            Action::Replan => {
                app.notice = Notice::quiet("Reading the local sync proof...");
                terminal
                    .draw(|frame| app.render(frame))
                    .context("failed to draw the review loading state")?;
                if complete_load_or_quit(app.load_review(), &events).await? {
                    return Ok(());
                }
            }
            Action::Open => app.open_selected(),
        }
    }
}

struct EventReader {
    receiver: mpsc::Receiver<std::result::Result<Event, String>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl EventReader {
    fn start() -> Self {
        let (sender, receiver) = mpsc::sync_channel(32);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => match event::read() {
                        Ok(event) => {
                            if sender.send(Ok(event)).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(Err(error.to_string()));
                            break;
                        }
                    },
                    Ok(false) => {}
                    Err(error) => {
                        let _ = sender.send(Err(error.to_string()));
                        break;
                    }
                }
            }
        });
        Self {
            receiver,
            stop,
            thread: Some(thread),
        }
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<Option<Event>> {
        match self.receiver.recv_timeout(timeout) {
            Ok(Ok(event)) => Ok(Some(event)),
            Ok(Err(error)) => Err(anyhow!(error).context("failed to read a terminal event")),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow!("terminal event reader stopped unexpectedly"))
            }
        }
    }
}

impl Drop for EventReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn complete_load_or_quit<F>(future: F, events: &EventReader) -> Result<bool>
where
    F: Future<Output = ()>,
{
    tokio::pin!(future);
    loop {
        tokio::select! {
            () = &mut future => return Ok(false),
            () = tokio::time::sleep(Duration::from_millis(25)) => {
                loop {
                    match events.receiver.try_recv() {
                        Ok(Ok(event)) if is_quit_event(&event) => return Ok(true),
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => {
                            return Err(anyhow!(error).context("failed to read a terminal event"));
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            return Err(anyhow!("terminal event reader stopped unexpectedly"));
                        }
                    }
                }
            }
        }
    }
}

fn is_quit_event(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    key.kind == KeyEventKind::Press
        && (key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)))
}

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
}

#[derive(Clone, Copy)]
struct Theme {
    color: bool,
}

impl Theme {
    fn new(color: bool) -> Self {
        Self { color }
    }

    fn canvas(self) -> Style {
        self.style(Color::Rgb(24, 32, 29), Color::Rgb(241, 243, 239))
    }

    fn body(self) -> Style {
        self.style(Color::Rgb(24, 32, 29), Color::Rgb(241, 243, 239))
    }

    fn muted(self) -> Style {
        self.style(Color::Rgb(80, 91, 86), Color::Rgb(241, 243, 239))
    }

    fn brand(self) -> Style {
        self.style(Color::Rgb(241, 243, 239), Color::Rgb(46, 79, 155))
            .add_modifier(Modifier::BOLD)
    }

    fn mode(self) -> Style {
        self.style(Color::Rgb(241, 243, 239), Color::Rgb(24, 32, 29))
            .add_modifier(Modifier::BOLD)
    }

    fn title(self) -> Style {
        self.style(Color::Rgb(24, 32, 29), Color::Rgb(241, 243, 239))
            .add_modifier(Modifier::BOLD)
    }

    fn detail_title(self) -> Style {
        self.style(Color::Rgb(46, 79, 155), Color::Rgb(241, 243, 239))
            .add_modifier(Modifier::BOLD)
    }

    fn folio(self) -> Style {
        self.style(Color::Rgb(80, 91, 86), Color::Rgb(241, 243, 239))
            .add_modifier(Modifier::DIM)
    }

    fn accent(self) -> Style {
        self.style(Color::Rgb(46, 79, 155), Color::Rgb(241, 243, 239))
            .add_modifier(Modifier::BOLD)
    }

    fn key(self) -> Style {
        self.style(Color::Rgb(46, 79, 155), Color::Rgb(241, 243, 239))
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    }

    fn rule(self) -> Style {
        self.style(Color::Rgb(150, 158, 152), Color::Rgb(241, 243, 239))
    }

    fn active_rule(self) -> Style {
        self.style(Color::Rgb(46, 79, 155), Color::Rgb(241, 243, 239))
    }

    fn selection(self) -> Style {
        self.style(Color::Rgb(241, 243, 239), Color::Rgb(46, 79, 155))
            .add_modifier(Modifier::BOLD)
    }

    fn success(self) -> Style {
        self.style(Color::Rgb(31, 104, 72), Color::Rgb(241, 243, 239))
            .add_modifier(Modifier::BOLD)
    }

    fn warning(self) -> Style {
        self.style(Color::Rgb(133, 83, 0), Color::Rgb(241, 243, 239))
            .add_modifier(Modifier::BOLD)
    }

    fn error(self) -> Style {
        self.style(Color::Rgb(169, 42, 67), Color::Rgb(241, 243, 239))
            .add_modifier(Modifier::BOLD)
    }

    fn style(self, foreground: Color, background: Color) -> Style {
        if self.color {
            Style::default().fg(foreground).bg(background)
        } else {
            Style::default()
        }
    }
}

fn galley_block(title: &str, theme: Theme, active: bool) -> Block<'_> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(if active {
            theme.active_rule()
        } else {
            theme.rule()
        })
        .style(theme.canvas())
}

fn render_scrollable_overlay(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    block: Block<'_>,
    pinned_lines: Vec<Line<'static>>,
    lines: Vec<Line<'static>>,
    requested_scroll: u16,
    theme: Theme,
) -> u16 {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return 0;
    }

    let pinned_height = pinned_lines
        .len()
        .min(usize::from(inner.height.saturating_sub(1))) as u16;
    if pinned_height > 0 {
        frame.render_widget(
            Paragraph::new(pinned_lines).style(theme.canvas()),
            Rect::new(inner.x, inner.y, inner.width, pinned_height),
        );
    }
    let content_height = inner.height.saturating_sub(pinned_height).saturating_sub(1);
    let content_area = Rect::new(
        inner.x,
        inner.y.saturating_add(pinned_height),
        inner.width,
        content_height,
    );
    let total_rows = wrapped_line_count(&lines, inner.width).max(1);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let max_scroll = total_rows
        .saturating_sub(usize::from(content_height))
        .min(usize::from(u16::MAX)) as u16;
    let scroll = requested_scroll.min(max_scroll);
    frame.render_widget(paragraph.scroll((scroll, 0)), content_area);

    let visible_start = usize::from(scroll) + 1;
    let visible_end = (usize::from(scroll) + usize::from(content_height)).min(total_rows);
    let above = if scroll > 0 { "MORE ABOVE" } else { "TOP" };
    let below = if scroll < max_scroll {
        "MORE BELOW"
    } else {
        "END"
    };
    let position = format!("{above}  /  {visible_start}-{visible_end} OF {total_rows}  /  {below}");
    frame.render_widget(
        Paragraph::new(truncate_text(&position, usize::from(inner.width)))
            .alignment(Alignment::Right)
            .style(theme.folio()),
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
    );
    max_scroll
}

fn wrapped_line_count(lines: &[Line<'_>], width: u16) -> usize {
    let width = usize::from(width).max(1);
    lines
        .iter()
        .map(|line| wrapped_text_line_count(&line.to_string(), width))
        .sum()
}

fn wrapped_text_line_count(value: &str, max_width: usize) -> usize {
    let mut wrapped_lines = 0;
    let mut line_width = 0usize;
    let mut word_width = 0usize;
    let mut whitespace_width = 0usize;
    let mut line_has_content = false;
    let mut word_has_content = false;
    let mut whitespace_widths = Vec::new();
    let mut previous_was_non_whitespace = false;

    for character in value.chars() {
        let is_whitespace = character.is_whitespace();
        let symbol_width = character_display_width(character);
        if symbol_width > max_width {
            continue;
        }

        let word_found = previous_was_non_whitespace && is_whitespace;
        let untrimmed_overflow =
            !line_has_content && word_width + whitespace_width + symbol_width > max_width;
        if word_found || untrimmed_overflow {
            line_has_content |= !whitespace_widths.is_empty();
            line_width += whitespace_width;
            line_has_content |= word_has_content;
            line_width += word_width;
            whitespace_widths.clear();
            whitespace_width = 0;
            word_width = 0;
            word_has_content = false;
        }

        let line_full = line_width >= max_width;
        let pending_word_overflow =
            symbol_width > 0 && line_width + whitespace_width + word_width >= max_width;
        if line_full || pending_word_overflow {
            wrapped_lines += 1;
            let mut remaining_width = max_width.saturating_sub(line_width);
            line_width = 0;
            line_has_content = false;
            while whitespace_widths
                .first()
                .is_some_and(|width| *width <= remaining_width)
            {
                let width = whitespace_widths.remove(0);
                whitespace_width -= width;
                remaining_width -= width;
            }
            if is_whitespace && whitespace_widths.is_empty() {
                previous_was_non_whitespace = false;
                continue;
            }
        }

        if is_whitespace {
            whitespace_width += symbol_width;
            whitespace_widths.push(symbol_width);
        } else {
            word_width += symbol_width;
            word_has_content = true;
        }
        previous_was_non_whitespace = !is_whitespace;
    }

    line_has_content |= !whitespace_widths.is_empty();
    line_has_content |= word_has_content;
    if line_has_content {
        wrapped_lines += 1;
    }
    wrapped_lines.max(1)
}

fn margin_tab_span(tab: MarginTab, selected: MarginTab, theme: Theme) -> Span<'static> {
    let index = match tab {
        MarginTab::Metadata => 1,
        MarginTab::Attachments => 2,
        MarginTab::Comments => 3,
        MarginTab::Properties => 4,
    };
    let marker = if tab == selected { ">" } else { "" };
    let label = format!("{marker}{index} {} ", tab.label());
    Span::styled(
        label,
        if tab == selected {
            theme.selection()
        } else {
            theme.muted()
        },
    )
}

fn metadata_lines(item: &ContentItem, theme: Theme) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::styled(safe_text(&item.title), theme.detail_title()),
        Line::raw(""),
        field_line("TYPE", item.kind.to_string(), theme),
        field_line("STATUS", safe_text(&item.status), theme),
        field_line("CONTENT", safe_text(&item.id), theme),
        field_line(
            "SPACE",
            item.space_key
                .as_deref()
                .map(safe_text)
                .unwrap_or_else(|| "-".into()),
            theme,
        ),
        field_line(
            "PARENT",
            item.parent_id
                .as_deref()
                .map(safe_text)
                .unwrap_or_else(|| "root".into()),
            theme,
        ),
        field_line(
            "VERSION",
            item.version
                .map_or_else(|| "-".into(), |version| version.to_string()),
            theme,
        ),
    ];
    if !item.labels.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled("LABELS", theme.folio()));
        for label in &item.labels {
            lines.push(Line::styled(safe_text(label), theme.body()));
        }
    }
    if let Some(updated) = item.updated_at {
        lines.push(Line::raw(""));
        lines.push(field_line(
            "UPDATED",
            updated.format("%Y-%m-%d %H:%M UTC").to_string(),
            theme,
        ));
    }
    lines
}

fn field_line(label: &str, value: String, theme: Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<9}"), theme.folio()),
        Span::styled(value, theme.body()),
    ])
}

fn resource_lines<T>(
    state: &ResourceState<T>,
    label: &str,
    render: impl Fn(&T) -> String,
    theme: Theme,
) -> Vec<Line<'static>> {
    match state {
        ResourceState::Idle => vec![
            Line::styled(format!("{label} NOT LOADED"), theme.title()),
            Line::styled(
                "Unfold this page to read its margin evidence.",
                theme.muted(),
            ),
        ],
        ResourceState::Failed(error) => vec![
            Line::styled(format!("{label} UNAVAILABLE"), theme.error()),
            Line::styled(safe_text(error), theme.body()),
            Line::styled("The page proof remains available.", theme.muted()),
        ],
        ResourceState::Ready(items) if items.is_empty() => vec![
            Line::styled(format!("NO {label}"), theme.title()),
            Line::styled("Confluence returned no items for this page.", theme.muted()),
        ],
        ResourceState::Ready(items) => {
            let mut lines = vec![
                Line::styled(format!("{} {label}", items.len()), theme.folio()),
                Line::raw(""),
            ];
            for item in items {
                lines.push(Line::styled(safe_text(&render(item)), theme.body()));
                lines.push(Line::raw(""));
            }
            lines
        }
    }
}

fn markdown_lines(markdown: &str, theme: Theme) -> Vec<Line<'static>> {
    if markdown.trim().is_empty() {
        return vec![
            Line::styled("THIS PAGE IS BLANK", theme.title()),
            Line::styled(
                "Confluence returned no readable body content.",
                theme.muted(),
            ),
        ];
    }
    let mut in_code = false;
    let mut extension_depth = 0usize;
    let mut lines = Vec::new();
    for raw in markdown.lines() {
        let line = safe_text(raw);
        let trimmed = line.trim();
        if let Some(extension) = trimmed.strip_prefix("~~~~confluence-") {
            extension_depth = extension_depth.saturating_add(1);
            lines.push(Line::styled(
                format!("[{}]", extension.replace('-', " ").to_uppercase()),
                theme.folio(),
            ));
            continue;
        }
        if trimmed == "~~~~" {
            extension_depth = extension_depth.saturating_sub(1);
            lines.push(Line::raw(""));
            continue;
        }
        if let Some(extension) = trimmed.strip_prefix(":::confluence-") {
            extension_depth = extension_depth.saturating_add(1);
            lines.push(Line::styled(
                format!("[{}]", extension.replace('-', " ").to_uppercase()),
                theme.folio(),
            ));
            continue;
        }
        if trimmed == ":::" {
            extension_depth = extension_depth.saturating_sub(1);
            lines.push(Line::raw(""));
            continue;
        }
        if trimmed == "<p />" {
            continue;
        }
        if trimmed.starts_with("--- cell") {
            lines.push(Line::styled("[CELL]", theme.folio()));
            continue;
        }
        if extension_depth > 0
            && (trimmed == "---"
                || trimmed
                    .split_once(':')
                    .is_some_and(|(key, _)| !key.contains(char::is_whitespace)))
        {
            continue;
        }
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            lines.push(Line::styled(line, theme.folio()));
            continue;
        }
        if in_code {
            lines.push(Line::styled(line, theme.accent()));
            continue;
        }
        let line = strip_inline_html(&strip_placeholder_targets(&line));
        if let Some(title) = line.strip_prefix("# ") {
            lines.push(Line::styled(title.to_string(), theme.detail_title()));
        } else if let Some(title) = line
            .strip_prefix("## ")
            .or_else(|| line.strip_prefix("### "))
        {
            lines.push(Line::styled(title.to_string(), theme.title()));
        } else if line.starts_with('>') || (extension_depth > 0 && line.contains(": ")) {
            lines.push(Line::styled(line, theme.muted()));
        } else {
            lines.push(Line::styled(line, theme.body()));
        }
    }
    lines
}

fn strip_placeholder_targets(value: &str) -> String {
    let mut value = value.to_string();
    for scheme in [
        "confluence-page://",
        "confluence-user://",
        "confluence-status://",
    ] {
        loop {
            let needle = format!("]({scheme}");
            let Some(start) = value.find(&needle) else {
                break;
            };
            let target_start = start + 1;
            let Some(relative_end) = value[target_start..].find(')') else {
                break;
            };
            let target_end = target_start + relative_end;
            value.replace_range(target_start..=target_end, "");
        }
    }
    value
}

fn strip_inline_html(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '<'
            && characters
                .peek()
                .is_some_and(|next| next.is_ascii_alphabetic() || *next == '/')
        {
            let mut closed = false;
            for next in characters.by_ref() {
                if next == '>' {
                    closed = true;
                    break;
                }
            }
            if closed {
                continue;
            }
            output.push('<');
            break;
        }
        output.push(character);
    }
    output.extend(characters);
    output
}

fn diff_lines(diff: &str, theme: Theme) -> Vec<Line<'static>> {
    diff.lines()
        .map(|raw| {
            let line = safe_text(raw);
            if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
                Line::styled(line, theme.folio())
            } else if line.starts_with('+') {
                Line::styled(line, theme.success())
            } else if line.starts_with('-') {
                Line::styled(line, theme.error())
            } else {
                Line::styled(line, theme.body())
            }
        })
        .collect()
}

fn action_mark(action: &PlanActionKind, theme: Theme) -> (&'static str, Style) {
    match action {
        PlanActionKind::CreateContent | PlanActionKind::UploadAttachment => ("+", theme.success()),
        PlanActionKind::UpdateContent
        | PlanActionKind::UpdateLabels
        | PlanActionKind::UpdateProperties => ("~", theme.warning()),
        PlanActionKind::MoveContent => (">", theme.accent()),
        PlanActionKind::DeleteAttachment | PlanActionKind::DeleteRemote => ("-", theme.error()),
        PlanActionKind::Noop => ("=", theme.muted()),
    }
}

fn action_label(action: &PlanActionKind) -> &'static str {
    match action {
        PlanActionKind::CreateContent => "CREATE PAGE",
        PlanActionKind::UpdateContent => "UPDATE PAGE",
        PlanActionKind::MoveContent => "MOVE PAGE",
        PlanActionKind::UploadAttachment => "UPLOAD FILE",
        PlanActionKind::DeleteAttachment => "DELETE FILE",
        PlanActionKind::UpdateLabels => "UPDATE LABELS",
        PlanActionKind::UpdateProperties => "UPDATE PROPERTIES",
        PlanActionKind::DeleteRemote => "DELETE REMOTE",
        PlanActionKind::Noop => "CLEAN",
    }
}

fn action_style(action: &PlanActionKind, theme: Theme) -> Style {
    action_mark(action, theme).1
}

fn review_counts(items: &[PlanItem]) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for item in items {
        let key = match item.action {
            PlanActionKind::CreateContent => "create",
            PlanActionKind::UploadAttachment => "upload",
            PlanActionKind::UpdateContent
            | PlanActionKind::UpdateLabels
            | PlanActionKind::UpdateProperties => "update",
            PlanActionKind::MoveContent => "move",
            PlanActionKind::DeleteAttachment | PlanActionKind::DeleteRemote => "delete",
            PlanActionKind::Noop => "clean",
        };
        *counts.entry(key).or_default() += 1;
    }
    counts
}

fn page_rows(mut items: Vec<ContentItem>, limit: usize) -> Vec<PageRow> {
    items.sort_by(|left, right| {
        left.title
            .to_ascii_lowercase()
            .cmp(&right.title.to_ascii_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    let ids: BTreeSet<String> = items.iter().map(|item| item.id.clone()).collect();
    let mut children: BTreeMap<Option<String>, Vec<usize>> = BTreeMap::new();
    for (index, item) in items.iter().enumerate() {
        let parent = item
            .parent_id
            .as_ref()
            .filter(|parent| ids.contains(*parent))
            .cloned();
        children.entry(parent).or_default().push(index);
    }
    let mut rows = Vec::new();
    let mut visited = BTreeSet::new();
    append_page_rows(None, 0, &items, &children, &mut visited, &mut rows, limit);
    for index in 0..items.len() {
        if rows.len() >= limit {
            break;
        }
        if !visited.contains(&index) {
            append_one_page_row(index, 0, &items, &children, &mut visited, &mut rows, limit);
        }
    }
    rows
}

fn append_page_rows(
    parent: Option<String>,
    depth: usize,
    items: &[ContentItem],
    children: &BTreeMap<Option<String>, Vec<usize>>,
    visited: &mut BTreeSet<usize>,
    rows: &mut Vec<PageRow>,
    limit: usize,
) {
    let Some(indexes) = children.get(&parent) else {
        return;
    };
    for &index in indexes {
        if rows.len() >= limit {
            return;
        }
        append_one_page_row(index, depth, items, children, visited, rows, limit);
    }
}

fn append_one_page_row(
    index: usize,
    depth: usize,
    items: &[ContentItem],
    children: &BTreeMap<Option<String>, Vec<usize>>,
    visited: &mut BTreeSet<usize>,
    rows: &mut Vec<PageRow>,
    limit: usize,
) {
    if !visited.insert(index) || rows.len() >= limit {
        return;
    }
    let item = items[index].clone();
    let id = item.id.clone();
    rows.push(PageRow { item, depth });
    append_page_rows(
        Some(id),
        depth.saturating_add(1),
        items,
        children,
        visited,
        rows,
        limit,
    );
}

fn render_input(
    frame: &mut ratatui::Frame<'_>,
    theme: Theme,
    title: &str,
    prompt: &str,
    buffer: &str,
) {
    let width = frame.area().width.saturating_sub(4).min(78);
    let area = centered_rect(width, 7, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(format!(" {title}  /  ENTER OPEN  /  ESC CANCEL "))
        .borders(Borders::ALL)
        .border_style(theme.active_rule())
        .style(theme.canvas());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(safe_text(prompt)).style(theme.muted()),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let input_y = inner.y.saturating_add(2);
    frame.render_widget(
        Paragraph::new("> ").style(theme.key()),
        Rect::new(inner.x, input_y, 2.min(inner.width), 1),
    );
    let input_area = Rect::new(
        inner.x.saturating_add(2),
        input_y,
        inner.width.saturating_sub(2),
        1,
    );
    let buffer = safe_text(buffer);
    let visible = tail_by_display_width(&buffer, usize::from(input_area.width.saturating_sub(1)));
    let visible_width = display_width(visible).min(usize::from(u16::MAX)) as u16;
    frame.render_widget(
        Paragraph::new(visible).style(theme.body().add_modifier(Modifier::BOLD)),
        input_area,
    );
    let cursor_x = input_area
        .x
        .saturating_add(visible_width)
        .min(input_area.right().saturating_sub(1));
    frame.set_cursor_position((cursor_x, input_y));
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .split(area);
    Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .split(vertical[0])[0]
}

fn inset(area: Rect, horizontal: u16, vertical: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(horizontal),
        y: area.y.saturating_add(vertical),
        width: area.width.saturating_sub(horizontal.saturating_mul(2)),
        height: area.height.saturating_sub(vertical.saturating_mul(2)),
    }
}

fn compact_site(site: &str) -> String {
    safe_text(
        site.trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/'),
    )
}

fn human_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{} B", bytes as u64)
    }
}

fn safe_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn display_width(value: &str) -> usize {
    Line::raw(value).width()
}

fn character_display_width(character: char) -> usize {
    let mut encoded = [0; 4];
    display_width(character.encode_utf8(&mut encoded))
}

fn truncate_text(value: &str, max_width: usize) -> String {
    if display_width(value) <= max_width {
        return value.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let content_width = max_width - 3;
    let mut used = 0;
    let mut output = String::new();
    for character in value.chars() {
        let width = character_display_width(character);
        if used + width > content_width {
            break;
        }
        used += width;
        output.push(character);
    }
    output.push_str("...");
    output
}

fn tail_by_display_width(value: &str, max_width: usize) -> &str {
    if display_width(value) <= max_width {
        return value;
    }
    let total_width = display_width(value);
    let mut removed_width = 0;
    let mut start = value.len();
    for (index, character) in value.char_indices() {
        if total_width.saturating_sub(removed_width) <= max_width {
            start = index;
            break;
        }
        removed_width += character_display_width(character);
    }
    let mut visible = &value[start..];
    while let Some(character) = visible.chars().next() {
        if character_display_width(character) != 0 {
            break;
        }
        visible = &visible[character.len_utf8()..];
    }
    visible
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect::<String>();
    [
        "password",
        "passwd",
        "secret",
        "privatekey",
        "accesskey",
        "signingkey",
        "apikey",
        "token",
        "cookie",
        "credential",
        "authorization",
        "bearer",
    ]
    .iter()
    .any(|sensitive| normalized.contains(sensitive))
}

fn redact_json_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    let value = if is_sensitive_key(key) {
                        serde_json::Value::String("[REDACTED]".into())
                    } else {
                        redact_json_value(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact_json_value).collect())
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use ratatui::backend::TestBackend;
    use serde_json::json;

    fn content(id: &str, title: &str, parent_id: Option<&str>, body: Option<&str>) -> ContentItem {
        let fixture_time = Utc.with_ymd_and_hms(2026, 8, 28, 18, 8, 0).unwrap();
        ContentItem {
            id: id.into(),
            kind: crate::model::ContentKind::Page,
            title: title.into(),
            status: "current".into(),
            space_id: Some("42".into()),
            space_key: Some("DOCS".into()),
            parent_id: parent_id.map(str::to_string),
            version: Some(7),
            body_storage: body.map(str::to_string),
            labels: vec!["handbook".into()],
            properties: BTreeMap::new(),
            web_url: Some(format!(
                "https://example.atlassian.net/wiki/spaces/DOCS/pages/{id}"
            )),
            created_at: Some(fixture_time),
            updated_at: Some(fixture_time),
        }
    }

    fn app() -> App {
        let mut app = App::new(TuiOptions {
            profile: "work".into(),
            site: "https://example.atlassian.net/wiki".into(),
            initial_space: Some("DOCS".into()),
            review_path: None,
            delete_remote: false,
            page_size: 200,
            color: true,
        });
        app.spaces = vec![SpaceSummary {
            id: "42".into(),
            key: "DOCS".into(),
            name: "Documentation".into(),
            space_type: Some("global".into()),
            homepage_id: Some("100".into()),
            web_url: None,
        }];
        app.active_space = app.spaces.first().cloned();
        app.pages = page_rows(
            vec![
                content("101", "Release checklist", Some("100"), None),
                content("100", "Engineering handbook", None, None),
                content("102", "Runbooks", Some("100"), None),
            ],
            200,
        );
        app.page_state.select(Some(0));
        let mut selected = app.pages[0].item.clone();
        selected.body_storage = Some(
            "<h1>Engineering handbook</h1><p>Everything the team needs, in one place.</p><h2>Start here</h2><ul><li>Find the release owner</li><li>Open the deployment runbook</li></ul>".into(),
        );
        app.page_proof = PageProof {
            item: Some(selected.clone()),
            markdown: storage_to_markdown(selected.body_storage.as_deref().unwrap()),
            attachments: ResourceState::Ready(vec![AttachmentInfo {
                id: "att-1".into(),
                title: "release-map.pdf".into(),
                version: Some(2),
                media_type: Some("application/pdf".into()),
                file_size: Some(1536),
                download_url: None,
                comment: None,
            }]),
            comments: ResourceState::Ready(Vec::new()),
            properties: ResourceState::Ready(vec![ContentProperty {
                id: Some("prop-1".into()),
                key: "api_token".into(),
                value: json!("never-render-this"),
                version: Some(1),
            }]),
        };
        app.workspace_loaded = true;
        app.notice = Notice::success("Page proof and margin evidence are ready.");
        app
    }

    fn rendered_text(buffer: &ratatui::buffer::Buffer) -> String {
        let area = buffer.area;
        let mut output = String::new();
        for y in area.y..area.bottom() {
            let mut line = String::new();
            for x in area.x..area.right() {
                line.push_str(buffer[(x, y)].symbol());
            }
            output.push_str(line.trim_end());
            output.push('\n');
        }
        output
    }

    fn render_app(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        rendered_text(terminal.backend().buffer())
    }

    fn review_app() -> App {
        let mut app = app();
        app.mode = Mode::Review;
        app.review_items = vec![PlanItem {
            action: PlanActionKind::UpdateContent,
            title: "Release checklist".into(),
            content_id: Some("101".into()),
            path: PathBuf::from("docs/release-checklist"),
            details: "changed: body".into(),
            diff: Some("--- previous\n+++ current\n-old step\n+reviewed step\n".into()),
        }];
        app.review_state.select(Some(0));
        app.notice = Notice::warning(
            "1 local change is ready for review; remote drift is checked by apply.",
        );
        app
    }

    #[test]
    fn page_hierarchy_is_parent_first_and_depth_aware() {
        let rows = page_rows(
            vec![
                content("child", "Child", Some("root"), None),
                content("root", "Root", None, None),
            ],
            20,
        );
        assert_eq!(rows[0].item.id, "root");
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].item.id, "child");
        assert_eq!(rows[1].depth, 1);
    }

    #[test]
    fn wide_proof_desk_renders_contents_galley_and_margin_without_secrets() {
        let backend = TestBackend::new(140, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.margin_tab = MarginTab::Properties;
        terminal.draw(|frame| app.render(frame)).unwrap();
        let text = rendered_text(terminal.backend().buffer());
        if std::env::var_os("CONFLUENCE_TUI_SNAPSHOT").is_some() {
            eprintln!("\n{text}");
        }
        assert!(text.contains("PROOF DESK"));
        assert!(text.contains("CONTENTS"));
        assert!(text.contains("GALLEY"));
        assert!(text.contains("PROOF MARGIN"));
        assert!(text.contains("Engineering handbook"));
        assert!(text.contains("[REDACTED]"));
        assert!(!text.contains("never-render-this"));
    }

    #[test]
    fn medium_layout_keeps_contents_and_galley_but_defers_margin() {
        let backend = TestBackend::new(92, 26);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let text = rendered_text(terminal.backend().buffer());
        if std::env::var_os("CONFLUENCE_TUI_SNAPSHOT").is_some() {
            eprintln!("\n{text}");
        }
        assert!(text.contains("CONTENTS"));
        assert!(text.contains("GALLEY"));
        assert!(!text.contains("PROOF MARGIN"));
    }

    #[test]
    fn compact_layout_preserves_a_complete_folio_and_unfolds_the_proof() {
        let backend = TestBackend::new(72, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let text = rendered_text(terminal.backend().buffer());
        if std::env::var_os("CONFLUENCE_TUI_SNAPSHOT").is_some() {
            eprintln!("\n{text}");
        }
        assert!(text.contains("CONTENTS"));
        assert!(!text.contains("GALLEY"));

        app.overlay = Overlay::Proof;
        terminal.draw(|frame| app.render(frame)).unwrap();
        let text = rendered_text(terminal.backend().buffer());
        assert!(text.contains("GALLEY"));
        assert!(text.contains("Everything the team needs, in one place"));
    }

    #[test]
    fn compact_header_keeps_profile_site_space_and_mode_in_context() {
        let mut app = app();
        let text = render_app(&mut app, 72, 24);
        assert!(text.contains("BROWSE"));
        assert!(text.contains("work  /  example.atlassian.net/wiki  /  DOCS"));
    }

    #[test]
    fn review_margin_is_reachable_from_compact_list_and_proof() {
        let mut app = review_app();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
            Action::None
        );
        assert_eq!(app.overlay, Overlay::Margin);
        let text = render_app(&mut app, 72, 24);
        assert!(text.contains("REVIEW MARKS"));
        assert!(text.contains("LOCAL PLAN ONLY"));

        app.overlay = Overlay::Proof;
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
            Action::None
        );
        assert_eq!(app.overlay, Overlay::Margin);
    }

    #[test]
    fn overlay_footers_only_advertise_actions_the_overlay_accepts() {
        let mut app = review_app();
        app.overlay = Overlay::Proof;
        let proof = render_app(&mut app, 92, 26);
        assert!(proof.contains("j/k scroll"));
        assert!(proof.contains("m margin"));
        assert!(proof.contains("esc back"));
        assert!(!proof.contains("q quit"));
        assert!(!proof.contains("tab browse"));

        app.overlay = Overlay::Margin;
        let margin = render_app(&mut app, 92, 26);
        assert!(margin.contains("m/esc back to local plan"));
        assert!(!margin.contains("1-4 margin view"));
        assert!(!margin.contains("q quit"));

        app.overlay = Overlay::Help;
        let help = render_app(&mut app, 92, 26);
        assert!(help.contains("Inspect the selected local diff"));
        assert!(help.contains("Rebuild the local sync plan"));
        assert!(!help.contains("Show metadata, files, notes, or properties"));
    }

    #[test]
    fn margin_evidence_never_leaks_from_the_previously_selected_page() {
        let backend = TestBackend::new(140, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.page_state.select(Some(1));
        app.margin_tab = MarginTab::Properties;
        terminal.draw(|frame| app.render(frame)).unwrap();
        let text = rendered_text(terminal.backend().buffer());
        assert!(text.contains("PROPERTIES NOT LOADED"));
        assert!(!text.contains("api_token"));
        assert!(!text.contains("never-render-this"));
    }

    #[test]
    fn tiny_terminals_name_the_minimum_and_recovery() {
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let text = rendered_text(terminal.backend().buffer());
        assert!(text.contains("THE PROOF DESK NEEDS MORE ROOM"));
        assert!(text.contains("q or Ctrl-C"));
    }

    #[test]
    fn no_color_uses_terminal_defaults() {
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.color = false;
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
        );
    }

    #[test]
    fn no_color_marks_the_active_margin_tab_with_text() {
        let mut app = app();
        app.color = false;
        app.margin_tab = MarginTab::Attachments;
        let text = render_app(&mut app, 140, 34);
        assert!(text.contains(">2 FILE"));
        assert!(!text.contains(">1 META"));
    }

    #[test]
    fn colored_render_never_falls_back_to_the_host_terminal_background() {
        let backend = TestBackend::new(140, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| cell.bg != Color::Reset)
        );
    }

    #[test]
    fn review_mode_explains_local_only_boundary_and_renders_diff() {
        let backend = TestBackend::new(140, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = review_app();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let text = rendered_text(terminal.backend().buffer());
        if std::env::var_os("CONFLUENCE_TUI_SNAPSHOT").is_some() {
            eprintln!("\n{text}");
        }
        assert!(text.contains("SYNC PROOF"));
        assert!(text.contains("CHANGE GALLEY"));
        assert!(text.contains("LOCAL PLAN ONLY"));
        assert!(text.contains("reviewed step"));
        assert!(text.contains("versions and refuses drift"));
        assert!(text.contains("DELETE  OFF"));
        assert!(text.contains("Use --delete-remote to plan"));
        assert!(text.contains("removals."));
    }

    #[test]
    fn review_marks_make_opt_in_attachment_deletions_explicit() {
        let mut app = review_app();
        app.delete_remote = true;
        app.review_items.push(PlanItem {
            action: PlanActionKind::DeleteAttachment,
            title: "retired-diagram.png".into(),
            content_id: Some("att-2".into()),
            path: PathBuf::from("docs/attachments"),
            details: "attachment removed locally".into(),
            diff: None,
        });
        let text = render_app(&mut app, 140, 34);
        assert!(text.contains("DELETE  1"));
        assert!(!text.contains("DELETE  OFF"));
        assert!(!text.contains("Use --delete-remote to plan removals"));
        assert!(app.delete_remote);
    }

    #[test]
    fn keyboard_map_switches_modes_and_opens_complete_surfaces() {
        let mut app = app();
        app.review_path = Some(PathBuf::from("missing-on-purpose"));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        );
        assert_eq!(app.overlay, Overlay::Proof);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Action::None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Action::Replan
        );
        assert_eq!(app.mode, Mode::Review);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
    }

    #[test]
    fn control_characters_are_removed_from_remote_copy() {
        assert_eq!(safe_text("safe\u{1b}[31m"), "safe [31m");
    }

    #[test]
    fn camel_case_secret_property_names_are_redacted() {
        for key in [
            "accessToken",
            "authToken",
            "sessionCookie",
            "clientCredential",
            "authorizationHeader",
            "privateKeyPem",
            "access-key-id",
            "SIGNING_KEY",
            "x.api key",
        ] {
            assert!(
                is_sensitive_key(key),
                "{key} should be treated as sensitive"
            );
        }
        assert!(!is_sensitive_key("editorTheme"));
        assert!(!is_sensitive_key("publicKey"));
    }

    #[test]
    fn nested_property_secrets_are_redacted_through_objects_and_arrays() {
        let value = json!({
            "safe": "visible",
            "connection": {
                "privateKey": "never-private",
                "nested": [{"accessKeyId": "never-access", "region": "eu-west-1"}]
            },
            "records": [{"signing-key": "never-signing"}]
        });
        let rendered = redact_json_value(&value).to_string();
        assert!(rendered.contains("visible"));
        assert!(rendered.contains("eu-west-1"));
        assert_eq!(rendered.matches("[REDACTED]").count(), 3);
        assert!(!rendered.contains("never-private"));
        assert!(!rendered.contains("never-access"));
        assert!(!rendered.contains("never-signing"));

        let mut app = app();
        app.margin_tab = MarginTab::Properties;
        app.page_proof.properties = ResourceState::Ready(vec![ContentProperty {
            id: Some("nested".into()),
            key: "connectionConfig".into(),
            value,
            version: Some(1),
        }]);
        let screen = render_app(&mut app, 140, 34);
        assert!(screen.contains("[REDACTED]"));
        assert!(!screen.contains("never-private"));
        assert!(!screen.contains("never-access"));
    }

    #[test]
    fn help_overlay_scrolls_to_its_end_at_the_minimum_supported_size() {
        let mut app = app();
        app.overlay = Overlay::Help;
        let top = render_app(&mut app, MIN_WIDTH, MIN_HEIGHT);
        assert!(top.contains("MORE BELOW"));
        assert!(app.overlay_max_scroll > 0);

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.overlay_scroll, 2);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.overlay_scroll, 0);
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert!(app.overlay_scroll > 0);
        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.overlay_scroll, 0);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
            Action::None
        );
        let bottom = render_app(&mut app, MIN_WIDTH, MIN_HEIGHT);
        assert!(bottom.contains("MORE ABOVE"));
        assert!(bottom.contains("END"));
        assert!(bottom.contains("Review never contacts or changes Confluence; apply"));
        assert!(bottom.contains("remains a separate CLI command."));

        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.overlay_scroll, 0);
    }

    #[test]
    fn long_margin_resources_scroll_with_position_cues_and_reset_between_tabs() {
        let mut app = app();
        app.page_proof.attachments = ResourceState::Ready(
            (0..18)
                .map(|index| AttachmentInfo {
                    id: format!("att-{index}"),
                    title: format!("evidence-{index:02}-with-a-descriptive-file-name.pdf"),
                    version: Some(1),
                    media_type: Some("application/pdf".into()),
                    file_size: Some(2048),
                    download_url: None,
                    comment: None,
                })
                .collect(),
        );
        app.margin_tab = MarginTab::Attachments;
        app.overlay = Overlay::Margin;
        let top = render_app(&mut app, MIN_WIDTH, MIN_HEIGHT);
        assert!(top.contains("MORE BELOW"));

        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        let bottom = render_app(&mut app, MIN_WIDTH, MIN_HEIGHT);
        assert!(bottom.contains("MORE ABOVE"));
        assert!(bottom.contains("evidence-17"));
        assert!(bottom.contains(">2 FILE"));

        app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert_eq!(app.overlay_scroll, 0);
        assert_eq!(app.overlay_max_scroll, 0);
    }

    #[test]
    fn browser_url_fallback_never_uses_a_stale_page_proof_and_rejects_unsafe_schemes() {
        let mut app = app();
        app.page_state.select(Some(1));
        app.pages[1].item.web_url = None;
        assert_eq!(
            app.selected_browser_url(),
            Err("This page did not include a browser URL.")
        );

        let mut selected = app.pages[1].item.clone();
        selected.web_url = Some("file:///tmp/not-confluence".into());
        app.page_proof.item = Some(selected);
        assert_eq!(
            app.selected_browser_url(),
            Err("Confluence browser URLs must use http or https.")
        );

        app.page_proof.item.as_mut().unwrap().web_url =
            Some("https://example.atlassian.net/wiki/pages/101".into());
        assert_eq!(
            app.selected_browser_url(),
            Ok("https://example.atlassian.net/wiki/pages/101")
        );
    }

    #[test]
    fn unicode_text_uses_terminal_cell_width_for_truncation_and_input_tails() {
        let text = "發布準備 café";
        let truncated = truncate_text(text, 10);
        assert!(display_width(&truncated) <= 10);
        assert!(truncated.ends_with("..."));
        assert!(!truncated.contains('\u{fffd}'));

        let tail = tail_by_display_width("prefix/發布準備", 8);
        assert_eq!(tail, "發布準備");
        assert_eq!(display_width(tail), 8);
    }

    #[tokio::test]
    async fn quit_detaches_an_in_flight_review_plan_without_waiting_for_it() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let events = EventReader {
            receiver,
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        };
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let plan_receiver = launch_review_plan_with(move || {
            started_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            Err(anyhow!("test planner released"))
        })
        .unwrap();
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        sender
            .send(Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
            ))))
            .unwrap();

        let quit = tokio::time::timeout(
            Duration::from_millis(250),
            complete_load_or_quit(
                async move {
                    let _ = plan_receiver.await;
                },
                &events,
            ),
        )
        .await
        .expect("quit should not wait for the blocking review planner")
        .unwrap();

        assert!(quit);
        release_sender.send(()).unwrap();
    }

    #[test]
    fn page_proofs_collapse_sync_markup_without_hiding_readable_copy() {
        let markdown = "~~~~confluence-layout-section fixed-width\nbreakout-mode: default\n--- cell ---\n:::confluence-macro panel\nbgColor: #fff\n---\nSee [Release notes](confluence-page://page?content-title=Release+notes).\n*<span style=\"color: red;\">Write an update</span>*\n:::\n~~~~";
        let rendered = markdown_lines(markdown, Theme::new(false))
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("[LAYOUT SECTION FIXED WIDTH]"));
        assert!(rendered.contains("[CELL]"));
        assert!(rendered.contains("[MACRO PANEL]"));
        assert!(rendered.contains("See [Release notes]."));
        assert!(rendered.contains("*Write an update*"));
        assert!(!rendered.contains("confluence-page://"));
        assert!(!rendered.contains("bgColor"));
        assert!(!rendered.contains("breakout-mode"));
        assert!(!rendered.contains("<span"));
        assert!(!rendered.contains("~~~~"));
        assert!(!rendered.contains(":::confluence"));
    }
}
