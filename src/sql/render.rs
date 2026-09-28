//! Renders Toasty's statement AST as T-SQL.
//!
//! T-SQL differs from the dialects `toasty_sql` knows in ways that reach into
//! every clause, which is why this is a renderer rather than a set of tweaks:
//!
//! * identifiers are `[bracket]` quoted, with `]` escaped by doubling it;
//! * bind parameters are named `@p1`, `@p2`, … rather than positional;
//! * `LIMIT` is `OFFSET … ROWS FETCH NEXT … ROWS ONLY`, which requires an
//!   `ORDER BY`;
//! * `RETURNING` is an `OUTPUT INSERTED.<col>` / `OUTPUT DELETED.<col>` clause,
//!   placed after the column list (INSERT) or the `SET` clause (UPDATE);
//! * booleans are `BIT`, so `TRUE`/`FALSE` are `1`/`0`;
//! * `FOR UPDATE` is the `WITH (UPDLOCK, ROWLOCK)` table hint.

use std::fmt::Write as _;

use toasty_core::{
    Error, Result,
    schema::db,
    stmt::{self, BinaryOp, Expr, Returning, Statement},
};

/// Renders a core statement as T-SQL against `schema`.
pub(crate) fn render(schema: &db::Schema, stmt: &Statement) -> Result<String> {
    Renderer::new(schema).statement(stmt)
}

/// Column qualifier used for an `OUTPUT` clause.
const OUTPUT_INSERTED: &str = "INSERTED";
const OUTPUT_DELETED: &str = "DELETED";

/// The aliases a `MERGE` gives the table it writes and the row it proposes.
const MERGE_TARGET: &str = "target";
const MERGE_SOURCE: &str = "src";

/// The collation that makes a comparison case-sensitive.
///
/// SQL Server's default collation is case-insensitive, which is not what
/// Toasty's string predicates promise, so the comparisons that need it opt in
/// to a binary collation.
const CASE_SENSITIVE_COLLATION: &str = "Latin1_General_BIN2";

struct Renderer<'a, 's> {
    schema: &'a db::Schema,
    sql: String,
    /// Nesting depth, used to build distinct table aliases for subqueries.
    depth: usize,
    /// The single table a DML statement writes to. When set, column references
    /// are resolved against it rather than against a `FROM` source.
    dml_table: Option<db::TableId>,
    /// Set while rendering an `OUTPUT` clause, so column references become
    /// `INSERTED.[col]` / `DELETED.[col]`.
    output: Option<&'static str>,
    /// The source tables of the enclosing queries, outermost first. A column
    /// reference names its own nesting level, so an inner query has to be able
    /// to resolve a reference into an outer one.
    sources: Vec<&'s stmt::SourceTable>,
    /// For an INSERT, one flag per target column marking whether the column
    /// is actually written. Generated (`IDENTITY`) columns are dropped:
    /// T-SQL rejects `DEFAULT` or `NULL` as an explicit identity value.
    insert_columns: Option<Vec<bool>>,

    /// Set while the body of a query that asked for a row lock is rendered, so
    /// its `FROM` clause can carry the T-SQL table hint. Consumed by the first
    /// source rendered, which is the query's own.
    row_lock: bool,

    /// Set while a `MERGE` is rendered. The statement has a source relation
    /// whose column names match the table being written, so columns of the
    /// written table have to be qualified to stay unambiguous.
    merge: bool,
}

impl<'a, 's> Renderer<'a, 's> {
    fn new(schema: &'a db::Schema) -> Self {
        Self {
            schema,
            sql: String::new(),
            depth: 0,
            dml_table: None,
            output: None,
            sources: Vec::new(),
            insert_columns: None,
            row_lock: false,
            merge: false,
        }
    }

    fn statement(mut self, stmt: &'s Statement) -> Result<String> {
        match stmt {
            Statement::Query(query) => self.query(query)?,
            Statement::Insert(insert) => self.insert(insert)?,
            Statement::Update(update) => self.update(update)?,
            Statement::Delete(delete) => self.delete(delete)?,
        }

        Ok(self.sql)
    }

    // ---------------------------------------------------------------- writes

    fn insert(&mut self, insert: &'s stmt::Insert) -> Result<()> {
        let stmt::InsertTarget::Table(target) = &insert.target else {
            return Err(unsupported(format!(
                "unsupported insert target: {:?}",
                insert.target
            )));
        };

        // An upsert has two possible outcomes and has to return the row either
        // way, in one statement: `MERGE` does both, an `INSERT` cannot, and
        // choosing a statement after a read is not atomic enough for the engine.
        if insert.upsert.is_some() {
            return self.merge_insert(insert, target);
        }

        self.dml_table = Some(target.table);

        // A generated column must be omitted from the target list entirely:
        // T-SQL rejects both `DEFAULT` and `NULL` as an explicit identity
        // value, so the planner's `DEFAULT` placeholder cannot be sent.
        let keep: Vec<bool> = target
            .columns
            .iter()
            .map(|column| !self.schema.column(*column).auto_increment)
            .collect();

        let table = self.table_name(target.table);
        write!(self.sql, "INSERT INTO {table}").expect("string write");

        let mut written = 0;
        if keep.iter().any(|keep| *keep) {
            self.sql.push_str(" (");
            for (column, keep) in target.columns.iter().zip(&keep) {
                if !keep {
                    continue;
                }

                if written > 0 {
                    self.sql.push_str(", ");
                }
                written += 1;

                let name = self.column_def_name(*column);
                write!(self.sql, "[{name}]").expect("string write");
            }
            self.sql.push(')');
        }

        if let Some(returning) = &insert.returning {
            self.output_clause(returning, OUTPUT_INSERTED)?;
        }

        // When every target column is generated there is nothing to write, and
        // T-SQL spells that `DEFAULT VALUES` rather than empty parentheses.
        if written == 0 {
            self.sql.push_str(" DEFAULT VALUES");
            self.dml_table = None;
            return Ok(());
        }

        self.sql.push(' ');
        self.insert_columns = Some(keep);
        let result = self.query(&insert.source);
        self.insert_columns = None;
        result?;

        self.dml_table = None;

        Ok(())
    }

    /// Renders an upsert as `MERGE`.
    ///
    /// `MERGE` is the only T-SQL statement that chooses between inserting and
    /// updating *and* returns the resulting row from whichever branch ran, in
    /// one round trip. That is what an upsert has to do: `upsert_or_ignore`
    /// needs no row back when the target already exists, and choosing a second
    /// statement after reading is not atomic.
    ///
    /// The conflict target is matched on exactly the lowered columns, so a
    /// conflict on some *other* unique constraint still raises — which is what
    /// `upsert_targeted_ignore` requires.
    fn merge_insert(
        &mut self,
        insert: &'s stmt::Insert,
        target: &'s stmt::InsertTable,
    ) -> Result<()> {
        let upsert = insert.upsert.as_deref().expect("the caller checked");

        // The target is lowered to columns by the time a SQL driver sees it.
        let stmt::UpsertTarget::Columns(conflict) = &upsert.target else {
            return Err(unsupported(format!(
                "SQL Server driver requires a lowered upsert target, found {:?}",
                upsert.target
            )));
        };

        let stmt::ExprSet::Values(rows) = &insert.source.body else {
            return Err(unsupported(
                "SQL Server driver requires an upsert source to be a list of values",
            ));
        };

        // A generated column cannot be written — T-SQL rejects an explicit
        // identity value — so it is left out of the insert branch. It stays in
        // the source, because it may be the column the conflict is matched on.
        let keep: Vec<bool> = target
            .columns
            .iter()
            .map(|column| !self.schema.column(*column).auto_increment)
            .collect();

        self.dml_table = Some(target.table);
        self.merge = true;

        let result = (|| -> Result<()> {
            let table = self.table_name(target.table);
            write!(
                self.sql,
                "MERGE INTO {table} WITH (HOLDLOCK) AS {MERGE_TARGET}"
            )
            .expect("string write");

            self.merge_source(rows, target, upsert)?;

            self.sql.push_str(" ON ");
            for (index, column) in conflict.iter().enumerate() {
                if index > 0 {
                    self.sql.push_str(" AND ");
                }

                let name = self.column_def_name(*column);
                write!(
                    self.sql,
                    "{MERGE_TARGET}.[{name}] = {MERGE_SOURCE}.[{name}]"
                )
                .expect("string write");
            }

            // An ignored conflict runs no action at all, which is T-SQL's
            // `DO NOTHING`: the row is left alone and, because no action ran,
            // `OUTPUT` reports nothing for it.
            if matches!(upsert.action, stmt::UpsertAction::Update) {
                self.sql.push_str(" WHEN MATCHED THEN UPDATE SET ");
                self.merge_update(target, upsert)?;
            }

            self.sql.push_str(" WHEN NOT MATCHED THEN INSERT (");

            let mut written = 0;
            for (column, keep) in target.columns.iter().zip(&keep) {
                if !*keep {
                    continue;
                }

                if written > 0 {
                    self.sql.push_str(", ");
                }
                written += 1;

                let name = self.column_def_name(*column);
                write!(self.sql, "[{name}]").expect("string write");
            }

            self.sql.push_str(") VALUES (");

            let mut written = 0;
            for (column, keep) in target.columns.iter().zip(&keep) {
                if !*keep {
                    continue;
                }

                if written > 0 {
                    self.sql.push_str(", ");
                }
                written += 1;

                let name = self.column_def_name(*column);
                write!(self.sql, "{MERGE_SOURCE}.[{name}]").expect("string write");
            }

            self.sql.push(')');

            if let Some(returning) = &insert.returning {
                self.output_clause(returning, OUTPUT_INSERTED)?;
            }

            // T-SQL insists a `MERGE` end with a semicolon. Every statement is
            // sent on its own, so this is the whole batch rather than a
            // separator, but the server rejects the statement without it.
            self.sql.push(';');

            Ok(())
        })();

        self.merge = false;
        self.dml_table = None;

        result
    }

    /// Renders the `USING (VALUES …) AS src (…)` an upsert's source becomes.
    ///
    /// The rows carry the create branch's values, so the insert branch needs no
    /// expressions of its own: an `on_create` assignment wins over a shared one,
    /// which wins over a declared default, which wins over the value the row was
    /// planned with.
    fn merge_source(
        &mut self,
        rows: &'s stmt::Values,
        target: &'s stmt::InsertTable,
        upsert: &'s stmt::Upsert,
    ) -> Result<()> {
        self.sql.push_str(" USING (VALUES ");

        for (index, row) in rows.rows.iter().enumerate() {
            if index > 0 {
                self.sql.push_str(", ");
            }

            let stmt::Expr::Record(record) = row else {
                return Err(unsupported(format!(
                    "SQL Server driver requires an upsert row to be a record, found {row:?}"
                )));
            };

            self.sql.push('(');

            for (position, column) in target.columns.iter().enumerate() {
                if position > 0 {
                    self.sql.push_str(", ");
                }

                // The row already carries the create branch's values: the
                // planner derives them from the shared assignments before the
                // statement reaches the driver, which is why `shared` does not
                // apply here — it is retained for the conflicting row. Only an
                // explicit `on_create` assignment overrides the row.
                let assignment = upsert_assignment(position, &[&upsert.create]);

                match assignment {
                    Some(stmt::Assignment::Set(expr)) => self.value_of(expr)?,
                    Some(other) => {
                        return Err(unsupported(format!(
                            "SQL Server driver cannot express {other:?} for a column an upsert creates"
                        )));
                    }
                    None => {
                        let field = record.fields.get(position).ok_or_else(|| {
                            unsupported(
                                "SQL Server driver found an upsert row shorter than its column list",
                            )
                        })?;
                        self.value_of(field)?;
                    }
                }

                let _ = column;
            }

            self.sql.push(')');
        }

        write!(self.sql, ") AS {MERGE_SOURCE} (").expect("string write");

        for (position, column) in target.columns.iter().enumerate() {
            if position > 0 {
                self.sql.push_str(", ");
            }

            let name = self.column_def_name(*column);
            write!(self.sql, "[{name}]").expect("string write");
        }

        self.sql.push(')');

        Ok(())
    }

    /// Renders the `SET` list of an upsert's update branch.
    ///
    /// Only the columns an assignment names are touched, and an `on_update`
    /// assignment wins over a shared one, which wins over a `#[update]` default.
    fn merge_update(
        &mut self,
        target: &'s stmt::InsertTable,
        upsert: &'s stmt::Upsert,
    ) -> Result<()> {
        let mut written = 0;

        for (position, column) in target.columns.iter().enumerate() {
            let Some(assignment) = upsert_assignment(
                position,
                &[&upsert.update_defaults, &upsert.shared, &upsert.update],
            ) else {
                continue;
            };

            if written > 0 {
                self.sql.push_str(", ");
            }
            written += 1;

            let name = self.column_def_name(*column);
            self.write_column(name);
            self.sql.push_str(" = ");
            self.assignment(assignment, name)?;
        }

        if written == 0 {
            return Err(unsupported(
                "SQL Server driver requires an upsert that updates a conflicting row to assign something",
            ));
        }

        Ok(())
    }

    fn update(&mut self, update: &'s stmt::Update) -> Result<()> {
        let stmt::UpdateTarget::Table(table_id) = &update.target else {
            return Err(unsupported(format!(
                "unsupported update target: {:?}",
                update.target
            )));
        };

        self.dml_table = Some(*table_id);

        let table = self.table_name(*table_id);
        write!(self.sql, "UPDATE {table} SET ").expect("string write");
        self.assignments(&update.assignments)?;

        if let Some(returning) = &update.returning {
            self.output_clause(returning, OUTPUT_INSERTED)?;
        }

        self.filter(&update.filter)?;
        self.condition(&update.condition)?;

        self.dml_table = None;

        Ok(())
    }

    fn delete(&mut self, delete: &'s stmt::Delete) -> Result<()> {
        let stmt::Source::Table(source) = &delete.from else {
            return Err(unsupported(format!(
                "unsupported delete source: {:?}",
                delete.from
            )));
        };

        let [table_ref] = &source.tables[..] else {
            return Err(unsupported(
                "SQL Server delete with more than one table is not supported",
            ));
        };

        let stmt::TableRef::Table(table_id) = table_ref else {
            return Err(unsupported(format!(
                "unsupported delete table: {table_ref:?}"
            )));
        };

        self.dml_table = Some(*table_id);

        let table = self.table_name(*table_id);
        write!(self.sql, "DELETE FROM {table}").expect("string write");

        if let Some(returning) = &delete.returning {
            self.output_clause(returning, OUTPUT_DELETED)?;
        }

        self.filter(&delete.filter)?;
        self.condition(&delete.condition)?;

        self.dml_table = None;

        Ok(())
    }

    /// Renders `OUTPUT INSERTED.[a], INSERTED.[b]` (or `DELETED`).
    fn output_clause(&mut self, returning: &'s Returning, prefix: &'static str) -> Result<()> {
        self.sql.push_str(" OUTPUT ");
        self.output = Some(prefix);
        let result = self.returning_list(returning);
        self.output = None;
        result
    }

    /// Renders a column of the table being written.
    ///
    /// A plain `INSERT`/`UPDATE`/`DELETE` names its table once, so a bare
    /// `[col]` can only mean that table. A `MERGE` also has a source relation
    /// carrying the same column names, so the written table is qualified.
    fn write_column(&mut self, name: &str) {
        if self.merge {
            write!(self.sql, "{MERGE_TARGET}.[{name}]").expect("string write");
        } else {
            write!(self.sql, "[{name}]").expect("string write");
        }
    }

    fn assignments(&mut self, assignments: &'s stmt::Assignments) -> Result<()> {
        if !assignments.unsupported().is_empty() {
            return Err(unsupported(format!(
                "unsupported update assignment(s): {}",
                assignments.unsupported().join("; ")
            )));
        }

        for (index, (projection, assignment)) in assignments.iter().enumerate() {
            if index > 0 {
                self.sql.push_str(", ");
            }

            // An assignment targets a single column; anything deeper is a
            // document/list update this driver does not render yet.
            let [column] = projection.as_slice() else {
                return Err(unsupported(
                    "SQL Server driver only supports assignments to plain columns",
                ));
            };

            let name = self.dml_column_name(*column)?;
            self.write_column(name);
            self.sql.push_str(" = ");

            self.assignment(assignment, name)?;
        }

        Ok(())
    }

    fn assignment(&mut self, assignment: &'s stmt::Assignment, column: &str) -> Result<()> {
        match assignment {
            stmt::Assignment::Set(expr) => self.value_of(expr)?,
            // `push` and `extend`: both are "append these elements", and both
            // arrive as a list bound to one JSON-text parameter.
            //
            // T-SQL before SQL Server 2025 cannot combine two JSON arrays with a
            // function, so the two canonical texts are spliced: drop the stored
            // array's closing bracket and the incoming array's opening bracket,
            // then join with a comma. Both sides come from the same encoder, so
            // `[]` is exactly how an empty array is spelled, and every array
            // ends in `]`, so `DATALENGTH(...) / 2` is its element count with no
            // trailing-blank ambiguity of the kind `LEN` has.
            stmt::Assignment::Append(expr) => {
                self.sql.push_str("CASE WHEN ");
                self.write_column(column);
                self.sql.push_str(" = N'[]' THEN ");
                self.value_of(expr)?;
                self.sql.push_str(" WHEN ");
                self.value_of(expr)?;
                self.sql.push_str(" = N'[]' THEN ");
                self.write_column(column);
                self.sql.push_str(" ELSE SUBSTRING(");
                self.write_column(column);
                self.sql.push_str(", 1, DATALENGTH(");
                self.write_column(column);
                self.sql.push_str(") / 2 - 1) + N',' + SUBSTRING(");
                self.value_of(expr)?;
                self.sql.push_str(", 2, DATALENGTH(");
                self.value_of(expr)?;
                self.sql.push_str(") / 2 - 1) END");
            }
            stmt::Assignment::Add(expr) => {
                self.write_column(column);
                self.sql.push_str(" + ");
                self.value_of(expr)?;
            }
            stmt::Assignment::Subtract(expr) => {
                self.write_column(column);
                self.sql.push_str(" - ");
                self.value_of(expr)?;
            }
            stmt::Assignment::Batch(_items) => {
                // A batch is rendered as a comma-separated list at the call
                // site; the first item is the left-hand side already emitted.
                return Err(unsupported(
                    "SQL Server driver does not support batched assignments",
                ));
            }
            other => {
                return Err(unsupported(format!(
                    "SQL Server driver does not support the assignment {other:?}"
                )));
            }
        }

        Ok(())
    }

    /// Renders the target of a DML statement's `WHERE` guard.
    fn condition(&mut self, condition: &'s stmt::Condition) -> Result<()> {
        if let Some(expr) = &condition.expr {
            self.sql.push_str(" AND ");
            self.predicate(expr)?;
        }

        Ok(())
    }

    // ---------------------------------------------------------------- queries

    /// Renders the `WITH` clause a query defines its common table expressions in.
    ///
    /// The clause leads the statement in T-SQL, which is why it is rendered
    /// before the body rather than in clause order.
    fn with_clause(&mut self, with: &'s stmt::With) -> Result<()> {
        self.sql.push_str("WITH ");

        for (index, cte) in with.ctes.iter().enumerate() {
            if index > 0 {
                self.sql.push_str(", ");
            }

            write!(self.sql, "{} AS (", cte_name(self.depth, index)).expect("string write");
            self.query(&cte.query)?;
            self.sql.push(')');
        }

        self.sql.push(' ');

        Ok(())
    }

    fn query(&mut self, query: &'s stmt::Query) -> Result<()> {
        self.depth += 1;

        // T-SQL wants the `WITH` clause at the head of the statement, before the
        // body, and the CTEs are named after the depth they are defined at
        // because a reference can only carry a position.
        if let Some(with) = &query.with {
            self.with_clause(with)?;
        }

        // `Lock::Update` is a `FOR UPDATE`, which T-SQL spells as a table hint on
        // the source rather than as a trailing clause, so it has to be known
        // before the body is rendered.
        let row_lock = query
            .locks
            .iter()
            .any(|lock| matches!(lock, stmt::Lock::Update));
        let outer_row_lock = std::mem::replace(&mut self.row_lock, row_lock);

        // The query's source stays in scope for the whole statement: `ORDER BY`
        // and `OFFSET … FETCH NEXT` reference its columns too.
        let source = match &query.body {
            stmt::ExprSet::Select(select) => match &select.source {
                stmt::Source::Table(source) => Some(source),
                stmt::Source::Model(_) => None,
            },
            _ => None,
        };

        if let Some(source) = source {
            self.sources.push(source);
        }

        let result = (|| -> Result<()> {
            self.expr_set(&query.body)?;

            // The hint belongs to this query's own `FROM`, which has just been
            // rendered. Clearing the flag here keeps a nested query's source
            // from inheriting it.
            self.row_lock = false;

            // `OFFSET … FETCH NEXT` requires an `ORDER BY`. When the query has
            // none, a constant ordering keeps the row set deterministic enough
            // for the engine, which never depends on an unspecified order.
            let mut order_by_rendered = false;

            if let Some(order_by) = &query.order_by {
                self.order_by(order_by)?;
                order_by_rendered = true;
            }

            if let Some(limit) = &query.limit {
                self.limit(limit, order_by_rendered)?;
            }

            for lock in &query.locks {
                self.lock(lock)?;
            }

            Ok(())
        })();

        if source.is_some() {
            self.sources.pop();
        }

        self.row_lock = outer_row_lock;
        self.depth -= 1;

        result
    }

    fn lock(&mut self, lock: &'s stmt::Lock) -> Result<()> {
        match lock {
            // Already rendered as a table hint on the source, because T-SQL has
            // no trailing `FOR UPDATE`.
            stmt::Lock::Update => Ok(()),
            stmt::Lock::Share => Err(unsupported(
                "SQL Server has no shared row-lock hint equivalent to `FOR SHARE`",
            )),
        }
    }

    fn expr_set(&mut self, set: &'s stmt::ExprSet) -> Result<()> {
        match set {
            stmt::ExprSet::Select(select) => self.select(select),
            stmt::ExprSet::Values(values) => self.values(values),
            stmt::ExprSet::SetOp(op) => Err(unsupported(format!(
                "SQL Server driver does not render set operations yet: {op:?}"
            ))),
            other => Err(unsupported(format!(
                "SQL Server driver does not render {other:?} as a query body"
            ))),
        }
    }

    fn select(&mut self, select: &'s stmt::Select) -> Result<()> {
        let stmt::Source::Table(source) = &select.source else {
            return Err(unsupported(format!(
                "SQL Server driver does not render the source {:?}",
                select.source
            )));
        };

        let source_is_empty = source.from.is_empty();

        self.sql.push_str("SELECT ");

        if select.distinct {
            self.sql.push_str("DISTINCT ");
        }

        self.returning_list(&select.returning)?;

        if !source_is_empty {
            self.sql.push_str(" FROM ");
            self.source(source)?;
            self.filter(&select.filter)?;
        }

        Ok(())
    }

    fn values(&mut self, values: &'s stmt::Values) -> Result<()> {
        self.sql.push_str("VALUES ");

        // Cloned so the mask is not borrowed from `self` while rendering, which
        // needs `self` mutably.
        let keep_mask = self.insert_columns.clone();

        for (index, row) in values.rows.iter().enumerate() {
            if index > 0 {
                self.sql.push_str(", ");
            }

            match (&keep_mask, row) {
                // An INSERT whose target list dropped generated columns also
                // drops those fields from each row.
                (Some(keep), Expr::Record(record)) => {
                    self.sql.push('(');

                    let mut written = 0;
                    for (field, keep) in record.fields.iter().zip(keep) {
                        if !keep {
                            continue;
                        }

                        if written > 0 {
                            self.sql.push_str(", ");
                        }
                        written += 1;

                        self.value_of(field)?;
                    }

                    self.sql.push(')');
                }
                _ => self.value_of(row)?,
            }
        }

        Ok(())
    }

    fn filter(&mut self, filter: &'s stmt::Filter) -> Result<()> {
        if let Some(expr) = &filter.expr {
            self.sql.push_str(" WHERE ");
            self.predicate(expr)?;
        }

        Ok(())
    }

    /// Renders an expression in a condition position.
    ///
    /// T-SQL has no boolean expression type, so a bare boolean value — an
    /// always-true filter bound as a parameter, for instance — has to be
    /// compared to `1` to be usable as a predicate.
    fn predicate(&mut self, expr: &'s Expr) -> Result<()> {
        if is_predicate(expr) {
            return self.expr(expr);
        }

        self.sql.push('(');
        self.expr(expr)?;
        self.sql.push_str(" = 1)");

        Ok(())
    }

    /// Renders an expression where the statement needs a value.
    ///
    /// T-SQL has no boolean expression type, so a predicate in a value position
    /// — a comparison projected by a conditional read, for example — is
    /// projected as a bit rather than left as a bare comparison.
    fn value_of(&mut self, expr: &'s Expr) -> Result<()> {
        if is_predicate(expr) {
            self.sql.push_str("CASE WHEN ");
            self.expr(expr)?;
            self.sql.push_str(" THEN 1 ELSE 0 END");
            return Ok(());
        }

        self.expr(expr)
    }

    fn order_by(&mut self, order_by: &'s stmt::OrderBy) -> Result<()> {
        self.sql.push_str(" ORDER BY ");

        for (index, item) in order_by.exprs.iter().enumerate() {
            if index > 0 {
                self.sql.push_str(", ");
            }

            self.expr(&item.expr)?;

            match item.order {
                Some(stmt::Direction::Asc) | None => self.sql.push_str(" ASC"),
                Some(stmt::Direction::Desc) => self.sql.push_str(" DESC"),
            }
        }

        Ok(())
    }

    fn limit(&mut self, limit: &'s stmt::Limit, order_by_rendered: bool) -> Result<()> {
        // Both pagination strategies come down to the same clause. The engine
        // lowers a cursor into an ordinary filter before the statement reaches
        // the driver, so a cursor page is an offset page with no offset, and the
        // engine derives the next/previous cursors from the rows it gets back.
        let (offset, fetch) = match limit {
            stmt::Limit::Cursor(cursor) => {
                if cursor.after.is_some() {
                    return Err(unsupported(
                        "SQL Server driver requires the engine to lower a cursor into a filter",
                    ));
                }

                (None, &cursor.page_size)
            }
            stmt::Limit::Offset(offset) => (offset.offset.as_ref(), &offset.limit),
        };

        if !order_by_rendered {
            // `OFFSET`/`FETCH` is only valid after an `ORDER BY`.
            self.sql.push_str(" ORDER BY (SELECT NULL)");
        }

        self.sql.push_str(" OFFSET ");

        match offset {
            Some(offset) => self.expr(offset)?,
            None => self.sql.push('0'),
        }

        self.sql.push_str(" ROWS FETCH NEXT ");
        self.expr(fetch)?;
        self.sql.push_str(" ROWS ONLY");

        Ok(())
    }

    fn source(&mut self, table: &'s stmt::SourceTable) -> Result<()> {
        let level = self.level();

        for (index, with_joins) in table.from.iter().enumerate() {
            if index > 0 {
                self.sql.push_str(", ");
            }

            match &with_joins.relation {
                stmt::TableFactor::Table(id) => {
                    self.table_ref(&table.tables[id.0])?;
                    let alias = self.table_alias(level, id.0);
                    write!(self.sql, " AS {alias}").expect("string write");
                    self.derived_column_list(&table.tables[id.0])?;

                    // A locking read is a table hint in T-SQL, not a trailing
                    // clause, so it is emitted on the query's own `FROM` item.
                    if self.row_lock {
                        self.sql.push_str(" WITH (UPDLOCK, ROWLOCK)");
                        self.row_lock = false;
                    }
                }
            }

            for join in &with_joins.joins {
                let keyword = match &join.constraint {
                    stmt::JoinOp::Inner(_) => " INNER JOIN ",
                    stmt::JoinOp::Left(_) => " LEFT JOIN ",
                };
                self.sql.push_str(keyword);
                self.table_ref(&table.tables[join.table.0])?;
                let alias = self.table_alias(level, join.table.0);
                write!(self.sql, " AS {alias}").expect("string write");
                self.derived_column_list(&table.tables[join.table.0])?;
                self.sql.push_str(" ON ");

                let condition = match &join.constraint {
                    stmt::JoinOp::Inner(expr) | stmt::JoinOp::Left(expr) => expr,
                };
                self.predicate(condition)?;
            }
        }

        Ok(())
    }

    /// Names the columns of a `VALUES`-derived table.
    ///
    /// Postgres and SQLite auto-name those columns, but T-SQL requires them to
    /// be listed, and outer references address them by exactly these names.
    fn derived_column_list(&mut self, table_ref: &'s stmt::TableRef) -> Result<()> {
        let stmt::TableRef::Derived(derived) = table_ref else {
            return Ok(());
        };

        let stmt::ExprSet::Values(values) = &derived.subquery.body else {
            return Ok(());
        };

        let Some(first) = values.rows.first() else {
            return Ok(());
        };

        let fields = match first {
            Expr::Record(record) => record.fields.len(),
            _ => 1,
        };

        self.sql.push('(');
        for column in 0..fields {
            if column > 0 {
                self.sql.push_str(", ");
            }
            let name = column_alias(column);
            self.sql.push_str(&name);
        }
        self.sql.push(')');

        Ok(())
    }

    fn table_ref(&mut self, table_ref: &'s stmt::TableRef) -> Result<()> {
        match table_ref {
            stmt::TableRef::Table(id) => {
                let name = self.table_name(*id);
                write!(self.sql, "{name}").expect("string write");
                Ok(())
            }
            stmt::TableRef::Derived(derived) => {
                self.sql.push('(');
                self.query(&derived.subquery)?;
                self.sql.push(')');
                Ok(())
            }
            // The AST has no names for CTEs, so a reference is a position and
            // the name is derived from it: the depth the CTE is defined at,
            // which is `nesting` levels up from here, and its index.
            stmt::TableRef::Cte { nesting, index } => {
                let defined_at = self.depth.checked_sub(*nesting).ok_or_else(|| {
                    unsupported("a common table expression reference escapes the query scope")
                })?;

                write!(self.sql, "{}", cte_name(defined_at, *index)).expect("string write");
                Ok(())
            }
            // A table-valued argument is a bound array, which the TDS layer
            // cannot carry.
            stmt::TableRef::Arg(_) => Err(unsupported(
                "SQL Server driver does not render an array-valued table reference",
            )),
        }
    }

    /// The nesting level of the query currently being rendered.
    fn level(&self) -> usize {
        self.sources.len().saturating_sub(1)
    }

    /// The alias for the `index`th table at `level`.
    fn table_alias(&self, level: usize, index: usize) -> String {
        format!("tbl_{level}_{index}")
    }

    // ------------------------------------------------------------- returning

    /// Renders the projection list of a `SELECT`, or the projection an
    /// `OUTPUT` clause reuses.
    fn returning_list(&mut self, returning: &'s Returning) -> Result<()> {
        match returning {
            // The planner expresses a projection as a record of columns. A
            // select list and an `OUTPUT` clause both want those fields
            // comma-separated, not wrapped in a row constructor.
            Returning::Project(expr) | Returning::Expr(expr) => match expr {
                Expr::Record(record) => self.projection(&record.fields),
                Expr::List(list) => self.expr_list(&list.items),
                expr => self.value_of(expr),
            },
            // `Model` asks for the whole row. The engine's expected result types
            // line up with the table's columns in order, so expanding the
            // target table's columns is the schema-driven equivalent.
            Returning::Model { .. } | Returning::Changed => {
                let table = self
                    .dml_table
                    .ok_or_else(|| unsupported("returning a whole row outside a DML statement"))?;
                self.all_columns(table)
            }
        }
    }

    /// Renders expressions as a bare comma-separated list, with no wrapping
    /// parentheses.
    fn expr_list(&mut self, exprs: &'s [Expr]) -> Result<()> {
        for (index, expr) in exprs.iter().enumerate() {
            if index > 0 {
                self.sql.push_str(", ");
            }
            self.value_of(expr)?;
        }

        Ok(())
    }

    /// Renders a `SELECT` projection, aliasing every position.
    ///
    /// A subquery's output is read by its positional alias (`column1`,
    /// `column2`, …), and an `OUTPUT` clause accepts no aliases, so they are
    /// emitted only for a select list.
    fn projection(&mut self, fields: &'s [Expr]) -> Result<()> {
        for (index, field) in fields.iter().enumerate() {
            if index > 0 {
                self.sql.push_str(", ");
            }

            self.value_of(field)?;

            if self.output.is_none() {
                let name = column_alias(index);
                write!(self.sql, " AS [{name}]").expect("string write");
            }
        }

        Ok(())
    }

    /// Lists every column of `table` with the current reference treatment.
    fn all_columns(&mut self, table: db::TableId) -> Result<()> {
        let count = self.schema.table(table).columns.len();

        for column in 0..count {
            if column > 0 {
                self.sql.push_str(", ");
            }

            match self.output {
                Some(prefix) => {
                    let name = self.column_def_name(db::ColumnId {
                        table,
                        index: column,
                    });
                    write!(self.sql, "{prefix}.[{name}]").expect("string write");
                }
                // Inside a `SELECT`, the columns are addressed through the
                // table alias the source assigned them.
                None => {
                    let alias = self.table_alias(self.level(), 0);
                    let name = self.column_def_name(db::ColumnId {
                        table,
                        index: column,
                    });
                    write!(self.sql, "{alias}.[{name}]").expect("string write");
                }
            }
        }

        Ok(())
    }

    // ----------------------------------------------------------- expressions

    fn expr(&mut self, expr: &'s Expr) -> Result<()> {
        match expr {
            Expr::Value(value) | Expr::Static(value) => self.value(value),
            Expr::Arg(arg) => {
                // `position` indexes the operation's parameter list, and the
                // RPC parameters are named to match.
                write!(self.sql, "@p{}", arg.position + 1).expect("string write");
                Ok(())
            }
            Expr::Reference(reference) => match reference {
                stmt::ExprReference::Column(column) => self.reference(column),
                other => Err(unsupported(format!(
                    "SQL Server driver does not render the reference {other:?}"
                ))),
            },
            Expr::BinaryOp(op) => {
                self.value_of(&op.lhs)?;
                self.sql.push_str(binary_op(op.op));
                self.value_of(&op.rhs)
            }
            Expr::And(and) => self.delimited(&and.operands, " AND "),
            Expr::Or(or) => self.delimited(&or.operands, " OR "),
            Expr::Not(not) => {
                self.sql.push_str("NOT (");
                self.predicate(&not.expr)?;
                self.sql.push(')');
                Ok(())
            }
            Expr::IsNull(is_null) => {
                self.expr(&is_null.expr)?;
                self.sql.push_str(if is_null.negated {
                    " IS NOT NULL"
                } else {
                    " IS NULL"
                });
                Ok(())
            }
            Expr::Between(between) => {
                self.expr(&between.expr)?;
                self.sql.push_str(" BETWEEN ");
                self.expr(&between.low)?;
                self.sql.push_str(" AND ");
                self.expr(&between.high)
            }
            Expr::Like(like) => {
                self.expr(&like.expr)?;
                self.sql.push_str(" LIKE ");
                self.expr(&like.pattern)?;

                if let Some(escape) = like.escape {
                    write!(self.sql, " ESCAPE '{}'", escape).expect("string write");
                }

                // `case_insensitive` is left to the database: T-SQL `LIKE`
                // follows the column's collation, which is the behaviour Toasty
                // documents for this backend.
                Ok(())
            }
            Expr::StartsWith(starts_with) => {
                // The planner rewrites the prefix into a finished `LIKE` pattern
                // (`%`, `_` and `!` escaped, with a trailing `%` appended)
                // because `Capability::binary_like_starts_with` is set, so all
                // that is left is the matching `ESCAPE` character.
                //
                // `starts_with` is case-sensitive, but the default collation is
                // not, so the comparison is forced to a binary collation. The
                // operand is parenthesised because `COLLATE` binds to whatever
                // expression precedes it.
                self.sql.push_str("((");
                self.expr(&starts_with.expr)?;
                write!(self.sql, ") COLLATE {CASE_SENSITIVE_COLLATION} LIKE ")
                    .expect("string write");
                self.expr(&starts_with.prefix)?;
                self.sql.push_str(" ESCAPE '!')");
                Ok(())
            }
            Expr::InList(in_list) => {
                // A composite key compares a row value, which T-SQL does not
                // have, so `(a, b) IN ((x, y), (z, w))` has to be expanded.
                if let Some(fields) = row_fields(&in_list.expr) {
                    return self.row_in_list(fields, &in_list.list);
                }

                self.expr(&in_list.expr)?;
                self.sql.push_str(" IN ");

                match &*in_list.list {
                    Expr::List(list) => {
                        self.sql.push('(');
                        for (index, item) in list.items.iter().enumerate() {
                            if index > 0 {
                                self.sql.push_str(", ");
                            }
                            self.expr(item)?;
                        }
                        self.sql.push(')');
                        Ok(())
                    }
                    Expr::Value(stmt::Value::List(items)) => {
                        self.sql.push('(');
                        for (index, item) in items.iter().enumerate() {
                            if index > 0 {
                                self.sql.push_str(", ");
                            }
                            self.value(item)?;
                        }
                        self.sql.push(')');
                        Ok(())
                    }
                    // A list bound as a single parameter needs an array bind,
                    // which TDS does not offer.
                    _ => Err(unsupported(
                        "SQL Server driver requires list membership to be expanded into individual parameters",
                    )),
                }
            }
            Expr::InSubquery(in_subquery) => {
                // As with `Expr::InList`, a row value on the left has no direct
                // T-SQL spelling.
                if let Some(fields) = row_fields(&in_subquery.expr) {
                    return self.row_in_subquery(fields, &in_subquery.query);
                }

                self.expr(&in_subquery.expr)?;
                self.sql.push_str(if in_subquery.negated {
                    " NOT IN ("
                } else {
                    " IN ("
                });
                self.query(&in_subquery.query)?;
                self.sql.push(')');
                Ok(())
            }
            Expr::Exists(exists) => {
                self.sql.push_str("EXISTS (");
                self.query(&exists.subquery)?;
                self.sql.push(')');
                Ok(())
            }
            Expr::AnyOp(op) => self.any_op(op),
            Expr::AllOp(op) => self.quantified(&op.lhs, op.op, &op.rhs, "ALL"),
            Expr::Cast(cast) => {
                self.sql.push_str("CAST(");
                self.expr(&cast.expr)?;
                self.sql.push_str(" AS ");
                let ty = crate::type_map::column_type_for(&cast.ty)?;
                self.sql.push_str(&ty);
                self.sql.push(')');
                Ok(())
            }
            Expr::Func(func) => self.func(func),
            // The element count of a `Vec<scalar>` column. `OPENJSON` is the
            // only way to enumerate a JSON array in T-SQL before SQL Server
            // 2025, and it is also what the membership predicates use, so one
            // idiom covers the whole subsystem.
            Expr::Length(length) => {
                self.sql.push_str("(SELECT COUNT(*) FROM OPENJSON(");
                self.expr(&length.expr)?;
                self.sql.push_str("))");
                Ok(())
            }
            Expr::Record(record) => {
                self.sql.push('(');
                for (index, field) in record.fields.iter().enumerate() {
                    if index > 0 {
                        self.sql.push_str(", ");
                    }
                    self.value_of(field)?;
                }
                self.sql.push(')');
                Ok(())
            }
            // A projection of the row an upsert proposes. `MERGE` exposes that
            // row as its source relation, which is where the value being
            // considered for insertion can be read from the update branch.
            Expr::Project(project) => {
                let Expr::Incoming(incoming) = project.base.as_ref() else {
                    return Err(unsupported(format!(
                        "SQL Server driver does not render the projection {project:?}"
                    )));
                };

                let stmt::ExprIncoming::Table(table) = incoming else {
                    return Err(unsupported(
                        "SQL Server driver requires the incoming row of an upsert to be lowered to a table",
                    ));
                };

                let [column] = project.projection.as_slice() else {
                    return Err(unsupported(
                        "SQL Server driver only projects a single column of an upsert's incoming row",
                    ));
                };

                let name = self.column_def_name_checked(db::ColumnId {
                    table: *table,
                    index: *column,
                })?;

                write!(self.sql, "{MERGE_SOURCE}.[{name}]").expect("string write");
                Ok(())
            }
            Expr::List(list) => {
                self.sql.push('(');
                for (index, item) in list.items.iter().enumerate() {
                    if index > 0 {
                        self.sql.push_str(", ");
                    }
                    self.value_of(item)?;
                }
                self.sql.push(')');
                Ok(())
            }
            Expr::Ident(name) => {
                write!(self.sql, "{name}").expect("string write");
                Ok(())
            }
            // A column the statement leaves to its default. T-SQL accepts the
            // `DEFAULT` keyword in a `VALUES` list, and the planner omits
            // generated columns from the target list entirely.
            Expr::Default => {
                self.sql.push_str("DEFAULT");
                Ok(())
            }
            Expr::Stmt(stmt) => {
                self.sql.push('(');
                self.statement_inner(&stmt.stmt)?;
                self.sql.push(')');
                Ok(())
            }
            other => Err(unsupported(format!(
                "SQL Server driver does not render the expression {other:?}"
            ))),
        }
    }

    /// Renders `<lhs> <op> {ANY|ALL} (rhs)`.
    fn quantified(
        &mut self,
        lhs: &'s Expr,
        op: BinaryOp,
        rhs: &'s Expr,
        quantifier: &str,
    ) -> Result<()> {
        self.expr(lhs)?;
        self.sql.push_str(binary_op(op));
        write!(self.sql, "{quantifier} (").expect("string write");
        self.expr(rhs)?;
        self.sql.push(')');
        Ok(())
    }

    /// Renders `value = ANY(<collection>)` as membership in a JSON array.
    ///
    /// T-SQL has no array type, and its `ANY` quantifier only accepts a
    /// subquery, so the collection is enumerated with `OPENJSON` instead. The
    /// extracted `[value]` is the element's JSON text, which for a string is the
    /// unquoted contents, so this compares the same way the bound operand does.
    ///
    /// The elements are forced to a binary collation because the suite requires
    /// membership to be case-sensitive while a server-default collation is not.
    ///
    /// The engine only emits this node for `Vec<scalar>` membership —
    /// `Path::contains`, and the per-element expansion of `Intersects` and
    /// `IsSuperset` for a driver without native set predicates — so the operator
    /// is always equality.
    fn any_op(&mut self, op: &'s stmt::ExprAnyOp) -> Result<()> {
        if op.op != BinaryOp::Eq {
            return Err(unsupported(format!(
                "SQL Server driver does not render the quantified comparison {op:?} over a collection"
            )));
        }

        self.expr(&op.lhs)?;
        self.sql.push_str(" IN (SELECT [value] COLLATE ");
        self.sql.push_str(CASE_SENSITIVE_COLLATION);
        self.sql.push_str(" FROM OPENJSON(");
        self.expr(&op.rhs)?;
        self.sql.push_str("))");
        Ok(())
    }

    fn delimited(&mut self, operands: &'s [Expr], separator: &str) -> Result<()> {
        if operands.is_empty() {
            // An empty conjunction is true, an empty disjunction false; T-SQL
            // needs a literal for either.
            self.sql.push_str(if separator.contains("AND") {
                "1=1"
            } else {
                "1=0"
            });
            return Ok(());
        }

        for (index, operand) in operands.iter().enumerate() {
            if index > 0 {
                self.sql.push_str(separator);
            }
            self.sql.push('(');
            self.predicate(operand)?;
            self.sql.push(')');
        }

        Ok(())
    }

    /// Renders `(a, b) IN ((x, y), (z, w))`.
    ///
    /// T-SQL has no row value constructor, so a composite `IN` list becomes the
    /// equivalent `OR` of `AND`s. `NULL` handling carries over unchanged: an
    /// unknown comparison stays unknown through `AND` and `OR`.
    fn row_in_list(&mut self, fields: &'s [Expr], list: &'s Expr) -> Result<()> {
        let Expr::List(rows) = list else {
            return Err(unsupported(format!(
                "SQL Server driver requires a composite key IN list of tuples, found {list:?}"
            )));
        };

        if rows.items.is_empty() {
            // An empty list matches nothing, but T-SQL still needs a boolean
            // expression wherever the planner put this one.
            self.sql.push_str("(1 = 0)");
            return Ok(());
        }

        self.sql.push('(');
        for (index, row) in rows.items.iter().enumerate() {
            if index > 0 {
                self.sql.push_str(" OR ");
            }

            let Some(row) = row_fields(row) else {
                return Err(unsupported(format!(
                    "SQL Server driver requires each element of a composite key IN list to be a tuple, found {row:?}"
                )));
            };
            if row.len() != fields.len() {
                return Err(unsupported(
                    "SQL Server driver requires composite key tuples to have matching arity",
                ));
            }

            self.sql.push('(');
            for (index, (lhs, rhs)) in fields.iter().zip(row).enumerate() {
                if index > 0 {
                    self.sql.push_str(" AND ");
                }
                self.value_of(lhs)?;
                self.sql.push_str(" = ");
                self.value_of(rhs)?;
            }
            self.sql.push(')');
        }
        self.sql.push(')');

        Ok(())
    }

    /// Renders `(a, b) IN (SELECT x, y FROM …)`.
    ///
    /// A row value cannot be compared against a subquery either, so the
    /// subquery is wrapped in a derived table. The explicit column list gives
    /// it the positional `column1`, `column2` names this renderer already uses
    /// for derived tables, which the correlation then compares against.
    fn row_in_subquery(&mut self, fields: &'s [Expr], query: &'s stmt::Query) -> Result<()> {
        // `depth` is bumped by every nested query, so sibling subqueries cannot
        // collide and neither can two nested composite `IN`s.
        let alias = quote_ident(&format!("in_{}", self.depth));

        self.sql.push_str("EXISTS (SELECT 1 FROM (");
        self.query(query)?;
        write!(self.sql, ") AS {alias} (").expect("string write");
        for index in 0..fields.len() {
            if index > 0 {
                self.sql.push_str(", ");
            }
            write!(self.sql, "[{}]", column_alias(index)).expect("string write");
        }
        self.sql.push_str(") WHERE ");

        for (index, field) in fields.iter().enumerate() {
            if index > 0 {
                self.sql.push_str(" AND ");
            }
            write!(self.sql, "{alias}.[{}] = ", column_alias(index)).expect("string write");
            self.value_of(field)?;
        }

        self.sql.push(')');

        Ok(())
    }

    fn func(&mut self, func: &'s stmt::ExprFunc) -> Result<()> {
        match func {
            stmt::ExprFunc::Count(count) => {
                self.sql.push_str("COUNT(");
                match &count.arg {
                    Some(arg) => self.expr(arg)?,
                    None => self.sql.push('*'),
                }
                self.sql.push(')');

                if let Some(filter) = &count.filter {
                    self.sql.push_str(" FILTER (WHERE ");
                    self.expr(filter)?;
                    self.sql.push(')');
                }

                Ok(())
            }
            // The engine asks for the key generated by the statement it just
            // ran; SQL Server reports it as `SCOPE_IDENTITY()`.
            stmt::ExprFunc::LastInsertId(_) => {
                self.sql.push_str("COALESCE(SCOPE_IDENTITY(), @@IDENTITY)");
                Ok(())
            }
            // Reading a field inside a `#[document]` embed.
            //
            // `JSON_VALUE` returns a scalar as `nvarchar(4000)` whatever it
            // actually holds, so the leaf's own type is cast back on: a number
            // has to compare as a number and a timestamp as a timestamp, not as
            // the text that happens to represent them. `JSON_QUERY` is the
            // counterpart for a leaf that is itself an object or array, which
            // `JSON_VALUE` would answer with `NULL`.
            //
            // No collation is forced, deliberately. The suite requires a
            // document string leaf to match with *the same* case sensitivity as a
            // plain column on the same backend, and `JSON_VALUE` inherits the
            // input's collation, so it already agrees with our column
            // comparisons — which are the server default. Forcing
            // `Latin1_General_BIN2` here, as the collection and `starts_with`
            // paths do, would make the leaf disagree with its own column.
            stmt::ExprFunc::JsonExtract(extract) => {
                // A scalar function this driver provides, carried in the only
                // node with both an operand and free text. See `crate::funcs`:
                // the `!` prefix is what separates it from a real document
                // path extraction, which is the arm below.
                if let Some(call) = crate::funcs::decode(&extract.path) {
                    // A method hangs off the operand — `col.STArea()` — because
                    // that is the only form T-SQL offers for it. A function
                    // instead takes the operand as an argument, at the position
                    // the `{base}` token marks.
                    if call.method {
                        self.expr(&extract.base)?;
                        write!(self.sql, ".{}(", call.name).expect("string write");
                    } else {
                        write!(self.sql, "{}(", call.name).expect("string write");
                    }

                    for (index, arg) in call.args.iter().enumerate() {
                        if index > 0 {
                            self.sql.push_str(", ");
                        }

                        if crate::funcs::is_operand(arg) {
                            self.expr(&extract.base)?;
                        } else {
                            self.sql.push_str(arg);
                        }
                    }

                    self.sql.push(')');

                    return Ok(());
                }

                let object = matches!(extract.ty, stmt::Type::Object);
                self.sql.push_str(if object {
                    "JSON_QUERY("
                } else {
                    "CAST(JSON_VALUE("
                });
                self.expr(&extract.base)?;
                self.sql.push_str(", '$");
                for key in &extract.path {
                    // Keys come from Rust field names, so they need no quoting.
                    // A name needing `$."a b"` would have to be spelled by
                    // whoever introduces it.
                    self.sql.push('.');
                    self.sql.push_str(key);
                }
                self.sql.push_str("')");

                if object {
                    // `JSON_QUERY` is already closed by the path's own quote.
                    return Ok(());
                }

                self.sql.push_str(" AS ");
                self.sql
                    .push_str(&crate::type_map::column_type_for(&extract.ty)?);
                self.sql.push(')');
                Ok(())
            }
        }
    }

    fn reference(&mut self, column: &'s stmt::ExprColumn) -> Result<()> {
        // Inside a DML statement every reference at the statement's own scope
        // names a column of the single table being written, so no alias
        // qualification is needed. A nested query is a different matter: its
        // references belong to its source, and resolving them against the DML
        // table would silently pick the wrong column whenever the two tables
        // share a column position. `depth` is bumped by every nested query, so
        // it distinguishes the two.
        if self.dml_table.is_some() && self.depth == 0 {
            let name = self.dml_column_name(column.column)?;

            return match self.output {
                // `OUTPUT` names one side of the write rather than the merged
                // table, so the alias is not used there.
                Some(prefix) => {
                    write!(self.sql, "{prefix}.[{name}]").expect("string write");
                    Ok(())
                }
                None => {
                    self.write_column(name);
                    Ok(())
                }
            };
        }

        let level = self
            .level()
            .checked_sub(column.nesting)
            .ok_or_else(|| unsupported("column reference escapes the query scope"))?;

        let alias = self.table_alias(level, column.table);
        let name = self.source_column_name(level, column.table, column.column)?;
        write!(self.sql, "{alias}.[{name}]").expect("string write");

        Ok(())
    }

    /// Resolves a column of the DML target table by index.
    fn dml_column_name(&self, column: usize) -> Result<&'a str> {
        let table = self
            .dml_table
            .ok_or_else(|| unsupported("column reference outside a table"))?;
        self.column_def_name_checked(db::ColumnId {
            table,
            index: column,
        })
    }

    /// Resolves a column of the query source at `level`.
    fn source_column_name(&self, level: usize, table: usize, column: usize) -> Result<String> {
        let source = self
            .sources
            .get(level)
            .ok_or_else(|| unsupported("column reference outside a table source"))?;

        let table_ref = source
            .tables
            .get(table)
            .ok_or_else(|| unsupported("column reference to an unknown table"))?;

        // A real table's columns are named by the schema. A derived table or a
        // CTE exposes them under the positional alias its select list carries.
        match table_ref {
            stmt::TableRef::Table(table_id) => {
                let table = self.schema.table(*table_id);
                let name = table
                    .columns
                    .get(column)
                    .ok_or_else(|| Error::invalid_statement("column index out of bounds"))?;
                Ok(name.name.clone())
            }
            _ => Ok(column_alias(column)),
        }
    }

    fn column_def_name(&self, id: db::ColumnId) -> &'a str {
        self.column_def_name_checked(id)
            .expect("column id from the schema must resolve")
    }

    fn column_def_name_checked(&self, id: db::ColumnId) -> Result<&'a str> {
        let table = self.schema.table(id.table);
        let column = table
            .columns
            .get(id.index)
            .ok_or_else(|| Error::invalid_statement("column index out of bounds"))?;
        Ok(&column.name)
    }

    fn table_name(&self, id: db::TableId) -> String {
        quote_ident(&self.schema.table(id).name)
    }

    /// Renders a value as a T-SQL literal.
    fn value(&mut self, value: &'s stmt::Value) -> Result<()> {
        match value {
            stmt::Value::Null => self.sql.push_str("NULL"),
            stmt::Value::Bool(value) => {
                self.sql.push_str(if *value { "1" } else { "0" });
            }
            stmt::Value::I8(value) => write!(self.sql, "{value}").expect("string write"),
            stmt::Value::I16(value) => write!(self.sql, "{value}").expect("string write"),
            stmt::Value::I32(value) => write!(self.sql, "{value}").expect("string write"),
            stmt::Value::I64(value) => write!(self.sql, "{value}").expect("string write"),
            stmt::Value::U8(value) => write!(self.sql, "{value}").expect("string write"),
            stmt::Value::U16(value) => write!(self.sql, "{value}").expect("string write"),
            stmt::Value::U32(value) => write!(self.sql, "{value}").expect("string write"),
            stmt::Value::U64(value) => write!(self.sql, "{value}").expect("string write"),
            stmt::Value::F32(value) => write!(self.sql, "{value}").expect("string write"),
            stmt::Value::F64(value) => write!(self.sql, "{value}").expect("string write"),
            stmt::Value::String(value) => {
                write!(self.sql, "N'{}'", value.replace('\'', "''")).expect("string write");
            }
            stmt::Value::Uuid(value) => {
                write!(self.sql, "'{value}'").expect("string write");
            }
            stmt::Value::Bytes(value) => {
                self.sql.push_str("0x");
                for byte in value {
                    write!(self.sql, "{byte:02X}").expect("string write");
                }
            }
            other => {
                return Err(unsupported(format!(
                    "SQL Server driver does not render the literal {other:?}"
                )));
            }
        }

        Ok(())
    }

    /// Renders a nested statement without the outer entry-point bookkeeping.
    fn statement_inner(&mut self, stmt: &'s Statement) -> Result<()> {
        match stmt {
            Statement::Query(query) => self.query(query),
            Statement::Insert(insert) => self.insert(insert),
            Statement::Update(update) => self.update(update),
            Statement::Delete(delete) => self.delete(delete),
        }
    }
}

/// The assignment an upsert makes to one column, looking through `groups` in
/// priority order: the last group that names the column wins.
///
/// The groups are the branch-specific ones first and the shared ones last, so
/// an explicit `on_create`/`on_update` assignment overrides a shared one, which
/// overrides a declared default.
fn upsert_assignment<'a>(
    column: usize,
    groups: &[&'a stmt::Assignments],
) -> Option<&'a stmt::Assignment> {
    let key = stmt::Projection::single(column);

    groups.iter().rev().find_map(|group| group.get(&key))
}

/// The fields of a row value such as the `(a, b)` in `(a, b) IN …`.
///
/// The planner spells a composite key as a record; a list is accepted too
/// because both render as a parenthesised tuple.
fn row_fields(expr: &Expr) -> Option<&[Expr]> {
    match expr {
        Expr::Record(record) => Some(&record.fields),
        Expr::List(list) => Some(&list.items),
        _ => None,
    }
}

/// Whether an expression is usable as a predicate without a comparison.
///
/// Anything else in a condition position is a boolean-valued operand, which
/// T-SQL requires to appear on the left of a comparison.
fn is_predicate(expr: &Expr) -> bool {
    match expr {
        Expr::BinaryOp(op) => is_comparison(op.op),
        Expr::And(_)
        | Expr::Or(_)
        | Expr::Not(_)
        | Expr::IsNull(_)
        | Expr::Between(_)
        | Expr::Like(_)
        | Expr::StartsWith(_)
        | Expr::InList(_)
        | Expr::InSubquery(_)
        | Expr::Exists(_)
        | Expr::AnyOp(_)
        | Expr::AllOp(_)
        | Expr::IsVariant(_)
        | Expr::Intersects(_)
        | Expr::IsSuperset(_)
        | Expr::Match(_) => true,
        _ => false,
    }
}

/// Whether a binary operator compares rather than computes.
fn is_comparison(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Ge | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Lt
    )
}

/// The positional alias a projection column carries.
///
/// T-SQL derives no column names of its own, so this renderer emits these names
/// in every select list and addresses derived-table columns by them. The
/// numbering matches what the engine expects (1-based, as in PostgreSQL).
fn column_alias(index: usize) -> String {
    format!("column{}", index + 1)
}

/// The name of the common table expression `index` levels inside the query
/// `depth` levels deep.
///
/// A CTE has no name in the AST — it is referenced by where it is defined and
/// its position — so the rendered name has to be a function of exactly those,
/// here as an identity: a reference carries the same two numbers.
fn cte_name(depth: usize, index: usize) -> String {
    quote_ident(&format!("cte_{depth}_{index}"))
}

/// Quotes an identifier, escaping `]` by doubling it.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// The T-SQL operator for a binary operation.
fn binary_op(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Eq => " = ",
        BinaryOp::Ne => " <> ",
        BinaryOp::Ge => " >= ",
        BinaryOp::Gt => " > ",
        BinaryOp::Le => " <= ",
        BinaryOp::Lt => " < ",
        BinaryOp::Add => " + ",
        BinaryOp::Sub => " - ",
    }
}

fn unsupported(message: impl Into<String>) -> Error {
    Error::unsupported_feature(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use toasty_core::schema::db;

    /// `items(id BIGINT PRIMARY KEY, version INT)`. `version` is the column the
    /// read-modify-write probe projects to check an OCC condition.
    fn schema() -> db::Schema {
        let id = db::ColumnId {
            table: db::TableId(0),
            index: 0,
        };

        db::Schema {
            tables: vec![db::Table {
                id: db::TableId(0),
                name: "items".to_owned(),
                columns: vec![
                    db::Column {
                        id,
                        name: "id".to_owned(),
                        ty: stmt::Type::I64,
                        storage_ty: db::Type::Integer(8),
                        nullable: false,
                        primary_key: true,
                        auto_increment: false,
                        versionable: false,
                    },
                    db::Column {
                        id: db::ColumnId {
                            table: db::TableId(0),
                            index: 1,
                        },
                        name: "version".to_owned(),
                        ty: stmt::Type::I32,
                        storage_ty: db::Type::Integer(4),
                        nullable: false,
                        primary_key: false,
                        auto_increment: false,
                        versionable: true,
                    },
                ],
                primary_key: db::PrimaryKey {
                    columns: vec![id],
                    index: db::IndexId {
                        table: db::TableId(0),
                        index: 0,
                    },
                },
                indices: Vec::new(),
            }],
        }
    }

    /// `SELECT [version] FROM [items] WHERE 1=1`, with the given row locks.
    fn probe(locks: Vec<stmt::Lock>) -> stmt::Statement {
        let source = stmt::SourceTable {
            tables: vec![stmt::TableRef::Table(db::TableId(0))],
            from: vec![stmt::TableWithJoins {
                relation: stmt::TableFactor::Table(stmt::SourceTableId(0)),
                joins: Vec::new(),
            }],
        };

        stmt::Statement::Query(stmt::Query {
            with: None,
            body: stmt::ExprSet::Select(Box::new(stmt::Select {
                returning: stmt::Returning::Project(stmt::Expr::column(stmt::ExprColumn {
                    nesting: 0,
                    table: 0,
                    column: 1,
                })),
                source: stmt::Source::Table(source),
                filter: stmt::Filter::new(true),
                distinct: false,
            })),
            single: false,
            order_by: None,
            limit: None,
            locks,
        })
    }

    /// `Lock::Update` is a `FOR UPDATE`, and the only T-SQL equivalent is a
    /// table hint, so it has to reach the `FROM` clause. A probe that silently
    /// does not lock lets a concurrent write land between the probe and the
    /// write — the lost update that a `#[version]` field exists to prevent.
    #[test]
    fn renders_a_row_lock_as_a_table_hint() {
        let sql = render(&schema(), &probe(vec![stmt::Lock::Update])).unwrap();

        assert!(
            sql.contains("FROM [items] AS tbl_0_0 WITH (UPDLOCK, ROWLOCK)"),
            "got: {sql}"
        );
    }

    #[test]
    fn omits_the_hint_when_no_lock_was_asked_for() {
        let sql = render(&schema(), &probe(Vec::new())).unwrap();

        assert!(!sql.contains("UPDLOCK"), "got: {sql}");
    }

    /// A scalar function this driver provides travels inside the node Toasty
    /// uses for document paths — see `crate::funcs`. The renderer has to tell
    /// the two apart, and to put the operand where the function wants it:
    /// `ISJSON` takes its subject first, `DATEPART` takes it last. The `{base}`
    /// token is what carries that position.
    #[test]
    fn renders_a_carried_function_call() {
        let call = |name: &str, args: &[&str]| {
            stmt::Expr::Func(stmt::ExprFunc::JsonExtract(stmt::FuncJsonExtract {
                base: Box::new(tags_column()),
                path: std::iter::once(format!("!{name}"))
                    .chain(args.iter().map(|arg| (*arg).to_owned()))
                    .collect(),
                ty: stmt::Type::Bool,
            }))
        };

        let sql = render(
            &schema(),
            &collection_filter(stmt::Expr::eq(call("ISJSON", &["{base}", "ARRAY"]), true)),
        )
        .unwrap();

        assert!(
            sql.contains("ISJSON(tbl_0_0.[version], ARRAY) = 1"),
            "got: {sql}"
        );

        let sql = render(
            &schema(),
            &collection_filter(stmt::Expr::eq(call("DATEPART", &["day", "{base}"]), 1)),
        )
        .unwrap();

        assert!(
            sql.contains("DATEPART(day, tbl_0_0.[version])"),
            "got: {sql}"
        );

        // The marker is the only thing separating a call from a document path,
        // so a path without it must still be an extraction.
        let document = stmt::Expr::Func(stmt::ExprFunc::JsonExtract(stmt::FuncJsonExtract {
            base: Box::new(tags_column()),
            path: vec!["a".to_owned()],
            ty: stmt::Type::I32,
        }));
        let sql = render(&schema(), &collection_filter(stmt::Expr::eq(document, 1))).unwrap();

        assert!(
            sql.contains("JSON_VALUE(tbl_0_0.[version], '$.a')"),
            "got: {sql}"
        );

        // A method takes the operand as its receiver, so it carries no `{base}`
        // and hangs off the column instead. This is the only form T-SQL offers
        // for the spatial vocabulary.
        let sql = render(
            &schema(),
            &collection_filter(stmt::Expr::eq(call(".STNumPoints", &[]), 1)),
        )
        .unwrap();

        assert!(
            sql.contains("tbl_0_0.[version].STNumPoints() = 1"),
            "got: {sql}"
        );
    }

    /// The hint must go on the query's own `FROM`, not on a table reached from
    /// inside it, or a locking probe would lock unrelated rows.
    #[test]
    fn keeps_the_hint_out_of_a_nested_subquery() {
        let schema = schema();
        let mut stmt = probe(vec![stmt::Lock::Update]);

        let stmt::Statement::Query(query) = &mut stmt else {
            unreachable!("the fixture is a query");
        };
        let stmt::ExprSet::Select(select) = &mut query.body else {
            unreachable!("the fixture selects");
        };

        let inner = stmt::SourceTable {
            tables: vec![stmt::TableRef::Table(db::TableId(0))],
            from: vec![stmt::TableWithJoins {
                relation: stmt::TableFactor::Table(stmt::SourceTableId(0)),
                joins: Vec::new(),
            }],
        };
        select.filter = stmt::Filter::new(stmt::Expr::exists(stmt::Query {
            with: None,
            body: stmt::ExprSet::Select(Box::new(stmt::Select {
                returning: stmt::Returning::Project(stmt::Expr::Value(stmt::Value::from(1_i64))),
                source: stmt::Source::Table(inner),
                filter: stmt::Filter::new(true),
                distinct: false,
            })),
            single: false,
            order_by: None,
            limit: None,
            locks: Vec::new(),
        }));

        let sql = render(&schema, &stmt).unwrap();

        // The aliases are positional, so `tbl_0_0` is the probe's own source
        // and `tbl_1_0` is the one inside the subquery.
        assert!(
            sql.contains("FROM [items] AS tbl_0_0 WITH (UPDLOCK, ROWLOCK)"),
            "the probe's own FROM must carry the hint, got: {sql}"
        );
        assert!(
            !sql.contains("tbl_1_0 WITH (UPDLOCK"),
            "a nested source must not inherit the hint, got: {sql}"
        );
    }

    #[test]
    fn rejects_a_shared_lock_it_cannot_honour() {
        let error = render(&schema(), &probe(vec![stmt::Lock::Share])).unwrap_err();

        assert!(error.to_string().contains("FOR SHARE"), "got: {error}");
    }

    /// `items(id BIGINT PRIMARY KEY, tags)` where `tags` is a `Vec<String>`
    /// column, stored as JSON text.
    fn collection_schema() -> db::Schema {
        let id = db::ColumnId {
            table: db::TableId(0),
            index: 0,
        };

        db::Schema {
            tables: vec![db::Table {
                id: db::TableId(0),
                name: "items".to_owned(),
                columns: vec![
                    db::Column {
                        id,
                        name: "id".to_owned(),
                        ty: stmt::Type::I64,
                        storage_ty: db::Type::Integer(8),
                        nullable: false,
                        primary_key: true,
                        auto_increment: false,
                        versionable: false,
                    },
                    db::Column {
                        id: db::ColumnId {
                            table: db::TableId(0),
                            index: 1,
                        },
                        name: "tags".to_owned(),
                        ty: stmt::Type::List(Box::new(stmt::Type::String)),
                        storage_ty: db::Type::List(Box::new(db::Type::VarChar(4000))),
                        nullable: false,
                        primary_key: false,
                        auto_increment: false,
                        versionable: false,
                    },
                ],
                primary_key: db::PrimaryKey {
                    columns: vec![id],
                    index: db::IndexId {
                        table: db::TableId(0),
                        index: 0,
                    },
                },
                indices: Vec::new(),
            }],
        }
    }

    /// The `tags` column as an expression, for building fixtures by hand.
    fn tags_column() -> stmt::Expr {
        stmt::Expr::column(stmt::ExprColumn {
            nesting: 0,
            table: 0,
            column: 1,
        })
    }

    /// `SELECT 1 FROM [items] WHERE <filter>`.
    fn collection_filter(filter: stmt::Expr) -> stmt::Statement {
        let source = stmt::SourceTable {
            tables: vec![stmt::TableRef::Table(db::TableId(0))],
            from: vec![stmt::TableWithJoins {
                relation: stmt::TableFactor::Table(stmt::SourceTableId(0)),
                joins: Vec::new(),
            }],
        };

        stmt::Statement::Query(stmt::Query {
            with: None,
            body: stmt::ExprSet::Select(Box::new(stmt::Select {
                returning: stmt::Returning::Project(stmt::Expr::Value(stmt::Value::from(1_i64))),
                source: stmt::Source::Table(source),
                filter: stmt::Filter::new(filter),
                distinct: false,
            })),
            single: false,
            order_by: None,
            limit: None,
            locks: Vec::new(),
        })
    }

    /// Membership in a JSON collection has to be an `OPENJSON` enumeration —
    /// T-SQL's `ANY` only accepts a subquery — and the enumerated element text
    /// has to be forced to a binary collation, because the server default is
    /// case-insensitive while Toasty's string predicates are not.
    ///
    /// The operand is an `Expr::Arg` because that is the shape the engine's
    /// extract pass leaves: a whole collection values list binds as one
    /// parameter, so the renderer never sees the list itself.
    #[test]
    fn renders_collection_membership_through_openjson() {
        let stmt = collection_filter(stmt::Expr::any_op(
            stmt::Expr::arg(0),
            stmt::BinaryOp::Eq,
            tags_column(),
        ));

        let sql = render(&collection_schema(), &stmt).unwrap();

        assert!(
            sql.contains(&format!(
                "@p1 IN (SELECT [value] COLLATE {CASE_SENSITIVE_COLLATION} FROM OPENJSON(tbl_0_0.[tags]))"
            )),
            "got: {sql}"
        );
    }

    /// The element count is the same enumeration counted, and the same arm
    /// serves `is_empty`, which is `Length(col) = 0`.
    #[test]
    fn renders_a_collection_length_as_a_count() {
        let stmt = collection_filter(stmt::Expr::eq(
            stmt::Expr::array_length(tags_column()),
            stmt::Expr::Value(stmt::Value::I64(0)),
        ));

        let sql = render(&collection_schema(), &stmt).unwrap();

        assert!(
            sql.contains("(SELECT COUNT(*) FROM OPENJSON(tbl_0_0.[tags])) = 0"),
            "got: {sql}"
        );
    }

    /// `push` and `extend` are both an append, and no T-SQL function joins two
    /// JSON arrays, so the stored and incoming canonical texts are spliced. Each
    /// guard matters: without the first an append onto an empty array leaves a
    /// leading comma behind, and without the second an empty incoming array
    /// truncates the stored one.
    #[test]
    fn renders_an_append_as_a_text_splice_of_both_arrays() {
        let mut assignments = stmt::Assignments::new();
        assignments.append(stmt::Projection::single(1), stmt::Expr::arg(0));

        let stmt = stmt::Statement::Update(stmt::Update {
            target: stmt::UpdateTarget::Table(db::TableId(0)),
            assignments,
            filter: stmt::Filter::default(),
            condition: stmt::Condition::default(),
            returning: None,
        });

        let sql = render(&collection_schema(), &stmt).unwrap();

        // The column and the incoming array are each referenced three times, and
        // a repeated `@pN` is one `sp_executesql` parameter, so the splice costs
        // no extra binds.
        assert_eq!(
            sql,
            "UPDATE [items] SET [tags] = CASE WHEN [tags] = N'[]' THEN @p1 \
             WHEN @p1 = N'[]' THEN [tags] \
             ELSE SUBSTRING([tags], 1, DATALENGTH([tags]) / 2 - 1) + N',' \
             + SUBSTRING(@p1, 2, DATALENGTH(@p1) / 2 - 1) END"
        );
    }

    /// A `#[document]` leaf is read with `JSON_VALUE`, cast back to the leaf's
    /// own type: `JSON_VALUE` answers with `nvarchar` whatever the value really
    /// is, so without the cast a number would compare as text and a timestamp as
    /// the string that spells it.
    ///
    /// Nothing is forced to a binary collation here, unlike the collection and
    /// `starts_with` paths. `JSON_VALUE` inherits the input's collation, and the
    /// suite requires a document leaf to match with the *same* case sensitivity
    /// as a plain column — so forcing one would make the leaf disagree with its
    /// own column.
    #[test]
    fn renders_a_document_leaf_as_a_casted_json_value() {
        // The base is whatever expression the plan chose; the column's declared
        // type does not affect the rendering under test.
        let extract = |path: &str, ty: stmt::Type| {
            stmt::Expr::from(stmt::ExprFunc::JsonExtract(stmt::FuncJsonExtract {
                base: Box::new(tags_column()),
                path: vec![path.to_owned()],
                ty,
            }))
        };

        let stmt = collection_filter(stmt::Expr::eq(
            extract("theme", stmt::Type::String),
            stmt::Expr::arg(0),
        ));
        let sql = render(&collection_schema(), &stmt).unwrap();

        assert!(
            sql.contains("CAST(JSON_VALUE(tbl_0_0.[tags], '$.theme') AS NVARCHAR(4000)) = @p1"),
            "got: {sql}"
        );

        // A leaf that is itself an object or array needs `JSON_QUERY`; asking
        // `JSON_VALUE` for one answers `NULL`.
        let stmt = collection_filter(stmt::Expr::is_not_null(extract(
            "address",
            stmt::Type::Object,
        )));
        let sql = render(&collection_schema(), &stmt).unwrap();

        assert!(
            sql.contains("JSON_QUERY(tbl_0_0.[tags], '$.address') IS NOT NULL"),
            "got: {sql}"
        );
    }
}
