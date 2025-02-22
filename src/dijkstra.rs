use std::cmp::{min, Ordering, Reverse};
use std::collections::BinaryHeap;
use std::hash::{Hash, Hasher};

use crate::lines::LineNumber;
use crate::syntax::{ChangeKind, Syntax};
use rustc_hash::FxHashMap;
use strsim::normalized_levenshtein;
use Edge::*;

#[derive(Debug, Clone)]
struct Vertex<'a> {
    lhs_syntax: Option<&'a Syntax<'a>>,
    lhs_prev_novel: Option<LineNumber>,
    rhs_syntax: Option<&'a Syntax<'a>>,
    rhs_prev_novel: Option<LineNumber>,
}
impl<'a> PartialEq for Vertex<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.lhs_syntax.map(|node| node.id()) == other.lhs_syntax.map(|node| node.id())
            && self.rhs_syntax.map(|node| node.id()) == other.rhs_syntax.map(|node| node.id())
    }
}
impl<'a> Eq for Vertex<'a> {}

impl<'a> Hash for Vertex<'a> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.lhs_syntax.map(|node| node.id()).hash(state);
        self.rhs_syntax.map(|node| node.id()).hash(state);
    }
}

impl<'a> Vertex<'a> {
    fn is_end(&self) -> bool {
        self.lhs_syntax.is_none() && self.rhs_syntax.is_none()
    }
}

// Rust requires that PartialEq, PartialOrd and Ord agree.
// https://doc.rust-lang.org/std/cmp/trait.Ord.html
//
// For `Vertex`, we want to compare by distance in a priority queue, but
// equality should only consider LHS/RHS node when deciding if we've
// visited a vertex. We define separate wrappers for these two use
// cases.
#[derive(Debug)]
struct OrdVertex<'a> {
    distance: u64,
    prev: Option<(Vertex<'a>, Edge)>,
    v: Vertex<'a>,
}

impl<'a> PartialOrd for OrdVertex<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<'a> Ord for OrdVertex<'a> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance.cmp(&other.distance)
    }
}

impl<'a> PartialEq for OrdVertex<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.distance == other.distance
    }
}
impl<'a> Eq for OrdVertex<'a> {}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
enum Edge {
    UnchangedNode(u64),
    UnchangedDelimiter(u64),
    ReplacedComment,
    NovelAtomLHS { contiguous: bool },
    NovelAtomRHS { contiguous: bool },
    NovelDelimiterLHS { contiguous: bool },
    NovelDelimiterRHS { contiguous: bool },
    NovelTreeLHS { num_descendants: u64 },
    NovelTreeRHS { num_descendants: u64 },
}

impl Edge {
    fn cost(&self) -> u64 {
        match self {
            // Matching nodes is always best.
            UnchangedNode(depth_difference) => min(40, *depth_difference),
            // Matching an outer delimiter is good.
            UnchangedDelimiter(depth_difference) => 100 + min(40, *depth_difference),

            // Replacing a comment is better than treating it as novel.
            ReplacedComment => 150,

            // Otherwise, we've added/removed a node.
            NovelAtomLHS { contiguous } | NovelAtomRHS { contiguous } => {
                if *contiguous {
                    200
                } else {
                    201
                }
            }
            NovelDelimiterLHS { contiguous } | NovelDelimiterRHS { contiguous } => {
                if *contiguous {
                    200
                } else {
                    201
                }
            }

            // For large trees, it's better to mark the whole tree as
            // novel rather than marking 90% of the children as
            // novel. This stops us matching up completely unrelated trees.
            NovelTreeLHS { num_descendants } | NovelTreeRHS { num_descendants } => {
                200 + (*num_descendants - 10) * NovelDelimiterLHS { contiguous: false }.cost()
            }
        }
    }
}

fn shortest_path(start: Vertex) -> Vec<(Edge, Vertex)> {
    // We want to visit nodes with the shortest distance first, but
    // BinaryHeap is a max-heap. Ensure nodes are wrapped with Reverse
    // to flip comparisons.
    let mut heap: BinaryHeap<Reverse<_>> = BinaryHeap::new();

    heap.push(Reverse(OrdVertex {
        distance: 0,
        prev: None,
        v: start.clone(),
    }));

    // TODO: this grows very big. Consider using IDA* to reduce memory
    // usage.
    let mut predecessors: FxHashMap<Vertex, Option<(Vertex, Edge)>> = FxHashMap::default();

    let end;
    loop {
        match heap.pop() {
            Some(Reverse(OrdVertex { distance, prev, v })) => {
                if predecessors.contains_key(&v) {
                    continue;
                }
                predecessors.insert(v.clone(), prev);

                if v.is_end() {
                    end = v;
                    break;
                }

                for (edge, new_v) in neighbours(&v) {
                    if predecessors.contains_key(&new_v) {
                        continue;
                    }
                    let new_v_distance = distance + edge.cost();

                    heap.push(Reverse(OrdVertex {
                        distance: new_v_distance,
                        prev: Some((v.clone(), edge)),
                        v: new_v,
                    }));
                }
            }
            None => panic!("Ran out of graph nodes before reaching end"),
        }
    }

    let mut current = end;
    let mut res: Vec<(Edge, Vertex)> = vec![];
    while let Some(Some((node, edge))) = predecessors.remove(&current) {
        res.push((edge, node.clone()));
        current = node;
    }

    res.reverse();
    res
}

const NOVEL_TREE_THRESHOLD: u64 = 20;

fn neighbours<'a>(v: &Vertex<'a>) -> Vec<(Edge, Vertex<'a>)> {
    let mut res = vec![];

    if let (Some(lhs_syntax), Some(rhs_syntax)) = (&v.lhs_syntax, &v.rhs_syntax) {
        if lhs_syntax.equal_content(rhs_syntax) {
            let depth_difference = (lhs_syntax.info().num_ancestors.get() as i64
                - rhs_syntax.info().num_ancestors.get() as i64)
                .abs() as u64;

            // Both nodes are equal, the happy case.
            res.push((
                UnchangedNode(depth_difference),
                Vertex {
                    lhs_syntax: lhs_syntax.next(),
                    rhs_syntax: rhs_syntax.next(),
                    lhs_prev_novel: None,
                    rhs_prev_novel: None,
                },
            ));
        }

        if let (
            Syntax::List {
                open_content: lhs_open_content,
                close_content: lhs_close_content,
                children: lhs_children,
                ..
            },
            Syntax::List {
                open_content: rhs_open_content,
                close_content: rhs_close_content,
                children: rhs_children,
                ..
            },
        ) = (lhs_syntax, rhs_syntax)
        {
            // The list delimiters are equal, but children may not be.
            if lhs_open_content == rhs_open_content && lhs_close_content == rhs_close_content {
                let lhs_next = if lhs_children.is_empty() {
                    lhs_syntax.next()
                } else {
                    Some(lhs_children[0])
                };
                let rhs_next = if rhs_children.is_empty() {
                    rhs_syntax.next()
                } else {
                    Some(rhs_children[0])
                };

                let depth_difference = (lhs_syntax.info().num_ancestors.get() as i64
                    - rhs_syntax.info().num_ancestors.get() as i64)
                    .abs() as u64;

                res.push((
                    UnchangedDelimiter(depth_difference),
                    Vertex {
                        lhs_syntax: lhs_next,
                        lhs_prev_novel: None,
                        rhs_syntax: rhs_next,
                        rhs_prev_novel: None,
                    },
                ));
            }
        }

        if let (
            Syntax::Atom {
                content: lhs_content,
                is_comment: lhs_is_comment,
                ..
            },
            Syntax::Atom {
                content: rhs_content,
                is_comment: rhs_is_comment,
                ..
            },
        ) = (lhs_syntax, rhs_syntax)
        {
            // Both sides are comments and their content is reasonably
            // similar.
            if *lhs_is_comment
                && *rhs_is_comment
                && normalized_levenshtein(lhs_content, rhs_content) > 0.4
            {
                res.push((
                    ReplacedComment,
                    Vertex {
                        lhs_syntax: lhs_syntax.next(),
                        lhs_prev_novel: None,
                        rhs_syntax: rhs_syntax.next(),
                        rhs_prev_novel: None,
                    },
                ));
            }
        }
    }

    if let Some(lhs_syntax) = &v.lhs_syntax {
        match lhs_syntax {
            // Step over this novel atom.
            Syntax::Atom { .. } => {
                res.push((
                    NovelAtomLHS {
                        contiguous: v.lhs_prev_novel == lhs_syntax.first_line(),
                    },
                    Vertex {
                        lhs_syntax: lhs_syntax.next(),
                        lhs_prev_novel: lhs_syntax.last_line(),
                        rhs_syntax: v.rhs_syntax,
                        rhs_prev_novel: v.rhs_prev_novel,
                    },
                ));
            }
            // Step into this partially/fully novel list.
            Syntax::List {
                open_position,
                children,
                num_descendants,
                ..
            } => {
                let lhs_next = if children.is_empty() {
                    lhs_syntax.next()
                } else {
                    // `lhs_prev_novel` only tracks nodes at the same level.
                    Some(children[0])
                };

                res.push((
                    NovelDelimiterLHS {
                        contiguous: v.lhs_prev_novel == lhs_syntax.first_line(),
                    },
                    Vertex {
                        lhs_syntax: lhs_next,
                        lhs_prev_novel: open_position.last().map(|lp| lp.line),
                        rhs_syntax: v.rhs_syntax,
                        rhs_prev_novel: v.rhs_prev_novel,
                    },
                ));

                if *num_descendants > NOVEL_TREE_THRESHOLD {
                    res.push((
                        NovelTreeLHS {
                            num_descendants: *num_descendants as u64,
                        },
                        Vertex {
                            lhs_syntax: lhs_syntax.next(),
                            lhs_prev_novel: v.lhs_prev_novel,
                            rhs_syntax: v.rhs_syntax,
                            rhs_prev_novel: v.rhs_prev_novel,
                        },
                    ));
                }
            }
        }
    }

    if let Some(rhs_syntax) = &v.rhs_syntax {
        match rhs_syntax {
            // Step over this novel atom.
            Syntax::Atom { .. } => {
                res.push((
                    NovelAtomRHS {
                        contiguous: v.rhs_prev_novel == rhs_syntax.first_line(),
                    },
                    Vertex {
                        lhs_syntax: v.lhs_syntax,
                        lhs_prev_novel: v.lhs_prev_novel,
                        rhs_syntax: rhs_syntax.next(),
                        rhs_prev_novel: rhs_syntax.last_line(),
                    },
                ));
            }
            // Step into this partially/fully novel list.
            Syntax::List {
                open_position,
                children,
                num_descendants,
                ..
            } => {
                let rhs_next = if children.is_empty() {
                    rhs_syntax.next()
                } else {
                    Some(children[0])
                };

                res.push((
                    NovelDelimiterRHS {
                        contiguous: v.rhs_prev_novel == rhs_syntax.first_line(),
                    },
                    Vertex {
                        lhs_syntax: v.lhs_syntax,
                        lhs_prev_novel: v.lhs_prev_novel,
                        rhs_syntax: rhs_next,
                        rhs_prev_novel: open_position.last().map(|lp| lp.line),
                    },
                ));

                if *num_descendants > NOVEL_TREE_THRESHOLD {
                    res.push((
                        NovelTreeRHS {
                            num_descendants: *num_descendants as u64,
                        },
                        Vertex {
                            lhs_syntax: v.lhs_syntax,
                            lhs_prev_novel: v.lhs_prev_novel,
                            rhs_syntax: rhs_syntax.next(),
                            rhs_prev_novel: v.rhs_prev_novel,
                        },
                    ));
                }
            }
        }
    }

    res
}

pub fn mark_syntax<'a>(lhs_syntax: Option<&'a Syntax<'a>>, rhs_syntax: Option<&'a Syntax<'a>>) {
    let start = Vertex {
        lhs_syntax,
        lhs_prev_novel: None,
        rhs_syntax,
        rhs_prev_novel: None,
    };
    let route = shortest_path(start);
    mark_route(&route);
}

fn mark_route(route: &[(Edge, Vertex)]) {
    for (e, v) in route {
        match e {
            UnchangedNode(_) => {
                // No change on this node or its children.
                let lhs = v.lhs_syntax.unwrap();
                let rhs = v.rhs_syntax.unwrap();
                lhs.set_change_deep(ChangeKind::Unchanged(rhs));
                rhs.set_change_deep(ChangeKind::Unchanged(lhs));
            }
            UnchangedDelimiter(_) => {
                // No change on the outer delimiter, but children may
                // have changed.
                let lhs = v.lhs_syntax.unwrap();
                let rhs = v.rhs_syntax.unwrap();
                lhs.set_change(ChangeKind::Unchanged(rhs));
                rhs.set_change(ChangeKind::Unchanged(lhs));
            }
            ReplacedComment => {
                let lhs = v.lhs_syntax.unwrap();
                let rhs = v.rhs_syntax.unwrap();
                lhs.set_change(ChangeKind::ReplacedComment(lhs, rhs));
                rhs.set_change(ChangeKind::ReplacedComment(rhs, lhs));
            }
            NovelAtomLHS { .. } | NovelDelimiterLHS { .. } => {
                let lhs = v.lhs_syntax.unwrap();
                lhs.set_change(ChangeKind::Novel);
            }
            NovelAtomRHS { .. } | NovelDelimiterRHS { .. } => {
                let rhs = v.rhs_syntax.unwrap();
                rhs.set_change(ChangeKind::Novel);
            }
            NovelTreeLHS { .. } => {
                let lhs = v.lhs_syntax.unwrap();
                lhs.set_change_deep(ChangeKind::Novel);
            }
            NovelTreeRHS { .. } => {
                let rhs = v.rhs_syntax.unwrap();
                rhs.set_change_deep(ChangeKind::Novel);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::positions::SingleLineSpan;
    use crate::syntax::init_info;
    use crate::syntax::Syntax::*;
    use crate::syntax::SyntaxInfo;

    use itertools::Itertools;
    use std::cell::Cell;
    use typed_arena::Arena;

    fn pos_helper(line: usize) -> Vec<SingleLineSpan> {
        vec![SingleLineSpan {
            line: line.into(),
            start_col: 0,
            end_col: 1,
        }]
    }

    fn col_helper(line: usize, col: usize) -> Vec<SingleLineSpan> {
        vec![SingleLineSpan {
            line: line.into(),
            start_col: col,
            end_col: col + 1,
        }]
    }

    #[test]
    fn identical_atoms() {
        let arena = Arena::new();

        let lhs = arena.alloc(Atom {
            info: SyntaxInfo {
                unique_id: Cell::new(0),
                ..SyntaxInfo::new(0)
            },
            position: pos_helper(0),
            content: "foo".into(),
            is_comment: false,
        });

        // Same content as LHS.
        let rhs = arena.alloc(Atom {
            info: SyntaxInfo {
                unique_id: Cell::new(1),
                ..SyntaxInfo::new(1)
            },
            position: pos_helper(1),
            content: "foo".into(),
            is_comment: false,
        });

        let start = Vertex {
            lhs_syntax: Some(lhs),
            lhs_prev_novel: None,
            rhs_syntax: Some(rhs),
            rhs_prev_novel: None,
        };
        let route = shortest_path(start);

        let actions = route.iter().map(|(action, _)| *action).collect_vec();
        assert_eq!(actions, vec![UnchangedNode(0)]);
    }

    #[test]
    fn extra_atom_lhs() {
        let arena = Arena::new();

        let lhs: Vec<&Syntax> = vec![Syntax::new_list(
            &arena,
            "[".into(),
            pos_helper(0),
            vec![Syntax::new_atom(&arena, pos_helper(1), "foo")],
            "]".into(),
            pos_helper(2),
        )];
        init_info(&lhs);

        let rhs: Vec<&Syntax> = vec![Syntax::new_list(
            &arena,
            "[".into(),
            pos_helper(0),
            vec![],
            "]".into(),
            pos_helper(2),
        )];
        init_info(&rhs);

        let start = Vertex {
            lhs_syntax: lhs.get(0).map(|n| *n),
            lhs_prev_novel: None,
            rhs_syntax: rhs.get(0).map(|n| *n),
            rhs_prev_novel: None,
        };
        let route = shortest_path(start);

        let actions = route.iter().map(|(action, _)| *action).collect_vec();
        assert_eq!(
            actions,
            vec![UnchangedDelimiter(0), NovelAtomLHS { contiguous: false }]
        );
    }

    #[test]
    fn repeated_atoms() {
        let arena = Arena::new();

        let lhs: Vec<&Syntax> = vec![Syntax::new_list(
            &arena,
            "[".into(),
            pos_helper(0),
            vec![],
            "]".into(),
            pos_helper(2),
        )];
        init_info(&lhs);

        let rhs: Vec<&Syntax> = vec![Syntax::new_list(
            &arena,
            "[".into(),
            pos_helper(0),
            vec![
                Syntax::new_atom(&arena, pos_helper(1), "foo"),
                Syntax::new_atom(&arena, pos_helper(2), "foo"),
            ],
            "]".into(),
            pos_helper(3),
        )];
        init_info(&rhs);

        let start = Vertex {
            lhs_syntax: lhs.get(0).map(|n| *n),
            lhs_prev_novel: None,
            rhs_syntax: rhs.get(0).map(|n| *n),
            rhs_prev_novel: None,
        };
        let route = shortest_path(start);

        let actions = route.iter().map(|(action, _)| *action).collect_vec();
        assert_eq!(
            actions,
            vec![
                UnchangedDelimiter(0),
                NovelAtomRHS { contiguous: false },
                NovelAtomRHS { contiguous: false }
            ]
        );
    }

    #[test]
    fn atom_after_empty_list() {
        let arena = Arena::new();

        let lhs: Vec<&Syntax> = vec![Syntax::new_list(
            &arena,
            "[".into(),
            pos_helper(0),
            vec![
                Syntax::new_list(
                    &arena,
                    "(".into(),
                    pos_helper(1),
                    vec![],
                    ")".into(),
                    pos_helper(2),
                ),
                Syntax::new_atom(&arena, pos_helper(3), "foo"),
            ],
            "]".into(),
            pos_helper(4),
        )];
        init_info(&lhs);

        let rhs: Vec<&Syntax> = vec![Syntax::new_list(
            &arena,
            "{".into(),
            pos_helper(0),
            vec![
                Syntax::new_list(
                    &arena,
                    "(".into(),
                    pos_helper(1),
                    vec![],
                    ")".into(),
                    pos_helper(2),
                ),
                Syntax::new_atom(&arena, pos_helper(3), "foo"),
            ],
            "}".into(),
            pos_helper(4),
        )];
        init_info(&rhs);

        let start = Vertex {
            lhs_syntax: lhs.get(0).map(|n| *n),
            lhs_prev_novel: None,
            rhs_syntax: rhs.get(0).map(|n| *n),
            rhs_prev_novel: None,
        };
        let route = shortest_path(start);

        let actions = route.iter().map(|(action, _)| *action).collect_vec();
        assert_eq!(
            actions,
            vec![
                NovelDelimiterRHS { contiguous: false },
                NovelDelimiterLHS { contiguous: false },
                UnchangedNode(0),
                UnchangedNode(0)
            ],
        );
    }

    #[test]
    fn prefer_atoms_same_line() {
        let arena = Arena::new();

        let lhs: Vec<&Syntax> = vec![
            Syntax::new_atom(&arena, col_helper(1, 0), "foo"),
            Syntax::new_atom(&arena, col_helper(2, 0), "bar"),
            Syntax::new_atom(&arena, col_helper(2, 1), "foo"),
        ];
        init_info(&lhs);

        let rhs: Vec<&Syntax> = vec![Syntax::new_atom(&arena, col_helper(1, 0), "foo")];
        init_info(&rhs);

        let start = Vertex {
            lhs_syntax: lhs.get(0).map(|n| *n),
            lhs_prev_novel: None,
            rhs_syntax: rhs.get(0).map(|n| *n),
            rhs_prev_novel: None,
        };
        let route = shortest_path(start);

        let actions = route.iter().map(|(action, _)| *action).collect_vec();
        assert_eq!(
            actions,
            vec![
                UnchangedNode(0),
                NovelAtomLHS { contiguous: false },
                NovelAtomLHS { contiguous: true },
            ]
        );
    }

    #[test]
    fn prefer_children_same_line() {
        let arena = Arena::new();

        let lhs: Vec<&Syntax> = vec![Syntax::new_list(
            &arena,
            "[".into(),
            col_helper(1, 0),
            vec![Syntax::new_atom(&arena, col_helper(1, 2), "1")],
            "]".into(),
            pos_helper(2),
        )];
        init_info(&lhs);

        let rhs: Vec<&Syntax> = vec![];

        let start = Vertex {
            lhs_syntax: lhs.get(0).map(|n| *n),
            lhs_prev_novel: None,
            rhs_syntax: rhs.get(0).map(|n| *n),
            rhs_prev_novel: None,
        };
        let route = shortest_path(start);

        let actions = route.iter().map(|(action, _)| *action).collect_vec();
        assert_eq!(
            actions,
            vec![
                NovelDelimiterLHS { contiguous: false },
                NovelAtomLHS { contiguous: true },
            ]
        );
    }

    #[test]
    fn test_novel_tree() {
        let arena = Arena::new();

        let lhs: Vec<&Syntax> = vec![Syntax::new_list(
            &arena,
            "[".into(),
            pos_helper(0),
            vec![
                Syntax::new_atom(&arena, pos_helper(1), "1"),
                Syntax::new_atom(&arena, pos_helper(2), "2"),
                Syntax::new_atom(&arena, pos_helper(3), "3"),
                Syntax::new_atom(&arena, pos_helper(4), "4"),
                Syntax::new_atom(&arena, pos_helper(5), "5"),
                Syntax::new_atom(&arena, pos_helper(6), "6"),
                Syntax::new_atom(&arena, pos_helper(7), "7"),
                Syntax::new_atom(&arena, pos_helper(8), "8"),
                Syntax::new_atom(&arena, pos_helper(9), "9"),
                Syntax::new_atom(&arena, pos_helper(10), "10"),
                Syntax::new_atom(&arena, pos_helper(11), "11"),
                Syntax::new_atom(&arena, pos_helper(12), "12"),
                Syntax::new_atom(&arena, pos_helper(13), "13"),
                Syntax::new_atom(&arena, pos_helper(14), "14"),
                Syntax::new_atom(&arena, pos_helper(15), "15"),
                Syntax::new_atom(&arena, pos_helper(16), "16"),
                Syntax::new_atom(&arena, pos_helper(17), "17"),
                Syntax::new_atom(&arena, pos_helper(18), "18"),
                Syntax::new_atom(&arena, pos_helper(19), "19"),
                Syntax::new_atom(&arena, pos_helper(20), "20"),
                Syntax::new_atom(&arena, pos_helper(21), "21"),
            ],
            "]".into(),
            pos_helper(100),
        )];
        init_info(&lhs);

        let rhs: Vec<&Syntax> = vec![Syntax::new_list(
            &arena,
            "[".into(),
            pos_helper(0),
            vec![
                Syntax::new_atom(&arena, pos_helper(1), "d1"),
                Syntax::new_atom(&arena, pos_helper(2), "d2"),
                Syntax::new_atom(&arena, pos_helper(3), "d3"),
                Syntax::new_atom(&arena, pos_helper(4), "d4"),
                Syntax::new_atom(&arena, pos_helper(5), "d5"),
                Syntax::new_atom(&arena, pos_helper(6), "d6"),
                Syntax::new_atom(&arena, pos_helper(7), "d7"),
                Syntax::new_atom(&arena, pos_helper(8), "d8"),
                Syntax::new_atom(&arena, pos_helper(9), "d9"),
                // This is the only common atom:
                Syntax::new_atom(&arena, pos_helper(10), "10"),
                Syntax::new_atom(&arena, pos_helper(11), "d11"),
                Syntax::new_atom(&arena, pos_helper(12), "d12"),
                Syntax::new_atom(&arena, pos_helper(13), "d13"),
                Syntax::new_atom(&arena, pos_helper(14), "d14"),
                Syntax::new_atom(&arena, pos_helper(15), "d15"),
                Syntax::new_atom(&arena, pos_helper(16), "d16"),
                Syntax::new_atom(&arena, pos_helper(17), "d17"),
                Syntax::new_atom(&arena, pos_helper(18), "d18"),
                Syntax::new_atom(&arena, pos_helper(19), "d19"),
                Syntax::new_atom(&arena, pos_helper(20), "d20"),
                Syntax::new_atom(&arena, pos_helper(21), "d21"),
            ],
            "]".into(),
            pos_helper(100),
        )];
        init_info(&rhs);

        let start = Vertex {
            lhs_syntax: lhs.get(0).map(|n| *n),
            lhs_prev_novel: None,
            rhs_syntax: rhs.get(0).map(|n| *n),
            rhs_prev_novel: None,
        };
        let route = shortest_path(start);

        let actions = route.iter().map(|(action, _)| *action).collect_vec();
        assert_eq!(
            actions,
            vec![
                NovelTreeLHS {
                    num_descendants: 21
                },
                NovelTreeRHS {
                    num_descendants: 21
                },
            ]
        );
    }
    #[test]
    fn replace_similar_comment() {
        let arena = Arena::new();

        let lhs: Vec<&Syntax> = vec![Syntax::new_comment(
            &arena,
            pos_helper(1),
            "the quick brown fox",
        )];
        init_info(&lhs);

        let rhs: Vec<&Syntax> = vec![Syntax::new_comment(
            &arena,
            pos_helper(1),
            "the quick brown cat",
        )];
        init_info(&rhs);

        let start = Vertex {
            lhs_syntax: lhs.get(0).map(|n| *n),
            lhs_prev_novel: None,
            rhs_syntax: rhs.get(0).map(|n| *n),
            rhs_prev_novel: None,
        };
        let route = shortest_path(start);

        let actions = route.iter().map(|(action, _)| *action).collect_vec();
        assert_eq!(actions, vec![ReplacedComment]);
    }

    #[test]
    fn replace_very_different_comment() {
        let arena = Arena::new();

        let lhs: Vec<&Syntax> = vec![Syntax::new_comment(
            &arena,
            pos_helper(1),
            "the quick brown fox",
        )];
        init_info(&lhs);

        let rhs: Vec<&Syntax> = vec![Syntax::new_comment(&arena, pos_helper(1), "foo bar")];
        init_info(&rhs);

        let start = Vertex {
            lhs_syntax: lhs.get(0).map(|n| *n),
            lhs_prev_novel: None,
            rhs_syntax: rhs.get(0).map(|n| *n),
            rhs_prev_novel: None,
        };
        let route = shortest_path(start);

        let actions = route.iter().map(|(action, _)| *action).collect_vec();
        assert_eq!(
            actions,
            vec![
                NovelAtomLHS { contiguous: false },
                NovelAtomRHS { contiguous: false }
            ]
        );
    }
}
