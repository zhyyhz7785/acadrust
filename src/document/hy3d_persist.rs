// SPDX-License-Identifier: MIT OR Apache-2.0
// Ported from HY3D core/src/persist.rs at bb811a0cace2dd1af6ec2b47ae1d19f42c617a12 (HY3D's workspace license).
// Native CAD records retain their original Arc<T>; no second geometry object.
//! 内存里的分块写时复制。扇出只存在这里，不写进文件。
//!
//! 叶子保存指向单条记录的共享指针。修改一条记录时，只复制通往该叶的内部节点、
//! 该叶的指针数组，以及被改的那一条记录。其余记录继续共用。
//! 记录是否同一份，由调用方比较记录的值。这里不拿指针地址当内容相等。

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt::{self, Debug};
use std::marker::PhantomData;
use std::ops::{Bound, Index, IndexMut, RangeBounds};
use std::sync::Arc;

#[cfg(any(feature = "serde", feature = "hy3d-persist-serde"))]
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 内部节点和叶子的最大子项数。只在内存里使用。
pub const FANOUT: usize = 32;
const MIN_OCCUPANCY: usize = FANOUT / 2;

/// 按键有序的持久化表。克隆只复制根指针。
pub struct CowMap<K, V> {
    root: Option<Arc<Node<K, V>>>,
    len: usize,
}

enum Node<K, V> {
    Leaf(Leaf<K, V>),
    Branch(Branch<K, V>),
}

struct Leaf<K, V> {
    keys: Vec<K>,
    records: Vec<Arc<V>>,
}

struct Branch<K, V> {
    /// `keys[i]` 是 `children[i + 1]` 子树里的最小键。
    keys: Vec<K>,
    children: Vec<Arc<Node<K, V>>>,
}

/// 仅供内核派生数据使用的子树摘要缓存。Weak 不保留旧几何；
/// Arc::make_mut 会在存在 Weak 时换出被修改的节点，未修改子树仍可复用。
/// 调用方必须在摘要所依赖的外部状态改变时丢弃整个缓存。
pub struct CowFoldMemo<K, V, A> {
    nodes: std::collections::HashMap<usize, (std::sync::Weak<Node<K, V>>, A)>,
    root: Option<std::sync::Weak<Node<K, V>>>,
}

impl<K, V, A> Default for CowFoldMemo<K, V, A> {
    fn default() -> Self {
        Self { nodes: Default::default(), root: None }
    }
}

impl<K, V, A: Copy> CowFoldMemo<K, V, A> {
    pub fn matches_root(&self, map: &CowMap<K, V>) -> bool {
        match (&self.root, &map.root) {
            (None, None) => true,
            (Some(old), Some(now)) => old.as_ptr() == Arc::as_ptr(now),
            _ => false,
        }
    }

    pub fn fold(
        &mut self,
        map: &CowMap<K, V>,
        empty: A,
        mut add: impl FnMut(&mut A, &V),
        mut merge: impl FnMut(&mut A, A),
    ) -> A {
        // 弱引用可能还对应历史里的活节点。上限按当前模型规模设定，
        // 不随撤销历史或反复编辑无限增长。
        let budget = (map.len() / MIN_OCCUPANCY + 1).saturating_mul(4).max(128);
        // 小改后的热路径不扫全缓存。只有超过容量预算才回收退役弱引用，
        // 历史仍持有太多节点时整表退回；几何始终由模型/历史管理。
        if self.nodes.len() > budget {
            self.nodes.retain(|_, (node, _)| node.strong_count() != 0);
            if self.nodes.len() > budget {
                self.nodes.clear();
            }
        }
        let Some(root) = map.root.as_ref() else {
            self.root = None;
            self.nodes.clear();
            return empty;
        };
        let value = self.fold_node(root, empty, &mut add, &mut merge);
        self.root = Some(Arc::downgrade(root));
        value
    }

    fn fold_node(
        &mut self,
        node: &Arc<Node<K, V>>,
        empty: A,
        add: &mut impl FnMut(&mut A, &V),
        merge: &mut impl FnMut(&mut A, A),
    ) -> A {
        let address = Arc::as_ptr(node) as usize;
        if let Some((weak, value)) = self.nodes.get(&address) {
            // Weak 保留分配身份，地址不会在该 Weak 存活时被别的节点重用。
            if weak.as_ptr() == Arc::as_ptr(node) {
                return *value;
            }
        }
        let mut result = empty;
        match node.as_ref() {
            Node::Leaf(leaf) => {
                for record in &leaf.records {
                    add(&mut result, record.as_ref());
                }
            }
            Node::Branch(branch) => {
                for child in &branch.children {
                    let value = self.fold_node(child, empty, add, merge);
                    merge(&mut result, value);
                }
            }
        }
        self.nodes.insert(address, (Arc::downgrade(node), result));
        result
    }
}

struct Rise<K, V> {
    sep: K,
    right: Arc<Node<K, V>>,
}

enum Moved<K, V> {
    Item(K, Arc<V>),
    Sub(Arc<Node<K, V>>),
}

impl<K, V> Clone for CowMap<K, V> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            len: self.len,
        }
    }
}

impl<K: Clone, V> Clone for Node<K, V> {
    fn clone(&self) -> Self {
        match self {
            Node::Leaf(leaf) => Node::Leaf(leaf.clone()),
            Node::Branch(branch) => Node::Branch(branch.clone()),
        }
    }
}

impl<K: Clone, V> Clone for Leaf<K, V> {
    fn clone(&self) -> Self {
        Self {
            keys: self.keys.clone(),
            records: self.records.clone(),
        }
    }
}

impl<K: Clone, V> Clone for Branch<K, V> {
    fn clone(&self) -> Self {
        Self {
            keys: self.keys.clone(),
            children: self.children.clone(),
        }
    }
}

impl<K, V> Default for CowMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> CowMap<K, V> {
    pub fn new() -> Self {
        Self { root: None, len: 0 }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// 两份表是否还挂着同一棵根。保存回写保真记录时用来判断锁外编码期间有没有人改过这张表。
    pub fn same_root(&self, other: &Self) -> bool {
        match (&self.root, &other.root) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Ord + Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.shared_record(key).map(Arc::as_ref)
    }

    /// 内部历史捕获共享不可变记录；后续修改仍由 Arc::make_mut 隔离。
    pub fn shared_record<Q>(&self, key: &Q) -> Option<&Arc<V>>
    where
        K: Ord + Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let mut node = self.root.as_ref()?;
        loop {
            match node.as_ref() {
                Node::Leaf(leaf) => {
                    let i = search_by(&leaf.keys, key).ok()?;
                    return Some(&leaf.records[i]);
                }
                Node::Branch(branch) => {
                    let i = child_index(&branch.keys, key);
                    node = &branch.children[i];
                }
            }
        }
    }

    pub fn get_key_value<Q>(&self, key: &Q) -> Option<(&K, &V)>
    where
        K: Ord + Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let mut node = self.root.as_ref()?;
        loop {
            match node.as_ref() {
                Node::Leaf(leaf) => {
                    let i = search_by(&leaf.keys, key).ok()?;
                    return Some((&leaf.keys[i], leaf.records[i].as_ref()));
                }
                Node::Branch(branch) => {
                    let i = child_index(&branch.keys, key);
                    node = &branch.children[i];
                }
            }
        }
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Ord + Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.get(key).is_some()
    }

    /// 只复制通往该记录的路径和这一条记录。同一片叶子里的其他记录不复制。
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Ord + Clone + Borrow<Q>,
        Q: Ord + ?Sized,
        V: Clone,
    {
        let root = self.root.as_mut()?;
        get_mut_node(root, key)
    }

    pub fn clear(&mut self) {
        self.root = None;
        self.len = 0;
    }

    /// 插入或替换。替换时返回原来的那条记录。记录仍被别的根持有时，返回值是一份克隆。
    pub fn insert(&mut self, key: K, value: V) -> Option<V>
    where
        K: Ord + Clone,
        V: Clone,
    {
        self.insert_arc(key, value).map(owned_record)
    }

    fn insert_arc(&mut self, key: K, value: V) -> Option<Arc<V>>
    where
        K: Ord + Clone,
    {
        if self.root.is_none() {
            self.root = Some(Arc::new(Node::Leaf(Leaf {
                keys: vec![key],
                records: vec![Arc::new(value)],
            })));
            self.len = 1;
            return None;
        }
        let root = self.root.as_mut().unwrap();
        let (old, rise) = insert_node(root, key, value);
        if let Some(rise) = rise {
            let left = Arc::clone(self.root.as_ref().unwrap());
            self.root = Some(Arc::new(Node::Branch(Branch {
                keys: vec![rise.sep],
                children: vec![left, rise.right],
            })));
        }
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    /// 键不存在时不复制路径。
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Ord + Clone + Borrow<Q>,
        Q: Ord + ?Sized,
        V: Clone,
    {
        self.remove_arc(key).map(owned_record)
    }

    fn remove_arc<Q>(&mut self, key: &Q) -> Option<Arc<V>>
    where
        K: Ord + Clone + Borrow<Q>,
        Q: Ord + ?Sized,
    {
        if !self.contains_key(key) {
            return None;
        }
        let root = self.root.as_mut().unwrap();
        let old = remove_node(root, key);
        self.collapse();
        if old.is_some() {
            self.len -= 1;
        }
        old
    }

    pub fn iter(&self) -> Iter<'_, K, V>
    where
        K: Ord,
    {
        self.iter_bounded(Bound::Unbounded, Bound::Unbounded)
    }

    /// 叶子上的共享记录指针。克隆这枚指针不复制记录。
    pub fn iter_arcs(&self) -> ArcIter<'_, K, V>
    where
        K: Ord,
    {
        let mut stack = Vec::new();
        if let Some(root) = self.root.as_deref() {
            position(&mut stack, root, Bound::Unbounded);
        }
        ArcIter { stack }
    }

    pub fn range<R>(&self, range: R) -> Iter<'_, K, V>
    where
        K: Ord + Clone,
        R: RangeBounds<K>,
    {
        let start = match range.start_bound() {
            Bound::Unbounded => Bound::Unbounded,
            Bound::Included(k) => Bound::Included(k),
            Bound::Excluded(k) => Bound::Excluded(k),
        };
        self.iter_bounded(start, owned_bound(range.end_bound()))
    }

    fn iter_bounded(&self, start: Bound<&K>, end: Bound<K>) -> Iter<'_, K, V>
    where
        K: Ord,
    {
        let mut stack = Vec::new();
        if let Some(root) = self.root.as_deref() {
            position(&mut stack, root, start);
        }
        Iter { stack, end, rev_before: None }
    }

    fn collapse(&mut self) {
        loop {
            let next = match self.root.as_deref() {
                Some(Node::Leaf(leaf)) if leaf.keys.is_empty() => {
                    self.root = None;
                    return;
                }
                Some(Node::Branch(branch)) if branch.children.len() <= 1 => {
                    branch.children.first().cloned()
                }
                _ => return,
            };
            self.root = next;
        }
    }
}

fn owned_bound<K: Clone>(bound: Bound<&K>) -> Bound<K> {
    match bound {
        Bound::Unbounded => Bound::Unbounded,
        Bound::Included(k) => Bound::Included(k.clone()),
        Bound::Excluded(k) => Bound::Excluded(k.clone()),
    }
}

fn search_by<K, Q>(keys: &[K], key: &Q) -> Result<usize, usize>
where
    K: Borrow<Q>,
    Q: Ord + ?Sized,
{
    keys.binary_search_by(|probe| probe.borrow().cmp(key))
}

fn child_index<K, Q>(keys: &[K], key: &Q) -> usize
where
    K: Borrow<Q>,
    Q: Ord + ?Sized,
{
    match search_by(keys, key) {
        Ok(i) => i + 1,
        Err(i) => i,
    }
}

fn first_key<K, V>(node: &Node<K, V>) -> &K {
    match node {
        Node::Leaf(leaf) => &leaf.keys[0],
        Node::Branch(branch) => first_key(&branch.children[0]),
    }
}

fn node_empty<K, V>(node: &Node<K, V>) -> bool {
    occupancy(node) == 0
}

fn occupancy<K, V>(node: &Node<K, V>) -> usize {
    match node {
        Node::Leaf(leaf) => leaf.keys.len(),
        Node::Branch(branch) => branch.children.len(),
    }
}

fn recompute_seps<K: Clone, V>(branch: &mut Branch<K, V>) {
    debug_assert!(branch.children.iter().all(|child| !node_empty(child)));
    branch.keys.clear();
    for child in branch.children.iter().skip(1) {
        branch.keys.push(first_key(child).clone());
    }
}

fn insert_node<K: Ord + Clone, V>(
    node: &mut Arc<Node<K, V>>,
    key: K,
    value: V,
) -> (Option<Arc<V>>, Option<Rise<K, V>>) {
    match Arc::make_mut(node) {
        Node::Leaf(leaf) => match leaf.keys.binary_search(&key) {
            Ok(i) => {
                let old = std::mem::replace(&mut leaf.records[i], Arc::new(value));
                (Some(old), None)
            }
            Err(i) => {
                leaf.keys.insert(i, key);
                leaf.records.insert(i, Arc::new(value));
                let rise = (leaf.keys.len() > FANOUT).then(|| split_leaf(leaf));
                (None, rise)
            }
        },
        Node::Branch(branch) => {
            let i = child_index(&branch.keys, &key);
            let (old, rise) = insert_node(&mut branch.children[i], key, value);
            // 路由保证非首子树的新键不小于它现有的最小键；
            // 首子树没有分隔键。插入/替换只在子树分裂时新增一个分隔键，
            // 不必每层遍历其它子树并克隆全部最小键。
            if let Some(rise) = rise {
                branch.keys.insert(i, rise.sep);
                branch.children.insert(i + 1, rise.right);
            }
            let up = (branch.children.len() > FANOUT).then(|| split_branch(branch));
            (old, up)
        }
    }
}

fn split_leaf<K: Clone, V>(leaf: &mut Leaf<K, V>) -> Rise<K, V> {
    let mid = leaf.keys.len() / 2;
    let keys = leaf.keys.split_off(mid);
    let records = leaf.records.split_off(mid);
    let sep = keys[0].clone();
    Rise {
        sep,
        right: Arc::new(Node::Leaf(Leaf { keys, records })),
    }
}

fn split_branch<K, V>(branch: &mut Branch<K, V>) -> Rise<K, V> {
    let mid = branch.children.len() / 2;
    let children = branch.children.split_off(mid);
    // 已有分隔键按同样的边界切开；中间那个键提升给父节点。
    let keys = branch.keys.split_off(mid);
    let sep = branch.keys.pop().expect("分裂前有中间分隔键");
    Rise {
        sep,
        right: Arc::new(Node::Branch(Branch { keys, children })),
    }
}

fn remove_node<K, Q, V>(node: &mut Arc<Node<K, V>>, key: &Q) -> Option<Arc<V>>
where
    K: Ord + Clone + Borrow<Q>,
    Q: Ord + ?Sized,
{
    match Arc::make_mut(node) {
        Node::Leaf(leaf) => {
            let i = search_by(&leaf.keys, key).ok()?;
            leaf.keys.remove(i);
            Some(leaf.records.remove(i))
        }
        Node::Branch(branch) => {
            let i = child_index(&branch.keys, key);
            let old = remove_node(&mut branch.children[i], key)?;
            if node_empty(&branch.children[i]) {
                branch.children.remove(i);
                if branch.children.is_empty() {
                    branch.keys.clear();
                } else {
                    branch.keys.remove(i.saturating_sub(1));
                }
            } else if occupancy(&branch.children[i]) < MIN_OCCUPANCY {
                rebalance(branch, i);
                // 借位/合并仍统一重算分隔键，保持原平衡规则。
                recompute_seps(branch);
            } else if i > 0 && <K as Borrow<Q>>::borrow(&branch.keys[i - 1]) == key {
                // 普通删除只有删掉当前子树最小键时才会改变分隔键。
                branch.keys[i - 1] = first_key(&branch.children[i]).clone();
            }
            Some(old)
        }
    }
}

fn rebalance<K: Clone, V>(branch: &mut Branch<K, V>, i: usize) {
    if i > 0 && occupancy(&branch.children[i - 1]) > MIN_OCCUPANCY {
        steal_left(branch, i);
    } else if i + 1 < branch.children.len() && occupancy(&branch.children[i + 1]) > MIN_OCCUPANCY {
        steal_right(branch, i);
    } else if i > 0 {
        merge_left(branch, i);
    } else if i + 1 < branch.children.len() {
        merge_right(branch, i);
    }
}

fn steal_left<K: Clone, V>(branch: &mut Branch<K, V>, i: usize) {
    let moved = {
        let left = Arc::make_mut(&mut branch.children[i - 1]);
        match left {
            Node::Leaf(leaf) => {
                let key = leaf.keys.pop().expect("左叶有记录");
                let record = leaf.records.pop().expect("左叶有记录");
                Moved::Item(key, record)
            }
            Node::Branch(child) => {
                let sub = child.children.pop().expect("左枝有子节点");
                recompute_seps(child);
                Moved::Sub(sub)
            }
        }
    };
    let right = Arc::make_mut(&mut branch.children[i]);
    match (right, moved) {
        (Node::Leaf(leaf), Moved::Item(key, record)) => {
            leaf.keys.insert(0, key);
            leaf.records.insert(0, record);
        }
        (Node::Branch(child), Moved::Sub(sub)) => {
            child.children.insert(0, sub);
            recompute_seps(child);
        }
        _ => unreachable!("同一层节点种类一致"),
    }
}

fn steal_right<K: Clone, V>(branch: &mut Branch<K, V>, i: usize) {
    let moved = {
        let right = Arc::make_mut(&mut branch.children[i + 1]);
        match right {
            Node::Leaf(leaf) => {
                let key = leaf.keys.remove(0);
                let record = leaf.records.remove(0);
                Moved::Item(key, record)
            }
            Node::Branch(child) => {
                let sub = child.children.remove(0);
                recompute_seps(child);
                Moved::Sub(sub)
            }
        }
    };
    let left = Arc::make_mut(&mut branch.children[i]);
    match (left, moved) {
        (Node::Leaf(leaf), Moved::Item(key, record)) => {
            leaf.keys.push(key);
            leaf.records.push(record);
        }
        (Node::Branch(child), Moved::Sub(sub)) => {
            child.children.push(sub);
            recompute_seps(child);
        }
        _ => unreachable!("同一层节点种类一致"),
    }
}

fn merge_left<K: Clone, V>(branch: &mut Branch<K, V>, i: usize) {
    let right = branch.children.remove(i);
    let left = Arc::make_mut(&mut branch.children[i - 1]);
    append_node(left, right.as_ref());
}

fn merge_right<K: Clone, V>(branch: &mut Branch<K, V>, i: usize) {
    let right = branch.children.remove(i + 1);
    let left = Arc::make_mut(&mut branch.children[i]);
    append_node(left, right.as_ref());
}

fn append_node<K: Clone, V>(left: &mut Node<K, V>, right: &Node<K, V>) {
    match (left, right) {
        (Node::Leaf(dst), Node::Leaf(src)) => {
            dst.keys.extend(src.keys.iter().cloned());
            dst.records.extend(src.records.iter().cloned());
        }
        (Node::Branch(dst), Node::Branch(src)) => {
            dst.children.extend(src.children.iter().cloned());
            recompute_seps(dst);
        }
        _ => unreachable!("同一层节点种类一致"),
    }
}

fn get_mut_node<'a, K, Q, V>(node: &'a mut Arc<Node<K, V>>, key: &Q) -> Option<&'a mut V>
where
    K: Ord + Clone + Borrow<Q>,
    Q: Ord + ?Sized,
    V: Clone,
{
    match Arc::make_mut(node) {
        Node::Leaf(leaf) => {
            let i = search_by(&leaf.keys, key).ok()?;
            Some(Arc::make_mut(&mut leaf.records[i]))
        }
        Node::Branch(branch) => {
            let i = child_index(&branch.keys, key);
            get_mut_node(&mut branch.children[i], key)
        }
    }
}

fn lower_index<K: Ord>(keys: &[K], start: Bound<&K>) -> usize {
    match start {
        Bound::Unbounded => 0,
        Bound::Included(key) => keys.binary_search(key).unwrap_or_else(|i| i),
        Bound::Excluded(key) => match keys.binary_search(key) {
            Ok(i) => i + 1,
            Err(i) => i,
        },
    }
}

fn position<'a, K, V>(stack: &mut Vec<Frame<'a, K, V>>, node: &'a Node<K, V>, start: Bound<&K>)
where
    K: Ord,
{
    match node {
        Node::Leaf(leaf) => {
            let next = lower_index(&leaf.keys, start);
            stack.push(Frame { node, next });
        }
        Node::Branch(branch) => {
            let i = match start {
                Bound::Unbounded => 0,
                Bound::Included(key) | Bound::Excluded(key) => child_index(&branch.keys, key),
            };
            stack.push(Frame { node, next: i + 1 });
            position(stack, &branch.children[i], start);
        }
    }
}

fn within_end<K: Ord>(key: &K, end: &Bound<K>) -> bool {
    match end {
        Bound::Unbounded => true,
        Bound::Included(bound) => key <= bound,
        Bound::Excluded(bound) => key < bound,
    }
}

struct Frame<'a, K, V> {
    node: &'a Node<K, V>,
    next: usize,
}

pub struct Iter<'a, K, V> {
    stack: Vec<Frame<'a, K, V>>,
    end: Bound<K>,
    rev_before: Option<K>,
}

impl<'a, K, V> Iterator for Iter<'a, K, V>
where
    K: Ord,
{
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let frame = self.stack.last_mut()?;
            match frame.node {
                Node::Leaf(leaf) => {
                    if frame.next >= leaf.keys.len() {
                        self.stack.pop();
                        continue;
                    }
                    let i = frame.next;
                    let key = &leaf.keys[i];
                    if !within_end(key, &self.end) || self.rev_before.as_ref().is_some_and(|limit| key >= limit) {
                        self.stack.clear();
                        return None;
                    }
                    frame.next += 1;
                    return Some((key, leaf.records[i].as_ref()));
                }
                Node::Branch(branch) => {
                    if frame.next >= branch.children.len() {
                        self.stack.pop();
                        continue;
                    }
                    let child = branch.children[frame.next].as_ref();
                    frame.next += 1;
                    self.stack.push(Frame { node: child, next: 0 });
                }
            }
        }
    }
}

impl<'a, K, V> DoubleEndedIterator for Iter<'a, K, V>
where
    K: Ord + Clone,
{
    fn next_back(&mut self) -> Option<Self::Item> {
        let root = self.stack.first()?.node;
        let (key, value) = last_before(root, &self.end, self.rev_before.as_ref())?;
        self.rev_before = Some(key.clone());
        Some((key, value))
    }
}

/// 按键顺序交出叶子里的共享指针。
pub struct ArcIter<'a, K, V> {
    stack: Vec<Frame<'a, K, V>>,
}

impl<'a, K, V> Iterator for ArcIter<'a, K, V>
where
    K: Ord,
{
    type Item = (&'a K, &'a Arc<V>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let frame = self.stack.last_mut()?;
            match frame.node {
                Node::Leaf(leaf) => {
                    if frame.next >= leaf.keys.len() {
                        self.stack.pop();
                        continue;
                    }
                    let i = frame.next;
                    frame.next += 1;
                    return Some((&leaf.keys[i], &leaf.records[i]));
                }
                Node::Branch(branch) => {
                    if frame.next >= branch.children.len() {
                        self.stack.pop();
                        continue;
                    }
                    let child = branch.children[frame.next].as_ref();
                    frame.next += 1;
                    self.stack.push(Frame { node: child, next: 0 });
                }
            }
        }
    }
}

fn last_before<'a, K: Ord, V>(
    node: &'a Node<K, V>,
    end: &Bound<K>,
    before: Option<&K>,
) -> Option<(&'a K, &'a V)> {
    let open = |key: &K| within_end(key, end) && before.is_none_or(|limit| key < limit);
    match node {
        Node::Leaf(leaf) => {
            for i in (0..leaf.keys.len()).rev() {
                let key = &leaf.keys[i];
                if open(key) {
                    return Some((key, leaf.records[i].as_ref()));
                }
            }
            None
        }
        Node::Branch(branch) => {
            for i in (0..branch.children.len()).rev() {
                if i > 0 {
                    let min = &branch.keys[i - 1];
                    if !within_end(min, end) || before.is_some_and(|limit| min >= limit) {
                        continue;
                    }
                }
                return last_before(&branch.children[i], end, before);
            }
            None
        }
    }
}

/// 按下标排列的持久化序列。克隆只复制根指针。
pub struct CowSeq<T> {
    root: Option<Arc<SeqNode<T>>>,
    len: usize,
}

enum SeqNode<T> {
    Leaf(SeqLeaf<T>),
    Branch(SeqBranch<T>),
}

struct SeqLeaf<T> {
    records: Vec<Arc<T>>,
}

struct SeqBranch<T> {
    children: Vec<Arc<SeqNode<T>>>,
    lens: Vec<usize>,
}

enum SeqMoved<T> {
    Item(Arc<T>),
    Sub(Arc<SeqNode<T>>),
}

impl<T> Clone for CowSeq<T> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            len: self.len,
        }
    }
}

impl<T> Clone for SeqNode<T> {
    fn clone(&self) -> Self {
        match self {
            SeqNode::Leaf(leaf) => SeqNode::Leaf(leaf.clone()),
            SeqNode::Branch(branch) => SeqNode::Branch(branch.clone()),
        }
    }
}

impl<T> Clone for SeqLeaf<T> {
    fn clone(&self) -> Self {
        Self {
            records: self.records.clone(),
        }
    }
}

impl<T> Clone for SeqBranch<T> {
    fn clone(&self) -> Self {
        Self {
            children: self.children.clone(),
            lens: self.lens.clone(),
        }
    }
}

impl<T> Default for CowSeq<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> CowSeq<T> {
    pub fn new() -> Self {
        Self { root: None, len: 0 }
    }

    /// 内部快照可用根身份确认内容没有被写入，不改变通用 PartialEq 的值比较语义。
    pub fn same_root(&self, other: &Self) -> bool {
        match (&self.root, &other.root) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.root = None;
        self.len = 0;
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        seq_get(self.root.as_ref()?.as_ref(), index)
    }

    /// 只复制通往该记录的路径和这一条记录。
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T>
    where
        T: Clone,
    {
        if index >= self.len {
            return None;
        }
        let root = self.root.as_mut()?;
        seq_get_mut(root, index)
    }

    pub fn push(&mut self, value: T) {
        let index = self.len;
        self.insert(index, value);
    }

    pub fn insert(&mut self, index: usize, value: T) {
        assert!(index <= self.len, "持久化序列下标越界");
        let value = Arc::new(value);
        if self.root.is_none() {
            self.root = Some(Arc::new(SeqNode::Leaf(SeqLeaf { records: vec![value] })));
            self.len = 1;
            return;
        }
        let root = self.root.as_mut().unwrap();
        if let Some(right) = seq_insert(root, index, value) {
            let left = Arc::clone(self.root.as_ref().unwrap());
            let mut branch = SeqBranch {
                children: vec![left, right],
                lens: Vec::new(),
            };
            rebuild_lens(&mut branch);
            self.root = Some(Arc::new(SeqNode::Branch(branch)));
        }
        self.len += 1;
    }

    pub fn remove(&mut self, index: usize) -> Option<T>
    where
        T: Clone,
    {
        self.remove_arc(index).map(owned_record)
    }

    fn remove_arc(&mut self, index: usize) -> Option<Arc<T>> {
        if index >= self.len {
            return None;
        }
        let root = self.root.as_mut().unwrap();
        let old = seq_remove(root, index);
        self.collapse();
        if old.is_some() {
            self.len -= 1;
        }
        old
    }

    pub fn pop(&mut self) -> Option<T>
    where
        T: Clone,
    {
        if self.len == 0 {
            None
        } else {
            self.remove(self.len - 1)
        }
    }

    pub fn iter(&self) -> SeqIter<'_, T> {
        let mut stack = Vec::new();
        if let Some(root) = self.root.as_deref() {
            stack.push(SeqFrame { node: root, next: 0 });
        }
        SeqIter { stack, root: self.root.as_deref(), index: 0, back: self.len }
    }

    fn collapse(&mut self) {
        loop {
            let next = match self.root.as_deref() {
                Some(SeqNode::Leaf(leaf)) if leaf.records.is_empty() => {
                    self.root = None;
                    return;
                }
                Some(SeqNode::Branch(branch)) if branch.children.len() <= 1 => {
                    branch.children.first().cloned()
                }
                _ => return,
            };
            self.root = next;
        }
    }
}

fn seq_count<T>(node: &SeqNode<T>) -> usize {
    match node {
        SeqNode::Leaf(leaf) => leaf.records.len(),
        SeqNode::Branch(branch) => branch.lens.iter().copied().sum(),
    }
}

fn seq_occupancy<T>(node: &SeqNode<T>) -> usize {
    match node {
        SeqNode::Leaf(leaf) => leaf.records.len(),
        SeqNode::Branch(branch) => branch.children.len(),
    }
}

fn rebuild_lens<T>(branch: &mut SeqBranch<T>) {
    branch.lens.clear();
    branch.lens.extend(branch.children.iter().map(|child| seq_count(child)));
}

fn find_child<T>(branch: &SeqBranch<T>, index: usize) -> (usize, usize) {
    let mut offset = 0;
    for (i, len) in branch.lens.iter().copied().enumerate() {
        let last = i + 1 == branch.lens.len();
        if index < offset + len || (last && index == offset + len) {
            return (i, index - offset);
        }
        offset += len;
    }
    (0, 0)
}

fn seq_get<T>(node: &SeqNode<T>, index: usize) -> Option<&T> {
    match node {
        SeqNode::Leaf(leaf) => leaf.records.get(index).map(Arc::as_ref),
        SeqNode::Branch(branch) => {
            let (i, local) = find_child(branch, index);
            let child = branch.children.get(i)?;
            seq_get(child, local)
        }
    }
}

fn seq_get_mut<T: Clone>(node: &mut Arc<SeqNode<T>>, index: usize) -> Option<&mut T> {
    match Arc::make_mut(node) {
        SeqNode::Leaf(leaf) => leaf.records.get_mut(index).map(Arc::make_mut),
        SeqNode::Branch(branch) => {
            let (i, local) = find_child(branch, index);
            seq_get_mut(&mut branch.children[i], local)
        }
    }
}

fn seq_insert<T>(node: &mut Arc<SeqNode<T>>, index: usize, value: Arc<T>) -> Option<Arc<SeqNode<T>>> {
    match Arc::make_mut(node) {
        SeqNode::Leaf(leaf) => {
            leaf.records.insert(index, value);
            if leaf.records.len() <= FANOUT {
                None
            } else {
                let mid = leaf.records.len() / 2;
                let records = leaf.records.split_off(mid);
                Some(Arc::new(SeqNode::Leaf(SeqLeaf { records })))
            }
        }
        SeqNode::Branch(branch) => {
            let (i, local) = find_child(branch, index);
            let right = seq_insert(&mut branch.children[i], local, value);
            // 只有命中的子树增加一条记录；分裂时把其原长度分成两份。
            // 其余子树长度没有变化，避免每次 push 扫描整层。
            branch.lens[i] += 1;
            if let Some(right) = right {
                let right_len = seq_count(&right);
                branch.lens[i] -= right_len;
                branch.lens.insert(i + 1, right_len);
                branch.children.insert(i + 1, right);
            }
            if branch.children.len() <= FANOUT {
                None
            } else {
                let mid = branch.children.len() / 2;
                let children = branch.children.split_off(mid);
                let lens = branch.lens.split_off(mid);
                Some(Arc::new(SeqNode::Branch(SeqBranch { children, lens })))
            }
        }
    }
}

fn seq_remove<T>(node: &mut Arc<SeqNode<T>>, index: usize) -> Option<Arc<T>> {
    match Arc::make_mut(node) {
        SeqNode::Leaf(leaf) => {
            if index >= leaf.records.len() {
                None
            } else {
                Some(leaf.records.remove(index))
            }
        }
        SeqNode::Branch(branch) => {
            let (i, local) = find_child(branch, index);
            let old = seq_remove(&mut branch.children[i], local)?;
            branch.lens[i] -= 1;
            if branch.lens[i] == 0 {
                branch.children.remove(i);
                branch.lens.remove(i);
            } else if seq_occupancy(&branch.children[i]) < MIN_OCCUPANCY {
                seq_rebalance(branch, i);
                rebuild_lens(branch);
            }
            Some(old)
        }
    }
}

fn seq_rebalance<T>(branch: &mut SeqBranch<T>, i: usize) {
    if i > 0 && seq_occupancy(&branch.children[i - 1]) > MIN_OCCUPANCY {
        seq_steal_left(branch, i);
    } else if i + 1 < branch.children.len()
        && seq_occupancy(&branch.children[i + 1]) > MIN_OCCUPANCY
    {
        seq_steal_right(branch, i);
    } else if i > 0 {
        seq_merge_left(branch, i);
    } else if i + 1 < branch.children.len() {
        seq_merge_right(branch, i);
    }
}

fn seq_steal_left<T>(branch: &mut SeqBranch<T>, i: usize) {
    let moved = {
        let left = Arc::make_mut(&mut branch.children[i - 1]);
        match left {
            SeqNode::Leaf(leaf) => SeqMoved::Item(leaf.records.pop().expect("左叶有记录")),
            SeqNode::Branch(child) => {
                let sub = child.children.pop().expect("左枝有子节点");
                rebuild_lens(child);
                SeqMoved::Sub(sub)
            }
        }
    };
    let right = Arc::make_mut(&mut branch.children[i]);
    match (right, moved) {
        (SeqNode::Leaf(leaf), SeqMoved::Item(record)) => {
            leaf.records.insert(0, record);
        }
        (SeqNode::Branch(child), SeqMoved::Sub(sub)) => {
            child.children.insert(0, sub);
            rebuild_lens(child);
        }
        _ => unreachable!("同一层节点种类一致"),
    }
}

fn seq_steal_right<T>(branch: &mut SeqBranch<T>, i: usize) {
    let moved = {
        let right = Arc::make_mut(&mut branch.children[i + 1]);
        match right {
            SeqNode::Leaf(leaf) => SeqMoved::Item(leaf.records.remove(0)),
            SeqNode::Branch(child) => {
                let sub = child.children.remove(0);
                rebuild_lens(child);
                SeqMoved::Sub(sub)
            }
        }
    };
    let left = Arc::make_mut(&mut branch.children[i]);
    match (left, moved) {
        (SeqNode::Leaf(leaf), SeqMoved::Item(record)) => leaf.records.push(record),
        (SeqNode::Branch(child), SeqMoved::Sub(sub)) => {
            child.children.push(sub);
            rebuild_lens(child);
        }
        _ => unreachable!("同一层节点种类一致"),
    }
}

fn seq_merge_left<T>(branch: &mut SeqBranch<T>, i: usize) {
    let right = branch.children.remove(i);
    let left = Arc::make_mut(&mut branch.children[i - 1]);
    seq_append(left, right.as_ref());
}

fn seq_merge_right<T>(branch: &mut SeqBranch<T>, i: usize) {
    let right = branch.children.remove(i + 1);
    let left = Arc::make_mut(&mut branch.children[i]);
    seq_append(left, right.as_ref());
}

fn seq_append<T>(left: &mut SeqNode<T>, right: &SeqNode<T>) {
    match (left, right) {
        (SeqNode::Leaf(dst), SeqNode::Leaf(src)) => {
            dst.records.extend(src.records.iter().cloned());
        }
        (SeqNode::Branch(dst), SeqNode::Branch(src)) => {
            dst.children.extend(src.children.iter().cloned());
            rebuild_lens(dst);
        }
        _ => unreachable!("同一层节点种类一致"),
    }
}

struct SeqFrame<'a, T> {
    node: &'a SeqNode<T>,
    next: usize,
}

pub struct SeqIter<'a, T> {
    stack: Vec<SeqFrame<'a, T>>,
    root: Option<&'a SeqNode<T>>,
    index: usize,
    back: usize,
}

impl<'a, T> Iterator for SeqIter<'a, T> {
    type Item = &'a T;

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.back.saturating_sub(self.index);
        (left, Some(left))
    }

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.back {
            return None;
        }
        loop {
            let frame = self.stack.last_mut()?;
            match frame.node {
                SeqNode::Leaf(leaf) => {
                    if frame.next >= leaf.records.len() {
                        self.stack.pop();
                        continue;
                    }
                    let i = frame.next;
                    frame.next += 1;
                    self.index += 1;
                    return Some(leaf.records[i].as_ref());
                }
                SeqNode::Branch(branch) => {
                    if frame.next >= branch.children.len() {
                        self.stack.pop();
                        continue;
                    }
                    let child = branch.children[frame.next].as_ref();
                    frame.next += 1;
                    self.stack.push(SeqFrame { node: child, next: 0 });
                }
            }
        }
    }
}

impl<'a, T> DoubleEndedIterator for SeqIter<'a, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.index >= self.back {
            return None;
        }
        self.back -= 1;
        self.root.and_then(|root| seq_index(root, self.back))
    }
}

fn seq_index<T>(node: &SeqNode<T>, mut index: usize) -> Option<&T> {
    match node {
        SeqNode::Leaf(leaf) => leaf.records.get(index).map(|record| record.as_ref()),
        SeqNode::Branch(branch) => {
            for (child, len) in branch.children.iter().zip(branch.lens.iter()) {
                if index < *len {
                    return seq_index(child, index);
                }
                index -= *len;
            }
            None
        }
    }
}

impl<'a, T> ExactSizeIterator for SeqIter<'a, T> {}

fn owned_record<T: Clone>(arc: Arc<T>) -> T {
    Arc::try_unwrap(arc).unwrap_or_else(|arc| (*arc).clone())
}

impl<K: Ord + Clone, V> CowMap<K, V> {
    /// Build balanced nodes bottom-up from sorted, unique keys without
    /// inserting each key separately. Only the root may be below half full.
    /// The caller must supply strictly increasing keys.
    pub(crate) fn from_sorted_unique(entries: Vec<(K, V)>) -> Self {
        debug_assert!(entries.windows(2).all(|pair| pair[0].0 < pair[1].0));
        let len = entries.len();
        if len == 0 { return Self::new(); }
        let mut entries = entries.into_iter();
        let groups = len.div_ceil(FANOUT);
        let mut level = Vec::with_capacity(groups);
        for i in 0..groups {
            let count = len / groups + usize::from(i < len % groups);
            let mut keys = Vec::with_capacity(count);
            let mut records = Vec::with_capacity(count);
            for (key, value) in entries.by_ref().take(count) {
                keys.push(key);
                records.push(Arc::new(value));
            }
            level.push(Arc::new(Node::Leaf(Leaf { keys, records })));
        }
        while level.len() > 1 {
            let count = level.len();
            let groups = count.div_ceil(FANOUT);
            let mut children = level.into_iter();
            let mut next = Vec::with_capacity(groups);
            for i in 0..groups {
                let size = count / groups + usize::from(i < count % groups);
                let mut branch = Branch {
                    children: children.by_ref().take(size).collect(),
                    keys: Vec::new(),
                };
                recompute_seps(&mut branch);
                next.push(Arc::new(Node::Branch(branch)));
            }
            level = next;
        }
        Self { root: level.pop(), len }
    }
}

impl<K, V> CowMap<K, V> {
    /// Persistent nodes allocate locally; there is no contiguous capacity to reserve.
    pub(crate) fn reserve(&mut self, _additional: usize) {}

    pub fn keys(&self) -> Keys<'_, K, V>
    where
        K: Ord,
    {
        Keys { forward: Some(self.iter()), buf: Vec::new(), lo: 0, hi: 0 }
    }

    pub fn values(&self) -> impl Iterator<Item = &V> + DoubleEndedIterator + '_
    where
        K: Ord + Clone,
    {
        self.iter().map(|(_, value)| value)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> + '_
    where
        K: Ord + Clone,
        V: Clone,
    {
        self.iter_mut().map(|(_, value)| value)
    }

    pub fn iter_mut(&mut self) -> IterMut<'_, K, V>
    where
        K: Ord + Clone,
        V: Clone,
    {
        if let Some(root) = self.root.as_mut() {
            uniquify(root);
        }
        let mut slots = Vec::new();
        if let Some(root) = self.root.as_mut() {
            collect_map_slots(Arc::get_mut(root).expect("根节点已独占"), &mut slots);
        }
        IterMut { slots, pos: 0, _marker: PhantomData }
    }

    pub fn first_key_value(&self) -> Option<(&K, &V)>
    where
        K: Ord,
    {
        self.iter().next()
    }

    pub fn last_key_value(&self) -> Option<(&K, &V)>
    where
        K: Ord,
    {
        self.root.as_deref().map(rightmost)
    }

    pub fn pop_first(&mut self) -> Option<(K, V)>
    where
        K: Ord + Clone,
        V: Clone,
    {
        let key = self.first_key_value()?.0.clone();
        let value = self.remove(&key)?;
        Some((key, value))
    }

    pub fn pop_last(&mut self) -> Option<(K, V)>
    where
        K: Ord + Clone,
        V: Clone,
    {
        let key = self.last_key_value()?.0.clone();
        let value = self.remove(&key)?;
        Some((key, value))
    }

    pub fn entry(&mut self, key: K) -> Entry<'_, K, V>
    where
        K: Ord,
    {
        if self.contains_key(&key) {
            Entry::Occupied(Occupied { map: self, key })
        } else {
            Entry::Vacant(Vacant { map: self, key })
        }
    }

    pub fn append(&mut self, other: &mut Self)
    where
        K: Ord + Clone,
        V: Clone,
    {
        let taken = std::mem::take(other);
        self.extend(taken);
    }

    pub fn retain<F>(&mut self, mut keep: F)
    where
        K: Ord + Clone,
        V: Clone,
        F: FnMut(&K, &mut V) -> bool,
    {
        let keys: Vec<K> = self.keys().cloned().collect();
        let mut dropped = Vec::new();
        for key in &keys {
            let stay = {
                let value = self.get_mut(key).expect("键仍在表里");
                keep(key, value)
            };
            if !stay {
                dropped.push(key.clone());
            }
        }
        for key in dropped {
            self.remove(&key);
        }
    }
}

pub enum Entry<'a, K, V> {
    Occupied(Occupied<'a, K, V>),
    Vacant(Vacant<'a, K, V>),
}

pub struct Occupied<'a, K, V> {
    map: &'a mut CowMap<K, V>,
    key: K,
}

pub struct Vacant<'a, K, V> {
    map: &'a mut CowMap<K, V>,
    key: K,
}

impl<'a, K, V> Entry<'a, K, V> {
    pub fn or_insert(self, default: V) -> &'a mut V
    where
        K: Ord + Clone,
        V: Clone,
    {
        match self {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(default),
        }
    }

    pub fn or_insert_with<F>(self, default: F) -> &'a mut V
    where
        K: Ord + Clone,
        V: Clone,
        F: FnOnce() -> V,
    {
        match self {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(default()),
        }
    }

    pub fn or_default(self) -> &'a mut V
    where
        K: Ord + Clone,
        V: Clone + Default,
    {
        self.or_insert_with(V::default)
    }

    pub fn and_modify<F>(self, change: F) -> Self
    where
        K: Ord + Clone,
        V: Clone,
        F: FnOnce(&mut V),
    {
        match self {
            Entry::Occupied(mut entry) => {
                change(entry.get_mut());
                Entry::Occupied(entry)
            }
            vacant => vacant,
        }
    }

    pub fn key(&self) -> &K {
        match self {
            Entry::Occupied(entry) => entry.key(),
            Entry::Vacant(entry) => entry.key(),
        }
    }
}

impl<'a, K, V> Occupied<'a, K, V> {
    pub fn key(&self) -> &K {
        &self.key
    }

    pub fn get(&self) -> &V
    where
        K: Ord,
    {
        self.map.get(&self.key).expect("占用项仍在表里")
    }

    pub fn get_mut(&mut self) -> &mut V
    where
        K: Ord + Clone,
        V: Clone,
    {
        self.map.get_mut(&self.key).expect("占用项仍在表里")
    }

    pub fn into_mut(self) -> &'a mut V
    where
        K: Ord + Clone,
        V: Clone,
    {
        let key = self.key;
        self.map.get_mut(&key).expect("占用项仍在表里")
    }

    pub fn insert(&mut self, value: V) -> V
    where
        K: Ord + Clone,
        V: Clone,
    {
        self.map.insert(self.key.clone(), value).expect("占用项仍在表里")
    }

    pub fn remove(self) -> V
    where
        K: Ord + Clone,
        V: Clone,
    {
        self.map.remove(&self.key).expect("占用项仍在表里")
    }
}

impl<'a, K, V> Vacant<'a, K, V> {
    pub fn key(&self) -> &K {
        &self.key
    }

    pub fn insert(self, value: V) -> &'a mut V
    where
        K: Ord + Clone,
        V: Clone,
    {
        let key = self.key.clone();
        self.map.insert(self.key, value);
        self.map.get_mut(&key).expect("刚插入的键")
    }
}

pub struct Keys<'a, K, V> {
    forward: Option<Iter<'a, K, V>>,
    buf: Vec<&'a K>,
    lo: usize,
    hi: usize,
}

impl<'a, K: Ord + Clone, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(iter) = self.forward.as_mut() {
            if let Some((key, _)) = iter.next() {
                return Some(key);
            }
            self.forward = None;
        }
        if self.lo < self.hi {
            let key = self.buf[self.lo];
            self.lo += 1;
            Some(key)
        } else {
            None
        }
    }
}

impl<'a, K: Ord + Clone, V> DoubleEndedIterator for Keys<'a, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if let Some(iter) = self.forward.take() {
            self.buf.extend(iter.map(|(key, _)| key));
            self.lo = 0;
            self.hi = self.buf.len();
        }
        if self.lo < self.hi {
            self.hi -= 1;
            Some(self.buf[self.hi])
        } else {
            None
        }
    }
}

impl<'a, K: Ord + Clone, V: Clone> IntoIterator for &'a mut CowMap<K, V> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<'a, K: Ord, V> IntoIterator for &'a CowMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<K: Ord + Clone, V: Clone> IntoIterator for CowMap<K, V> {
    type Item = (K, V);
    type IntoIter = std::vec::IntoIter<(K, V)>;

    fn into_iter(self) -> Self::IntoIter {
        let mut items = Vec::with_capacity(self.len);
        if let Some(root) = self.root {
            own_node(root, &mut items);
        }
        items.into_iter()
    }
}

impl<K: Ord + Clone, V> Extend<(K, V)> for CowMap<K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (key, value) in iter {
            self.insert_arc(key, value);
        }
    }
}

impl<K: Ord + Clone, V> FromIterator<(K, V)> for CowMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut map = Self::new();
        map.extend(iter);
        map
    }
}

impl<K: Ord + Clone, V> From<BTreeMap<K, V>> for CowMap<K, V> {
    fn from(map: BTreeMap<K, V>) -> Self {
        map.into_iter().collect()
    }
}

impl<K: Ord, V: PartialEq> PartialEq for CowMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl<K: Ord, V: Eq> Eq for CowMap<K, V> {}

impl<K: Ord + Debug, V: Debug> Debug for CowMap<K, V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_map().entries(self.iter()).finish()
    }
}

#[cfg(any(feature = "serde", feature = "hy3d-persist-serde"))]
impl<K: Serialize + Ord, V: Serialize> Serialize for CowMap<K, V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

#[cfg(any(feature = "serde", feature = "hy3d-persist-serde"))]
impl<'de, K, V> Deserialize<'de> for CowMap<K, V>
where
    K: Deserialize<'de> + Ord + Clone,
    V: Deserialize<'de>,
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(BTreeMap::<K, V>::deserialize(deserializer)?.into())
    }
}

impl<K: Ord, V> Index<&K> for CowMap<K, V> {
    type Output = V;

    fn index(&self, key: &K) -> &V {
        self.get(key).expect("键不存在")
    }
}

impl<K: Ord + Clone, V: Clone> IndexMut<&K> for CowMap<K, V> {
    fn index_mut(&mut self, key: &K) -> &mut V {
        self.get_mut(key).expect("键不存在")
    }
}

struct MapSlot<K, V> {
    key: *const K,
    record: *mut Arc<V>,
}

pub struct IterMut<'a, K, V> {
    slots: Vec<MapSlot<K, V>>,
    pos: usize,
    _marker: PhantomData<&'a mut CowMap<K, V>>,
}

impl<'a, K, V: Clone> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    fn next(&mut self) -> Option<Self::Item> {
        let slot = self.slots.get(self.pos)?;
        self.pos += 1;
        // 节点在迭代前已经独占，遍历不再改树形。每条记录单独 make_mut，先前交出的可变引用仍指向自己的分配。
        unsafe {
            let key = &*slot.key;
            let record = &mut *slot.record;
            Some((key, Arc::make_mut(record)))
        }
    }
}

fn uniquify<K: Clone, V>(node: &mut Arc<Node<K, V>>) {
    if let Node::Branch(branch) = Arc::make_mut(node) {
        for child in &mut branch.children {
            uniquify(child);
        }
    }
}

fn collect_map_slots<K, V>(node: &mut Node<K, V>, out: &mut Vec<MapSlot<K, V>>) {
    match node {
        Node::Leaf(leaf) => {
            for index in 0..leaf.keys.len() {
                out.push(MapSlot {
                    key: &leaf.keys[index],
                    record: &mut leaf.records[index],
                });
            }
        }
        Node::Branch(branch) => {
            for child in &mut branch.children {
                collect_map_slots(Arc::get_mut(child).expect("节点已独占"), out);
            }
        }
    }
}

fn rightmost<'a, K, V>(node: &'a Node<K, V>) -> (&'a K, &'a V) {
    match node {
        Node::Leaf(leaf) => {
            let index = leaf.keys.len() - 1;
            (&leaf.keys[index], leaf.records[index].as_ref())
        }
        Node::Branch(branch) => rightmost(branch.children.last().expect("枝节点有子节点")),
    }
}

fn own_node<K: Clone, V: Clone>(node: Arc<Node<K, V>>, out: &mut Vec<(K, V)>) {
    match Arc::try_unwrap(node) {
        Ok(Node::Leaf(leaf)) => {
            for (key, record) in leaf.keys.into_iter().zip(leaf.records) {
                out.push((key, owned_record(record)));
            }
        }
        Ok(Node::Branch(branch)) => {
            for child in branch.children {
                own_node(child, out);
            }
        }
        Err(shared) => own_shared(shared.as_ref(), out),
    }
}

fn own_shared<K: Clone, V: Clone>(node: &Node<K, V>, out: &mut Vec<(K, V)>) {
    match node {
        Node::Leaf(leaf) => {
            for (key, record) in leaf.keys.iter().zip(leaf.records.iter()) {
                out.push((key.clone(), (*record.as_ref()).clone()));
            }
        }
        Node::Branch(branch) => {
            for child in &branch.children {
                own_shared(child, out);
            }
        }
    }
}

impl<T> CowSeq<T> {
    pub fn reserve(&mut self, _additional: usize) {}

    pub fn reserve_exact(&mut self, _additional: usize) {}

    pub fn first(&self) -> Option<&T> {
        self.get(0)
    }

    pub fn last(&self) -> Option<&T> {
        self.len.checked_sub(1).and_then(|index| self.get(index))
    }

    pub fn first_mut(&mut self) -> Option<&mut T>
    where
        T: Clone,
    {
        self.get_mut(0)
    }

    pub fn last_mut(&mut self) -> Option<&mut T>
    where
        T: Clone,
    {
        let index = self.len.checked_sub(1)?;
        self.get_mut(index)
    }

    pub fn iter_mut(&mut self) -> SeqIterMut<'_, T>
    where
        T: Clone,
    {
        if let Some(root) = self.root.as_mut() {
            uniquify_seq(root);
        }
        let mut slots = Vec::new();
        if let Some(root) = self.root.as_mut() {
            collect_seq_slots(Arc::get_mut(root).expect("根节点已独占"), &mut slots);
        }
        SeqIterMut { slots, pos: 0, _marker: PhantomData }
    }

    pub fn truncate(&mut self, len: usize)
    where
        T: Clone,
    {
        while self.len > len {
            self.pop();
        }
    }

    pub fn retain<F>(&mut self, mut keep: F)
    where
        T: Clone,
        F: FnMut(&T) -> bool,
    {
        let old = std::mem::take(self);
        for record in old.into_arcs() {
            if keep(record.as_ref()) {
                self.push_arc(record);
            }
        }
    }

    pub fn retain_mut<F>(&mut self, mut keep: F)
    where
        T: Clone,
        F: FnMut(&mut T) -> bool,
    {
        let old = std::mem::take(self);
        for mut record in old.into_arcs() {
            if keep(Arc::make_mut(&mut record)) {
                self.push_arc(record);
            }
        }
    }

    fn into_arcs(self) -> Vec<Arc<T>> {
        let mut out = Vec::with_capacity(self.len);
        if let Some(root) = self.root {
            seq_arcs(root, &mut out);
        }
        out
    }

    fn push_arc(&mut self, value: Arc<T>) {
        let index = self.len;
        self.insert_arc(index, value);
    }

    fn insert_arc(&mut self, index: usize, value: Arc<T>) {
        assert!(index <= self.len, "持久化序列下标越界");
        if self.root.is_none() {
            self.root = Some(Arc::new(SeqNode::Leaf(SeqLeaf { records: vec![value] })));
            self.len = 1;
            return;
        }
        let root = self.root.as_mut().unwrap();
        if let Some(right) = seq_insert(root, index, value) {
            let left = Arc::clone(self.root.as_ref().unwrap());
            let mut branch = SeqBranch {
                children: vec![left, right],
                lens: Vec::new(),
            };
            rebuild_lens(&mut branch);
            self.root = Some(Arc::new(SeqNode::Branch(branch)));
        }
        self.len += 1;
    }
}

impl<'a, T: Clone> IntoIterator for &'a mut CowSeq<T> {
    type Item = &'a mut T;
    type IntoIter = SeqIterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<'a, T> IntoIterator for &'a CowSeq<T> {
    type Item = &'a T;
    type IntoIter = SeqIter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<T: Clone> IntoIterator for CowSeq<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.into_arcs().into_iter().map(owned_record).collect::<Vec<_>>().into_iter()
    }
}

impl<T> Extend<T> for CowSeq<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for value in iter {
            self.push(value);
        }
    }
}

impl<T> FromIterator<T> for CowSeq<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut seq = Self::new();
        seq.extend(iter);
        seq
    }
}

impl<T> From<Vec<T>> for CowSeq<T> {
    fn from(values: Vec<T>) -> Self {
        values.into_iter().collect()
    }
}

impl<T: PartialEq> PartialEq for CowSeq<T> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl<T: Eq> Eq for CowSeq<T> {}

impl<T: Debug> Debug for CowSeq<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(any(feature = "serde", feature = "hy3d-persist-serde"))]
impl<T: Serialize> Serialize for CowSeq<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

#[cfg(any(feature = "serde", feature = "hy3d-persist-serde"))]
impl<'de, T: Deserialize<'de>> Deserialize<'de> for CowSeq<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Vec::<T>::deserialize(deserializer)?.into())
    }
}

impl<T> Index<usize> for CowSeq<T> {
    type Output = T;

    fn index(&self, index: usize) -> &T {
        self.get(index).expect("下标越界")
    }
}

impl<T: Clone> IndexMut<usize> for CowSeq<T> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        self.get_mut(index).expect("下标越界")
    }
}

struct SeqSlot<T> {
    record: *mut Arc<T>,
}

pub struct SeqIterMut<'a, T> {
    slots: Vec<SeqSlot<T>>,
    pos: usize,
    _marker: PhantomData<&'a mut CowSeq<T>>,
}

impl<'a, T: Clone> Iterator for SeqIterMut<'a, T> {
    type Item = &'a mut T;

    fn next(&mut self) -> Option<Self::Item> {
        let slot = self.slots.get(self.pos)?;
        self.pos += 1;
        unsafe { Some(Arc::make_mut(&mut *slot.record)) }
    }
}

fn uniquify_seq<T>(node: &mut Arc<SeqNode<T>>) {
    if let SeqNode::Branch(branch) = Arc::make_mut(node) {
        for child in &mut branch.children {
            uniquify_seq(child);
        }
    }
}

fn collect_seq_slots<T>(node: &mut SeqNode<T>, out: &mut Vec<SeqSlot<T>>) {
    match node {
        SeqNode::Leaf(leaf) => {
            for record in &mut leaf.records {
                out.push(SeqSlot { record });
            }
        }
        SeqNode::Branch(branch) => {
            for child in &mut branch.children {
                collect_seq_slots(Arc::get_mut(child).expect("节点已独占"), out);
            }
        }
    }
}

fn seq_arcs<T>(node: Arc<SeqNode<T>>, out: &mut Vec<Arc<T>>) {
    match Arc::try_unwrap(node) {
        Ok(SeqNode::Leaf(leaf)) => out.extend(leaf.records),
        Ok(SeqNode::Branch(branch)) => {
            for child in branch.children {
                seq_arcs(child, out);
            }
        }
        Err(shared) => seq_arcs_shared(shared.as_ref(), out),
    }
}

fn seq_arcs_shared<T>(node: &SeqNode<T>, out: &mut Vec<Arc<T>>) {
    match node {
        SeqNode::Leaf(leaf) => out.extend(leaf.records.iter().cloned()),
        SeqNode::Branch(branch) => {
            for child in &branch.children {
                seq_arcs_shared(child, out);
            }
        }
    }
}


// Native CAD adapter: borrow existing Arc records rather than cloning an
// EntityType (which deliberately discards its original encoding).
impl<T> CowSeq<T> {
    pub(crate) fn shared_at(&self, index: usize) -> Option<&Arc<T>> {
        seq_shared_at(self.root.as_deref()?, index)
    }

    pub(crate) fn shared_at_mut(&mut self, index: usize) -> Option<&mut Arc<T>> {
        if index >= self.len { return None; }
        seq_shared_at_mut(self.root.as_mut()?, index)
    }

    pub(crate) fn push_shared(&mut self, value: Arc<T>) {
        self.push_arc(value);
    }

    pub(crate) fn swap_remove_shared(&mut self, index: usize) -> Arc<T> {
        assert!(index < self.len, "entity slot out of range");
        let last = self.len - 1;
        if index == last {
            return self.remove_arc(last).expect("last slot exists");
        }
        let replacement = self.remove_arc(last).expect("last slot exists");
        std::mem::replace(self.shared_at_mut(index).expect("slot exists"), replacement)
    }

    /// Build a balanced tree in linear time when an empty store adopts a
    /// loader's Vec of native Arcs. Every non-root group is at least half full.
    pub(crate) fn from_shared_vec(records: Vec<Arc<T>>) -> Self {
        let len = records.len();
        if len == 0 { return Self::new(); }
        let mut records = records.into_iter();
        let groups = len.div_ceil(FANOUT);
        let mut level = Vec::with_capacity(groups);
        for i in 0..groups {
            let count = len / groups + usize::from(i < len % groups);
            level.push(Arc::new(SeqNode::Leaf(SeqLeaf {
                records: records.by_ref().take(count).collect(),
            })));
        }
        while level.len() > 1 {
            let count = level.len();
            let groups = count.div_ceil(FANOUT);
            let mut children = level.into_iter();
            let mut next = Vec::with_capacity(groups);
            for i in 0..groups {
                let size = count / groups + usize::from(i < count % groups);
                let mut branch = SeqBranch {
                    children: children.by_ref().take(size).collect(),
                    lens: Vec::new(),
                };
                rebuild_lens(&mut branch);
                next.push(Arc::new(SeqNode::Branch(branch)));
            }
            level = next;
        }
        Self { root: level.pop(), len }
    }

    pub(crate) fn shared_iter(&self) -> SeqSharedIter<'_, T> {
        let mut stack = Vec::new();
        if let Some(root) = self.root.as_deref() {
            stack.push(SeqFrame { node: root, next: 0 });
        }
        SeqSharedIter { stack, root: self.root.as_deref(), index: 0, back: self.len }
    }

    pub(crate) fn shared_iter_mut(&mut self) -> std::vec::IntoIter<&mut Arc<T>> {
        let mut out = Vec::with_capacity(self.len);
        if let Some(root) = &mut self.root {
            collect_shared_slots(root, &mut out);
        }
        out.into_iter()
    }
}

fn seq_shared_at<T>(node: &SeqNode<T>, mut index: usize) -> Option<&Arc<T>> {
    match node {
        SeqNode::Leaf(leaf) => leaf.records.get(index),
        SeqNode::Branch(branch) => {
            for (child, len) in branch.children.iter().zip(&branch.lens) {
                if index < *len { return seq_shared_at(child, index); }
                index -= *len;
            }
            None
        }
    }
}

fn seq_shared_at_mut<T>(node: &mut Arc<SeqNode<T>>, mut index: usize) -> Option<&mut Arc<T>> {
    match Arc::make_mut(node) {
        SeqNode::Leaf(leaf) => leaf.records.get_mut(index),
        SeqNode::Branch(branch) => {
            for (child, len) in branch.children.iter_mut().zip(&branch.lens) {
                if index < *len { return seq_shared_at_mut(child, index); }
                index -= *len;
            }
            None
        }
    }
}

fn collect_shared_slots<'a, T>(node: &'a mut Arc<SeqNode<T>>, out: &mut Vec<&'a mut Arc<T>>) {
    match Arc::make_mut(node) {
        SeqNode::Leaf(leaf) => out.extend(leaf.records.iter_mut()),
        SeqNode::Branch(branch) => {
            for child in &mut branch.children { collect_shared_slots(child, out); }
        }
    }
}

pub(crate) struct SeqSharedIter<'a, T> {
    stack: Vec<SeqFrame<'a, T>>,
    root: Option<&'a SeqNode<T>>,
    index: usize,
    back: usize,
}

impl<'a, T> Iterator for SeqSharedIter<'a, T> {
    type Item = &'a Arc<T>;
    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.back.saturating_sub(self.index);
        (left, Some(left))
    }
    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.back { return None; }
        loop {
            let frame = self.stack.last_mut()?;
            match frame.node {
                SeqNode::Leaf(leaf) => {
                    if frame.next >= leaf.records.len() {
                        self.stack.pop();
                        continue;
                    }
                    let i = frame.next;
                    frame.next += 1;
                    self.index += 1;
                    return Some(&leaf.records[i]);
                }
                SeqNode::Branch(branch) => {
                    if frame.next >= branch.children.len() {
                        self.stack.pop();
                        continue;
                    }
                    let child = branch.children[frame.next].as_ref();
                    frame.next += 1;
                    self.stack.push(SeqFrame { node: child, next: 0 });
                }
            }
        }
    }
}

impl<'a, T> DoubleEndedIterator for SeqSharedIter<'a, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.index >= self.back { return None; }
        self.back -= 1;
        self.root.and_then(|root| seq_shared_at(root, self.back))
    }
}
impl<T> ExactSizeIterator for SeqSharedIter<'_, T> {}

