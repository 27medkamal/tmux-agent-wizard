// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! OpenCode's plugin lists: what taw's install adds itself to, and its uninstall takes itself
//! out of.
//!
//! OpenCode reads a TUI plugin from a list in its own config files. Version 1 keeps it in
//! the `plugin` array of `tui.jsonc`, or of `tui.json` (herdr #4250: a user who already has a
//! `tui.json` keeps their registration there); version 2 keeps it in the `plugins` array of
//! `cli.json`. All three are JSONC, so they may carry comments and trailing commas that a
//! rewrite through `serde_json` would drop. The lists are edited through `jsonc_parser`'s
//! comment-keeping tree, as herdr edits them, so the comments, the commas and the layout of
//! the rest of the file come through.
//!
//! An entry in a list is a string (`"./taw-tui-session.js"`), an object whose `package` is
//! that string, or a tuple whose first item is (`["./taw-tui-session.js", {...options}]`), as
//! herdr reads them.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstArray, CstInputValue, CstObject, CstObjectProp, CstRootNode};
use serde_json::Value;

use super::{
    ConfigEdit, IntegrationError, check_config_target, plan_text_edit_checked, uninstall_text_edit,
};

/// Version 1's TUI config files, in the order OpenCode is asked: `tui.jsonc` is the one taw
/// creates, and `tui.json` is only ever added to when it already holds the registration.
pub(super) const TUI_CONFIGS: [&str; 2] = ["tui.jsonc", "tui.json"];
/// Version 2's config file.
pub(super) const CLI_CONFIG: &str = "cli.json";
/// The array of a TUI config that lists its plugins.
pub(super) const TUI_KEY: &str = "plugin";
/// The array of `cli.json` that lists its plugins.
pub(super) const CLI_KEY: &str = "plugins";

// ---------------------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------------------

/// JSONC as OpenCode reads it: JSON, plus comments and a comma after the last item of an
/// object or an array. Nothing looser: no single quotes, no bare keys, no missing commas, no
/// hex numbers. herdr's options.
fn jsonc_parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: false,
        allow_trailing_commas: true,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

fn parse_root(text: &str) -> std::result::Result<CstRootNode, String> {
    CstRootNode::parse(text, &jsonc_parse_options())
        .map_err(|err| format!("it cannot be read as JSONC ({err})"))
}

fn root_object(root: &CstRootNode) -> std::result::Result<CstObject, String> {
    root.value()
        .and_then(|value| value.as_object())
        .ok_or_else(|| "the config must be a JSON object".to_string())
}

/// The property `key` of `object` and the array it holds: `None` when there is no such
/// property, and an error when it holds something other than an array.
fn list_of(
    object: &CstObject,
    key: &str,
) -> std::result::Result<Option<(CstObjectProp, CstArray)>, String> {
    let Some(property) = object.get(key) else {
        return Ok(None);
    };
    let list = property
        .array_value()
        .ok_or_else(|| format!("`{key}` must be an array"))?;
    Ok(Some((property, list)))
}

/// herdr's `plugin_entry_matches`: the string itself, an object whose `package` is it, or a
/// tuple that starts with it.
fn plugin_entry_matches(entry: &Value, spec: &str) -> bool {
    entry.as_str() == Some(spec)
        || entry.get("package").and_then(Value::as_str) == Some(spec)
        || entry
            .as_array()
            .and_then(|parts| parts.first())
            .and_then(Value::as_str)
            == Some(spec)
}

/// Whether `list` has an entry that registers `spec`, in any of the three forms.
fn registers(list: &CstArray, spec: &str) -> bool {
    list.elements().iter().any(|entry| {
        entry
            .to_serde_value()
            .is_some_and(|entry| plugin_entry_matches(&entry, spec))
    })
}

/// `text` is a JSONC object whose `key`, when it has one, is an array.
pub(super) fn validate_list(text: &str, key: &str) -> std::result::Result<(), String> {
    let root = parse_root(text)?;
    list_of(&root_object(&root)?, key).map(|_| ())
}

/// Whether the list `key` of `text` already holds `spec`, in any of the three forms. Anything
/// that cannot be read holds nothing.
pub(super) fn is_registered(text: &str, key: &str, spec: &str) -> bool {
    let Ok(root) = parse_root(text) else {
        return false;
    };
    let Ok(object) = root_object(&root) else {
        return false;
    };
    matches!(list_of(&object, key), Ok(Some((_, list))) if registers(&list, spec))
}

/// `text` with the list `key` given `spec`: after the list's last entry, or as a new property
/// of the object when it has no such list. The config is returned as it is when `spec` is
/// already in the list, in any form.
pub(super) fn add_entry(text: &str, key: &str, spec: &str) -> std::result::Result<String, String> {
    let root = parse_root(text)?;
    let object = root_object(&root)?;
    match list_of(&object, key)? {
        Some((_, list)) if registers(&list, spec) => return Ok(text.to_string()),
        Some((_, list)) => {
            list.append(CstInputValue::String(spec.to_string()));
        }
        None => {
            object.append(
                key,
                CstInputValue::Array(vec![CstInputValue::String(spec.to_string())]),
            );
        }
    }
    Ok(root.to_string())
}

/// `text` without any entry of the list `key` that registers `spec`, and without the list
/// when that leaves it empty, as herdr does. Text that cannot be read, or that registers
/// nothing, comes back as it is.
pub(super) fn remove_entry(text: &str, key: &str, spec: &str) -> String {
    let Ok(root) = parse_root(text) else {
        return text.to_string();
    };
    let Ok(object) = root_object(&root) else {
        return text.to_string();
    };
    let Ok(Some((property, list))) = list_of(&object, key) else {
        return text.to_string();
    };
    let mut removed = false;
    for entry in list.elements() {
        if entry
            .to_serde_value()
            .is_some_and(|entry| plugin_entry_matches(&entry, spec))
        {
            entry.remove();
            removed = true;
        }
    }
    if !removed {
        return text.to_string();
    }
    if list.elements().is_empty() {
        property.remove();
    }
    root.to_string()
}

/// How many times [`settled`] makes its trip before it takes the text as it is: the second is
/// enough for any layout that has been tried, and this is the limit for one that has not.
const SETTLING_TRIPS: usize = 4;

/// `text` as it comes out of an add and a remove of `spec`, made again until nothing changes,
/// for comparing two configs that differ only by what those trips change, which is not the
/// user's doing: the tree writes a one-line object over several lines when it adds a property
/// to it, an empty list or an object with nothing in it is what a removal leaves behind where
/// install made one, and a blank line before a closing bracket goes on the second trip. A blank
/// config is read as `{}`, as install reads it, and text that cannot be read comes back as it
/// is.
pub(super) fn settled(text: &str, key: &str, spec: &str) -> String {
    let mut current = text.to_string();
    for _ in 0..SETTLING_TRIPS {
        let Ok(added) = add_entry(base_of(&current), key, spec) else {
            return current;
        };
        let next = remove_entry(&added, key, spec);
        if next == current {
            break;
        }
        current = next;
    }
    current
}

// ---------------------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------------------

/// The file at `path` as text, or `None` when there is none.
fn read_config(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    std::fs::read_to_string(path)
        .map(Some)
        .with_context(|| format!("reading {}", path.display()))
}

fn unusable(path: &Path, detail: String) -> anyhow::Error {
    IntegrationError::UnusableConfig {
        path: path.display().to_string(),
        detail,
    }
    .into()
}

/// The `{}` a config that is not there, or is blank, starts from.
fn base_of(text: &str) -> &str {
    if text.trim().is_empty() { "{}\n" } else { text }
}

/// Every config taw may edit answers `check_config_target`: not a directory, not a second
/// name for a file with other hard links, and writable when it exists.
pub(super) fn check_targets(dir: &Path) -> Result<()> {
    for name in TUI_CONFIGS.iter().chain([&CLI_CONFIG]) {
        check_config_target(&dir.join(name))?;
    }
    Ok(())
}

/// Every list taw may add to is one it can add to.
pub(super) fn validate_configs(dir: &Path) -> Result<()> {
    let lists = [
        (TUI_CONFIGS[0], TUI_KEY),
        (TUI_CONFIGS[1], TUI_KEY),
        (CLI_CONFIG, CLI_KEY),
    ];
    for (name, key) in lists {
        let path = dir.join(name);
        let Some(text) = read_config(&path)? else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        validate_list(&text, key).map_err(|detail| unusable(&path, detail))?;
    }
    Ok(())
}

fn add_to(path: &Path, text: &str, key: &str, spec: &str) -> Result<String> {
    add_entry(base_of(text), key, spec).map_err(|detail| unusable(path, detail))
}

/// The edit that registers `spec` as a version 1 TUI plugin, or `None` when a TUI config
/// already does. Nothing is written.
pub(super) fn plan_tui_registration(
    dir: &Path,
    spec: &str,
    sidecar: impl Fn(&Path) -> PathBuf,
) -> Result<Option<ConfigEdit>> {
    for name in TUI_CONFIGS {
        let holds =
            read_config(&dir.join(name))?.is_some_and(|text| is_registered(&text, TUI_KEY, spec));
        if holds {
            return Ok(None);
        }
    }
    // Kept absent on a fresh install: OpenCode moves its old settings into a `tui.json` of
    // its own, and only while there is none.
    let path = dir.join(TUI_CONFIGS[0]);
    plan_text_edit_checked(&path, &sidecar(&path), |text| {
        add_to(&path, text, TUI_KEY, spec)
    })
    .map(Some)
}

/// Whether the version 2 registration has to wait: there is no `cli.json`, and OpenCode still
/// has version 1 settings (`tui.json`, or `kv.json` in its state directory) to move into one.
pub(super) fn cli_registration_deferred(dir: &Path, state_dir: &Path) -> bool {
    !dir.join(CLI_CONFIG).is_file()
        && (dir.join(TUI_CONFIGS[1]).is_file() || state_dir.join("kv.json").is_file())
}

/// The edit that registers `spec` as a version 2 plugin in `cli.json`, or `None` when it
/// already is or when OpenCode still has settings to move into a `cli.json` of its own.
/// Nothing is written.
pub(super) fn plan_cli_registration(
    dir: &Path,
    state_dir: &Path,
    spec: &str,
    sidecar: impl Fn(&Path) -> PathBuf,
) -> Result<Option<ConfigEdit>> {
    let path = dir.join(CLI_CONFIG);
    check_config_target(&path)?;
    match read_config(&path)? {
        Some(text) if is_registered(&text, CLI_KEY, spec) => return Ok(None),
        Some(_) => {}
        // OpenCode brings its version 1 TUI preferences (`tui.json`, `kv.json`) into `cli.json`
        // the first time version 2 starts, and only while there is no `cli.json`. Making one
        // first would skip that; when there is nothing to move, OpenCode never makes one, so
        // taw does.
        None => {
            if cli_registration_deferred(dir, state_dir) {
                return Ok(None);
            }
        }
    }
    plan_text_edit_checked(&path, &sidecar(&path), |text| {
        add_to(&path, text, CLI_KEY, spec)
    })
    .map(Some)
}

/// Takes `spec` out of one config, from the copy of it taw kept when it can be given back
/// whole and out of the file as it stands when it cannot. Says whether the file held it.
fn remove_from(
    path: &Path,
    key: &str,
    spec: &str,
    sidecar: &impl Fn(&Path) -> PathBuf,
) -> Result<bool> {
    let held = read_config(path)?.is_some_and(|text| is_registered(&text, key, spec));
    uninstall_text_edit(
        path,
        &sidecar(path),
        |text| remove_entry(text, key, spec),
        |text| settled(text, key, spec),
    )?;
    Ok(held)
}

/// Takes `spec` out of every TUI config, and says which files held it.
pub(super) fn remove_tui_registration(
    dir: &Path,
    spec: &str,
    sidecar: impl Fn(&Path) -> PathBuf,
) -> Result<Vec<PathBuf>> {
    let mut updated = Vec::new();
    for name in TUI_CONFIGS {
        let path = dir.join(name);
        if remove_from(&path, TUI_KEY, spec, &sidecar)? {
            updated.push(path);
        }
    }
    Ok(updated)
}

/// Takes `spec` out of `cli.json`, and says whether it held it.
pub(super) fn remove_cli_registration(
    dir: &Path,
    spec: &str,
    sidecar: impl Fn(&Path) -> PathBuf,
) -> Result<bool> {
    remove_from(&dir.join(CLI_CONFIG), CLI_KEY, spec, &sidecar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    const SPEC: &str = "./taw-tui-session.js";

    // -----------------------------------------------------------------------------------
    // reading JSONC
    // -----------------------------------------------------------------------------------

    /// `text` as the JSON it stands for: the comments and trailing commas gone.
    fn value_of(text: &str) -> Value {
        parse_root(text)
            .unwrap_or_else(|err| panic!("{err}: {text:?}"))
            .value()
            .and_then(|value| value.to_serde_value())
            .unwrap_or_else(|| panic!("no value: {text:?}"))
    }

    #[test]
    fn comments_and_trailing_commas_are_part_of_the_format() {
        let text = concat!(
            "// leading\n",
            "{\n",
            "  /* block */ \"theme\": \"system\", // trailing\n",
            "  \"plugin\": [\"example\", [\"configured\", {\"enabled\": true,}],],\n",
            "}\n",
            "// after\n",
        );
        assert!(validate_list(text, TUI_KEY).is_ok());
        assert_eq!(
            value_of(text),
            json!({
                "theme": "system",
                "plugin": ["example", ["configured", {"enabled": true}]],
            })
        );
    }

    #[test]
    fn what_opencode_does_not_read_is_refused() {
        for text in [
            "{'plugin': []}",
            "{plugin: []}",
            "{\"plugin\": [1 2]}",
            "{\"a\": 0x10}",
            "{\"a\": +1}",
            "{\"a\": 1} {\"b\": 2}",
            "{\"a\": 1",
            "not json",
            // A byte order mark is not read by the tree either, as in herdr.
            "\u{feff}{}",
        ] {
            assert!(
                validate_list(text, TUI_KEY).is_err(),
                "{text:?} must not be accepted"
            );
        }
    }

    #[test]
    fn a_config_that_is_not_an_object_is_refused_and_says_so() {
        for text in ["[]", "\"plugin\"", "3", "null", "// only a comment\n", ""] {
            let err = validate_list(text, TUI_KEY).unwrap_err();
            assert!(err.contains("object"), "{text:?}: {err}");
        }
    }

    #[test]
    fn a_list_that_is_not_an_array_is_refused_and_named() {
        let err = validate_list(r#"{"plugin": {}}"#, TUI_KEY).unwrap_err();
        assert!(err.contains("`plugin`") && err.contains("array"), "{err}");
        let err = validate_list(r#"{"plugins": "x"}"#, CLI_KEY).unwrap_err();
        assert!(err.contains("`plugins`") && err.contains("array"), "{err}");
        // A null is not an array either.
        assert!(validate_list(r#"{"plugin": null}"#, TUI_KEY).is_err());
        // A list under the other key is not this list.
        assert!(validate_list(r#"{"plugins": "x"}"#, TUI_KEY).is_ok());
        assert!(validate_list("{}", TUI_KEY).is_ok());
    }

    #[test]
    fn a_registration_is_found_in_each_of_the_forms_herdr_reads() {
        for (text, registered) in [
            (r#"{"plugin": ["./taw-tui-session.js"]}"#, true),
            (r#"{"plugin": ["a", "./taw-tui-session.js", "b"]}"#, true),
            (
                r#"{"plugin": [{"package": "./taw-tui-session.js", "options": {}}]}"#,
                true,
            ),
            (
                r#"{"plugin": [["./taw-tui-session.js", {"enabled": true}]]}"#,
                true,
            ),
            (r#"{"plugin": ["./taw-tui-session.jsx"]}"#, false),
            (r#"{"plugin": [["other", "./taw-tui-session.js"]]}"#, false),
            (r#"{"plugin": [{"name": "./taw-tui-session.js"}]}"#, false),
            (r#"{"plugin": []}"#, false),
            (r#"{"plugins": ["./taw-tui-session.js"]}"#, false),
            (r#"{}"#, false),
            (r#"{"plugin": ["./taw-tui-session.js"]"#, false),
            ("", false),
        ] {
            assert_eq!(is_registered(text, TUI_KEY, SPEC), registered, "{text}");
        }
    }

    // -----------------------------------------------------------------------------------
    // adding an entry
    // -----------------------------------------------------------------------------------

    fn add(text: &str) -> String {
        add_entry(text, TUI_KEY, SPEC).unwrap()
    }

    fn remove(text: &str) -> String {
        remove_entry(text, TUI_KEY, SPEC)
    }

    #[test]
    fn an_entry_goes_last_in_the_layout_the_config_already_has() {
        for (before, after) in [
            ("{}\n", "{\n  \"plugin\": [\"./taw-tui-session.js\"]\n}\n"),
            (
                "{\n  \"theme\": \"system\"\n}\n",
                "{\n  \"theme\": \"system\",\n  \"plugin\": [\"./taw-tui-session.js\"]\n}\n",
            ),
            (
                // A trailing comma stays where it was, after the new property.
                "{\n    \"theme\": \"system\",\n    \"mouse\": true,\n}\n",
                "{\n    \"theme\": \"system\",\n    \"mouse\": true,\n    \"plugin\": [\"./taw-tui-session.js\"],\n}\n",
            ),
            (
                "{\"plugin\":[\"a\",\"b\"]}",
                "{\"plugin\":[\"a\",\"b\", \"./taw-tui-session.js\"]}",
            ),
            (
                "{\n  \"plugin\": [\n    \"a\",\n    \"b\"\n  ]\n}\n",
                "{\n  \"plugin\": [\n    \"a\",\n    \"b\",\n    \"./taw-tui-session.js\"\n  ]\n}\n",
            ),
            (
                "{\n  \"plugin\": [\n    \"a\",\n    \"b\",\n  ],\n}\n",
                "{\n  \"plugin\": [\n    \"a\",\n    \"b\",\n    \"./taw-tui-session.js\",\n  ],\n}\n",
            ),
            (
                "{\n  \"plugin\": []\n}\n",
                "{\n  \"plugin\": [\"./taw-tui-session.js\"]\n}\n",
            ),
            (
                "{\n    \"theme\": \"system\",\n    \"plugin\": [\n        \"a\"\n    ]\n}\n",
                "{\n    \"theme\": \"system\",\n    \"plugin\": [\n        \"a\",\n        \"./taw-tui-session.js\"\n    ]\n}\n",
            ),
        ] {
            assert_eq!(add(before), after, "{before:?}");
        }
    }

    #[test]
    fn a_list_with_nothing_but_a_comment_gets_the_entry_after_it() {
        let added = add("{\n  \"plugin\": [\n    // none yet\n  ]\n}\n");
        assert_eq!(value_of(&added)["plugin"], json!([SPEC]));
        assert!(added.contains("// none yet"));
    }

    #[test]
    fn adding_keeps_the_comments_and_the_other_settings() {
        let text = concat!(
            "{\n",
            "  // Keep this comment.\n",
            "  \"theme\": \"system\", // and this one\n",
            "  \"plugin\": [\"example\", [\"configured\", {\"enabled\": true}]],\n",
            "}\n",
        );
        let added = add(text);
        assert_eq!(
            value_of(&added),
            json!({
                "theme": "system",
                "plugin": ["example", ["configured", {"enabled": true}], SPEC],
            })
        );
        // Nothing but the insertion changed.
        assert!(
            added.starts_with(
                "{\n  // Keep this comment.\n  \"theme\": \"system\", // and this one\n  \"plugin\": [\"example\", [\"configured\", {\"enabled\": true}], "
            ),
            "{added}"
        );
        assert!(added.ends_with("],\n}\n"), "{added}");
    }

    #[test]
    fn adding_is_idempotent_and_leaves_any_form_of_the_registration_alone() {
        for text in [
            "{\"plugin\": [\"./taw-tui-session.js\"]}",
            "{\"plugin\": [[\"./taw-tui-session.js\", {\"enabled\": true}]]}",
            "{\"plugin\": [{\"package\": \"./taw-tui-session.js\"}]}",
        ] {
            assert_eq!(add(text), text);
        }
        let once = add("{\"theme\": \"x\"}");
        assert_ne!(once, "{\"theme\": \"x\"}");
        assert_eq!(add(&once), once);
    }

    #[test]
    fn a_config_that_cannot_take_the_entry_is_refused() {
        assert!(
            add_entry("[]", TUI_KEY, SPEC)
                .unwrap_err()
                .contains("object")
        );
        assert!(
            add_entry("{\"plugin\": \"x\"}", TUI_KEY, SPEC)
                .unwrap_err()
                .contains("array")
        );
        assert!(add_entry("not json", TUI_KEY, SPEC).is_err());
    }

    #[test]
    fn a_file_keeps_its_own_line_ends_and_indent() {
        let added = add("{\r\n  \"theme\": \"system\"\r\n}\r\n");
        assert_eq!(
            added,
            "{\r\n  \"theme\": \"system\",\r\n  \"plugin\": [\"./taw-tui-session.js\"]\r\n}\r\n"
        );
        assert_eq!(remove(&added), "{\r\n  \"theme\": \"system\"\r\n}\r\n");

        let added = add("{\r\n  \"plugin\": [\r\n    \"a\"\r\n  ]\r\n}\r\n");
        assert!(!added.replace("\r\n", "").contains('\n'), "{added:?}");
        assert_eq!(
            remove(&added),
            "{\r\n  \"plugin\": [\r\n    \"a\"\r\n  ]\r\n}\r\n"
        );

        assert_eq!(
            add("{\n\t\"theme\": \"system\"\n}\n"),
            "{\n\t\"theme\": \"system\",\n\t\"plugin\": [\"./taw-tui-session.js\"]\n}\n"
        );
    }

    #[test]
    fn only_the_top_level_list_counts_not_one_of_the_same_name_further_down() {
        let text = "{\"editor\": {\"plugin\": [\"./taw-tui-session.js\"]}}";
        assert!(!is_registered(text, TUI_KEY, SPEC));
        let added = add(text);
        assert_eq!(
            value_of(&added),
            json!({"editor": {"plugin": [SPEC]}, "plugin": [SPEC]})
        );
        assert_eq!(value_of(&remove(&added)), value_of(text));
    }

    #[test]
    fn text_that_is_not_ascii_is_edited_without_a_slip() {
        let text = "{\n  \"theme\": \"日本語 ✓\", // コメント\n  \"plugin\": [\"é\"]\n}\n";
        let added = add(text);
        assert_eq!(value_of(&added)["plugin"], json!(["é", SPEC]));
        assert!(added.contains("// コメント"));
        assert_eq!(remove(&added), text);
        assert_eq!(
            remove("{\"plugin\": [\"é\", \"./taw-tui-session.js\", \"ü\"]} // }"),
            "{\"plugin\": [\"é\", \"ü\"]} // }"
        );
    }

    // -----------------------------------------------------------------------------------
    // removing an entry
    // -----------------------------------------------------------------------------------

    /// Configs as people lay them out, where what the tree adds it takes out again exactly.
    const EXACT_LAYOUTS: [&str; 20] = [
        "{}\n",
        "{}",
        "{\n  \"theme\": \"system\"\n}\n",
        "{\n  \"theme\": \"system\",\n  \"keybinds\": {}\n}\n",
        "{\n  \"theme\": \"system\",\n}\n",
        "{\n  // Keep this comment.\n  \"theme\": \"system\",\n  \"plugin\": [\"example\"],\n}\n",
        "{\"plugin\":[\"a\",\"b\"]}",
        "{\n  \"plugin\": [\n    \"a\",\n    \"b\"\n  ]\n}\n",
        "{\n  \"plugin\": [\n    \"a\",\n    \"b\",\n  ],\n}\n",
        "{\n  \"plugin\": [\"example\", [\"configured\", {\"enabled\": true}]],\n}\n",
        "{\r\n  \"theme\": \"system\"\r\n}\r\n",
        "{\n\t\"theme\": \"system\"\n}\n",
        "{\n  \"theme\": \"system\"\n} // end\n",
        "/* header */\n{\n  \"theme\": \"system\"\n}\n",
        "{\n  \"$schema\": \"https://opencode.ai/tui.json\",\n  \"theme\": \"tokyonight\"\n}\n",
        "{\n  // nothing yet\n}\n",
        "{\n  \"theme\": \"system\" // pick one\n}\n",
        "{\n  \"plugin\": [\n    // first\n    \"a\", // note a\n    /* second */ \"b\"\n  ]\n}\n",
        "{\n    \"theme\": \"system\",\n    \"plugin\": [\n        \"a\"\n    ]\n}\n",
        "{\n  \"keybinds\": {\n    \"leader\": \"ctrl+x\"\n  }\n}\n",
    ];

    /// Configs the tree lays out anew when it adds to them, so what it takes out is not what
    /// went in, only the same config: an object on one line is spread over several, and an
    /// empty list, or an empty object, is left out.
    const RELAID_LAYOUTS: [&str; 6] = [
        "{\"theme\":\"system\"}",
        "{\n}\n",
        "{ }\n",
        "{\n  \"plugin\": []\n}\n",
        "{\n  \"plugin\": [\n    // none yet\n  ]\n}\n",
        "{\"plugin\": []}",
    ];

    #[test]
    fn removing_undoes_adding_exactly_in_the_layouts_people_write() {
        for original in EXACT_LAYOUTS {
            let added = add(original);
            assert_ne!(added, original, "{original:?} was not changed");
            assert_eq!(remove(&added), original, "{original:?} -> {added:?}");
        }
    }

    #[test]
    fn a_config_the_tree_lays_out_anew_settles_to_the_same_text_before_and_after() {
        for original in RELAID_LAYOUTS {
            let added = add(original);
            let removed = remove(&added);
            assert_eq!(value_of(&added)["plugin"].as_array().map(Vec::len), Some(1));
            assert_eq!(
                settled(&removed, TUI_KEY, SPEC),
                settled(original, TUI_KEY, SPEC),
                "{original:?} -> {added:?} -> {removed:?}"
            );
        }
        // Blank is the `{}` install starts from.
        assert_eq!(settled("", TUI_KEY, SPEC), settled("{}\n", TUI_KEY, SPEC));
        assert_eq!(
            settled("  \n", TUI_KEY, SPEC),
            settled("{}\n", TUI_KEY, SPEC)
        );
        // Text that cannot be read is what it is.
        assert_eq!(settled("not json", TUI_KEY, SPEC), "not json");
    }

    #[test]
    fn what_the_user_wrote_is_not_settled_away() {
        // Each of these is a real difference between two configs, so each stays one.
        let base = settled("{\n  \"theme\": \"system\"\n}\n", TUI_KEY, SPEC);
        for other in [
            "{\n  \"theme\": \"tokyonight\"\n}\n",
            "{\n  // mine\n  \"theme\": \"system\"\n}\n",
            "{\n    \"theme\": \"system\"\n}\n",
            "{\n  \"theme\": \"system\",\n  \"plugin\": [\"a\"]\n}\n",
        ] {
            assert_ne!(settled(other, TUI_KEY, SPEC), base, "{other:?}");
        }
    }

    #[test]
    fn removing_takes_the_entry_and_one_comma_wherever_it_stands() {
        for (text, expected) in [
            (
                // The space that led to the entry stays: valid, and the tree's own.
                "{\"plugin\": [\"./taw-tui-session.js\", \"a\", \"b\"]}",
                "{\"plugin\": [ \"a\", \"b\"]}",
            ),
            (
                "{\"plugin\": [\"a\", \"./taw-tui-session.js\", \"b\"]}",
                "{\"plugin\": [\"a\", \"b\"]}",
            ),
            (
                "{\"plugin\": [\"a\", \"b\", \"./taw-tui-session.js\"]}",
                "{\"plugin\": [\"a\", \"b\"]}",
            ),
            (
                "{\"plugin\": [\"a\", \"./taw-tui-session.js\",]}",
                "{\"plugin\": [\"a\",]}",
            ),
            (
                "{\n  \"plugin\": [\n    \"a\",\n    \"./taw-tui-session.js\",\n    \"b\"\n  ]\n}\n",
                "{\n  \"plugin\": [\n    \"a\",\n    \"b\"\n  ]\n}\n",
            ),
            (
                "{\n  \"plugin\": [\n    \"./taw-tui-session.js\",\n    \"a\"\n  ]\n}\n",
                "{\n  \"plugin\": [\n    \"a\"\n  ]\n}\n",
            ),
        ] {
            let removed = remove(text);
            assert_eq!(removed, expected, "{text}");
            assert!(parse_root(&removed).is_ok(), "{removed}");
        }
    }

    #[test]
    fn removing_the_only_entry_drops_the_list_and_leaves_valid_json() {
        for (text, expected) in [
            ("{\"plugin\": [\"./taw-tui-session.js\"]}", "{}"),
            ("{\n  \"plugin\": [\"./taw-tui-session.js\"]\n}\n", "{}\n"),
            (
                "{\"theme\": \"x\", \"plugin\": [\"./taw-tui-session.js\"]}",
                "{\"theme\": \"x\"}",
            ),
            (
                "{\n  \"plugin\": [\"./taw-tui-session.js\"],\n  \"theme\": \"x\"\n}\n",
                "{\n  \"theme\": \"x\"\n}\n",
            ),
        ] {
            let removed = remove(text);
            assert_eq!(removed, expected, "{text}");
            assert!(parse_root(&removed).is_ok(), "{removed}");
        }
        // A comment on a line of its own stays, whatever it sits beside.
        let kept = remove("{\n  // note\n  \"plugin\": [\"./taw-tui-session.js\"]\n}\n");
        assert!(kept.contains("// note"), "{kept}");
        assert_eq!(value_of(&kept), json!({}));
    }

    #[test]
    fn removing_takes_every_form_of_the_registration_and_only_that() {
        let text = concat!(
            "{\"plugin\": [\"a\", [\"./taw-tui-session.js\", {\"x\": 1}], \"b\", ",
            "{\"package\": \"./taw-tui-session.js\"}, \"./taw-tui-session.js\", ",
            "\"./taw-tui-session.js\", \"c\"]}",
        );
        assert_eq!(value_of(&remove(text))["plugin"], json!(["a", "b", "c"]));
    }

    #[test]
    fn a_comment_written_beside_the_entry_goes_with_it_and_the_others_stay() {
        // The tree reads a comment on the entry's own line as the entry's, as herdr's does.
        let removed = remove(concat!(
            "{\n",
            "  // above the list\n",
            "  \"plugin\": [\n",
            "    // above a\n",
            "    \"a\", // beside a\n",
            "    \"./taw-tui-session.js\" // beside ours\n",
            "  ] // beside the list\n",
            "}\n",
        ));
        assert!(removed.contains("// above the list"), "{removed}");
        assert!(removed.contains("// above a"), "{removed}");
        assert!(removed.contains("// beside a"), "{removed}");
        assert!(!removed.contains("// beside ours"), "{removed}");
        assert!(removed.contains("// beside the list"), "{removed}");
        assert_eq!(value_of(&removed)["plugin"], json!(["a"]));
    }

    #[test]
    fn a_block_comment_after_the_last_entry_of_a_one_line_list_moves_with_the_entry_added() {
        // The tree reads a comment on the last entry's own line as that entry's, so an entry
        // added after it takes the comment along, and takes it out again when it goes. It is
        // the one comment an add and a remove do not leave where it was; the copy of the config
        // that install keeps is what gives it back to a config that nothing else has changed.
        let original = "{\"plugin\": [\"a\" /* note a */]}";
        let added = add(original);
        assert_eq!(
            added,
            "{\"plugin\": [\"a\", \"./taw-tui-session.js\" /* note a */]}"
        );
        assert_eq!(remove(&added), "{\"plugin\": [\"a\"]}");

        let dir = unique_dir();
        let config = dir.path().join("tui.jsonc");
        fs::write(&config, original).unwrap();
        register_tui(dir.path()).unwrap();
        remove_tui_registration(dir.path(), SPEC, sidecar).unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), original);
        // On its own line, or after a line comment, it stays where it was.
        for exact in [
            "{\n  \"plugin\": [\n    \"a\" /* note a */\n  ]\n}\n",
            "{\n  \"plugin\": [\n    \"a\" // note a\n  ]\n}\n",
        ] {
            assert_eq!(remove(&add(exact)), exact);
        }
    }

    #[test]
    fn removing_from_text_it_cannot_read_or_that_holds_nothing_changes_nothing() {
        for text in [
            "",
            "not json",
            "{\"plugin\": [\"a\"]}",
            "{}",
            "{\"plugin\": \"./taw-tui-session.js\"}",
            "{\"plugins\": [\"./taw-tui-session.js\"]}",
            "[\"./taw-tui-session.js\"]",
            // An empty list is left as it is when nothing was taken out of it.
            "{\"plugin\": []}",
        ] {
            assert_eq!(remove(text), text);
        }
    }

    #[test]
    fn the_two_keys_are_edited_independently() {
        let text = "{\"plugin\": [\"./taw-tui-session.js\"], \"plugins\": [\"./taw-tui-session.js\", \"x\"]}";
        let removed = remove_entry(text, CLI_KEY, SPEC);
        assert_eq!(
            value_of(&removed),
            json!({"plugin": [SPEC], "plugins": ["x"]})
        );
    }

    // -----------------------------------------------------------------------------------
    // any layout
    // -----------------------------------------------------------------------------------

    /// A small deterministic generator, so a failure names the document that broke it.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }

        fn pick<'a>(&mut self, options: &[&'a str]) -> &'a str {
            options[self.below(options.len())]
        }
    }

    /// White space and comments, anywhere JSONC allows them.
    fn trivia(rng: &mut Rng) -> &'static str {
        rng.pick(&[
            "",
            "",
            " ",
            "\n  ",
            "\n    ",
            "\n",
            "\t",
            "\r\n  ",
            " /* c */ ",
            " // c\n  ",
            "\n  // c\n  ",
        ])
    }

    /// What may follow the last item of a list or object before its closing bracket or its
    /// trailing comma. Not a block comment on the item's own line: the tree reads that as the
    /// item's and moves it to whatever is added after it (see
    /// `a_block_comment_after_the_last_entry_of_a_one_line_list_moves_with_the_entry_added`).
    fn closing_trivia(rng: &mut Rng) -> &'static str {
        rng.pick(&[
            "",
            "",
            " ",
            "\n  ",
            "\n",
            "\t",
            "\r\n  ",
            " // c\n  ",
            "\n  // c\n  ",
        ])
    }

    fn entry(rng: &mut Rng) -> String {
        match rng.below(4) {
            0 => format!("\"pkg-{}\"", rng.below(5)),
            1 => format!("[\"tuple-{}\", {{\"on\": true}}]", rng.below(5)),
            2 => format!("{{\"package\": \"obj-{}\"}}", rng.below(5)),
            _ => format!("\"./path/{}.js\"", rng.below(5)),
        }
    }

    /// `items` joined with commas and scattered white space, sometimes with a trailing comma.
    fn joined(rng: &mut Rng, items: &[String]) -> String {
        let mut out = String::new();
        for (index, item) in items.iter().enumerate() {
            out.push_str(trivia(rng));
            out.push_str(item);
            if index + 1 < items.len() {
                out.push_str(trivia(rng));
                out.push(',');
            } else {
                out.push_str(closing_trivia(rng));
                if rng.below(3) == 0 {
                    out.push(',');
                }
            }
        }
        out.push_str(closing_trivia(rng));
        out
    }

    /// A config: some properties in some order, the list among them most of the time.
    fn document(rng: &mut Rng) -> String {
        let mut members = vec![
            "\"theme\": \"system\"".to_string(),
            "\"mouse\": true".to_string(),
            "\"nested\": {\"plugin\": [\"deep\"]}".to_string(),
            "\"limit\": 1.5e3".to_string(),
        ];
        members.truncate(rng.below(5));
        if rng.below(5) > 0 {
            let entries: Vec<String> = (0..rng.below(4)).map(|_| entry(rng)).collect();
            let list = format!("\"plugin\": [{}]", joined(rng, &entries));
            let at = rng.below(members.len() + 1);
            members.insert(at, list);
        }
        let lead = trivia(rng);
        let body = joined(rng, &members);
        format!("{lead}{{{body}}}{}", rng.pick(&["", "\n", "\n// end\n"]))
    }

    /// The config's value with an empty `plugin` list taken out, which is how taw leaves one.
    fn meaning(text: &str) -> Value {
        let mut value = value_of(text);
        let empty = value["plugin"].as_array().is_some_and(Vec::is_empty);
        if empty {
            value.as_object_mut().unwrap().remove("plugin");
        }
        value
    }

    fn comments(text: &str) -> usize {
        text.matches("//").count() + text.matches("/*").count()
    }

    #[test]
    fn whatever_the_layout_an_added_entry_comes_out_again_and_nothing_else_moves() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..2000 {
            let doc = document(&mut rng);
            let context = format!("round {round}: {doc:?}");
            assert!(parse_root(&doc).is_ok(), "the generator broke: {context}");

            let added = add(&doc);
            assert!(
                is_registered(&added, TUI_KEY, SPEC),
                "{context} -> {added:?}"
            );
            let mut expected = value_of(&doc);
            match expected.get_mut("plugin").and_then(Value::as_array_mut) {
                Some(list) => list.push(json!(SPEC)),
                None => {
                    expected["plugin"] = json!([SPEC]);
                }
            }
            assert_eq!(value_of(&added), expected, "{context} -> {added:?}");
            assert_eq!(comments(&added), comments(&doc), "{context} -> {added:?}");
            // Adding what is there is no change.
            assert_eq!(add(&added), added, "{context}");

            let removed = remove(&added);
            assert!(
                !is_registered(&removed, TUI_KEY, SPEC),
                "{context} -> {removed:?}"
            );
            assert_eq!(meaning(&removed), meaning(&doc), "{context} -> {removed:?}");
            // The comments all stay, except that a list a removal leaves empty goes with the
            // comments written on its own line, as they are the list's, in herdr's edit too.
            let emptied = value_of(&doc)
                .get("plugin")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty);
            if emptied {
                assert!(
                    comments(&removed) <= comments(&doc),
                    "{context} -> {removed:?}"
                );
            } else {
                assert_eq!(
                    comments(&removed),
                    comments(&doc),
                    "{context} -> {removed:?}"
                );
            }
            // What the list held before is what is left, so a second removal is no change.
            assert_eq!(remove(&removed), removed, "{context}");
            // And it is the same config as before, by the measure an uninstall uses to tell
            // whether the user changed anything since.
            assert_eq!(
                settled(&removed, TUI_KEY, SPEC),
                settled(&doc, TUI_KEY, SPEC),
                "{context} -> {removed:?}"
            );
        }
    }

    #[test]
    fn whatever_the_layout_an_untouched_config_is_given_back_byte_for_byte() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        // Each round writes and fsyncs files, so fewer than the text-only test above; the
        // predicate an exact revert rests on is checked for all of that test's rounds.
        for round in 0..100 {
            let doc = document(&mut rng);
            let context = format!("round {round}: {doc:?}");
            let dir = unique_dir();
            let config = dir.path().join("tui.jsonc");
            fs::write(&config, &doc).unwrap();

            let edit = plan_tui_registration(dir.path(), SPEC, sidecar)
                .unwrap()
                .unwrap();
            edit.apply().unwrap();
            assert!(is_registered(
                &fs::read_to_string(&config).unwrap(),
                TUI_KEY,
                SPEC
            ));
            remove_tui_registration(dir.path(), SPEC, sidecar).unwrap();

            assert_eq!(fs::read_to_string(&config).unwrap(), doc, "{context}");
            assert!(!sidecar(&config).exists(), "{context}");
        }
    }

    // -----------------------------------------------------------------------------------
    // the files
    // -----------------------------------------------------------------------------------

    fn unique_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn sidecar(config: &Path) -> PathBuf {
        let mut name = config.as_os_str().to_owned();
        name.push(".orig");
        PathBuf::from(name)
    }

    fn parse_file(path: &Path) -> Value {
        value_of(&fs::read_to_string(path).unwrap())
    }

    fn register_tui(dir: &Path) -> Option<PathBuf> {
        plan_tui_registration(dir, SPEC, sidecar)
            .unwrap()
            .map(|edit| {
                let path = edit.config_path.clone();
                edit.apply().unwrap();
                path
            })
    }

    #[test]
    fn a_tui_registration_keeps_the_comments_of_the_config_and_comes_out_again() {
        let dir = unique_dir();
        let config = dir.path().join("tui.jsonc");
        let original = concat!(
            "{\n",
            "  // Keep this comment.\n",
            "  \"theme\": \"system\",\n",
            "  \"plugin\": [\"example\", [\"configured\", {\"enabled\": true}]],\n",
            "}\n",
        );
        fs::write(&config, original).unwrap();

        assert_eq!(register_tui(dir.path()), Some(config.clone()));
        // Once registered, there is nothing more to plan.
        assert!(
            plan_tui_registration(dir.path(), SPEC, sidecar)
                .unwrap()
                .is_none()
        );

        let installed = fs::read_to_string(&config).unwrap();
        assert!(installed.contains("// Keep this comment."));
        assert_eq!(
            parse_file(&config)["plugin"],
            json!(["example", ["configured", {"enabled": true}], SPEC])
        );
        assert_eq!(fs::read_to_string(sidecar(&config)).unwrap(), original);

        // Nothing else touched it, so it is given back as it was found.
        assert_eq!(
            remove_tui_registration(dir.path(), SPEC, sidecar).unwrap(),
            vec![config.clone()]
        );
        assert_eq!(fs::read_to_string(&config).unwrap(), original);
        assert!(!sidecar(&config).exists());
    }

    #[test]
    fn what_was_edited_since_the_install_stays_edited_when_it_is_taken_out() {
        let dir = unique_dir();
        let config = dir.path().join("tui.jsonc");
        fs::write(&config, "{\n  \"theme\": \"system\"\n}\n").unwrap();
        register_tui(dir.path());
        assert!(fs::read_to_string(&config).unwrap().contains(SPEC));
        let edited = fs::read_to_string(&config)
            .unwrap()
            .replace("\"system\"", "\"tokyonight\"")
            .replace("{\n", "{\n  // mine\n");
        fs::write(&config, &edited).unwrap();

        remove_tui_registration(dir.path(), SPEC, sidecar).unwrap();

        let left = fs::read_to_string(&config).unwrap();
        assert!(
            left.contains("// mine") && left.contains("tokyonight"),
            "{left}"
        );
        assert!(!left.contains("taw-tui-session"), "{left}");
    }

    #[test]
    fn a_copy_that_already_held_the_entry_is_never_restored() {
        // The registration was in the file before taw ever ran (carried over from another
        // machine): the copy taw took holds it, and giving that back would leave an entry for
        // a plugin file that uninstall is about to delete.
        let dir = unique_dir();
        let config = dir.path().join("tui.jsonc");
        fs::write(&config, format!("{{\"plugin\": [\"{SPEC}\", \"other\"]}}")).unwrap();
        // Nothing to plan, so no copy is taken; put one in place the way an old install would.
        fs::write(sidecar(&config), fs::read(&config).unwrap()).unwrap();

        remove_tui_registration(dir.path(), SPEC, sidecar).unwrap();

        assert_eq!(parse_file(&config)["plugin"], json!(["other"]));
    }

    #[test]
    fn a_fresh_install_makes_tui_jsonc_and_leaves_tui_json_for_opencode_to_migrate_into() {
        let dir = unique_dir();
        let legacy = dir.path().join("opencode.json");
        fs::write(&legacy, "{\n  \"theme\": \"system\"\n}\n").unwrap();

        let path = register_tui(dir.path()).unwrap();

        assert_eq!(path, dir.path().join("tui.jsonc"));
        assert!(!dir.path().join("tui.json").exists());
        assert_eq!(
            fs::read_to_string(&legacy).unwrap(),
            "{\n  \"theme\": \"system\"\n}\n"
        );
        assert_eq!(parse_file(&path), json!({"plugin": [SPEC]}));

        // The file did not exist before, so taking the entry out takes the file with it.
        remove_tui_registration(dir.path(), SPEC, sidecar).unwrap();
        assert!(!path.exists());
        assert!(!sidecar(&path).exists());
    }

    #[test]
    fn a_registration_already_in_tui_json_is_the_one_that_counts() {
        let dir = unique_dir();
        let config = dir.path().join("tui.json");
        fs::write(&config, format!("{{\"plugin\":[\"{SPEC}\"]}}")).unwrap();

        assert!(
            plan_tui_registration(dir.path(), SPEC, sidecar)
                .unwrap()
                .is_none()
        );
        assert!(!dir.path().join("tui.jsonc").exists());

        assert_eq!(
            remove_tui_registration(dir.path(), SPEC, sidecar).unwrap(),
            vec![config.clone()]
        );
        assert_eq!(parse_file(&config), json!({}));
    }

    #[test]
    fn a_registration_given_as_a_tuple_with_options_is_found_and_taken_out() {
        let dir = unique_dir();
        let config = dir.path().join("tui.jsonc");
        fs::write(
            &config,
            format!("{{\"plugin\":[[\"{SPEC}\",{{\"enabled\":true}}]]}}"),
        )
        .unwrap();

        assert!(
            plan_tui_registration(dir.path(), SPEC, sidecar)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            remove_tui_registration(dir.path(), SPEC, sidecar).unwrap(),
            vec![config.clone()]
        );
    }

    #[test]
    fn taking_the_entry_out_of_a_config_that_holds_none_reports_no_file() {
        let dir = unique_dir();
        fs::write(dir.path().join("tui.jsonc"), "{\"plugin\": [\"a\"]}").unwrap();
        assert!(
            remove_tui_registration(dir.path(), SPEC, sidecar)
                .unwrap()
                .is_empty()
        );
        assert!(!remove_cli_registration(dir.path(), SPEC, sidecar).unwrap());
        assert_eq!(
            fs::read_to_string(dir.path().join("tui.jsonc")).unwrap(),
            "{\"plugin\": [\"a\"]}"
        );
    }

    #[test]
    fn a_cli_registration_keeps_options_and_the_other_preferences() {
        let dir = unique_dir();
        let state = unique_dir();
        let path = dir.path().join("cli.json");
        let original = r#"{"theme":{"name":"catppuccin"},"plugins":[{"package":"./taw-opencode","options":{"custom":true}},"example"]}"#;
        fs::write(&path, original).unwrap();

        // Already there, in the object form: nothing to plan, nothing rewritten.
        assert!(
            plan_cli_registration(dir.path(), state.path(), "./taw-opencode", sidecar)
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);

        assert!(remove_cli_registration(dir.path(), "./taw-opencode", sidecar).unwrap());
        assert_eq!(parse_file(&path)["plugins"], json!(["example"]));
        assert_eq!(parse_file(&path)["theme"]["name"], "catppuccin");

        for _ in 0..2 {
            if let Some(edit) =
                plan_cli_registration(dir.path(), state.path(), "./taw-opencode", sidecar).unwrap()
            {
                edit.apply().unwrap();
            }
        }
        assert_eq!(
            parse_file(&path)["plugins"],
            json!(["example", "./taw-opencode"])
        );
    }

    #[test]
    fn a_cli_config_opencode_would_never_make_is_made_when_there_is_nothing_to_move_into_it() {
        let dir = unique_dir();
        let state = unique_dir();

        let edit = plan_cli_registration(dir.path(), state.path(), "./taw-opencode", sidecar)
            .unwrap()
            .expect("cli.json should be created when OpenCode has nothing to migrate");
        edit.apply().unwrap();

        let path = dir.path().join("cli.json");
        assert_eq!(parse_file(&path), json!({"plugins": ["./taw-opencode"]}));
        assert!(remove_cli_registration(dir.path(), "./taw-opencode", sidecar).unwrap());
        assert!(!path.exists(), "the file it made goes with the entry");
    }

    #[test]
    fn a_cli_config_waits_while_opencode_still_has_settings_to_move_into_one() {
        let dir = unique_dir();
        let state = unique_dir();
        fs::write(dir.path().join("tui.json"), "{}").unwrap();
        assert!(
            plan_cli_registration(dir.path(), state.path(), "./taw-opencode", sidecar)
                .unwrap()
                .is_none()
        );
        assert!(!dir.path().join("cli.json").exists());

        fs::remove_file(dir.path().join("tui.json")).unwrap();
        fs::write(state.path().join("kv.json"), "{}").unwrap();
        assert!(
            plan_cli_registration(dir.path(), state.path(), "./taw-opencode", sidecar)
                .unwrap()
                .is_none()
        );
        assert!(!dir.path().join("cli.json").exists());

        // A cli.json that is already there is not waited for: OpenCode migrates only
        // while it is absent, and it is not.
        fs::write(dir.path().join("cli.json"), "{}").unwrap();
        assert!(
            plan_cli_registration(dir.path(), state.path(), "./taw-opencode", sidecar)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_deferred_cli_registration_can_be_told_from_one_already_made() {
        let dir = unique_dir();
        let state = unique_dir();
        assert!(!cli_registration_deferred(dir.path(), state.path()));

        fs::write(dir.path().join("tui.json"), "{}").unwrap();
        assert!(cli_registration_deferred(dir.path(), state.path()));
        fs::remove_file(dir.path().join("tui.json")).unwrap();

        fs::write(state.path().join("kv.json"), "{}").unwrap();
        assert!(cli_registration_deferred(dir.path(), state.path()));

        // Once there is a cli.json, nothing is being waited for.
        fs::write(dir.path().join("cli.json"), "{}").unwrap();
        assert!(!cli_registration_deferred(dir.path(), state.path()));
    }

    #[test]
    fn a_plugin_list_that_is_not_an_array_stops_the_install_before_anything_is_planned() {
        for (name, body) in [
            ("cli.json", r#"{"plugins":{}}"#),
            ("tui.jsonc", r#"{"plugin":"x"}"#),
            ("tui.json", r#"{"plugin":3}"#),
            ("tui.json", "not json"),
        ] {
            let dir = unique_dir();
            fs::write(dir.path().join(name), body).unwrap();
            let err = validate_configs(dir.path()).unwrap_err();
            let refusal = err.downcast_ref::<IntegrationError>().unwrap_or_else(|| {
                panic!("{name}: an untyped error: {err:#}");
            });
            assert!(
                matches!(refusal, IntegrationError::UnusableConfig { .. }),
                "{name}: {err:#}"
            );
            assert!(err.to_string().contains(name), "{err}");
        }
        let dir = unique_dir();
        fs::write(
            dir.path().join("tui.jsonc"),
            "{\n  // fine\n  \"plugin\": [],\n}\n",
        )
        .unwrap();
        validate_configs(dir.path()).unwrap();
    }

    #[test]
    fn planning_over_a_config_it_cannot_read_is_refused_with_its_name_and_takes_no_copy() {
        for (name, body) in [
            ("tui.jsonc", "{\"plugin\": "),
            ("tui.jsonc", "{\"plugin\": \"./x.js\"}"),
            // The tree does not read a byte order mark, as in herdr: refused, not rewritten.
            ("tui.jsonc", "\u{feff}{}"),
        ] {
            let dir = unique_dir();
            let path = dir.path().join(name);
            fs::write(&path, body).unwrap();

            let err = plan_tui_registration(dir.path(), SPEC, sidecar).unwrap_err();

            assert!(
                matches!(
                    err.downcast_ref::<IntegrationError>(),
                    Some(IntegrationError::UnusableConfig { .. })
                ),
                "{body}: {err:#}"
            );
            assert!(err.to_string().contains(name), "{err}");
            assert!(!sidecar(&path).exists());
            assert_eq!(fs::read_to_string(&path).unwrap(), body);
        }
    }

    #[test]
    fn a_reinstall_plans_nothing_and_so_leaves_the_first_copy_alone() {
        let dir = unique_dir();
        let config = dir.path().join("tui.jsonc");
        fs::write(&config, "{\"theme\": \"system\"}").unwrap();
        register_tui(dir.path());
        let copy = fs::read(sidecar(&config)).unwrap();
        assert_eq!(copy, b"{\"theme\": \"system\"}");

        assert!(
            plan_tui_registration(dir.path(), SPEC, sidecar)
                .unwrap()
                .is_none()
        );

        assert_eq!(fs::read(sidecar(&config)).unwrap(), copy);
    }

    #[test]
    fn a_config_that_cannot_be_written_stops_the_install_before_anything_is_planned() {
        let dir = unique_dir();
        // A directory where a config file should be.
        fs::create_dir(dir.path().join("cli.json")).unwrap();
        assert!(check_targets(dir.path()).is_err());
    }

    #[test]
    fn without_a_copy_the_last_entry_leaves_an_empty_managed_config() {
        // As in herdr: a config that holds the entry and that taw kept no copy of (an earlier
        // install, or a copy that was deleted) is left as an empty object, not removed.
        let dir = unique_dir();
        let config = dir.path().join("tui.jsonc");
        fs::write(&config, format!("{{\"plugin\": [\"{SPEC}\"]}}")).unwrap();

        assert_eq!(
            remove_tui_registration(dir.path(), SPEC, sidecar).unwrap(),
            vec![config.clone()]
        );

        assert!(config.is_file());
        assert_eq!(parse_file(&config), json!({}));
    }

    #[test]
    fn a_config_the_tree_lays_out_anew_is_still_given_back_as_found() {
        for original in RELAID_LAYOUTS {
            let dir = unique_dir();
            let config = dir.path().join("tui.jsonc");
            fs::write(&config, original).unwrap();

            register_tui(dir.path()).unwrap();
            assert_ne!(
                fs::read_to_string(sidecar(&config)).unwrap(),
                "",
                "{original:?}: the copy holds the config as it was"
            );
            remove_tui_registration(dir.path(), SPEC, sidecar).unwrap();

            assert_eq!(fs::read_to_string(&config).unwrap(), original);
            assert!(!sidecar(&config).exists());
        }
    }

    #[test]
    fn a_blank_config_is_the_empty_object_it_starts_from_and_is_given_back_blank() {
        for blank in ["", "\n", "  \n\t"] {
            let dir = unique_dir();
            let config = dir.path().join("tui.jsonc");
            fs::write(&config, blank).unwrap();
            validate_configs(dir.path()).unwrap();

            register_tui(dir.path()).unwrap();
            assert_eq!(parse_file(&config), json!({"plugin": [SPEC]}));
            remove_tui_registration(dir.path(), SPEC, sidecar).unwrap();

            // A blank copy is a copy of nothing: the file goes, as one install made would.
            if blank.is_empty() {
                assert!(!config.exists());
            } else {
                assert_eq!(fs::read_to_string(&config).unwrap(), blank);
            }
        }
    }

    /// Follows herdr's `failed_cli_registration_preserves_existing_config`: a write that
    /// fails partway (here, by the file-size limit) leaves the config as it was, and no
    /// staging file behind. The limit is set on a child of this test, after the config exists.
    #[cfg(unix)]
    #[test]
    fn a_failed_registration_preserves_the_existing_config() {
        const CHILD_CONFIG: &str = "TAW_TEST_OPENCODE_CONFIG_DIR";
        if let Some(dir) = std::env::var_os(CHILD_CONFIG) {
            let dir = PathBuf::from(dir);
            let edit = plan_cli_registration(&dir, &dir.join("state"), "./taw-opencode", sidecar)
                .unwrap()
                .expect("a registration to make");
            let err = edit.apply().unwrap_err();
            let too_large = err
                .chain()
                .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                .any(|io| io.kind() == std::io::ErrorKind::FileTooLarge);
            assert!(too_large, "not the file-size limit: {err:#}");
            println!("registration reached the file-size limit");
            return;
        }

        let dir = unique_dir();
        let path = dir.path().join("cli.json");
        let original = r#"{"theme":{"name":"catppuccin"},"plugins":["example"]}"#;
        fs::write(&path, original).unwrap();
        // Ignoring SIGXFSZ makes the kernel answer EFBIG instead of killing the child.
        let output = std::process::Command::new("bash")
            .args(["-c", "trap '' XFSZ; ulimit -f 0; exec \"$@\"", "taw-test"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "integration::opencode_config::tests::a_failed_registration_preserves_the_existing_config",
                "--nocapture",
            ])
            .env(CHILD_CONFIG, dir.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "child failed: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("registration reached the file-size limit")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no staging file, and no copy of a config that was not replaced"
        );
    }
}
