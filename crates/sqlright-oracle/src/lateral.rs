//! The LATERAL oracle: a LATERAL join must return the same rows as its
//! JSON form. A port of the `LateralJsonOracle` of SQLancer
//! (LeMikaelF/sqlancer, branch `claude/sqlancer-oracle-mysql-postgres-6i35zm`),
//! which is itself a port of the `LateralMatchesJsonEach` property of the
//! turso simulator.
//!
//! The oracle takes `SELECT ... FROM t [AS a] WHERE p` and builds a query
//! with one or two LATERAL joins. The predicate `p` is the filter of the
//! first join. The outer table, the correlation and the selected columns
//! come from the tables of the test case (the instantiation context):
//!
//! ```sql
//! SELECT o.c1, s0.j0 FROM v1 AS o
//! LEFT JOIN LATERAL (SELECT i0.c2 AS j0 FROM v0 AS i0 WHERE (i0.c3 < o.c1) AND (p)) AS s0 ON TRUE
//! WHERE TRUE;
//! ```
//!
//! The JSON form of a join collects the rows of the subquery into one JSON
//! array and turns the array back into rows with a table function. It
//! declares the type of every column, so that the values do not change:
//!
//! ```sql
//! LEFT JOIN json_to_recordset((SELECT json_group_array(json_object('j0', i0.c2))
//!   FROM v0 AS i0 WHERE (i0.c3 < o.c1) AND (p))) AS s0(j0 INT) ON TRUE
//! ```
//!
//! tursopg has no `json_agg` and no `json_build_object`; the turso
//! functions `json_group_array` and `json_object` do the same work. A
//! column definition list cannot have a type modifier, so the declared
//! type is the type name without its parameters. A subquery with DISTINCT,
//! ORDER BY or LIMIT keeps those in a derived table, and the JSON array
//! collects its rows. The two forms must return the same rows, in any order.

use sqlright_ir::{IrId, IrTree, IrType};
use sqlright_mutator::fix::FixContext;
use sqlright_mutator::ir_wrapper;
use sqlright_mutator::oracle::{AllCompRes, OraCompRes, Oracle};
use sqlright_mutator::rng::Rng;
use sqlright_mutator::Mutator;
use sqlright_pg_parser::parse_query;

use crate::shape::{
    compare_row_sets, has_aggregate_call, has_clause, has_distinct, has_window_function,
    single_table, where_expr,
};

pub struct Lateral;

const TEMPLATES: &[&str] = &[
    "SELECT x FROM x WHERE x = 0;",
    "SELECT x FROM x WHERE x > 0 AND x < 10;",
    "SELECT x FROM x WHERE x IS NOT NULL;",
    "SELECT x FROM x AS a WHERE a.x <> 0 OR a.x IS NULL;",
    "SELECT x FROM x WHERE x IN (0, 1, 2);",
    "SELECT x FROM x WHERE x LIKE 'x';",
    "SELECT x FROM x WHERE x = (SELECT MAX(x) FROM x);",
];

const OUTER_ALIAS: &str = "o";
const ROWS_ALIAS: &str = "q";
const MAX_SELECTED_COLUMNS: usize = 3;
const MAX_LIMIT: u64 = 3;

/// The values of two columns of the same group compare without a
/// conversion that depends on the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Number,
    Text,
    Bool,
    Date,
    Timestamp,
    Time,
    Other,
}

#[derive(Debug, Clone)]
struct Column {
    name: String,
    /// The type name without its parameters, for the column definition
    /// list of `json_to_recordset`.
    declared_type: String,
    group: Group,
    /// False when the JSON form cannot return the exact values of the
    /// column (`NUMERIC` loses its scale, `UUID` cannot go into JSON).
    selectable: bool,
}

#[derive(Debug, Clone)]
struct Table {
    name: String,
    columns: Vec<Column>,
}

/// A column that a subquery can read: a column of the outer table, or a
/// column that an earlier join returns.
#[derive(Debug, Clone)]
struct Visible {
    text: String,
    declared_type: String,
    group: Group,
    selectable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JoinType {
    Comma,
    Cross,
    Inner,
    Left,
}

#[derive(Debug, Clone)]
struct Join {
    kind: JoinType,
    on: String,
    alias: String,
    sub: Subquery,
}

/// A table of a subquery with the comparisons of its ON clause (empty
/// for the first table).
#[derive(Debug, Clone)]
struct SubqueryTable {
    name: String,
    alias: String,
    on: Vec<String>,
}

#[derive(Debug, Clone)]
struct Subquery {
    tables: Vec<SubqueryTable>,
    distinct: bool,
    /// The selected expressions with their declared types.
    columns: Vec<(String, String)>,
    correlation: String,
    filter_with_or: bool,
    filter: String,
    order_by: Vec<(usize, bool)>,
    limit: Option<u64>,
}

impl Subquery {
    fn changes_the_rows(&self) -> bool {
        self.distinct || !self.order_by.is_empty() || self.limit.is_some()
    }
}

#[derive(Debug, Clone)]
struct LateralQuery {
    table: String,
    columns: Vec<String>,
    joins: Vec<Join>,
    where_clause: String,
}

const COMPARISON_OPERATORS: &[&str] = &[
    "=",
    "<>",
    "<",
    "<=",
    ">",
    ">=",
    "IS NOT DISTINCT FROM",
    "IS DISTINCT FROM",
];

fn group_of(type_name: &str) -> Group {
    match type_name {
        "int" | "integer" | "int2" | "int4" | "int8" | "smallint" | "bigint" | "tinyint"
        | "mediumint" | "serial" | "bigserial" | "smallserial" | "numeric" | "decimal" | "real"
        | "float" | "float4" | "float8" | "double" | "double precision" => Group::Number,
        "text" | "varchar" | "character varying" | "char" | "character" | "bpchar" | "nchar"
        | "nvarchar" | "name" | "citext" | "clob" => Group::Text,
        "boolean" | "bool" => Group::Bool,
        "date" => Group::Date,
        "timestamp"
        | "timestamp without time zone"
        | "timestamptz"
        | "timestamp with time zone" => Group::Timestamp,
        "time" | "time without time zone" | "timetz" | "time with time zone" => Group::Time,
        _ => Group::Other,
    }
}

/// True when a value of the type comes back unchanged from JSON (tested
/// on tursopg: `NUMERIC` loses its scale, `CHAR` has a modifier always,
/// `UUID` and `JSONB` cannot go into JSON).
fn survives_json(type_name: &str) -> bool {
    matches!(
        type_name,
        "int"
            | "integer"
            | "int2"
            | "int4"
            | "int8"
            | "smallint"
            | "bigint"
            | "real"
            | "float4"
            | "float8"
            | "double precision"
            | "text"
            | "varchar"
            | "character varying"
            | "boolean"
            | "bool"
            | "date"
            | "timestamp"
            | "timestamp without time zone"
            | "time"
            | "time without time zone"
    )
}

/// The type name without parameters, in lowercase with single spaces:
/// `NUMERIC(10, 2)` gives `numeric`.
fn base_type_name(typename: &str) -> String {
    let no_params = match typename.find('(') {
        Some(pos) => &typename[..pos],
        None => typename,
    };
    no_params
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn column_of(name: &str, typename: &str) -> Column {
    let declared_type = base_type_name(typename);
    Column {
        name: name.to_string(),
        group: group_of(&declared_type),
        selectable: survives_json(&declared_type),
        declared_type,
    }
}

/// The tables of the test case whose columns have a known type.
fn tables_of(ctx: &FixContext) -> Vec<Table> {
    let mut tables = Vec::new();
    for name in &ctx.v_table_names {
        if ctx.v_table_without_column_types.contains(name) {
            continue;
        }
        let Some(cols) = ctx.m_tables.get(name) else {
            continue;
        };
        let columns: Vec<Column> = cols
            .iter()
            .filter_map(|c| ctx.m_column2typename.get(c).map(|ty| column_of(c, ty)))
            .collect();
        if !columns.is_empty() {
            tables.push(Table {
                name: name.clone(),
                columns,
            });
        }
    }
    tables
}

fn column_name(position: usize) -> String {
    format!("j{position}")
}

fn join_alias(position: usize) -> String {
    format!("s{position}")
}

/// Build a random LATERAL query around the predicate of the oracle SELECT.
struct Generator<'a> {
    rng: &'a mut Rng,
    tables: &'a [Table],
}

impl Generator<'_> {
    fn generate(&mut self, inner: &Table, inner_alias: &str, predicate: &str) -> LateralQuery {
        let outer = self.tables[self.rng.rand_int(self.tables.len())].clone();
        let mut visible: Vec<Visible> = outer
            .columns
            .iter()
            .map(|c| Visible {
                text: format!("{OUTER_ALIAS}.{}", c.name),
                declared_type: c.declared_type.clone(),
                group: c.group,
                selectable: c.selectable,
            })
            .collect();
        let mut joins = Vec::new();
        let first = self.join(0, inner, inner_alias, predicate, &outer, &visible);
        if let Some(join) = first {
            self.make_visible(0, &join, &mut visible);
            joins.push(join);
            if self.rng.chance(1, 2) {
                let second_table = self.tables[self.rng.rand_int(self.tables.len())].clone();
                let filter = self.true_or_predicate(&second_table, "i1");
                if let Some(join) = self.join(1, &second_table, "i1", &filter, &outer, &visible) {
                    self.make_visible(1, &join, &mut visible);
                    joins.push(join);
                }
            }
        }
        let column_count = 1 + self
            .rng
            .rand_int(outer.columns.len().min(MAX_SELECTED_COLUMNS));
        let columns = self
            .pick(&outer.columns, column_count)
            .iter()
            .map(|c| format!("{OUTER_ALIAS}.{}", c.name))
            .collect();
        let where_clause = self.true_or_predicate(&outer, OUTER_ALIAS);
        LateralQuery {
            table: outer.name.clone(),
            columns,
            joins: keep_commas_at_the_end(joins),
            where_clause,
        }
    }

    fn make_visible(&mut self, position: usize, join: &Join, visible: &mut Vec<Visible>) {
        for (column, (_, declared_type)) in join.sub.columns.iter().enumerate() {
            visible.push(Visible {
                text: format!("{}.{}", join_alias(position), column_name(column)),
                declared_type: declared_type.clone(),
                group: group_of(declared_type),
                selectable: true,
            });
        }
    }

    /// One LATERAL join whose subquery reads `table` (and maybe one more
    /// table) with `filter` as its filter.
    fn join(
        &mut self,
        position: usize,
        table: &Table,
        alias: &str,
        filter: &str,
        outer: &Table,
        visible: &[Visible],
    ) -> Option<Join> {
        let kind = [
            JoinType::Comma,
            JoinType::Cross,
            JoinType::Inner,
            JoinType::Left,
        ][self.rng.rand_int(4)];
        let mut local: Vec<Visible> = table
            .columns
            .iter()
            .map(|c| Visible {
                text: format!("{alias}.{}", c.name),
                declared_type: c.declared_type.clone(),
                group: c.group,
                selectable: c.selectable,
            })
            .collect();
        let mut tables = vec![SubqueryTable {
            name: table.name.clone(),
            alias: alias.to_string(),
            on: Vec::new(),
        }];
        // A second table in the subquery. Not the table itself: a bare
        // column name in the filter would then be ambiguous.
        let other_tables: Vec<Table> = self
            .tables
            .iter()
            .filter(|t| t.name != table.name)
            .cloned()
            .collect();
        if !other_tables.is_empty() && self.rng.chance(1, 3) {
            let joined = other_tables[self.rng.rand_int(other_tables.len())].clone();
            let joined_alias = format!("{alias}_1");
            let joined_columns: Vec<Visible> = joined
                .columns
                .iter()
                .map(|c| Visible {
                    text: format!("{joined_alias}.{}", c.name),
                    declared_type: c.declared_type.clone(),
                    group: c.group,
                    selectable: c.selectable,
                })
                .collect();
            let mut on = Vec::new();
            if let Some(cmp) = self.comparison(&joined_columns, &local, true) {
                on.push(cmp);
            }
            if on.is_empty() || self.rng.chance(1, 2) {
                if let Some(cmp) = self.comparison(&joined_columns, visible, false) {
                    on.push(cmp);
                }
            }
            if !on.is_empty() {
                tables.push(SubqueryTable {
                    name: joined.name.clone(),
                    alias: joined_alias,
                    on,
                });
                local.extend(joined_columns);
            }
        }
        let correlation = self.comparison(&local, visible, false)?;
        let distinct = self.rng.chance(1, 5);
        let limit = if self.rng.chance(3, 10) {
            Some(1 + self.rng.rand_int(MAX_LIMIT as usize) as u64)
        } else {
            None
        };
        let canonical = distinct || limit.is_some();
        let selectable_local: Vec<Visible> = local
            .iter()
            .filter(|c| {
                c.selectable
                    && (!canonical || c.group != Group::Number || is_integer(&c.declared_type))
            })
            .cloned()
            .collect();
        if selectable_local.is_empty() {
            return None;
        }
        let count = 1 + self
            .rng
            .rand_int(selectable_local.len().min(MAX_SELECTED_COLUMNS));
        let mut selected: Vec<Visible> = self.pick(&selectable_local, count);
        let can_select_outer = kind != JoinType::Left || self.rng.chance(1, 2);
        if can_select_outer && self.rng.chance(3, 10) {
            let outer_selectable: Vec<Visible> =
                visible.iter().filter(|c| c.selectable).cloned().collect();
            if let Some(c) = self.rng.choose(&outer_selectable) {
                selected.push(c.clone());
            }
        }
        let columns = selected
            .iter()
            .map(|c| (c.text.clone(), c.declared_type.clone()))
            .collect::<Vec<_>>();
        let order_by = if limit.is_some() {
            self.order_by(columns.len(), columns.len())
        } else if self.rng.chance(1, 5) {
            let keys = 1 + self.rng.rand_int(columns.len());
            self.order_by(columns.len(), keys)
        } else {
            Vec::new()
        };
        let on = match kind {
            JoinType::Inner | JoinType::Left => self.true_or_predicate(outer, OUTER_ALIAS),
            JoinType::Comma | JoinType::Cross => String::new(),
        };
        Some(Join {
            kind,
            on,
            alias: join_alias(position),
            sub: Subquery {
                tables,
                distinct,
                columns,
                correlation,
                filter_with_or: self.rng.chance(3, 10),
                filter: filter.to_string(),
                order_by,
                limit,
            },
        })
    }

    /// `(a op b)` for a column of `left` and a column of `right` whose
    /// values compare. With `equal`, the operator is `=`.
    fn comparison(&mut self, left: &[Visible], right: &[Visible], equal: bool) -> Option<String> {
        let pairs: Vec<(&Visible, &Visible)> = left
            .iter()
            .flat_map(|l| {
                right
                    .iter()
                    .filter(move |r| l.group == r.group && l.group != Group::Other)
                    .map(move |r| (l, r))
            })
            .collect();
        let (l, r) = *self.rng.choose(&pairs)?;
        let op = if equal || self.rng.chance(1, 2) {
            "="
        } else {
            COMPARISON_OPERATORS[self.rng.rand_int(COMPARISON_OPERATORS.len())]
        };
        if self.rng.chance(1, 2) {
            Some(format!("({} {op} {})", l.text, r.text))
        } else {
            Some(format!("({} {op} {})", r.text, l.text))
        }
    }

    fn order_by(&mut self, column_count: usize, keys: usize) -> Vec<(usize, bool)> {
        let positions: Vec<usize> = (0..column_count).collect();
        self.pick(&positions, keys)
            .into_iter()
            .map(|p| (p, self.rng.chance(1, 2)))
            .collect()
    }

    /// `TRUE`, or a small predicate on a column of the table.
    fn true_or_predicate(&mut self, table: &Table, alias: &str) -> String {
        if self.rng.chance(1, 2) || table.columns.is_empty() {
            return "TRUE".to_string();
        }
        let column = &table.columns[self.rng.rand_int(table.columns.len())];
        let reference = format!("{alias}.{}", column.name);
        match self.rng.rand_int(4) {
            0 => format!("{reference} IS NOT NULL"),
            1 => format!("{reference} IS NULL"),
            2 => format!("{reference} = {reference}"),
            _ => {
                let other = &table.columns[self.rng.rand_int(table.columns.len())];
                if other.group == column.group && other.group != Group::Other {
                    let op = COMPARISON_OPERATORS[self.rng.rand_int(COMPARISON_OPERATORS.len())];
                    format!("{reference} {op} {alias}.{}", other.name)
                } else {
                    format!("{reference} IS NOT NULL")
                }
            }
        }
    }

    /// `count` random distinct items, in the order of `items`.
    fn pick<T: Clone>(&mut self, items: &[T], count: usize) -> Vec<T> {
        let mut indexes: Vec<usize> = (0..items.len()).collect();
        for i in (1..indexes.len()).rev() {
            let j = self.rng.rand_int(i + 1);
            indexes.swap(i, j);
        }
        indexes.truncate(count.min(items.len()));
        indexes.sort_unstable();
        indexes.into_iter().map(|i| items[i].clone()).collect()
    }
}

fn is_integer(declared_type: &str) -> bool {
    matches!(
        declared_type,
        "int" | "integer" | "int2" | "int4" | "int8" | "smallint" | "bigint"
    )
}

/// A comma join after another kind of join is a syntax error, so the
/// comma joins before it become CROSS JOINs.
fn keep_commas_at_the_end(mut joins: Vec<Join>) -> Vec<Join> {
    let mut later_is_not_comma = false;
    for join in joins.iter_mut().rev() {
        if join.kind == JoinType::Comma && later_is_not_comma {
            join.kind = JoinType::Cross;
        } else if join.kind != JoinType::Comma {
            later_is_not_comma = true;
        }
    }
    joins
}

// -- printing --------------------------------------------------------------------

fn lateral_query(query: &LateralQuery) -> String {
    let mut sql = query_head(query);
    for join in &query.joins {
        sql.push_str(&lateral_join(join));
    }
    sql.push_str(&format!(" WHERE {}", query.where_clause));
    sql
}

/// The query with the joins at `json_joins` in the JSON form.
fn json_query(query: &LateralQuery, json_joins: &[usize]) -> String {
    let mut sql = query_head(query);
    for (position, join) in query.joins.iter().enumerate() {
        if json_joins.contains(&position) {
            sql.push_str(&join_clause(join, &json_table(join)));
        } else {
            sql.push_str(&lateral_join(join));
        }
    }
    sql.push_str(&format!(" WHERE {}", query.where_clause));
    sql
}

fn query_head(query: &LateralQuery) -> String {
    let mut columns = query.columns.clone();
    for (position, join) in query.joins.iter().enumerate() {
        for column in 0..join.sub.columns.len() {
            columns.push(format!("{}.{}", join_alias(position), column_name(column)));
        }
    }
    format!(
        "SELECT {} FROM {} AS {OUTER_ALIAS}",
        columns.join(", "),
        query.table
    )
}

fn lateral_join(join: &Join) -> String {
    join_clause(
        join,
        &format!("LATERAL ({}) AS {}", subquery(&join.sub), join.alias),
    )
}

fn join_clause(join: &Join, table: &str) -> String {
    match join.kind {
        JoinType::Comma => format!(", {table}"),
        JoinType::Cross => format!(" CROSS JOIN {table}"),
        JoinType::Inner => format!(" INNER JOIN {table} ON {}", join.on),
        JoinType::Left => format!(" LEFT JOIN {table} ON {}", join.on),
    }
}

fn subquery(sub: &Subquery) -> String {
    let columns: Vec<String> = sub
        .columns
        .iter()
        .enumerate()
        .map(|(i, (expr, _))| format!("{expr} AS {}", column_name(i)))
        .collect();
    let mut sql = String::from("SELECT ");
    if sub.distinct {
        sql.push_str("DISTINCT ");
    }
    sql.push_str(&columns.join(", "));
    sql.push_str(&from_and_where(sub));
    if !sub.order_by.is_empty() {
        let terms: Vec<String> = sub
            .order_by
            .iter()
            .map(|(column, descending)| {
                format!(
                    "{} {}",
                    column_name(*column),
                    if *descending { "DESC" } else { "ASC" }
                )
            })
            .collect();
        sql.push_str(&format!(" ORDER BY {}", terms.join(", ")));
    }
    if let Some(limit) = sub.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }
    sql
}

fn from_and_where(sub: &Subquery) -> String {
    let mut sql = String::from(" FROM ");
    for table in &sub.tables {
        if !table.on.is_empty() {
            sql.push_str(" JOIN ");
        }
        sql.push_str(&format!("{} AS {}", table.name, table.alias));
        if !table.on.is_empty() {
            sql.push_str(&format!(" ON {}", table.on.join(" AND ")));
        }
    }
    sql.push_str(&format!(
        " WHERE {} {} ({})",
        sub.correlation,
        if sub.filter_with_or { "OR" } else { "AND" },
        sub.filter
    ));
    sql
}

/// `json_to_recordset((SELECT json_group_array(json_object(...)) ...)) AS s(j0 type, ...)`.
fn json_table(join: &Join) -> String {
    let sub = &join.sub;
    let rows = if sub.changes_the_rows() {
        let values: Vec<String> = (0..sub.columns.len())
            .map(|i| format!("'{}', {ROWS_ALIAS}.{}", column_name(i), column_name(i)))
            .collect();
        format!(
            "SELECT json_group_array(json_object({})) FROM ({}) AS {ROWS_ALIAS}",
            values.join(", "),
            subquery(sub)
        )
    } else {
        let values: Vec<String> = sub
            .columns
            .iter()
            .enumerate()
            .map(|(i, (expr, _))| format!("'{}', {expr}", column_name(i)))
            .collect();
        format!(
            "SELECT json_group_array(json_object({})){}",
            values.join(", "),
            from_and_where(sub)
        )
    };
    let definitions: Vec<String> = sub
        .columns
        .iter()
        .enumerate()
        .map(|(i, (_, declared_type))| format!("{} {declared_type}", column_name(i)))
        .collect();
    format!(
        "json_to_recordset(({rows})) AS {}({})",
        join.alias,
        definitions.join(", ")
    )
}

impl Lateral {
    /// `SELECT ... FROM <one table> WHERE p` without aggregates, grouping,
    /// DISTINCT, LIMIT, window functions or set operators: its predicate
    /// becomes the filter of a LATERAL subquery.
    fn is_plain_select_on_one_table(t: &IrTree, stmt: IrId) -> bool {
        if t.ty(stmt) != IrType::SelectStmt {
            return false;
        }
        if !ir_wrapper::check_node_num(t, stmt, 200) {
            return false;
        }
        if ir_wrapper::is_exist_limit_clause(t, stmt)
            || ir_wrapper::is_exist_set_operator(t, stmt)
            || has_clause(t, stmt, IrType::GroupClause)
            || has_clause(t, stmt, IrType::HavingClause)
            || has_clause(t, stmt, IrType::IntoClause)
            || has_clause(t, stmt, IrType::ForLockingClause)
            || has_clause(t, stmt, IrType::WithClause)
            || has_distinct(t, stmt)
            || has_window_function(t, stmt)
            || has_aggregate_call(t, stmt)
        {
            return false;
        }
        single_table(t, stmt).is_some() && where_expr(t, stmt).is_some()
    }

    /// The two forms as text: the LATERAL query and its JSON form.
    fn forms_text(mutator: &mut Mutator, t: &IrTree, stmt: IrId) -> Option<(String, String)> {
        let table = single_table(t, stmt)?;
        let predicate = t.to_string(where_expr(t, stmt)?);
        let tables = tables_of(&mutator.ctx);
        let inner = tables.iter().find(|x| x.name == table.name)?.clone();
        let inner_alias = table.alias.clone().unwrap_or_else(|| "i0".to_string());
        if [OUTER_ALIAS, ROWS_ALIAS, "i1", "i0_1", "i1_1", "s0", "s1"]
            .contains(&inner_alias.as_str())
        {
            return None;
        }
        let query = {
            let mut generator = Generator {
                rng: &mut mutator.rng,
                tables: &tables,
            };
            generator.generate(&inner, &inner_alias, &predicate)
        };
        if query.joins.is_empty() {
            return None;
        }
        let mut json_joins: Vec<usize> = (0..query.joins.len())
            .filter(|_| mutator.rng.chance(1, 2))
            .collect();
        if json_joins.is_empty() {
            json_joins.push(mutator.rng.rand_int(query.joins.len()));
        }
        Some((lateral_query(&query), json_query(&query, &json_joins)))
    }
}

impl Oracle for Lateral {
    fn oracle_type(&self) -> &'static str {
        "LATERAL"
    }

    fn template_select_stmt(&self, mutator: &mut Mutator) -> String {
        TEMPLATES[mutator.rng.rand_int(TEMPLATES.len())].to_string()
    }

    fn is_oracle_select_stmt(&self, t: &mut IrTree, stmt: IrId) -> bool {
        Self::is_plain_select_on_one_table(t, stmt)
    }

    fn post_fix_transform_select_stmt(
        &self,
        mutator: &mut Mutator,
        t: &mut IrTree,
        stmt: IrId,
        _run: usize,
    ) -> Vec<IrTree> {
        t.get_mut(stmt).parent = None;
        if !Self::is_plain_select_on_one_table(t, stmt) {
            return Vec::new();
        }
        let Some((lateral, json)) = Self::forms_text(mutator, t, stmt) else {
            return Vec::new();
        };
        let mut forms = Vec::new();
        for text in [lateral, json] {
            let Some(q) = parse_query(&format!("{text};")) else {
                return Vec::new();
            };
            let Some(first) = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root) else {
                return Vec::new();
            };
            forms.push(q.tree.extract(first));
        }
        forms
    }

    fn compare_results(&self, all: &mut AllCompRes) {
        all.final_res = Some(OraCompRes::Pass);
        let mut is_all_err = true;
        for res in all.v_res.iter_mut() {
            res.res_str_2 = "EMPTY".to_string();
            res.res_str_3 = "EMPTY".to_string();
            if !compare_row_sets(res) {
                is_all_err = false;
            }
            if res.comp_res == OraCompRes::Fail {
                all.final_res = Some(OraCompRes::Fail);
            }
        }
        if is_all_err && all.final_res != Some(OraCompRes::Fail) {
            all.final_res = Some(OraCompRes::Error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mutator_with_tables(seed: u64) -> Mutator {
        let mut m = Mutator::new(seed);
        for (table, columns) in [
            (
                "v0",
                vec![("c1", "INT"), ("c2", "text"), ("c3", "NUMERIC(10, 2)")],
            ),
            (
                "v1",
                vec![("c4", "bigint"), ("c5", "varchar(20)"), ("c6", "uuid")],
            ),
            ("v2", vec![("c7", "date"), ("c8", "boolean")]),
        ] {
            m.ctx.v_table_names.push(table.to_string());
            m.ctx.m_tables.insert(
                table.to_string(),
                columns.iter().map(|(c, _)| c.to_string()).collect(),
            );
            for (c, ty) in columns {
                m.ctx
                    .m_column2typename
                    .insert(c.to_string(), ty.to_string());
            }
        }
        m
    }

    fn forms(sql: &str, seed: u64) -> Vec<String> {
        let mut m = mutator_with_tables(seed);
        let mut q = parse_query(sql).unwrap();
        let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        Lateral
            .post_fix_transform_select_stmt(&mut m, &mut q.tree, stmt, 0)
            .iter()
            .map(|f| f.to_string(f.root.unwrap()))
            .collect()
    }

    #[test]
    fn a_table_made_from_a_select_is_not_used() {
        let mut m = mutator_with_tables(1);
        let names = |m: &Mutator| {
            tables_of(&m.ctx)
                .iter()
                .map(|t| t.name.clone())
                .collect::<Vec<_>>()
        };
        assert!(names(&m).contains(&"v1".to_string()));
        m.ctx.v_table_without_column_types.push("v1".to_string());
        assert!(!names(&m).contains(&"v1".to_string()));
        assert!(names(&m).contains(&"v0".to_string()));
    }

    #[test]
    fn classifies_types() {
        assert_eq!(base_type_name("NUMERIC(10, 2)"), "numeric");
        assert_eq!(base_type_name("double  precision"), "double precision");
        assert_eq!(group_of("numeric"), Group::Number);
        assert_eq!(group_of("varchar"), Group::Text);
        assert_eq!(group_of("uuid"), Group::Other);
        assert!(survives_json("int"));
        assert!(!survives_json("numeric"));
        assert!(!survives_json("char"));
        assert!(!survives_json("uuid"));
    }

    #[test]
    fn makes_a_lateral_query_and_its_json_form_that_both_parse() {
        let mut seen_json = false;
        let mut seen_left = false;
        for seed in 1..60 {
            let texts = forms("SELECT c1 FROM v0 AS a0 WHERE a0.c1 > 1 OR c2 = 'x';", seed);
            if texts.is_empty() {
                continue;
            }
            assert_eq!(texts.len(), 2, "seed {seed}");
            let (lateral, json) = (&texts[0], &texts[1]);
            assert!(lateral.contains("LATERAL ("), "{lateral}");
            assert!(lateral.contains("FROM v0 AS a0"), "{lateral}");
            assert!(lateral.contains("a0 . c1 > 1 OR c2 = 'x'"), "{lateral}");
            assert!(json.contains("json_to_recordset"), "{json}");
            assert!(json.contains("json_group_array ( json_object ("), "{json}");
            assert!(
                !json.contains("numeric"),
                "numeric columns are not selected: {json}"
            );
            assert!(
                !json.contains("uuid"),
                "uuid columns are not selected: {json}"
            );
            assert!(parse_query(&format!("{lateral};")).is_some(), "{lateral}");
            assert!(parse_query(&format!("{json};")).is_some(), "{json}");
            seen_json = true;
            seen_left |= lateral.contains("LEFT JOIN LATERAL");
        }
        assert!(seen_json);
        assert!(seen_left);
    }

    #[test]
    fn never_joins_the_inner_table_with_itself() {
        for seed in 1..80 {
            let texts = forms("SELECT c1 FROM v0 WHERE c1 > 1;", seed);
            let Some(lateral) = texts.first() else {
                continue;
            };
            assert!(!lateral.contains("FROM v0 AS i0 JOIN v0 AS"), "{lateral}");
        }
    }

    #[test]
    fn gives_no_forms_for_an_unknown_table() {
        assert!(forms("SELECT c1 FROM v9 WHERE c1 > 1;", 1).is_empty());
    }

    #[test]
    fn prints_the_two_forms_of_a_fixed_query() {
        let join = Join {
            kind: JoinType::Left,
            on: "TRUE".into(),
            alias: "s0".into(),
            sub: Subquery {
                tables: vec![SubqueryTable {
                    name: "v0".into(),
                    alias: "i0".into(),
                    on: Vec::new(),
                }],
                distinct: false,
                columns: vec![("i0.c2".into(), "text".into())],
                correlation: "(i0.c1 < o.c4)".into(),
                filter_with_or: false,
                filter: "i0.c1 > 0".into(),
                order_by: Vec::new(),
                limit: None,
            },
        };
        let query = LateralQuery {
            table: "v1".into(),
            columns: vec!["o.c4".into()],
            joins: vec![join],
            where_clause: "TRUE".into(),
        };
        assert_eq!(
            lateral_query(&query),
            "SELECT o.c4, s0.j0 FROM v1 AS o LEFT JOIN LATERAL (SELECT i0.c2 AS j0 FROM v0 AS i0 WHERE (i0.c1 < o.c4) AND (i0.c1 > 0)) AS s0 ON TRUE WHERE TRUE"
        );
        assert_eq!(
            json_query(&query, &[0]),
            "SELECT o.c4, s0.j0 FROM v1 AS o LEFT JOIN json_to_recordset((SELECT json_group_array(json_object('j0', i0.c2)) FROM v0 AS i0 WHERE (i0.c1 < o.c4) AND (i0.c1 > 0))) AS s0(j0 text) ON TRUE WHERE TRUE"
        );
        let mut limited = query.clone();
        limited.joins[0].sub.limit = Some(2);
        limited.joins[0].sub.order_by = vec![(0, true)];
        limited.joins[0].kind = JoinType::Comma;
        assert_eq!(
            json_query(&limited, &[0]),
            "SELECT o.c4, s0.j0 FROM v1 AS o, json_to_recordset((SELECT json_group_array(json_object('j0', q.j0)) FROM (SELECT i0.c2 AS j0 FROM v0 AS i0 WHERE (i0.c1 < o.c4) AND (i0.c1 > 0) ORDER BY j0 DESC LIMIT 2) AS q)) AS s0(j0 text) WHERE TRUE"
        );
    }

    #[test]
    fn turns_a_comma_join_before_another_join_into_a_cross_join() {
        let mk = |kind| Join {
            kind,
            on: "TRUE".into(),
            alias: "s".into(),
            sub: Subquery {
                tables: Vec::new(),
                distinct: false,
                columns: Vec::new(),
                correlation: String::new(),
                filter_with_or: false,
                filter: String::new(),
                order_by: Vec::new(),
                limit: None,
            },
        };
        let joins = keep_commas_at_the_end(vec![mk(JoinType::Comma), mk(JoinType::Left)]);
        assert_eq!(joins[0].kind, JoinType::Cross);
        assert_eq!(joins[1].kind, JoinType::Left);
        let joins = keep_commas_at_the_end(vec![mk(JoinType::Left), mk(JoinType::Comma)]);
        assert_eq!(joins[0].kind, JoinType::Left);
        assert_eq!(joins[1].kind, JoinType::Comma);
    }
}
