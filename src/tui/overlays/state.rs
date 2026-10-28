//! What each overlay is, and how it answers a key press.
//!
//! No rendering and no tmux: every type here is a small state machine returning a
//! [`Response`] for the app to act on, which is what makes the whole overlay layer
//! testable without a terminal.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::KeysPreset;
use crate::store::AgentState;
use crate::tui::actions::Action;

use crate::tui::app;
use crate::tui::input::Input;
use crate::tui::keys::{Action as KeyAction, Keymap};
use crate::tui::picker::ProjectPicker;

/// What the app should do after an overlay handled a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    /// Nothing to do; the overlay stays open.
    Stay,
    /// Close the overlay without doing anything.
    Close,
    /// Close the overlay and run this action.
    Run(Action),
    /// A prompt was submitted with this text. The app validates it, then either runs
    /// something and closes, or writes an error back into the prompt.
    Submit(String),
    /// Close the overlay and do this to the cursor row. A menu entry cannot carry the
    /// finished action: what `close` means depends on the row, and a prompt has to be
    /// prefilled from — and validated against — the snapshot. Both are the app's job.
    Row(RowAction),
}

/// What a menu entry does to the cursor row: exactly the work the key of the same name
/// does, so a menu can never drift from the keymap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowAction {
    /// Go to the row: jump to a pane, switch to a session, open a closed worktree.
    Go,
    /// Rename: the chooser on an agent row, the window on a plain pane, the session on a
    /// header.
    Rename,
    /// `rename-window` on the row's window.
    RenameWindow,
    /// `@agent_name` on the row's agent (herdr's agent label, and the CLI handle).
    NameAgent,
    /// Close: `kill-pane`, `kill-session`, or `git worktree remove`.
    Close,
    /// A new worktree cut from the row's session.
    NewWorktree,
    /// The options overlay.
    Options,
    /// The clipboard's image into the row's agent: what `ctrl-v` does in the default preset.
    PasteImage,
}

/// What the search preset's help says of the paste-image key once `[keys]` binds it.
pub const PASTE_IMAGE_HELP: &str = "paste the clipboard's image into the agent";

/// What the default preset's help says on the row of rename, close and the paste-image key (`ctrl-v`: the
/// clipboard's image into the agent).
pub const PASTE_IMAGE_ROW_HELP: &str = "rename  \u{b7}  close  \u{b7}  paste image";

/// What an agent row's right-click menu calls the paste-image entry.
pub const PASTE_IMAGE_ENTRY: &str = "paste image";

/// What both presets' help says of the right-click menu (a shell row's menu has no paste-image entry).
pub const MENU_HELP: &str = "menu: go, rename, close, new worktree, paste image";

/// The overlay currently covering the sidebar, if any.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Overlay {
    #[default]
    None,
    Help(Scroll),
    /// A read-only dump of the effective configuration, built once when opened.
    Settings(Scroll, String),
    /// The "new session" picker: a path to open or create a workspace from. Boxed
    /// because it is by far the largest overlay, and every `Overlay` — including the
    /// `None` the popup holds nearly all the time — would otherwise be its size.
    Project(Box<ProjectPicker>),
    Prompt(Prompt),
    Confirm(Confirm),
    Menu(Menu),
}

impl Overlay {
    pub fn is_open(&self) -> bool {
        !matches!(self, Overlay::None)
    }

    /// Whether the open overlay is a text field: the rename and branch prompts, and the
    /// project picker. The others take single keys.
    pub fn takes_text(&self) -> bool {
        matches!(self, Overlay::Prompt(_) | Overlay::Project(_))
    }

    /// Feeds a key to whichever overlay is open. `Esc` always closes.
    pub fn on_key(&mut self, key: KeyEvent) -> Response {
        if key.code == KeyCode::Esc {
            return Response::Close;
        }
        match self {
            Overlay::None => Response::Stay,
            Overlay::Help(scroll) | Overlay::Settings(scroll, _) => {
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Enter) {
                    return Response::Close;
                }
                scroll.on_key(key);
                Response::Stay
            }
            Overlay::Project(picker) => picker.on_key(key),
            Overlay::Prompt(prompt) => prompt.on_key(key),
            Overlay::Confirm(confirm) => confirm.on_key(key),
            Overlay::Menu(menu) => menu.on_key(key),
        }
    }

    /// Moves the selection of a list-shaped overlay, for the mouse wheel.
    pub fn scroll(&mut self, down: bool) {
        match self {
            Overlay::Help(scroll) | Overlay::Settings(scroll, _) => scroll.by(down),
            Overlay::Project(picker) => picker.move_by(if down { 1 } else { -1 }),
            Overlay::Menu(menu) => menu.move_by(if down { 1 } else { -1 }),
            _ => {}
        }
    }

    /// Selects the overlay's item at index `index` (a mouse click), returning the action
    /// when that click also activates it (menus and pickers activate on click).
    pub fn click_item(&mut self, index: usize) -> Response {
        match self {
            // The project picker only highlights: its outcome line is what the click was
            // for, and opening on one click would skip reading it.
            Overlay::Project(picker) => {
                picker.select(index);
                Response::Stay
            }
            Overlay::Menu(menu) => {
                menu.select(index);
                menu.activate()
            }
            _ => Response::Stay,
        }
    }

    /// The key hints for the open overlay, shown on the popup's bottom line.
    pub fn hints(&self) -> &'static str {
        match self {
            Overlay::None => "",
            Overlay::Help(_) | Overlay::Settings(_, _) => "j/k scroll  ·  Esc close",
            Overlay::Project(_) => {
                "type a path or project  ·  Tab complete  ·  Up/Down choose  ·  Enter create or switch  ·  Esc cancel"
            }
            Overlay::Prompt(_) => "Enter confirm  ·  Esc cancel",
            Overlay::Confirm(confirm) => confirm.answers.hint(),
            Overlay::Menu(_) => "a letter runs it  ·  j/k move  ·  Enter choose  ·  Esc cancel",
        }
    }
}

/// A scroll offset for the text overlays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scroll(pub u16);

impl Scroll {
    fn on_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.by(true),
            KeyCode::Char('k') | KeyCode::Up => self.by(false),
            KeyCode::PageDown => self.0 = self.0.saturating_add(10),
            KeyCode::PageUp => self.0 = self.0.saturating_sub(10),
            KeyCode::Home => self.0 = 0,
            _ => {}
        }
    }

    fn by(&mut self, down: bool) {
        self.0 = if down {
            self.0.saturating_add(1)
        } else {
            self.0.saturating_sub(1)
        };
    }
}

/// One entry of a lettered menu: the key that runs it, what it says, and what it does.
///
/// A menu is a list of entries, and pressing an entry's key runs it
/// straight away; `key` is `None` for an entry reachable only by
/// moving to it and pressing Enter (the context list's `go`, whose key is Enter itself).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub key: Option<char>,
    pub label: String,
    pub choice: RowAction,
}

impl Item {
    /// An entry `key` runs.
    pub fn new(key: char, label: impl Into<String>, choice: RowAction) -> Item {
        Item {
            key: Some(key),
            label: label.into(),
            choice,
        }
    }

    /// An entry with no key of its own: Enter (or a click) on the highlight runs it.
    pub fn plain(label: impl Into<String>, choice: RowAction) -> Item {
        Item {
            key: None,
            label: label.into(),
            choice,
        }
    }

    fn chosen(&self) -> Response {
        Response::Row(self.choice)
    }
}

/// What a [`Prompt`] is asking for, so the app knows what to do with the answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptKind {
    /// A branch for a new worktree cut from `workspace`'s repository.
    NewWorktree { workspace: String },
    /// herdr's agent label (`@agent_name`) on a pane.
    RenameAgent { pane: String },
    /// `rename-window` on a window id.
    RenameWindow { window: String },
    /// `rename-session` on a session id.
    RenameSession { session: String },
}

/// A one-line text prompt: a rename, or a branch for a new worktree.
///
/// The line is an [`Input`], the same one the filter and the project picker edit through,
/// so every readline chord works here: prompts (rename, branch) with readline
/// editing and inline validation errors.
#[derive(Clone, Debug)]
pub struct Prompt {
    pub kind: PromptKind,
    pub title: String,
    pub input: Input,
    /// Set by the app when a submitted value was rejected.
    pub error: Option<String>,
}

/// `Input` has no `PartialEq` (it wraps `tui_input`'s, which has none either), so the
/// comparison is over what the input *is*: its text and where the cursor sits — the same
/// shape [`ProjectPicker`] compares by.
impl PartialEq for Prompt {
    fn eq(&self, other: &Prompt) -> bool {
        self.kind == other.kind
            && self.title == other.title
            && self.input.value() == other.input.value()
            && self.input.cursor() == other.input.cursor()
            && self.error == other.error
    }
}

impl Eq for Prompt {}

impl Prompt {
    pub fn new(kind: PromptKind, title: impl Into<String>, initial: impl Into<String>) -> Prompt {
        Prompt {
            kind,
            title: title.into(),
            input: Input::new(&initial.into()),
            error: None,
        }
    }

    /// Enter submits the line, trimmed; `Esc` never arrives here ([`Overlay::on_key`]
    /// closes on it first). Everything else is offered to the input, and a key it takes
    /// clears a rejected value's error — the user is already fixing it, and the next Enter
    /// says so again if they have not.
    fn on_key(&mut self, key: KeyEvent) -> Response {
        if key.code == KeyCode::Enter {
            return Response::Submit(self.input.value().trim().to_string());
        }
        if self.input.handle(key) {
            self.error = None;
        }
        Response::Stay
    }
}

/// A yes/no question guarding a destructive action (`[ui] confirm_close`), or a question the
/// action raised once it ran (a removal git refused).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    pub message: String,
    pub action: Action,
    /// Which keys answer it, and what each one means.
    pub answers: Answers,
}

/// How a [`Confirm`] is answered. Esc always cancels, before any of this is asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answers {
    /// `y` or Enter runs the action; `n` and `q` cancel. Every ordinary confirmation.
    Plain,
    /// `y` runs the action; Enter, `n` and `q` all cancel. For what loses work: nothing runs
    /// on a stray Enter, and the question says so with its capital (`y / N`).
    Strict,
    /// `y` runs the action; Enter and `n` run `otherwise`; `q` cancels. A question with two
    /// ways to go on (`y / N`, where `N` is the smaller of them).
    Choice { otherwise: Action },
}

impl Answers {
    /// What the popup's bottom line says answers a confirmation of this kind.
    pub fn hint(&self) -> &'static str {
        match self {
            Answers::Plain => "y or Enter confirm  \u{b7}  n or Esc cancel",
            // The no-break spaces keep `n, Enter or Esc` one key where the hint line splits a
            // hint at its first space; it draws them as spaces.
            Answers::Strict => "y confirm  \u{b7}  n,\u{a0}Enter\u{a0}or\u{a0}Esc cancel",
            Answers::Choice { .. } => {
                "y close them too  \u{b7}  n or Enter just this one  \u{b7}  Esc cancel"
            }
        }
    }

    /// The answers as the box under the question draws them: a capital is the default.
    pub fn line(&self) -> &'static str {
        match self {
            Answers::Plain => "y / n",
            Answers::Strict | Answers::Choice { .. } => "y / N",
        }
    }
}

impl Confirm {
    /// An ordinary confirmation: `y` or Enter runs `action`.
    pub fn new(message: impl Into<String>, action: Action) -> Confirm {
        Confirm {
            message: message.into(),
            action,
            answers: Answers::Plain,
        }
    }

    /// A question only `y` says yes to.
    pub fn strict(message: impl Into<String>, action: Action) -> Confirm {
        Confirm {
            message: message.into(),
            action,
            answers: Answers::Strict,
        }
    }

    /// A question whose `y` runs `action` and whose Enter or `n` runs `otherwise`.
    pub fn choice(message: impl Into<String>, action: Action, otherwise: Action) -> Confirm {
        Confirm {
            message: message.into(),
            action,
            answers: Answers::Choice { otherwise },
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Response {
        let yes = matches!(key.code, KeyCode::Char('y' | 'Y'));
        let no = matches!(key.code, KeyCode::Char('n' | 'N'));
        let enter = key.code == KeyCode::Enter;
        let quit = key.code == KeyCode::Char('q');
        match &self.answers {
            Answers::Plain if yes || enter => Response::Run(self.action.clone()),
            Answers::Plain if no || quit => Response::Close,
            Answers::Strict if yes => Response::Run(self.action.clone()),
            Answers::Strict if no || enter || quit => Response::Close,
            Answers::Choice { .. } if yes => Response::Run(self.action.clone()),
            Answers::Choice { otherwise } if no || enter => Response::Run(otherwise.clone()),
            Answers::Choice { .. } if quit => Response::Close,
            _ => Response::Stay,
        }
    }
}

/// A lettered menu: the rename chooser, and the right-click context list.
///
/// Every entry carries the key that runs it and pressing that key runs it immediately —
/// the key is looked up before anything else, so `w` in
/// the rename menu is one keystroke, not "move to it, then Enter". `j`/`k` and Enter still
/// work, for the mouse and for an entry with no key of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Menu {
    /// What the menu is asking, e.g. `rename what? api:2:zsh.1`.
    pub title: String,
    pub items: Vec<Item>,
    pub sel: usize,
    /// Where the click was, so the menu can be drawn under the pointer.
    pub anchor: (u16, u16),
}

impl Menu {
    pub fn new(title: impl Into<String>, items: Vec<Item>, anchor: (u16, u16)) -> Menu {
        Menu {
            title: title.into(),
            items,
            sel: 0,
            anchor,
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Response {
        // An entry's own letter wins over the movement keys: a menu offering `k` runs it
        // rather than moving up, which is what the labels promise.
        if let KeyCode::Char(c) = key.code
            && let Some(item) = self.items.iter().find(|i| i.key == Some(c))
        {
            return item.chosen();
        }
        match key.code {
            KeyCode::Enter => self.activate(),
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => {
                self.move_by(1);
                Response::Stay
            }
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => {
                self.move_by(-1);
                Response::Stay
            }
            KeyCode::Char('q') => Response::Close,
            _ => Response::Stay,
        }
    }

    fn activate(&self) -> Response {
        match self.items.get(self.sel) {
            Some(item) => item.chosen(),
            None => Response::Stay,
        }
    }

    fn move_by(&mut self, delta: isize) {
        self.sel = app::step(self.sel, self.items.len(), delta);
    }

    fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.sel = index;
        }
    }
}

/// One line of the help overlay.
///
/// The overlay is data, not text: a key line names the [`KeyAction`]s it runs rather than
/// the keys themselves, so a `[keys]` override shows its own key and the help cannot drift
/// from the keymap. Rendering (`super::render`) is what turns this into coloured columns.
pub enum Help {
    /// A group title: `go`, `act on the row`, `legend`.
    Group(&'static str),
    /// Keys from the keymap: within a group joined by ` / `, groups by three spaces:
    /// `j / k   J / K`.
    Keys(&'static [&'static [KeyAction]], &'static str),
    /// Two actions that are one line, like [`Help::Keys`] with one group of two, whose other
    /// keys the meaning names after it, in brackets, as the keymap binds them: the search
    /// preset's `Down / Up` line ends `(also ctrl-j / ctrl-k, ctrl-n / ctrl-p)` with the
    /// preset's keys ([`also`]). `alone` is the word for a key of one action that no key of
    /// the other pairs with: `next`, `previous`.
    Pair {
        of: [KeyAction; 2],
        what: &'static str,
        alone: [&'static str; 2],
    },
    /// A key line the keymap cannot name: typing itself, the mouse.
    Literal(&'static str, &'static str),
    /// The tmux prefix chord for what a [`Help::Keys`] line just above it does from the
    /// popup, so the two read as one action. While that line is shown the
    /// chord says it is the same (`same`); when `[keys]` leaves its actions no key, so the
    /// line is dropped, it says the whole thing itself (`alone`). `of` is that line's actions.
    Chord {
        keys: &'static str,
        of: &'static [&'static [KeyAction]],
        same: &'static str,
        alone: &'static str,
    },
    /// A legend line whose glyph is an agent state's, drawn in that state's colour.
    /// `None` is a session with no agents in it.
    State(Option<AgentState>, &'static str, &'static str),
    /// A legend line with a fixed glyph: a plain pane, a closed worktree.
    Glyph(&'static str, &'static str, &'static str),
    /// A legend line with no glyph at all — what a target like `2:zsh.1` means.
    Note(&'static str, &'static str),
    /// The key line for the number keys: the keys `[keys] select_session` binds, as the hint line
    /// spells them (`1-9`), and what they do. Dropped while nothing is bound, as the search
    /// preset ships, like a [`Help::Keys`] line whose actions have no key.
    NumberKeys(&'static str),
    /// The modal preset's key line for the letters, spelled as the hint line spells them
    /// ([`Keymap::letters_hint`]: `a-e`, or `a b d e` while a `[keys]` entry takes `c`), and
    /// beside them a row's address as [`Help::TypedRow`] names it (`a-e   4a`); its words are
    /// these, then what the address reaches, in the search too ([`TYPED_ROW_IN_SEARCH`]). One
    /// line rather than two, so the whole help still fits a 150-column, 39-row client. Drawn only
    /// while the letters work, `[ui] labels` on, as the number keys' line is only while the
    /// numbers do, and dropped when `[keys]` takes every letter.
    LetterKeys(&'static str),
    /// The search preset's line for reaching a row by typing its address, so `4a` reaches row a
    /// of session 4 in the search too: its key column is session 4's number and the first letter that can name a row (`4a`, [`Keymap::label`] and
    /// [`Keymap::free_letters`]), its words `row a of session 4: ` and then these. Drawn only
    /// while the letters work, as [`Help::LetterKeys`] is, and dropped when `[keys]` takes every
    /// letter.
    TypedRow(&'static str),
    /// The legend's line for a session's number, its subject drawn as the tree draws the numbers
    /// right now: in the key colour while their keys work, dim while they do not. Its words are
    /// the first while they work and the second while they do not ([`Help::number_words`]).
    Number(&'static str, &'static str, &'static str),
    /// The line under Enter's, with no key of its own, about Enter on a row of another machine: drawn only in the
    /// popup, and only while rows of other machines are listed. Its words name the key that brings you back
    /// ([`crate::machine::words::enter_on_machine_help`]), which the keymap decides.
    Machines,
    Blank,
}

/// The key column of one [`Help::Keys`] line, as the keymap actually binds it. `None` when
/// nothing is bound to any of its actions, which is what drops the line entirely.
pub fn help_keys(groups: &[&[KeyAction]], keymap: &Keymap) -> Option<String> {
    let text = groups
        .iter()
        .filter_map(|group| {
            let keys: Vec<String> = group.iter().filter_map(|&a| keymap.short_key(a)).collect();
            (!keys.is_empty()).then(|| keys.join(" / "))
        })
        .collect::<Vec<_>>()
        .join("   ");
    (!text.is_empty()).then_some(text)
}

/// What a [`Help::Pair`] line adds to its meaning: every key bound to either action but the
/// one its key column names, in brackets. A key of the first action and one of the second go
/// together as `a / b`, in the order [`Keymap::keys_for`] sorts them, and the keys one action
/// has left over after that are named with that action's word: `(also ctrl-j / ctrl-k, ctrl-n
/// for next)`. `None` when neither action has another key.
pub fn also(of: [KeyAction; 2], alone: [&str; 2], keymap: &Keymap) -> Option<String> {
    let rest = |action: KeyAction| -> Vec<String> {
        let named = keymap.short_key(action);
        keymap
            .keys_for(action)
            .into_iter()
            .filter(|key| Some(key) != named.as_ref())
            .collect()
    };
    let (first, second) = (rest(of[0]), rest(of[1]));
    let mut parts: Vec<String> = first
        .iter()
        .zip(&second)
        .map(|(a, b)| format!("{a} / {b}"))
        .collect();
    let paired = parts.len();
    for (keys, word) in [(&first, alone[0]), (&second, alone[1])] {
        if keys.len() > paired {
            parts.push(format!("{} for {word}", keys[paired..].join(" and ")));
        }
    }
    (!parts.is_empty()).then(|| format!("(also {})", parts.join(", ")))
}

impl Help {
    /// What the legend's number line says beside its 3: that the number puts the cursor on its
    /// session while the number keys `work` (`[ui] labels` on and a key bound, as
    /// [`App::numbers_work`](crate::tui::app::App) has it), and only how the sessions are
    /// numbered while they do not (the search preset as it ships, or labels off). `None` for any
    /// other line.
    pub fn number_words(&self, work: bool) -> Option<&'static str> {
        match self {
            Help::Number(_, keyed, label) => Some(if work { keyed } else { label }),
            _ => None,
        }
    }

    /// The key column and the meaning of a key line as `keymap` binds it: what a
    /// [`Help::Keys`], [`Help::Pair`], [`Help::Literal`], [`Help::Chord`], [`Help::NumberKeys`],
    /// [`Help::LetterKeys`] or [`Help::TypedRow`] says. `None` for anything else, and for a
    /// `Keys`, `Pair`, `NumberKeys`, `LetterKeys` or `TypedRow` line nothing is bound to (no
    /// letter left, for the letters), which is what drops it.
    pub fn key_line(&self, keymap: &Keymap) -> Option<(String, String)> {
        match self {
            Help::NumberKeys(what) => Some((keymap.select_session_hint()?, (*what).to_string())),
            Help::LetterKeys(what) => {
                let letters = keymap.letters_hint()?;
                // With no address a query can hold, the letters' own line alone.
                Some(match typed_address(keymap) {
                    Some((address, letter)) => (
                        format!("{letters}   {address}"),
                        format!("{what}  \u{b7}  row {letter} of session 4, {TYPED_ROW_IN_SEARCH}"),
                    ),
                    None => (letters, (*what).to_string()),
                })
            }
            Help::TypedRow(what) => {
                let (address, letter) = typed_address(keymap)?;
                Some((address, format!("row {letter} of session 4: {what}")))
            }
            Help::Keys(groups, what) => Some((help_keys(groups, keymap)?, (*what).to_string())),
            Help::Pair { of, what, alone } => {
                let keys = help_keys(&[&of[..]], keymap)?;
                Some(match also(*of, *alone, keymap) {
                    Some(also) => (keys, format!("{what} {also}")),
                    None => (keys, (*what).to_string()),
                })
            }
            Help::Literal(keys, what) => Some(((*keys).to_string(), (*what).to_string())),
            Help::Chord {
                keys,
                of,
                same,
                alone,
            } => {
                let what = if help_keys(of, keymap).is_some() {
                    same
                } else {
                    alone
                };
                Some(((*keys).to_string(), (*what).to_string()))
            }
            _ => None,
        }
    }
}

/// `entries` (the help of `preset`) as `taw app` says it: Enter goes in, the quit keys go back in where you
/// were (the app ends when there was nowhere), and `Q` or Ctrl-C ends the app at once. The keys are the ones
/// `keymap` binds: a `[keys]` entry for `quit` names its own, and a `Q` that `[keys]` gave to another action is
/// not named. In the search preset `Q` is text for the search, so only Ctrl-C is named there. An app the doorway
/// started (`back_to_tmux`) says that those keys go back to tmux, which it does when it ends.
pub fn app_help(
    entries: Vec<Help>,
    preset: KeysPreset,
    keymap: &Keymap,
    back_to_tmux: bool,
) -> Vec<Help> {
    let mut out = Vec::with_capacity(entries.len() + 2);
    for entry in entries {
        match entry {
            Help::Keys(groups, _) if groups == [&[KeyAction::Go][..]] => {
                out.push(Help::Keys(groups, "go in to the row under the cursor"));
                // The quit action's own line, said here and dropped where the popup has it: by Esc
                // where Esc is a quit key (with the modal `q` beside it when that is
                // the only other), else by the key the keymap names it by.
                let back = if back_to_tmux {
                    "back in where you came from; with none, back to tmux"
                } else {
                    "back in where you came from; with none, quit"
                };
                let keys = keymap.keys_for(KeyAction::Quit);
                out.push(
                    match keys.iter().map(String::as_str).collect::<Vec<_>>()[..] {
                        ["Esc"] => Help::Literal("Esc", back),
                        ["Esc", "q"] => Help::Literal("Esc / q", back),
                        _ => Help::Keys(&[&[KeyAction::Quit]], back),
                    },
                );
                let capital_q = keymap
                    .lookup(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT))
                    .is_none();
                let at_once = if back_to_tmux {
                    crate::tui::app::BACK_TO_TMUX
                } else {
                    "quit at once"
                };
                out.push(match preset {
                    KeysPreset::Modal if capital_q => Help::Literal("Q / Ctrl-C", at_once),
                    _ => Help::Literal("Ctrl-C", at_once),
                });
            }
            // Said above, and the search preset's own quit line would say it twice.
            Help::Keys(groups, _) if groups == [&[KeyAction::Quit][..]] => {}
            // The other words for ending the app say where it ends up, in an app the doorway started.
            Help::Literal(keys, "quit") if back_to_tmux => {
                out.push(Help::Literal(keys, "back to tmux"));
            }
            Help::Literal(keys, "options  \u{b7}  this help  \u{b7}  quit") if back_to_tmux => {
                out.push(Help::Literal(
                    keys,
                    "options  \u{b7}  this help  \u{b7}  back to tmux",
                ));
            }
            Help::Keys(groups, "options  \u{b7}  this help  \u{b7}  quit") if back_to_tmux => {
                out.push(Help::Keys(
                    groups,
                    "options  \u{b7}  this help  \u{b7}  back to tmux",
                ));
            }
            other => out.push(other),
        }
    }
    out
}

/// The help overlay's groups, in this order: how to move, how to go,
/// what the keys do to the row the cursor is on, the mouse, and what every glyph means.
///
/// The two presets say the same things about different keys, and the search preset has a
/// few of its own (typing filters), so each gets its own list; the mouse and the legend
/// are shared. The modal list is written for someone new: one
/// thing to a line, said in words. Both fit one screen: with the presets' own keys no
/// line is wider than 98 columns, so a popup 113 columns wide shows every one of them
/// whole, and the modal help is at most 40 lines.
pub fn help_text(preset: KeysPreset) -> Vec<Help> {
    help_text_for(preset, false)
}

/// The chord's help line sits right under the next up line and says it is
/// the same action, outside the popup. When `[keys]` leaves next up no key that line is gone,
/// which is how the search preset ships, and the chord's says what next up walks (what needs
/// you, else every agent), in a line that fits the 98 columns of every
/// other one.
const NEXT_UP_CHORD_HELP: &str =
    "the same next up from anywhere in tmux, straight there without the popup";
const NEXT_UP_CHORD_ALONE_HELP: &str =
    "next up from anywhere in tmux: what needs you, else every agent";

/// What the number keys do, in the help's key line for them (both presets, [`Help::NumberKeys`]):
/// the one line that says what yellow means in the popup.
const NUMBER_KEYS_HELP: &str = "jump to that session (yellow: a key that works now)";

/// Session 4's row address as the tree draws it, its number and the first letter that can name
/// a row (`4a`), with that letter: `None` when `[keys]` takes every letter, or when the number
/// is drawn as a key's name a query cannot hold (`ctrl-4`, [`Keymap::typed_label`]).
fn typed_address(keymap: &Keymap) -> Option<(String, char)> {
    let letter = *keymap.free_letters().first()?;
    Some((format!("{}{letter}", keymap.typed_label(4)?), letter))
}

/// What the modal preset's letters do, on their line before the address ([`Help::LetterKeys`]).
const LETTER_KEYS_HELP: &str = "jump to a row of your session";

/// The modal preset's letters line ends `row a of session 4, ` and then this: the same two keys
/// reach it in the search (`/`) too.
const TYPED_ROW_IN_SEARCH: &str = "in the search too";

/// How to reach a row in the search preset, after `row a of session 4: ` ([`Help::TypedRow`]):
/// typing its number and its letter is the way there.
const SEARCH_TYPED_ROW_HELP: &str = "type its number, then its letter";

/// The legend's words beside a session's number while the numbers work, in each preset, and
/// while they do not (`[ui] labels` off): the modal preset's number is a key, the search
/// preset's is typed into the search, which puts the cursor on its session.
const NUMBER_KEYED: &str = "session: bold, numbered; its number puts the cursor on it  \u{b7}  * current  \u{b7}  - previous";
const SEARCH_NUMBER_KEYED: &str = "session: bold, numbered; typing its number puts the cursor on it  \u{b7}  * current  \u{b7}  - previous";
const NUMBER_LABEL: &str =
    "session: bold, numbered in tmux order  \u{b7}  * current  \u{b7}  - previous";

/// The filter's help line: what the filter is, that it matches the state word a row draws
/// too (as herdr's Go To does, so `/blocked` lists the blocked agents), and, when `[ui]
/// projects_in_filter` is on, that it also lists directories to open under the sessions.
const FILTER_HELP: &str = "filter (fzf syntax), by state too";
const FILTER_WITH_PROJECTS_HELP: &str =
    "filter (fzf syntax), by state too; directories to open show below";

/// [`help_text`] for a popup whose filter does or does not list directories
/// (`[ui] projects_in_filter`): the one line about the filter says which.
pub fn help_text_for(preset: KeysPreset, projects_in_filter: bool) -> Vec<Help> {
    use Help::{
        Blank, Chord, Glyph, Group, Keys, LetterKeys, Literal, Machines, Note, Number, NumberKeys,
        Pair, State, TypedRow,
    };
    use KeyAction as K;
    let filter = if projects_in_filter {
        FILTER_WITH_PROJECTS_HELP
    } else {
        FILTER_HELP
    };
    // The chord for next up is drawn right under next up's own line in both presets.
    let next_up_chord = Chord {
        keys: "prefix+O",
        of: &[&[K::NextAttention, K::PreviousAttention]],
        same: NEXT_UP_CHORD_HELP,
        alone: NEXT_UP_CHORD_ALONE_HELP,
    };
    let mut out = Vec::new();
    match preset {
        KeysPreset::Modal => out.extend([
            Group("move"),
            Keys(&[&[K::MoveDown, K::MoveUp]], "next / previous row"),
            Keys(
                &[&[K::NextSession, K::PreviousSession]],
                "next / previous session",
            ),
            // The one line that says what yellow means in the popup: the numbers, the cursor's
            // session's letters and this help's key column are keys that work.
            NumberKeys(NUMBER_KEYS_HELP),
            // The letters are always beside the rows, so they are keys of their own rather than
            // a number's sequel; every session's rows wear them, and only the cursor's
            // session's are lit, the keys. The line names a row's address too, `4a`, which
            // reaches it in the search.
            LetterKeys(LETTER_KEYS_HELP),
            Keys(
                &[&[K::NextAttention, K::PreviousAttention]],
                "next up: who to deal with next (what needs you, else every agent)",
            ),
            next_up_chord,
            Keys(
                &[&[K::Shallower, K::Deeper]],
                "show less / more: sessions, agents, every pane",
            ),
            Keys(
                &[&[K::ToggleFlat]],
                "flat: one list of what is in view (agents, or every pane), most urgent first",
            ),
            Keys(&[&[K::Filter]], filter),
            Blank,
            Group("go"),
            Keys(&[&[K::Go]], "go to the row under the cursor"),
            Machines,
            Blank,
            Group("act on the row"),
            // The paste key shares the row: the help is exactly as tall as the sample client shows, so
            // a row of its own would push the legend's last line off it.
            Keys(
                &[&[K::Rename], &[K::Close], &[K::PasteImage]],
                PASTE_IMAGE_ROW_HELP,
            ),
            Keys(
                &[&[K::NewSession], &[K::NewWorktree]],
                "new session  \u{b7}  new worktree",
            ),
            Keys(
                &[&[K::Options], &[K::Help], &[K::Quit]],
                "options  \u{b7}  this help  \u{b7}  quit",
            ),
        ]),
        // A pure fzf search: typing, fzf's own keys to move, Enter and Esc.
        // Every line but the literals is read off the keymap, so a `[keys]` entry that
        // rebinds one of them shows its own key, and a line whose actions have no key
        // left goes, as can a whole group (`render::drop_empty_groups`): one line per
        // action reachable here only through `[keys]`, in the modal help's own wording,
        // so binding one is enough to see it in `?` without changing what a default
        // install shows.
        KeysPreset::Search => out.extend([
            Group("move"),
            Literal("type", filter),
            // A row's number and letter are part of what the search matches.
            TypedRow(SEARCH_TYPED_ROW_HELP),
            // The key column names each action by its shortest key, `Down` and `Up` here, so
            // the preset's other two pairs for the same two actions go in the meaning, read
            // off the keymap like the key column: `(also ctrl-j / ctrl-k, ctrl-n / ctrl-p)`.
            Pair {
                of: [K::MoveDown, K::MoveUp],
                what: "next / previous row",
                alone: ["next", "previous"],
            },
            Keys(
                &[&[K::NextSession, K::PreviousSession]],
                "next / previous session",
            ),
            // Only while `[keys] select_session` binds the numbers: the preset's digits are text
            // for its query, and a bound key acts, its keymap being asked first.
            NumberKeys(NUMBER_KEYS_HELP),
            Keys(
                &[&[K::NextAttention, K::PreviousAttention]],
                "next up: who to deal with next (what needs you, else every agent)",
            ),
            next_up_chord,
            Keys(
                &[&[K::Shallower, K::Deeper]],
                "show less / more: sessions, agents, every pane",
            ),
            Keys(
                &[&[K::ToggleFlat]],
                "flat: one list of what is in view (agents, or every pane), most urgent first",
            ),
            Blank,
            Group("go"),
            Keys(&[&[K::Go]], "go to the row under the cursor"),
            Machines,
            Keys(&[&[K::Quit]], "quit"),
            Blank,
            Group("act on the row"),
            Keys(&[&[K::Rename], &[K::Close]], "rename  \u{b7}  close"),
            // Bound by no key of this preset (its menu has the entry), so it shows only once `[keys]` binds it.
            Keys(&[&[K::PasteImage]], PASTE_IMAGE_HELP),
            Keys(
                &[&[K::NewSession], &[K::NewWorktree]],
                "new session  \u{b7}  new worktree",
            ),
            Keys(&[&[K::Options], &[K::Help]], "options  \u{b7}  this help"),
        ]),
    }
    // The mouse is not a `[keys]` binding, but it is the popup's, and nothing else
    // documents it.
    out.extend([
        // No blank line before the mouse or the legend: the legend's line for the box makes
        // the help one line longer, and the modal help is as long as a 40-row client shows
        // without scrolling. The accent titles set the groups apart.
        Group("mouse"),
        Literal("click", "select the row"),
        Literal("double-click", "go to the row"),
        Literal("right-click", MENU_HELP),
        Literal("wheel", "scroll the tree, or the preview under the pointer"),
        // No blank line here either: the tree line's note makes the legend one line longer. The accent title still sets the legend apart from the mouse lines.
        Group("legend"),
        State(Some(AgentState::Blocked), "blocked", "waiting on you"),
        State(Some(AgentState::Working), "working", "busy"),
        State(
            Some(AgentState::Done),
            "done",
            "finished, you have not looked yet",
        ),
        State(
            Some(AgentState::Idle),
            "idle",
            "looked at, or quiet; a finished agent you have looked at is idle too",
        ),
        State(
            Some(AgentState::Unknown),
            "unknown",
            "agent present, state unreadable",
        ),
        Glyph(
            "\u{25a2}",
            "pane",
            "no agent; the column shows the program in it",
        ),
        Glyph("\u{25cc}", "closed", "a worktree whose session is closed"),
        // All the marker claims: taw saw a container, nothing about how it is set up.
        Note("\u{1f4e6}", "in a container that taw started, or saw start"),
        // The modal preset binds the digits to the numbers; the search preset types its digits
        // into the filter, where a typed number names its session. The words follow the
        // preset, and the number is drawn as the tree draws the numbers at that moment, yellow
        // while they work; with `[ui] labels` off they promise nothing.
        Number(
            "3",
            match preset {
                KeysPreset::Modal => NUMBER_KEYED,
                KeysPreset::Search => SEARCH_NUMBER_KEYED,
            },
            NUMBER_LABEL,
        ),
        Note(
            "\u{251c}",
            "row: under its session (agent, pane or worktree); coloured only when it needs you",
        ),
        Note(
            "\u{251c}",
            "tree line: white on each session's open window, blue down your own session",
        ),
        Note(
            "2:zsh.1",
            "window 2, named zsh, pane 1, the way tmux writes it",
        ),
    ]);
    out
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::tui::overlays::test_support::keymap_with;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn menu() -> Menu {
        Menu::new(
            "rename what? api:2:zsh.1",
            vec![
                Item::new('w', "the window  (now \"zsh\")", RowAction::RenameWindow),
                Item::new('a', "the agent  (now \"claude\")", RowAction::NameAgent),
            ],
            (4, 5),
        )
    }

    #[test]
    fn esc_closes_any_overlay() {
        let mut overlays = [
            Overlay::Help(Scroll::default()),
            Overlay::Settings(Scroll::default(), String::new()),
            Overlay::Project(Box::new(ProjectPicker::new(
                &crate::config::Config::default(),
                std::path::Path::new("/home/nobody"),
            ))),
            Overlay::Prompt(Prompt::new(
                PromptKind::RenameWindow {
                    window: "@1".to_string(),
                },
                "rename-window",
                "zsh",
            )),
            Overlay::Confirm(Confirm::new(
                "kill-pane api:2.1 (claude)?",
                Action::KillPane {
                    pane: "%1".to_string(),
                },
            )),
            Overlay::Menu(menu()),
        ];
        for overlay in &mut overlays {
            assert_eq!(
                overlay.on_key(key(KeyCode::Esc)),
                Response::Close,
                "{overlay:?}"
            );
        }
    }

    #[test]
    fn a_prompt_edits_one_line_and_submits_it_trimmed() {
        let mut p = Prompt::new(
            PromptKind::NewWorktree {
                workspace: "$1".to_string(),
            },
            "new worktree in api: branch",
            "feat/",
        );
        for c in "pdf ".chars() {
            p.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(p.input.value(), "feat/pdf ");
        assert_eq!(
            p.on_key(key(KeyCode::Enter)),
            Response::Submit("feat/pdf".to_string())
        );
    }

    /// A prompt edits through the same `Input` the filter and the picker use, so every
    /// readline chord works in it: prompts (rename, branch) with readline
    /// editing.
    #[test]
    fn ctrl_w_in_a_prompt_deletes_the_word_before_the_cursor() {
        let mut p = Prompt::new(
            PromptKind::RenameSession {
                session: "$2".to_string(),
            },
            "rename-session",
            "api feat pdf",
        );

        assert_eq!(
            p.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            Response::Stay
        );

        assert_eq!(p.input.value(), "api feat ");
    }

    #[test]
    fn left_then_typing_inserts_in_the_middle_of_a_prompt() {
        let mut p = Prompt::new(
            PromptKind::RenameWindow {
                window: "@21".to_string(),
            },
            "rename-window",
            "tests",
        );

        for _ in 0..5 {
            p.on_key(key(KeyCode::Left));
        }
        p.on_key(key(KeyCode::Char('u')));
        p.on_key(key(KeyCode::Char('i')));

        // A backspace-only prompt could not make this edit without retyping the rest.
        assert_eq!(p.input.value(), "uitests");
        assert_eq!(
            p.on_key(key(KeyCode::Enter)),
            Response::Submit("uitests".to_string())
        );
    }

    #[test]
    fn a_prompt_leaves_enter_and_esc_to_the_overlay() {
        let mut p = Prompt::new(
            PromptKind::RenameSession {
                session: "$2".to_string(),
            },
            "rename-session",
            "api",
        );

        // Esc never reaches a prompt (`Overlay::on_key` closes on it first), and Enter
        // submits rather than editing: neither may end up in the text.
        assert_eq!(
            p.on_key(key(KeyCode::Enter)),
            Response::Submit("api".to_string())
        );
        assert_eq!(p.input.value(), "api");
    }

    #[test]
    fn typing_in_a_prompt_clears_a_rejected_value_error() {
        let mut p = Prompt::new(
            PromptKind::RenameAgent {
                pane: "%1".to_string(),
            },
            "name agent",
            "",
        );
        p.error = Some("name is taken".to_string());
        p.on_key(key(KeyCode::Char('a')));
        assert_eq!(p.error, None);
    }

    #[test]
    fn a_confirmation_runs_its_action_on_y_or_enter_and_cancels_on_n() {
        let action = Action::KillWorkspace {
            session: "$2".to_string(),
        };
        let mut c = Confirm::new("kill-session api?", action.clone());
        assert_eq!(
            c.on_key(key(KeyCode::Char('y'))),
            Response::Run(action.clone())
        );
        assert_eq!(c.on_key(key(KeyCode::Enter)), Response::Run(action));
        assert_eq!(c.on_key(key(KeyCode::Char('n'))), Response::Close);
        assert_eq!(c.on_key(key(KeyCode::Char('x'))), Response::Stay);
    }

    /// A question that loses work is answered only by `y`: a stray Enter, or an `n`, keeps it.
    #[test]
    fn a_strict_confirmation_runs_its_action_on_y_alone() {
        let action = Action::RemoveWorktree {
            workspace: "w6".to_string(),
            force: true,
        };
        let mut c = Confirm::strict("remove anyway?", action.clone());
        for code in [KeyCode::Char('y'), KeyCode::Char('Y')] {
            assert_eq!(
                c.on_key(key(code)),
                Response::Run(action.clone()),
                "{code:?}"
            );
        }
        for code in [
            KeyCode::Enter,
            KeyCode::Char('n'),
            KeyCode::Char('N'),
            KeyCode::Char('q'),
        ] {
            assert_eq!(c.on_key(key(code)), Response::Close, "{code:?} keeps it");
        }
        for code in [KeyCode::Char('x'), KeyCode::Tab, KeyCode::Char(' ')] {
            assert_eq!(c.on_key(key(code)), Response::Stay, "{code:?}");
        }
    }

    /// The group question: `y` is the wider of the two answers, Enter and `n` the default and
    /// narrower one, and `q` (like Esc) is neither.
    #[test]
    fn a_choice_runs_its_action_on_y_and_the_other_on_enter_or_n() {
        let all = Action::KillGroup {
            session: "$2".to_string(),
        };
        let one = Action::KillWorkspace {
            session: "$2".to_string(),
        };
        let mut c = Confirm::choice("close them too?", all.clone(), one.clone());
        for code in [KeyCode::Char('y'), KeyCode::Char('Y')] {
            assert_eq!(c.on_key(key(code)), Response::Run(all.clone()), "{code:?}");
        }
        for code in [KeyCode::Enter, KeyCode::Char('n'), KeyCode::Char('N')] {
            assert_eq!(c.on_key(key(code)), Response::Run(one.clone()), "{code:?}");
        }
        assert_eq!(c.on_key(key(KeyCode::Char('q'))), Response::Close);
        assert_eq!(c.on_key(key(KeyCode::Char('x'))), Response::Stay);
    }

    #[test]
    fn esc_cancels_every_kind_of_confirmation() {
        let action = Action::KillWorkspace {
            session: "$2".to_string(),
        };
        for mut overlay in [
            Overlay::Confirm(Confirm::new("kill-session api?", action.clone())),
            Overlay::Confirm(Confirm::strict("remove anyway?", action.clone())),
            Overlay::Confirm(Confirm::choice("close them too?", action.clone(), action)),
        ] {
            assert_eq!(
                overlay.on_key(key(KeyCode::Esc)),
                Response::Close,
                "{overlay:?}"
            );
        }
    }

    /// The bottom line names the keys of the kind that is open, and the box's own line puts a
    /// capital on the answer Enter gives (or, for a strict one, the answer that keeps it).
    #[test]
    fn the_hint_and_the_answer_line_follow_the_kind_of_confirmation() {
        let action = Action::KillWorkspace {
            session: "$2".to_string(),
        };
        let plain = Overlay::Confirm(Confirm::new("a?", action.clone()));
        let strict = Overlay::Confirm(Confirm::strict("b?", action.clone()));
        let choice = Overlay::Confirm(Confirm::choice("c?", action.clone(), action));

        assert_eq!(plain.hints(), "y or Enter confirm  \u{b7}  n or Esc cancel");
        assert_eq!(
            strict.hints(),
            "y confirm  \u{b7}  n,\u{a0}Enter\u{a0}or\u{a0}Esc cancel"
        );
        assert_eq!(
            choice.hints(),
            "y close them too  \u{b7}  n or Enter just this one  \u{b7}  Esc cancel"
        );
        for (overlay, line) in [(plain, "y / n"), (strict, "y / N"), (choice, "y / N")] {
            let Overlay::Confirm(confirm) = overlay else {
                unreachable!()
            };
            assert_eq!(confirm.answers.line(), line);
        }
    }

    #[test]
    fn a_menu_entrys_own_letter_runs_it_straight_away() {
        let mut m = menu();
        assert_eq!(
            m.on_key(key(KeyCode::Char('a'))),
            Response::Row(RowAction::NameAgent)
        );
        // The highlight never moved: the letter acted, it did not navigate.
        assert_eq!(m.sel, 0);
    }

    #[test]
    fn a_menu_still_moves_with_j_k_and_runs_the_highlight_on_enter() {
        let mut m = Menu::new(
            "api:2:zsh.1 claude",
            vec![
                Item::plain("go", RowAction::Go),
                Item::new('w', "new worktree\u{2026}", RowAction::NewWorktree),
            ],
            (4, 5),
        );
        m.on_key(key(KeyCode::Down));
        assert_eq!(m.sel, 1);
        assert_eq!(
            m.on_key(key(KeyCode::Enter)),
            Response::Row(RowAction::NewWorktree)
        );
    }

    #[test]
    fn clicking_a_menu_row_selects_and_activates_it() {
        let mut overlay = Overlay::Menu(Menu::new(
            "api:2:zsh.1",
            vec![
                Item::new('r', "rename (window or agent)", RowAction::Rename),
                Item::new('x', "close this pane (kill-pane)", RowAction::Close),
            ],
            (0, 0),
        ));
        assert_eq!(overlay.click_item(1), Response::Row(RowAction::Close));
    }

    #[test]
    fn help_covers_every_key_the_modal_preset_binds() {
        let keymap = crate::tui::keys::preset(KeysPreset::Modal);
        let keys: String = help_text(KeysPreset::Modal)
            .iter()
            .filter_map(|entry| entry.key_line(&keymap))
            .map(|(keys, _)| keys)
            .collect::<Vec<_>>()
            .join(" ");
        for expected in [
            "Tab", "Enter", "prefix+O", "j / k", "J / K", "/", "h / l", "f", "r", "x", "n", "w",
            "o", "?", "q",
        ] {
            assert!(
                keys.contains(expected),
                "help is missing '{expected}': {keys}"
            );
        }
    }

    #[test]
    fn help_names_an_overridden_key_rather_than_the_presets_own() {
        let (keymap, _) = crate::tui::keys::from_config(&crate::config::Keys {
            preset: KeysPreset::Modal,
            bindings: [(
                "new_session".to_string(),
                crate::config::KeySpec::One("N".to_string()),
            )]
            .into_iter()
            .collect(),
        });
        let line = help_text(KeysPreset::Modal)
            .iter()
            .find_map(|entry| match entry {
                Help::Keys(groups, what) if what.starts_with("new session") => {
                    help_keys(groups, &keymap)
                }
                _ => None,
            })
            .expect("a line for the project picker");
        // The new-session line carries new worktree beside it: `N`, then the preset's `w`.
        assert_eq!(line, "N   w");
    }

    /// Every key line of a preset's help, as `(keys, meaning)`, keys as the preset binds
    /// them — the help's key column, in the order drawn.
    fn key_lines(preset: KeysPreset) -> Vec<(String, String)> {
        key_lines_under(preset, &crate::tui::keys::preset(preset))
    }

    /// [`key_lines`] under `keymap`, for a `[keys]` table that changes what the preset binds.
    fn key_lines_under(preset: KeysPreset, keymap: &Keymap) -> Vec<(String, String)> {
        help_text(preset)
            .iter()
            .filter_map(|entry| entry.key_line(keymap))
            .collect()
    }

    fn owned(lines: &[(&str, &str)]) -> Vec<(String, String)> {
        lines
            .iter()
            .map(|(k, w)| ((*k).to_string(), (*w).to_string()))
            .collect()
    }

    /// What the `prefix+O` line says right under the next up line: that it
    /// is the same next up, outside the popup.
    const SAME_NEXT_UP: &str =
        "the same next up from anywhere in tmux, straight there without the popup";

    /// What it says when `[keys]` leaves next up no key, so there is no line above it to be the
    /// same as: what next up walks.
    const ALONE_NEXT_UP: &str = "next up from anywhere in tmux: what needs you, else every agent";

    /// The modal help is written for someone new: how to move, how to go, what the keys do to
    /// the row, the mouse, each line one thing, in words rather than shorthand.
    #[test]
    fn the_modal_help_lists_its_keys_in_its_order_and_wording() {
        assert_eq!(
            key_lines(KeysPreset::Modal),
            owned(&[
                ("j / k", "next / previous row"),
                ("J / K", "next / previous session"),
                ("1-9", "jump to that session (yellow: a key that works now)"),
                (
                    "a-e   4a",
                    "jump to a row of your session  \u{b7}  row a of session 4, in the search too"
                ),
                (
                    "Tab / Shift-Tab",
                    "next up: who to deal with next (what needs you, else every agent)"
                ),
                ("prefix+O", SAME_NEXT_UP),
                ("h / l", "show less / more: sessions, agents, every pane"),
                (
                    "f",
                    "flat: one list of what is in view (agents, or every pane), most urgent first"
                ),
                ("/", "filter (fzf syntax), by state too"),
                ("Enter", "go to the row under the cursor"),
                (
                    "r   x   ctrl-v",
                    "rename  \u{b7}  close  \u{b7}  paste image"
                ),
                ("n   w", "new session  \u{b7}  new worktree"),
                ("o   ?   q", "options  \u{b7}  this help  \u{b7}  quit"),
                ("click", "select the row"),
                ("double-click", "go to the row"),
                (
                    "right-click",
                    "menu: go, rename, close, new worktree, paste image"
                ),
                ("wheel", "scroll the tree, or the preview under the pointer"),
            ])
        );
    }

    /// With the filter listing directories (`[ui] projects_in_filter`) its
    /// help line says so: the modal preset's `/` line, and the search preset's `type` line,
    /// whose filter has no key of its own. Nothing else in either help changes, and with the
    /// switch off both are the help they always were.
    #[test]
    fn the_filters_help_line_says_directories_show_below_only_when_it_lists_them() {
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            let keymap = crate::tui::keys::preset(preset);
            let lines = |projects: bool| -> Vec<(String, String)> {
                help_text_for(preset, projects)
                    .iter()
                    .filter_map(|entry| entry.key_line(&keymap))
                    .collect()
            };
            let (off, on) = (lines(false), lines(true));

            assert_eq!(off, key_lines(preset), "{preset:?}: off is the help it was");
            assert_eq!(on.len(), off.len(), "{preset:?}: no line is added");
            let changed: Vec<_> = off.iter().zip(&on).filter(|(a, b)| a != b).collect();
            assert_eq!(
                changed.len(),
                1,
                "{preset:?}: only the filter's line: {changed:?}"
            );
            let (before, after) = changed[0];
            assert_eq!(before.1, "filter (fzf syntax), by state too", "{preset:?}");
            assert_eq!(
                after.1, "filter (fzf syntax), by state too; directories to open show below",
                "{preset:?}"
            );
            assert_eq!(before.0, after.0, "{preset:?}: the same key");
        }
    }

    /// The search preset is a pure fzf search, and its help says so: typing
    /// filters, the row keys move (the key column names each action by its shortest key,
    /// `Down` and `Up`, so fzf's other two pairs are in the meaning), Enter goes, Esc quits.
    /// Then the mouse. No chord is named, because none is bound.
    #[test]
    fn the_search_help_lists_its_keys_in_the_specs_order_and_wording() {
        assert_eq!(
            key_lines(KeysPreset::Search),
            owned(&[
                ("type", "filter (fzf syntax), by state too"),
                ("4a", "row a of session 4: type its number, then its letter"),
                (
                    "Down / Up",
                    "next / previous row (also ctrl-j / ctrl-k, ctrl-n / ctrl-p)"
                ),
                // Nothing is bound to next up here, so the chord's line is not "the same" as
                // any line above it: it says what next up is.
                ("prefix+O", ALONE_NEXT_UP),
                ("Enter", "go to the row under the cursor"),
                ("Esc", "quit"),
                ("click", "select the row"),
                ("double-click", "go to the row"),
                (
                    "right-click",
                    "menu: go, rename, close, new worktree, paste image"
                ),
                ("wheel", "scroll the tree, or the preview under the pointer"),
            ])
        );
    }

    /// The search preset's move line names the keys that move as `[keys]` binds them, not
    /// `also ctrl-j / ctrl-k, ctrl-n / ctrl-p` whatever is bound, since after a rebind of
    /// `move_down` or `move_up` those keys move nothing and type nothing. Each action is in
    /// the key column by its shortest key and its other keys are in the brackets, a key of
    /// moving down beside one of moving up, and what one of the two has left over named for
    /// what it does; with no other key there are no brackets.
    #[test]
    fn the_search_help_names_the_move_keys_as_bound() {
        let line = |keymap: &Keymap| -> (String, String) {
            key_lines_under(KeysPreset::Search, keymap)
                .into_iter()
                .find(|(_, what)| what.starts_with("next / previous row"))
                .expect("a move line")
        };
        let owned = |keys: &str, what: &str| (keys.to_string(), what.to_string());
        assert_eq!(
            line(&crate::tui::keys::preset(KeysPreset::Search)),
            owned(
                "Down / Up",
                "next / previous row (also ctrl-j / ctrl-k, ctrl-n / ctrl-p)"
            ),
            "the preset's keys: as it always read"
        );
        assert_eq!(
            line(&keymap_with(
                KeysPreset::Search,
                &[("move_down", &["ctrl+d"]), ("move_up", &["ctrl+u"])]
            )),
            owned("ctrl-d / ctrl-u", "next / previous row")
        );
        assert_eq!(
            line(&keymap_with(
                KeysPreset::Search,
                &[
                    ("move_down", &["down", "ctrl+j"]),
                    ("move_up", &["up", "ctrl+k"])
                ]
            )),
            owned("Down / Up", "next / previous row (also ctrl-j / ctrl-k)")
        );
        assert_eq!(
            line(&keymap_with(
                KeysPreset::Search,
                &[("move_down", &["ctrl+d"])]
            )),
            owned(
                "ctrl-d / Up",
                "next / previous row (also ctrl-k and ctrl-p for previous)"
            ),
            "moving up keeps the preset's three keys"
        );
        assert_eq!(
            line(&keymap_with(
                KeysPreset::Search,
                &[("move_down", &["down", "ctrl+j", "ctrl+n", "pagedown"])]
            )),
            owned(
                "Down / Up",
                "next / previous row (also PageDown / ctrl-k, ctrl-j / ctrl-p, ctrl-n for next)"
            ),
            "one key of moving down more than of moving up"
        );
    }

    /// The `prefix+O` line is the next up line's neighbour, so it reads as the same action as
    /// `Tab`, just outside the popup: right under it, in either preset, and once only. The search preset has no key for next up until `[keys]` gives it one.
    #[test]
    fn the_chord_line_is_right_under_the_next_up_line_in_both_presets() {
        let next_up = [
            ("next_attention", &["tab"][..]),
            ("previous_attention", &["shift+tab"][..]),
        ];
        for (preset, keymap) in [
            (
                KeysPreset::Modal,
                crate::tui::keys::preset(KeysPreset::Modal),
            ),
            (
                KeysPreset::Search,
                keymap_with(KeysPreset::Search, &next_up),
            ),
        ] {
            let lines = key_lines_under(preset, &keymap);
            let walk = lines
                .iter()
                .position(|(keys, _)| keys == "Tab / Shift-Tab")
                .unwrap_or_else(|| panic!("{preset:?}: a next up line: {lines:?}"));
            assert_eq!(
                lines[walk + 1],
                ("prefix+O".to_string(), SAME_NEXT_UP.to_string()),
                "{preset:?}: {lines:?}"
            );
            assert_eq!(
                lines.iter().filter(|(keys, _)| keys == "prefix+O").count(),
                1,
                "{preset:?}: {lines:?}"
            );
        }
    }

    /// It is not in the `go` group: that group is what Enter (and, in the search
    /// preset, Esc) does, and the chord belongs with the walk it repeats.
    #[test]
    fn the_go_group_no_longer_holds_the_chord() {
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            let keymap = crate::tui::keys::preset(preset);
            let mut group = "";
            let mut go = Vec::new();
            for entry in help_text(preset) {
                match &entry {
                    Help::Group(title) => group = title,
                    _ if group == "go" => go.extend(entry.key_line(&keymap)),
                    _ => {}
                }
            }
            let keys: Vec<&str> = go.iter().map(|(keys, _)| keys.as_str()).collect();
            let expected: &[&str] = match preset {
                KeysPreset::Modal => &["Enter"],
                KeysPreset::Search => &["Enter", "Esc"],
            };
            assert_eq!(keys, expected, "{preset:?}");
        }
    }

    /// With `[keys]` leaving next up no key its line is gone, and the chord's line must not
    /// say "the same" to nothing: it says what next up walks, and stays
    /// where that line would have been. With one of the two keys left the line is still there,
    /// and so is "the same".
    #[test]
    fn with_no_key_for_next_up_the_chord_line_says_what_next_up_is() {
        let keymap = keymap_with(
            KeysPreset::Modal,
            &[("next_attention", &[]), ("previous_attention", &[])],
        );
        let lines = key_lines_under(KeysPreset::Modal, &keymap);
        assert!(
            lines.iter().all(|(keys, _)| !keys.starts_with("Tab")),
            "the next up line is gone: {lines:?}"
        );
        let chord = lines
            .iter()
            .position(|(keys, _)| keys == "prefix+O")
            .unwrap_or_else(|| panic!("the chord's line: {lines:?}"));
        assert_eq!(lines[chord].1, ALONE_NEXT_UP, "{lines:?}");
        assert_eq!(
            lines[chord - 1].0,
            "a-e   4a",
            "where the next up line was: {lines:?}"
        );
        assert_eq!(lines[chord + 1].0, "h / l", "{lines:?}");

        // Whichever of the two keys is the one left, the line is drawn under it and the chord
        // is the same as it: the chord counts both of next up's keys, not just the first.
        for (unbound, left) in [
            ("previous_attention", "Tab"),
            ("next_attention", "Shift-Tab"),
        ] {
            let keymap = keymap_with(KeysPreset::Modal, &[(unbound, &[])]);
            let lines = key_lines_under(KeysPreset::Modal, &keymap);
            let walk = lines
                .iter()
                .position(|(keys, _)| keys == left)
                .unwrap_or_else(|| {
                    panic!(
                        "{unbound} unbound: the next up line is still there as {left}: {lines:?}"
                    )
                });
            assert_eq!(
                lines[walk + 1],
                ("prefix+O".to_string(), SAME_NEXT_UP.to_string()),
                "{unbound} unbound: {lines:?}"
            );
        }
    }

    /// Every key the search preset binds is named somewhere in its help: in the key column
    /// or, for the second and third key of an action, in the line's meaning.
    #[test]
    fn help_covers_every_key_the_search_preset_binds() {
        let keymap = crate::tui::keys::preset(KeysPreset::Search);
        let text: String = key_lines(KeysPreset::Search)
            .iter()
            .map(|(keys, what)| format!("{keys} {what}"))
            .collect::<Vec<_>>()
            .join("\n");
        use KeyAction as K;
        let every = [
            K::NextAttention,
            K::PreviousAttention,
            K::Go,
            K::MoveDown,
            K::MoveUp,
            K::NextSession,
            K::PreviousSession,
            K::Filter,
            K::Shallower,
            K::Deeper,
            K::ToggleFlat,
            K::Rename,
            K::Close,
            K::NewSession,
            K::NewWorktree,
            K::Options,
            K::Help,
            K::Quit,
        ]
        .into_iter()
        .chain((1..=9).map(K::SelectSession));
        let mut named = 0;
        for action in every {
            for key in keymap.keys_for(action) {
                assert!(text.contains(&key), "{key} ({action:?}) is not in:\n{text}");
                named += 1;
            }
        }
        assert_eq!(named, 8, "Enter, Esc, and three keys each for down and up");
    }

    /// The legend's `3` says what a session's number does while the numbers work: the modal
    /// preset binds the digits, and in the search preset typing a number puts the cursor on its
    /// session, so both promise it in their preset's words. With the numbers not working
    /// (`[ui] labels` off) neither promises anything.
    #[test]
    fn the_legend_says_a_number_moves_the_cursor_only_while_the_numbers_work() {
        let note = |preset, work| {
            help_text(preset)
                .iter()
                .find_map(|entry| entry.number_words(work))
                .expect("a legend line for a session's number")
        };
        let modal = note(KeysPreset::Modal, true);
        assert!(
            modal.starts_with("session: bold, numbered; its number puts the cursor on it"),
            "{modal}"
        );
        let search = note(KeysPreset::Search, true);
        assert!(
            search.starts_with("session: bold, numbered; typing its number puts the cursor on it"),
            "{search}"
        );
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            let off = note(preset, false);
            assert!(!off.contains("cursor"), "{preset:?}: {off}");
            assert!(
                off.starts_with("session: bold, numbered in tmux order"),
                "{off}"
            );
        }
    }

    /// The lines for a row's address name it as the tree draws it: the number a `[keys]`
    /// entry gives session 4 and the first letter `[keys]` leaves to the rows, the modal
    /// preset's beside its letters and the search preset's on a line of its own. With every
    /// letter taken there is no address and both lines are dropped.
    #[test]
    fn the_address_lines_name_the_first_free_letter_and_go_with_the_letters() {
        let line = |preset, keymap: &Keymap| {
            help_text(preset)
                .iter()
                .filter(|entry| matches!(entry, Help::LetterKeys(_) | Help::TypedRow(_)))
                .find_map(|entry| entry.key_line(keymap))
        };
        let taken_a = keymap_with(KeysPreset::Modal, &[("rename", &["a"])]);
        assert_eq!(
            line(KeysPreset::Modal, &taken_a),
            Some((
                "b-e   4b".to_string(),
                "jump to a row of your session  \u{b7}  row b of session 4, in the search too"
                    .to_string()
            ))
        );
        let taken_a = keymap_with(KeysPreset::Search, &[("rename", &["a"])]);
        assert_eq!(
            line(KeysPreset::Search, &taken_a),
            Some((
                "4b".to_string(),
                "row b of session 4: type its number, then its letter".to_string()
            ))
        );
        let all = [
            ("rename", &["a"][..]),
            ("close", &["b"][..]),
            ("new_session", &["c"][..]),
            ("new_worktree", &["d"][..]),
            ("options", &["e"][..]),
        ];
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            assert_eq!(line(preset, &keymap_with(preset, &all)), None, "{preset:?}");
        }
    }

    /// The legend: `*` is the session you are in
    /// (`current`) and `-` the one you came from (`previous`), the connector's note says what
    /// hangs from it (a row, whatever it is, not an agent) and that it is coloured only when
    /// it needs you, the place column is written the way tmux writes it, and idle says a
    /// finished agent you have looked at is idle too. No dash, arrow or curly quote. A line of
    /// its own, under the connector's, says what colour the tree line is: white on the window
    /// each session has open, blue down your own session.
    #[test]
    fn the_legend_says_what_the_star_the_connector_and_the_place_mean() {
        let legend = |preset| -> Vec<(&'static str, &'static str)> {
            let keymap = crate::tui::keys::preset(preset);
            help_text(preset)
                .iter()
                .filter_map(|entry| match entry {
                    Help::Note(subject, what) => Some((*subject, *what)),
                    Help::Number(subject, ..) => {
                        let work = keymap.select_session_hint().is_some();
                        Some((*subject, entry.number_words(work).unwrap_or_default()))
                    }
                    Help::State(Some(AgentState::Idle), _, what) => Some(("idle", *what)),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            legend(KeysPreset::Modal),
            [
                (
                    "idle",
                    "looked at, or quiet; a finished agent you have looked at is idle too"
                ),
                ("\u{1f4e6}", "in a container that taw started, or saw start"),
                (
                    "3",
                    "session: bold, numbered; its number puts the cursor on it  \u{b7}  * current  \u{b7}  - previous"
                ),
                (
                    "\u{251c}",
                    "row: under its session (agent, pane or worktree); coloured only when it needs you"
                ),
                (
                    "\u{251c}",
                    "tree line: white on each session's open window, blue down your own session"
                ),
                (
                    "2:zsh.1",
                    "window 2, named zsh, pane 1, the way tmux writes it"
                ),
            ]
        );
        let search = legend(KeysPreset::Search);
        assert_eq!(
            search[2],
            (
                "3",
                "session: bold, numbered in tmux order  \u{b7}  * current  \u{b7}  - previous"
            )
        );
        assert_eq!(search[3..], legend(KeysPreset::Modal)[3..]);
        assert_eq!(search[..2], legend(KeysPreset::Modal)[..2]);
    }

    /// Nothing that was taken out of the popup is in either help: not the keys, not the
    /// chord that closed the popup, not the filter's "a number alone is that session".
    #[test]
    fn the_help_names_none_of_the_removed_keys() {
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            for (keys, what) in key_lines(preset) {
                for gone in ["X", "s", "v", "c", "prefix+C"] {
                    assert!(
                        !keys.split_whitespace().any(|key| key == gone),
                        "{preset:?}: {keys} {what}"
                    );
                }
                for gone in [
                    "split",
                    "new window",
                    "close window",
                    "whole window",
                    "close the popup",
                    "a number alone",
                ] {
                    assert!(!what.contains(gone), "{preset:?}: {keys} {what}");
                }
            }
        }
    }

    /// The groups each preset's help is in, in order: the mouse has one of its own, shared
    /// by both, right before the legend. `help_text` itself always has the search preset's
    /// "act on the row" (everything under it is reachable only through `[keys]`):
    /// [`super::super::render::drop_empty_groups`] is what drops it from
    /// what actually reaches the screen while nothing under it is bound.
    #[test]
    fn help_groups_the_keys_and_ends_with_the_legend() {
        let groups = |preset| -> Vec<&str> {
            help_text(preset)
                .iter()
                .filter_map(|entry| match entry {
                    Help::Group(title) => Some(*title),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            groups(KeysPreset::Modal),
            ["move", "go", "act on the row", "mouse", "legend"]
        );
        assert_eq!(
            groups(KeysPreset::Search),
            ["move", "go", "act on the row", "mouse", "legend"]
        );
    }

    /// The modal help is 36 lines, which is all a 40-row client shows without scrolling: the
    /// popup's 39 rows, less the hint line under the box and the box's two borders. The tree
    /// line's note made the legend one line longer, so the blank line
    /// before the legend's title went to pay for it: the title, in the accent, separates the
    /// legend from the mouse lines as well as a blank did. The legend's line for the box
    /// made the help one line longer again, and the blank line before the mouse group went
    /// the same way. Every other group keeps its blank line, in both presets.
    #[test]
    fn only_the_mouse_and_the_legend_go_without_a_blank_line_before_their_title() {
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            let entries = help_text(preset);
            for (at, entry) in entries.iter().enumerate() {
                let Help::Group(title) = entry else { continue };
                let blank_before = at > 0 && matches!(entries[at - 1], Help::Blank);
                assert_eq!(
                    blank_before,
                    !matches!(*title, "move" | "mouse" | "legend"),
                    "{preset:?}: a blank line before the {title} group"
                );
            }
        }
    }

    #[test]
    fn the_legend_says_what_the_box_claims_and_nothing_more() {
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            let entries = help_text(preset);
            let closed = entries
                .iter()
                .position(|entry| matches!(entry, Help::Glyph("\u{25cc}", ..)))
                .unwrap();
            assert!(
                matches!(
                    entries[closed + 1],
                    Help::Note("\u{1f4e6}", "in a container that taw started, or saw start")
                ),
                "{preset:?}: the box's line comes right after the closed worktree's"
            );
        }
    }
}
