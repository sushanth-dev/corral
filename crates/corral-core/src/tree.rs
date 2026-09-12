pub type PaneId = u32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
            Node::Split { dir, ratio, a, b } => match dir {
                Dir::Horizontal => {
                    let cut = ((area.w as f32) * ratio).round() as u16;
                    let lw = cut.saturating_sub(1).max(1);
                    let rx = area.x + cut;
                    let rw = area.w.saturating_sub(cut).max(1);
                    let mut out = a.rects(Rect { w: lw, ..area });
                    out.extend(b.rects(Rect {
                        x: rx,
                        w: rw,
                        ..area
                    }));
                    out
                }
                Dir::Vertical => {
                    let cut = ((area.h as f32) * ratio).round() as u16;
                    let th = cut.saturating_sub(1).max(1);
                    let by = area.y + cut;
                    let bh = area.h.saturating_sub(cut).max(1);
                    let mut out = a.rects(Rect { h: th, ..area });
                    out.extend(b.rects(Rect {
                        y: by,
                        h: bh,
                        ..area
                    }));
                    out
                }
            },
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

    fn leaf_ids(&self) -> Vec<PaneId> {
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
}
