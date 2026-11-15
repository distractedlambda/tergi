//! Laying out the commit graph, one terminal row per commit.
//!
//! We keep a list of lanes. Each active lane is waiting for a particular
//! commit to show up. When a commit arrives, it takes the leftmost lane that
//! waits for it, and any other lanes waiting for it join it on that row. Its
//! first parent continues in the same lane, and other parents either join a
//! lane that already waits for them or get a new lane.
//!
//! Each lane is two cells wide: the lane itself and a gap to its right that
//! horizontal lines pass through.

use gix::ObjectId;

pub const UP: u16 = 1;
pub const DOWN: u16 = 2;
pub const LEFT: u16 = 4;
pub const RIGHT: u16 = 8;
pub const NODE: u16 = 16;

/// Rows wider than this many lanes are cut off. A very wide graph isn't
/// readable anyway, and this keeps memory bounded on huge repos.
pub const MAX_LANES: usize = 48;

pub const LANE_COLORS: u8 = 8;

/// One graph cell: direction bits in the low byte and a color index in the
/// high byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell(pub u16);

impl Cell {
    fn new(bits: u16, color: u8) -> Self {
        Cell(bits | (u16::from(color) << 8))
    }
    pub fn bits(self) -> u16 {
        self.0 & 0xff
    }
    pub fn color(self) -> u8 {
        (self.0 >> 8) as u8
    }
    pub fn is_empty(self) -> bool {
        self.bits() == 0
    }

    pub fn glyph(self) -> char {
        let b = self.bits();
        if b & NODE != 0 {
            return '●';
        }
        match b & 0xf {
            0 => ' ',
            x if x == UP | DOWN => '│',
            x if x == UP | DOWN | LEFT | RIGHT => '┼',
            x if x == UP | DOWN | LEFT => '┤',
            x if x == UP | DOWN | RIGHT => '├',
            x if x == UP | LEFT => '╯',
            x if x == UP | RIGHT => '╰',
            x if x == DOWN | LEFT => '╮',
            x if x == DOWN | RIGHT => '╭',
            x if x == UP | LEFT | RIGHT => '┴',
            x if x == DOWN | LEFT | RIGHT => '┬',
            x if x == UP || x == DOWN => '│',
            _ => '─',
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphRow {
    /// Two cells per lane: the lane, then the gap after it.
    pub cells: Box<[Cell]>,
    pub node_lane: u16,
}

impl GraphRow {
    pub fn render(&self) -> String {
        let mut s: String = self.cells.iter().map(|c| c.glyph()).collect();
        let trimmed = s.trim_end().len();
        s.truncate(trimmed);
        s
    }
}

#[derive(Debug, Clone, Copy)]
struct Lane {
    id: ObjectId,
    color: u8,
}

#[derive(Default)]
pub struct GraphBuilder {
    lanes: Vec<Option<Lane>>,
    next_color: u8,
}

impl GraphBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    fn take_color(&mut self) -> u8 {
        let c = self.next_color;
        self.next_color = (self.next_color + 1) % LANE_COLORS;
        c
    }

    pub fn push(&mut self, id: ObjectId, parents: &[ObjectId]) -> GraphRow {
        let before: Vec<bool> = self.lanes.iter().map(Option::is_some).collect();
        let waiting: Vec<usize> = self
            .lanes
            .iter()
            .enumerate()
            .filter(|(_, l)| l.is_some_and(|l| l.id == id))
            .map(|(i, _)| i)
            .collect();

        let (node, node_color) = match waiting.first() {
            Some(&c) => (c, self.lanes[c].map_or(0, |l| l.color)),
            None => {
                let slot = self.lanes.iter().position(Option::is_none);
                let c = slot.unwrap_or_else(|| {
                    self.lanes.push(None);
                    self.lanes.len() - 1
                });
                (c, self.take_color())
            }
        };

        // Spans are horizontal connections: (from, to, color).
        let mut spans: Vec<(usize, usize, u8)> = Vec::new();
        let mut freed: Vec<usize> = Vec::new();
        for &i in waiting.iter().skip(1) {
            let color = self.lanes[i].map_or(node_color, |l| l.color);
            spans.push((node, i, color));
            self.lanes[i] = None;
            freed.push(i);
        }

        self.lanes[node] = parents.first().map(|&p| Lane {
            id: p,
            color: node_color,
        });

        let mut new_lanes: Vec<usize> = Vec::new();
        for (k, &p) in parents.iter().enumerate().skip(1) {
            if parents[..k].contains(&p) {
                continue;
            }
            let existing = self
                .lanes
                .iter()
                .enumerate()
                .position(|(j, l)| j != node && l.is_some_and(|l| l.id == p));
            if let Some(j) = existing {
                let color = self.lanes[j].map_or(node_color, |l| l.color);
                spans.push((node, j, color));
            } else {
                let slot = self
                    .lanes
                    .iter()
                    .enumerate()
                    .position(|(j, l)| l.is_none() && j != node && !freed.contains(&j));
                let j = slot.unwrap_or_else(|| {
                    self.lanes.push(None);
                    self.lanes.len() - 1
                });
                let color = self.take_color();
                self.lanes[j] = Some(Lane { id: p, color });
                new_lanes.push(j);
                spans.push((node, j, color));
            }
        }

        let width = self.lanes.len().max(before.len());
        let mut bits = vec![0u16; width * 2];
        let mut colors = vec![0u8; width * 2];
        for i in 0..width {
            let up = before.get(i).copied().unwrap_or(false);
            let down = self.lanes.get(i).is_some_and(Option::is_some);
            let mut b = 0;
            if up && (i != node || !waiting.is_empty()) {
                b |= UP;
            }
            if down {
                b |= DOWN;
            }
            bits[i * 2] = b;
            // Converging lanes keep the color they had above; everything
            // else uses the lane's current color.
            colors[i * 2] = self.lanes.get(i).copied().flatten().map_or_else(
                || waiting_color(&spans, i).unwrap_or(node_color),
                |l| l.color,
            );
        }
        for &(from, to, color) in &spans {
            let (lo, hi) = if from < to { (from, to) } else { (to, from) };
            for cell in lo * 2 + 1..hi * 2 {
                if cell % 2 == 1 {
                    bits[cell] |= LEFT | RIGHT;
                    colors[cell] = color;
                } else {
                    let had_vertical = bits[cell] & (UP | DOWN) != 0;
                    bits[cell] |= LEFT | RIGHT;
                    if !had_vertical {
                        colors[cell] = color;
                    }
                }
            }
            let end = to * 2;
            bits[end] |= if to > from { LEFT } else { RIGHT };
            if !new_lanes.contains(&to) && bits[end] & (UP | DOWN) == UP | DOWN {
                // Joining a lane that keeps going: keep that lane's color.
            } else {
                colors[end] = color;
            }
        }
        bits[node * 2] |= NODE;
        colors[node * 2] = node_color;

        while self.lanes.last().is_some_and(Option::is_none) {
            self.lanes.pop();
        }

        let keep = (MAX_LANES * 2).min(bits.len());
        let mut cells: Vec<Cell> = bits[..keep]
            .iter()
            .zip(&colors[..keep])
            .map(|(&b, &c)| Cell::new(b, c))
            .collect();
        while cells.last().is_some_and(|c| c.is_empty()) {
            cells.pop();
        }
        GraphRow {
            cells: cells.into_boxed_slice(),
            node_lane: u16::try_from(node).unwrap_or(u16::MAX),
        }
    }
}

fn waiting_color(spans: &[(usize, usize, u8)], lane: usize) -> Option<u8> {
    spans.iter().find(|s| s.1 == lane).map(|s| s.2)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use proptest::prelude::*;

    use super::*;

    fn id(n: u8) -> ObjectId {
        let mut b = [0u8; 20];
        b[19] = n;
        ObjectId::from_bytes_or_panic(&b)
    }

    fn render(commits: &[(u8, &[u8])]) -> String {
        let mut g = GraphBuilder::new();
        commits
            .iter()
            .map(|(c, ps)| {
                let parents: Vec<ObjectId> = ps.iter().map(|&p| id(p)).collect();
                format!("{} {}", g.push(id(*c), &parents).render(), c)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn linear() {
        assert_eq!(render(&[(3, &[2]), (2, &[1]), (1, &[])]), "● 3\n● 2\n● 1");
    }

    #[test]
    fn branch_and_merge() {
        // 5 merges 4 (main) and 3 (feature); both come from 2.
        let out = render(&[(5, &[4, 3]), (3, &[2]), (4, &[2]), (2, &[1]), (1, &[])]);
        insta::assert_snapshot!(out, @r"
        ●─╮ 5
        │ ● 3
        ● │ 4
        ●─╯ 2
        ● 1
        ");
    }

    #[test]
    fn two_tips() {
        // Two branch tips 4 and 3 that fork from 2.
        let out = render(&[(4, &[2]), (3, &[2]), (2, &[1]), (1, &[])]);
        insta::assert_snapshot!(out, @r"
        ● 4
        │ ● 3
        ●─╯ 2
        ● 1
        ");
    }

    #[test]
    fn octopus() {
        let out = render(&[(9, &[8, 7, 6]), (6, &[1]), (7, &[1]), (8, &[1]), (1, &[])]);
        insta::assert_snapshot!(out, @r"
        ●─┬─╮ 9
        │ │ ● 6
        │ ● │ 7
        ● │ │ 8
        ●─┴─╯ 1
        ");
    }

    #[test]
    fn merge_into_existing_lane() {
        // 6 and 5 are tips. 6 merges 4, which 5 also has as its parent.
        let out = render(&[(6, &[3, 4]), (5, &[4]), (4, &[2]), (3, &[2]), (2, &[])]);
        insta::assert_snapshot!(out, @r"
        ●─╮ 6
        │ │ ● 5
        │ ●─╯ 4
        ● │ 3
        ●─╯ 2
        ");
    }

    /// A history in the order git lists it, newest first. Commit `i` has
    /// parents `i + offset`. Parents past the end are never listed, as in a
    /// shallow clone or a walk that stops early.
    fn history() -> impl Strategy<Value = Vec<Vec<usize>>> {
        prop::collection::vec(prop::collection::vec(1usize..6, 0..4), 1..16).prop_map(|commits| {
            commits
                .into_iter()
                .enumerate()
                .map(|(i, offsets)| offsets.into_iter().map(|o| i + o).collect())
                .collect()
        })
    }

    fn cell(row: &GraphRow, i: usize) -> Cell {
        row.cells.get(i).copied().unwrap_or_default()
    }

    proptest! {
        #[test]
        fn lines_connect(commits in history()) {
            let mut g = GraphBuilder::new();
            let rows: Vec<GraphRow> = commits
                .iter()
                .enumerate()
                .map(|(i, ps)| {
                    let parents: Vec<ObjectId> = ps.iter().map(|&p| id(p as u8)).collect();
                    g.push(id(i as u8), &parents)
                })
                .collect();
            let shown: Vec<String> = rows.iter().map(GraphRow::render).collect();

            let mut pending: HashSet<usize> = HashSet::new();
            for (i, row) in rows.iter().enumerate() {
                prop_assert!(row.cells.len() <= MAX_LANES * 2);
                let node = usize::from(row.node_lane) * 2;
                let nodes: Vec<usize> = (0..row.cells.len()).filter(|&c| cell(row, c).bits() & NODE != 0).collect();
                prop_assert_eq!(nodes, vec![node], "{:#?}", shown);

                // The node has a line above when a newer commit has it as a
                // parent, and a line below when it has parents.
                let bits = cell(row, node).bits();
                prop_assert_eq!(bits & UP != 0, pending.remove(&i), "row {} of {:#?}", i, shown);
                prop_assert_eq!(bits & DOWN != 0, !commits[i].is_empty(), "row {} of {:#?}", i, shown);
                pending.extend(commits[i].iter().copied());

                // Horizontal lines join up. The node itself has no
                // horizontal bits.
                for c in 0..row.cells.len() {
                    let b = cell(row, c).bits();
                    let joins = |other: Cell, side: u16| other.bits() & (side | NODE) != 0;
                    if b & RIGHT != 0 {
                        prop_assert!(joins(cell(row, c + 1), LEFT), "row {} cell {} of {:#?}", i, c, shown);
                    }
                    if b & LEFT != 0 {
                        prop_assert!(c > 0 && joins(cell(row, c - 1), RIGHT), "row {} cell {} of {:#?}", i, c, shown);
                    }
                }

                // Every lane going down continues in the next row, in the
                // same color.
                let down: Vec<usize> = (0..row.cells.len()).step_by(2).filter(|&c| cell(row, c).bits() & DOWN != 0).collect();
                if let Some(next) = rows.get(i + 1) {
                    let up: Vec<usize> = (0..next.cells.len()).step_by(2).filter(|&c| cell(next, c).bits() & UP != 0).collect();
                    prop_assert_eq!(&down, &up, "rows {} and {} of {:#?}", i, i + 1, shown);
                    for &c in &down {
                        prop_assert_eq!(cell(row, c).color(), cell(next, c).color(), "lane {} in row {} of {:#?}", c / 2, i, shown);
                    }
                }
                // Each commit we still wait for has a lane, and no lane
                // waits for anything else.
                prop_assert!(down.len() >= pending.len(), "row {} of {:#?}", i, shown);
                prop_assert_eq!(down.is_empty(), pending.is_empty(), "row {} of {:#?}", i, shown);
            }
        }
    }
}
