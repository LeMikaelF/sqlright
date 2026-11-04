//! The SQLRight intermediate representation (IR).
//!
//! A SQL query is a binary tree of [`IrNode`]s. Each node comes from one
//! grammar rule and has an [`IrType`] named after that rule. A node prints
//! itself as `prefix left middle right suffix`, where `prefix`, `middle` and
//! `suffix` are the keywords of the rule (the [`Op`]). Leaf nodes hold an
//! identifier or a literal in `str_val`.
//!
//! Identifier nodes also carry a [`DataType`] (table name, column name, ...)
//! and a [`DataFlag`] (the statement defines, uses or removes that name). The
//! mutator uses them to fill in names that fit the statements before them.
//!
//! The nodes of one tree live in an [`IrTree`] arena and point to each other
//! with [`IrId`]s. This replaces the raw pointers of the C++ SQLRight. A node
//! that is removed from the tree stays in the arena until the tree is dropped.

pub mod generated {
    pub mod ir_type;
}
pub mod util;

use std::borrow::Cow;

pub use generated::ir_type::{DataType, IrType};

/// How a statement relates to the name in an identifier node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum DataFlag {
    Unknown = 0x0,
    Define = 0x1,
    Undefine = 0x2,
    Global = 0x4,
    Use = 0x8,
    MapToClosestOne = 0x10,
    MapToAll = 0x20,
    Replace = 0x40,
    Alias = 0x80,
    NoSplit = 0x100,
    NoModi = 0x200,
}

impl DataFlag {
    /// The C++ name, as the debug output prints it.
    pub fn as_str(self) -> &'static str {
        match self {
            DataFlag::Use => "kUse",
            DataFlag::MapToClosestOne => "kMapToClosestOne",
            DataFlag::NoSplit => "kNoSplit",
            DataFlag::Global => "kGlobal",
            DataFlag::Replace => "kReplace",
            DataFlag::Undefine => "kUndefine",
            DataFlag::Alias => "kAlias",
            DataFlag::MapToAll => "kMapToAll",
            DataFlag::Define => "kDefine",
            DataFlag::NoModi | DataFlag::Unknown => "kUnknown",
        }
    }
}

/// The kind of option a `reloption` node holds, so the mutator can pick a
/// matching option name and value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum RelOptionType {
    #[default]
    Unknown = 0,
    StorageParameters = 1,
    SetConfigurationOptions = 2,
    AlterAttribute = 3,
    AlterAttributeReset = 4,
}

/// The column type a `Typename` node describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColType {
    Unknown,
    Int,
    Float,
    Boolean,
    String,
}

/// The fixed text of a node: `prefix left middle right suffix`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Op {
    pub prefix: Cow<'static, str>,
    pub middle: Cow<'static, str>,
    pub suffix: Cow<'static, str>,
}

impl Op {
    pub const fn new(prefix: &'static str, middle: &'static str, suffix: &'static str) -> Op {
        Op {
            prefix: Cow::Borrowed(prefix),
            middle: Cow::Borrowed(middle),
            suffix: Cow::Borrowed(suffix),
        }
    }

    pub const fn empty() -> Op {
        Op::new("", "", "")
    }

    pub const fn mid(middle: &'static str) -> Op {
        Op::new("", middle, "")
    }

    pub fn owned(prefix: impl Into<String>, middle: impl Into<String>, suffix: impl Into<String>) -> Op {
        Op {
            prefix: Cow::Owned(prefix.into()),
            middle: Cow::Owned(middle.into()),
            suffix: Cow::Owned(suffix.into()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.prefix.is_empty() && self.middle.is_empty() && self.suffix.is_empty()
    }
}

/// Index of a node in an [`IrTree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IrId(pub u32);

impl IrId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// The numeric value of a literal node. Literals that the parser builds keep
/// their text in `str_val` and have no numeric value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IrValue {
    None,
    Int(i64),
    Float(f64),
    Bool(bool),
}

/// One node of the IR tree.
#[derive(Debug, Clone)]
pub struct IrNode {
    pub ty: IrType,
    pub op: Option<Op>,
    pub left: Option<IrId>,
    pub right: Option<IrId>,
    pub parent: Option<IrId>,
    pub str_val: String,
    pub value: IrValue,
    pub data_type: DataType,
    pub data_flag: DataFlag,
    pub scope: i32,
    pub option_type: RelOptionType,
    pub mutated_times: u32,
    /// The oracle set this to keep the mutator from changing the node.
    pub is_node_struct_fixed: bool,
    /// Position of the node in the pre-order list of its tree, or -1.
    pub uniq_id_in_tree: i32,
    /// False after `deep_drop`. A dropped node is never part of the tree.
    pub alive: bool,
}

impl IrNode {
    fn new(ty: IrType) -> IrNode {
        IrNode {
            ty,
            op: None,
            left: None,
            right: None,
            parent: None,
            str_val: String::new(),
            value: IrValue::None,
            data_type: DataType::Whatever,
            data_flag: DataFlag::Unknown,
            scope: -1,
            option_type: RelOptionType::Unknown,
            mutated_times: 0,
            is_node_struct_fixed: false,
            uniq_id_in_tree: -1,
            alive: true,
        }
    }

    pub fn prefix(&self) -> &str {
        self.op.as_ref().map(|o| o.prefix.as_ref()).unwrap_or("")
    }

    pub fn middle(&self) -> &str {
        self.op.as_ref().map(|o| o.middle.as_ref()).unwrap_or("")
    }

    pub fn suffix(&self) -> &str {
        self.op.as_ref().map(|o| o.suffix.as_ref()).unwrap_or("")
    }

    /// True when the node has no text and no children.
    pub fn is_empty(&self) -> bool {
        if let Some(op) = &self.op {
            if !op.is_empty() {
                return false;
            }
        }
        if !self.str_val.is_empty() {
            return false;
        }
        self.left.is_none() && self.right.is_none()
    }
}

/// An arena of IR nodes with one root.
#[derive(Debug, Clone, Default)]
pub struct IrTree {
    nodes: Vec<IrNode>,
    pub root: Option<IrId>,
}

impl IrTree {
    pub fn new() -> IrTree {
        IrTree::default()
    }

    /// Number of nodes ever allocated, dropped nodes included.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty_tree(&self) -> bool {
        self.nodes.is_empty()
    }

    fn push(&mut self, node: IrNode) -> IrId {
        let id = IrId(self.nodes.len() as u32);
        self.nodes.push(node);
        id
    }

    pub fn get(&self, id: IrId) -> &IrNode {
        &self.nodes[id.index()]
    }

    pub fn get_mut(&mut self, id: IrId) -> &mut IrNode {
        &mut self.nodes[id.index()]
    }

    // -- construction --------------------------------------------------------

    /// An inner node, like the C++ `IR(type, op, left, right)`.
    pub fn node(&mut self, ty: IrType, op: Op, left: Option<IrId>, right: Option<IrId>) -> IrId {
        let mut n = IrNode::new(ty);
        n.op = Some(op);
        n.left = left;
        n.right = right;
        n.scope = 0;
        let id = self.push(n);
        if let Some(l) = left {
            self.nodes[l.index()].parent = Some(id);
        }
        if let Some(r) = right {
            self.nodes[r.index()].parent = Some(id);
        }
        id
    }

    /// A leaf with a string value, like the C++ `IR(type, str, data_type, scope, flag)`.
    pub fn leaf_str(
        &mut self,
        ty: IrType,
        str_val: impl Into<String>,
        data_type: DataType,
        scope: i32,
        data_flag: DataFlag,
    ) -> IrId {
        let mut n = IrNode::new(ty);
        n.str_val = str_val.into();
        n.data_type = data_type;
        n.scope = scope;
        n.data_flag = data_flag;
        self.push(n)
    }

    /// A leaf with a string value and the C++ default role (`kDataWhatever`, `kUse`).
    pub fn leaf(&mut self, ty: IrType, str_val: impl Into<String>) -> IrId {
        self.leaf_str(ty, str_val, DataType::Whatever, -1, DataFlag::Use)
    }

    /// A leaf with a numeric value, like the C++ `IR(type, int, ...)`.
    pub fn leaf_value(&mut self, ty: IrType, value: impl Into<IrValue>, data_type: DataType, scope: i32, data_flag: DataFlag) -> IrId {
        let mut n = IrNode::new(ty);
        n.value = value.into();
        n.data_type = data_type;
        n.scope = scope;
        n.data_flag = data_flag;
        self.push(n)
    }

    // -- accessors ------------------------------------------------------------

    pub fn ty(&self, id: IrId) -> IrType {
        self.get(id).ty
    }

    pub fn left(&self, id: IrId) -> Option<IrId> {
        self.get(id).left
    }

    pub fn right(&self, id: IrId) -> Option<IrId> {
        self.get(id).right
    }

    pub fn parent(&self, id: IrId) -> Option<IrId> {
        self.get(id).parent
    }

    pub fn str_val(&self, id: IrId) -> &str {
        &self.get(id).str_val
    }

    pub fn data_type(&self, id: IrId) -> DataType {
        self.get(id).data_type
    }

    pub fn data_flag(&self, id: IrId) -> DataFlag {
        self.get(id).data_flag
    }

    pub fn option_type(&self, id: IrId) -> RelOptionType {
        self.get(id).option_type
    }

    pub fn prefix(&self, id: IrId) -> &str {
        self.get(id).prefix()
    }

    pub fn middle(&self, id: IrId) -> &str {
        self.get(id).middle()
    }

    pub fn suffix(&self, id: IrId) -> &str {
        self.get(id).suffix()
    }

    pub fn is_empty(&self, id: IrId) -> bool {
        self.get(id).is_empty()
    }

    pub fn set_str_val(&mut self, id: IrId, s: impl Into<String>) {
        self.get_mut(id).str_val = s.into();
    }

    pub fn set_ir_type(&mut self, id: IrId, ty: IrType) {
        self.get_mut(id).ty = ty;
    }

    pub fn set_data_type(&mut self, id: IrId, data_type: DataType) {
        self.get_mut(id).data_type = data_type;
    }

    pub fn set_data_flag(&mut self, id: IrId, data_flag: DataFlag) {
        self.get_mut(id).data_flag = data_flag;
    }

    /// Replace the fixed text of a node.
    pub fn replace_op(&mut self, id: IrId, op: Op) {
        self.get_mut(id).op = Some(op);
    }

    // -- tree edits -----------------------------------------------------------

    /// Set the left child. The old left child keeps its parent pointer, and
    /// the old parent of the new child keeps its child pointer, like in C++.
    pub fn update_left(&mut self, id: IrId, new_left: Option<IrId>) {
        self.get_mut(id).left = new_left;
        if let Some(l) = new_left {
            self.get_mut(l).parent = Some(id);
        }
    }

    pub fn update_right(&mut self, id: IrId, new_right: Option<IrId>) {
        self.get_mut(id).right = new_right;
        if let Some(r) = new_right {
            self.get_mut(r).parent = Some(id);
        }
    }

    /// The parent of `child`, when `child` is inside the subtree of `root`
    /// (and is not `root` itself).
    pub fn locate_parent(&self, root: IrId, child: IrId) -> Option<IrId> {
        let mut p = Some(child);
        while let Some(cur) = p {
            if self.parent(cur) == Some(root) {
                return self.parent(child);
            }
            p = self.parent(cur);
        }
        None
    }

    /// The top of the tree that holds `id`.
    pub fn get_root(&self, id: IrId) -> IrId {
        let mut node = id;
        while let Some(p) = self.parent(node) {
            node = p;
        }
        node
    }

    /// Replace `old_node` by `new_node` inside the subtree of `root`.
    /// Returns false when `old_node` is not below `root`. `old_node` is
    /// detached but not dropped.
    pub fn swap_node(&mut self, root: IrId, old_node: IrId, new_node: Option<IrId>) -> bool {
        let Some(parent) = self.locate_parent(root, old_node) else {
            return false;
        };
        if self.left(parent) == Some(old_node) {
            self.update_left(parent, new_node);
        } else if self.right(parent) == Some(old_node) {
            self.update_right(parent, new_node);
        } else {
            return false;
        }
        self.get_mut(old_node).parent = None;
        true
    }

    /// Unlink `node` from the subtree of `root`, keep the node.
    pub fn detach_node(&mut self, root: IrId, node: IrId) -> bool {
        self.swap_node(root, node, None)
    }

    /// Mark the subtree of `id` as dropped. The nodes stay in the arena.
    pub fn deep_drop(&mut self, id: IrId) {
        let node = self.get(id);
        let (l, r) = (node.left, node.right);
        if let Some(l) = l {
            self.deep_drop(l);
        }
        if let Some(r) = r {
            self.deep_drop(r);
        }
        self.get_mut(id).alive = false;
    }

    /// Copy the subtree of `id` inside this tree. The copy has no parent.
    pub fn deep_copy(&mut self, id: IrId) -> IrId {
        let src = self.get(id).clone();
        let left = src.left.map(|l| self.deep_copy(l));
        let right = src.right.map(|r| self.deep_copy(r));
        let mut n = src;
        n.left = left;
        n.right = right;
        n.parent = None;
        n.is_node_struct_fixed = false;
        n.uniq_id_in_tree = -1;
        n.alive = true;
        let new_id = self.push(n);
        if let Some(l) = left {
            self.get_mut(l).parent = Some(new_id);
        }
        if let Some(r) = right {
            self.get_mut(r).parent = Some(new_id);
        }
        new_id
    }

    /// Copy the subtree `id` of `other` into this tree. The copy has no parent.
    pub fn import(&mut self, other: &IrTree, id: IrId) -> IrId {
        let src = other.get(id);
        let left = src.left.map(|l| self.import(other, l));
        let right = src.right.map(|r| self.import(other, r));
        let mut n = src.clone();
        n.left = left;
        n.right = right;
        n.parent = None;
        n.is_node_struct_fixed = false;
        n.uniq_id_in_tree = -1;
        n.alive = true;
        let new_id = self.push(n);
        if let Some(l) = left {
            self.get_mut(l).parent = Some(new_id);
        }
        if let Some(r) = right {
            self.get_mut(r).parent = Some(new_id);
        }
        new_id
    }

    /// Copy the subtree `id` into a new tree whose root is the copy.
    pub fn extract(&self, id: IrId) -> IrTree {
        let mut t = IrTree::new();
        let root = t.import(self, id);
        t.root = Some(root);
        t
    }

    // -- traversal -------------------------------------------------------------

    /// All nodes below and including `root`, in pre-order (node, left
    /// subtree, right subtree), except that `root` itself comes last. This is
    /// the order the C++ `get_all_ir_node` produces.
    pub fn all_nodes(&self, root: IrId) -> Vec<IrId> {
        let mut out = Vec::new();
        self.preorder(root, &mut out);
        out.remove(0);
        out.push(root);
        out
    }

    /// Pre-order list of the subtree (node, left subtree, right subtree).
    pub fn preorder(&self, id: IrId, out: &mut Vec<IrId>) {
        out.push(id);
        if let Some(l) = self.left(id) {
            self.preorder(l, out);
        }
        if let Some(r) = self.right(id) {
            self.preorder(r, out);
        }
    }

    /// Number the nodes below `root` in the order of [`IrTree::all_nodes`].
    pub fn assign_uniq_ids(&mut self, root: IrId) -> Vec<IrId> {
        let nodes = self.all_nodes(root);
        for (i, id) in nodes.iter().enumerate() {
            self.get_mut(*id).uniq_id_in_tree = i as i32;
        }
        nodes
    }

    /// Number of nodes in the subtree.
    pub fn count_nodes(&self, id: IrId) -> usize {
        1 + self.left(id).map_or(0, |l| self.count_nodes(l)) + self.right(id).map_or(0, |r| self.count_nodes(r))
    }

    // -- printing ---------------------------------------------------------------

    /// The SQL text of the subtree, normalized with [`util::trim_string`].
    pub fn to_string(&self, id: IrId) -> String {
        let mut res = String::new();
        self.to_string_core(id, &mut res);
        util::trim_string(&mut res);
        res
    }

    fn to_string_core(&self, id: IrId, res: &mut String) {
        let n = self.get(id);
        if n.data_type == DataType::Collate {
            res.push('"');
            res.push_str(&n.str_val);
            res.push('"');
            return;
        }
        match n.ty {
            IrType::IntLiteral => {
                if !n.str_val.is_empty() {
                    res.push_str(&n.str_val);
                } else if let IrValue::Int(i) = n.value {
                    res.push_str(&i.to_string());
                } else {
                    res.push('0');
                }
                return;
            }
            IrType::FloatLiteral => {
                if !n.str_val.is_empty() {
                    res.push_str(&n.str_val);
                } else if let IrValue::Float(f) = n.value {
                    res.push_str(&format!("{f:.6}"));
                } else {
                    res.push_str("0.000000");
                }
                return;
            }
            IrType::BoolLiteral => {
                if !n.str_val.is_empty() {
                    res.push_str(&n.str_val);
                } else if n.value == IrValue::Bool(true) {
                    res.push_str(" TRUE ");
                } else {
                    res.push_str(" FALSE ");
                }
                return;
            }
            IrType::Identifier => {
                res.push_str(&n.str_val);
                return;
            }
            IrType::StringLiteral => {
                res.push('\'');
                res.push_str(&n.str_val);
                res.push('\'');
                return;
            }
            _ => {}
        }
        if !n.str_val.is_empty() {
            res.push_str(&n.str_val);
            return;
        }
        if let Some(op) = &n.op {
            res.push_str(&op.prefix);
            res.push(' ');
        }
        if let Some(l) = n.left {
            self.to_string_core(l, res);
            res.push(' ');
        }
        if let Some(op) = &n.op {
            res.push_str(&op.middle);
            res.push(' ');
        }
        if let Some(r) = n.right {
            self.to_string_core(r, res);
            res.push(' ');
        }
        if let Some(op) = &n.op {
            res.push_str(&op.suffix);
        }
    }

    /// A line-per-node dump of the subtree for debugging and for comparison
    /// with the C++ implementation.
    pub fn dump(&self, id: IrId) -> String {
        let mut out = String::new();
        self.dump_core(id, 0, &mut out);
        out
    }

    fn dump_core(&self, id: IrId, level: usize, out: &mut String) {
        let n = self.get(id);
        for _ in 0..level {
            out.push(' ');
        }
        out.push_str(&format!(
            "{} {} {} {} [{}|{}|{}] <{}>\n",
            n.ty.as_str(),
            n.data_type.debug_name(),
            n.data_flag.as_str(),
            n.option_type as u8,
            n.prefix(),
            n.middle(),
            n.suffix(),
            n.str_val
        ));
        if let Some(l) = n.left {
            self.dump_core(l, level + 1, out);
        }
        if let Some(r) = n.right {
            self.dump_core(r, level + 1, out);
        }
    }

    // -- helpers used by the grammar actions and the oracles ----------------------

    /// Set the role of a node, whatever its type.
    pub fn set_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        let n = self.get_mut(id);
        n.data_type = data_type;
        n.data_flag = data_flag;
        true
    }

    /// Set the role of an identifier node.
    pub fn set_iden_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::Identifier);
        self.set_type(id, data_type, data_flag)
    }

    /// Set the role of a `qualified_name` node (its identifier). A qualified
    /// name with a right child is a `schema.name`; its first part is treated
    /// as a table name that follows another one.
    pub fn set_qualified_name_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::QualifiedName);
        let Some(iden) = self.left(id) else {
            return false;
        };
        if self.right(id).is_some() {
            self.set_type(iden, DataType::TableNameFollow, DataFlag::Use);
            return true;
        }
        self.set_type(iden, data_type, data_flag)
    }

    pub fn set_qualified_name_list_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::QualifiedNameList);
        let qualified_name = match self.right(id) {
            Some(r) => r,
            None => match self.left(id) {
                Some(l) => l,
                None => return false,
            },
        };
        if let Some(iden) = self.left(qualified_name) {
            self.set_iden_type(iden, data_type, data_flag);
        }
        if self.right(id).is_some() {
            if let Some(l) = self.left(id) {
                return self.set_qualified_name_list_type(l, data_type, data_flag);
            }
        }
        true
    }

    pub fn set_reloption_elem_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::ReloptionElem);
        self.set_type(id, data_type, data_flag)
    }

    pub fn set_any_name_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::AnyName);
        let Some(iden) = self.left(id) else {
            return false;
        };
        self.set_type(iden, data_type, data_flag)
    }

    pub fn set_any_name_list_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::AnyNameList);
        let anyname = match self.right(id) {
            Some(r) => r,
            None => match self.left(id) {
                Some(l) => l,
                None => return false,
            },
        };
        if let Some(iden) = self.left(anyname) {
            self.set_iden_type(iden, data_type, data_flag);
        }
        if self.right(id).is_some() {
            if let Some(l) = self.left(id) {
                return self.set_any_name_list_type(l, data_type, data_flag);
            }
        }
        true
    }

    pub fn set_opt_columnlist_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::OptColumnList);
        match self.left(id) {
            Some(l) => self.set_columnlist_type(l, data_type, data_flag),
            None => true,
        }
    }

    pub fn set_columnlist_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::ColumnList);
        let elem = match self.right(id) {
            Some(r) => r,
            None => match self.left(id) {
                Some(l) => l,
                None => return false,
            },
        };
        if let Some(iden) = self.left(elem) {
            self.set_iden_type(iden, data_type, data_flag);
        }
        if self.right(id).is_some() {
            if let Some(l) = self.left(id) {
                return self.set_columnlist_type(l, data_type, data_flag);
            }
        }
        true
    }

    pub fn set_insert_columnlist_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::InsertColumnList);
        let elem = match self.right(id) {
            Some(r) => r,
            None => match self.left(id) {
                Some(l) => l,
                None => return false,
            },
        };
        if let Some(iden) = self.left(elem) {
            self.set_iden_type(iden, data_type, data_flag);
        }
        if self.right(id).is_some() {
            if let Some(l) = self.left(id) {
                return self.set_insert_columnlist_type(l, data_type, data_flag);
            }
        }
        true
    }

    pub fn set_rolelist_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::RoleList);
        let iden = match self.right(id) {
            Some(r) => r,
            None => match self.left(id) {
                Some(l) => l,
                None => return false,
            },
        };
        self.set_iden_type(iden, data_type, data_flag);
        if self.right(id).is_some() {
            if let Some(l) = self.left(id) {
                return self.set_rolelist_type(l, data_type, data_flag);
            }
        }
        true
    }

    pub fn set_generic_set_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::GenericSet);
        self.set_type(id, data_type, data_flag)
    }

    pub fn set_opt_name_list_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::OptNameList);
        match self.left(id) {
            Some(l) => self.set_name_list_type(l, data_type, data_flag),
            None => false,
        }
    }

    pub fn set_name_list_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::NameList);
        match (self.left(id), self.right(id)) {
            (Some(next_list), Some(name)) => {
                let a = self.set_name_list_type(next_list, data_type, data_flag);
                let b = self.set_iden_type(name, data_type, data_flag);
                a && b
            }
            (Some(name), None) => self.set_iden_type(name, data_type, data_flag),
            _ => false,
        }
    }

    pub fn set_cluster_index_specification_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::ClusterIndexSpecification);
        match self.left(id) {
            Some(iden) => self.set_iden_type(iden, data_type, data_flag),
            None => false,
        }
    }

    pub fn set_relation_expr_type(&mut self, id: IrId, data_type: DataType, data_flag: DataFlag) -> bool {
        debug_assert_eq!(self.ty(id), IrType::RelationExpr);
        match self.left(id) {
            Some(qn) => self.set_qualified_name_type(qn, data_type, data_flag),
            None => false,
        }
    }

    pub fn set_rel_option_type(&mut self, id: IrId, option_type: RelOptionType) -> bool {
        self.get_mut(id).option_type = option_type;
        true
    }

    pub fn set_opt_reloptions_option_type(&mut self, id: IrId, option_type: RelOptionType) -> bool {
        debug_assert_eq!(self.ty(id), IrType::OptReloptions);
        match self.left(id) {
            Some(l) => self.set_reloptions_option_type(l, option_type),
            None => false,
        }
    }

    pub fn set_reloptions_option_type(&mut self, id: IrId, option_type: RelOptionType) -> bool {
        debug_assert_eq!(self.ty(id), IrType::Reloptions);
        match self.left(id) {
            Some(l) => self.set_reloption_list_option_type(l, option_type),
            None => false,
        }
    }

    pub fn set_reloption_list_option_type(&mut self, id: IrId, option_type: RelOptionType) -> bool {
        debug_assert_eq!(self.ty(id), IrType::ReloptionList);
        match (self.left(id), self.right(id)) {
            (Some(next_list), Some(elem)) => {
                let a = self.set_reloption_list_option_type(next_list, option_type);
                let b = self.set_reloption_elem_option_type(elem, option_type);
                a && b
            }
            (Some(elem), None) => self.set_reloption_elem_option_type(elem, option_type),
            _ => false,
        }
    }

    pub fn set_reloption_elem_option_type(&mut self, id: IrId, option_type: RelOptionType) -> bool {
        debug_assert_eq!(self.ty(id), IrType::ReloptionElem);
        self.set_rel_option_type(id, option_type)
    }

    /// True for `ADD` in an `add_drop` node.
    pub fn add_drop_is_add(&self, id: IrId) -> bool {
        self.prefix(id) == "ADD"
    }

    /// Which kind of object an `object_type_any_name` node names.
    pub fn get_object_type_any_name(&self, id: IrId) -> i32 {
        let p = self.prefix(id);
        let table = [
            ("TABLE", 0),
            ("SEQUENCE", 1),
            ("VIEW", 2),
            ("MATERIALIZED VIEW", 3),
            ("INDEX", 4),
            ("FOREIGN TABLE", 5),
            ("COLLATION", 6),
            ("CONVERSION", 7),
            ("STATISTICS", 8),
            ("TEXT SEARCH PARSER", 9),
            ("TEXT SEARCH DICTIONARY", 10),
            ("TEXT SEARCH TEMPLATE", 11),
            ("TEXT SEARCH CONFIGURATION", 12),
        ];
        for (name, code) in table {
            if p.starts_with(name) {
                return code;
            }
        }
        -1
    }

    /// Which kind of object an `object_type_name` node names.
    pub fn get_object_type(&self, id: IrId) -> i32 {
        let p = self.prefix(id);
        // The C++ compares prefixes of odd lengths; the effect is a plain prefix test.
        if p.starts_with("DATAB") {
            0
        } else if p.starts_with("ROLE") {
            1
        } else if p.starts_with("SUBS") {
            2
        } else if p.starts_with("TABLESPACE") {
            3
        } else {
            -1
        }
    }

    /// Which kind of object a `REINDEX` target names.
    pub fn get_reindex_target_type(&self, id: IrId) -> i32 {
        let p = self.prefix(id);
        if p.starts_with("INDEX") {
            0
        } else if p.starts_with("TABLE") {
            1
        } else if p.starts_with("SCHEMA") {
            2
        } else if p.starts_with("SYSTEM") {
            3
        } else if p.starts_with("DATABASE") {
            4
        } else {
            -1
        }
    }

    /// True when a `target_el` node has an `AS alias`.
    pub fn target_el_is_exist_alias(&self, id: IrId) -> bool {
        let n = self.get(id);
        if n.is_empty() || n.op.is_none() {
            return false;
        }
        n.middle() == "AS"
    }

    /// Rename the alias of a `target_el` node that has one.
    pub fn target_el_set_alias(&mut self, id: IrId, alias: &str) -> bool {
        if !self.target_el_is_exist_alias(id) {
            return false;
        }
        match self.right(id) {
            Some(iden) => {
                self.set_str_val(iden, alias);
                true
            }
            None => false,
        }
    }

    /// Rename the function of a `func_name` node.
    pub fn func_name_set_str(&mut self, id: IrId, name: &str) -> bool {
        debug_assert_eq!(self.ty(id), IrType::FuncName);
        match self.left(id) {
            Some(iden) => {
                self.set_str_val(iden, name);
                true
            }
            None => false,
        }
    }

    /// The column type a `Typename` node describes.
    pub fn typename_ir_get_type(&self, id: IrId) -> ColType {
        let Some(simple) = self.left(id) else {
            return ColType::Unknown;
        };
        if self.ty(simple) != IrType::SimpleTypename {
            return ColType::Unknown;
        }
        let Some(sub) = self.left(simple) else {
            return ColType::Unknown;
        };
        if self.ty(sub) == IrType::Numeric {
            let prefix = self.prefix(sub);
            if util::find_string_in(prefix, "INT") {
                return ColType::Int;
            } else if util::find_string_in(prefix, "BOOL") {
                return ColType::Boolean;
            } else if util::find_string_in(prefix, "REAL")
                || util::find_string_in(prefix, "DOUBLE")
                || util::find_string_in(prefix, "DEC")
                || util::find_string_in(prefix, "FLOAT")
            {
                return ColType::Float;
            }
            return ColType::Unknown;
        }
        ColType::String
    }
}

impl From<i64> for IrValue {
    fn from(v: i64) -> IrValue {
        IrValue::Int(v)
    }
}

impl From<f64> for IrValue {
    fn from(v: f64) -> IrValue {
        IrValue::Float(v)
    }
}

impl From<bool> for IrValue {
    fn from(v: bool) -> IrValue {
        IrValue::Bool(v)
    }
}

impl DataType {
    /// The name the C++ debug output uses, for example `data_tableName`.
    pub fn debug_name(self) -> &'static str {
        match self {
            DataType::Whatever => "data_whatever",
            DataType::TableName => "data_tableName",
            DataType::ColumnName => "data_columnName",
            DataType::ViewName => "data_viewName",
            DataType::FunctionName => "data_functionName",
            DataType::PragmaKey => "data_pragmaKey",
            DataType::PragmaValue => "data_pragmaValue",
            DataType::TableSpaceName => "data_tableSpaceName",
            DataType::SequenceName => "data_SequenceName",
            DataType::ExtensionName => "data_extensionName",
            DataType::RoleName => "data_roleName",
            DataType::SchemaName => "data_SchemaName",
            DataType::Database => "data_dataDatabase",
            DataType::TriggerName => "data_triggername",
            DataType::WindowName => "data_windowName",
            DataType::TriggerFunction => "data_triggerFunction",
            DataType::DomainName => "data_domainName",
            DataType::AliasName => "data_aliasName",
            DataType::Literal => "data_literal",
            DataType::IndexName => "data_indexName",
            DataType::GroupName => "data_groupName",
            DataType::UserName => "data_UserName",
            DataType::DatabaseName => "data_DatabaseName",
            DataType::SystemName => "data_SystemName",
            DataType::ConversionName => "data_ConversionName",
            DataType::AggregateArguments => "data_aggregateArguments",
            DataType::NonReservedWord => "data_nonReservedWord",
            DataType::FixLater => "data_fixLater",
            DataType::ConstraintName => "data_constraintName",
            DataType::RelOption => "data_relOption",
            DataType::GenericType => "data_genericType",
            DataType::TableNameFollow => "data_tableNameFollow",
            DataType::ColumnNameFollow => "data_columnNameFollow",
            DataType::Collate => "data_collate",
            DataType::StatisticName => "data_statisticName",
            DataType::ForeignTableName => "data_foreignTableName",
            DataType::AliasTableName => "data_aliasTableName",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_string_joins_parts_with_spaces() {
        let mut t = IrTree::new();
        let a = t.leaf_str(IrType::Identifier, "v0", DataType::TableName, 0, DataFlag::Use);
        let from = t.node(IrType::FromClause, Op::new("FROM", "", ""), Some(a), None);
        let star = t.leaf(IrType::TargetEl, "*");
        let sel = t.node(IrType::SimpleSelect, Op::new("SELECT", "", ""), Some(star), Some(from));
        assert_eq!(t.to_string(sel), "SELECT * FROM v0");
    }

    #[test]
    fn swap_and_detach() {
        let mut t = IrTree::new();
        let a = t.leaf(IrType::Identifier, "a");
        let b = t.leaf(IrType::Identifier, "b");
        let root = t.node(IrType::Stmt, Op::empty(), Some(a), Some(b));
        let c = t.leaf(IrType::Identifier, "c");
        assert!(t.swap_node(root, a, Some(c)));
        assert_eq!(t.left(root), Some(c));
        assert_eq!(t.parent(c), Some(root));
        assert_eq!(t.parent(a), None);
        assert!(!t.swap_node(root, root, Some(a)));
        assert!(t.detach_node(root, b));
        assert_eq!(t.right(root), None);
        assert_eq!(t.to_string(root), "c");
    }

    #[test]
    fn all_nodes_is_preorder_with_root_last() {
        let mut t = IrTree::new();
        let a = t.leaf(IrType::Identifier, "a");
        let b = t.leaf(IrType::Identifier, "b");
        let inner = t.node(IrType::Stmt, Op::empty(), Some(a), Some(b));
        let c = t.leaf(IrType::Identifier, "c");
        let root = t.node(IrType::ParseToplevel, Op::empty(), Some(inner), Some(c));
        assert_eq!(t.all_nodes(root), vec![inner, a, b, c, root]);
    }
}
