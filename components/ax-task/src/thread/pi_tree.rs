//! Allocation-free ordered PI waiter linkage.

use alloc::boxed::Box;
use core::{cell::UnsafeCell, cmp::Ordering, fmt};

use crate::{SchedulingUrgency, ThreadId};

/// Stable ordering copied into both the lock waiter tree and owner donor tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PiWaitKey {
    pub(crate) urgency: SchedulingUrgency,
    pub(crate) sequence: u64,
    pub(crate) thread: ThreadId,
}

impl PiWaitKey {
    pub(crate) const fn new(urgency: SchedulingUrgency, sequence: u64, thread: ThreadId) -> Self {
        Self {
            urgency,
            sequence,
            thread,
        }
    }
}

impl Ord for PiWaitKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.urgency
            .cmp(&other.urgency)
            .then_with(|| self.sequence.cmp(&other.sequence))
            .then_with(|| self.thread.cmp(&other.thread))
    }
}

impl PartialOrd for PiWaitKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

type PiWaitLink = Option<Box<PiWaitNode>>;

/// One preallocated AVL linkage owned by a blocked thread.
pub(crate) struct PiWaitNode {
    key: PiWaitKey,
    left: PiWaitLink,
    right: PiWaitLink,
    height: usize,
}

impl PiWaitNode {
    fn empty() -> Box<Self> {
        Box::new(Self {
            key: PiWaitKey::new(
                SchedulingUrgency::new(u8::MAX, u64::MAX),
                u64::MAX,
                ThreadId::from_parts(0, 0),
            ),
            left: None,
            right: None,
            height: 1,
        })
    }

    fn reset(&mut self, key: PiWaitKey) {
        self.key = key;
        self.left = None;
        self.right = None;
        self.height = 1;
    }

    fn refresh(&mut self) {
        self.height = link_height(&self.left)
            .max(link_height(&self.right))
            .saturating_add(1);
    }
}

impl fmt::Debug for PiWaitNode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PiWaitNode")
            .field("key", &self.key)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// The two independent tree links required by Linux-style PI ownership.
///
/// One link belongs to the mutex waiter tree. The other is linked only while
/// this waiter is the top waiter of a lock owned by another thread.
pub(crate) struct PiWaitNodeStorage {
    lock_waiter: UnsafeCell<Option<Box<PiWaitNode>>>,
    owner_donor: UnsafeCell<Option<Box<PiWaitNode>>>,
}

impl PiWaitNodeStorage {
    pub(crate) fn new() -> Self {
        Self {
            lock_waiter: UnsafeCell::new(Some(PiWaitNode::empty())),
            owner_donor: UnsafeCell::new(Some(PiWaitNode::empty())),
        }
    }

    pub(crate) unsafe fn take_lock_waiter(&self) -> Box<PiWaitNode> {
        unsafe { &mut *self.lock_waiter.get() }
            .take()
            .expect("one thread cannot wait on two PI locks")
    }

    pub(crate) unsafe fn return_lock_waiter(&self, node: Box<PiWaitNode>) {
        assert!(
            unsafe { &mut *self.lock_waiter.get() }
                .replace(node)
                .is_none(),
            "unlinked PI waiter must have one storage owner"
        );
    }

    pub(crate) unsafe fn take_owner_donor(&self) -> Box<PiWaitNode> {
        unsafe { &mut *self.owner_donor.get() }
            .take()
            .expect("one PI waiter can donate through only one lock owner")
    }

    pub(crate) unsafe fn return_owner_donor(&self, node: Box<PiWaitNode>) {
        assert!(
            unsafe { &mut *self.owner_donor.get() }
                .replace(node)
                .is_none(),
            "unlinked PI donor must have one storage owner"
        );
    }
}

// SAFETY: the task-system PI transaction serializes both link transfers. A
// linked node is absent from this storage and cannot be taken a second time.
unsafe impl Sync for PiWaitNodeStorage {}

impl fmt::Debug for PiWaitNodeStorage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PiWaitNodeStorage")
            .finish_non_exhaustive()
    }
}

/// Cached ordered set used for one lock's waiters or one owner's lock tops.
#[derive(Debug)]
pub(crate) struct PiWaitTree {
    root: PiWaitLink,
    first: Option<PiWaitKey>,
    len: usize,
}

impl PiWaitTree {
    pub(crate) const fn new() -> Self {
        Self {
            root: None,
            first: None,
            len: 0,
        }
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn first(&self) -> Option<PiWaitKey> {
        self.first
    }

    pub(crate) fn contains(&self, key: PiWaitKey) -> bool {
        find_node(self.root.as_deref(), key).is_some()
    }

    pub(crate) fn insert(&mut self, key: PiWaitKey, mut node: Box<PiWaitNode>) {
        node.reset(key);
        self.root = insert_node(self.root.take(), node);
        self.first = Some(self.first.map_or(key, |first| first.min(key)));
        self.len = self
            .len
            .checked_add(1)
            .expect("PI waiter tree length overflow");
    }

    pub(crate) fn remove(&mut self, key: PiWaitKey) -> Option<Box<PiWaitNode>> {
        let (root, removed) = remove_node(self.root.take(), key);
        self.root = root;
        if removed.is_some() {
            self.len -= 1;
            if self.first == Some(key) {
                self.first = find_first(self.root.as_deref()).map(|node| node.key);
            }
        }
        removed
    }

    #[cfg(test)]
    pub(crate) fn assert_invariants(&self) {
        let mut previous = None;
        let (count, _) = validate_node(self.root.as_deref(), &mut previous);
        assert_eq!(count, self.len);
        assert_eq!(
            self.first,
            find_first(self.root.as_deref()).map(|node| node.key)
        );
    }
}

fn link_height(link: &PiWaitLink) -> usize {
    link.as_deref().map_or(0, |node| node.height)
}

fn balance_factor(node: &PiWaitNode) -> isize {
    link_height(&node.left) as isize - link_height(&node.right) as isize
}

fn rotate_left(mut root: Box<PiWaitNode>) -> Box<PiWaitNode> {
    let mut promoted = root
        .right
        .take()
        .expect("left rotation requires a right PI waiter child");
    root.right = promoted.left.take();
    root.refresh();
    promoted.left = Some(root);
    promoted.refresh();
    promoted
}

fn rotate_right(mut root: Box<PiWaitNode>) -> Box<PiWaitNode> {
    let mut promoted = root
        .left
        .take()
        .expect("right rotation requires a left PI waiter child");
    root.left = promoted.right.take();
    root.refresh();
    promoted.right = Some(root);
    promoted.refresh();
    promoted
}

fn rebalance(mut node: Box<PiWaitNode>) -> Box<PiWaitNode> {
    node.refresh();
    match balance_factor(&node) {
        factor if factor > 1 => {
            if node
                .left
                .as_deref()
                .is_some_and(|left| balance_factor(left) < 0)
            {
                let left = node.left.take().expect("PI balance requires left child");
                node.left = Some(rotate_left(left));
            }
            rotate_right(node)
        }
        factor if factor < -1 => {
            if node
                .right
                .as_deref()
                .is_some_and(|right| balance_factor(right) > 0)
            {
                let right = node.right.take().expect("PI balance requires right child");
                node.right = Some(rotate_right(right));
            }
            rotate_left(node)
        }
        _ => node,
    }
}

fn insert_node(root: PiWaitLink, inserted: Box<PiWaitNode>) -> PiWaitLink {
    let Some(mut root) = root else {
        return Some(inserted);
    };
    match inserted.key.cmp(&root.key) {
        Ordering::Less => root.left = insert_node(root.left.take(), inserted),
        Ordering::Greater => root.right = insert_node(root.right.take(), inserted),
        Ordering::Equal => panic!("PI waiter tree key must be unique"),
    }
    Some(rebalance(root))
}

fn remove_node(root: PiWaitLink, key: PiWaitKey) -> (PiWaitLink, Option<Box<PiWaitNode>>) {
    let Some(mut root) = root else {
        return (None, None);
    };
    match key.cmp(&root.key) {
        Ordering::Less => {
            let (left, removed) = remove_node(root.left.take(), key);
            root.left = left;
            (Some(rebalance(root)), removed)
        }
        Ordering::Greater => {
            let (right, removed) = remove_node(root.right.take(), key);
            root.right = right;
            (Some(rebalance(root)), removed)
        }
        Ordering::Equal => match (root.left.take(), root.right.take()) {
            (None, right) => (right, Some(root)),
            (left, None) => (left, Some(root)),
            (Some(left), Some(right)) => {
                let (right, mut successor) = take_min(right);
                successor.left = Some(left);
                successor.right = right;
                (Some(rebalance(successor)), Some(root))
            }
        },
    }
}

fn take_min(mut root: Box<PiWaitNode>) -> (PiWaitLink, Box<PiWaitNode>) {
    let Some(left) = root.left.take() else {
        let right = root.right.take();
        root.refresh();
        return (right, root);
    };
    let (left, minimum) = take_min(left);
    root.left = left;
    (Some(rebalance(root)), minimum)
}

fn find_node(node: Option<&PiWaitNode>, key: PiWaitKey) -> Option<&PiWaitNode> {
    let node = node?;
    match key.cmp(&node.key) {
        Ordering::Less => find_node(node.left.as_deref(), key),
        Ordering::Greater => find_node(node.right.as_deref(), key),
        Ordering::Equal => Some(node),
    }
}

fn find_first(node: Option<&PiWaitNode>) -> Option<&PiWaitNode> {
    let mut current = node?;
    while let Some(left) = current.left.as_deref() {
        current = left;
    }
    Some(current)
}

#[cfg(test)]
fn validate_node(node: Option<&PiWaitNode>, previous: &mut Option<PiWaitKey>) -> (usize, usize) {
    let Some(node) = node else {
        return (0, 0);
    };
    let (left_count, left_height) = validate_node(node.left.as_deref(), previous);
    assert!(previous.is_none_or(|key| key < node.key));
    *previous = Some(node.key);
    let (right_count, right_height) = validate_node(node.right.as_deref(), previous);
    let height = left_height.max(right_height).saturating_add(1);
    assert_eq!(node.height, height);
    assert!((left_height as isize - right_height as isize).abs() <= 1);
    (left_count + right_count + 1, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(primary: u64) -> PiWaitKey {
        PiWaitKey::new(
            SchedulingUrgency::new(1, primary),
            primary,
            ThreadId::from_parts(primary as u32 + 1, 1),
        )
    }

    #[test]
    fn cached_first_and_avl_links_survive_reordering_removals() {
        let mut tree = PiWaitTree::new();
        for primary in [30, 10, 50, 20, 40, 60] {
            tree.insert(key(primary), PiWaitNode::empty());
            tree.assert_invariants();
        }
        assert_eq!(tree.first(), Some(key(10)));

        for primary in [10, 50, 30, 20, 40, 60] {
            let removed = tree.remove(key(primary)).expect("inserted key must exist");
            assert_eq!(removed.key, key(primary));
            tree.assert_invariants();
        }
        assert!(tree.is_empty());
    }
}
