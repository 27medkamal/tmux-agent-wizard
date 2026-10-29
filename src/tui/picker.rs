//! The "new session" picker: type a directory, see what Enter would do, then do it.
//!
//! Three things share the overlay:
//!
//! - a one-line [`Input`] holding a path, which Tab completes anywhere on the filesystem;
//! - the candidates under it — filesystem completion first, then `zoxide`'s history and
//!   the configured project directories, fuzzy-ranked against what was typed, each
//!   tagged with where it came from or that a session is already running there;
//! - an [`Outcome`]: the exact thing Enter would do, spelled out before it happens —
//!   switch to the session running at that path, or `new-session` under a proposed name,
//!   with a note when that name had to dodge a collision.
//!
//! Nothing here touches tmux or runs `zoxide`: [`ProjectPicker::refresh`] is handed the
//! snapshot and `zoxide`'s directories (queried once, when the picker opens) and returns
//! a [`Response`] for the app to act on, which is what makes the whole thing testable
//! without a terminal.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{Config, NameMode};
use crate::session::naming::{Existing, expand_tilde, session_name_for, tilde};
use crate::session::projects::{Candidate, Source, complete_path, project_dir_candidates, rank};
use crate::tui::actions::Action;
use crate::tui::app;
use crate::tui::data::Snapshot;
use crate::tui::input::Input;
use crate::tui::overlays::state::Response;
use crate::tui::tree::{self, Query};

/// The most candidates the picker ever lists: the box is
/// drawn over the sidebar, and a longer list would push the outcome line — the point of
/// the overlay — off the bottom of a short popup.
const MAX_CANDIDATES: usize = 10;

/// How many folders of each source `taw session candidates` tells at most: a whole zoxide history
/// is hundreds of directories, and a machine's answer is read through ssh (1000 rows are about
/// 80 KB). The two sources are capped apart, so a long history never pushes the project folders out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OfferedCaps {
    pub zoxide: usize,
    pub projects: usize,
}

/// What `taw session candidates` sends: 1000 folders from zoxide and 1000 from `project_dirs`.
pub const OFFERED_CAPS: OfferedCaps = OfferedCaps {
    zoxide: 1000,
    projects: 1000,
};

/// What a candidate's row says about it on the right: where it came from, or that a
/// session is already running there.
///
/// Decided by the path, not by which list produced it: a
/// completion the user typed their way to is still `zoxide`'s if `zoxide` knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    Running,
    Zoxide,
    ProjectDir,
}

impl Tag {
    pub fn label(self) -> &'static str {
        match self {
            Tag::Running => "running",
            Tag::Zoxide => "zoxide",
            Tag::ProjectDir => "project dir",
        }
    }
}

/// What Enter would do with the picker's current target — the line under the candidates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The directory Enter acts on, in tilde form: the highlighted candidate, or what was
    /// typed when nothing matched.
    pub target: String,
    /// The name of the session already running there, when there is one — Enter switches
    /// to it and creates nothing.
    pub running: Option<String>,
    /// The session name a new one would take, when nothing is running there.
    pub name: Option<String>,
    /// Why [`Outcome::name`] is longer than the directory's own name.
    pub collision: Option<String>,
    /// Whether the target is a directory that exists. Enter refuses one that is not,
    /// unless a session is running there anyway.
    pub exists: bool,
}

/// One session already running, as the picker compares against it: by *path*, so the
/// same directory opened under two different session names is still "already running".
#[derive(Clone, Debug, PartialEq, Eq)]
struct Running {
    name: String,
    /// Tilde form, which is what [`session_name_for`] compares.
    tilde: String,
    /// Canonicalised where the filesystem allows it, so `/tmp` and `/private/tmp`, or a
    /// symlinked checkout and its target, are one directory.
    at: PathBuf,
}

/// The "new session" overlay: an input, its candidates, and what Enter would do.
#[derive(Clone, Debug)]
pub struct ProjectPicker {
    pub input: Input,
    /// At most [`MAX_CANDIDATES`], completion first, then the ranked sources.
    pub candidates: Vec<Candidate>,
    pub sel: usize,
    /// `None` only while the input is empty, when there is nothing to preview.
    pub outcome: Option<Outcome>,
    /// Set by Enter on a directory that is not there; cleared by the next key.
    pub error: Option<String>,
    /// One per entry of `candidates`, in the same order — always rebuilt with it.
    tags: Vec<Option<Tag>>,
    /// Kept so Enter can resolve a `~` without being handed the world again: a key
    /// arrives between two [`ProjectPicker::refresh`] calls, and acting on the previous
    /// refresh's target would open the wrong directory.
    home: PathBuf,
    mode: NameMode,
    sessions: Vec<Running>,
    /// Set for the picker of another machine ([`ProjectPicker::far`]): its list is that machine's answer,
    /// filtered here, and Enter makes the session there. `None` for this machine's own picker.
    pub far: Option<Far>,
}

/// A folder another machine's taw offered (`taw session candidates --json`), for that machine's picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FarFolder {
    /// `session`, `zoxide` or `project`, as [`Offered::kind`].
    pub kind: String,
    /// The folder in tilde form, as that machine shows it.
    pub name: String,
    /// The folder, absolute on that machine: what Enter sends.
    pub dir: String,
    /// The session running in the folder there, when there is one.
    pub session: Option<String>,
}

/// What another machine's taw listed for its picker: the folders, whether the list was cut (`truncated`), and
/// tmux's own words when tmux there failed for a reason other than no server (the folders are still listed).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FarList {
    pub folders: Vec<FarFolder>,
    pub truncated: bool,
    pub sessions_error: Option<String>,
}

/// What another machine answered when its picker asked for its folders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FarAnswer {
    Folders(FarList),
    /// The taw there is older than `session candidates`: the app asks for a folder or a term instead.
    OldTaw,
    /// Any other failure, in the words to show.
    Said(String),
}

/// What the picker of another machine knows: whose it is, the list it was given, and whether it waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Far {
    pub machine: String,
    /// The last list the machine gave in this run of the app; empty until one came.
    pub folders: Vec<FarFolder>,
    /// That list was cut short over there.
    pub truncated: bool,
    /// tmux's own words on that machine, when it could not list the sessions there.
    pub sessions_error: Option<String>,
    /// The machine has been asked and has not answered yet.
    pub asking: bool,
    /// The far error's own words, when the last ask failed.
    pub said: Option<String>,
    /// The index in `folders` of each candidate, in the candidates' order.
    shown: Vec<usize>,
}

/// `Input` has no `PartialEq` (it wraps `tui_input`'s, which has none either), so the
/// comparison is over what the input *is*: its text and where the cursor sits.
impl PartialEq for ProjectPicker {
    fn eq(&self, other: &ProjectPicker) -> bool {
        self.input.value() == other.input.value()
            && self.input.cursor() == other.input.cursor()
            && self.candidates == other.candidates
            && self.sel == other.sel
            && self.outcome == other.outcome
            && self.error == other.error
            && self.tags == other.tags
            && self.home == other.home
            && self.mode == other.mode
            && self.sessions == other.sessions
            && self.far == other.far
    }
}

impl Eq for ProjectPicker {}

impl ProjectPicker {
    /// An empty picker: nothing is typed, so `refresh` lists `zoxide` and
    /// the configured project directories right away — completion owns no keystroke
    /// until one looks like a path.
    pub fn new(cfg: &Config, home: &Path) -> ProjectPicker {
        ProjectPicker {
            input: Input::new(""),
            candidates: Vec::new(),
            sel: 0,
            outcome: None,
            error: None,
            tags: Vec::new(),
            home: home.to_path_buf(),
            mode: cfg.sessions.name,
            sessions: Vec::new(),
            far: None,
        }
    }

    /// The picker of `machine`, another machine: it shows `kept`, the list that machine gave last in this run
    /// of the app (none the first time), and says it is asking; [`Self::set_far_answer`] fills it.
    pub fn far(cfg: &Config, home: &Path, machine: &str, kept: FarList) -> ProjectPicker {
        let mut picker = ProjectPicker::new(cfg, home);
        picker.far = Some(Far {
            machine: machine.to_string(),
            folders: kept.folders,
            truncated: kept.truncated,
            sessions_error: kept.sessions_error,
            asking: true,
            said: None,
            shown: Vec::new(),
        });
        picker.refresh_far();
        picker
    }

    /// The machine's answer: its folders replace the list (what is typed stays, and the cursor stays on the same
    /// folder when it is still listed), or its error is shown over the list it had.
    pub fn set_far_answer(&mut self, folders: Result<FarList, String>) {
        let Some(far) = self.far.as_mut() else {
            return;
        };
        let was = far
            .shown
            .get(self.sel)
            .and_then(|at| far.folders.get(*at))
            .map(|folder| folder.dir.clone());
        far.asking = false;
        match folders {
            Ok(list) => {
                far.folders = list.folders;
                far.truncated = list.truncated;
                far.sessions_error = list.sessions_error;
                far.said = None;
            }
            Err(said) => far.said = Some(said),
        }
        self.refresh_far();
        if let (Some(was), Some(far)) = (was, self.far.as_ref()) {
            self.sel = far
                .shown
                .iter()
                .position(|at| far.folders[*at].dir == was)
                .unwrap_or(0);
        }
    }

    /// [`Self::refresh`] for another machine's picker: its folders, fuzzy-ranked against what is typed by the
    /// picker's own matcher, and nothing read here (what is typed is never completed over ssh).
    fn refresh_far(&mut self) {
        let typed = self.input.value().trim().to_string();
        let Some(far) = self.far.as_mut() else {
            return;
        };
        let query = Query {
            text: typed.clone(),
        };
        let mut scored: Vec<(i64, usize)> = far
            .folders
            .iter()
            .enumerate()
            .filter_map(|(at, folder)| {
                if typed.is_empty() {
                    Some((0, at))
                } else {
                    tree::score(&query, &folder.name).map(|(score, _)| (score, at))
                }
            })
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        far.shown = scored
            .into_iter()
            .take(MAX_CANDIDATES)
            .map(|(_, at)| at)
            .collect();
        self.candidates = far
            .shown
            .iter()
            .map(|at| Candidate {
                path: far.folders[*at].name.clone(),
                source: Source::Zoxide,
                is_dir_prefix: false,
            })
            .collect();
        self.tags = far
            .shown
            .iter()
            .map(|at| match far.folders[*at].kind.as_str() {
                "session" => Some(Tag::Running),
                "zoxide" => Some(Tag::Zoxide),
                "project" => Some(Tag::ProjectDir),
                _ => None,
            })
            .collect();
        if self.sel >= self.candidates.len() {
            self.sel = 0;
        }
        let picked = far.shown.get(self.sel).map(|at| &far.folders[*at]);
        self.outcome = match picked {
            Some(folder) => Some(Outcome {
                target: folder.name.clone(),
                running: folder.session.clone(),
                name: None,
                collision: None,
                exists: true,
            }),
            None => (!typed.is_empty()).then(|| Outcome {
                target: typed.clone(),
                running: None,
                name: None,
                collision: None,
                exists: true,
            }),
        };
    }

    /// Enter in another machine's picker: the session is made there for the highlighted folder's own path, or for
    /// what is typed when nothing matches.
    fn activate_far(&mut self) -> Response {
        let Some(far) = self.far.as_ref() else {
            return Response::Stay;
        };
        let (term, exact) = match far.shown.get(self.sel) {
            Some(at) => (far.folders[*at].dir.clone(), true),
            None => (self.input.value().trim().to_string(), false),
        };
        if term.is_empty() {
            return Response::Stay;
        }
        Response::Run(Action::NewRemoteSession {
            machine: far.machine.clone(),
            term,
            exact,
        })
    }

    /// The tag for the candidate at `index`, for the renderer.
    pub fn tag(&self, index: usize) -> Option<Tag> {
        self.tags.get(index).copied().flatten()
    }

    /// Recomputes the candidates and the outcome for what is typed now.
    ///
    /// `zoxide` is its directories in tilde form, queried once when the picker opened —
    /// never per keystroke, which is why this takes them rather than running anything.
    pub fn refresh(&mut self, snap: &Snapshot, cfg: &Config, home: &Path, zoxide: &[String]) {
        if self.far.is_some() {
            self.refresh_far();
            return;
        }
        self.home = home.to_path_buf();
        self.mode = cfg.sessions.name;
        self.sessions = running_sessions(snap, home);

        let typed = self.input.value().trim().to_string();
        self.candidates = candidates(&typed, cfg, home, zoxide, &self.sessions, MAX_CANDIDATES);
        if self.sel >= self.candidates.len() {
            self.sel = 0;
        }
        let zoxide = zoxide_in_use(cfg, zoxide);
        self.tags = self
            .candidates
            .iter()
            .map(|candidate| {
                tag_for(
                    &candidate.path,
                    &self.sessions,
                    zoxide,
                    &cfg.sessions.project_dirs,
                    home,
                )
            })
            .collect();

        let target = self.target();
        self.outcome = (!target.is_empty()).then(|| self.outcome_for(&target));
    }

    /// One key: Enter acts, Tab completes, the arrows (and ctrl-j/k, ctrl-n/p) move the
    /// highlight, and everything else is text for the [`Input`].
    ///
    /// A key that changed what is typed leaves the candidates stale — the caller runs
    /// [`ProjectPicker::refresh`] afterwards, which is where the filesystem is read.
    pub fn on_key(&mut self, key: KeyEvent) -> Response {
        if key.code != KeyCode::Enter {
            self.error = None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => return self.activate(),
            KeyCode::Tab => self.complete(),
            KeyCode::Down => self.move_by(1),
            KeyCode::Up => self.move_by(-1),
            // ctrl-k is "delete to the end of the line" everywhere else in taw; in a
            // picker it is the other half of ctrl-j.
            KeyCode::Char('j' | 'n') if ctrl => self.move_by(1),
            KeyCode::Char('k' | 'p') if ctrl => self.move_by(-1),
            _ => {
                if self.input.handle(key) {
                    self.sel = 0;
                }
            }
        }
        Response::Stay
    }

    /// Moves the highlight, wrapping at both ends.
    pub fn move_by(&mut self, delta: isize) {
        self.sel = app::step(self.sel, self.candidates.len(), delta);
    }

    /// Highlights the candidate at `index` — a click. It does not activate: the outcome
    /// line is the point of this overlay, and a click that created a session would be a
    /// click that skipped reading it.
    pub fn select(&mut self, index: usize) {
        if index < self.candidates.len() {
            self.sel = index;
        }
    }

    /// Replaces what is typed with the highlighted candidate, keeping the trailing `/`
    /// that lets the next Tab complete into it.
    fn complete(&mut self) {
        let Some(candidate) = self.candidates.get(self.sel) else {
            return;
        };
        let mut completed = candidate.path.clone();
        if candidate.is_dir_prefix {
            completed.push('/');
        }
        self.input.set(&completed);
        self.sel = 0;
    }

    /// Enter: open the target, or refuse a directory that is not there.
    ///
    /// The outcome is recomputed here rather than read off `self.outcome`, so a key that
    /// moved the highlight cannot open the previous refresh's directory.
    fn activate(&mut self) -> Response {
        if self.far.is_some() {
            return self.activate_far();
        }
        let target = self.target();
        if target.is_empty() {
            return Response::Stay;
        }
        let outcome = self.outcome_for(&target);
        let openable = outcome.exists || outcome.running.is_some();
        self.outcome = Some(outcome);
        if !openable {
            self.error = Some(format!("no such directory: {target}"));
            return Response::Stay;
        }
        Response::Run(Action::OpenProject { path: target })
    }

    /// The directory Enter acts on: the highlighted candidate, or what was typed when
    /// nothing matched (so a path the user knows exists still works with no candidates).
    fn target(&self) -> String {
        match self.candidates.get(self.sel) {
            Some(candidate) => candidate.path.clone(),
            None => trim_trailing_slashes(self.input.value().trim()),
        }
    }

    /// What Enter would do with `target`, as the outcome line says it.
    fn outcome_for(&self, target: &str) -> Outcome {
        let path = expand_tilde(target, &self.home);
        let running = running_at(&self.sessions, &path).map(|session| session.name.clone());
        let exists = path.is_dir();
        // A session already running there is switched to, so nothing is named and
        // nothing can collide.
        let proposed = running.is_none().then(|| {
            let existing: Vec<Existing> = self
                .sessions
                .iter()
                .map(|session| Existing {
                    name: session.name.clone(),
                    path: session.tilde.clone(),
                })
                .collect();
            session_name_for(target, self.mode, &existing)
        });
        Outcome {
            target: target.to_string(),
            running,
            name: proposed.as_ref().map(|p| p.name.clone()),
            collision: proposed.and_then(|p| p.collision),
            exists,
        }
    }
}

/// The candidates for `typed`, at most `limit`: filesystem completion first when `typed`
/// looks like a path, then `zoxide`'s history and the configured project directories,
/// fuzzy-ranked against it. The one place the list is made: the picker calls it with its
/// own cap, and the `/` filter (`[ui] projects_in_filter`) through [`projects`] with its
/// own, so the two can never list different directories for the same text.
///
/// `typed` is what is typed, trimmed. `zoxide` is its directories in tilde form, and is
/// ignored when `[sessions] zoxide` is off. `running` are the sessions of this machine: a
/// path typed in full, no slash after it, that is a running session's folder is the first
/// candidate, ahead of the folders inside it, so Enter switches to that session.
fn candidates(
    typed: &str,
    cfg: &Config,
    home: &Path,
    zoxide: &[String],
    running: &[Running],
    limit: usize,
) -> Vec<Candidate> {
    let project_dirs: Vec<String> = project_dir_candidates(&cfg.sessions.project_dirs, home);
    candidates_from(typed, cfg, home, zoxide, &project_dirs, running, limit)
}

/// [`candidates`] with the project folders already read, for a caller that cuts them first.
fn candidates_from(
    typed: &str,
    cfg: &Config,
    home: &Path,
    zoxide: &[String],
    project_dirs: &[String],
    running: &[Running],
    limit: usize,
) -> Vec<Candidate> {
    let zoxide = zoxide_in_use(cfg, zoxide);

    // `complete_path` treats anything that is not a recognised path prefix as a
    // literal path relative to *this process's* cwd (see `expand_tilde`'s fallback):
    // called unconditionally, a search term that merely contains a slash
    // (`work/api`) would be resolved against the popup's own working directory, and
    // `rank` always puts a completion candidate first regardless of score, so a
    // coincidental cwd match would then outrank the zoxide entry actually being
    // searched for. Only a `looks_like_path` typed text is a path at all.
    let mut pool = if looks_like_path(typed) {
        complete_path(typed, home)
    } else {
        Vec::new()
    };
    pool.extend(zoxide.iter().map(|path| from_source(path, Source::Zoxide)));
    pool.extend(
        project_dirs
            .iter()
            .map(|path| from_source(path, Source::ProjectDir)),
    );

    // One `Query`, not one per candidate: `tree::score` builds a matcher per call,
    // and a `zoxide` history is hundreds of directories long.
    let query = Query {
        text: typed.to_string(),
    };
    // An empty picker lists its sources rather than fuzzy-ranking them: every path
    // scores the same (0), so the stable sort below keeps `pool`'s order (zoxide's
    // own ranking, then the project directories) instead of `tree::score`'s, which
    // is not guaranteed to score an empty pattern at all.
    let mut ranked = rank(pool, typed, &|_typed, path| {
        if query.text.is_empty() {
            Some(0)
        } else {
            tree::score(&query, path).map(|(score, _)| score)
        }
    });
    if looks_like_path(typed)
        && !typed.ends_with('/')
        && running_at(running, &expand_tilde(typed, home)).is_some()
    {
        let at = canonical(&expand_tilde(typed, home));
        let at_index = ranked
            .iter()
            .position(|candidate| canonical(&expand_tilde(&candidate.path, home)) == at);
        let mut first = match at_index {
            Some(index) => ranked.remove(index),
            None => from_source(&trim_trailing_slashes(typed), Source::Completion),
        };
        // Tab goes on into it, as it does for the folder it was typed from.
        first.is_dir_prefix = true;
        ranked.insert(0, first);
    }
    ranked.truncate(limit);
    ranked
}

/// One directory the `/` filter lists under the sessions: a candidate the picker would
/// list for the same query, and what its row says about it on the right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    /// Tilde form, no trailing `/`: what Enter opens, as the picker's candidate is.
    pub path: String,
    pub tag: Option<Tag>,
}

/// The directories the `/` filter lists for what is typed in it, the picker's own list for
/// that text cut to `limit`, each with the picker's tag. Reads the filesystem for path
/// completion and the project directories, so it is for a keystroke, never a tick:
/// [`retag`] follows the sessions between keystrokes.
pub fn projects(
    typed: &str,
    snap: &Snapshot,
    cfg: &Config,
    home: &Path,
    zoxide: &[String],
    limit: usize,
) -> Vec<Project> {
    let project_dirs = project_dir_candidates(&cfg.sessions.project_dirs, home);
    projects_from(typed, snap, cfg, home, zoxide, &project_dirs, limit)
}

/// [`projects`] with the project folders already read.
fn projects_from(
    typed: &str,
    snap: &Snapshot,
    cfg: &Config,
    home: &Path,
    zoxide: &[String],
    project_dirs: &[String],
    limit: usize,
) -> Vec<Project> {
    let running = running_sessions(snap, home);
    let mut list: Vec<Project> = candidates_from(
        typed.trim(),
        cfg,
        home,
        zoxide,
        project_dirs,
        &running,
        limit,
    )
    .into_iter()
    .map(|candidate| Project {
        path: candidate.path,
        tag: None,
    })
    .collect();
    retag(&mut list, snap, cfg, home, zoxide);
    list
}

/// One row of what the picker offers an empty input, as `taw session candidates` tells it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offered {
    /// `session` for a folder with a session running in it, else `zoxide` or `project`: the
    /// picker's [`Tag`], as a word that does not change with the screen's wording.
    pub kind: &'static str,
    /// What the picker shows: the folder in tilde form.
    pub name: String,
    /// The folder, absolute.
    pub dir: PathBuf,
    /// The name of the session running in the folder, when there is one.
    pub session: Option<String>,
}

/// What [`offered`] lists, and whether a source had more than its cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferedList {
    pub rows: Vec<Offered>,
    /// A zoxide or project folder was left out because its source was longer than its cap.
    pub truncated: bool,
}

/// What the picker lists when nothing is typed, with at most `caps.zoxide` folders from zoxide and
/// `caps.projects` from `project_dirs`: [`projects`] for the empty text over the cut sources, so
/// the order, the dedupe and the tags are the picker's, with each folder made absolute and the
/// session running in it named.
pub fn offered(
    snap: &Snapshot,
    cfg: &Config,
    home: &Path,
    zoxide: &[String],
    caps: OfferedCaps,
) -> OfferedList {
    let running = running_sessions(snap, home);
    let zoxide = zoxide_in_use(cfg, zoxide);
    let mut project_dirs = project_dir_candidates(&cfg.sessions.project_dirs, home);
    let truncated = zoxide.len() > caps.zoxide || project_dirs.len() > caps.projects;
    let zoxide = &zoxide[..zoxide.len().min(caps.zoxide)];
    project_dirs.truncate(caps.projects);
    let rows = projects_from(
        "",
        snap,
        cfg,
        home,
        zoxide,
        &project_dirs,
        caps.zoxide + caps.projects,
    )
    .into_iter()
    .map(|project| {
        let dir = expand_tilde(&project.path, home);
        Offered {
            kind: match project.tag {
                Some(Tag::Running) => "session",
                Some(Tag::Zoxide) => "zoxide",
                Some(Tag::ProjectDir) | None => "project",
            },
            session: running_at(&running, &dir).map(|r| r.name.clone()),
            name: project.path,
            dir,
        }
    })
    .collect();
    OfferedList { rows, truncated }
}

/// Sets each project's tag from the sessions in `snap`, without listing anything again: a
/// directory a session is running in says so, and stops saying so when it closes.
pub fn retag(list: &mut [Project], snap: &Snapshot, cfg: &Config, home: &Path, zoxide: &[String]) {
    let sessions = running_sessions(snap, home);
    let zoxide = zoxide_in_use(cfg, zoxide);
    for project in list {
        project.tag = tag_for(
            &project.path,
            &sessions,
            zoxide,
            &cfg.sessions.project_dirs,
            home,
        );
    }
}

/// `zoxide`'s directories, unless `[sessions] zoxide` is off, when there are none.
fn zoxide_in_use<'a>(cfg: &Config, zoxide: &'a [String]) -> &'a [String] {
    if cfg.sessions.zoxide { zoxide } else { &[] }
}

/// Every session of this machine in `snap`, by path, as the tags and the outcome compare against
/// them. A session of another machine is not running here: its path is that machine's, and its name
/// is no name this machine's tmux has.
fn running_sessions(snap: &Snapshot, home: &Path) -> Vec<Running> {
    snap.spaces
        .iter()
        .filter(|space| space.machine.is_none())
        .map(|space| Running {
            name: space.session_name.clone(),
            tilde: tilde(&space.session_path, home),
            at: canonical(&space.session_path),
        })
        .collect()
}

/// A candidate from one of the two ranked sources, which are whole directories rather
/// than prefixes to keep completing into.
fn from_source(path: &str, source: Source) -> Candidate {
    Candidate {
        path: path.to_string(),
        source,
        is_dir_prefix: false,
    }
}

/// The session running in `path`, compared by canonical path — never by name, which is
/// only ever a label for one.
fn running_at<'a>(sessions: &'a [Running], path: &Path) -> Option<&'a Running> {
    let at = canonical(path);
    sessions.iter().find(|session| session.at == at)
}

/// `path` resolved through symlinks where it can be; unchanged where it cannot (it does
/// not exist yet, say), so two paths that are textually equal still compare equal.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The tag for a candidate path: running beats `zoxide` beats a project directory, and a
/// plain completion of a directory none of them knows has no tag at all.
fn tag_for(
    path: &str,
    sessions: &[Running],
    zoxide: &[String],
    project_dirs: &[PathBuf],
    home: &Path,
) -> Option<Tag> {
    if running_at(sessions, &expand_tilde(path, home)).is_some() {
        return Some(Tag::Running);
    }
    if zoxide.iter().any(|known| known == path) {
        return Some(Tag::Zoxide);
    }
    // Nothing inside a repository's `.git` folder is a project, whatever folder it is under.
    if path.split('/').any(|part| part == ".git") {
        return None;
    }
    let under_project_dir = project_dirs
        .iter()
        .any(|dir| path.starts_with(&format!("{}/", tilde(dir, home))));
    under_project_dir.then_some(Tag::ProjectDir)
}

/// `~/oss/api/` as `~/oss/api`: a trailing slash is how a path is completed *into*, not
/// part of the directory's name.
fn trim_trailing_slashes(typed: &str) -> String {
    typed.trim_end_matches('/').to_string()
}

/// Whether `typed` names a path rather than a search term: it starts with `~`, `/`,
/// `./` or `../`, or is exactly `.` or `..`. Typing something that looks like a path
/// brings filesystem completion in front (as [`rank`] already does for it); everything
/// else is a search term, fuzzy-matched against `zoxide` and the project directories
/// only — never resolved against the filesystem, which is what keeps a fragment like
/// `work/api` from being read as a literal path relative to this process's own working
/// directory.
fn looks_like_path(typed: &str) -> bool {
    let typed = typed.trim();
    typed == "."
        || typed == ".."
        || typed.starts_with('~')
        || typed.starts_with('/')
        || typed.starts_with("./")
        || typed.starts_with("../")
}

#[cfg(test)]
mod tests;
