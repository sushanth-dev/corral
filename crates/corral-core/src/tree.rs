pub type PaneId = u32;

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Dir {
    Horizontal,
    Vertical,
}

#[derive(Clone)]
pub enum Node {
    Leaf(PaneId),
    Split {
        dir: Dir,
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

impl Node {
    pub fn leaf(id: PaneId) -> Self {
        Node::Leaf(id)
    }

    pub fn split(dir: Dir, ratio: f32, a: Box<Node>, b: Box<Node>) -> Self {
        Node::Split { dir, ratio, a, b }
    }

    pub fn rects(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        match self {
            Node::Leaf(id) => vec![(*id, area)],
            Node::Split { dir, ratio, a, b } => {
                let (area_a, area_b) = split_areas(*dir, *ratio, area);
                let mut out = a.rects(area_a);
                out.extend(b.rects(area_b));
                out
            }
        }
    }

    /// Sets the ratio of the split whose immediate child is the leaf `at`,
    /// clamped so neither side collapses below one cell in `area` (the
    /// area this node itself covers). Returns whether such a split was
    /// found. A leaf touches exactly one immediate parent split, so this
    /// is the gutter a drag starting on that leaf's border would move; a
    /// leaf nested deeper on the same side has its own, closer parent
    /// split, found first since we check direct children before recursing.
    pub fn set_ratio_near(&mut self, area: Rect, at: PaneId, ratio: f32) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Split {
                dir,
                ratio: r,
                a,
                b,
            } => {
                let a_is_at = matches!(**a, Node::Leaf(x) if x == at);
                let b_is_at = matches!(**b, Node::Leaf(x) if x == at);
                if a_is_at || b_is_at {
                    let dim = match dir {
                        Dir::Horizontal => area.w,
                        Dir::Vertical => area.h,
                    };
                    *r = clamp_ratio(ratio, dim);
                    return true;
                }
                let (area_a, area_b) = split_areas(*dir, *r, area);
                a.set_ratio_near(area_a, at, ratio) || b.set_ratio_near(area_b, at, ratio)
            }
        }
    }

    pub fn focus_dir(&self, focused: PaneId, dir: Dir) -> Option<PaneId> {
        // Pane centers in a virtual 1000x1000 space to avoid u16 overflow.
        let area_for = |id: PaneId| -> Option<(i32, i32)> {
            self.rects(Rect {
                x: 0,
                y: 0,
                w: 1000,
                h: 1000,
            })
            .into_iter()
            .find(|(p, _)| *p == id)
            .map(|(_, r)| (r.x as i32 + r.w as i32 / 2, r.y as i32 + r.h as i32 / 2))
        };
        let (fx, fy) = area_for(focused)?;
        let mut best: Option<(PaneId, i64)> = None;
        for id in self.leaf_ids() {
            if id == focused {
                continue;
            }
            let Some((cx, cy)) = area_for(id) else {
                continue;
            };
            let (dx, dy) = (cx - fx, cy - fy);
            let (primary, lateral) = match dir {
                Dir::Horizontal => (dx, dy.abs()),
                Dir::Vertical => (dy, dx.abs()),
            };
            // The neighbor must sit strictly on the requested side.
            if primary == 0 {
                continue;
            }
            if lateral * 2 > primary.abs().max(1) * 5 {
                continue;
            }
            let dist = (dx as i64) * (dx as i64) + (dy as i64) * (dy as i64);
            if best.map(|(_, d)| dist < d).unwrap_or(true) {
                best = Some((id, dist));
            }
        }
        best.map(|(id, _)| id)
    }

    pub fn replace(&mut self, id: PaneId, new: Node) -> bool {
        match self {
            Node::Leaf(existing) if *existing == id => {
                *self = new;
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => {
                a.replace(id, new.clone()) || {
                    let _ = new;
                    b.replace(id, new)
                }
            }
        }
    }

    pub fn remove(&mut self, id: PaneId) -> Option<PaneId> {
        match self {
            Node::Leaf(_) => None,
            Node::Split { a, b, .. } => {
                if matches!(**a, Node::Leaf(x) if x == id) {
                    let replacement = std::mem::replace(b, Box::new(Node::Leaf(0)));
                    *self = *replacement;
                    return sibling_id(self);
                }
                if matches!(**b, Node::Leaf(x) if x == id) {
                    let replacement = std::mem::replace(a, Box::new(Node::Leaf(0)));
                    *self = *replacement;
                    return sibling_id(self);
                }
                a.remove(id).or_else(|| b.remove(id))
            }
        }
    }

    pub fn leaf_ids(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.collect_leaves(out);
                b.collect_leaves(out);
            }
        }
    }
}

fn sibling_id(node: &Node) -> Option<PaneId> {
    match node {
        Node::Leaf(id) => Some(*id),
        _ => None,
    }
}

/// Cuts `area` into the two sides of a split at `ratio`, leaving a
/// one-cell gutter between them. Shared by `rects` (tiling for render)
/// and `set_ratio_near` (finding a nested split's own area to clamp
/// against), so the two never disagree on where the cut falls.
fn split_areas(dir: Dir, ratio: f32, area: Rect) -> (Rect, Rect) {
    match dir {
        Dir::Horizontal => {
            let cut = ((area.w as f32) * ratio).round() as u16;
            let lw = cut.saturating_sub(1).max(1);
            let rx = area.x + cut;
            let rw = area.w.saturating_sub(cut).max(1);
            (
                Rect { w: lw, ..area },
                Rect {
                    x: rx,
                    w: rw,
                    ..area
                },
            )
        }
        Dir::Vertical => {
            let cut = ((area.h as f32) * ratio).round() as u16;
            let th = cut.saturating_sub(1).max(1);
            let by = area.y + cut;
            let bh = area.h.saturating_sub(cut).max(1);
            (
                Rect { h: th, ..area },
                Rect {
                    y: by,
                    h: bh,
                    ..area
                },
            )
        }
    }
}

/// Keeps a ratio from putting either side of a `dim`-cell split below one
/// cell.
fn clamp_ratio(ratio: f32, dim: u16) -> f32 {
    let dim = (dim.max(2)) as f32;
    ratio.clamp(1.0 / dim, 1.0 - 1.0 / dim)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        w: 101,
        h: 40,
    };

    #[test]
    fn single_leaf_covers_area() {
        let node = Node::leaf(1);
        assert_eq!(node.rects(AREA), vec![(1, AREA)]);
    }

    #[test]
    fn horizontal_split_leaves_one_gutter_column() {
        let node = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::leaf(2)),
        );
        let rects = node.rects(AREA);
        assert_eq!(
            rects[0],
            (
                1,
                Rect {
                    x: 0,
                    y: 0,
                    w: 50,
                    h: 40
                }
            )
        );
        assert_eq!(
            rects[1],
            (
                2,
                Rect {
                    x: 51,
                    y: 0,
                    w: 50,
                    h: 40
                }
            )
        );
    }

    #[test]
    fn nested_splits_tile_without_overlap() {
        let node = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::split(
                Dir::Vertical,
                0.5,
                Box::new(Node::leaf(2)),
                Box::new(Node::leaf(3)),
            )),
        );
        let rects = node.rects(AREA);
        assert_eq!(rects.len(), 3);
        for (i, (_, a)) in rects.iter().enumerate() {
            for (_, b) in rects.iter().skip(i + 1) {
                let overlap =
                    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
                assert!(!overlap, "{a:?} overlaps {b:?}");
            }
            assert!(a.w > 0 && a.h > 0);
        }
    }

    #[test]
    fn focus_moves_to_the_neighbor_on_the_requested_side() {
        let node = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::leaf(2)),
        );
        assert_eq!(node.focus_dir(1, Dir::Horizontal), Some(2));
        assert_eq!(node.focus_dir(2, Dir::Horizontal), Some(1));
        assert_eq!(node.focus_dir(1, Dir::Vertical), None);
    }

    #[test]
    fn focus_finds_neighbor_in_nested_tree() {
        let node = Node::split(
            Dir::Vertical,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::split(
                Dir::Horizontal,
                0.5,
                Box::new(Node::leaf(2)),
                Box::new(Node::leaf(3)),
            )),
        );
        // 2 and 3 sit side by side below 1.
        assert_eq!(node.focus_dir(2, Dir::Horizontal), Some(3));
        assert_eq!(node.focus_dir(3, Dir::Horizontal), Some(2));
        assert_eq!(node.focus_dir(2, Dir::Vertical), Some(1));
        // From 1 the nearest bottom pane wins; the gutter puts 3's center
        // slightly closer to 1's center than 2's, so accept either.
        let down = node.focus_dir(1, Dir::Vertical);
        assert!(matches!(down, Some(2) | Some(3)), "{down:?}");
    }

    #[test]
    fn replace_swaps_leaf_for_new_subtree() {
        let mut node = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::leaf(2)),
        );
        assert!(node.replace(2, Node::leaf(9)));
        assert_eq!(
            node.rects(AREA),
            vec![
                (
                    1,
                    Rect {
                        x: 0,
                        y: 0,
                        w: 50,
                        h: 40
                    }
                ),
                (
                    9,
                    Rect {
                        x: 51,
                        y: 0,
                        w: 50,
                        h: 40
                    }
                ),
            ]
        );
        assert!(!node.replace(42, Node::leaf(7)));
    }

    #[test]
    fn set_ratio_near_updates_the_matching_split() {
        let mut node = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::leaf(2)),
        );
        assert!(node.set_ratio_near(AREA, 1, 0.25));
        let rects = node.rects(AREA);
        assert_eq!(
            rects[0].1.w, 24,
            "left pane now gets a quarter, minus the gutter"
        );
        // Either leaf on the split identifies the same split.
        assert!(node.set_ratio_near(AREA, 2, 0.75));
        let rects = node.rects(AREA);
        assert_eq!(
            rects[1].1.w, 25,
            "right pane now gets a quarter of the area"
        );
    }

    #[test]
    fn set_ratio_near_finds_the_leafs_own_immediate_parent_in_a_nested_tree() {
        let mut node = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::split(
                Dir::Vertical,
                0.5,
                Box::new(Node::leaf(2)),
                Box::new(Node::leaf(3)),
            )),
        );
        // 2's own parent is the inner vertical split, not the outer one.
        assert!(node.set_ratio_near(AREA, 2, 0.75));
        let rects = node.rects(AREA);
        let (_, r1) = rects.iter().find(|(id, _)| *id == 1).unwrap();
        assert_eq!(r1.w, 50, "the outer split is untouched");
        let (_, r2) = rects.iter().find(|(id, _)| *id == 2).unwrap();
        assert!(r2.h > 20, "the inner split moved toward pane 2's ratio");
    }

    #[test]
    fn set_ratio_near_returns_false_when_the_pane_is_not_in_the_tree() {
        let mut node = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::leaf(2)),
        );
        assert!(!node.set_ratio_near(AREA, 42, 0.5));
    }

    #[test]
    fn set_ratio_near_clamps_so_neither_side_collapses_below_one_cell() {
        let mut node = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::leaf(2)),
        );
        let small = Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        };
        assert!(node.set_ratio_near(small, 1, 0.0));
        let rects = node.rects(small);
        assert!(rects[0].1.w >= 1, "left side kept at least one cell");
        assert!(rects[1].1.w >= 1, "right side kept at least one cell");

        assert!(node.set_ratio_near(small, 1, 1.0));
        let rects = node.rects(small);
        assert!(rects[0].1.w >= 1, "left side kept at least one cell");
        assert!(rects[1].1.w >= 1, "right side kept at least one cell");
    }

    #[test]
    fn remove_collapses_to_sibling() {
        let mut node = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::leaf(2)),
        );
        assert_eq!(node.remove(1), Some(2));
        assert_eq!(node.rects(AREA), vec![(2, AREA)]);
    }

    #[test]
    fn remove_on_root_leaf_is_a_no_op() {
        let mut node = Node::leaf(1);
        assert_eq!(node.remove(1), None);
        assert_eq!(node.rects(AREA), vec![(1, AREA)]);
    }

    #[test]
    fn rects_preserve_a_nonzero_origin() {
        let node = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::leaf(2)),
        );
        let area = Rect {
            x: 10,
            y: 5,
            w: 101,
            h: 40,
        };
        assert_eq!(
            node.rects(area),
            vec![
                (
                    1,
                    Rect {
                        x: 10,
                        y: 5,
                        w: 50,
                        h: 40
                    }
                ),
                (
                    2,
                    Rect {
                        x: 61,
                        y: 5,
                        w: 50,
                        h: 40
                    }
                ),
            ]
        );
    }

    #[test]
    fn asymmetric_ratio_gives_each_side_its_share() {
        let node = Node::split(
            Dir::Horizontal,
            0.25,
            Box::new(Node::leaf(1)),
            Box::new(Node::leaf(2)),
        );
        let area = Rect {
            x: 0,
            y: 0,
            w: 100,
            h: 10,
        };
        let rects = node.rects(area);
        assert_eq!(rects[0].1.w, 24, "left pane loses the gutter column");
        assert_eq!(rects[1].1.x, 25, "gutter column sits at the cut");
        assert_eq!(rects[1].1.w, 75);

        let node = Node::split(
            Dir::Vertical,
            0.75,
            Box::new(Node::leaf(1)),
            Box::new(Node::leaf(2)),
        );
        let tall = Rect {
            x: 0,
            y: 0,
            w: 100,
            h: 40,
        };
        let rects = node.rects(tall);
        assert_eq!(rects[0].1.h, 29);
        assert_eq!(rects[1].1.y, 30);
        assert_eq!(rects[1].1.h, 10);
    }

    #[test]
    fn degenerate_one_cell_areas_still_yield_positive_rects() {
        let area = Rect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        };
        let h = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::leaf(2)),
        );
        for (_, r) in h.rects(area) {
            assert!(r.w >= 1 && r.h >= 1, "horizontal gave {r:?}");
        }
        let v = Node::split(
            Dir::Vertical,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::leaf(2)),
        );
        for (_, r) in v.rects(area) {
            assert!(r.w >= 1 && r.h >= 1, "vertical gave {r:?}");
        }
    }

    #[test]
    fn removing_a_nested_pane_promotes_its_sibling_leaf() {
        let mut node = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::split(
                Dir::Vertical,
                0.5,
                Box::new(Node::leaf(2)),
                Box::new(Node::leaf(3)),
            )),
        );
        assert_eq!(node.remove(2), Some(3));
        let rects = node.rects(AREA);
        assert_eq!(rects.len(), 2);
        assert!(rects.iter().any(|(id, _)| *id == 1));
        assert!(rects.iter().any(|(id, _)| *id == 3));
    }

    #[test]
    fn removing_a_leaf_with_a_split_sibling_returns_none_but_collapses() {
        // Root H(V(2,3), 1): removing 1 collapses the root to V(2,3); the
        // sibling is a split, so no single pane id comes back.
        let mut node = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::split(
                Dir::Vertical,
                0.5,
                Box::new(Node::leaf(2)),
                Box::new(Node::leaf(3)),
            )),
            Box::new(Node::leaf(1)),
        );
        assert_eq!(node.remove(1), None);
        assert_eq!(node.rects(AREA).len(), 2);
    }

    #[test]
    fn replace_accepts_a_subtree_not_just_a_leaf() {
        let mut node = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::leaf(2)),
        );
        let subtree = Node::split(
            Dir::Vertical,
            0.5,
            Box::new(Node::leaf(8)),
            Box::new(Node::leaf(9)),
        );
        assert!(node.replace(2, subtree));
        let rects = node.rects(AREA);
        assert_eq!(rects.len(), 3);
        // The vertical cut of the right half (h 40) leaves a gutter row.
        assert_eq!(rects[1].1.h, 19);
        assert_eq!(rects[2].1.h, 20);
        assert_eq!(rects[2].1.y, 20);
    }

    #[test]
    fn focus_from_an_unknown_pane_is_none() {
        let node = Node::split(
            Dir::Horizontal,
            0.5,
            Box::new(Node::leaf(1)),
            Box::new(Node::leaf(2)),
        );
        assert_eq!(node.focus_dir(99, Dir::Horizontal), None);
        assert_eq!(node.focus_dir(99, Dir::Vertical), None);
    }

    #[test]
    fn focus_walks_within_a_row_of_three() {
        // Vertical split: panes 1 and 2 in the top row (Horizontal split),
        // pane 3 fills the bottom.
        let node = Node::split(
            Dir::Vertical,
            0.5,
            Box::new(Node::split(
                Dir::Horizontal,
                0.5,
                Box::new(Node::leaf(1)),
                Box::new(Node::leaf(2)),
            )),
            Box::new(Node::leaf(3)),
        );
        assert_eq!(node.focus_dir(1, Dir::Horizontal), Some(2));
        assert_eq!(node.focus_dir(2, Dir::Horizontal), Some(1));
        assert_eq!(node.focus_dir(1, Dir::Vertical), Some(3));
        assert_eq!(node.focus_dir(2, Dir::Vertical), Some(3));
    }

    #[test]
    fn rect_and_dir_round_trip_through_json() {
        for dir in [Dir::Horizontal, Dir::Vertical] {
            let line = serde_json::to_string(&dir).unwrap();
            assert_eq!(line, format!("\"{dir:?}\""));
            let back: Dir = serde_json::from_str(&line).unwrap();
            assert_eq!(back, dir);
        }
        let rect = Rect {
            x: 1,
            y: 2,
            w: 3,
            h: 4,
        };
        let line = serde_json::to_string(&rect).unwrap();
        assert_eq!(line, r#"{"x":1,"y":2,"w":3,"h":4}"#);
        let back: Rect = serde_json::from_str(&line).unwrap();
        assert_eq!(back, rect);
    }

    #[test]
    fn deep_nesting_stays_inside_the_area_and_disjoint() {
        let mut node = Node::leaf(0);
        for id in 1..8u32 {
            let dir = if id % 2 == 0 {
                Dir::Horizontal
            } else {
                Dir::Vertical
            };
            node = Node::split(dir, 0.5, Box::new(node), Box::new(Node::leaf(id)));
        }
        let rects = node.rects(AREA);
        assert_eq!(rects.len(), 8);
        for (_, r) in &rects {
            assert!(r.w > 0 && r.h > 0, "empty rect {r:?}");
            assert!(r.x + r.w <= AREA.w, "rect {r:?} exceeds width {}", AREA.w);
            assert!(r.y + r.h <= AREA.h, "rect {r:?} exceeds height {}", AREA.h);
        }
        for (i, (_, a)) in rects.iter().enumerate() {
            for (_, b) in rects.iter().skip(i + 1) {
                let overlap =
                    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
                assert!(!overlap, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn eight_panes_are_all_present_after_nesting() {
        let mut node = Node::leaf(0);
        for id in 1..8u32 {
            node = Node::split(
                Dir::Horizontal,
                0.5,
                Box::new(node),
                Box::new(Node::leaf(id)),
            );
        }
        let ids: Vec<PaneId> = node.rects(AREA).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }
}
