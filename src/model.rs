use serde::Deserialize;

/// sway container id. Unique for the session; distinct from workspace ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConId(pub i64);

impl std::fmt::Display for ConId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Geometry. Rects are output-local unless noted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn geometry(&self) -> String {
        format!("{},{} {}x{}", self.x, self.y, self.width, self.height)
    }

    /// Visible part of `self` inside `outer`, translated into outer-local
    /// coordinates. None when the intersection is smaller than 2x2.
    pub fn intersection(&self, outer: &Rect) -> Option<Rect> {
        let x1 = self.x.max(outer.x);
        let y1 = self.y.max(outer.y);
        let x2 = (self.x + self.width).min(outer.x + outer.width);
        let y2 = (self.y + self.height).min(outer.y + outer.height);
        if x2 - x1 < 2 || y2 - y1 < 2 {
            return None;
        }
        Some(Rect {
            x: x1 - outer.x,
            y: y1 - outer.y,
            width: x2 - x1,
            height: y2 - y1,
        })
    }
}

/// One visible window behind a terminal: id + output-local visible rect.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Below {
    pub con_id: ConId,
    pub rect: Rect,
}

/// One visible allowlisted window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub con_id: ConId,
    pub app_id: String,
    /// Visible part of the window on its output.
    pub rect: Rect,
    pub output: String,
    /// Every OTHER visible window on the same workspace (cache fingerprint).
    pub below: Vec<Below>,
}

impl Term {
    /// Overlay surface identity. A surface lives on one output; moving a
    /// window to another output needs a new surface (monitor is set pre-map).
    pub fn surface_key(&self) -> SurfaceKey {
        SurfaceKey {
            output: self.output.clone(),
            con_id: self.con_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SurfaceKey {
    pub output: String,
    pub con_id: ConId,
}

/// Output geometry from sway GET_OUTPUTS.
#[derive(Debug, Clone, Copy, Default)]
pub struct OutRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl OutRect {
    pub fn from_json(v: &serde_json::Value) -> Self {
        let g = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0) as i32;
        Self {
            x: g("x"),
            y: g("y"),
            width: g("width"),
            height: g("height"),
        }
    }
}
