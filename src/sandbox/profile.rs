//! Sandbox profiles: plain scripts `taw sandbox` runs.
//!
//! Yours are executable files in `<config dir>/sandbox/`, each named for its profile. taw's own,
//! `default`, is built in, and written to `<state dir>/sandbox/default` whenever it runs, so you
//! can read exactly what ran; a file of yours called `default` is used instead. Nothing is ever
//! read from the project: a project that could name its own sandbox could mount your home.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::Sandbox;
use crate::paths::Paths;

/// taw's own profile.
pub const DEFAULT_NAME: &str = "default";

/// Whether `name` can be a profile's: a letter, then letters, digits, `-` and `_`, ending in a
/// letter or a digit, as the default profile's image name `taw-sandbox-<name>` must.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && name.ends_with(|c: char| c.is_ascii_alphanumeric())
}

/// Where your profiles are: `<config dir>/sandbox/`, when the config folder is a full path. One
/// that is not (an empty or relative `TAW_CONFIG_DIR`) would be found from wherever taw runs, which
/// is a project's own folder, and a cloned repository could ship a `sandbox/default` that ran as
/// yours: there are no profiles of yours then ([`NOT_A_FULL_PATH`]).
pub fn yours_dir(paths: &Paths) -> Option<PathBuf> {
    paths
        .config_dir
        .is_absolute()
        .then(|| paths.config_dir.join("sandbox"))
}

/// Why there are no profiles of yours when the config folder is not a full path.
const NOT_A_FULL_PATH: &str =
    "TAW_CONFIG_DIR is not a full path, so taw reads no profiles of yours";

/// Where taw keeps what it writes for sandboxes: `<state dir>/sandbox/`.
pub fn state_dir(paths: &Paths) -> PathBuf {
    paths.state_dir.join("sandbox")
}

/// Where taw writes its own profile before it runs it.
pub fn builtin_path(paths: &Paths) -> PathBuf {
    state_dir(paths).join(DEFAULT_NAME)
}

/// The profile that runs for `agent` (an agent's id): `flag` when one was given, else the agent's
/// line in `[sandbox.agents]`, else `[sandbox] profile`, else taw's own.
pub fn pick<'a>(cfg: &'a Sandbox, agent: &str, flag: Option<&'a str>) -> &'a str {
    flag.or_else(|| cfg.agents.get(agent).map(String::as_str))
        .or(cfg.profile.as_deref())
        .unwrap_or(DEFAULT_NAME)
}

/// A profile taw can run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    /// The file that runs; for taw's own, where it is written before it runs.
    pub path: PathBuf,
    /// Whether it is taw's own, built in.
    pub builtin: bool,
}

/// Why a name finds no profile to run, in the words a person reads.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Missing {
    #[error(
        "{} is not a profile's name (a letter, then letters, digits, - and _, ending in a letter or digit)",
        spelled(.0)
    )]
    BadName(String),
    #[error("no profile called {name} (yours: {names}, in {dir})")]
    NoSuch {
        name: String,
        names: String,
        dir: String,
    },
    /// Your profiles folder, or the config folder it is in, is a link whose folder is not there, as
    /// stow leaves one when it has folded the whole folder into a link: a profile of yours could be
    /// behind it, so no name is taken for missing.
    #[error("your {which} {path} is a link to a folder that is not there (ls -ld {path})")]
    FolderGone { which: &'static str, path: String },
    /// A name that is not `default` while there are no profiles of yours ([`yours_dir`]).
    #[error("no profile called {name} ({NOT_A_FULL_PATH})")]
    NoFolder { name: String },
    /// A file of yours with the profile's name that you may not run: no x bit, or only others'.
    #[error("the profile {name} is not executable (chmod +x {path})")]
    NotExecutable { name: String, path: String },
    /// A profile of yours whose `#!` line names no program you can run: the system's own refusal
    /// would say "No such file or directory" of the profile, which is there.
    #[error(
        "the #! line of the profile {name} names {interpreter}, which is no program you can run (head -1 {path})"
    )]
    NoInterpreter {
        name: String,
        path: String,
        interpreter: String,
    },
    /// A profile of yours whose `#!` line names its program by a path that is not a full one: the
    /// system takes it from the folder the profile runs in, the project's, so a project could ship
    /// the program that runs your profile.
    #[error(
        "the #! line of the profile {name} names {interpreter}, which is not a full path, so the system would take it from the project's folder (head -1 {path})"
    )]
    InterpreterNotAFullPath {
        name: String,
        path: String,
        interpreter: String,
    },
    /// A link of yours with the profile's name whose file is not there, as stow and home-manager
    /// leave one when a file moves.
    #[error("the profile {name} is a link to a file that is not there (ls -l {path})")]
    BrokenLink { name: String, path: String },
    /// Something of yours with the profile's name that is no file: a folder, or a link to one.
    #[error("the profile {name} is not a file (ls -ld {path})")]
    NotAFile { name: String, path: String },
    /// Something of yours with the profile's name that taw cannot look at, and why: a link that
    /// leads back to itself, or a folder it may not look in.
    #[error("taw cannot look at the profile {name}: {why} (ls -ld {path})")]
    CannotLook {
        name: String,
        path: String,
        why: String,
    },
}

/// A name as a sentence says it: an empty one is "an empty name", never a hole.
fn spelled(name: &str) -> &str {
    if name.is_empty() {
        "an empty name"
    } else {
        name
    }
}

/// The key of `agent`'s line in `[sandbox.agents]` as TOML writes it: a bare word as it is, and any
/// other, an empty one among them, in quotes.
fn agent_key(agent: &str) -> String {
    let bare = !agent.is_empty()
        && agent
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if bare {
        format!("sandbox.agents.{agent}")
    } else {
        format!("sandbox.agents.{}", toml::Value::String(agent.to_string()))
    }
}

/// `path` as a person reads it: under your home, with `~` for it.
pub(crate) fn shown(path: &Path) -> String {
    shown_under(path, dirs::home_dir().as_deref())
}

/// [`shown`] with the home given: `~` for it when `path` is under it.
fn shown_under(path: &Path, home: Option<&Path>) -> String {
    match home {
        Some(home) => crate::session::naming::tilde(path, home),
        None => path.display().to_string(),
    }
}

/// Your profiles folder `dir`, or the config folder it is in, when it is a link that cannot be
/// followed: what a name with nothing of yours at all is refused with, since a profile of yours
/// could be behind it.
fn folder_gone(paths: &Paths, dir: &Path) -> Option<Missing> {
    [
        ("profiles folder", dir),
        ("config folder", paths.config_dir.as_path()),
    ]
    .into_iter()
    .find(|(_, folder)| {
        std::fs::symlink_metadata(folder).is_ok() && std::fs::metadata(folder).is_err()
    })
    .map(|(which, folder)| Missing::FolderGone {
        which,
        path: shown(folder),
    })
}

/// Whether `a` and `b` are one folder: by name, or, when both are there, through a link.
fn same_folder(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    a == b
        || matches!(
            (std::fs::metadata(a), std::fs::metadata(b)),
            (Ok(a), Ok(b)) if a.dev() == b.dev() && a.ino() == b.ino()
        )
}

/// Whether the file at `path` is one this process may run, as the system judges it when it runs it:
/// by the x bit that is yours, not any x bit. A relative path is taken from this folder, as the
/// system takes it.
fn runs(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(name) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: access only reads the NUL-terminated name it is handed, which lives until it returns.
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
        && unsafe { libc::access(name.as_ptr(), libc::X_OK) } == 0
}

/// The program the `#!` line of the file at `path` names, read as the system reads it (the first
/// word after `#!`, ended by a space, a tab or the line's end, so a `\r` of a file saved with
/// Windows line ends is part of it), and the way it is shown, with anything unprintable escaped.
/// `None` for a file with no `#!` line, and one that cannot be read (which running it then says).
fn interpreter(path: &Path) -> Option<(PathBuf, String)> {
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    let mut head = [0u8; 256];
    let read = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    let first = head[..read].split(|byte| *byte == b'\n').next()?;
    let word = first
        .strip_prefix(b"#!")?
        .split(|byte| *byte == b' ' || *byte == b'\t')
        .find(|word| !word.is_empty())?;
    Some((
        PathBuf::from(std::ffi::OsStr::from_bytes(word)),
        String::from_utf8_lossy(word).escape_debug().to_string(),
    ))
}

/// What the system said of a file it could not look at, as a clause: its words without the error's
/// number, `too many levels of symbolic links`.
fn why(err: &std::io::Error) -> String {
    let said = err.to_string();
    let said = said.split(" (os error").next().unwrap_or_default();
    let mut chars = said.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_lowercase().chain(chars).collect()
    })
}

/// The profile called `name`: a file of yours with that name, which you have to be able to run,
/// its `#!` program too, else taw's own for `default`. Only a name with nothing of yours at all is
/// taw's own: anything there with the name that cannot run, a link to a file that is not there
/// among them, is refused, so that taw's own never runs in place of yours.
pub fn find(paths: &Paths, name: &str) -> Result<Profile, Missing> {
    if !valid_name(name) {
        return Err(Missing::BadName(name.to_string()));
    }
    let name = name.to_string();
    let taws_own = |name: String| Profile {
        name,
        path: builtin_path(paths),
        builtin: true,
    };
    let Some(dir) = yours_dir(paths) else {
        return if name == DEFAULT_NAME {
            Ok(taws_own(name))
        } else {
            Err(Missing::NoFolder { name })
        };
    };
    // With one folder for config and state, yours is taw's own folder too, and the `default` there
    // is the one taw writes, which it writes again when an update changes it.
    if name == DEFAULT_NAME && same_folder(&dir, &state_dir(paths)) {
        return Ok(taws_own(name));
    }
    let file = dir.join(&name);
    match std::fs::symlink_metadata(&file) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(gone) = folder_gone(paths, &dir) {
                Err(gone)
            } else if name == DEFAULT_NAME {
                Ok(taws_own(name))
            } else {
                Err(Missing::NoSuch {
                    name,
                    names: runnable_names(paths).join(", "),
                    dir: format!("{}/", shown(&dir)),
                })
            }
        }
        Err(err) => Err(Missing::CannotLook {
            name,
            path: shown(&file),
            why: why(&err),
        }),
        Ok(_) => match std::fs::metadata(&file) {
            // What the system would refuse to run is refused here, before anything is made or
            // printed: its own refusal would come once the pane is marked.
            Ok(meta) if meta.is_file() && runs(&file) => match interpreter(&file) {
                Some((program, interpreter)) if program.is_relative() => {
                    Err(Missing::InterpreterNotAFullPath {
                        name,
                        path: shown(&file),
                        interpreter,
                    })
                }
                Some((program, interpreter)) if !runs(&program) => Err(Missing::NoInterpreter {
                    name,
                    path: shown(&file),
                    interpreter,
                }),
                _ => Ok(Profile {
                    name,
                    path: file,
                    builtin: false,
                }),
            },
            Ok(meta) if meta.is_file() => Err(Missing::NotExecutable {
                name,
                path: shown(&file),
            }),
            Ok(_) => Err(Missing::NotAFile {
                name,
                path: shown(&file),
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(Missing::BrokenLink {
                name,
                path: shown(&file),
            }),
            Err(err) => Err(Missing::CannotLook {
                name,
                path: shown(&file),
                why: why(&err),
            }),
        },
    }
}

/// The profiles taw can run, by name: `default` first, then your executable files with a
/// profile's name, sorted.
pub fn runnable_names(paths: &Paths) -> Vec<String> {
    let Some(dir) = yours_dir(paths) else {
        return vec![DEFAULT_NAME.to_string()];
    };
    let mut yours: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| valid_name(name) && name != DEFAULT_NAME)
        .filter(|name| runs(&dir.join(name)))
        .collect();
    yours.sort();
    let mut names = vec![DEFAULT_NAME.to_string()];
    names.extend(yours);
    names
}

/// Writes taw's own profile where it runs from, executable, by a rename, and only when what is
/// there is not it already, its text in a file that can run: the file is what ran. A write that
/// fails takes away what it staged.
pub fn write_builtin(paths: &Paths) -> std::io::Result<PathBuf> {
    let path = builtin_path(paths);
    if runs(&path)
        && std::fs::read_to_string(&path).is_ok_and(|text| text == crate::sandbox::DEFAULT_PROFILE)
    {
        return Ok(path);
    }
    std::fs::create_dir_all(state_dir(paths))?;
    let staged = state_dir(paths).join(format!(".{DEFAULT_NAME}.{}.tmp", std::process::id()));
    let written = std::fs::write(&staged, crate::sandbox::DEFAULT_PROFILE)
        .and_then(|()| std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)))
        .and_then(|()| std::fs::rename(&staged, &path));
    if let Err(err) = written {
        let _ = std::fs::remove_file(&staged);
        return Err(err);
    }
    Ok(path)
}

/// What `taw sandbox new` made: your new profile, and the profile it copied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Made {
    pub name: String,
    pub path: PathBuf,
    pub from: String,
}

/// Why `taw sandbox new` made nothing.
#[derive(Debug, thiserror::Error)]
pub enum NewError {
    #[error(transparent)]
    Name(Missing),
    /// There are no profiles of yours ([`yours_dir`]), so none is made.
    #[error("{NOT_A_FULL_PATH}: set it to a full path to make one")]
    NoFolder,
    /// With one folder for config and state, a `default` made there is taw's own, which taw
    /// writes again whenever it runs it.
    #[error(
        "taw writes its own default to {dir}, since your config folder and taw's state folder are one, so a default of yours would be replaced there: give your copy another name"
    )]
    TawsOwnFolder { dir: String },
    /// Your profiles folder, or the config folder it is in, is a link that cannot be followed.
    #[error(transparent)]
    Folder(Missing),
    #[error("a profile called {name} is already there: {path}")]
    Exists { name: String, path: String },
    #[error(transparent)]
    From(Missing),
    #[error("could not write {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

/// Copies the profile `from` to a new profile of yours called `name`, executable: taw's own
/// default when none is named, as its own comment says, and byte for byte, whatever language it
/// is in. A name that is taken, by a file or a link, is refused, and so is `default` where taw
/// writes its own (one folder for config and state).
pub fn new_profile(paths: &Paths, name: &str, from: Option<&str>) -> Result<Made, NewError> {
    use std::io::Write;
    if !valid_name(name) {
        return Err(NewError::Name(Missing::BadName(name.to_string())));
    }
    let Some(dir) = yours_dir(paths) else {
        return Err(NewError::NoFolder);
    };
    if name == DEFAULT_NAME && same_folder(&dir, &state_dir(paths)) {
        return Err(NewError::TawsOwnFolder { dir: shown(&dir) });
    }
    let path = dir.join(name);
    if std::fs::symlink_metadata(&path).is_ok() {
        return Err(NewError::Exists {
            name: name.to_string(),
            path: shown(&path),
        });
    }
    if let Some(gone) = folder_gone(paths, &dir) {
        return Err(NewError::Folder(gone));
    }
    let io = |at: &Path| {
        let at = shown(at);
        move |source| NewError::Io { path: at, source }
    };
    let (bytes, from) = match from {
        None => (
            crate::sandbox::DEFAULT_PROFILE.as_bytes().to_vec(),
            DEFAULT_NAME.to_string(),
        ),
        Some(from) => {
            let source = find(paths, from).map_err(NewError::From)?;
            let bytes = if source.builtin {
                crate::sandbox::DEFAULT_PROFILE.as_bytes().to_vec()
            } else {
                std::fs::read(&source.path).map_err(io(&source.path))?
            };
            (bytes, source.name)
        }
    };
    std::fs::create_dir_all(&dir).map_err(io(&dir))?;
    write_new(&path, &bytes, |file, bytes| file.write_all(bytes)).map_err(io(&path))?;
    Ok(Made {
        name: name.to_string(),
        path,
        from,
    })
}

/// Puts `bytes` in a new executable file at `path`, whole or not at all: `write` writes them to a
/// file staged beside it, which is then linked in under the name, never over a file that took the
/// name meanwhile. A write that fails takes away what it staged, and leaves the name free.
fn write_new(
    path: &Path,
    bytes: &[u8],
    write: impl FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let staged = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    // One left by a process that had this pid before is no one's.
    let _ = std::fs::remove_file(&staged);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)
        .and_then(|mut file| write(&mut file, bytes))
        .and_then(|()| std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)))
        .and_then(|()| std::fs::hard_link(&staged, path));
    let _ = std::fs::remove_file(&staged);
    written
}

/// One profile taw can run, as `taw sandbox list` says it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Listed {
    pub name: String,
    pub path: PathBuf,
    /// Whether it is a file of yours, rather than taw's own.
    pub yours: bool,
    /// Whether it is the one that runs for an agent the config names none for.
    pub default: bool,
    /// The agents `[sandbox.agents]` gives it, sorted.
    pub agents: Vec<String>,
}

/// A profile taw would run that it refuses, as `taw sandbox list` says it: its name, and why, in
/// the words `taw sandbox` says it with.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Refused {
    pub name: String,
    pub why: String,
}

/// Every profile taw would run that it refuses, `default` first: with none it can run, the list
/// says why.
pub fn refused(paths: &Paths) -> Vec<Refused> {
    runnable_names(paths)
        .into_iter()
        .filter_map(|name| {
            let why = find(paths, &name).err()?.to_string();
            Some(Refused { name, why })
        })
        .collect()
}

/// Every profile taw can run, `default` first.
pub fn list(paths: &Paths, cfg: &Sandbox) -> Vec<Listed> {
    let for_the_rest = cfg.profile.as_deref().unwrap_or(DEFAULT_NAME);
    runnable_names(paths)
        .into_iter()
        .filter_map(|name| find(paths, &name).ok())
        .map(|profile| Listed {
            agents: cfg
                .agents
                .iter()
                .filter(|(_, given)| **given == profile.name)
                .map(|(agent, _)| agent.clone())
                .collect(),
            default: profile.name == for_the_rest,
            yours: !profile.builtin,
            name: profile.name,
            path: profile.path,
        })
        .collect()
}

/// What `taw config check` says of `[sandbox]` and of your profiles folder, apart: what is about a
/// line of the config, and what is about the folder alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Findings {
    /// A name in `[sandbox]` that is no profile, and an agent taw does not know.
    pub config: Vec<String>,
    /// A file in the folder that no line names and that cannot run (one that is not executable,
    /// one whose #! program is not there, a link to a file that is not there), or whose name cannot
    /// be a profile's.
    pub folder: Vec<String>,
}

/// What `taw config check` says of `[sandbox]` and of your profiles folder, one warning each: a
/// name that is no profile, an agent taw does not know, a profile file that cannot run (it is not
/// executable, its #! program is not there, or it is a link to a file that is not there), and a
/// file there whose name cannot be a profile's. A file the config names is said once, under the
/// config's key. Dotfiles, folders and links to folders in the folder are left alone; `default` is
/// asked about as `taw sandbox` asks for it when the config names none, so that what it would
/// refuse is said too (a folder called default, a profiles folder that is a file or a link whose
/// folder is gone).
pub fn check(paths: &Paths, cfg: &Sandbox) -> Findings {
    let mut out = Vec::new();
    let mut said: Vec<String> = Vec::new();
    let mut told: Vec<String> = Vec::new();
    let mut named = |key: String, name: &str, out: &mut Vec<String>| {
        if let Err(missing) = find(paths, name) {
            said.push(name.to_string());
            told.push(missing.to_string());
            out.push(format!("{key}: {missing}"));
        }
    };
    if let Some(name) = &cfg.profile {
        named("sandbox.profile".to_string(), name, &mut out);
    }
    for (agent, name) in &cfg.agents {
        match dropped_line(cfg, agent, name) {
            Some(line) => out.push(line),
            None => named(agent_key(agent), name, &mut out),
        }
    }
    let mut folder = Vec::new();
    // What `taw sandbox` would refuse for `default` when you name none, said once: a folder called
    // default is no file of the folder's, and a folder whose link is gone has none to list.
    if !said.iter().any(|name| name == DEFAULT_NAME)
        && let Err(missing) = find(paths, DEFAULT_NAME)
    {
        said.push(DEFAULT_NAME.to_string());
        if !told.contains(&missing.to_string()) {
            folder.push(format!("sandbox: {missing}"));
        }
    }
    let Some(dir) = yours_dir(paths) else {
        folder.push(format!("sandbox: {NOT_A_FULL_PATH}"));
        return Findings {
            config: out,
            folder,
        };
    };
    let mut files: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| !std::fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect();
    files.sort();
    for name in files {
        if said.contains(&name) {
            continue;
        }
        if !valid_name(&name) {
            folder.push(format!(
                "sandbox: {name} in {}/ is not a profile's name (a letter, then letters, digits, - and _, ending in a letter or digit), so taw sandbox cannot run it",
                shown(&dir)
            ));
        } else if let Err(missing) = find(paths, &name) {
            folder.push(format!("sandbox: {missing}"));
        }
    }
    folder.extend(one_image_for_two(paths));
    Findings {
        config: out,
        folder,
    }
}

/// What config check says of the line `agent = "name"` of `[sandbox.agents]` when taw never reads
/// it: a name that is no agent's, or another spelling of one. `None` for a line taw reads.
fn dropped_line(cfg: &Sandbox, agent: &str, name: &str) -> Option<String> {
    let key = agent_key(agent);
    match crate::detect::identify(agent, Some(agent)) {
        Some(id) if id == agent => None,
        // Its line written under taw's name would give the table that key twice.
        Some(id) if cfg.agents.contains_key(&id) => Some(format!(
            "{key}: taw calls this agent {id}, and the table has a {id} line too: keep one of the two"
        )),
        Some(id) => Some(format!(
            "{key}: taw calls this agent {id}, so its line is {id} = \"{name}\""
        )),
        None if agent.is_empty() => Some(format!(
            "{key}: taw knows no agent with an empty name (taw manifest list names them)"
        )),
        None => Some(format!(
            "{key}: taw knows no agent called {agent} (taw manifest list names them)"
        )),
    }
}

/// The lines of `[sandbox.agents]` taw never reads, in config check's words: which profile they
/// meant, `taw sandbox` cannot tell.
pub fn dropped_lines(cfg: &Sandbox) -> Vec<String> {
    cfg.agents
        .iter()
        .filter_map(|(agent, name)| dropped_line(cfg, agent, name))
        .collect()
}

/// The image the default profile, and a copy of it, builds for the profile called `name`:
/// `taw-sandbox-` and the name, lowercase, with one `-` for each run of `-` and `_`, as
/// `default-profile.sh` names it.
fn image_of(name: &str) -> String {
    let mut image = String::from("taw-sandbox-");
    let mut dashed = false;
    for c in name.chars() {
        if c == '-' || c == '_' {
            if !dashed {
                image.push('-');
            }
            dashed = true;
        } else {
            image.push(c.to_ascii_lowercase());
            dashed = false;
        }
    }
    image
}

/// What config check says of profiles that can run whose names give one image name (`a-b` and
/// `a_b`, `Max` and `max` on a disk that tells case apart): a start of one rebuilds the image the
/// other built, when their recipes differ. One file under two names is one profile.
fn one_image_for_two(paths: &Paths) -> Vec<String> {
    use std::os::unix::fs::MetadataExt;
    /// The file a profile is, by its device and inode; none for taw's own.
    type File = Option<(u64, u64)>;
    let mut by_image: std::collections::BTreeMap<String, Vec<(String, File)>> =
        std::collections::BTreeMap::new();
    for name in runnable_names(paths) {
        let Ok(profile) = find(paths, &name) else {
            continue;
        };
        let file = (!profile.builtin)
            .then(|| std::fs::metadata(&profile.path).ok())
            .flatten()
            .map(|meta| (meta.dev(), meta.ino()));
        by_image
            .entry(image_of(&name))
            .or_default()
            .push((name, file));
    }
    let mut said = Vec::new();
    for (image, profiles) in by_image {
        let mut files: Vec<File> = profiles.iter().map(|(_, file)| *file).collect();
        files.sort();
        files.dedup();
        if files.len() < 2 {
            continue;
        }
        let names: Vec<&str> = profiles.iter().map(|(name, _)| name.as_str()).collect();
        let (last, first) = names.split_last().expect("two names at least");
        said.push(format!(
            "sandbox: the profiles {} and {last} share one image name, {image}, so a start of one can rebuild the other's: rename one of them",
            first.join(", ")
        ));
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn paths(dir: &std::path::Path) -> Paths {
        crate::plugin::tests_support::test_paths(dir)
    }

    fn yours(paths: &Paths, name: &str, mode: u32) -> PathBuf {
        let dir = yours_dir(paths).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name);
        std::fs::write(&file, "#!/bin/sh\nexec docker run img \"$@\"\n").unwrap();
        crate::test_exec::make_executable_with_mode(&file, mode);
        file
    }

    #[test]
    fn a_profiles_name_is_a_letter_then_letters_digits_dashes_and_underscores_ending_in_a_letter_or_digit()
     {
        for good in ["default", "max", "work-2", "box_b", "A"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "2fast",
            "-x",
            "_x",
            "max-",
            "box_",
            "a b",
            "mine.sh",
            "../x",
            "x/y",
            "caf\u{e9}",
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn which_runs_is_the_flag_then_the_agents_line_then_the_profile_then_taws_own() {
        let none = Sandbox::default();
        assert_eq!(pick(&none, "claude", None), "default");
        let cfg = Sandbox {
            profile: Some("max".to_string()),
            agents: [("codex".to_string(), "box".to_string())].into(),
        };
        assert_eq!(pick(&cfg, "claude", None), "max");
        assert_eq!(pick(&cfg, "codex", None), "box");
        assert_eq!(pick(&cfg, "codex", Some("team")), "team");
    }

    #[test]
    fn a_profile_of_yours_is_an_executable_file_in_your_folder() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let file = yours(&paths, "max", 0o755);

        assert_eq!(
            find(&paths, "max").unwrap(),
            Profile {
                name: "max".to_string(),
                path: file,
                builtin: false
            }
        );
    }

    #[test]
    fn default_is_taws_own_unless_you_have_one_of_that_name() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());

        let builtin = find(&paths, "default").unwrap();
        assert_eq!(
            builtin,
            Profile {
                name: "default".to_string(),
                path: builtin_path(&paths),
                builtin: true
            }
        );

        let file = yours(&paths, "default", 0o755);
        assert_eq!(
            find(&paths, "default").unwrap(),
            Profile {
                name: "default".to_string(),
                path: file,
                builtin: false
            }
        );
    }

    /// A file of yours that is not executable is refused with the chmod line, your
    /// own copy of `default` included, and taw's own never stands in for it.
    #[test]
    fn a_file_of_yours_that_is_not_executable_is_refused_and_never_stepped_over() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let file = yours(&paths, "max", 0o644);
        let mine = yours(&paths, "default", 0o644);

        assert_eq!(
            find(&paths, "max").unwrap_err().to_string(),
            format!(
                "the profile max is not executable (chmod +x {})",
                file.display()
            )
        );
        assert_eq!(
            find(&paths, "default").unwrap_err().to_string(),
            format!(
                "the profile default is not executable (chmod +x {})",
                mine.display()
            )
        );
    }

    /// What the system would refuse to run, or would run from the project, is refused before
    /// anything is made, naming what is wrong, so the start, the list and config check say the
    /// same: a profile whose #! line names no program you can run (one that is not there, a folder,
    /// a file only others may run, or one with the `\r` of Windows line ends, shown escaped), one
    /// whose #! program is not named by a full path (the system would take it from the project's
    /// folder), and a profile only others may run, with the chmod line. A profile whose #! names a
    /// program that runs, or that has no #! line, runs.
    #[test]
    fn a_profile_the_system_would_refuse_to_run_is_refused_naming_why() {
        if crate::test_root::skip_as_root(
            "root runs a file whose mode lets only other users run it",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let folder = yours_dir(&paths).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        let write = |name: &str, text: &[u8], mode: u32| {
            let path = folder.join(name);
            std::fs::write(&path, text).unwrap();
            crate::test_exec::make_executable_with_mode(&path, mode);
            path
        };
        let others = write("others-only", b"#!/bin/sh\nexit 0\n", 0o655);
        let others_text = others.display().to_string();
        let gone = write(
            "gone",
            b"#!/no/such/bash -eu\nexec docker run img \"$@\"\n",
            0o755,
        );
        write("relative", b"#!no-such-sh\n", 0o755);
        write("in-a-folder", b"#!bin/sh -eu\n", 0o755);
        write(
            "folder",
            format!("#!{}\n", folder.display()).as_bytes(),
            0o755,
        );
        write("theirs", format!("#!{others_text} -x\n").as_bytes(), 0o755);
        write("windows", b"#!/bin/sh\r\nexit 0\r\n", 0o755);
        write("spaced", b"#! /bin/sh\nexec docker run img \"$@\"\n", 0o755);
        write("tabbed", b"#!\t/bin/sh -eu\n", 0o755);
        write("plain", b"#!/bin/sh\n", 0o755);
        write("bare", b"exec docker run img \"$@\"\n", 0o755);
        write("binary", b"\xcf\xfa\xed\xfe\x07\x00\x00\x01", 0o755);
        let interpreter = |name: &str| match find(&paths, name) {
            Err(Missing::NoInterpreter {
                name: refused,
                path,
                interpreter,
            }) => {
                assert_eq!(refused, name);
                assert_eq!(path, folder.join(name).display().to_string());
                interpreter
            }
            other => panic!("{name}: {other:?}"),
        };

        assert_eq!(
            find(&paths, "gone").unwrap_err().to_string(),
            format!(
                "the #! line of the profile gone names /no/such/bash, which is no program you can run (head -1 {})",
                gone.display()
            )
        );
        assert_eq!(interpreter("gone"), "/no/such/bash");
        for (name, program) in [("relative", "no-such-sh"), ("in-a-folder", "bin/sh")] {
            let file = folder.join(name);
            assert_eq!(
                find(&paths, name).unwrap_err().to_string(),
                format!(
                    "the #! line of the profile {name} names {program}, which is not a full path, so the system would take it from the project's folder (head -1 {})",
                    file.display()
                )
            );
        }
        assert_eq!(interpreter("folder"), folder.display().to_string());
        assert_eq!(interpreter("theirs"), others_text);
        assert_eq!(interpreter("windows"), "/bin/sh\\r");
        assert_eq!(
            find(&paths, "others-only").unwrap_err(),
            Missing::NotExecutable {
                name: "others-only".to_string(),
                path: others_text.clone(),
            }
        );
        for name in ["spaced", "tabbed", "plain", "bare", "binary"] {
            assert!(
                find(&paths, name).is_ok(),
                "{name}: {:?}",
                find(&paths, name)
            );
        }

        // The list has none of them, says why of each it would run, and config check says it too.
        assert_eq!(
            list(&paths, &Sandbox::default())
                .iter()
                .map(|listed| listed.name.as_str())
                .collect::<Vec<_>>(),
            ["default", "bare", "binary", "plain", "spaced", "tabbed"]
        );
        assert_eq!(
            refused(&paths)
                .iter()
                .map(|refused| refused.name.as_str())
                .collect::<Vec<_>>(),
            [
                "folder",
                "gone",
                "in-a-folder",
                "relative",
                "theirs",
                "windows"
            ]
        );
        let findings = check(&paths, &Sandbox::default());
        for line in [
            format!(
                "sandbox: the #! line of the profile gone names /no/such/bash, which is no program you can run (head -1 {})",
                gone.display()
            ),
            format!("sandbox: the profile others-only is not executable (chmod +x {others_text})"),
        ] {
            assert!(findings.folder.contains(&line), "{line}: {findings:?}");
        }
    }

    #[test]
    fn a_name_that_is_no_profile_says_which_ones_there_are_and_where() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        yours(&paths, "max", 0o755);
        yours(&paths, "zed", 0o755);
        yours(&paths, "default", 0o755);
        yours(&paths, "draft", 0o644);
        std::fs::create_dir_all(yours_dir(&paths).unwrap().join("old")).unwrap();

        // Your own default is the one default, and the rest are sorted: the folder lists zed
        // before max.
        assert_eq!(
            find(&paths, "team").unwrap_err().to_string(),
            format!(
                "no profile called team (yours: default, max, zed, in {}/)",
                yours_dir(&paths).unwrap().display()
            )
        );
        assert_eq!(
            find(&paths, "../x").unwrap_err().to_string(),
            "../x is not a profile's name (a letter, then letters, digits, - and _, ending in a letter or digit)"
        );
    }

    #[test]
    fn a_link_to_an_executable_file_elsewhere_is_a_profile() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let elsewhere = dir.path().join("dotfiles-box");
        std::fs::write(&elsewhere, "#!/bin/sh\n").unwrap();
        crate::test_exec::make_executable(&elsewhere);
        std::fs::create_dir_all(yours_dir(&paths).unwrap()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, yours_dir(&paths).unwrap().join("box")).unwrap();

        assert_eq!(
            find(&paths, "box").unwrap().path,
            yours_dir(&paths).unwrap().join("box")
        );
    }

    /// When the config and state folders are one, by name or through a link, taw's own is written
    /// where a `default` of yours would be: that file is taw's own, which it writes again when an
    /// update changes it, and never one of yours.
    #[test]
    fn with_one_folder_for_config_and_state_the_default_there_is_taws_own() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("state-link");
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("config"), &link).unwrap();
        for state in [dir.path().join("config"), link] {
            let mut paths = paths(dir.path());
            paths.state_dir = state.clone();
            let path = write_builtin(&paths).unwrap();
            std::fs::write(&path, "an older taw's own").unwrap();

            assert_eq!(
                find(&paths, "default").unwrap(),
                Profile {
                    name: "default".to_string(),
                    path: builtin_path(&paths),
                    builtin: true
                },
                "{state:?}"
            );
            assert_eq!(runnable_names(&paths), ["default"], "{state:?}");
            write_builtin(&paths).unwrap();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                crate::sandbox::DEFAULT_PROFILE
            );
        }
    }

    /// `target` as a path from the folder the tests run in, the crate's own: as many `..` as it takes
    /// to reach `/`, then the rest of it.
    fn from_here(target: &std::path::Path) -> PathBuf {
        let here = std::env::current_dir().unwrap();
        let mut path = PathBuf::new();
        for _ in here.components().skip(1) {
            path.push("..");
        }
        path.push(target.strip_prefix("/").unwrap());
        path
    }

    /// A config folder that is not a full path (an empty or relative `TAW_CONFIG_DIR`) is found from
    /// wherever taw runs, which is a project's own folder: a cloned repository could ship a
    /// `sandbox/default` that ran as yours. There are no profiles of yours then, and `default` is
    /// taw's own.
    #[test]
    fn a_config_folder_that_is_not_a_full_path_has_no_profiles_of_yours() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("cloned");
        for name in ["default", "max"] {
            let file = project.join("sandbox").join(name);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "#!/bin/sh\n").unwrap();
            crate::test_exec::make_executable(&file);
        }
        for config_dir in [from_here(&project), PathBuf::new()] {
            let mut paths = paths(dir.path());
            paths.config_dir = config_dir.clone();
            assert_eq!(
                find(&paths, "default").unwrap(),
                Profile {
                    name: "default".to_string(),
                    path: builtin_path(&paths),
                    builtin: true
                },
                "{config_dir:?}"
            );
            assert_eq!(
                find(&paths, "max").unwrap_err().to_string(),
                "no profile called max (TAW_CONFIG_DIR is not a full path, so taw reads no profiles of yours)",
                "{config_dir:?}"
            );
            assert_eq!(runnable_names(&paths), ["default"], "{config_dir:?}");
        }
    }

    /// When your whole profiles folder is a link whose folder is gone (stow folds `sandbox/` into one
    /// link whenever it can), or the config folder it is in is, no name is taken for missing: every
    /// one is refused, `default` among them, so taw's own never runs in place of yours.
    #[test]
    fn a_profiles_or_config_folder_whose_link_is_gone_is_refused_for_every_name() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let folder = yours_dir(&paths).unwrap();
        std::os::unix::fs::symlink(dir.path().join("dotfiles-moved").join("sandbox"), &folder)
            .unwrap();
        for name in ["default", "max"] {
            assert_eq!(
                find(&paths, name).unwrap_err().to_string(),
                format!(
                    "your profiles folder {0} is a link to a folder that is not there (ls -ld {0})",
                    folder.display()
                )
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let mut paths = crate::plugin::tests_support::test_paths(dir.path());
        let config = dir.path().join("cfg");
        std::os::unix::fs::symlink(dir.path().join("moved"), &config).unwrap();
        paths.config_file = config.join("config.toml");
        paths.config_dir = config.clone();
        for name in ["default", "max"] {
            assert_eq!(
                find(&paths, name).unwrap_err().to_string(),
                format!(
                    "your config folder {0} is a link to a folder that is not there (ls -ld {0})",
                    config.display()
                )
            );
        }
    }

    /// A link of yours whose file is gone (stow and home-manager leave one when a file moves) is
    /// refused in words of its own, and so is a folder with a profile's name and a link that leads
    /// back to itself: only a name with nothing of yours at all is taw's own `default`, so taw's own
    /// never runs in place of yours.
    #[test]
    fn something_of_yours_that_cannot_run_is_refused_and_never_stepped_over() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(yours_dir(&paths).unwrap()).unwrap();
        for name in ["default", "max"] {
            let link = yours_dir(&paths).unwrap().join(name);
            std::os::unix::fs::symlink(dir.path().join("moved-away").join(name), &link).unwrap();
            assert_eq!(
                find(&paths, name).unwrap_err().to_string(),
                format!(
                    "the profile {name} is a link to a file that is not there (ls -l {})",
                    link.display()
                )
            );
            std::fs::remove_file(&link).unwrap();

            std::fs::create_dir(&link).unwrap();
            assert_eq!(
                find(&paths, name).unwrap_err().to_string(),
                format!(
                    "the profile {name} is not a file (ls -ld {})",
                    link.display()
                )
            );
            std::fs::remove_dir(&link).unwrap();

            std::os::unix::fs::symlink(&link, &link).unwrap();
            assert_eq!(
                find(&paths, name).unwrap_err().to_string(),
                format!(
                    "taw cannot look at the profile {name}: too many levels of symbolic links (ls -ld {})",
                    link.display()
                )
            );
            std::fs::remove_file(&link).unwrap();
        }
    }

    #[test]
    fn taws_own_is_written_where_you_can_read_it_and_only_when_it_changed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());

        let path = write_builtin(&paths).unwrap();
        assert_eq!(path, builtin_path(&paths));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            crate::sandbox::DEFAULT_PROFILE
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let inode = |path: &std::path::Path| {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(path).unwrap().ino()
        };
        let written = inode(&path);
        write_builtin(&paths).unwrap();
        assert_eq!(inode(&path), written, "what was there already is kept");

        // A file changed by hand is replaced by a rename: a hard link to it keeps what it held.
        std::fs::write(&path, "changed by hand").unwrap();
        let kept = dir.path().join("kept");
        std::fs::hard_link(&path, &kept).unwrap();
        write_builtin(&paths).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            crate::sandbox::DEFAULT_PROFILE
        );
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "changed by hand");
    }

    /// A path under your home is shown with `~` for it, and any other as it is.
    #[test]
    fn a_path_under_your_home_is_shown_with_a_tilde() {
        let home = std::path::Path::new("/Users/you");
        assert_eq!(
            shown_under(
                std::path::Path::new("/Users/you/.config/tmux-agent-wizard/sandbox/max"),
                Some(home)
            ),
            "~/.config/tmux-agent-wizard/sandbox/max"
        );
        assert_eq!(
            shown_under(std::path::Path::new("/opt/profiles/max"), Some(home)),
            "/opt/profiles/max"
        );
        assert_eq!(
            shown_under(std::path::Path::new("/Users/you/max"), None),
            "/Users/you/max"
        );
    }

    /// taw's own is what runs, so a copy of it that cannot run is no copy: it is written again,
    /// executable, the text being the same.
    #[test]
    fn taws_own_is_written_again_when_it_is_there_but_cannot_run() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let path = write_builtin(&paths).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_builtin(&paths).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            crate::sandbox::DEFAULT_PROFILE
        );
    }

    /// A write that fails, here because something that is not a file is where taw's own goes,
    /// says so and leaves nothing of its own in taw's folder: each try would leave another.
    #[test]
    fn a_write_of_taws_own_that_fails_leaves_nothing_of_its_own_behind() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(builtin_path(&paths).join("in-the-way")).unwrap();

        assert!(write_builtin(&paths).is_err());
        let left: Vec<String> = std::fs::read_dir(state_dir(&paths))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["default"]);
    }

    #[test]
    fn new_copies_taws_own_into_your_folder_executable_and_refuses_a_name_that_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());

        let made = new_profile(&paths, "mine", None).unwrap();

        assert_eq!(
            made,
            Made {
                name: "mine".to_string(),
                path: yours_dir(&paths).unwrap().join("mine"),
                from: "default".to_string()
            }
        );
        assert_eq!(
            std::fs::read_to_string(&made.path).unwrap(),
            crate::sandbox::DEFAULT_PROFILE
        );
        assert_eq!(
            std::fs::metadata(&made.path).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            new_profile(&paths, "mine", None).unwrap_err().to_string(),
            format!(
                "a profile called mine is already there: {}",
                made.path.display()
            )
        );
        assert_eq!(
            std::fs::read_to_string(&made.path).unwrap(),
            crate::sandbox::DEFAULT_PROFILE
        );
        // A link with the name is taken too, even one to a file that is not there.
        let gone = yours_dir(&paths).unwrap().join("gone");
        std::os::unix::fs::symlink(dir.path().join("moved-away"), &gone).unwrap();
        assert_eq!(
            new_profile(&paths, "gone", None).unwrap_err().to_string(),
            format!("a profile called gone is already there: {}", gone.display())
        );
        assert!(!dir.path().join("moved-away").exists());
        assert!(matches!(
            new_profile(&paths, "my box", None),
            Err(NewError::Name(Missing::BadName(_)))
        ));
    }

    /// `--from` copies a profile of yours byte for byte, whatever language it is in. Without it, a
    /// new profile is a copy of taw's own default, as the default's own comment says, even when
    /// you have a default of your own, which `--from default` copies.
    #[test]
    fn new_from_copies_another_profile_and_new_default_is_your_copy_that_runs_in_its_place() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let boxed = yours(&paths, "box", 0o755);
        std::fs::write(&boxed, "#!/bin/sh\n# box\nexec docker run box \"$@\"\n").unwrap();
        let raw = yours(&paths, "raw", 0o755);
        std::fs::write(&raw, b"\x7fELF\xff\xfe\x00not text").unwrap();

        let copy = new_profile(&paths, "work", Some("box")).unwrap();
        assert_eq!(
            std::fs::read_to_string(&copy.path).unwrap(),
            "#!/bin/sh\n# box\nexec docker run box \"$@\"\n"
        );
        assert_eq!(copy.from, "box");
        let copy = new_profile(&paths, "raw-copy", Some("raw")).unwrap();
        assert_eq!(
            std::fs::read(&copy.path).unwrap(),
            b"\x7fELF\xff\xfe\x00not text"
        );

        let mine = new_profile(&paths, "default", None).unwrap();
        assert!(!find(&paths, "default").unwrap().builtin);
        std::fs::write(&mine.path, "#!/bin/sh\n# my default\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(new_profile(&paths, "fresh", None).unwrap().path).unwrap(),
            crate::sandbox::DEFAULT_PROFILE
        );
        assert_eq!(
            std::fs::read_to_string(new_profile(&paths, "again", Some("default")).unwrap().path)
                .unwrap(),
            "#!/bin/sh\n# my default\n"
        );
        assert!(matches!(
            new_profile(&paths, "x", Some("team")),
            Err(NewError::From(Missing::NoSuch { .. }))
        ));
        assert!(!yours_dir(&paths).unwrap().join("x").exists());
    }

    /// A new profile is there whole or not at all: while it is written, nothing is under its name
    /// and what is staged cannot run; a write that fails part way leaves nothing under the name and
    /// nothing staged, so the next try can take it; a file that takes the name meanwhile is left as
    /// it is.
    #[test]
    fn a_new_profile_is_there_whole_or_not_at_all() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let folder = yours_dir(&paths).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        let entries = || {
            let mut names: Vec<String> = std::fs::read_dir(&folder)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        let half = folder.join("half");

        let err = write_new(
            &half,
            b"#!/bin/sh\nexec docker run img \"$@\"\n",
            |file, bytes| {
                assert!(std::fs::symlink_metadata(&half).is_err());
                assert_eq!(file.metadata()?.permissions().mode() & 0o777, 0o600);
                file.write_all(&bytes[..10])?;
                Err(std::io::Error::other("the disk is full"))
            },
        )
        .unwrap_err();

        assert_eq!(err.to_string(), "the disk is full");
        assert!(entries().is_empty(), "{:?}", entries());
        new_profile(&paths, "half", None).unwrap();
        assert_eq!(
            std::fs::read_to_string(&half).unwrap(),
            crate::sandbox::DEFAULT_PROFILE
        );

        let taken = folder.join("taken");
        let err = write_new(&taken, b"#!/bin/sh\n", |file, bytes| {
            std::fs::write(&taken, "theirs")?;
            file.write_all(bytes)
        })
        .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "theirs");
        assert_eq!(entries(), ["half", "taken"]);
    }

    #[test]
    fn list_names_each_profile_taw_can_run_its_file_and_the_agents_the_config_gives_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let max = yours(&paths, "max", 0o755);
        yours(&paths, "draft", 0o644);
        let cfg = Sandbox {
            profile: Some("max".to_string()),
            agents: [
                ("codex".to_string(), "default".to_string()),
                ("gemini".to_string(), "default".to_string()),
            ]
            .into(),
        };

        assert_eq!(
            list(&paths, &cfg),
            [
                Listed {
                    name: "default".to_string(),
                    path: builtin_path(&paths),
                    yours: false,
                    default: false,
                    agents: vec!["codex".to_string(), "gemini".to_string()]
                },
                Listed {
                    name: "max".to_string(),
                    path: max,
                    yours: true,
                    default: true,
                    agents: vec![]
                },
            ]
        );
        assert_eq!(
            list(&paths, &Sandbox::default())
                .iter()
                .map(|listed| (listed.name.as_str(), listed.default))
                .collect::<Vec<_>>(),
            [("default", true), ("max", false)]
        );
    }

    /// A profile taw would run and refuses is not listed, and the list says why, in the words
    /// taw sandbox says it with: with a profiles folder whose link is gone, that is every one,
    /// default among them; with a folder named default, the rest still run.
    #[test]
    fn list_says_why_of_each_profile_taw_would_run_and_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let mine = paths(dir.path());
        yours(&mine, "max", 0o755);
        assert_eq!(refused(&mine), []);

        std::fs::create_dir(yours_dir(&mine).unwrap().join("default")).unwrap();
        assert_eq!(
            list(&mine, &Sandbox::default())
                .iter()
                .map(|listed| listed.name.as_str())
                .collect::<Vec<_>>(),
            ["max"]
        );
        assert_eq!(
            refused(&mine),
            [Refused {
                name: "default".to_string(),
                why: find(&mine, "default").unwrap_err().to_string(),
            }]
        );

        let dir = tempfile::tempdir().unwrap();
        let stowed = paths(dir.path());
        std::fs::create_dir_all(&stowed.config_dir).unwrap();
        let folder = yours_dir(&stowed).unwrap();
        std::os::unix::fs::symlink(dir.path().join("dotfiles-moved").join("sandbox"), &folder)
            .unwrap();
        assert_eq!(list(&stowed, &Sandbox::default()), []);
        assert_eq!(
            refused(&stowed),
            [Refused {
                name: "default".to_string(),
                why: format!(
                    "your profiles folder {0} is a link to a folder that is not there (ls -ld {0})",
                    folder.display()
                ),
            }]
        );
    }

    /// The refused part of `taw sandbox list` holds only what taw would run and then refuses: a file
    /// that is not executable, and one with a bad name, are in neither part of the list (`taw config
    /// check` says them), while a folder named default, and an executable file whose `#!` names no
    /// full path, are refused with why.
    #[test]
    fn refused_holds_only_what_taw_would_run_not_files_that_are_not_executable_or_badly_named() {
        let dir = tempfile::tempdir().unwrap();
        let mine = paths(dir.path());
        yours(&mine, "max", 0o644);
        yours(&mine, "2fast", 0o755);
        yours(&mine, "mine.sh", 0o644);
        assert_eq!(refused(&mine), []);
        assert_eq!(runnable_names(&mine), ["default"]);

        let folder = yours_dir(&mine).unwrap();
        let file = folder.join("relative");
        std::fs::write(&file, "#!no-such-sh\n").unwrap();
        crate::test_exec::make_executable(&file);
        assert_eq!(
            refused(&mine)
                .iter()
                .map(|refused| refused.name.as_str())
                .collect::<Vec<_>>(),
            ["relative"]
        );

        std::fs::create_dir(folder.join("default")).unwrap();
        assert_eq!(
            refused(&mine)
                .iter()
                .map(|refused| refused.name.as_str())
                .collect::<Vec<_>>(),
            ["default", "relative"]
        );
        assert_eq!(list(&mine, &Sandbox::default()), []);
    }

    /// With no profiles of yours (the config folder is not a full path), nothing is made, anywhere,
    /// and new says why; the list is taw's own default alone.
    #[test]
    fn new_makes_nothing_where_taw_reads_no_profiles_of_yours() {
        let dir = tempfile::tempdir().unwrap();
        for config_dir in [PathBuf::new(), PathBuf::from("cfg")] {
            let mut paths = paths(dir.path());
            paths.config_dir = config_dir.clone();

            assert_eq!(
                new_profile(&paths, "mine", None).unwrap_err().to_string(),
                "TAW_CONFIG_DIR is not a full path, so taw reads no profiles of yours: set it to a full path to make one",
                "{config_dir:?}"
            );
            assert!(!std::path::Path::new("cfg").exists());
            assert!(!std::path::Path::new("sandbox").exists());
            assert_eq!(
                list(&paths, &Sandbox::default()),
                [Listed {
                    name: "default".to_string(),
                    path: builtin_path(&paths),
                    yours: false,
                    default: true,
                    agents: vec![]
                }]
            );
        }
    }

    /// With one folder for config and state, a default you made there is replaced by taw's own the
    /// next time taw runs it: new default refuses and says why, and another name is made.
    #[test]
    fn new_default_is_refused_where_taw_writes_its_own_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = paths(dir.path());
        paths.state_dir = paths.config_dir.clone();

        assert_eq!(
            new_profile(&paths, "default", None)
                .unwrap_err()
                .to_string(),
            format!(
                "taw writes its own default to {}, since your config folder and taw's state folder are one, so a default of yours would be replaced there: give your copy another name",
                yours_dir(&paths).unwrap().display()
            )
        );
        assert!(!builtin_path(&paths).exists());
        assert!(new_profile(&paths, "mine", None).is_ok());
    }

    /// A profiles folder that is a link to a folder that is not there (stow folds `sandbox/` into
    /// one) is refused in its own words, and nothing is made through it.
    #[test]
    fn new_through_a_profiles_folder_whose_link_is_gone_makes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let folder = yours_dir(&paths).unwrap();
        std::os::unix::fs::symlink(dir.path().join("dotfiles-moved").join("sandbox"), &folder)
            .unwrap();

        assert_eq!(
            new_profile(&paths, "mine", None).unwrap_err().to_string(),
            format!(
                "your profiles folder {0} is a link to a folder that is not there (ls -ld {0})",
                folder.display()
            )
        );
        assert!(!dir.path().join("dotfiles-moved").exists());
    }
}
