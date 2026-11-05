//! Context-based instantiation: fill in the names and literals of a
//! statement so that it fits the statements before it. Port of
//! `Mutator::fix_dependency` and its helpers in `mutate.cpp`.
//!
//! The context remembers which tables, columns, indexes and views the
//! statements so far have created, and the literals they used. A statement
//! that defines a name gets a fresh one. A statement that uses a name gets
//! one from the context. A statement that drops a name removes it.

use std::collections::{HashMap, HashSet};

use sqlright_ir::{ColType, DataFlag, DataType, IrId, IrTree, IrType, IrValue};

use crate::ir_wrapper;
use crate::mutator::Mutator;
use crate::relopt;

/// The names and literals of the current test case.
#[derive(Debug, Clone, Default)]
pub struct FixContext {
    /// Table name to column names.
    pub m_tables: HashMap<String, Vec<String>>,
    /// Table name to index names.
    pub m_table2index: HashMap<String, Vec<String>>,
    /// All tables created so far.
    pub v_table_names: Vec<String>,
    /// Tables used in the current statement.
    pub v_table_names_single: Vec<String>,
    /// Tables created by the current statement.
    pub v_create_table_names_single: Vec<String>,
    /// Aliases defined in the current statement.
    pub v_alias_names_single: Vec<String>,
    /// Table name to its aliases, in the current statement.
    pub m_table2alias_single: HashMap<String, Vec<String>>,
    /// Column name to its type.
    pub m_column2datatype: HashMap<String, ColType>,
    /// Columns used in the current statement, to type the literals.
    pub v_column_names_single: Vec<String>,
    /// Tables used in `table.column` references in the current statement.
    pub v_table_name_follow_single: Vec<String>,
    pub v_statistics_name: Vec<String>,
    pub v_sequence_name: Vec<String>,
    pub v_view_name: Vec<String>,
    pub v_constraint_name: Vec<String>,
    pub v_foreign_table_name: Vec<String>,
    pub v_create_foreign_table_names_single: Vec<String>,
    /// Tables created with `PARTITION BY`.
    pub v_table_with_partition_name: Vec<String>,
    pub v_int_literals: Vec<i64>,
    pub v_float_literals: Vec<f64>,
    pub v_string_literals: Vec<String>,
    pub v_saved_reloption_str: Vec<String>,
}

impl FixContext {
    /// Forget the state of the current statement.
    pub fn reset_data_library_single_stmt(&mut self) {
        self.v_table_names_single.clear();
        self.v_create_table_names_single.clear();
        self.v_alias_names_single.clear();
        self.m_table2alias_single.clear();
        self.v_column_names_single.clear();
        self.v_table_name_follow_single.clear();
        self.v_create_foreign_table_names_single.clear();
    }

    /// Forget the state of the current test case.
    pub fn reset_data_library(&mut self) {
        let saved_reloptions = std::mem::take(&mut self.v_saved_reloption_str);
        *self = FixContext::default();
        self.v_saved_reloption_str = saved_reloptions;
    }

    fn column_type(&self, column: &str) -> ColType {
        self.m_column2datatype
            .get(column)
            .copied()
            .unwrap_or(ColType::Unknown)
    }
}

/// The roles the instantiation step fills in.
const TYPES_TO_FIX: &[DataType] = &[
    DataType::ColumnName,
    DataType::TableName,
    DataType::PragmaKey,
    DataType::PragmaValue,
    DataType::Literal,
    DataType::RelOption,
    DataType::IndexName,
    DataType::AliasName,
    DataType::TableNameFollow,
    DataType::ColumnNameFollow,
    DataType::StatisticName,
    DataType::SequenceName,
    DataType::ViewName,
    DataType::ForeignTableName,
    DataType::ConstraintName,
    DataType::AliasTableName,
];

/// In-order list of the nodes below `root` whose role is in `types`.
pub fn collect_ir(t: &IrTree, root: IrId, types: &[DataType], out: &mut Vec<IrId>) {
    if let Some(l) = t.left(root) {
        collect_ir(t, l, types, out);
    }
    if types.contains(&t.data_type(root)) {
        out.push(root);
    }
    if let Some(r) = t.right(root) {
        collect_ir(t, r, types, out);
    }
}

/// The nodes of a statement that the instantiation step fills in.
pub fn collect_fixable(t: &IrTree, stmt: IrId) -> Vec<IrId> {
    let mut out = Vec::new();
    collect_ir(t, stmt, TYPES_TO_FIX, &mut out);
    out
}

fn is_bad_table_name(name: &str) -> bool {
    name.is_empty() || name == "x" || name == "y"
}

fn remove_value(v: &mut Vec<String>, value: &str) {
    v.retain(|s| s != value);
}

fn float_str(f: f64) -> String {
    format!("{f:.6}")
}

impl Mutator {
    /// Fill in the names of one statement. `ir_to_fix_vecs` holds, for the
    /// statement and for each of its subqueries, the nodes to fill in.
    pub fn fix_dependency(
        &mut self,
        t: &mut IrTree,
        stmt: IrId,
        ir_to_fix_vecs: &[Vec<IrId>],
    ) -> bool {
        let mut fixed: HashSet<IrId> = HashSet::new();
        let mut to_drop: Vec<IrId> = Vec::new();
        let stmt_type = t.ty(stmt);
        let cur_ir_str = t.to_string(stmt);
        let mut is_replace_table = false;

        for ir_to_fix_vec in ir_to_fix_vecs {
            let mut v_with_clause_alias_table_name: Vec<String> = Vec::new();

            // Definitions of table names.
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                let n = t.get(id);
                if (n.data_type == DataType::TableName || n.data_type == DataType::ForeignTableName)
                    && n.data_flag == DataFlag::Define
                {
                    let new_name = self.gen_id_name();
                    if n.data_type == DataType::ForeignTableName {
                        self.ctx
                            .v_create_foreign_table_names_single
                            .push(new_name.clone());
                    }
                    t.set_str_val(id, new_name.clone());
                    fixed.insert(id);
                    self.ctx.v_create_table_names_single.push(new_name);
                    is_replace_table = true;
                }
            }

            // Removal of table names.
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                let n = t.get(id);
                if n.data_type == DataType::TableName && n.data_flag == DataFlag::Undefine {
                    if !self.ctx.v_table_names.is_empty() {
                        let removed = self.ctx.v_table_names
                            [self.rng.rand_int(self.ctx.v_table_names.len())]
                        .clone();
                        remove_value(&mut self.ctx.v_table_names, &removed);
                        remove_value(&mut self.ctx.v_table_names_single, &removed);
                        t.set_str_val(id, removed.clone());
                        fixed.insert(id);
                        if is_replace_table && !self.ctx.v_create_table_names_single.is_empty() {
                            let new_table = self.ctx.v_create_table_names_single[0].clone();
                            let cols = self.ctx.m_tables.get(&removed).cloned().unwrap_or_default();
                            self.ctx.m_tables.insert(new_table, cols);
                        }
                    } else {
                        fixed.insert(id);
                    }
                } else if n.data_type == DataType::ForeignTableName
                    && n.data_flag == DataFlag::Undefine
                {
                    if !self.ctx.v_foreign_table_name.is_empty() {
                        let removed = self.ctx.v_foreign_table_name
                            [self.rng.rand_int(self.ctx.v_foreign_table_name.len())]
                        .clone();
                        remove_value(&mut self.ctx.v_foreign_table_name, &removed);
                        remove_value(&mut self.ctx.v_table_names, &removed);
                        remove_value(&mut self.ctx.v_table_names_single, &removed);
                        t.set_str_val(id, removed.clone());
                        fixed.insert(id);
                        if is_replace_table
                            && !self.ctx.v_create_foreign_table_names_single.is_empty()
                        {
                            let new_table = self.ctx.v_create_foreign_table_names_single[0].clone();
                            let cols = self.ctx.m_tables.get(&removed).cloned().unwrap_or_default();
                            self.ctx.m_tables.insert(new_table, cols);
                        }
                    } else {
                        fixed.insert(id);
                    }
                }
            }

            // Uses of table names.
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                let n = t.get(id);
                if !(n.data_type == DataType::TableName && n.data_flag == DataFlag::Use) {
                    continue;
                }
                let ori_str = n.str_val.clone();
                // A system catalog is kept nine times out of ten.
                if self.v_sys_catalogs_name.contains(&ori_str) && self.rng.rand_int(10) < 9 {
                    continue;
                }
                // In PARTITION OF, use a table that has partitions.
                if ir_wrapper::is_ir_in_type(t, id, IrType::CreateStmt_30)
                    || ir_wrapper::is_ir_in_type(t, id, IrType::CreateStmt_38)
                    || ir_wrapper::is_ir_in_type(t, id, IrType::CreateForeignTableStmt_7)
                    || ir_wrapper::is_ir_in_type(t, id, IrType::CreateForeignTableStmt_11)
                {
                    if !self.ctx.v_table_with_partition_name.is_empty() {
                        let name = self.ctx.v_table_with_partition_name[self
                            .rng
                            .rand_int(self.ctx.v_table_with_partition_name.len())]
                        .clone();
                        t.set_str_val(id, name);
                        fixed.insert(id);
                        continue;
                    } else if self.rng.rand_int(5) < 1 {
                        fixed.insert(id);
                        continue;
                    }
                }
                if self.ctx.v_table_names.is_empty()
                    && self.ctx.v_table_names_single.is_empty()
                    && self.ctx.v_create_table_names_single.is_empty()
                {
                    fixed.insert(id);
                    continue;
                }
                let used_name = if !self.ctx.v_table_names.is_empty() {
                    self.ctx.v_table_names[self.rng.rand_int(self.ctx.v_table_names.len())].clone()
                } else if !self.ctx.v_table_names_single.is_empty() {
                    self.ctx.v_table_names_single
                        [self.rng.rand_int(self.ctx.v_table_names_single.len())]
                    .clone()
                } else {
                    self.ctx.v_create_table_names_single[self
                        .rng
                        .rand_int(self.ctx.v_create_table_names_single.len())]
                    .clone()
                };
                t.set_str_val(id, used_name.clone());
                fixed.insert(id);
                self.ctx.v_table_names_single.push(used_name.clone());
                if stmt_type == IrType::CreateStmt
                    && ir_wrapper::is_ir_in_type(t, id, IrType::TableLikeClause)
                {
                    if let Some(new_table) = self.ctx.v_create_table_names_single.first().cloned() {
                        let cols = self
                            .ctx
                            .m_tables
                            .get(&used_name)
                            .cloned()
                            .unwrap_or_default();
                        self.ctx.m_tables.insert(new_table, cols);
                    }
                }
            }

            // Definitions of view names.
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                let n = t.get(id);
                if n.data_type == DataType::ViewName && n.data_flag == DataFlag::Define {
                    let name = self.gen_view_name();
                    t.set_str_val(id, name.clone());
                    fixed.insert(id);
                    self.ctx.v_create_table_names_single.push(name.clone());
                    self.ctx.v_view_name.push(name);
                }
            }

            // Removal and uses of view names.
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                let n = t.get(id);
                if n.data_type == DataType::ViewName && n.data_flag == DataFlag::Undefine {
                    if self.ctx.v_view_name.is_empty() {
                        fixed.insert(id);
                        continue;
                    }
                    let view =
                        self.ctx.v_view_name[self.rng.rand_int(self.ctx.v_view_name.len())].clone();
                    t.set_str_val(id, view.clone());
                    fixed.insert(id);
                    remove_value(&mut self.ctx.v_view_name, &view);
                    remove_value(&mut self.ctx.v_table_names, &view);
                    remove_value(&mut self.ctx.v_create_table_names_single, &view);
                }
                let n = t.get(id);
                if n.data_type == DataType::ViewName && n.data_flag == DataFlag::Use {
                    if self.ctx.v_view_name.is_empty() {
                        continue;
                    }
                    let view =
                        self.ctx.v_view_name[self.rng.rand_int(self.ctx.v_view_name.len())].clone();
                    t.set_str_val(id, view.clone());
                    fixed.insert(id);
                    self.ctx.v_table_names_single.push(view);
                }
            }

            // Alias table names (WITH clause names and table aliases).
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                let n = t.get(id);
                if n.data_type == DataType::AliasTableName && n.data_flag == DataFlag::Define {
                    let name = self.gen_alias_name();
                    t.set_str_val(id, name.clone());
                    fixed.insert(id);
                    if ir_wrapper::is_ir_in_type(t, id, IrType::WithClause) {
                        v_with_clause_alias_table_name.push(name);
                    } else {
                        self.ctx.v_table_names_single.push(name);
                    }
                }
            }

            // Aliases. All aliases are treated as aliases of a table.
            let mut alias_idx = 0;
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                if t.data_type(id) != DataType::AliasName {
                    continue;
                }
                let mut closest = String::new();
                if !v_with_clause_alias_table_name.is_empty() {
                    closest = v_with_clause_alias_table_name
                        [self.rng.rand_int(v_with_clause_alias_table_name.len())]
                    .clone();
                } else if !self.ctx.v_table_names_single.is_empty() {
                    if alias_idx < self.ctx.v_table_names_single.len() {
                        closest = self.ctx.v_table_names_single[alias_idx].clone();
                        alias_idx += 1;
                    } else {
                        closest = self.ctx.v_table_names_single
                            [self.rng.rand_int(self.ctx.v_table_names_single.len())]
                        .clone();
                    }
                } else if !self.ctx.v_create_table_names_single.is_empty() {
                    closest = self.ctx.v_create_table_names_single[0].clone();
                } else if !self.ctx.v_table_names.is_empty() {
                    closest = self.ctx.v_table_names
                        [self.rng.rand_int(self.ctx.v_table_names.len())]
                    .clone();
                }
                let alias = self.gen_alias_name();
                t.set_str_val(id, alias.clone());
                if !is_bad_table_name(&closest) {
                    self.ctx
                        .m_table2alias_single
                        .entry(closest)
                        .or_default()
                        .push(alias.clone());
                }
                self.ctx.v_alias_names_single.push(alias);
                fixed.insert(id);
            }

            // Definitions, replacements and removals of column names.
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                // Values of an INSERT are literals, whatever the grammar says.
                if stmt_type == IrType::InsertStmt
                    && ir_wrapper::is_ir_in_type(t, id, IrType::ValuesClause)
                {
                    t.set_type(id, DataType::Literal, DataFlag::Unknown);
                    continue;
                }
                let n = t.get(id);
                if n.data_type == DataType::ColumnName
                    && (n.data_flag == DataFlag::Define || n.data_flag == DataFlag::Replace)
                {
                    let flag = n.data_flag;
                    let new_name = self.gen_column_name();
                    t.set_str_val(id, new_name.clone());
                    fixed.insert(id);
                    let mut closest = String::new();
                    if !self.ctx.v_create_table_names_single.is_empty() {
                        closest = self.ctx.v_create_table_names_single[0].clone();
                    } else if !self.ctx.v_table_names_single.is_empty() {
                        closest = self.ctx.v_table_names_single[0].clone();
                    } else if !self.ctx.v_table_names.is_empty() {
                        if self.rng.rand_int(5) < 4 {
                            continue;
                        }
                        closest = self.ctx.v_table_names
                            [self.rng.rand_int(self.ctx.v_table_names.len())]
                        .clone();
                    }
                    if is_bad_table_name(&closest) {
                        continue;
                    }
                    self.ctx
                        .m_tables
                        .entry(closest)
                        .or_default()
                        .push(new_name.clone());
                    if flag == DataFlag::Define {
                        let col_type = match t.parent(id).and_then(|p| t.right(p)) {
                            Some(r) if t.ty(r) == IrType::Typename => t.typename_ir_get_type(r),
                            _ => ColType::Unknown,
                        };
                        self.ctx.m_column2datatype.insert(new_name, col_type);
                    } else {
                        let mut cols = Vec::new();
                        collect_ir(t, stmt, &[DataType::ColumnName], &mut cols);
                        let prev_type = match cols.first() {
                            Some(c) => self.ctx.column_type(t.str_val(*c)),
                            None => ColType::Unknown,
                        };
                        self.ctx.m_column2datatype.insert(new_name, prev_type);
                    }
                } else if n.data_type == DataType::ColumnName && n.data_flag == DataFlag::Undefine {
                    let closest = self
                        .ctx
                        .v_table_names_single
                        .first()
                        .cloned()
                        .unwrap_or_default();
                    if is_bad_table_name(&closest) {
                        fixed.insert(id);
                        continue;
                    }
                    let cols = self.ctx.m_tables.entry(closest).or_default();
                    if cols.is_empty() {
                        fixed.insert(id);
                        continue;
                    }
                    let removed = cols[self.rng.rand_int(cols.len())].clone();
                    cols.retain(|c| *c != removed);
                    t.set_str_val(id, removed);
                    fixed.insert(id);
                }
            }

            // Uses of column names.
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                if stmt_type == IrType::InsertStmt
                    && ir_wrapper::is_ir_in_type(t, id, IrType::ValuesClause)
                {
                    t.set_type(id, DataType::Literal, DataFlag::Unknown);
                    continue;
                }
                let n = t.get(id);
                if !(n.data_type == DataType::ColumnName && n.data_flag == DataFlag::Use) {
                    continue;
                }
                let ori_str = n.str_val.clone();
                if self.v_sys_column_name.contains(&ori_str) && self.rng.rand_int(4) >= 1 {
                    continue;
                } else if !ir_wrapper::is_ir_in_type(t, id, IrType::WithClause)
                    && !self.ctx.v_alias_names_single.is_empty()
                    && self.rng.rand_int(3) < 2
                {
                    let alias = self.ctx.v_alias_names_single
                        [self.rng.rand_int(self.ctx.v_alias_names_single.len())]
                    .clone();
                    t.set_str_val(id, alias);
                    continue;
                } else if self.rng.rand_int(20) < 1 {
                    let col = self.v_sys_column_name
                        [self.rng.rand_int(self.v_sys_column_name.len())]
                    .clone();
                    t.set_str_val(id, col);
                    continue;
                }
                let mut closest = String::new();
                if !self.ctx.v_table_names_single.is_empty() {
                    closest = self.ctx.v_table_names_single
                        [self.rng.rand_int(self.ctx.v_table_names_single.len())]
                    .clone();
                } else if !self.ctx.v_create_table_names_single.is_empty() {
                    closest = self.ctx.v_create_table_names_single[0].clone();
                } else if !self.ctx.v_alias_names_single.is_empty() {
                    let alias = self.ctx.v_alias_names_single
                        [self.rng.rand_int(self.ctx.v_alias_names_single.len())]
                    .clone();
                    t.set_str_val(id, alias);
                    fixed.insert(id);
                    continue;
                } else if !self.ctx.v_table_names.is_empty() {
                    if self.rng.rand_int(5) < 4 {
                        fixed.insert(id);
                        continue;
                    }
                    closest = self.ctx.v_table_names
                        [self.rng.rand_int(self.ctx.v_table_names.len())]
                    .clone();
                }
                if is_bad_table_name(&closest) {
                    if !self.ctx.v_alias_names_single.is_empty() {
                        let alias = self.ctx.v_alias_names_single
                            [self.rng.rand_int(self.ctx.v_alias_names_single.len())]
                        .clone();
                        t.set_str_val(id, alias);
                    }
                    fixed.insert(id);
                    continue;
                }
                let cols = self.ctx.m_tables.get(&closest).cloned().unwrap_or_default();
                if !cols.is_empty() {
                    let col = cols[self.rng.rand_int(cols.len())].clone();
                    t.set_str_val(id, col.clone());
                    fixed.insert(id);
                    self.ctx.v_column_names_single.push(col);
                } else {
                    fixed.insert(id);
                }
            }

            // `table.column`: the table part.
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                if t.data_type(id) != DataType::TableNameFollow {
                    continue;
                }
                let mut chosen = if !self.ctx.v_table_names_single.is_empty() {
                    self.ctx.v_table_names_single
                        [self.rng.rand_int(self.ctx.v_table_names_single.len())]
                    .clone()
                } else if !self.ctx.v_create_table_names_single.is_empty() {
                    self.ctx.v_create_table_names_single[self
                        .rng
                        .rand_int(self.ctx.v_create_table_names_single.len())]
                    .clone()
                } else if !self.ctx.v_table_names.is_empty() {
                    self.ctx.v_table_names[self.rng.rand_int(self.ctx.v_table_names.len())].clone()
                } else {
                    fixed.insert(id);
                    continue;
                };
                self.ctx.v_table_name_follow_single.push(chosen.clone());
                if let Some(aliases) = self.ctx.m_table2alias_single.get(&chosen) {
                    if let Some(a) = aliases.first() {
                        chosen = a.clone();
                    }
                }
                t.set_str_val(id, chosen);
                fixed.insert(id);
            }

            // `table.column`: the column part. The C++ always uses the first
            // table of the statement.
            let table_follow_idx = 0;
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                if t.data_type(id) != DataType::ColumnNameFollow {
                    continue;
                }
                if table_follow_idx < self.ctx.v_table_name_follow_single.len() {
                    let table = self.ctx.v_table_name_follow_single[table_follow_idx].clone();
                    let cols = self.ctx.m_tables.get(&table).cloned().unwrap_or_default();
                    if cols.is_empty() {
                        fixed.insert(id);
                        continue;
                    }
                    let col = cols[self.rng.rand_int(cols.len())].clone();
                    t.set_str_val(id, col);
                    fixed.insert(id);
                } else {
                    fixed.insert(id);
                }
            }

            // Index names.
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                let n = t.get(id);
                if n.data_type != DataType::IndexName {
                    continue;
                }
                match n.data_flag {
                    DataFlag::Define => {
                        let name = self.gen_index_name();
                        t.set_str_val(id, name.clone());
                        fixed.insert(id);
                        if let Some(table) = self.ctx.v_table_names_single.first().cloned() {
                            self.ctx.m_table2index.entry(table).or_default().push(name);
                        }
                    }
                    DataFlag::Undefine => {
                        let mut tmp = "y".to_string();
                        if let Some(table) = self.ctx.v_table_names_single.first().cloned() {
                            let idx_names = self.ctx.m_table2index.entry(table).or_default();
                            if idx_names.is_empty() {
                                continue;
                            }
                            tmp = idx_names[self.rng.rand_int(idx_names.len())].clone();
                            idx_names.retain(|s| *s != tmp);
                        } else {
                            let keys: Vec<String> =
                                self.ctx.m_table2index.keys().cloned().collect();
                            for key in keys {
                                let idx_names = self.ctx.m_table2index.get_mut(&key).unwrap();
                                if idx_names.is_empty() {
                                    continue;
                                }
                                tmp = idx_names[self.rng.rand_int(idx_names.len())].clone();
                                idx_names.retain(|s| *s != tmp);
                            }
                        }
                        if tmp != "y" {
                            t.set_str_val(id, tmp);
                            fixed.insert(id);
                        }
                    }
                    DataFlag::Use => {
                        let mut tmp = "y".to_string();
                        if let Some(table) = self.ctx.v_table_names_single.first().cloned() {
                            let idx_names = self
                                .ctx
                                .m_table2index
                                .get(&table)
                                .cloned()
                                .unwrap_or_default();
                            if idx_names.is_empty() {
                                continue;
                            }
                            tmp = idx_names[self.rng.rand_int(idx_names.len())].clone();
                        } else {
                            let all: Vec<Vec<String>> =
                                self.ctx.m_table2index.values().cloned().collect();
                            for idx_names in all {
                                if idx_names.is_empty() {
                                    continue;
                                }
                                tmp = idx_names[self.rng.rand_int(idx_names.len())].clone();
                            }
                        }
                        if tmp != "y" {
                            t.set_str_val(id, tmp);
                            fixed.insert(id);
                        }
                    }
                    _ => {}
                }
            }

            // Literals.
            let mut cur_literal_idx: i64 = -1;
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                if t.data_type(id) != DataType::Literal {
                    continue;
                }
                fixed.insert(id);
                let ori_str = t.str_val(id).to_string();
                let ty = t.ty(id);
                // The marker statements of a saved test case hold the literals
                // 'BEGIN VERI n' and 'END VERI n'. A query with such a literal
                // makes the harness print a data row that looks like a marker.
                // Thus the mutator must not learn or reuse them.
                let is_marker = ty == IrType::StringLiteral && is_marker_text(&ori_str);
                if ty == IrType::StringLiteral
                    && !is_marker
                    && !self.common_string_library.contains(&ori_str)
                    && self.rng.rand_int(10) < 5
                {
                    self.common_string_library.push(ori_str.clone());
                }
                let is_select = stmt_type == IrType::SelectStmt;
                let keep_ori = (!is_select && self.rng.rand_int(100) < 99)
                    || (is_select && self.rng.rand_int(60) < 10);
                if keep_ori {
                    match ty {
                        IrType::IntLiteral => {
                            if let Ok(v) = ori_str.parse::<i64>() {
                                self.ctx.v_int_literals.push(v);
                            }
                        }
                        IrType::FloatLiteral => {
                            if let Ok(v) = ori_str.parse::<f64>() {
                                self.ctx.v_float_literals.push(v);
                            }
                        }
                        IrType::StringLiteral if !is_marker => {
                            self.ctx.v_string_literals.push(ori_str.clone())
                        }
                        _ => {}
                    }
                    continue;
                }
                if stmt_type == IrType::VariableSetStmt
                    || stmt_type == IrType::VariableResetStmt
                    || ir_wrapper::is_ir_in_type(t, id, IrType::GenericSet)
                    || ir_wrapper::is_ir_in_type(t, id, IrType::ReloptionElem)
                    || ir_wrapper::is_ir_in_type(t, id, IrType::DefElem)
                    || ir_wrapper::is_ir_in_type(t, id, IrType::Reloptions)
                {
                    continue;
                }
                cur_literal_idx += 1;
                let lit_idx = cur_literal_idx as usize;
                let mut column_type = ColType::Unknown;
                if self.ctx.v_column_names_single.len() > lit_idx {
                    column_type = self
                        .ctx
                        .column_type(&self.ctx.v_column_names_single[lit_idx]);
                } else if let Some(first_table) = self.ctx.v_table_names_single.first() {
                    if let Some(cols) = self.ctx.m_tables.get(first_table) {
                        if cols.len() > lit_idx {
                            column_type = self.ctx.column_type(&cols[lit_idx]);
                        }
                    }
                }
                let keep_type = (!is_select && self.rng.rand_int(20) < 19)
                    || (is_select && self.rng.rand_int(50) < 10);
                if keep_type {
                    column_type = match ty {
                        IrType::IntLiteral => ColType::Int,
                        IrType::FloatLiteral => ColType::Float,
                        IrType::BoolLiteral => ColType::Boolean,
                        IrType::StringLiteral => ColType::String,
                        _ => column_type,
                    };
                }
                let is_length = ir_wrapper::is_ir_in_type(t, id, IrType::BitWithLength)
                    || ir_wrapper::is_ir_in_type(t, id, IrType::CharacterWithLength);
                if is_length {
                    column_type = ColType::Int;
                }
                if column_type == ColType::Unknown {
                    column_type = match self.rng.rand_int(4) {
                        0 => ColType::Int,
                        1 => ColType::Float,
                        2 => ColType::Boolean,
                        _ => ColType::String,
                    };
                }
                match column_type {
                    ColType::Int | ColType::Unknown => {
                        let value = self.pick_int_literal(&ori_str, is_length);
                        let n = t.get_mut(id);
                        n.value = IrValue::Int(value);
                        n.str_val = value.to_string();
                    }
                    ColType::Float => {
                        let value = self.pick_float_literal(&ori_str);
                        let n = t.get_mut(id);
                        n.value = IrValue::Float(value);
                        n.str_val = float_str(value);
                        n.ty = IrType::FloatLiteral;
                    }
                    ColType::Boolean => {
                        let s = if self.rng.rand_int(100) < 50 {
                            "TRUE"
                        } else {
                            "FALSE"
                        };
                        let n = t.get_mut(id);
                        n.str_val = s.to_string();
                        n.ty = IrType::BoolLiteral;
                    }
                    ColType::String => {
                        if !self.ctx.v_string_literals.is_empty() && self.rng.rand_int(10) < 9 {
                            let s = self.ctx.v_string_literals
                                [self.rng.rand_int(self.ctx.v_string_literals.len())]
                            .clone();
                            // The C++ kept the old node type here, so an int
                            // literal became an unquoted word.
                            let n = t.get_mut(id);
                            n.str_val = s;
                            n.ty = IrType::StringLiteral;
                            continue;
                        }
                        let s = self.get_a_string();
                        self.ctx.v_string_literals.push(s.clone());
                        let n = t.get_mut(id);
                        n.str_val = s;
                        n.ty = IrType::StringLiteral;
                    }
                }
            }

            // Reloptions.
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                if t.data_type(id) != DataType::RelOption {
                    continue;
                }
                fixed.insert(id);
                let ori_str = t.to_string(id);
                if !self.ctx.v_saved_reloption_str.contains(&ori_str) {
                    self.ctx.v_saved_reloption_str.push(ori_str);
                }
                if self.rng.rand_int(100) < 99 {
                    continue;
                }
                if self.rng.rand_int(5) < 4 && !self.ctx.v_saved_reloption_str.is_empty() {
                    let s = self.ctx.v_saved_reloption_str
                        [self.rng.rand_int(self.ctx.v_saved_reloption_str.len())]
                    .clone();
                    let new_node = t.leaf(IrType::ReloptionElem, s);
                    t.swap_node(stmt, id, Some(new_node));
                    to_drop.push(id);
                    continue;
                }
                let option_type = Mutator::rel_option_type(t, id);
                let Some(option) = relopt::get_rel_option_pair(&mut self.rng, option_type) else {
                    continue;
                };
                let new_node = if option.is_reset {
                    Mutator::new_reloption_node(t, &option.name, None)
                } else {
                    Mutator::new_reloption_node(t, &option.name, Some(&option.value))
                };
                t.swap_node(stmt, id, Some(new_node));
                to_drop.push(id);
            }

            // Statistics, sequences and constraints.
            for &id in ir_to_fix_vec {
                if fixed.contains(&id) {
                    continue;
                }
                let n = t.get(id);
                let (dt, flag) = (n.data_type, n.data_flag);
                if dt == DataType::StatisticName {
                    match flag {
                        DataFlag::Define => {
                            let name = self.gen_statistic_name();
                            t.set_str_val(id, name.clone());
                            fixed.insert(id);
                            self.ctx.v_statistics_name.push(name);
                        }
                        DataFlag::Undefine => {
                            if self.ctx.v_statistics_name.is_empty() {
                                continue;
                            }
                            let name = self.ctx.v_statistics_name
                                [self.rng.rand_int(self.ctx.v_statistics_name.len())]
                            .clone();
                            t.set_str_val(id, name.clone());
                            fixed.insert(id);
                            remove_value(&mut self.ctx.v_statistics_name, &name);
                        }
                        DataFlag::Use => {
                            if self.ctx.v_statistics_name.is_empty() {
                                continue;
                            }
                            let name = self.ctx.v_statistics_name
                                [self.rng.rand_int(self.ctx.v_statistics_name.len())]
                            .clone();
                            t.set_str_val(id, name);
                            fixed.insert(id);
                        }
                        _ => {}
                    }
                }
                if dt == DataType::SequenceName {
                    fixed.insert(id);
                    Self::fix_simple_name(
                        &mut self.rng,
                        t,
                        id,
                        flag,
                        &mut self.ctx.v_sequence_name,
                    );
                }
                if dt == DataType::ConstraintName {
                    fixed.insert(id);
                    Self::fix_simple_name(
                        &mut self.rng,
                        t,
                        id,
                        flag,
                        &mut self.ctx.v_constraint_name,
                    );
                }
            }
        }

        // Is the created table partitioned?
        let mut is_table_par = false;
        for clause in
            ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::OptPartitionClause, false)
        {
            if t.prefix(clause) == "PARTITION BY" {
                is_table_par = true;
                break;
            }
        }
        if !ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::PartitionSpec, false)
            .is_empty()
        {
            is_table_par = true;
        }
        if is_table_par {
            if let Some(first) = self.ctx.v_create_table_names_single.first().cloned() {
                self.ctx.v_table_with_partition_name.push(first);
            }
        }

        // The tables this statement created are now visible to the next ones.
        let created = self.ctx.v_create_table_names_single.clone();
        self.ctx.v_table_names.extend(created);
        let created_foreign = self.ctx.v_create_foreign_table_names_single.clone();
        self.ctx.v_foreign_table_name.extend(created_foreign);

        // Second pass: column mappings that need the final names.
        for ir_to_fix_vec in ir_to_fix_vecs {
            // Inheritance: the new table has the columns of its parents.
            for &id in ir_to_fix_vec {
                let n = t.get(id);
                if n.data_type == DataType::TableName
                    && stmt_type == IrType::CreateStmt
                    && n.data_flag == DataFlag::Use
                    && ir_wrapper::is_ir_in_type(t, id, IrType::OptInherit)
                {
                    if let Some(new_table) = self.ctx.v_create_table_names_single.first().cloned() {
                        let inherited = self
                            .ctx
                            .m_tables
                            .get(&n.str_val)
                            .cloned()
                            .unwrap_or_default();
                        self.ctx
                            .m_tables
                            .entry(new_table)
                            .or_default()
                            .extend(inherited);
                    }
                }
            }

            // CREATE VIEW ... AS SELECT: the view has the selected columns.
            for &id in ir_to_fix_vec {
                let dt = t.data_type(id);
                if dt != DataType::TableName && dt != DataType::ViewName {
                    continue;
                }
                let mut is_in_create_view = false;
                let mut cur = Some(id);
                while let Some(c) = cur {
                    let ty = t.ty(c);
                    if ty == IrType::Stmt {
                        break;
                    }
                    if ty == IrType::CreateViewStmt || ty == IrType::ViewStmt {
                        is_in_create_view = true;
                        break;
                    }
                    if cur_ir_str.contains("PARTITION OF") && cur_ir_str.contains("CREATE") {
                        is_in_create_view = true;
                        break;
                    }
                    if cur_ir_str.contains("CREATE TABLE") && cur_ir_str.contains("AS SELECT") {
                        is_in_create_view = true;
                        break;
                    }
                    cur = t.parent(c);
                }
                if !is_in_create_view {
                    continue;
                }
                let view_name = t.str_val(id).to_string();
                let mut mentioned = Vec::new();
                collect_ir(t, stmt, &[DataType::ColumnName], &mut mentioned);
                let mut aliases = Vec::new();
                collect_ir(t, stmt, &[DataType::AliasName], &mut aliases);
                mentioned.extend(aliases);
                for col in &mentioned {
                    let mut name = t.str_val(*col).to_string();
                    if name.contains('.') {
                        name = name.split('.').nth(1).unwrap_or("").to_string();
                    }
                    let cols = self.ctx.m_tables.entry(view_name.clone()).or_default();
                    if !cols.contains(&name) {
                        cols.push(name);
                    }
                }
                if mentioned.is_empty() {
                    let mut tables = Vec::new();
                    collect_ir(t, stmt, &[DataType::TableName], &mut tables);
                    for table in tables {
                        if t.data_flag(table) != DataFlag::Use {
                            continue;
                        }
                        let source_cols = self
                            .ctx
                            .m_tables
                            .get(t.str_val(table))
                            .cloned()
                            .unwrap_or_default();
                        let cols = self.ctx.m_tables.entry(view_name.clone()).or_default();
                        for c in source_cols {
                            if !cols.contains(&c) {
                                cols.push(c);
                            }
                        }
                    }
                }
                break;
            }
        }

        for id in to_drop {
            t.deep_drop(id);
        }
        true
    }

    /// Define, remove, or use a name from a plain list (sequences, constraints).
    /// Definitions keep the original name, like the C++.
    fn fix_simple_name(
        rng: &mut crate::rng::Rng,
        t: &mut IrTree,
        id: IrId,
        flag: DataFlag,
        names: &mut Vec<String>,
    ) {
        match flag {
            DataFlag::Define => {
                names.push(t.str_val(id).to_string());
            }
            DataFlag::Undefine => {
                if names.is_empty() {
                    return;
                }
                let name = names[rng.rand_int(names.len())].clone();
                t.set_str_val(id, name.clone());
                remove_value(names, &name);
            }
            DataFlag::Use => {
                if names.is_empty() {
                    return;
                }
                let name = names[rng.rand_int(names.len())].clone();
                t.set_str_val(id, name);
            }
            _ => {}
        }
    }

    /// A new integer literal, close to the original most of the time.
    fn pick_int_literal(&mut self, ori_str: &str, is_length: bool) -> i64 {
        if is_length {
            // Sizes of columns and strings stay small.
            return self.rng.rand_int(100) as i64;
        }
        if !self.ctx.v_int_literals.is_empty() && self.rng.rand_int(10) < 9 {
            return self.ctx.v_int_literals[self.rng.rand_int(self.ctx.v_int_literals.len())];
        }
        if self.rng.rand_int(5) < 4 {
            let ori_int: i64 = ori_str.trim().parse().unwrap_or(-1);
            if (0..=7).contains(&ori_int) && self.rng.rand_int(5) < 4 {
                let v = self.rng.rand_int(8) as i64;
                self.ctx.v_int_literals.push(v);
                return v;
            }
            let v = if ori_int < -10000 {
                if self.rng.rand_int(2) < 1 {
                    self.rng.rand_range(i32::MIN as i64, -10000)
                } else {
                    self.rng.rand_range(-10000, 10000)
                }
            } else if ori_int < -10 {
                self.rng.rand_range(-10000, -10)
            } else if ori_int < 0 {
                self.rng.rand_range(-10, 0)
            } else if ori_int < 10000 {
                self.rng.rand_range(0, 10)
            } else if self.rng.rand_int(2) < 1 {
                self.rng.rand_range(10000, i32::MAX as i64)
            } else {
                self.rng.rand_range(-10000, 10000)
            };
            self.ctx.v_int_literals.push(v);
            return v;
        }
        let v = if self.rng.rand_int(5) < 4 && !self.value_library.is_empty() {
            // The C++ stores the value in an int, which cuts it to 32 bits.
            self.value_library[self.rng.rand_int(self.value_library.len())] as i32 as i64
        } else {
            self.rng.rand_i64(i32::MAX as i64)
        };
        self.ctx.v_int_literals.push(v);
        v
    }

    /// A new float literal, close to the original most of the time.
    fn pick_float_literal(&mut self, ori_str: &str) -> f64 {
        if !self.ctx.v_float_literals.is_empty() && self.rng.rand_int(10) < 9 {
            return self.ctx.v_float_literals[self.rng.rand_int(self.ctx.v_float_literals.len())];
        }
        let v = if self.rng.rand_int(100) < 95 {
            // The C++ parses the original with stoi, so "1.5" reads as 1.
            let int_part: Option<i64> = ori_str
                .trim()
                .split(|c: char| !c.is_ascii_digit() && c != '-')
                .next()
                .and_then(|s| s.parse().ok());
            match int_part {
                None => self.rng.rand_double(0.0, f64::MAX),
                Some(i) => {
                    let ori = i as f64;
                    if ori < -10000.0 {
                        if self.rng.rand_int(2) < 1 {
                            self.rng.rand_double(-f64::MIN_POSITIVE, -10000.0)
                        } else {
                            self.rng.rand_double(-10000.0, 10000.0)
                        }
                    } else if ori < -10.0 {
                        self.rng.rand_double(-10000.0, -10.0)
                    } else if ori < 0.0 {
                        self.rng.rand_double(-10.0, 0.0)
                    } else if ori < 10.0 {
                        self.rng.rand_double(0.0, 10.0)
                    } else if ori < 10000.0 {
                        self.rng.rand_double(10.0, 10000.0)
                    } else if self.rng.rand_int(2) < 1 {
                        self.rng.rand_double(10000.0, f64::MAX)
                    } else {
                        self.rng.rand_double(-10000.0, 10000.0)
                    }
                }
            }
        } else {
            self.rng.rand_double(0.0, f64::MAX)
        };
        self.ctx.v_float_literals.push(v);
        v
    }
}

/// True for the text of a result marker: `BEGIN VERI n` or `END VERI n`, with
/// or without quotes around it.
pub(crate) fn is_marker_text(s: &str) -> bool {
    let s = s.trim().trim_matches('\'').trim();
    s.strip_prefix("BEGIN VERI ")
        .or_else(|| s.strip_prefix("END VERI "))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// True for the statement `SELECT 'BEGIN VERI n'` or `SELECT 'END VERI n'`.
/// The harness prints such a statement as a marker instead of running it, so
/// the mutator must not put one among the normal statements of a test case.
pub(crate) fn is_marker_statement(stmt: &str) -> bool {
    let s = stmt.trim().trim_end_matches(';').trim();
    let Some(head) = s.get(..6) else {
        return false;
    };
    if !head.eq_ignore_ascii_case("select") {
        return false;
    }
    let rest = s[6..].trim();
    rest.len() >= 2 && rest.starts_with('\'') && rest.ends_with('\'') && is_marker_text(rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::{AllCompRes, Oracle};
    use sqlright_pg_parser::parse_query;

    #[test]
    fn finds_the_text_of_a_marker() {
        assert!(is_marker_text("BEGIN VERI 0"));
        assert!(is_marker_text("'END VERI 12'"));
        assert!(!is_marker_text("END VERI"));
        assert!(!is_marker_text("END VERI x"));
        assert!(!is_marker_text("sample"));
    }

    #[test]
    fn finds_a_marker_statement() {
        assert!(is_marker_statement("SELECT 'BEGIN VERI 0';"));
        assert!(is_marker_statement("  select 'END VERI 3' ; "));
        // A marker literal inside a bigger statement is not a marker statement.
        assert!(!is_marker_statement("SELECT 'END VERI 1' FROM t;"));
        assert!(!is_marker_statement("SELECT 'sample';"));
        assert!(!is_marker_statement("CREATE TABLE t (a int);"));
        assert!(!is_marker_statement("SELECT"));
    }

    #[test]
    fn a_marker_statement_does_not_enter_the_library() {
        let mut m = Mutator::new(1);
        m.add_all_to_library(
            &AnySelect,
            "SELECT 'BEGIN VERI 1'; SELECT 'END VERI 0'; CREATE TABLE t (a int);",
        );
        let in_library = |word: &str| {
            m.real_ir_set
                .values()
                .flatten()
                .any(|entry| entry.query.contains(word))
        };
        assert!(in_library("CREATE TABLE"));
        assert!(!in_library("VERI"));
    }

    struct AnySelect;
    impl Oracle for AnySelect {
        fn oracle_type(&self) -> &'static str {
            "TEST"
        }
        fn template_select_stmt(&self, _m: &mut Mutator) -> String {
            "SELECT COUNT(*) FROM x WHERE x = 0;".to_string()
        }
        fn is_oracle_select_stmt(&self, t: &mut IrTree, stmt: IrId) -> bool {
            t.ty(stmt) == IrType::SelectStmt
        }
        fn post_fix_transform_select_stmt(
            &self,
            _m: &mut Mutator,
            t: &mut IrTree,
            stmt: IrId,
            _run: usize,
        ) -> Vec<IrTree> {
            vec![t.extract(stmt)]
        }
        fn compare_results(&self, _res: &mut AllCompRes) {}
    }

    #[test]
    fn names_follow_the_context() {
        let oracle = AnySelect;
        let mut m = Mutator::new(7);
        let sql = "CREATE TABLE t (a INT, b TEXT); INSERT INTO t VALUES (1, 'x'); SELECT a FROM t WHERE b = 'y'; DROP TABLE t;";
        let mut q = parse_query(sql).unwrap();
        m.pre_validate();
        let (mut trees, types) = m.pre_fix_transform(&oracle, &mut q.tree, q.root);
        assert_eq!(
            types,
            vec![
                StmtType::NotOracle,
                StmtType::NotOracle,
                StmtType::OracleSelect,
                StmtType::NotOracle
            ]
        );
        for tree in trees.iter_mut() {
            let r = tree.root.unwrap();
            m.validate(tree, r);
        }
        let out: Vec<String> = trees.iter().map(|t| t.to_string(t.root.unwrap())).collect();
        assert!(
            out[0].starts_with("CREATE TABLE v0 ( c1 INT, c2 text )"),
            "{}",
            out[0]
        );
        assert!(out[1].starts_with("INSERT INTO v0 VALUES"), "{}", out[1]);
        assert!(out[2].contains("FROM v0"), "{}", out[2]);
        assert!(
            out[2].starts_with("SELECT c1") || out[2].starts_with("SELECT c2"),
            "{}",
            out[2]
        );
        assert_eq!(out[3], "DROP TABLE v0");
        assert_eq!(m.ctx.v_table_names, Vec::<String>::new());
        let runs = m.post_fix_transform(&oracle, &mut trees, &types);
        let (marked, unmarked) = Mutator::ir_to_string(&runs[0]);
        assert!(marked.contains("SELECT 'BEGIN VERI 0'"));
        assert!(!unmarked.contains("VERI"));
    }

    use crate::mutator::StmtType;
}
