// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! Which agent, if any, a pane is running.
//!
//! Identity comes from the pane's command and the `@agent_kind_hint` option, and, when the
//! command is a container runtime, from the arguments the watcher reads off it (see
//! [`super::container`]). Screen content never decides it: herdr does not infer identity
//! from the screen either, and doing so would let a transcript impersonate an agent.

use std::{collections::HashMap, sync::OnceLock};

use crate::detect::engine::manifest_identity;

/// `(agent id, manifest source)` for the 22 agents herdr ships screen rules for,
/// in the order of herdr's `index.toml` catalog. Every file is named after its id.
pub(crate) const BUNDLED: &[(&str, &str)] = &[
    ("amp", include_str!("manifests/amp.toml")),
    ("agy", include_str!("manifests/agy.toml")),
    ("claude", include_str!("manifests/claude.toml")),
    ("cline", include_str!("manifests/cline.toml")),
    ("codex", include_str!("manifests/codex.toml")),
    ("cursor", include_str!("manifests/cursor.toml")),
    ("devin", include_str!("manifests/devin.toml")),
    ("droid", include_str!("manifests/droid.toml")),
    ("gemini", include_str!("manifests/gemini.toml")),
    ("grok", include_str!("manifests/grok.toml")),
    ("hermes", include_str!("manifests/hermes.toml")),
    ("kilo", include_str!("manifests/kilo.toml")),
    ("kimi", include_str!("manifests/kimi.toml")),
    ("kiro", include_str!("manifests/kiro.toml")),
    ("letta", include_str!("manifests/letta.toml")),
    ("maki", include_str!("manifests/maki.toml")),
    ("opencode", include_str!("manifests/opencode.toml")),
    ("pi", include_str!("manifests/pi.toml")),
    ("qodercli", include_str!("manifests/qodercli.toml")),
    ("qwen", include_str!("manifests/qwen.toml")),
    ("copilot", include_str!("manifests/copilot.toml")),
    ("muse", include_str!("manifests/muse.toml")),
];

/// The two agents herdr publishes under a file name that is not `<id>.toml`.
const REMOTE_PATH_EXCEPTIONS: &[(&str, &str)] = &[
    ("agy", "antigravity.toml"),
    ("copilot", "github-copilot.toml"),
];

/// The file name an agent's manifest is published under, relative to the base URL.
pub(crate) fn remote_path(id: &str) -> String {
    REMOTE_PATH_EXCEPTIONS
        .iter()
        .find(|(agent, _)| *agent == id)
        .map(|(_, path)| (*path).to_string())
        .unwrap_or_else(|| format!("{id}.toml"))
}

/// Canonical interactive executables, from herdr `detect::interactive_agent_executable`.
/// Only the entries that differ from the agent id matter; the rest are listed so a
/// rename upstream is a one-line change here.
const CANONICAL_EXECUTABLES: &[(&str, &str)] = &[
    ("pi", "pi"),
    ("claude", "claude"),
    ("codex", "codex"),
    ("gemini", "gemini"),
    // herdr lists "cursor-agent.cmd" for Windows; `normalize` strips the extension
    // before the table is consulted, so the bare name covers both.
    ("cursor-agent", "cursor"),
    ("devin", "devin"),
    ("agy", "agy"),
    ("cline", "cline"),
    // Cline's native binary. `normalize` drops launcher extensions only, so the
    // dot-prefixed name needs its own entry (herdr `identify_agent`: `"cline" | ".cline"`).
    (".cline", "cline"),
    ("opencode", "opencode"),
    ("copilot", "copilot"),
    ("kimi", "kimi"),
    ("kiro-cli", "kiro"),
    ("droid", "droid"),
    ("amp", "amp"),
    ("grok", "grok"),
    ("hermes", "hermes"),
    ("kilo", "kilo"),
    ("qodercli", "qodercli"),
    ("qwen", "qwen"),
    ("letta", "letta"),
    ("maki", "maki"),
    ("muse", "muse"),
];

/// Names herdr's `lookup_agent` knows that no bundled manifest lists among its `aliases`, with the id of
/// the agent each is. `opencode2` is OpenCode 2's preview binary (herdr v0.8.0, #2169).
const UNLISTED_ALIASES: &[(&str, &str)] = &[("opencode2", "opencode")];

/// Every agent id with a bundled manifest.
pub fn bundled_ids() -> &'static [&'static str] {
    static IDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    IDS.get_or_init(|| BUNDLED.iter().map(|(id, _)| *id).collect())
}

/// The command an agent starts with on this machine: the first entry of the canonical table that
/// names it, or its id. `cursor` starts as `cursor-agent`, `kiro` as `kiro-cli`.
pub fn executable(id: &str) -> String {
    CANONICAL_EXECUTABLES
        .iter()
        .find(|(_, agent)| *agent == id)
        .map_or(id, |(command, _)| *command)
        .to_string()
}

/// Which agent a pane is running, or `None` for a plain shell or unknown program.
///
/// `kind_hint` is the pane's `@agent_kind_hint` option and wins when it names a
/// bundled manifest — `taw agent start` sets it before the agent has even printed
/// its first byte. Otherwise the executable basename is matched against every
/// manifest's `id` and `aliases`, the canonical executable table and the names herdr
/// knows that no manifest lists, after herdr's normalisation: lowercased, with one
/// `.exe`/`.cmd`/`.bat`/`.ps1`/`.js` suffix dropped.
pub fn identify(pane_current_command: &str, kind_hint: Option<&str>) -> Option<String> {
    if let Some(hint) = kind_hint.map(str::trim).filter(|hint| !hint.is_empty())
        && let Some(id) = lookup(hint)
    {
        return Some(id);
    }
    lookup(basename(pane_current_command))
}

/// The bundled manifest text for an agent id.
pub(crate) fn bundled_manifest_text(id: &str) -> Option<&'static str> {
    BUNDLED
        .iter()
        .find(|(bundled_id, _)| *bundled_id == id)
        .map(|(_, toml)| *toml)
}

fn lookup(name: &str) -> Option<String> {
    let name = normalize(name);
    if name.is_empty() {
        return None;
    }
    if let Some((id, _)) = BUNDLED.iter().find(|(id, _)| **id == name) {
        return Some((*id).to_string());
    }
    if let Some(id) = alias_index().get(&name) {
        return Some((*id).to_string());
    }
    if let Some((_, id)) = CANONICAL_EXECUTABLES
        .iter()
        .chain(UNLISTED_ALIASES)
        .find(|(executable, _)| **executable == name)
    {
        return Some((*id).to_string());
    }
    is_muse_versioned_binary(&name).then(|| "muse".to_string())
}

/// Follows herdr's `is_muse_versioned_binary`. Muse's install-dir launcher
/// script resolves the active release and execs `muse-bin-<version>` (e.g.
/// `muse-bin-0.1.0-R708.1`), so the running process never carries a bare
/// `muse`/`muse-bin` alias — `muse.toml`'s own `aliases` cannot cover it. Require
/// a digit immediately after the `muse-bin-` prefix so unrelated binaries such as
/// `muse-binary` or a bare `muse-bin` stay unmatched. Re-basenames defensively,
/// matching herdr, since `lookup`'s `hint` caller does not basename before
/// normalizing.
fn is_muse_versioned_binary(name: &str) -> bool {
    basename(name)
        .strip_prefix("muse-bin-")
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
}

/// herdr's `normalized_agent_lookup_name`: lowercase, then drop one launcher
/// extension. Every id and alias the tables hold is already lowercase and bare.
pub(super) fn normalize(name: &str) -> String {
    let mut name = name.trim().to_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".ps1", ".js"] {
        if name.ends_with(suffix) {
            name.truncate(name.len() - suffix.len());
            break;
        }
    }
    name
}

/// Alias → id, read once from the bundled manifests so the table cannot drift.
fn alias_index() -> &'static HashMap<String, &'static str> {
    static INDEX: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut index = HashMap::new();
        for (id, toml) in BUNDLED {
            let Some((_, aliases)) = manifest_identity(toml) else {
                continue;
            };
            for alias in aliases {
                index.entry(alias).or_insert(*id);
            }
        }
        index
    })
}

/// herdr's `path_basename`: the last non-empty path component, so a trailing slash
/// does not swallow the name.
pub(super) fn basename(command: &str) -> &str {
    let trimmed = command.trim();
    trimmed
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test net the gate puts first on PATH (scripts/agent-net.sh) has a refusing stub for every name
    /// taw knows an agent by: the bundled manifests' ids and aliases, the canonical executables, the names
    /// no manifest lists, the agents of `src/detect/job.rs`'s own table and those taw installs an
    /// integration for; for every agent taw does not know that a test saves or reports as a pane's agent,
    /// so that a restore that resumed it would find the net (herdr's Prime Agent, which reports itself);
    /// and for every container runtime taw recognises. A name with a space or a colon in it is none a test
    /// types as the command's one word, and the net has none for it.
    ///
    /// And for nothing else but three agents no table here knows, which a test could type all the same:
    /// the list is of agents and runtimes. A program that reaches the desktop of whoever runs the tests (a
    /// notifier, a sound player) has a stub in the desktop net (scripts/desktop-net-names.txt), which
    /// a test of src/notify holds to the programs taw runs.
    #[test]
    fn the_gates_net_has_a_stub_for_every_name_taw_knows_an_agent_or_a_runtime_by() {
        let net: std::collections::HashSet<&str> =
            include_str!("../../scripts/agent-net-names.txt")
                .lines()
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .collect();
        let mut names: Vec<String> = Vec::new();
        for (id, toml) in BUNDLED {
            names.push((*id).to_string());
            if let Some((_, aliases)) = manifest_identity(toml) {
                names.extend(aliases);
            }
        }
        names.extend(
            CANONICAL_EXECUTABLES
                .iter()
                .chain(UNLISTED_ALIASES)
                .map(|(name, _)| (*name).to_string()),
        );
        names.extend(["omp", "mastracode", "mastra-code"].map(str::to_string));
        names.push("prime-agent".to_string());
        names.extend(
            crate::integration::AGENTS
                .iter()
                .map(|agent| agent.to_string()),
        );
        names.extend(
            crate::detect::container::RUNTIMES
                .iter()
                .map(|(name, _)| (*name).to_string()),
        );
        let missing: Vec<&String> = names
            .iter()
            .filter(|name| !name.contains([' ', ':']) && !net.contains(name.as_str()))
            .collect();
        assert!(missing.is_empty(), "the net has no stub for {missing:?}");
        let unknown_agents = ["aider", "crush", "goose"];
        let mut extra: Vec<&&str> = net
            .iter()
            .filter(|stub| {
                !unknown_agents.contains(*stub) && !names.iter().any(|name| name == **stub)
            })
            .collect();
        extra.sort();
        assert!(
            extra.is_empty(),
            "the net has a stub for {extra:?}, which is no agent or runtime taw knows by name"
        );
    }

    #[test]
    fn bundled_ids_match_their_file_names_and_manifest_ids() {
        assert_eq!(bundled_ids().len(), 22);
        for (bundled_id, toml) in BUNDLED {
            let (id, _) = manifest_identity(toml)
                .unwrap_or_else(|| panic!("{bundled_id} has no readable manifest header"));
            assert_eq!(&id, bundled_id, "manifest id disagrees with bundled id");
        }
    }

    #[test]
    fn only_herdrs_two_renamed_manifests_leave_the_id_naming_scheme() {
        assert_eq!(remote_path("claude"), "claude.toml");
        assert_eq!(remote_path("agy"), "antigravity.toml");
        assert_eq!(remote_path("copilot"), "github-copilot.toml");
    }

    #[test]
    fn identify_matches_ids_aliases_and_canonical_executables() {
        assert_eq!(identify("claude", None).as_deref(), Some("claude"));
        assert_eq!(identify("claude-code", None).as_deref(), Some("claude"));
        assert_eq!(identify("cursor-agent", None).as_deref(), Some("cursor"));
        assert_eq!(identify("kiro-cli", None).as_deref(), Some("kiro"));
        assert_eq!(identify("antigravity", None).as_deref(), Some("agy"));
        assert_eq!(identify("github-copilot", None).as_deref(), Some("copilot"));
    }

    #[test]
    fn identify_reads_the_last_non_empty_path_component() {
        assert_eq!(
            identify("/opt/homebrew/bin/claude", None).as_deref(),
            Some("claude")
        );
        assert_eq!(identify(r"C:\tools\codex", None).as_deref(), Some("codex"));
        // A trailing separator must not swallow the name.
        assert_eq!(
            identify("/usr/bin/claude/", None).as_deref(),
            Some("claude")
        );
        assert_eq!(identify(r"C:\tools\codex\", None).as_deref(), Some("codex"));
    }

    #[test]
    fn identify_normalises_case_and_launcher_extensions_like_herdr() {
        assert_eq!(identify("Claude", None).as_deref(), Some("claude"));
        assert_eq!(identify("CODEX", None).as_deref(), Some("codex"));
        assert_eq!(identify("claude.exe", None).as_deref(), Some("claude"));
        assert_eq!(
            identify("cursor-agent.cmd", None).as_deref(),
            Some("cursor")
        );
        assert_eq!(identify("Claude-Code.BAT", None).as_deref(), Some("claude"));
        assert_eq!(identify("opencode.js", None).as_deref(), Some("opencode"));
        assert_eq!(identify("kiro-cli.ps1", None).as_deref(), Some("kiro"));
        assert_eq!(
            identify(r"C:\Program Files\claude\CLAUDE.EXE", None).as_deref(),
            Some("claude")
        );
        // Only one suffix is dropped, and only a known one.
        assert_eq!(identify("claude.exe.exe", None), None);
        assert_eq!(identify("claude.sh", None), None);
        assert_eq!(
            identify("Claude", Some("CLAUDE.exe")).as_deref(),
            Some("claude")
        );
    }

    /// herdr's `identify_known_agents` (v0.9.3 `src/detect/mod.rs`), widened to every name its
    /// `lookup_agent` table knows: each is the agent herdr says it is, by the id taw's manifests give
    /// it. `omp` and `mastracode` have no manifest here, so only the job reader names them (`job.rs`).
    #[test]
    fn identify_knows_every_name_herdrs_lookup_table_knows() {
        for (name, id) in [
            ("pi", "pi"),
            ("claude", "claude"),
            ("claude-code", "claude"),
            ("codex", "codex"),
            ("gemini", "gemini"),
            ("cursor", "cursor"),
            ("cursor-agent", "cursor"),
            ("devin", "devin"),
            ("devin-cli", "devin"),
            ("devin cli", "devin"),
            ("agy", "agy"),
            ("antigravity", "agy"),
            ("antigravity-cli", "agy"),
            ("cline", "cline"),
            (".cline", "cline"),
            ("opencode", "opencode"),
            ("opencode.exe", "opencode"),
            ("opencode2", "opencode"),
            ("opencode2.exe", "opencode"),
            ("/usr/local/bin/opencode2", "opencode"),
            ("open-code", "opencode"),
            ("copilot", "copilot"),
            ("github-copilot", "copilot"),
            ("ghcs", "copilot"),
            ("kimi", "kimi"),
            ("kimi-code", "kimi"),
            ("Kimi Code", "kimi"),
            ("kiro", "kiro"),
            ("kiro-cli", "kiro"),
            ("droid", "droid"),
            ("amp", "amp"),
            ("amp-local", "amp"),
            ("grok", "grok"),
            ("grok-build", "grok"),
            ("hermes", "hermes"),
            ("hermes-agent", "hermes"),
            ("kilo", "kilo"),
            ("kilo-code", "kilo"),
            ("kilo code", "kilo"),
            ("qodercli", "qodercli"),
            ("qoderclicn", "qodercli"),
            ("qoder", "qodercli"),
            ("qodercn", "qodercli"),
            ("qwen", "qwen"),
            ("qwen-code", "qwen"),
            ("Qwen Code", "qwen"),
            ("letta", "letta"),
            ("letta-code", "letta"),
            ("Letta Code", "letta"),
            ("maki", "maki"),
            ("muse", "muse"),
            ("muse-code", "muse"),
            ("muse-cli", "muse"),
            ("muse-bin-0.1.0-R708.1", "muse"),
        ] {
            assert_eq!(identify(name, None).as_deref(), Some(id), "{name}");
            // A hint is read the same way: `taw agent start --kind opencode2` is OpenCode.
            if !name.contains('/') {
                assert_eq!(
                    identify("zsh", Some(name)).as_deref(),
                    Some(id),
                    "hint {name}"
                );
            }
        }
        // A name that only starts like one is not it.
        assert_eq!(identify("opencode2-helper", None), None);
        assert_eq!(identify("opencode22", None), None);
    }

    #[test]
    fn identify_rejects_plain_programs() {
        assert_eq!(identify("docker", None), None);
        assert_eq!(identify("zsh", None), None);
        assert_eq!(identify("", None), None);
        assert_eq!(identify("/", None), None);
        assert_eq!(identify("   ", None), None);
    }

    #[test]
    fn a_hint_wins_over_the_command_but_only_when_it_names_a_manifest() {
        assert_eq!(
            identify("docker", Some("claude")).as_deref(),
            Some("claude")
        );
        assert_eq!(
            identify("docker", Some("claude-code")).as_deref(),
            Some("claude")
        );
        // A hint that names nothing falls through to the command.
        assert_eq!(
            identify("codex", Some("nonsense")).as_deref(),
            Some("codex")
        );
        assert_eq!(identify("docker", Some("")), None);
    }

    /// herdr's muse support: `muse`/`muse-code`/`muse-cli` are the launcher aliases
    /// (declared in `muse.toml`'s own `aliases`), but the process actually running is
    /// muse's install-dir launcher having already `exec`'d the versioned release
    /// binary — `muse-bin-<version>` — which carries no bare `muse` alias at all.
    /// `is_muse_versioned_binary` recognizes that shape instead.
    #[test]
    fn identify_matches_muse_and_its_versioned_binary() {
        assert_eq!(identify("muse", None).as_deref(), Some("muse"));
        assert_eq!(identify("muse-code", None).as_deref(), Some("muse"));
        assert_eq!(identify("muse-cli", None).as_deref(), Some("muse"));
        assert_eq!(
            identify("muse-bin-0.1.0-R708.1", None).as_deref(),
            Some("muse")
        );
        assert_eq!(identify("muse-bin-1.2.3", None).as_deref(), Some("muse"));
        assert_eq!(
            identify("/home/user/.local/bin/muse-bin-0.2.1-R1215.1", None).as_deref(),
            Some("muse")
        );
        assert_eq!(
            identify(r"C:\Users\user\muse-bin-0.2.1-R1215.1.exe", None).as_deref(),
            Some("muse")
        );
    }

    /// herdr v0.9.1: Cline's npm package ships a native binary named `.cline`. `normalize`
    /// drops launcher extensions only, so the dot-prefixed name is a lookup of its own
    /// (herdr's `identify_agent`: `"cline" | ".cline"`). Adapted from herdr's
    /// `identify_agent_in_job_detects_cline_native_binaries`: taw sees the process name
    /// and path tmux reports, not the argv.
    #[test]
    fn identify_matches_clines_native_binary() {
        for command in [
            ".cline",
            "/home/user/.npm/lib/node_modules/cline/bin/.cline",
            "cline",
            "/usr/local/lib/node_modules/@cline/cli-darwin-arm64/bin/cline",
            "cline.exe",
            r"C:\Users\user\AppData\Roaming\npm\node_modules\@cline\cli-windows-x64\bin\cline.exe",
        ] {
            assert_eq!(
                identify(command, None).as_deref(),
                Some("cline"),
                "{command}"
            );
        }
    }

    /// herdr v0.9.1's Letta Code: `letta` is its executable, `letta-code` and `letta code`
    /// are the manifest's own aliases (herdr's `identify_agent`). herdr also reads a
    /// process's argv to skip non-interactive runs such as `letta -p "..."`; tmux gives
    /// taw only the command's name, so a one-shot run reads as a Letta pane until it exits.
    #[test]
    fn identify_matches_letta_and_its_aliases() {
        for command in [
            "letta",
            "Letta",
            "letta-code",
            "Letta Code",
            "/usr/local/bin/letta",
            "letta.exe",
        ] {
            assert_eq!(
                identify(command, None).as_deref(),
                Some("letta"),
                "{command}"
            );
        }
        // `taw agent start --kind letta` is a hint, and a hint that names a manifest wins.
        assert_eq!(identify("zsh", Some("letta")).as_deref(), Some("letta"));
    }

    #[test]
    fn identify_rejects_letta_lookalikes() {
        for command in [
            "lett",
            "lettabox",
            "letta-helper",
            "letta-server",
            "netletta",
        ] {
            assert_eq!(identify(command, None), None, "{command}");
        }
    }

    /// herdr's `identify_agent_in_job_rejects_unrelated_cline_mentions`, less the cases
    /// that read argv.
    #[test]
    fn identify_rejects_cline_lookalikes() {
        for command in [
            ".cline-helper",
            "/path/to/.cline-helper",
            "cline-helper",
            ".clines",
            "MainThread",
        ] {
            assert_eq!(identify(command, None), None, "{command}");
        }
    }

    #[test]
    fn identify_rejects_muse_lookalikes() {
        assert_eq!(identify("museum", None), None);
        assert_eq!(identify("muse-helper", None), None);
        assert_eq!(identify("muser", None), None);
        assert_eq!(identify("musescore", None), None);
        assert_eq!(identify("muse-bin", None), None);
        assert_eq!(identify("muse-bin-", None), None);
        assert_eq!(identify("muse-binary", None), None);
    }

    #[test]
    fn an_agent_starts_with_its_canonical_command_or_its_id() {
        assert_eq!(executable("cursor"), "cursor-agent");
        assert_eq!(executable("kiro"), "kiro-cli");
        assert_eq!(executable("cline"), "cline");
        assert_eq!(executable("claude"), "claude");
        for id in bundled_ids() {
            assert_eq!(
                identify(&executable(id), None).as_deref(),
                Some(*id),
                "{id}"
            );
        }
    }
}
