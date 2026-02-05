use std::collections::HashMap;
use std::time::Instant;
use winit::dpi::PhysicalPosition;
use winit::window::CursorIcon;

pub const WEB_SCROLL_STEP: f64 = 48.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebKey {
    Escape,
    Enter,
    Backspace,
    Delete,
    Tab,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WebMode {
    Normal,
    Insert,
    Visual,
    VisualLine,
    Hint,
    MarkSet,
    MarkJump,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebHintAction {
    Open,
    OpenNewTab,
    CopyLink,
}

#[derive(Clone, Debug)]
struct WebHintState {
    action: WebHintAction,
    keys: String,
}

#[derive(Default)]
struct WebPending {
    g: bool,
    z: bool,
    y: bool,
    bracket: Option<char>,
}

#[derive(Clone, Debug)]
struct WebMark {
    url: String,
    scroll_x: f64,
    scroll_y: f64,
}

#[derive(Clone, Debug)]
struct WebPendingScroll {
    url: String,
    scroll_x: f64,
    scroll_y: f64,
}

pub struct WebCommandState {
    mode: WebMode,
    pending: WebPending,
    hint: Option<WebHintState>,
    last_find: Option<String>,
    last_find_backward: bool,
    marks: HashMap<char, WebMark>,
    pending_scroll: Option<WebPendingScroll>,
    help_visible: bool,
    cursor_pending: bool,
    last_cursor: Option<CursorIcon>,
    last_cursor_pos: Option<PhysicalPosition<f64>>,
    cursor_bootstrapped: bool,
    last_cursor_request: Option<Instant>,
}

impl WebCommandState {
    pub(crate) fn mode(&self) -> WebMode {
        self.mode
    }

    fn reset_pending(&mut self) {
        self.pending = WebPending::default();
    }

    fn set_mode(&mut self, mode: WebMode) {
        self.mode = mode;
        if mode != WebMode::Hint {
            self.hint = None;
        }
        if !matches!(mode, WebMode::Hint | WebMode::MarkSet | WebMode::MarkJump) {
            self.reset_pending();
        }
    }

    pub(crate) fn reset_mode(&mut self) {
        self.set_mode(WebMode::Normal);
    }

    pub(crate) fn status_label(&self) -> &'static str {
        match self.mode {
            WebMode::Normal => "NORMAL",
            WebMode::Insert => "INSERT",
            WebMode::Visual => "VISUAL",
            WebMode::VisualLine => "VISUAL LINE",
            WebMode::Hint => "HINT",
            WebMode::MarkSet => "MARK SET",
            WebMode::MarkJump => "MARK JUMP",
        }
    }

    pub(crate) fn set_mark(&mut self, name: char, url: String, scroll_x: f64, scroll_y: f64) {
        self.marks.insert(name, WebMark { url, scroll_x, scroll_y });
    }

    pub(crate) fn take_pending_scroll(&mut self, url: &str) -> Option<(f64, f64)> {
        let pending = self.pending_scroll.take()?;
        if pending.url == url {
            Some((pending.scroll_x, pending.scroll_y))
        } else {
            self.pending_scroll = Some(pending);
            None
        }
    }

    pub(crate) fn cursor_pending(&self) -> bool {
        self.cursor_pending
    }

    pub(crate) fn set_cursor_pending(&mut self, pending: bool) {
        self.cursor_pending = pending;
    }

    pub(crate) fn last_cursor(&self) -> Option<CursorIcon> {
        self.last_cursor
    }

    pub(crate) fn set_last_cursor(&mut self, cursor: CursorIcon) {
        self.last_cursor = Some(cursor);
    }

    pub(crate) fn set_last_cursor_pos(&mut self, position: PhysicalPosition<f64>) {
        self.last_cursor_pos = Some(position);
    }

    pub(crate) fn last_cursor_pos(&self) -> Option<PhysicalPosition<f64>> {
        self.last_cursor_pos
    }

    pub(crate) fn cursor_bootstrapped(&self) -> bool {
        self.cursor_bootstrapped
    }

    pub(crate) fn set_cursor_bootstrapped(&mut self, bootstrapped: bool) {
        self.cursor_bootstrapped = bootstrapped;
    }

    pub(crate) fn last_cursor_request(&self) -> Option<Instant> {
        self.last_cursor_request
    }

    pub(crate) fn set_last_cursor_request(&mut self, instant: Instant) {
        self.last_cursor_request = Some(instant);
    }

    pub(crate) fn clear_last_cursor_request(&mut self) {
        self.last_cursor_request = None;
    }
}

impl Default for WebCommandState {
    fn default() -> Self {
        Self {
            mode: WebMode::Normal,
            pending: WebPending::default(),
            hint: None,
            last_find: None,
            last_find_backward: false,
            marks: HashMap::default(),
            pending_scroll: None,
            help_visible: false,
            cursor_pending: false,
            last_cursor: None,
            last_cursor_pos: None,
            cursor_bootstrapped: false,
            last_cursor_request: None,
        }
    }
}

pub trait WebActions {
    fn scroll_by(&mut self, dx: f64, dy: f64);
    fn scroll_half_page(&mut self, down: bool);
    fn scroll_top(&mut self);
    fn scroll_bottom(&mut self);
    fn scroll_far_left(&mut self);
    fn scroll_far_right(&mut self);
    fn scroll_to(&mut self, x: f64, y: f64);

    fn go_back(&mut self);
    fn go_forward(&mut self);

    fn open_command_bar(&mut self, input: &str);
    fn start_find_prompt(&mut self);
    fn find(&mut self, query: &str, backwards: bool);

    fn hints_start(&mut self, action: WebHintAction);
    fn hints_update(&mut self, keys: &str, action: WebHintAction);
    fn hints_cancel(&mut self);

    fn copy_selection(&mut self);
    fn clear_selection(&mut self);
    fn start_visual_selection(&mut self);
    fn visual_move(&mut self, direction: &str, granularity: &str);

    fn focus_input(&mut self);
    fn blur_active_element(&mut self);

    fn insert_text(&mut self, text: &str);
    fn delete_backward(&mut self);
    fn delete_forward(&mut self);
    fn insert_paragraph(&mut self);
    fn insert_tab(&mut self);
    fn caret_move(&mut self, direction: &str, granularity: &str);

    fn view_source(&mut self);
    fn follow_rel(&mut self, rel: &str);
    fn copy_url(&mut self);
    fn open_clipboard(&mut self, new_tab: bool);
    fn up_url(&mut self, root: bool);

    fn new_tab(&mut self);
    fn close_tab(&mut self);
    fn restore_tab(&mut self);
    fn select_previous_tab(&mut self);
    fn select_next_tab(&mut self);
    fn select_tab_at_index(&mut self, index: usize);
    fn select_last_tab(&mut self);
    fn reload(&mut self);

    fn show_help(&mut self);
    fn hide_help(&mut self);

    fn request_mark_set(&mut self, name: char, url: String);
    fn current_url(&mut self) -> Option<String>;
    fn open_url(&mut self, url: String);
    fn push_error(&mut self, message: String);
}

pub fn handle_key(
    state: &mut WebCommandState,
    actions: &mut impl WebActions,
    key: WebKey,
    text: &str,
) -> bool {
    if matches!(key, WebKey::Escape) {
        handle_escape(state, actions);
        return true;
    }

    match state.mode {
        WebMode::Insert => return handle_insert(state, actions, key, text),
        WebMode::Hint => return handle_hint(state, actions, key, text),
        WebMode::MarkSet => return handle_mark_set(state, actions, text),
        WebMode::MarkJump => return handle_mark_jump(state, actions, text),
        WebMode::Visual | WebMode::VisualLine => return handle_visual(state, actions, text),
        WebMode::Normal => (),
    }

    let mut chars = text.chars();
    let Some(ch) = chars.next() else {
        return false;
    };
    if chars.next().is_some() {
        return false;
    }

    let mut retry = true;
    while retry {
        retry = false;

        if let Some(bracket) = state.pending.bracket {
            state.pending.bracket = None;
            if bracket == ch {
                match bracket {
                    '[' => {
                        actions.follow_rel("prev");
                        return true;
                    },
                    ']' => {
                        actions.follow_rel("next");
                        return true;
                    },
                    _ => (),
                }
            } else {
                retry = true;
                continue;
            }
        }

        if state.pending.g {
            state.pending.g = false;
            match ch {
                'g' => {
                    actions.scroll_top();
                    return true;
                },
                '0' => {
                    actions.select_tab_at_index(0);
                    return true;
                },
                '$' => {
                    actions.select_last_tab();
                    return true;
                },
                'u' => {
                    actions.up_url(false);
                    return true;
                },
                'U' => {
                    actions.up_url(true);
                    return true;
                },
                's' => {
                    actions.view_source();
                    return true;
                },
                'i' => {
                    actions.focus_input();
                    state.set_mode(WebMode::Insert);
                    return true;
                },
                _ => {
                    retry = true;
                    continue;
                },
            }
        }

        if state.pending.z {
            state.pending.z = false;
            match ch {
                'H' | 'h' => {
                    actions.scroll_far_left();
                    return true;
                },
                'L' | 'l' => {
                    actions.scroll_far_right();
                    return true;
                },
                _ => {
                    retry = true;
                    continue;
                },
            }
        }

        if state.pending.y {
            state.pending.y = false;
            match ch {
                'y' => {
                    actions.copy_url();
                    return true;
                },
                'f' => {
                    start_hints(state, actions, WebHintAction::CopyLink);
                    return true;
                },
                _ => {
                    retry = true;
                    continue;
                },
            }
        }
    }

    match ch {
        'j' => actions.scroll_by(0.0, WEB_SCROLL_STEP),
        'k' => actions.scroll_by(0.0, -WEB_SCROLL_STEP),
        'h' => actions.scroll_by(-WEB_SCROLL_STEP, 0.0),
        'l' => actions.scroll_by(WEB_SCROLL_STEP, 0.0),
        'd' => actions.scroll_half_page(true),
        'u' => actions.scroll_half_page(false),
        'G' => actions.scroll_bottom(),
        'g' => {
            state.pending.g = true;
            return true;
        },
        'z' => {
            state.pending.z = true;
            return true;
        },
        '[' => {
            state.pending.bracket = Some('[');
            return true;
        },
        ']' => {
            state.pending.bracket = Some(']');
            return true;
        },
        'f' => {
            start_hints(state, actions, WebHintAction::Open);
            return true;
        },
        'F' => {
            start_hints(state, actions, WebHintAction::OpenNewTab);
            return true;
        },
        'y' => {
            state.pending.y = true;
            return true;
        },
        'H' => {
            actions.go_back();
            return true;
        },
        'L' => {
            actions.go_forward();
            return true;
        },
        '/' => {
            actions.start_find_prompt();
            return true;
        },
        'n' => {
            find_next(state, actions, false);
            return true;
        },
        'N' => {
            find_next(state, actions, true);
            return true;
        },
        'v' => {
            toggle_visual(state, actions, false);
            return true;
        },
        'V' => {
            toggle_visual(state, actions, true);
            return true;
        },
        'p' => {
            actions.open_clipboard(false);
            return true;
        },
        'P' => {
            actions.open_clipboard(true);
            return true;
        },
        't' => {
            actions.new_tab();
            return true;
        },
        'x' => {
            actions.close_tab();
            return true;
        },
        'X' => {
            actions.restore_tab();
            return true;
        },
        'J' => {
            actions.select_previous_tab();
            return true;
        },
        'K' => {
            actions.select_next_tab();
            return true;
        },
        'o' => {
            actions.open_command_bar("o ");
            return true;
        },
        'O' => {
            actions.open_command_bar("O ");
            return true;
        },
        'b' => {
            actions.open_command_bar("b ");
            return true;
        },
        'B' => {
            actions.open_command_bar("B ");
            return true;
        },
        'T' => {
            actions.open_command_bar("T ");
            return true;
        },
        'r' => {
            actions.reload();
            return true;
        },
        'm' => {
            state.set_mode(WebMode::MarkSet);
            return true;
        },
        '`' => {
            state.set_mode(WebMode::MarkJump);
            return true;
        },
        '?' => {
            toggle_help(state, actions);
            return true;
        },
        _ => (),
    }

    true
}

pub fn find(
    state: &mut WebCommandState,
    actions: &mut impl WebActions,
    query: &str,
    backwards: bool,
) {
    actions.find(query, backwards);
    state.last_find = Some(query.to_string());
    state.last_find_backward = backwards;
}

fn find_next(state: &mut WebCommandState, actions: &mut impl WebActions, backwards: bool) {
    let Some(query) = state.last_find.clone() else {
        actions.push_error(String::from("No active search"));
        return;
    };
    find(state, actions, &query, backwards);
}

fn handle_escape(state: &mut WebCommandState, actions: &mut impl WebActions) {
    if state.help_visible {
        actions.hide_help();
        state.help_visible = false;
        return;
    }

    match state.mode {
        WebMode::Hint => actions.hints_cancel(),
        WebMode::Visual | WebMode::VisualLine => actions.clear_selection(),
        WebMode::Insert => actions.blur_active_element(),
        WebMode::Normal | WebMode::MarkSet | WebMode::MarkJump => (),
    }

    state.set_mode(WebMode::Normal);
}

fn handle_insert(
    state: &mut WebCommandState,
    actions: &mut impl WebActions,
    key: WebKey,
    text: &str,
) -> bool {
    match key {
        WebKey::Escape => {
            handle_escape(state, actions);
            return true;
        },
        WebKey::Backspace => {
            actions.delete_backward();
            return true;
        },
        WebKey::Delete => {
            actions.delete_forward();
            return true;
        },
        WebKey::Enter => {
            actions.insert_paragraph();
            return true;
        },
        WebKey::Tab => {
            actions.insert_tab();
            return true;
        },
        WebKey::ArrowLeft => {
            actions.caret_move("backward", "character");
            return true;
        },
        WebKey::ArrowRight => {
            actions.caret_move("forward", "character");
            return true;
        },
        WebKey::ArrowUp => {
            actions.caret_move("backward", "line");
            return true;
        },
        WebKey::ArrowDown => {
            actions.caret_move("forward", "line");
            return true;
        },
        WebKey::Other => (),
    }

    if !text.is_empty() {
        actions.insert_text(text);
    }

    true
}

fn handle_hint(
    state: &mut WebCommandState,
    actions: &mut impl WebActions,
    key: WebKey,
    text: &str,
) -> bool {
    let Some(hint) = state.hint.as_mut() else {
        state.set_mode(WebMode::Normal);
        return true;
    };

    match key {
        WebKey::Escape => {
            actions.hints_cancel();
            state.set_mode(WebMode::Normal);
            return true;
        },
        WebKey::Backspace => {
            let (keys, action) = {
                hint.keys.pop();
                (hint.keys.clone(), hint.action)
            };
            actions.hints_update(&keys, action);
            return true;
        },
        WebKey::Enter => {
            let (keys, action) = (hint.keys.clone(), hint.action);
            actions.hints_update(&keys, action);
            return true;
        },
        _ => (),
    }

    let Some(ch) = single_char(text) else {
        return true;
    };
    hint.keys.push(ch.to_ascii_lowercase());
    let (keys, action) = (hint.keys.clone(), hint.action);
    actions.hints_update(&keys, action);
    true
}

fn start_hints(state: &mut WebCommandState, actions: &mut impl WebActions, action: WebHintAction) {
    state.set_mode(WebMode::Hint);
    state.hint = Some(WebHintState { action, keys: String::new() });
    actions.hints_start(action);
}

fn handle_mark_set(state: &mut WebCommandState, actions: &mut impl WebActions, text: &str) -> bool {
    let Some(name) = single_char(text) else {
        return true;
    };
    state.set_mode(WebMode::Normal);

    let Some(url) = actions.current_url() else {
        actions.push_error(String::from("No active URL for mark"));
        return true;
    };

    actions.request_mark_set(name, url);
    true
}

fn handle_mark_jump(
    state: &mut WebCommandState,
    actions: &mut impl WebActions,
    text: &str,
) -> bool {
    let Some(name) = single_char(text) else {
        return true;
    };
    state.set_mode(WebMode::Normal);

    let Some(mark) = state.marks.get(&name).cloned() else {
        actions.push_error(format!("Unknown mark: {name}"));
        return true;
    };

    if actions.current_url().as_deref() == Some(mark.url.as_str()) {
        actions.scroll_to(mark.scroll_x, mark.scroll_y);
    } else {
        state.pending_scroll = Some(WebPendingScroll {
            url: mark.url.clone(),
            scroll_x: mark.scroll_x,
            scroll_y: mark.scroll_y,
        });
        actions.open_url(mark.url);
    }

    true
}

fn handle_visual(state: &mut WebCommandState, actions: &mut impl WebActions, text: &str) -> bool {
    let Some(ch) = single_char(text) else {
        return true;
    };

    match ch {
        'y' => {
            actions.copy_selection();
            actions.clear_selection();
            state.set_mode(WebMode::Normal);
            return true;
        },
        'v' => {
            toggle_visual(state, actions, false);
            return true;
        },
        'V' => {
            toggle_visual(state, actions, true);
            return true;
        },
        _ => (),
    }

    let line_mode = matches!(state.mode, WebMode::VisualLine);
    let granularity = if line_mode { "line" } else { "character" };
    match ch {
        'h' => actions.visual_move("backward", granularity),
        'l' => actions.visual_move("forward", granularity),
        'k' => actions.visual_move("backward", "line"),
        'j' => actions.visual_move("forward", "line"),
        _ => (),
    }

    true
}

fn toggle_visual(state: &mut WebCommandState, actions: &mut impl WebActions, line_mode: bool) {
    let target = if line_mode { WebMode::VisualLine } else { WebMode::Visual };
    if state.mode == target {
        actions.clear_selection();
        state.set_mode(WebMode::Normal);
        return;
    }

    state.set_mode(target);
    actions.start_visual_selection();
}

fn toggle_help(state: &mut WebCommandState, actions: &mut impl WebActions) {
    if state.help_visible {
        actions.hide_help();
        state.help_visible = false;
    } else {
        actions.show_help();
        state.help_visible = true;
    }
}

fn single_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    enum ActionCall {
        ScrollBy(f64, f64),
        ScrollHalfPage(bool),
        ScrollTop,
        ScrollBottom,
        ScrollFarLeft,
        ScrollFarRight,
        ScrollTo(f64, f64),
        GoBack,
        GoForward,
        OpenCommandBar(String),
        StartFindPrompt,
        Find(String, bool),
        HintsStart(WebHintAction),
        HintsUpdate(String, WebHintAction),
        HintsCancel,
        CopySelection,
        ClearSelection,
        StartVisualSelection,
        VisualMove(String, String),
        FocusInput,
        BlurActiveElement,
        InsertText(String),
        DeleteBackward,
        DeleteForward,
        InsertParagraph,
        InsertTab,
        CaretMove(String, String),
        ViewSource,
        FollowRel(String),
        CopyUrl,
        OpenClipboard(bool),
        UpUrl(bool),
        NewTab,
        CloseTab,
        RestoreTab,
        SelectPreviousTab,
        SelectNextTab,
        SelectTabAtIndex(usize),
        SelectLastTab,
        Reload,
        ShowHelp,
        HideHelp,
        RequestMarkSet(char, String),
        OpenUrl(String),
        PushError(String),
    }

    #[derive(Default)]
    struct MockActions {
        calls: Vec<ActionCall>,
        current_url: Option<String>,
    }

    impl MockActions {
        fn last_call(&self) -> Option<&ActionCall> {
            self.calls.last()
        }
    }

    impl WebActions for MockActions {
        fn scroll_by(&mut self, dx: f64, dy: f64) {
            self.calls.push(ActionCall::ScrollBy(dx, dy));
        }

        fn scroll_half_page(&mut self, down: bool) {
            self.calls.push(ActionCall::ScrollHalfPage(down));
        }

        fn scroll_top(&mut self) {
            self.calls.push(ActionCall::ScrollTop);
        }

        fn scroll_bottom(&mut self) {
            self.calls.push(ActionCall::ScrollBottom);
        }

        fn scroll_far_left(&mut self) {
            self.calls.push(ActionCall::ScrollFarLeft);
        }

        fn scroll_far_right(&mut self) {
            self.calls.push(ActionCall::ScrollFarRight);
        }

        fn scroll_to(&mut self, x: f64, y: f64) {
            self.calls.push(ActionCall::ScrollTo(x, y));
        }

        fn go_back(&mut self) {
            self.calls.push(ActionCall::GoBack);
        }

        fn go_forward(&mut self) {
            self.calls.push(ActionCall::GoForward);
        }

        fn open_command_bar(&mut self, input: &str) {
            self.calls.push(ActionCall::OpenCommandBar(input.to_string()));
        }

        fn start_find_prompt(&mut self) {
            self.calls.push(ActionCall::StartFindPrompt);
        }

        fn find(&mut self, query: &str, backwards: bool) {
            self.calls.push(ActionCall::Find(query.to_string(), backwards));
        }

        fn hints_start(&mut self, action: WebHintAction) {
            self.calls.push(ActionCall::HintsStart(action));
        }

        fn hints_update(&mut self, keys: &str, action: WebHintAction) {
            self.calls.push(ActionCall::HintsUpdate(keys.to_string(), action));
        }

        fn hints_cancel(&mut self) {
            self.calls.push(ActionCall::HintsCancel);
        }

        fn copy_selection(&mut self) {
            self.calls.push(ActionCall::CopySelection);
        }

        fn clear_selection(&mut self) {
            self.calls.push(ActionCall::ClearSelection);
        }

        fn start_visual_selection(&mut self) {
            self.calls.push(ActionCall::StartVisualSelection);
        }

        fn visual_move(&mut self, direction: &str, granularity: &str) {
            self.calls.push(ActionCall::VisualMove(direction.to_string(), granularity.to_string()));
        }

        fn focus_input(&mut self) {
            self.calls.push(ActionCall::FocusInput);
        }

        fn blur_active_element(&mut self) {
            self.calls.push(ActionCall::BlurActiveElement);
        }

        fn insert_text(&mut self, text: &str) {
            self.calls.push(ActionCall::InsertText(text.to_string()));
        }

        fn delete_backward(&mut self) {
            self.calls.push(ActionCall::DeleteBackward);
        }

        fn delete_forward(&mut self) {
            self.calls.push(ActionCall::DeleteForward);
        }

        fn insert_paragraph(&mut self) {
            self.calls.push(ActionCall::InsertParagraph);
        }

        fn insert_tab(&mut self) {
            self.calls.push(ActionCall::InsertTab);
        }

        fn caret_move(&mut self, direction: &str, granularity: &str) {
            self.calls.push(ActionCall::CaretMove(direction.to_string(), granularity.to_string()));
        }

        fn view_source(&mut self) {
            self.calls.push(ActionCall::ViewSource);
        }

        fn follow_rel(&mut self, rel: &str) {
            self.calls.push(ActionCall::FollowRel(rel.to_string()));
        }

        fn copy_url(&mut self) {
            self.calls.push(ActionCall::CopyUrl);
        }

        fn open_clipboard(&mut self, new_tab: bool) {
            self.calls.push(ActionCall::OpenClipboard(new_tab));
        }

        fn up_url(&mut self, root: bool) {
            self.calls.push(ActionCall::UpUrl(root));
        }

        fn new_tab(&mut self) {
            self.calls.push(ActionCall::NewTab);
        }

        fn close_tab(&mut self) {
            self.calls.push(ActionCall::CloseTab);
        }

        fn restore_tab(&mut self) {
            self.calls.push(ActionCall::RestoreTab);
        }

        fn select_previous_tab(&mut self) {
            self.calls.push(ActionCall::SelectPreviousTab);
        }

        fn select_next_tab(&mut self) {
            self.calls.push(ActionCall::SelectNextTab);
        }

        fn select_tab_at_index(&mut self, index: usize) {
            self.calls.push(ActionCall::SelectTabAtIndex(index));
        }

        fn select_last_tab(&mut self) {
            self.calls.push(ActionCall::SelectLastTab);
        }

        fn reload(&mut self) {
            self.calls.push(ActionCall::Reload);
        }

        fn show_help(&mut self) {
            self.calls.push(ActionCall::ShowHelp);
        }

        fn hide_help(&mut self) {
            self.calls.push(ActionCall::HideHelp);
        }

        fn request_mark_set(&mut self, name: char, url: String) {
            self.calls.push(ActionCall::RequestMarkSet(name, url));
        }

        fn current_url(&mut self) -> Option<String> {
            self.current_url.clone()
        }

        fn open_url(&mut self, url: String) {
            self.calls.push(ActionCall::OpenUrl(url));
        }

        fn push_error(&mut self, message: String) {
            self.calls.push(ActionCall::PushError(message));
        }
    }

    fn press(state: &mut WebCommandState, actions: &mut MockActions, ch: char) {
        let mut text = String::new();
        text.push(ch);
        assert!(handle_key(state, actions, WebKey::Other, &text));
    }

    fn press_key(state: &mut WebCommandState, actions: &mut MockActions, key: WebKey) {
        assert!(handle_key(state, actions, key, ""));
    }

    #[test]
    fn navigation_commands() {
        let mut state = WebCommandState::default();
        let mut actions = MockActions::default();

        press(&mut state, &mut actions, 'j');
        assert_eq!(actions.last_call(), Some(&ActionCall::ScrollBy(0.0, WEB_SCROLL_STEP)));
        press(&mut state, &mut actions, 'k');
        assert_eq!(actions.last_call(), Some(&ActionCall::ScrollBy(0.0, -WEB_SCROLL_STEP)));
        press(&mut state, &mut actions, 'h');
        assert_eq!(actions.last_call(), Some(&ActionCall::ScrollBy(-WEB_SCROLL_STEP, 0.0)));
        press(&mut state, &mut actions, 'l');
        assert_eq!(actions.last_call(), Some(&ActionCall::ScrollBy(WEB_SCROLL_STEP, 0.0)));

        press(&mut state, &mut actions, 'd');
        assert_eq!(actions.last_call(), Some(&ActionCall::ScrollHalfPage(true)));
        press(&mut state, &mut actions, 'u');
        assert_eq!(actions.last_call(), Some(&ActionCall::ScrollHalfPage(false)));

        press(&mut state, &mut actions, 'G');
        assert_eq!(actions.last_call(), Some(&ActionCall::ScrollBottom));

        press(&mut state, &mut actions, 'g');
        press(&mut state, &mut actions, 'g');
        assert_eq!(actions.last_call(), Some(&ActionCall::ScrollTop));

        press(&mut state, &mut actions, 'z');
        press(&mut state, &mut actions, 'H');
        assert_eq!(actions.last_call(), Some(&ActionCall::ScrollFarLeft));
        press(&mut state, &mut actions, 'z');
        press(&mut state, &mut actions, 'L');
        assert_eq!(actions.last_call(), Some(&ActionCall::ScrollFarRight));
    }

    #[test]
    fn link_and_input_commands() {
        let mut state = WebCommandState::default();
        let mut actions = MockActions::default();

        press(&mut state, &mut actions, 'f');
        assert_eq!(state.mode, WebMode::Hint);
        assert_eq!(actions.last_call(), Some(&ActionCall::HintsStart(WebHintAction::Open)));

        state = WebCommandState::default();
        press(&mut state, &mut actions, 'F');
        assert_eq!(state.mode, WebMode::Hint);
        assert_eq!(actions.last_call(), Some(&ActionCall::HintsStart(WebHintAction::OpenNewTab)));

        state = WebCommandState::default();
        press(&mut state, &mut actions, 'y');
        press(&mut state, &mut actions, 'f');
        assert_eq!(state.mode, WebMode::Hint);
        assert_eq!(actions.last_call(), Some(&ActionCall::HintsStart(WebHintAction::CopyLink)));

        state = WebCommandState::default();
        press(&mut state, &mut actions, 'g');
        press(&mut state, &mut actions, 'i');
        assert_eq!(state.mode, WebMode::Insert);
        assert_eq!(actions.last_call(), Some(&ActionCall::FocusInput));
    }

    #[test]
    fn find_and_visual_commands() {
        let mut state = WebCommandState::default();
        let mut actions = MockActions::default();

        press(&mut state, &mut actions, '/');
        assert_eq!(actions.last_call(), Some(&ActionCall::StartFindPrompt));

        state = WebCommandState::default();
        press(&mut state, &mut actions, 'n');
        assert_eq!(
            actions.last_call(),
            Some(&ActionCall::PushError(String::from("No active search")))
        );

        state = WebCommandState::default();
        state.last_find = Some(String::from("needle"));
        press(&mut state, &mut actions, 'n');
        assert_eq!(actions.last_call(), Some(&ActionCall::Find(String::from("needle"), false)));
        press(&mut state, &mut actions, 'N');
        assert_eq!(actions.last_call(), Some(&ActionCall::Find(String::from("needle"), true)));

        state = WebCommandState::default();
        press(&mut state, &mut actions, 'v');
        assert_eq!(state.mode, WebMode::Visual);
        assert_eq!(actions.last_call(), Some(&ActionCall::StartVisualSelection));
        press(&mut state, &mut actions, 'y');
        assert_eq!(state.mode, WebMode::Normal);
        assert_eq!(actions.last_call(), Some(&ActionCall::ClearSelection));

        state = WebCommandState::default();
        press(&mut state, &mut actions, 'V');
        assert_eq!(state.mode, WebMode::VisualLine);
        assert_eq!(actions.last_call(), Some(&ActionCall::StartVisualSelection));
    }

    #[test]
    fn history_and_url_commands() {
        let mut state = WebCommandState::default();
        let mut actions = MockActions::default();

        press(&mut state, &mut actions, 'H');
        assert_eq!(actions.last_call(), Some(&ActionCall::GoBack));
        press(&mut state, &mut actions, 'L');
        assert_eq!(actions.last_call(), Some(&ActionCall::GoForward));

        press(&mut state, &mut actions, 'y');
        press(&mut state, &mut actions, 'y');
        assert_eq!(actions.last_call(), Some(&ActionCall::CopyUrl));

        press(&mut state, &mut actions, 'p');
        assert_eq!(actions.last_call(), Some(&ActionCall::OpenClipboard(false)));
        press(&mut state, &mut actions, 'P');
        assert_eq!(actions.last_call(), Some(&ActionCall::OpenClipboard(true)));

        press(&mut state, &mut actions, 'g');
        press(&mut state, &mut actions, 'u');
        assert_eq!(actions.last_call(), Some(&ActionCall::UpUrl(false)));
        press(&mut state, &mut actions, 'g');
        press(&mut state, &mut actions, 'U');
        assert_eq!(actions.last_call(), Some(&ActionCall::UpUrl(true)));
    }

    #[test]
    fn tabs_and_omnibar_commands() {
        let mut state = WebCommandState::default();
        let mut actions = MockActions::default();

        press(&mut state, &mut actions, 't');
        assert_eq!(actions.last_call(), Some(&ActionCall::NewTab));
        press(&mut state, &mut actions, 'x');
        assert_eq!(actions.last_call(), Some(&ActionCall::CloseTab));
        press(&mut state, &mut actions, 'X');
        assert_eq!(actions.last_call(), Some(&ActionCall::RestoreTab));

        press(&mut state, &mut actions, 'J');
        assert_eq!(actions.last_call(), Some(&ActionCall::SelectPreviousTab));
        press(&mut state, &mut actions, 'K');
        assert_eq!(actions.last_call(), Some(&ActionCall::SelectNextTab));

        press(&mut state, &mut actions, 'g');
        press(&mut state, &mut actions, '0');
        assert_eq!(actions.last_call(), Some(&ActionCall::SelectTabAtIndex(0)));
        press(&mut state, &mut actions, 'g');
        press(&mut state, &mut actions, '$');
        assert_eq!(actions.last_call(), Some(&ActionCall::SelectLastTab));

        press(&mut state, &mut actions, 'o');
        assert_eq!(actions.last_call(), Some(&ActionCall::OpenCommandBar(String::from("o "))));
        press(&mut state, &mut actions, 'O');
        assert_eq!(actions.last_call(), Some(&ActionCall::OpenCommandBar(String::from("O "))));
        press(&mut state, &mut actions, 'b');
        assert_eq!(actions.last_call(), Some(&ActionCall::OpenCommandBar(String::from("b "))));
        press(&mut state, &mut actions, 'B');
        assert_eq!(actions.last_call(), Some(&ActionCall::OpenCommandBar(String::from("B "))));
        press(&mut state, &mut actions, 'T');
        assert_eq!(actions.last_call(), Some(&ActionCall::OpenCommandBar(String::from("T "))));
    }

    #[test]
    fn misc_commands() {
        let mut state = WebCommandState::default();
        let mut actions = MockActions::default();

        press(&mut state, &mut actions, 'r');
        assert_eq!(actions.last_call(), Some(&ActionCall::Reload));

        press(&mut state, &mut actions, 'g');
        press(&mut state, &mut actions, 's');
        assert_eq!(actions.last_call(), Some(&ActionCall::ViewSource));

        press(&mut state, &mut actions, '[');
        press(&mut state, &mut actions, '[');
        assert_eq!(actions.last_call(), Some(&ActionCall::FollowRel(String::from("prev"))));
        press(&mut state, &mut actions, ']');
        press(&mut state, &mut actions, ']');
        assert_eq!(actions.last_call(), Some(&ActionCall::FollowRel(String::from("next"))));

        state = WebCommandState::default();
        actions.current_url = Some(String::from("https://example.com"));
        press(&mut state, &mut actions, 'm');
        press(&mut state, &mut actions, 'a');
        assert_eq!(
            actions.last_call(),
            Some(&ActionCall::RequestMarkSet('a', String::from("https://example.com")))
        );

        state = WebCommandState::default();
        state.set_mark('a', String::from("https://example.com"), 10.0, 20.0);
        actions.current_url = Some(String::from("https://example.com"));
        press(&mut state, &mut actions, '`');
        press(&mut state, &mut actions, 'a');
        assert_eq!(actions.last_call(), Some(&ActionCall::ScrollTo(10.0, 20.0)));

        state = WebCommandState::default();
        state.set_mark('a', String::from("https://example.com"), 1.0, 2.0);
        actions.current_url = Some(String::from("https://other.com"));
        press(&mut state, &mut actions, '`');
        press(&mut state, &mut actions, 'a');
        assert_eq!(
            actions.last_call(),
            Some(&ActionCall::OpenUrl(String::from("https://example.com")))
        );
        assert!(state.pending_scroll.is_some());

        state = WebCommandState::default();
        press(&mut state, &mut actions, '?');
        assert_eq!(actions.last_call(), Some(&ActionCall::ShowHelp));
        assert!(state.help_visible);
        press_key(&mut state, &mut actions, WebKey::Escape);
        assert_eq!(actions.last_call(), Some(&ActionCall::HideHelp));
        assert!(!state.help_visible);
    }
}
