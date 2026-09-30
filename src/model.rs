use image::RgbaImage;

/// Geometry + window types. Pixels stay raw RgbaImage end to end (no PNG roundtrip).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
}

/// con id + (x, y, w, h). Shared by Term.below, Snapshot.below, tree walk.
pub type BelowEntry = (i64, (i32, i32, i32, i32));

/// One visible allowlisted window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub con_id: i64,
    pub app_id: String,
    /// Output-local rect (screencopy crop space).
    pub rect: Rect,
    pub output: String,
    pub ws_id: i64,
    pub floating: bool,
    /// Every OTHER visible window on the same workspace (cache fingerprint).
    pub below: Vec<BelowEntry>,
}

impl Term {
    pub fn key(&self) -> (String, i64) {
        (self.output.clone(), self.con_id)
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub img: RgbaImage,
    pub rect: Rect,
    pub output: String,
    pub below: Vec<BelowEntry>,
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
