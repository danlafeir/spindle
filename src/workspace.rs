use ratatui::layout::{Constraint, Layout, Rect};

pub type PaneId = usize;

// Below this a half can't show a bordered pane with usable room inside.
const MIN_WIDTH: u16 = 10;
const MIN_HEIGHT: u16 = 4;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Axis {
    SideBySide,
    Stacked,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, PartialEq)]
enum Node {
    Pane(PaneId),
    Split { axis: Axis, children: Vec<Node> },
}

impl Node {
    fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Pane(id) => out.push(*id),
            Node::Split { children, .. } => children.iter().for_each(|c| c.leaves(out)),
        }
    }

    fn rects(&self, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Node::Pane(id) => out.push((*id, area)),
            Node::Split { axis, children } => {
                let n = children.len() as u32;
                let constraints = vec![Constraint::Ratio(1, n); children.len()];
                let areas = match axis {
                    Axis::SideBySide => Layout::horizontal(constraints).split(area),
                    Axis::Stacked => Layout::vertical(constraints).split(area),
                };
                for (child, area) in children.iter().zip(areas.iter()) {
                    child.rects(*area, out);
                }
            }
        }
    }

    /// Splitting along the axis a pane is already split on adds a sibling rather than nesting,
    /// so three side-by-side splits give three equal columns, as in tmux.
    fn split(&mut self, target: PaneId, axis: Axis, new: PaneId) -> bool {
        match self {
            Node::Pane(id) if *id == target => {
                *self = Node::Split {
                    axis,
                    children: vec![Node::Pane(target), Node::Pane(new)],
                };
                true
            }
            Node::Pane(_) => false,
            Node::Split {
                axis: own,
                children,
            } => {
                if *own == axis
                    && let Some(i) = children.iter().position(|c| *c == Node::Pane(target))
                {
                    children.insert(i + 1, Node::Pane(new));
                    return true;
                }
                children.iter_mut().any(|c| c.split(target, axis, new))
            }
        }
    }

    /// Removes `target` from below this split, collapsing any split left with one child.
    fn remove(&mut self, target: PaneId) -> bool {
        let Node::Split { axis, children } = self else {
            return false;
        };
        let removed = match children.iter().position(|c| *c == Node::Pane(target)) {
            Some(i) => {
                children.remove(i);
                true
            }
            None => children.iter_mut().any(|c| c.remove(target)),
        };
        // A child that collapsed onto our own axis is spliced in, keeping siblings equal-sized.
        let mut i = 0;
        while i < children.len() {
            match &mut children[i] {
                Node::Split {
                    axis: inner,
                    children: grandchildren,
                } if inner == axis => {
                    let grandchildren = std::mem::take(grandchildren);
                    let n = grandchildren.len();
                    children.splice(i..=i, grandchildren);
                    i += n;
                }
                _ => i += 1,
            }
        }
        if children.len() == 1 {
            *self = children.pop().expect("one child");
        }
        removed
    }
}

/// One workspace's panes: a tree of splits, each sharing its area equally among its children.
#[derive(Debug)]
pub struct Workspace {
    root: Node,
    focus: PaneId,
}

impl Workspace {
    pub fn new(pane: PaneId) -> Workspace {
        Workspace {
            root: Node::Pane(pane),
            focus: pane,
        }
    }

    pub fn focus(&self) -> PaneId {
        self.focus
    }

    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.root.leaves(&mut out);
        out
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        self.panes().contains(&pane)
    }

    /// Every pane's outer rectangle when the workspace fills `area`, in tree order.
    pub fn rects(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        self.root.rects(area, &mut out);
        out
    }

    fn focused_rect(&self, area: Rect) -> Rect {
        self.rects(area)
            .into_iter()
            .find_map(|(id, r)| (id == self.focus).then_some(r))
            .expect("focus is always a pane in the tree")
    }

    pub fn can_split(&self, axis: Axis, area: Rect) -> bool {
        let r = self.focused_rect(area);
        match axis {
            Axis::SideBySide => r.width >= 2 * MIN_WIDTH,
            Axis::Stacked => r.height >= 2 * MIN_HEIGHT,
        }
    }

    /// Splits the focused pane, putting `new` after it and moving focus there.
    pub fn split(&mut self, axis: Axis, new: PaneId) {
        self.root.split(self.focus, axis, new);
        self.focus = new;
    }

    /// Drops `pane`, moving focus to the pane before it if it had focus. Returns true when that
    /// was the last pane, leaving the workspace empty.
    pub fn remove(&mut self, pane: PaneId) -> bool {
        if self.root == Node::Pane(pane) {
            return true;
        }
        let before = self.panes();
        if !self.root.remove(pane) {
            return false;
        }
        if self.focus == pane {
            let i = before.iter().position(|p| *p == pane).unwrap_or(0);
            self.focus = self.panes()[i.saturating_sub(1)];
        }
        false
    }

    pub fn set_focus(&mut self, pane: PaneId) {
        if self.contains(pane) {
            self.focus = pane;
        }
    }

    pub fn focus_next(&mut self) {
        let panes = self.panes();
        let i = panes.iter().position(|p| *p == self.focus).unwrap_or(0);
        self.focus = panes[(i + 1) % panes.len()];
    }

    /// Moves focus to the nearest pane in `dir` that overlaps the focused one across that
    /// direction, preferring the one most in line with its centre.
    pub fn focus_toward(&mut self, dir: Direction, area: Rect) {
        let cur = self.focused_rect(area);
        let centre = |r: Rect| (r.x * 2 + r.width, r.y * 2 + r.height);
        let (cx, cy) = centre(cur);
        let best = self
            .rects(area)
            .into_iter()
            .filter(|(id, _)| *id != self.focus)
            .filter_map(|(id, r)| {
                let overlaps_y = r.y < cur.bottom() && cur.y < r.bottom();
                let overlaps_x = r.x < cur.right() && cur.x < r.right();
                let (x, y) = centre(r);
                let gap = match dir {
                    Direction::Left if overlaps_y && r.right() <= cur.x => cur.x - r.right(),
                    Direction::Right if overlaps_y && r.x >= cur.right() => r.x - cur.right(),
                    Direction::Up if overlaps_x && r.bottom() <= cur.y => cur.y - r.bottom(),
                    Direction::Down if overlaps_x && r.y >= cur.bottom() => r.y - cur.bottom(),
                    _ => return None,
                };
                let off_line = match dir {
                    Direction::Left | Direction::Right => y.abs_diff(cy),
                    Direction::Up | Direction::Down => x.abs_diff(cx),
                };
                Some(((gap, off_line), id))
            })
            .min();
        if let Some((_, id)) = best {
            self.focus = id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect::new(0, 0, 120, 40);

    fn rect_of(ws: &Workspace, pane: PaneId) -> Rect {
        ws.rects(AREA)
            .into_iter()
            .find_map(|(id, r)| (id == pane).then_some(r))
            .unwrap()
    }

    #[test]
    fn single_pane_fills_the_area() {
        let ws = Workspace::new(1);
        assert_eq!(ws.rects(AREA), vec![(1, AREA)]);
    }

    #[test]
    fn repeated_splits_on_one_axis_share_space_equally() {
        let mut ws = Workspace::new(1);
        ws.split(Axis::SideBySide, 2);
        ws.split(Axis::SideBySide, 3);
        assert_eq!(ws.panes(), vec![1, 2, 3]);
        assert_eq!(ws.focus(), 3);
        let widths: Vec<u16> = ws.rects(AREA).iter().map(|(_, r)| r.width).collect();
        assert_eq!(widths, vec![40, 40, 40]);
    }

    #[test]
    fn splitting_across_nests_inside_the_focused_pane() {
        let mut ws = Workspace::new(1);
        ws.split(Axis::SideBySide, 2);
        ws.split(Axis::Stacked, 3);
        assert_eq!(rect_of(&ws, 1), Rect::new(0, 0, 60, 40));
        assert_eq!(rect_of(&ws, 2), Rect::new(60, 0, 60, 20));
        assert_eq!(rect_of(&ws, 3), Rect::new(60, 20, 60, 20));
    }

    #[test]
    fn removing_collapses_splits_and_moves_focus_back() {
        let mut ws = Workspace::new(1);
        ws.split(Axis::SideBySide, 2);
        ws.split(Axis::Stacked, 3);
        assert!(!ws.remove(3));
        assert_eq!(ws.focus(), 2);
        assert_eq!(rect_of(&ws, 2), Rect::new(60, 0, 60, 40));
        assert!(!ws.remove(1));
        assert_eq!(ws.rects(AREA), vec![(2, AREA)]);
        assert!(ws.remove(2), "last pane empties the workspace");
    }

    #[test]
    fn collapsed_split_on_the_same_axis_is_flattened() {
        // [1 | [2 / [3 | 4]]], then removing 2 leaves [1 | 3 | 4] in equal thirds.
        let mut ws = Workspace::new(1);
        ws.split(Axis::SideBySide, 2);
        ws.split(Axis::Stacked, 3);
        ws.split(Axis::SideBySide, 4);
        ws.remove(2);
        let widths: Vec<u16> = ws.rects(AREA).iter().map(|(_, r)| r.width).collect();
        assert_eq!(widths, vec![40, 40, 40]);
    }

    #[test]
    fn focus_moves_by_direction_and_cycles() {
        let mut ws = Workspace::new(1);
        ws.split(Axis::SideBySide, 2);
        ws.split(Axis::Stacked, 3);
        ws.focus_toward(Direction::Up, AREA);
        assert_eq!(ws.focus(), 2);
        ws.focus_toward(Direction::Up, AREA);
        assert_eq!(ws.focus(), 2, "nothing above");
        ws.focus_toward(Direction::Left, AREA);
        assert_eq!(ws.focus(), 1);
        ws.focus_toward(Direction::Right, AREA);
        assert_eq!(
            ws.focus(),
            2,
            "the pane most in line with 1's centre wins the tie"
        );
        ws.focus_next();
        assert_eq!(ws.focus(), 3);
        ws.focus_next();
        assert_eq!(ws.focus(), 1);
    }

    #[test]
    fn tiny_panes_refuse_to_split() {
        let ws = Workspace::new(1);
        assert!(ws.can_split(Axis::SideBySide, AREA));
        assert!(!ws.can_split(Axis::SideBySide, Rect::new(0, 0, 19, 40)));
        assert!(!ws.can_split(Axis::Stacked, Rect::new(0, 0, 120, 7)));
    }
}
