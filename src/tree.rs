use crate::ipc;
use crate::model::{BelowEntry, Rect, Term};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Deserialize)]
struct Node {
    #[serde(rename = "type", default)]
    kind: String,
    id: Option<i64>,
    name: Option<String>,
    rect: Option<Rect>,
    pid: Option<i64>,
    app_id: Option<String>,
    #[serde(default)]
    visible: bool,
    #[serde(default)]
    fullscreen_mode: i64,
    #[serde(default)]
    nodes: Vec<Node>,
    #[serde(default)]
    floating_nodes: Vec<Node>,
}

impl Node {
    fn children(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().chain(self.floating_nodes.iter())
    }

    fn is_con(&self) -> bool {
        matches!(self.kind.as_str(), "con" | "floating_con")
    }

    /// Real visible window that can be captured: has a client, is not
    /// fullscreen, and is on the visible workspace.
    fn is_eligible(&self) -> bool {
        (self.pid.unwrap_or(0) != 0 || self.app_id.is_some())
            && self.visible
            && self.fullscreen_mode == 0
    }
}

/// Reject fully off-screen windows; translate into output-local coordinates.
fn rect_on_output(rect: &Rect, out: &Rect) -> Option<Rect> {
    if rect.width < 2
        || rect.height < 2
        || rect.x >= out.x + out.width
        || rect.y >= out.y + out.height
        || rect.x + rect.width <= out.x
        || rect.y + rect.height <= out.y
    {
        return None;
    }
    Some(Rect {
        x: rect.x - out.x,
        y: rect.y - out.y,
        width: rect.width,
        height: rect.height,
    })
}

struct Walker<'a> {
    allow: &'a HashSet<String>,
    found: Vec<(Term, i64)>,
    /// ws_id -> visible leaves for that workspace (cache fingerprint input).
    ws_leaves: HashMap<i64, Vec<BelowEntry>>,
    /// Every con/floating_con id in the tree, including hidden workspaces and
    /// `__i3` (scratchpad), so their snapshots survive while hidden.
    all_ids: HashSet<i64>,
}

impl<'a> Walker<'a> {
    fn new(allow: &'a HashSet<String>) -> Self {
        Self {
            allow,
            found: Vec::new(),
            ws_leaves: HashMap::new(),
            all_ids: HashSet::new(),
        }
    }

    fn walk<'n>(
        &mut self,
        node: &'n Node,
        out_name: Option<&'n str>,
        out_rect: Option<Rect>,
        ws_id: Option<i64>,
        hidden: bool,
    ) {
        let (mut out_name, mut ws_id, mut hidden) = (out_name, ws_id, hidden);
        let mut out_rect = out_rect;

        if node.kind == "output" {
            if node.name.as_deref() == Some("__i3") {
                hidden = true;
            }
            out_name = node.name.as_deref();
            out_rect = node.rect.or(out_rect);
            ws_id = None;
        } else if node.kind == "workspace" {
            ws_id = node.id;
        }

        if node.is_con() {
            if let Some(id) = node.id {
                self.all_ids.insert(id);
                if !hidden && node.is_eligible() {
                    if let (Some(rect), Some(out), Some(ws)) = (node.rect, out_rect, ws_id) {
                        if let Some(visible) = rect_on_output(&rect, &out) {
                            self.ws_leaves
                                .entry(ws)
                                .or_default()
                                .push((id, (visible.x, visible.y, visible.width, visible.height)));
                            if let Some(app_id) = node.app_id.as_deref() {
                                if self.allow.contains(app_id) {
                                    self.found.push((
                                        Term {
                                            con_id: id,
                                            app_id: app_id.to_string(),
                                            rect: visible,
                                            output: out_name.unwrap_or_default().to_string(),
                                            below: Vec::new(),
                                        },
                                        ws,
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        for child in node.children() {
            self.walk(child, out_name, out_rect, ws_id, hidden);
        }
    }

    fn finish(self) -> (Vec<Term>, HashSet<i64>) {
        let mut out = Vec::with_capacity(self.found.len());
        for (mut t, ws) in self.found {
            let mut below: Vec<BelowEntry> = self
                .ws_leaves
                .get(&ws)
                .map(|v| v.iter().filter(|e| e.0 != t.con_id).cloned().collect())
                .unwrap_or_default();
            // con_ids are unique; stability buys nothing.
            below.sort_unstable();
            t.below = below;
            out.push(t);
        }
        (out, self.all_ids)
    }
}

fn collect(tree: &Node, allow: &HashSet<String>) -> (Vec<Term>, HashSet<i64>) {
    let mut w = Walker::new(allow);
    w.walk(tree, None, None, None, false);
    w.finish()
}

/// All VISIBLE allowlisted windows + every con id in the tree.
/// Single GET_TREE roundtrip: the id set drives cache eviction of closed
/// windows without dropping hidden-workspace snapshots.
pub fn get_visible_terminals_and_all_ids(
    allow: &HashSet<String>,
) -> Result<(Vec<Term>, HashSet<i64>)> {
    // Persistent connection: GET_TREE fires every refresh, so skip the
    // connect/close pair per call (same thread-local slot as the capture
    // commands; strictly sequential, never concurrent).
    let value = ipc::once_reused(ipc::T_GET_TREE, "")?;
    let tree: Node = serde_json::from_value(value).context("parsing sway tree")?;
    Ok(collect(&tree, allow))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Node {
        serde_json::from_str(json).expect("test tree must parse")
    }

    fn allow(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reads_typed_tree_fields() {
        let tree = parse(
            r#"{
              "type":"root","nodes":[
                {"type":"output","name":"DP-1","rect":{"x":0,"y":0,"width":100,"height":100},
                 "nodes":[
                   {"type":"workspace","id":7,"nodes":[
                     {"type":"con","id":1,"pid":10,"app_id":"foot","visible":true,"fullscreen_mode":0,
                      "rect":{"x":10,"y":5,"width":50,"height":40}}
                   ]}
                 ]}
              ]}"#,
        );
        let (terms, ids) = collect(&tree, &allow(&["foot"]));

        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].con_id, 1);
        assert_eq!(terms[0].app_id, "foot");
        assert_eq!(terms[0].output, "DP-1");
        assert_eq!(
            terms[0].rect,
            Rect {
                x: 10,
                y: 5,
                width: 50,
                height: 40
            }
        );
        assert!(terms[0].below.is_empty());
        assert!(ids.contains(&1));
    }

    #[test]
    fn fullscreen_and_hidden_terms_are_ids_only() {
        let tree = parse(
            r#"{
              "type":"root","nodes":[
                {"type":"output","name":"DP-1","rect":{"x":0,"y":0,"width":100,"height":100},
                 "nodes":[
                   {"type":"workspace","id":7,"nodes":[
                     {"type":"con","id":1,"pid":10,"app_id":"foot","visible":true,"fullscreen_mode":0,
                      "rect":{"x":0,"y":0,"width":50,"height":40}},
                     {"type":"con","id":3,"pid":12,"app_id":"foot","visible":true,"fullscreen_mode":1,
                      "rect":{"x":0,"y":0,"width":100,"height":100}},
                     {"type":"con","id":4,"pid":13,"app_id":"foot","visible":false,"fullscreen_mode":0,
                      "rect":{"x":0,"y":0,"width":50,"height":40}}
                   ]}
                 ]}
              ]}"#,
        );
        let (terms, ids) = collect(&tree, &allow(&["foot"]));

        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].con_id, 1);
        assert!(ids.contains(&3));
        assert!(ids.contains(&4));
    }

    #[test]
    fn __i3_children_keep_ids_but_are_not_terms() {
        let tree = parse(
            r#"{
              "type":"root","nodes":[
                {"type":"output","name":"__i3","rect":{"x":0,"y":0,"width":100,"height":100},
                 "nodes":[
                   {"type":"workspace","id":9,"nodes":[
                     {"type":"con","id":9,"pid":10,"app_id":"foot","visible":true,"fullscreen_mode":0,
                      "rect":{"x":0,"y":0,"width":50,"height":40}}
                   ]}
                 ]}
              ]}"#,
        );
        let (terms, ids) = collect(&tree, &allow(&["foot"]));

        assert!(terms.is_empty());
        assert!(ids.contains(&9));
    }
}
