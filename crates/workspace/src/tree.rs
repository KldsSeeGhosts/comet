use serde::{Deserialize, Serialize};

use crate::{LayoutError, Result};

pub const MAX_DEPTH: usize = 16;
pub const MIN_RATIO: f64 = 0.1;
pub const MAX_RATIO: f64 = 0.9;

/// Horizontal splits place first on the left; vertical splits place it above second.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SplitNode<T> {
    Split {
        horizontal: bool,
        ratio: f64,
        first: Box<Self>,
        second: Box<Self>,
    },
    Leaf {
        content: T,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Branch {
    First,
    Second,
}

impl Direction {
    fn horizontal(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }

    fn before(self) -> bool {
        matches!(self, Self::Left | Self::Up)
    }
}

/// Returns the nearest normalized edge within its outer 20%. Corner ties prefer
/// left, right, up, then down. Outside, central and non-finite points return None.
pub fn edge_zone(x: f64, y: f64, width: f64, height: f64) -> Option<Direction> {
    if ![x, y, width, height].iter().all(|v| v.is_finite())
        || width <= 0.0
        || height <= 0.0
        || x < 0.0
        || y < 0.0
        || x > width
        || y > height
    {
        return None;
    }
    let x = x / width;
    let y = y / height;
    [
        (x, Direction::Left),
        (1.0 - x, Direction::Right),
        (y, Direction::Up),
        (1.0 - y, Direction::Down),
    ]
    .into_iter()
    .filter(|(distance, _)| *distance <= 0.2)
    .min_by(|a, b| a.0.total_cmp(&b.0))
    .map(|(_, direction)| direction)
}

pub(crate) fn validate_ratio(ratio: f64) -> Result<()> {
    if !ratio.is_finite() || !(MIN_RATIO..=MAX_RATIO).contains(&ratio) {
        return Err(LayoutError::Invalid(
            "split ratio must be finite and within 0.1..=0.9",
        ));
    }
    Ok(())
}

impl<T> SplitNode<T> {
    pub fn leaf(content: T) -> Self {
        Self::Leaf { content }
    }

    pub fn first_leaf(&self) -> &T {
        match self {
            Self::Leaf { content } => content,
            Self::Split { first, .. } => first.first_leaf(),
        }
    }

    /// The last leaf of this subtree (the mirror of [`Self::first_leaf`]).
    pub fn last_leaf(&self) -> &T {
        match self {
            Self::Leaf { content } => content,
            Self::Split { second, .. } => second.last_leaf(),
        }
    }

    pub(crate) fn collect<'a>(&'a self, depth: usize, leaves: &mut Vec<&'a T>) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(LayoutError::Limit("tree depth"));
        }
        match self {
            Self::Leaf { content } => leaves.push(content),
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                validate_ratio(*ratio)?;
                first.collect(depth + 1, leaves)?;
                second.collect(depth + 1, leaves)?;
            }
        }
        Ok(())
    }

    pub(crate) fn ratio_mut(&mut self, path: &[Branch]) -> Result<&mut f64> {
        match (self, path.split_first()) {
            (Self::Split { ratio, .. }, None) => Ok(ratio),
            (Self::Split { first, second, .. }, Some((branch, rest))) => match branch {
                Branch::First => first.ratio_mut(rest),
                Branch::Second => second.ratio_mut(rest),
            },
            _ => Err(LayoutError::Invalid("tree path does not identify a split")),
        }
    }
}

impl<T: Copy + Eq> SplitNode<T> {
    pub(crate) fn insert(&mut self, target: T, added: T, direction: Direction) -> bool {
        match self {
            Self::Leaf { content } if *content == target => {
                let old = Self::leaf(*content);
                let new = Self::leaf(added);
                let (first, second) = if direction.before() {
                    (new, old)
                } else {
                    (old, new)
                };
                *self = Self::Split {
                    horizontal: direction.horizontal(),
                    ratio: 0.5,
                    first: Box::new(first),
                    second: Box::new(second),
                };
                true
            }
            Self::Leaf { .. } => false,
            Self::Split { first, second, .. } => {
                first.insert(target, added, direction) || second.insert(target, added, direction)
            }
        }
    }

    /// Whether `source` and `target` are direct siblings in a split node,
    /// with `source` already on the `direction` side of `target`.
    pub(crate) fn is_sibling_in_direction(
        &self,
        source: T,
        target: T,
        direction: Direction,
    ) -> bool {
        match self {
            Self::Leaf { .. } => false,
            Self::Split {
                first,
                second,
                horizontal,
                ..
            } => {
                let source_side = match (&**first, &**second) {
                    (Self::Leaf { content: a }, Self::Leaf { content: b }) => {
                        if *a == source && *b == target {
                            Some(Branch::First)
                        } else if *a == target && *b == source {
                            Some(Branch::Second)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                let Some(source_branch) = source_side else {
                    return false;
                };
                let matches = matches!(
                    (source_branch, direction),
                    (Branch::First, Direction::Left | Direction::Up)
                        | (Branch::Second, Direction::Right | Direction::Down)
                );
                let axis_ok = match direction {
                    Direction::Left | Direction::Right => *horizontal,
                    Direction::Up | Direction::Down => !*horizontal,
                };
                matches && axis_ok
            }
        }
    }

    /// Remove a leaf and promote its sibling.
    ///
    /// Returns `(surviving tree, nearest surviving leaf)`:
    /// - the tree is `None` when the removed leaf was the entire tree;
    /// - the nearest leaf is the promoted sibling subtree's leaf that hugs
    ///   the seam the removed leaf left behind: its first leaf when the
    ///   removed leaf sat in the first child, its last leaf when it sat in
    ///   the second. `None` when the target was not present (the tree comes
    ///   back unchanged).
    ///
    /// Callers use the nearest leaf as the focus fallback after a close, so
    /// focus lands beside the closed leaf instead of on the tree's global
    /// first leaf.
    pub(crate) fn remove(self, target: T) -> (Option<Self>, Option<T>) {
        match self {
            Self::Leaf { content } => {
                if content == target {
                    (None, None)
                } else {
                    (Some(Self::Leaf { content }), None)
                }
            }
            Self::Split {
                horizontal,
                ratio,
                first,
                second,
            } => {
                let (new_first, first_nearest) = first.remove(target);
                let Some(new_first) = new_first else {
                    // The first child WAS the removed leaf: the second child
                    // is promoted whole, and its first leaf hugs the seam.
                    let nearest = *second.first_leaf();
                    return (Some(*second), Some(nearest));
                };
                if let Some(nearest) = first_nearest {
                    // Removed deeper inside the surviving first child.
                    return (
                        Some(Self::Split {
                            horizontal,
                            ratio,
                            first: Box::new(new_first),
                            second,
                        }),
                        Some(nearest),
                    );
                }
                // Not in the first child: try the second.
                let (new_second, second_nearest) = second.remove(target);
                let Some(new_second) = new_second else {
                    // The second child WAS the removed leaf: the first child
                    // is promoted whole, and its last leaf hugs the seam.
                    let nearest = *new_first.last_leaf();
                    return (Some(new_first), Some(nearest));
                };
                (
                    Some(Self::Split {
                        horizontal,
                        ratio,
                        first: Box::new(new_first),
                        second: Box::new(new_second),
                    }),
                    second_nearest,
                )
            }
        }
    }
}

/// Swap two leaf contents in the tree — used by `move_pane`'s sibling path.
pub(crate) fn swap_leaves<T: Copy + Eq>(node: &mut SplitNode<T>, a: T, b: T) {
    match node {
        SplitNode::Leaf { content } => {
            if *content == a {
                *content = b;
            } else if *content == b {
                *content = a;
            }
        }
        SplitNode::Split { first, second, .. } => {
            swap_leaves(first, a, b);
            swap_leaves(second, a, b);
        }
    }
}
