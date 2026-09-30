use crate::ipc;
use crate::model::{BelowEntry, Rect, Term};
use anyhow::Result;
use std::collections::{HashMap, HashSet};

fn as_i32(v: &serde_json::Value, key: &str) -> i32 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0) as i32
}

fn rect_on_output(rect: &serde_json::Value, out: &serde_json::Value) -> Option<Rect> {
    let (rx, ry, rw, rh) = (
        as_i32(rect, "x"),
        as_i32(rect, "y"),
        as_i32(rect, "width"),
        as_i32(rect, "height"),
    );
    let (ox, oy, ow, oh) = (
        as_i32(out, "x"),
        as_i32(out, "y"),
        as_i32(out, "width"),
        as_i32(out, "height"),
    );
    if rw < 2 || rh < 2 || rx >= ox + ow || ry >= oy + oh || rx + rw <= ox || ry + rh <= oy {
        return None;
    }
    Some(Rect {
        x: rx - ox,
        y: ry - oy,
        width: rw,
        height: rh,
    })
}

struct Walker<'a> {
    allow: &'a HashSet<String>,
    found: Vec<Term>,
    /// ws_id -> list of [`BelowEntry`] for ALL visible leaves.
    ws_leaves: HashMap<i64, Vec<BelowEntry>>,
}

impl Walker<'_> {
    fn walk(
        &mut self,
        node: &serde_json::Value,
        out_name: Option<String>,
        out_rect: serde_json::Value,
        ws_id: Option<i64>,
    ) {
        let ntype = node.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let (mut out_name, mut out_rect, mut ws_id) = (out_name, out_rect, ws_id);
        if ntype == "output" {
            if node.get("name").and_then(|n| n.as_str()) == Some("__i3") {
                return;
            }
            out_name = node
                .get("name")
                .and_then(|n| n.as_str())
                .map(|s| s.to_string());
            out_rect = node.get("rect").cloned().unwrap_or(serde_json::json!({}));
            ws_id = None;
        } else if ntype == "workspace" {
            ws_id = node.get("id").and_then(|id| id.as_i64());
        }
        if (ntype == "con" || ntype == "floating_con")
            && (node.get("pid").and_then(|p| p.as_i64()).unwrap_or(0) != 0
                || node.get("app_id").and_then(|a| a.as_str()).is_some())
            && node
                .get("visible")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            && node
                .get("fullscreen_mode")
                .and_then(|f| f.as_i64())
                .unwrap_or(0)
                == 0
        {
            let empty = serde_json::json!({});
            let rect_v = node.get("rect").unwrap_or(&empty);
            if let Some(rect) = rect_on_output(rect_v, &out_rect) {
                if let (Some(con_id), Some(ws)) = (node.get("id").and_then(|id| id.as_i64()), ws_id)
                {
                    self.ws_leaves
                        .entry(ws)
                        .or_default()
                        .push((con_id, (rect.x, rect.y, rect.width, rect.height)));
                    if let Some(app_id) = node.get("app_id").and_then(|a| a.as_str()) {
                        if self.allow.contains(app_id) {
                            self.found.push(Term {
                                con_id,
                                app_id: app_id.to_string(),
                                rect,
                                output: out_name.clone().unwrap_or_default(),
                                ws_id: ws,
                                floating: ntype == "floating_con",
                                below: Vec::new(),
                            });
                        }
                    }
                }
            }
        }
        let empty_vec = Vec::new();
        let nodes = node
            .get("nodes")
            .and_then(|n| n.as_array())
            .unwrap_or(&empty_vec);
        let floating = node
            .get("floating_nodes")
            .and_then(|n| n.as_array())
            .unwrap_or(&empty_vec);
        for child in nodes.iter().chain(floating.iter()) {
            self.walk(child, out_name.clone(), out_rect.clone(), ws_id);
        }
    }
}

/// All VISIBLE allowlisted windows. Each carries a `below` fingerprint,
/// so focus/workspace switches are pure cache hits.
pub fn get_visible_terminals(allow: &HashSet<String>) -> Result<Vec<Term>> {
    let tree = ipc::once(ipc::T_GET_TREE, "")?;
    let mut w = Walker {
        allow,
        found: Vec::new(),
        ws_leaves: HashMap::new(),
    };
    let empty = Vec::new();
    let tops = tree
        .get("nodes")
        .and_then(|n| n.as_array())
        .unwrap_or(&empty);
    for top in tops {
        w.walk(top, None, serde_json::json!({}), None);
    }
    let mut out = Vec::with_capacity(w.found.len());
    for mut t in w.found {
        let mut below: Vec<BelowEntry> = w
            .ws_leaves
            .get(&t.ws_id)
            .map(|v| v.iter().filter(|e| e.0 != t.con_id).cloned().collect())
            .unwrap_or_default();
        below.sort();
        t.below = below;
        out.push(t);
    }
    Ok(out)
}
