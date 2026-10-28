//! Parses tmux's `#{window_layout}` string into herdr's nested split-tree shape, for
//! `taw pane layout` (and `layout <tab>`).
//!
//! # tmux's layout grammar
//!
//! tmux writes a window's layout in one of two formats, and [`parse`] reads both into the
//! same [`Cell`] tree. A string that starts with `{` is the JSON format of tmux 3.8 and
//! later; anything else is the older format, which tmux before 3.8 writes and 3.8 still
//! accepts.
//!
//! ## The older format (tmux 3.2 to 3.7)
//!
//! ```text
//! layout := <checksum> "," cell
//! cell   := <W> "x" <H> "," <x> "," <y> ( "," <pane-id> | "{" cell ("," cell)+ "}" | "[" cell ("," cell)+ "]" )
//! ```
//!
//! A leaf ends in the pane's id number (the `<n>` of `%<n>`; see [`Kind::Leaf`]); a
//! container replaces that number with a group. `{…}` holds cells side by side (tmux's
//! `left-right` layout, what `pane split --direction right` produces); `[…]` stacks them
//! (`top-bottom`, from `--direction down`).
//!
//! ## The JSON format (tmux 3.8)
//!
//! ```text
//! layout := {"V":2,"L":cell}
//! cell   := {"t":"p","w":<W>,"h":<H>,"x":<x>,"y":<y>, <state>, "i":<pane-index>, [ "z":<z>, ] "I":"%<pane-id>"}
//!         | {"t":"h" or "v","w":<W>,"h":<H>,"x":<x>,"y":<y>,"c":[cell (,cell)*]}
//! state  := "a":true (the active pane) | "l":<n> (its place in the last-panes list) | nothing
//! ```
//!
//! `t` is the cell type: `p` a pane, `h` cells side by side (the older `{…}`), `v` cells
//! stacked (the older `[…]`). Note that the key `h` is the height, while the value `h` of
//! `t` is a type. The pane id in `I` is what the older format writes at the end of a
//! leaf; `i` (the pane's index in the window) is not used. Unknown keys are ignored.
//!
//! Floating panes (a pane cell with a `z` key) are not part of the split tree. tmux's own
//! older-format output leaves them out, and so does [`parse`]: a floating cell is
//! dropped, a split left with no cells is dropped, a split left with one cell is replaced
//! by that cell, and the root gets x and y 0 (once the floating cells are dropped the root
//! is always tiled, as tmux's own output has it). A window with only floating panes has
//! no tree, so it is an error.
//!
//! # herdr's shape
//!
//! Taken verbatim from `third_party/herdr/reference/socket-api.mdx` (`layout.export`):
//! "`root` is a BSP tree of `pane` and `split` nodes. Pane nodes can include `pane_id`,
//! `label`, `cwd`, and argv `command`. Split nodes use `direction` (`right` or `down`),
//! `ratio`, `first`, and `second`."
//!
//! ```json
//! {"type":"split","direction":"right","ratio":0.5,
//!  "first":{"type":"pane","pane_id":"w1:p1","terminal_id":"%1","width":40,"height":24,"x":0,"y":0},
//!  "second":{"type":"pane","pane_id":"w1:p2","terminal_id":"%2","width":39,"height":24,"x":41,"y":0}}
//! ```
//!
//! So `direction` reuses the `right`/`down` vocabulary of `pane split --direction`:
//! `right` for children side by side (tmux `{}`), `down` for stacked (`[]`). `ratio` is
//! the first child's share of the span the split covers, matching the `--ratio` a
//! `pane split` would have taken.
//!
//! herdr's tree is **binary**, but a tmux layout group can hold three or more cells (a
//! window split twice in the same direction). Such a group is emitted as right-nested
//! splits of the same direction — the BSP form of the same geometry — so `first`/`second`
//! always hold exactly one node each.
//!
//! Pane nodes carry the tmux geometry (`width`/`height`/`x`/`y`) and `terminal_id`
//! alongside `pane_id`; herdr's "can include" leaves room for extra fields, and these are
//! what make the tree useful for deciding where to split next.

use serde_json::{Value, json};

use crate::cli::envelope::CliError;

/// One parsed layout cell: a pane, or a split of two or more cells.
#[derive(Debug, PartialEq)]
pub struct Cell {
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    pub kind: Kind,
}

#[derive(Debug, PartialEq)]
pub enum Kind {
    /// A pane, identified by the **pane-id number** tmux writes into a layout leaf — the
    /// `<n>` of `%<n>`, which is persistent and never reused. This is *not* the pane's
    /// positional `#{pane_index}`: the two agree until a pane is killed and then diverge
    /// (kill `%0` and the panes are indexes 0,1 but ids `%1`,`%2`, so the layout leaves
    /// read `…,1 …,2`). Resolve it against `#{pane_id}`, never `#{pane_index}`.
    Leaf { pane_number: u32 },
    /// `{…}` — children side by side.
    Horizontal(Vec<Cell>),
    /// `[…]` — children stacked.
    Vertical(Vec<Cell>),
}

/// Parses a `#{window_layout}` (or `#{window_visible_layout}`) string, in the older
/// `<checksum>,<cell>` format or tmux 3.8's JSON format; see the module docs.
pub fn parse(layout: &str) -> anyhow::Result<Cell> {
    if layout.starts_with('{') {
        parse_json(layout)
    } else {
        parse_old(layout)
    }
}

fn parse_old(layout: &str) -> anyhow::Result<Cell> {
    let (_checksum, rest) = layout.split_once(',').ok_or_else(|| {
        CliError::invalid_argument(format!("malformed window layout '{layout}': no checksum"))
    })?;
    let mut p = Parser {
        s: rest,
        i: 0,
        original: layout,
    };
    let cell = p.cell()?;
    if p.i != p.s.len() {
        return Err(CliError::invalid_argument(format!(
            "malformed window layout '{layout}': trailing input at byte {}",
            p.i
        )));
    }
    Ok(cell)
}

/// The error for a JSON layout: names the layout and what was wrong.
fn json_err<T>(layout: &str, what: impl std::fmt::Display) -> anyhow::Result<T> {
    Err(CliError::invalid_argument(format!(
        "malformed window layout '{layout}': {what}"
    )))
}

/// `{"V":2,"L":<cell>}`, tmux 3.8's format. Floating panes are left out of the tree the
/// way tmux's own older-format output leaves them out (see the module docs).
fn parse_json(layout: &str) -> anyhow::Result<Cell> {
    let doc: Value = match serde_json::from_str(layout) {
        Ok(doc) => doc,
        Err(e) => return json_err(layout, format!("not valid JSON ({e})")),
    };
    let Some(top) = doc.as_object() else {
        return json_err(layout, "expected a JSON object");
    };
    match top.get("V") {
        Some(Value::Number(n)) if n.as_u64() == Some(2) => {}
        Some(v) => return json_err(layout, format!("unsupported layout version {v}")),
        None => return json_err(layout, "no layout version \"V\""),
    }
    let Some(root) = top.get("L") else {
        return json_err(layout, "no cell \"L\"");
    };
    match json_cell(layout, root)? {
        Some(mut cell) => {
            cell.x = 0;
            cell.y = 0;
            Ok(cell)
        }
        None => Err(CliError::invalid_argument(format!(
            "window layout '{layout}' holds only floating panes, so it has no split tree"
        ))),
    }
}

/// A coordinate or size key of a JSON cell, as a signed number (tmux writes `x` and `y`
/// with `%d`).
fn json_int(layout: &str, cell: &serde_json::Map<String, Value>, key: &str) -> anyhow::Result<i64> {
    match cell.get(key) {
        Some(Value::Number(n)) => match (n.as_i64(), n.is_u64()) {
            (Some(v), _) => Ok(v),
            (None, true) => json_err(layout, format!("\"{key}\" does not fit in 32 bits")),
            (None, false) => json_err(layout, format!("\"{key}\" is not an integer")),
        },
        Some(_) => json_err(layout, format!("\"{key}\" is not an integer")),
        None => json_err(layout, format!("no \"{key}\" in a cell")),
    }
}

/// A key that must be a non-negative integer that fits in `u32`.
fn json_u32(layout: &str, cell: &serde_json::Map<String, Value>, key: &str) -> anyhow::Result<u32> {
    let v = json_int(layout, cell, key)?;
    match u32::try_from(v) {
        Ok(v) => Ok(v),
        Err(_) => json_err(
            layout,
            format!("\"{key}\" is {v}, expected a number from 0 to {}", u32::MAX),
        ),
    }
}

/// A pane cell's `"I":"%<digits>"`, as the number.
fn json_pane_number(layout: &str, cell: &serde_json::Map<String, Value>) -> anyhow::Result<u32> {
    let Some(Value::String(id)) = cell.get("I") else {
        return json_err(layout, "a pane cell has no pane id \"I\" string");
    };
    let digits = id.strip_prefix('%').unwrap_or("");
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return json_err(layout, format!("pane id {id:?} is not '%' and digits"));
    }
    match digits.parse() {
        Ok(n) => Ok(n),
        Err(_) => json_err(layout, format!("pane id {id:?} does not fit in 32 bits")),
    }
}

/// One JSON cell as a [`Cell`], or `None` when it is a floating pane or a split with only
/// floating panes under it.
fn json_cell(layout: &str, value: &Value) -> anyhow::Result<Option<Cell>> {
    let Some(cell) = value.as_object() else {
        return json_err(layout, "a cell is not a JSON object");
    };
    let Some(Value::String(kind)) = cell.get("t") else {
        return json_err(layout, "a cell has no type \"t\" string");
    };
    if !matches!(kind.as_str(), "p" | "h" | "v") {
        return json_err(layout, format!("unknown cell type {kind:?}"));
    }

    if kind == "p" {
        // A floating pane carries a z-index. It has no place in the split tree, but it is
        // still checked, so a damaged string does not pass because of where it is damaged.
        if let Some(z) = cell.get("z") {
            if !(z.is_i64() || z.is_u64()) {
                return json_err(layout, "\"z\" is not an integer");
            }
            for key in ["w", "h", "x", "y"] {
                json_int(layout, cell, key)?;
            }
            json_pane_number(layout, cell)?;
            return Ok(None);
        }
    }

    if kind == "p" {
        return Ok(Some(Cell {
            width: json_u32(layout, cell, "w")?,
            height: json_u32(layout, cell, "h")?,
            x: json_u32(layout, cell, "x")?,
            y: json_u32(layout, cell, "y")?,
            kind: Kind::Leaf {
                pane_number: json_pane_number(layout, cell)?,
            },
        }));
    }

    let Some(Value::Array(children)) = cell.get("c") else {
        return json_err(layout, "a split has no \"c\" array of cells");
    };
    if children.is_empty() {
        return json_err(layout, "a split has no cells");
    }
    let mut kept = Vec::new();
    for child in children {
        if let Some(c) = json_cell(layout, child)? {
            kept.push(c);
        }
    }
    // A split with nothing tiled under it is dropped, and its offsets may be negative: tmux gives
    // it those of a floating cell. They are checked all the same, as signed numbers like a floating
    // cell's, so a damaged string does not pass because of where it is damaged.
    if kept.is_empty() {
        for key in ["w", "h", "x", "y"] {
            json_int(layout, cell, key)?;
        }
        return Ok(None);
    }
    let width = json_u32(layout, cell, "w")?;
    let height = json_u32(layout, cell, "h")?;
    let x = json_u32(layout, cell, "x")?;
    let y = json_u32(layout, cell, "y")?;
    Ok(match kept.len() {
        1 => kept.pop(),
        _ if kind == "h" => Some(Cell {
            width,
            height,
            x,
            y,
            kind: Kind::Horizontal(kept),
        }),
        _ => Some(Cell {
            width,
            height,
            x,
            y,
            kind: Kind::Vertical(kept),
        }),
    })
}

struct Parser<'a> {
    s: &'a str,
    i: usize,
    original: &'a str,
}

impl<'a> Parser<'a> {
    fn err<T>(&self, what: &str) -> anyhow::Result<T> {
        Err(CliError::invalid_argument(format!(
            "malformed window layout '{}': expected {what} at byte {}",
            self.original, self.i
        )))
    }

    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.i).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    /// Reads a run of ASCII digits as a `u32`.
    fn number(&mut self) -> anyhow::Result<u32> {
        let start = self.i;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.i += 1;
        }
        if start == self.i {
            return self.err("a number");
        }
        self.s[start..self.i]
            .parse()
            .or_else(|_| self.err("a number that fits in 32 bits"))
    }

    /// `<W>x<H>,<x>,<y>` followed by a pane index or a brace group.
    fn cell(&mut self) -> anyhow::Result<Cell> {
        let width = self.number()?;
        if !self.eat(b'x') {
            return self.err("'x' between width and height");
        }
        let height = self.number()?;
        if !self.eat(b',') {
            return self.err("',' before x");
        }
        let x = self.number()?;
        if !self.eat(b',') {
            return self.err("',' before y");
        }
        let y = self.number()?;

        let kind = if self.eat(b',') {
            Kind::Leaf {
                pane_number: self.number()?,
            }
        } else if self.eat(b'{') {
            let children = self.children(b'}')?;
            Kind::Horizontal(children)
        } else if self.eat(b'[') {
            let children = self.children(b']')?;
            Kind::Vertical(children)
        } else {
            return self.err("',<pane-index>', '{' or '['");
        };

        Ok(Cell {
            width,
            height,
            x,
            y,
            kind,
        })
    }

    /// One or more comma-separated cells, up to `close`.
    fn children(&mut self, close: u8) -> anyhow::Result<Vec<Cell>> {
        let mut children = vec![self.cell()?];
        while self.eat(b',') {
            children.push(self.cell()?);
        }
        if !self.eat(close) {
            return self.err(&format!("'{}' closing the group", close as char));
        }
        Ok(children)
    }
}

/// Resolves a layout leaf's pane-id NUMBER (`%<n>`) to that pane's `(public id, tmux id,
/// live pane_index)`, or `None` when no live pane carries that id.
pub type PaneIds<'a> = &'a dyn Fn(u32) -> Option<(String, String, u32)>;

/// Renders a parsed [`Cell`] as herdr's JSON tree — `pane` and `split` nodes, `first` and
/// `second`, per the module docs.
pub fn to_json(cell: &Cell, pane_id_of: PaneIds<'_>) -> Value {
    match &cell.kind {
        Kind::Leaf { pane_number } => {
            // Resolve the leaf's pane-id number against the live panes; a match carries the
            // pane's *real* `#{pane_index}`, so the reported index is the pane's own, not
            // the id number that identifies it in the layout string.
            let (public, terminal, index) = match pane_id_of(*pane_number) {
                Some((p, t, i)) => (Value::String(p), Value::String(t), json!(i)),
                None => (Value::Null, Value::Null, Value::Null),
            };
            json!({
                "type": "pane",
                "pane_id": public,
                "terminal_id": terminal,
                "pane_index": index,
                "width": cell.width,
                "height": cell.height,
                "x": cell.x,
                "y": cell.y,
            })
        }
        Kind::Horizontal(children) => split_json(children, true, pane_id_of),
        Kind::Vertical(children) => split_json(children, false, pane_id_of),
    }
}

/// One split located by its path from the root — what it takes to move its divider.
#[derive(Debug, PartialEq)]
pub struct Split {
    /// `true` for a left-right split (tmux `{…}`), `false` for top-bottom (`[…]`).
    pub horizontal: bool,
    /// How many cells the split covers along its own axis, dividers included. A nested
    /// split covers only its own branch, not the whole window.
    pub span: u32,
    /// The pane-id number (`<n>` of `%<n>`) of the first pane in the split's `first`
    /// branch — the pane whose size along the axis *is* the split's ratio, and so the one
    /// `resize-pane` addresses.
    pub first_pane_number: u32,
}

/// The split at `path` (`false` = `first`, `true` = `second`), or `None` when the path
/// reaches a pane, runs off the tree, or names a tree with no split at all.
///
/// Paths follow the same right-nested binary form [`to_json`] and [`flatten`] render, so
/// a split's reported `id` addresses the same node here.
pub fn at_path(cell: &Cell, path: &[bool]) -> Option<Split> {
    let (children, horizontal) = match &cell.kind {
        Kind::Leaf { .. } => return None,
        Kind::Horizontal(children) => (children.as_slice(), true),
        Kind::Vertical(children) => (children.as_slice(), false),
    };
    group_at_path(children, horizontal, path)
}

fn group_at_path(children: &[Cell], horizontal: bool, path: &[bool]) -> Option<Split> {
    let (first, rest) = children.split_first()?;
    if rest.is_empty() {
        // A group of one is not a split: descend into the child instead.
        return at_path(first, path);
    }
    match path.split_first() {
        None => Some(Split {
            horizontal,
            span: span(children, horizontal),
            first_pane_number: first_pane_number(first),
        }),
        Some((false, tail)) => at_path(first, tail),
        Some((true, tail)) => {
            if rest.len() == 1 {
                at_path(&rest[0], tail)
            } else {
                group_at_path(rest, horizontal, tail)
            }
        }
    }
}

/// The pane-id number of the first pane reachable from `cell`, in `first`-branch order.
fn first_pane_number(cell: &Cell) -> u32 {
    match &cell.kind {
        Kind::Leaf { pane_number } => *pane_number,
        Kind::Horizontal(children) | Kind::Vertical(children) => {
            children.first().map(first_pane_number).unwrap_or_default()
        }
    }
}

/// A path as the `id` [`flatten`] reports for that split.
pub fn render_path(path: &[bool]) -> String {
    if path.is_empty() {
        return "root".to_string();
    }
    let steps: Vec<&str> = path
        .iter()
        .map(|&second| if second { "second" } else { "first" })
        .collect();
    format!("root.{}", steps.join("."))
}

/// herdr's flat `PaneLayoutSnapshot` view of a layout: every pane with its rect and
/// whether it is focused, and every binary split with its rect, direction and ratio.
///
/// This is the same tree [`to_json`] renders, listed rather than nested — the shape
/// herdr's `layout.updated` event carries (`herdr-src/src/api/schema/events.rs`,
/// `EventData::LayoutUpdated`). A split's `id` is the path taken to reach it from the
/// root, in the `first`/`second` vocabulary `taw layout set-split-ratio --path` takes, so
/// a consumer can turn any split in this list straight into a ratio change.
#[derive(Debug, Default, PartialEq)]
pub struct Flat {
    pub panes: Vec<Value>,
    pub splits: Vec<Value>,
}

/// Flattens a parsed layout tree. `focused_pane_id` is the public id of the window's
/// active pane, which marks exactly one entry of `panes` as focused.
pub fn flatten(cell: &Cell, pane_id_of: PaneIds<'_>, focused_pane_id: &str) -> Flat {
    let mut flat = Flat::default();
    let mut path: Vec<&'static str> = Vec::new();
    walk(cell, pane_id_of, focused_pane_id, &mut path, &mut flat);
    flat
}

fn rect(x: u32, y: u32, width: u32, height: u32) -> Value {
    json!({ "x": x, "y": y, "width": width, "height": height })
}

/// `root`, `root.first`, `root.second.first`, … — the split's path from the root.
fn split_id(path: &[&str]) -> String {
    if path.is_empty() {
        "root".to_string()
    } else {
        format!("root.{}", path.join("."))
    }
}

fn walk(
    cell: &Cell,
    pane_id_of: PaneIds<'_>,
    focused_pane_id: &str,
    path: &mut Vec<&'static str>,
    flat: &mut Flat,
) {
    match &cell.kind {
        Kind::Leaf { pane_number } => {
            // A leaf tmux listed but `list-panes` no longer knows (raced with a kill) still
            // occupies its cell: reporting a null id keeps the geometry whole, where
            // dropping the entry would describe a layout with a hole in it.
            let public = pane_id_of(*pane_number).map(|(p, _, _)| p);
            let focused = public.as_deref() == Some(focused_pane_id);
            flat.panes.push(json!({
                "pane_id": public,
                "focused": focused,
                "rect": rect(cell.x, cell.y, cell.width, cell.height),
            }));
        }
        Kind::Horizontal(children) => {
            walk_group(children, true, pane_id_of, focused_pane_id, path, flat);
        }
        Kind::Vertical(children) => {
            walk_group(children, false, pane_id_of, focused_pane_id, path, flat);
        }
    }
}

/// One tmux layout group, in the same right-nested binary form [`split_json`] renders, so
/// a split's path here names the same node the nested tree does.
fn walk_group(
    children: &[Cell],
    horizontal: bool,
    pane_id_of: PaneIds<'_>,
    focused_pane_id: &str,
    path: &mut Vec<&'static str>,
    flat: &mut Flat,
) {
    let Some((first, rest)) = children.split_first() else {
        return;
    };
    if rest.is_empty() {
        // A group of one is not a split (tmux never emits one, but the grammar permits it).
        walk(first, pane_id_of, focused_pane_id, path, flat);
        return;
    }
    let span = span(children, horizontal);
    let near = offset(first, horizontal);
    let (x, y, width, height) = if horizontal {
        (near, first.y, span, first.height)
    } else {
        (first.x, near, first.width, span)
    };
    flat.splits.push(json!({
        "id": split_id(path),
        "direction": if horizontal { "right" } else { "down" },
        "ratio": ratio(extent(first, horizontal), span),
        "rect": rect(x, y, width, height),
    }));

    path.push("first");
    walk(first, pane_id_of, focused_pane_id, path, flat);
    path.pop();

    path.push("second");
    if rest.len() == 1 {
        walk(&rest[0], pane_id_of, focused_pane_id, path, flat);
    } else {
        walk_group(rest, horizontal, pane_id_of, focused_pane_id, path, flat);
    }
    path.pop();
}

/// One tmux layout group as a right-nested chain of binary `split` nodes.
///
/// A group of two is one split. A group of three or more — a window split repeatedly in
/// the same direction — becomes `split(c0, split(c1, split(c2, …)))`, all of the same
/// direction, which is the BSP form of that geometry. A group of one (which tmux never
/// emits, but the grammar permits) is just that child.
fn split_json(children: &[Cell], horizontal: bool, pane_id_of: PaneIds<'_>) -> Value {
    let Some((first, rest)) = children.split_first() else {
        // An empty group cannot be parsed (`children` requires at least one cell), so
        // there is nothing sensible to render.
        return Value::Null;
    };
    if rest.is_empty() {
        return to_json(first, pane_id_of);
    }
    let second = if rest.len() == 1 {
        to_json(&rest[0], pane_id_of)
    } else {
        split_json(rest, horizontal, pane_id_of)
    };
    json!({
        "type": "split",
        "direction": if horizontal { "right" } else { "down" },
        "ratio": ratio(extent(first, horizontal), span(children, horizontal)),
        "first": to_json(first, pane_id_of),
        "second": second,
    })
}

/// A cell's size along the split axis.
fn extent(cell: &Cell, horizontal: bool) -> u32 {
    if horizontal { cell.width } else { cell.height }
}

/// A cell's offset along the split axis.
fn offset(cell: &Cell, horizontal: bool) -> u32 {
    if horizontal { cell.x } else { cell.y }
}

/// The span `children` cover along the split axis, from the first cell's near edge to the
/// last cell's far edge — dividers between them included.
///
/// Measured from the children rather than taken from the parent cell so that the nested
/// splits of a three-way group each get the ratio of the span *they* cover, not of the
/// whole window.
fn span(children: &[Cell], horizontal: bool) -> u32 {
    match (children.first(), children.last()) {
        (Some(first), Some(last)) => (offset(last, horizontal) + extent(last, horizontal))
            .saturating_sub(offset(first, horizontal)),
        _ => 0,
    }
}

/// The first child's share of the span, rounded to three decimals so the number stays
/// readable (a 40-of-80 split is `0.5`, not `0.5000000001`). `0.0` guards a zero-size
/// span rather than dividing by zero.
fn ratio(first: u32, total: u32) -> Value {
    if total == 0 {
        return json!(0.0);
    }
    let raw = f64::from(first) / f64::from(total);
    json!((raw * 1000.0).round() / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::envelope::code_for;

    /// Test resolver: pane-id NUMBER n (a layout leaf) is `w1:p<n+1>` / `%<n+1>`, and
    /// reports its live `#{pane_index}` as `n` (so these structural tests keep asserting
    /// the familiar numbers; the id-number-vs-index divergence is proven end-to-end by the
    /// killed-pane integration test in `tests/cli_json.rs`).
    fn ids(number: u32) -> Option<(String, String, u32)> {
        Some((
            format!("w1:p{}", number + 1),
            format!("%{}", number + 1),
            number,
        ))
    }

    #[test]
    fn parses_a_single_pane_window_as_a_pane_node() {
        let cell = parse("f1a2,80x24,0,0,0").unwrap();
        assert_eq!(
            cell,
            Cell {
                width: 80,
                height: 24,
                x: 0,
                y: 0,
                kind: Kind::Leaf { pane_number: 0 },
            }
        );
        let json = to_json(&cell, &ids);
        // herdr calls leaves `pane`, not `leaf` (socket-api.mdx, `layout.export`).
        assert_eq!(json["type"], "pane");
        assert_eq!(json["pane_id"], "w1:p1");
        assert_eq!(json["terminal_id"], "%1");
        assert_eq!(json["width"], 80);
        assert_eq!(json["height"], 24);
    }

    #[test]
    fn a_left_right_split_uses_direction_right_with_first_and_second() {
        // What tmux reports after `split-window -h` in an 80x24 window.
        let cell = parse("bd5b,80x24,0,0{40x24,0,0,0,39x24,41,0,1}").unwrap();
        let json = to_json(&cell, &ids);
        assert_eq!(json["type"], "split");
        assert_eq!(json["direction"], "right");
        assert_eq!(json["ratio"], 0.5);
        // Binary: `first`/`second`, never a `children` array.
        assert!(json.get("children").is_none(), "{json}");
        assert_eq!(json["first"]["type"], "pane");
        assert_eq!(json["first"]["pane_id"], "w1:p1");
        assert_eq!(json["first"]["width"], 40);
        assert_eq!(json["second"]["pane_id"], "w1:p2");
        assert_eq!(json["second"]["x"], 41);
    }

    #[test]
    fn a_top_bottom_split_uses_direction_down() {
        let cell = parse("abcd,80x24,0,0[80x12,0,0,0,80x11,0,13,1]").unwrap();
        let json = to_json(&cell, &ids);
        assert_eq!(json["type"], "split");
        assert_eq!(json["direction"], "down");
        assert_eq!(json["ratio"], 0.5);
        assert_eq!(json["first"]["height"], 12);
        assert_eq!(json["second"]["y"], 13);
    }

    #[test]
    fn parses_nested_splits() {
        // Left column full height; right column split top/bottom.
        let layout = "1234,80x24,0,0{40x24,0,0,0,39x24,41,0[39x12,41,0,1,39x11,41,13,2]}";
        let cell = parse(layout).unwrap();
        let json = to_json(&cell, &ids);
        assert_eq!(json["direction"], "right");
        assert_eq!(json["first"]["type"], "pane");
        assert_eq!(json["second"]["type"], "split");
        assert_eq!(json["second"]["direction"], "down");
        assert_eq!(json["second"]["first"]["pane_id"], "w1:p2");
        assert_eq!(json["second"]["second"]["pane_id"], "w1:p3");
    }

    #[test]
    fn a_three_way_group_becomes_right_nested_binary_splits() {
        // 90 wide, split twice to the right: 30 | 29 | 29 with two divider columns.
        let cell = parse("aaaa,90x24,0,0{30x24,0,0,0,29x24,31,0,1,29x24,61,0,2}").unwrap();
        let json = to_json(&cell, &ids);
        // herdr's tree is binary, so the group nests rather than growing an array.
        assert_eq!(json["type"], "split");
        assert_eq!(json["direction"], "right");
        assert_eq!(json["first"]["pane_id"], "w1:p1");
        assert_eq!(json["ratio"], 0.333, "30 of the full 90-cell span");

        let second = &json["second"];
        assert_eq!(second["type"], "split");
        assert_eq!(
            second["direction"], "right",
            "a nested group keeps its parent's axis"
        );
        assert_eq!(second["first"]["pane_id"], "w1:p2");
        assert_eq!(second["second"]["pane_id"], "w1:p3");
        // The nested split's ratio is of the span *it* covers (columns 31..=89 = 59),
        // not of the whole window: 29/59 ≈ 0.492.
        assert_eq!(second["ratio"], 0.492);
    }

    #[test]
    fn an_uneven_split_reports_its_real_ratio() {
        let cell = parse("bbbb,80x24,0,0{20x24,0,0,0,59x24,21,0,1}").unwrap();
        assert_eq!(to_json(&cell, &ids)["ratio"], 0.25);
    }

    #[test]
    fn a_pane_node_with_no_matching_live_pane_reports_null_ids() {
        let cell = parse("f1a2,80x24,0,0,7").unwrap();
        let json = to_json(&cell, &|_| None);
        assert_eq!(json["type"], "pane");
        assert_eq!(json["pane_id"], Value::Null);
        assert_eq!(json["terminal_id"], Value::Null);
        // Unresolved: the live pane_index is unknown, so it is null rather than the leaf's
        // id number (echoing that number here was the bug — it is not an index).
        assert_eq!(json["pane_index"], Value::Null);
    }

    #[test]
    fn a_single_child_group_collapses_to_that_child() {
        // tmux never emits a one-cell group, but the grammar permits it and a split of
        // one has no meaningful `first`/`second`.
        let cell = parse("cccc,80x24,0,0{80x24,0,0,3}").unwrap();
        let json = to_json(&cell, &ids);
        assert_eq!(json["type"], "pane");
        assert_eq!(json["pane_index"], 3);
    }

    #[test]
    fn rejects_malformed_layouts_with_invalid_argument() {
        for bad in [
            "",                                   // no checksum
            "abcd",                               // no cell
            "abcd,",                              // empty cell
            "abcd,80x24",                         // no x,y
            "abcd,80x24,0",                       // no y
            "abcd,80x24,0,0",                     // no index or group
            "abcd,80x24,0,0{40x24,0,0,0",         // unclosed group
            "abcd,80x24,0,0{}",                   // empty group
            "abcd,80x24,0,0,0trailing",           // trailing input
            "abcd,80by24,0,0,0",                  // no 'x' separator
            "abcd,99999999999999999999x24,0,0,0", // number overflows u32
        ] {
            let err = parse(bad).unwrap_err();
            assert_eq!(
                code_for(&err),
                "invalid_argument",
                "should have rejected {bad:?}"
            );
        }
    }

    #[test]
    fn a_zero_sized_container_reports_a_zero_ratio_rather_than_dividing_by_zero() {
        assert_eq!(ratio(0, 0), json!(0.0));
        assert_eq!(ratio(5, 0), json!(0.0));
    }
}

#[cfg(test)]
mod json_format_tests {
    use super::*;
    use crate::cli::envelope::code_for;

    // Every JSON string below was written by a private tmux 3.8 server (80x24 window,
    // panes running `sleep`). The older string beside it is what tmux 3.6 writes for the
    // same window. For floating panes (which 3.6 does not have) the older string was not
    // captured from tmux as written: it is derived from tmux 3.8's compat code (the
    // floating cells left out, one-cell splits collapsed) and checksummed with tmux's
    // algorithm.

    /// (what, JSON string, older string) for windows without floating panes.
    const PAIRS: &[(&str, &str, &str)] = &[
        (
            "a single pane",
            r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"y":0,"a":true,"i":0,"I":"%0"}}"#,
            "b25d,80x24,0,0,0",
        ),
        (
            "two panes side by side",
            r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":40,"h":24,"x":0,"y":0,"l":0,"i":0,"I":"%1"},{"t":"p","w":39,"h":24,"x":41,"y":0,"a":true,"i":1,"I":"%2"}]}}"#,
            "020a,80x24,0,0{40x24,0,0,1,39x24,41,0,2}",
        ),
        (
            "two panes stacked",
            r#"{"V":2,"L":{"t":"v","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":80,"h":12,"x":0,"y":0,"l":0,"i":0,"I":"%3"},{"t":"p","w":80,"h":11,"x":0,"y":13,"a":true,"i":1,"I":"%4"}]}}"#,
            "41a3,80x24,0,0[80x12,0,0,3,80x11,0,13,4]",
        ),
        (
            "a stack nested in a side-by-side split",
            r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":40,"h":24,"x":0,"y":0,"l":1,"i":0,"I":"%5"},{"t":"v","w":39,"h":24,"x":41,"y":0,"c":[{"t":"p","w":39,"h":12,"x":41,"y":0,"l":0,"i":1,"I":"%6"},{"t":"p","w":39,"h":11,"x":41,"y":13,"a":true,"i":2,"I":"%7"}]}]}}"#,
            "1b85,80x24,0,0{40x24,0,0,5,39x24,41,0[39x12,41,0,6,39x11,41,13,7]}",
        ),
        (
            "a group of three",
            r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":40,"h":24,"x":0,"y":0,"l":1,"i":0,"I":"%8"},{"t":"p","w":19,"h":24,"x":41,"y":0,"l":0,"i":1,"I":"%9"},{"t":"p","w":19,"h":24,"x":61,"y":0,"a":true,"i":2,"I":"%10"}]}}"#,
            "be26,80x24,0,0{40x24,0,0,8,19x24,41,0,9,19x24,61,0,10}",
        ),
        (
            "a window whose first pane was killed (id %12, index 0)",
            r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"y":0,"a":true,"i":0,"I":"%12"}}"#,
            "5961,80x24,0,0,12",
        ),
        (
            "a zoomed window's window_layout (the panes as they are unzoomed)",
            r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":40,"h":24,"x":0,"y":0,"a":true,"i":0,"I":"%13"},{"t":"p","w":39,"h":24,"x":41,"y":0,"l":0,"i":1,"I":"%14"}]}}"#,
            "fb17,80x24,0,0{40x24,0,0,13,39x24,41,0,14}",
        ),
        (
            "a zoomed window's window_visible_layout (the zoomed pane alone)",
            r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"y":0,"a":true,"i":0,"I":"%13"}}"#,
            "5962,80x24,0,0,13",
        ),
        (
            "a floating pane among two tiled ones",
            r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":40,"h":24,"x":0,"y":0,"l":0,"i":0,"I":"%15"},{"t":"p","w":39,"h":24,"x":41,"y":0,"a":true,"i":1,"I":"%16"},{"t":"p","w":18,"h":6,"x":4,"y":2,"i":2,"z":0,"I":"%17"}]}}"#,
            "fb1c,80x24,0,0{40x24,0,0,15,39x24,41,0,16}",
        ),
        (
            "a floating pane beside one tiled pane (the split collapses)",
            r#"{"V":2,"L":{"t":"v","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":80,"h":24,"x":0,"y":0,"a":true,"i":0,"I":"%18"},{"t":"p","w":18,"h":6,"x":4,"y":2,"i":1,"z":0,"I":"%19"}]}}"#,
            "5967,80x24,0,0,18",
        ),
        (
            "two floating panes, one with a lower z, among two tiled ones",
            r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":40,"h":24,"x":0,"y":0,"l":0,"i":0,"I":"%20"},{"t":"p","w":39,"h":24,"x":41,"y":0,"a":true,"i":1,"I":"%21"},{"t":"p","w":18,"h":6,"x":8,"y":4,"i":3,"z":0,"I":"%23"},{"t":"p","w":18,"h":6,"x":4,"y":2,"i":2,"z":1,"I":"%22"}]}}"#,
            "bb11,80x24,0,0{40x24,0,0,20,39x24,41,0,21}",
        ),
    ];

    #[test]
    fn the_json_string_and_the_older_string_of_each_window_parse_to_the_same_cell() {
        for (what, json, old) in PAIRS {
            let from_json = parse(json).unwrap_or_else(|e| panic!("{what}: {e}"));
            let from_old = parse(old).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(from_json, from_old, "{what}");
        }
    }

    #[test]
    fn a_single_json_pane_is_a_leaf_with_its_pane_id_number() {
        let cell = parse(PAIRS[0].1).unwrap();
        assert_eq!(
            cell,
            Cell {
                width: 80,
                height: 24,
                x: 0,
                y: 0,
                kind: Kind::Leaf { pane_number: 0 },
            }
        );
    }

    #[test]
    fn json_h_and_v_are_side_by_side_and_stacked() {
        let side = parse(PAIRS[1].1).unwrap();
        assert!(
            matches!(side.kind, Kind::Horizontal(ref c) if c.len() == 2),
            "{side:?}"
        );
        let stack = parse(PAIRS[2].1).unwrap();
        assert!(
            matches!(stack.kind, Kind::Vertical(ref c) if c.len() == 2),
            "{stack:?}"
        );
    }

    #[test]
    fn the_leaf_number_is_the_pane_id_never_the_index() {
        // Index 0, id %12: the older format writes 12, and so must this one.
        let cell = parse(PAIRS[5].1).unwrap();
        assert_eq!(cell.kind, Kind::Leaf { pane_number: 12 });
        // In a group the ids run 8, 9, 10 while the indexes run 0, 1, 2.
        let Kind::Horizontal(cells) = parse(PAIRS[4].1).unwrap().kind else {
            panic!("not a side-by-side group");
        };
        let numbers: Vec<_> = cells
            .iter()
            .map(|c| match c.kind {
                Kind::Leaf { pane_number } => pane_number,
                _ => panic!("not a leaf"),
            })
            .collect();
        assert_eq!(numbers, [8, 9, 10]);
    }

    #[test]
    fn a_json_pane_cell_ignores_the_active_and_last_markers_and_unknown_keys() {
        let plain = r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"y":0,"I":"%3"}}"#;
        let busy = r#"{"V":2,"future":[1,2],"L":{"t":"p","w":80,"h":24,"x":0,"y":0,"l":4,"a":true,"i":9,"n":"x","I":"%3"}}"#;
        assert_eq!(parse(plain).unwrap(), parse(busy).unwrap());
    }

    #[test]
    fn a_floating_pane_is_not_in_the_tree() {
        // Tiled panes %15 and %16 only: the floating %17 is gone and the split keeps two.
        let Kind::Horizontal(cells) = parse(PAIRS[8].1).unwrap().kind else {
            panic!("not a side-by-side group");
        };
        assert_eq!(cells.len(), 2);
        // The floating cell's geometry never reaches the tree.
        assert!(
            cells.iter().all(|c| c.width >= 39 && c.height == 24),
            "{cells:?}"
        );
    }

    #[test]
    fn a_split_left_with_no_tiled_cell_is_dropped_and_the_rest_stays() {
        // The shape tmux 3.8 writes: the split that holds only floating panes has w, h, x
        // and y all 0. The older string is what tmux's compat output gives for the window.
        let layout = r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":80,"h":24,"x":0,"y":0,"a":true,"i":0,"I":"%8"},{"t":"v","w":0,"h":0,"x":0,"y":0,"c":[{"t":"p","w":40,"h":6,"x":4,"y":2,"i":1,"z":1,"I":"%9"},{"t":"p","w":40,"h":6,"x":8,"y":4,"i":2,"z":0,"I":"%10"}]}]}}"#;
        let cell = parse(layout).unwrap();
        assert_eq!(cell.kind, Kind::Leaf { pane_number: 8 });
        assert_eq!(cell, parse("b265,80x24,0,0,8").unwrap());
    }

    #[test]
    fn a_floating_pane_may_sit_at_a_negative_position() {
        let layout = r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":40,"h":24,"x":0,"y":0,"i":0,"I":"%1"},{"t":"p","w":39,"h":24,"x":41,"y":0,"i":1,"I":"%2"},{"t":"p","w":18,"h":6,"x":-5,"y":-2,"i":2,"z":0,"I":"%3"}]}}"#;
        let Kind::Horizontal(cells) = parse(layout).unwrap().kind else {
            panic!("not a side-by-side group");
        };
        assert_eq!(cells.len(), 2);
    }

    #[test]
    fn a_tiled_root_is_moved_to_the_origin() {
        // What tmux's own older-format output does with the root of a window.
        let layout = r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":3,"y":2,"i":0,"I":"%0"}}"#;
        let cell = parse(layout).unwrap();
        assert_eq!((cell.x, cell.y), (0, 0));
    }

    #[test]
    fn a_split_of_one_cell_collapses_to_that_cell() {
        let layout = r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":80,"h":24,"x":0,"y":0,"i":0,"I":"%7"}]}}"#;
        assert_eq!(parse(layout).unwrap().kind, Kind::Leaf { pane_number: 7 });
    }

    #[test]
    fn the_json_tree_renders_like_the_older_tree() {
        let ids = |n: u32| Some((format!("w1:p{}", n + 1), format!("%{n}"), n));
        for (what, json, old) in PAIRS {
            assert_eq!(
                to_json(&parse(json).unwrap(), &ids),
                to_json(&parse(old).unwrap(), &ids),
                "{what}"
            );
        }
    }

    #[test]
    fn rejects_malformed_json_layouts_with_invalid_argument() {
        let leaf = r#"{"t":"p","w":80,"h":24,"x":0,"y":0,"i":0,"I":"%0"}"#;
        let cases: Vec<(&str, String)> = vec![
            ("not JSON", r#"{"V":2,"L":"#.to_string()),
            ("trailing input", format!(r#"{{"V":2,"L":{leaf}}} x"#)),
            ("no version", format!(r#"{{"L":{leaf}}}"#)),
            ("version 1", format!(r#"{{"V":1,"L":{leaf}}}"#)),
            ("version 3", format!(r#"{{"V":3,"L":{leaf}}}"#)),
            ("version as a string", format!(r#"{{"V":"2","L":{leaf}}}"#)),
            ("no cell", r#"{"V":2}"#.to_string()),
            ("cell is not an object", r#"{"V":2,"L":[]}"#.to_string()),
            (
                "no type",
                r#"{"V":2,"L":{"w":80,"h":24,"x":0,"y":0,"i":0,"I":"%0"}}"#.to_string(),
            ),
            (
                "type is not a string",
                r#"{"V":2,"L":{"t":1,"w":80,"h":24,"x":0,"y":0,"I":"%0"}}"#.to_string(),
            ),
            (
                "unknown type",
                r#"{"V":2,"L":{"t":"q","w":80,"h":24,"x":0,"y":0,"I":"%0"}}"#.to_string(),
            ),
            (
                "no width",
                r#"{"V":2,"L":{"t":"p","h":24,"x":0,"y":0,"I":"%0"}}"#.to_string(),
            ),
            (
                "no height",
                r#"{"V":2,"L":{"t":"p","w":80,"x":0,"y":0,"I":"%0"}}"#.to_string(),
            ),
            (
                "no x",
                r#"{"V":2,"L":{"t":"p","w":80,"h":24,"y":0,"I":"%0"}}"#.to_string(),
            ),
            (
                "no y",
                r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"I":"%0"}}"#.to_string(),
            ),
            (
                "width is a string",
                r#"{"V":2,"L":{"t":"p","w":"80","h":24,"x":0,"y":0,"I":"%0"}}"#.to_string(),
            ),
            (
                "width is a fraction",
                r#"{"V":2,"L":{"t":"p","w":80.5,"h":24,"x":0,"y":0,"I":"%0"}}"#.to_string(),
            ),
            (
                "width is negative",
                r#"{"V":2,"L":{"t":"p","w":-80,"h":24,"x":0,"y":0,"I":"%0"}}"#.to_string(),
            ),
            (
                "width overflows 32 bits",
                r#"{"V":2,"L":{"t":"p","w":99999999999,"h":24,"x":0,"y":0,"I":"%0"}}"#.to_string(),
            ),
            (
                "a tiled pane at a negative x",
                r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":-1,"y":0,"I":"%0"}}"#.to_string(),
            ),
            (
                "no pane id",
                r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"y":0,"i":0}}"#.to_string(),
            ),
            (
                "pane id is a number",
                r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"y":0,"I":0}}"#.to_string(),
            ),
            (
                "pane id without the percent sign",
                r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"y":0,"I":"3"}}"#.to_string(),
            ),
            (
                "pane id with no digits",
                r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"y":0,"I":"%"}}"#.to_string(),
            ),
            (
                "pane id with a letter",
                r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"y":0,"I":"%3a"}}"#.to_string(),
            ),
            (
                "pane id with a sign",
                r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"y":0,"I":"%-3"}}"#.to_string(),
            ),
            (
                "pane id with a plus sign",
                r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"y":0,"I":"%+3"}}"#.to_string(),
            ),
            (
                "an unknown type that has cells",
                format!(r#"{{"V":2,"L":{{"t":"q","w":80,"h":24,"x":0,"y":0,"c":[{leaf}]}}}}"#),
            ),
            (
                "an empty split beside a pane",
                format!(
                    r#"{{"V":2,"L":{{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[{leaf},{{"t":"v","w":80,"h":1,"x":0,"y":0,"c":[]}}]}}}}"#
                ),
            ),
            (
                "pane id overflows 32 bits",
                r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"y":0,"I":"%99999999999"}}"#.to_string(),
            ),
            (
                "a split with no c",
                r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0}}"#.to_string(),
            ),
            (
                "a split whose c is not an array",
                r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":{}}}"#.to_string(),
            ),
            (
                "a split with no cells",
                r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[]}}"#.to_string(),
            ),
            (
                "a bad cell inside a split",
                format!(
                    r#"{{"V":2,"L":{{"t":"v","w":80,"h":24,"x":0,"y":0,"c":[{leaf},{{"t":"p","w":80}}]}}}}"#
                ),
            ),
            (
                "a bad floating pane",
                format!(
                    r#"{{"V":2,"L":{{"t":"v","w":80,"h":24,"x":0,"y":0,"c":[{leaf},{{"t":"p","w":18,"h":6,"x":4,"y":2,"z":0,"I":"%x"}}]}}}}"#
                ),
            ),
            (
                "a floating pane with a z that is not an integer",
                format!(
                    r#"{{"V":2,"L":{{"t":"v","w":80,"h":24,"x":0,"y":0,"c":[{leaf},{{"t":"p","w":18,"h":6,"x":4,"y":2,"z":"0","I":"%5"}}]}}}}"#
                ),
            ),
            (
                "a floating pane with no width",
                format!(
                    r#"{{"V":2,"L":{{"t":"v","w":80,"h":24,"x":0,"y":0,"c":[{leaf},{{"t":"p","h":6,"x":4,"y":2,"i":1,"z":0,"I":"%5"}}]}}}}"#
                ),
            ),
            (
                "a floating pane whose x is not an integer",
                format!(
                    r#"{{"V":2,"L":{{"t":"v","w":80,"h":24,"x":0,"y":0,"c":[{leaf},{{"t":"p","w":18,"h":6,"x":"4","y":2,"i":1,"z":0,"I":"%5"}}]}}}}"#
                ),
            ),
        ];
        for (what, bad) in &cases {
            let err = parse(bad).expect_err(what);
            assert_eq!(code_for(&err), "invalid_argument", "{what}");
            assert!(
                err.to_string().contains(bad.as_str()),
                "{what}: the error should name the layout: {err}"
            );
        }
    }

    #[test]
    fn a_window_of_floating_panes_only_says_so_and_is_not_called_malformed() {
        // The first is what tmux 3.8 wrote after `break-pane -W` on the only tiled pane (a
        // negative position, as tmux writes it). The second is derived from tmux's source,
        // not captured: with a second floating pane, tmux wraps both in a split whose
        // geometry is copied from a floating cell (here a negative x, so the split's own
        // offsets must not be read before its cells).
        let one =
            r#"{"V":2,"L":{"t":"p","w":18,"h":4,"x":-4,"y":-2,"a":true,"i":0,"z":0,"I":"%3"}}"#;
        let two = r#"{"V":2,"L":{"t":"h","w":18,"h":4,"x":-4,"y":-2,"c":[{"t":"p","w":18,"h":4,"x":-4,"y":-2,"a":true,"i":0,"z":1,"I":"%3"},{"t":"p","w":18,"h":4,"x":6,"y":3,"i":1,"z":0,"I":"%4"}]}}"#;
        for (what, layout) in [("one floating pane", one), ("a split of two", two)] {
            let err = parse(layout).expect_err(what);
            assert_eq!(code_for(&err), "invalid_argument", "{what}");
            assert_eq!(
                err.to_string(),
                format!(
                    "window layout '{layout}' holds only floating panes, so it has no split tree"
                ),
                "{what}"
            );
        }
    }

    #[test]
    fn a_split_of_floating_panes_only_has_its_own_geometry_checked() {
        let tiled = r#"{"t":"p","w":80,"h":12,"x":0,"y":0,"i":0,"I":"%1"}"#;
        let floating = |n: u32, i: u32| {
            format!(
                r#"{{"t":"p","w":18,"h":4,"x":-4,"y":-2,"i":{i},"z":{n},"I":"%{}"}}"#,
                n + 5
            )
        };
        let (f0, f1) = (floating(0, 1), floating(1, 2));
        // The split of two floating panes, with its geometry as given, beside a tiled pane, and
        // as the whole window.
        let nested = |geometry: &str| {
            format!(
                r#"{{"V":2,"L":{{"t":"v","w":80,"h":24,"x":0,"y":0,"c":[{tiled},{{"t":"h",{geometry},"c":[{f0},{f1}]}}]}}}}"#
            )
        };
        let alone =
            |geometry: &str| format!(r#"{{"V":2,"L":{{"t":"h",{geometry},"c":[{f0},{f1}]}}}}"#);
        // As tmux writes it (offsets copied from a floating cell, negative) the tiled pane stays and
        // the window of floating panes only says so.
        let good = r#""w":18,"h":4,"x":-4,"y":-2"#;
        assert!(parse(&nested(good)).is_ok());
        assert!(
            parse(&alone(good))
                .expect_err("floating only")
                .to_string()
                .contains("holds only floating panes")
        );
        for (what, geometry) in [
            ("no width", r#""h":4,"x":-4,"y":-2"#),
            ("no height", r#""w":18,"x":-4,"y":-2"#),
            (
                "an x that is not an integer",
                r#""w":18,"h":4,"x":"4","y":-2"#,
            ),
            ("no y", r#""w":18,"h":4,"x":-4"#),
            (
                "a width that is not an integer",
                r#""w":1.5,"h":4,"x":-4,"y":-2"#,
            ),
        ] {
            for layout in [nested(geometry), alone(geometry)] {
                let err = parse(&layout).expect_err(what);
                assert_eq!(code_for(&err), "invalid_argument", "{what}");
                assert!(
                    err.to_string().contains(layout.as_str())
                        && !err.to_string().contains("holds only floating panes"),
                    "{what}: {err}"
                );
            }
        }
    }

    #[test]
    fn a_split_of_a_tiled_pane_and_floating_ones_keeps_its_own_geometry_check() {
        // A floating cell may have negative offsets, and those are not checked as a tiled cell's are;
        // a split with a tiled pane under it keeps the check on its own offsets.
        let layout = r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":-1,"y":0,"c":[{"t":"p","w":40,"h":24,"x":0,"y":0,"i":0,"I":"%1"},{"t":"p","w":39,"h":24,"x":41,"y":0,"i":1,"I":"%2"}]}}"#;
        assert!(parse(layout).is_err());
    }
}

#[cfg(test)]
mod flatten_tests {
    use super::*;

    fn ids(number: u32) -> Option<(String, String, u32)> {
        Some((
            format!("w1:p{}", number + 1),
            format!("%{}", number + 1),
            number,
        ))
    }

    #[test]
    fn a_single_pane_window_flattens_to_one_pane_and_no_splits() {
        let cell = parse("f1a2,80x24,0,0,0").unwrap();
        let flat = flatten(&cell, &ids, "w1:p1");
        assert_eq!(flat.splits, Vec::<Value>::new());
        assert_eq!(flat.panes.len(), 1);
        assert_eq!(flat.panes[0]["pane_id"], "w1:p1");
        assert_eq!(flat.panes[0]["focused"], true);
        assert_eq!(
            flat.panes[0]["rect"],
            json!({"x": 0, "y": 0, "width": 80, "height": 24})
        );
    }

    #[test]
    fn a_two_pane_split_flattens_to_two_panes_and_the_root_split() {
        let cell = parse("abcd,80x24,0,0{40x24,0,0,0,39x24,41,0,1}").unwrap();
        let flat = flatten(&cell, &ids, "w1:p2");
        assert_eq!(flat.panes.len(), 2);
        assert_eq!(flat.panes[0]["focused"], false);
        assert_eq!(flat.panes[1]["pane_id"], "w1:p2");
        assert_eq!(flat.panes[1]["focused"], true);
        assert_eq!(flat.splits.len(), 1);
        assert_eq!(flat.splits[0]["id"], "root");
        assert_eq!(flat.splits[0]["direction"], "right");
        assert_eq!(flat.splits[0]["ratio"], 0.5);
        // The split's rect is the span its children cover, divider included.
        assert_eq!(
            flat.splits[0]["rect"],
            json!({"x": 0, "y": 0, "width": 80, "height": 24})
        );
    }

    #[test]
    fn a_three_way_group_flattens_to_two_nested_splits_whose_ids_are_paths() {
        // Three cells side by side: the BSP form is split(c0, split(c1, c2)), so the
        // second split's id is the path taken to reach it — the same `first`/`second`
        // vocabulary `layout set-split-ratio --path` takes.
        let cell = parse("abcd,80x24,0,0{26x24,0,0,0,26x24,27,0,1,26x24,54,0,2}").unwrap();
        let flat = flatten(&cell, &ids, "w1:p1");
        assert_eq!(flat.panes.len(), 3);
        let ids_of: Vec<&str> = flat
            .splits
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids_of, vec!["root", "root.second"]);
        // The nested split covers only the span of the two cells it holds, not the window.
        assert_eq!(flat.splits[1]["rect"]["x"], 27);
        assert_eq!(flat.splits[1]["rect"]["width"], 53);
    }

    #[test]
    fn a_stacked_group_reports_direction_down() {
        let cell = parse("abcd,80x24,0,0[80x12,0,0,0,80x11,0,13,1]").unwrap();
        let flat = flatten(&cell, &ids, "w1:p1");
        assert_eq!(flat.splits.len(), 1);
        assert_eq!(flat.splits[0]["direction"], "down");
        assert_eq!(flat.splits[0]["rect"]["height"], 24);
    }

    #[test]
    fn an_unresolvable_leaf_reports_a_null_pane_id_rather_than_dropping_the_pane() {
        // A pane tmux listed in the layout but not in `list-panes` (raced with a kill)
        // must still occupy its cell: dropping it would report a layout with a hole.
        let cell = parse("abcd,80x24,0,0{40x24,0,0,0,39x24,41,0,1}").unwrap();
        let flat = flatten(&cell, &|_| None, "w1:p1");
        assert_eq!(flat.panes.len(), 2);
        assert!(flat.panes[0]["pane_id"].is_null());
        assert_eq!(flat.panes[0]["focused"], false);
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn the_root_split_is_the_empty_path() {
        let cell = parse("abcd,80x24,0,0{40x24,0,0,0,39x24,41,0,1}").unwrap();
        let found = at_path(&cell, &[]).unwrap();
        assert!(found.horizontal);
        assert_eq!(found.span, 80);
        assert_eq!(found.first_pane_number, 0);
    }

    #[test]
    fn a_nested_split_is_measured_against_its_own_span_not_the_window() {
        // split(c0, split(c1, c2)): the second branch covers only x 27..80.
        let cell = parse("abcd,80x24,0,0{26x24,0,0,0,26x24,27,0,1,26x24,54,0,2}").unwrap();
        let nested = at_path(&cell, &[true]).unwrap();
        assert!(nested.horizontal);
        assert_eq!(nested.span, 53);
        assert_eq!(nested.first_pane_number, 1);
    }

    #[test]
    fn a_path_that_reaches_a_pane_or_runs_off_the_tree_is_not_a_split() {
        let cell = parse("abcd,80x24,0,0{40x24,0,0,0,39x24,41,0,1}").unwrap();
        // `first` reaches a pane, not a split.
        assert!(at_path(&cell, &[false]).is_none());
        assert!(at_path(&cell, &[true, true, true]).is_none());
        // A single-pane window has no split at all.
        let single = parse("f1a2,80x24,0,0,0").unwrap();
        assert!(at_path(&single, &[]).is_none());
    }

    #[test]
    fn a_stacked_split_reports_the_vertical_axis() {
        let cell = parse("abcd,80x24,0,0[80x12,0,0,0,80x11,0,13,1]").unwrap();
        let found = at_path(&cell, &[]).unwrap();
        assert!(!found.horizontal);
        assert_eq!(found.span, 24);
    }

    #[test]
    fn render_path_matches_the_split_id_flatten_reports() {
        assert_eq!(render_path(&[]), "root");
        assert_eq!(render_path(&[false]), "root.first");
        assert_eq!(render_path(&[true, false]), "root.second.first");
    }
}
