//! Transient split geometry. The persisted layout always holds its final ratios.

use std::time::Instant;

use zeron_workspace::{Branch, SplitNode};

use crate::motion::{EASE_OUT_EXPO, MotionSpec, lerp, speed_scale};

const SPLIT_MOTION: MotionSpec = MotionSpec::new(200, EASE_OUT_EXPO);

#[derive(Clone, Debug)]
pub(super) enum VisualNode<T> {
    Leaf(T),
    Empty,
    Split {
        horizontal: bool,
        from: f32,
        ratio: f32,
        // A collapsing divider has no logical counterpart and cannot be dragged.
        path: Option<Vec<Branch>>,
        first: Box<Self>,
        second: Box<Self>,
    },
}

impl<T: Copy + Eq> VisualNode<T> {
    fn settled(node: &SplitNode<T>, path: Vec<Branch>) -> Self {
        match node {
            SplitNode::Leaf { content } => Self::Leaf(*content),
            SplitNode::Split { horizontal, ratio, first, second } => {
                let mut left = path.clone();
                left.push(Branch::First);
                let mut right = path.clone();
                right.push(Branch::Second);
                Self::Split {
                    horizontal: *horizontal, from: *ratio as f32, ratio: *ratio as f32,
                    path: Some(path),
                    first: Box::new(Self::settled(first, left)),
                    second: Box::new(Self::settled(second, right)),
                }
            }
        }
    }

    fn same_shape(&self, node: &SplitNode<T>) -> bool {
        match (self, node) {
            (Self::Leaf(a), SplitNode::Leaf { content: b }) => a == b,
            (Self::Split { horizontal: a, first: af, second: as_, .. },
             SplitNode::Split { horizontal: b, first: bf, second: bs, .. }) =>
                a == b && af.same_shape(bf) && as_.same_shape(bs),
            _ => false,
        }
    }

    fn find(&self, node: &SplitNode<T>) -> Option<&Self> {
        if self.same_shape(node) { return Some(self); }
        match self {
            Self::Split { first, second, .. } => first.find(node).or_else(|| second.find(node)),
            _ => None,
        }
    }

    fn moving(&self) -> bool {
        match self {
            Self::Split { from, ratio, first, second, .. } =>
                (from - ratio).abs() > f32::EPSILON || first.moving() || second.moving(),
            _ => false,
        }
    }

    fn sample(&self, progress: f32) -> Self {
        match self {
            Self::Split { horizontal, from, ratio, path, first, second } => {
                let value = lerp(*from, *ratio, progress);
                Self::Split {
                    horizontal: *horizontal, from: value, ratio: value, path: path.clone(),
                    first: Box::new(first.sample(progress)), second: Box::new(second.sample(progress)),
                }
            }
            node => node.clone(),
        }
    }
}

// Match by leaf identities rather than tree paths: a path may name a different
// split after a move. Empty exit branches never mount removed or moved entities.
fn transition<T: Copy + Eq>(old: &VisualNode<T>, new: &SplitNode<T>, path: Vec<Branch>) -> VisualNode<T> {
    if let VisualNode::Split { horizontal, ratio, first, second, .. } = old
        && !old.same_shape(new)
    {
        let survivor = if first.same_shape(new) { Some((first, true)) }
            else if second.same_shape(new) { Some((second, false)) } else { None };
        if let Some((survivor, before)) = survivor {
            let child = Box::new(transition(survivor, new, path));
            let empty = Box::new(VisualNode::Empty);
            let (first, second) = if before { (child, empty) } else { (empty, child) };
            return VisualNode::Split {
                horizontal: *horizontal, from: *ratio, ratio: if before { 1.0 } else { 0.0 },
                path: None, first, second,
            };
        }
    }
    match new {
        SplitNode::Leaf { content } => VisualNode::Leaf(*content),
        SplitNode::Split { horizontal, ratio, first, second } => {
            let matching = old.find(new);
            let (from, left, right) = if let Some(VisualNode::Split { ratio, first, second, .. }) = matching {
                (*ratio, first.as_ref(), second.as_ref())
            } else if old.same_shape(first) {
                (1.0, old, &VisualNode::Empty)
            } else if old.same_shape(second) {
                (0.0, &VisualNode::Empty, old)
            } else {
                (*ratio as f32, old, old)
            };
            let mut left_path = path.clone(); left_path.push(Branch::First);
            let mut right_path = path.clone(); right_path.push(Branch::Second);
            VisualNode::Split {
                horizontal: *horizontal, from, ratio: *ratio as f32, path: Some(path),
                first: Box::new(transition(left, first, left_path)),
                second: Box::new(transition(right, second, right_path)),
            }
        }
    }
}

pub(super) struct TreeMotion<T> {
    target: SplitNode<T>,
    origin: VisualNode<T>,
    started: Instant,
}

impl<T: Copy + Eq> TreeMotion<T> {
    pub fn new(target: &SplitNode<T>, now: Instant) -> Self {
        Self { target: target.clone(), origin: VisualNode::settled(target, vec![]), started: now }
    }

    pub fn update(&mut self, target: &SplitNode<T>, now: Instant, snap: bool) {
        if snap {
            self.target = target.clone();
            self.origin = VisualNode::settled(target, vec![]);
        } else if self.target != *target {
            let current = self.sample(now);
            self.origin = transition(&current, target, vec![]);
            self.target = target.clone();
            self.started = now;
        }
    }

    pub fn active(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) < SPLIT_MOTION.total().mul_f32(speed_scale())
            && self.origin.moving()
    }

    pub fn sample(&self, now: Instant) -> VisualNode<T> {
        if !self.active(now) { return VisualNode::settled(&self.target, vec![]); }
        let progress = now.saturating_duration_since(self.started).as_secs_f32()
            / SPLIT_MOTION.total().mul_f32(speed_scale()).as_secs_f32();
        self.origin.sample(SPLIT_MOTION.progress(progress))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn split(first: u8, second: u8, ratio: f64) -> SplitNode<u8> {
        SplitNode::Split { horizontal: true, ratio,
            first: Box::new(SplitNode::leaf(first)), second: Box::new(SplitNode::leaf(second)) }
    }

    fn ratio(node: VisualNode<u8>) -> f32 {
        let VisualNode::Split { ratio, .. } = node else { panic!("expected split") };
        ratio
    }

    #[test]
    fn insertion_eases_from_the_correct_edge_without_changing_the_model() {
        let now = Instant::now();
        for (initial, from) in [(1, 1.0), (2, 0.0)] {
            let target = split(1, 2, 0.5);
            let mut motion = TreeMotion::new(&SplitNode::leaf(initial), now);
            motion.update(&target, now, false);
            assert_eq!(ratio(motion.sample(now)), from);
            let mid = ratio(motion.sample(now + Duration::from_millis(50)));
            assert!((mid - 0.5).abs() < (from - 0.5).abs());
            assert_eq!(ratio(motion.sample(now + Duration::from_millis(200))), 0.5);
            assert_eq!(motion.target, target);
        }
    }

    #[test]
    fn removal_uses_empty_space_and_final_logical_divider_paths() {
        let now = Instant::now();
        let mut motion = TreeMotion::new(&split(1, 2, 0.3), now);
        motion.update(&SplitNode::leaf(2), now, false);
        let VisualNode::Split { path, first, second, .. } = motion.sample(now) else { panic!() };
        assert!(path.is_none());
        assert!(matches!(*first, VisualNode::Empty));
        assert!(matches!(*second, VisualNode::Leaf(2)));
        assert!(matches!(motion.sample(now + Duration::from_millis(200)), VisualNode::Leaf(2)));
    }

    #[test]
    fn equalization_rebases_and_manual_resize_or_reduced_motion_snaps() {
        let now = Instant::now();
        let mut motion = TreeMotion::new(&split(1, 2, 0.2), now);
        motion.update(&split(1, 2, 0.5), now, false);
        let later = now + Duration::from_millis(50);
        let current = ratio(motion.sample(later));
        motion.update(&split(1, 2, 0.8), later, false);
        assert_eq!(ratio(motion.sample(later)), current);
        motion.update(&split(1, 2, 0.4), later, true);
        assert_eq!(ratio(motion.sample(later)), 0.4);
        assert!(!motion.active(later));
    }
}
