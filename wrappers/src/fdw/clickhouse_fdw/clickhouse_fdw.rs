use crate::stats;
use chrono::{Date, DateTime, NaiveDate, NaiveDateTime, Utc};
use chrono_tz::Tz;
use clickhouse_rs::{types, types::Block, types::SqlType, ClientHandle, Pool};
use pgrx::pg_sys::panic::ErrorReport;
use pgrx::{prelude::PgSqlErrorCode, to_timestamp};
use regex::{Captures, Regex};
use std::collections::HashMap;

use supabase_wrappers::prelude::*;

fn field_to_cell(row: &types::Row<types::Complex>, i: usize) -> Option<Cell> {
    let sql_type = row.sql_type(i).unwrap();
    match sql_type {
        SqlType::UInt8 => {
            // Bool is stored as UInt8 in ClickHouse, so we treat it as bool here
            let value = row.get::<u8, usize>(i).unwrap();
            Some(Cell::Bool(value != 0))
        }
        SqlType::Int16 => {
            let value = row.get::<i16, usize>(i).unwrap();
            Some(Cell::I16(value))
        }
        SqlType::UInt16 => {
            let value = row.get::<u16, usize>(i).unwrap();
            Some(Cell::I32(value as i32))
        }
        SqlType::Int32 => {
            let value = row.get::<i32, usize>(i).unwrap();
            Some(Cell::I32(value))
        }
        SqlType::UInt32 => {
            let value = row.get::<u32, usize>(i).unwrap();
            Some(Cell::I64(value as i64))
        }
        SqlType::Float32 => {
            let value = row.get::<f32, usize>(i).unwrap();
            Some(Cell::F32(value))
        }
        SqlType::Float64 => {
            let value = row.get::<f64, usize>(i).unwrap();
            Some(Cell::F64(value))
        }
        SqlType::UInt64 => {
            let value = row.get::<u64, usize>(i).unwrap();
            Some(Cell::I64(value as i64))
        }
        SqlType::Int64 => {
            let value = row.get::<i64, usize>(i).unwrap();
            Some(Cell::I64(value))
        }
        SqlType::String => {
            let value = row.get::<String, usize>(i).unwrap();
            Some(Cell::String(value))
        }
        SqlType::Date => {
            let value = row.get::<Date<_>, usize>(i).unwrap();
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            let seconds_from_epoch = value.naive_utc().signed_duration_since(epoch).num_seconds();
            let ts = to_timestamp(seconds_from_epoch as f64);
            Some(Cell::Date(pgrx::Date::from(ts)))
        }
        SqlType::DateTime(_) => {
            let value = row.get::<DateTime<_>, usize>(i).unwrap();
            let ts = to_timestamp(value.timestamp() as f64);
            Some(Cell::Timestamp(ts.to_utc()))
        }
        _ => {
            report_error(
                PgSqlErrorCode::ERRCODE_FDW_INVALID_DATA_TYPE,
                &format!("data type {} is not supported", sql_type.to_string()),
            );
            None
        }
    }
}

#[wrappers_fdw(
    version = "0.1.3",
    author = "Supabase",
    website = "https://github.com/supabase/wrappers/tree/main/wrappers/src/fdw/clickhouse_fdw",
    error_type = "ClickHouseFdwError"
)]
pub(crate) struct ClickHouseFdw {
    rt: Runtime,
    conn_str: String,
    client: Option<ClientHandle>,
    table: String,
    rowid_col: String,
    tgt_cols: Vec<Column>,
    scan_blk: Option<Block<types::Complex>>,
    row_idx: usize,
    params: Vec<Qual>,
}

impl ClickHouseFdw {
    const FDW_NAME: &str = "ClickHouseFdw";

    fn create_client(&mut self) {
        let pool = Pool::new(self.conn_str.as_str());
        self.client = self.rt.block_on(pool.get_handle()).map_or_else(
            |err| {
                report_error(
                    PgSqlErrorCode::ERRCODE_FDW_UNABLE_TO_ESTABLISH_CONNECTION,
                    &format!("connection failed: {}", err),
                );
                None
            },
            Some,
        );
    }

    fn deparse(
        &mut self,
        quals: &[Qual],
        columns: &[Column],
        sorts: &[Sort],
        limit: &Option<Limit>,
    ) -> String {
        let table = if self.table.starts_with('(') {
            let re = Regex::new(r"\$\{(\w+)\}").unwrap();
            re.replace_all(&self.table, |caps: &Captures| {
                let param = &caps[1];
                match quals.iter().find(|&q| q.field == param) {
                    Some(qual) => {
                        self.params.push(qual.clone());
                        match &qual.value {
                            Value::Cell(cell) => cell.to_string(),
                            Value::Array(_) => {
                                report_error(
                                    PgSqlErrorCode::ERRCODE_FDW_ERROR,
                                    "invalid query parameter",
                                );
                                String::default()
                            }
                        }
                    }
                    None => {
                        report_error(
                            PgSqlErrorCode::ERRCODE_FDW_ERROR,
                            &format!("unmatched query parameter: {}", param),
                        );
                        String::default()
                    }
                }
            })
            .into_owned()
        } else {
            self.table.clone()
        };

        let tgts = if columns.is_empty() {
            "*".to_string()
        } else {
            columns
                .iter()
                .filter(|c| !self.params.iter().any(|p| p.field == c.name))
                .map(|c| c.name.clone())
                .collect::<Vec<String>>()
                .join(", ")
        };

        let mut sql = format!("select {} from {}", tgts, &table);

        if !quals.is_empty() {
            let cond = quals
                .iter()
                .filter(|q| !self.params.iter().any(|p| p.field == q.field))
                .map(|q| q.deparse())
                .collect::<Vec<String>>()
                .join(" and ");

            if !cond.is_empty() {
                sql.push_str(&format!(" where {}", cond));
            }
        }

        // push down sorts
        if !sorts.is_empty() {
            let order_by = sorts
                .iter()
                .map(|sort| sort.deparse())
                .collect::<Vec<String>>()
                .join(", ");
            sql.push_str(&format!(" order by {}", order_by));
        }

        // push down limits
        // Note: Postgres will take limit and offset locally after reading rows
        // from remote, so we calculate the real limit and only use it without
        // pushing down offset.
        if let Some(limit) = limit {
            let real_limit = limit.offset + limit.count;
            sql.push_str(&format!(" limit {}", real_limit));
        }

        sql
    }
}

enum ClickHouseFdwError {}

impl From<ClickHouseFdwError> for ErrorReport {
    fn from(_value: ClickHouseFdwError) -> Self {
        ErrorReport::new(PgSqlErrorCode::ERRCODE_FDW_ERROR, "", "")
    }
}

impl ForeignDataWrapper<ClickHouseFdwError> for ClickHouseFdw {
    fn new(options: &HashMap<String, String>) -> Result<Self, ClickHouseFdwError> {
        let rt = create_async_runtime();
        let conn_str = match options.get("conn_string") {
            Some(conn_str) => conn_str.to_owned(),
            None => require_option("conn_string_id", options)
                .and_then(|conn_str_id| get_vault_secret(&conn_str_id))
                .unwrap_or_default(),
        };

        stats::inc_stats(Self::FDW_NAME, stats::Metric::CreateTimes, 1);

        Ok(Self {
            rt,
            conn_str,
            client: None,
            table: String::default(),
            rowid_col: String::default(),
            tgt_cols: Vec::new(),
            scan_blk: None,
            row_idx: 0,
            params: Vec::new(),
        })
    }

    fn begin_scan(
        &mut self,
        quals: &[Qual],
        columns: &[Column],
        sorts: &[Sort],
        limit: &Option<Limit>,
        options: &HashMap<String, String>,
    ) -> Result<(), ClickHouseFdwError> {
        self.create_client();

        let table = require_option("table", options);
        if table.is_none() {
            return Ok(());
        }
        self.table = table.unwrap();
        self.tgt_cols = columns.to_vec();
        self.row_idx = 0;

        let sql = self.deparse(quals, columns, sorts, limit);

        if let Some(ref mut client) = self.client {
            // for simplicity purpose, we fetch whole query result to local,
            // may need optimization in the future.
            match self.rt.block_on(client.query(&sql).fetch_all()) {
                Ok(block) => {
                    stats::inc_stats(
                        Self::FDW_NAME,
                        stats::Metric::RowsIn,
                        block.row_count() as i64,
                    );
                    stats::inc_stats(
                        Self::FDW_NAME,
                        stats::Metric::RowsOut,
                        block.row_count() as i64,
                    );
                    self.scan_blk = Some(block);
                }
                Err(err) => report_error(
                    PgSqlErrorCode::ERRCODE_FDW_ERROR,
                    &format!("query failed: {}", err),
                ),
            }
        }

        Ok(())
    }

    fn iter_scan(&mut self, row: &mut Row) -> Result<Option<()>, ClickHouseFdwError> {
        if let Some(block) = &self.scan_blk {
            let mut rows = block.rows();

            if let Some(src_row) = rows.nth(self.row_idx) {
                for tgt_col in &self.tgt_cols {
                    if let Some(param) = self.params.iter().find(|&p| p.field == tgt_col.name) {
                        if let Value::Cell(cell) = &param.value {
                            row.push(&tgt_col.name, Some(cell.clone()));
                        }
                        continue;
                    }

                    let (i, _) = block
                        .columns()
                        .iter()
                        .enumerate()
                        .find(|(_, c)| c.name() == tgt_col.name)
                        .unwrap();
                    let cell = field_to_cell(&src_row, i);
                    let col_name = src_row.name(i).unwrap();
                    if cell.as_ref().is_none() {
                        return Ok(None);
                    }
                    row.push(col_name, cell);
                }
                self.row_idx += 1;
                return Ok(Some(()));
            }
        }
        Ok(None)
    }

    fn end_scan(&mut self) -> Result<(), ClickHouseFdwError> {
        self.scan_blk.take();
        Ok(())
    }

    fn begin_modify(
        &mut self,
        options: &HashMap<String, String>,
    ) -> Result<(), ClickHouseFdwError> {
        self.create_client();

        let table = require_option("table", options);
        let rowid_col = require_option("rowid_column", options);
        if table.is_none() || rowid_col.is_none() {
            return Ok(());
        }
        self.table = table.unwrap();
        self.rowid_col = rowid_col.unwrap();
        Ok(())
    }

    fn insert(&mut self, src: &Row) -> Result<(), ClickHouseFdwError> {
        if let Some(ref mut client) = self.client {
            let mut row = Vec::new();
            for (col_name, cell) in src.iter() {
                let col_name = col_name.to_owned();
                if let Some(cell) = cell {
                    match cell {
                        Cell::Bool(v) => row.push((col_name, types::Value::from(*v))),
                        Cell::F64(v) => row.push((col_name, types::Value::from(*v))),
                        Cell::I64(v) => row.push((col_name, types::Value::from(*v))),
                        Cell::String(v) => row.push((col_name, types::Value::from(v.as_str()))),
                        Cell::Date(_) => {
                            let s = cell.to_string().replace('\'', "");
                            if let Ok(tm) = NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
                                let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                                let duration = tm - epoch;
                                let dt = types::Value::Date(duration.num_days() as u16, Tz::UTC);
                                row.push((col_name, dt));
                            } else {
                                report_error(
                                    PgSqlErrorCode::ERRCODE_FDW_INVALID_STRING_FORMAT,
                                    &format!("invalid date format {}", s),
                                );
                            }
                        }
                        Cell::Timestamp(_) => {
                            let s = cell.to_string().replace('\'', "");
                            if let Ok(tm) = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S") {
                                let tm: DateTime<Utc> = DateTime::from_utc(tm, Utc);
                                row.push((col_name, types::Value::from(tm)));
                            } else {
                                report_error(
                                    PgSqlErrorCode::ERRCODE_FDW_INVALID_STRING_FORMAT,
                                    &format!("invalid timestamp format {}", s),
                                );
                            }
                        }
                        _ => report_error(
                            PgSqlErrorCode::ERRCODE_FDW_INVALID_DATA_TYPE,
                            &format!("field type {:?} not supported", cell),
                        ),
                    }
                }
            }
            let mut block = Block::new();
            block.push(row).unwrap();

            // execute query on ClickHouse
            if let Err(err) = self.rt.block_on(client.insert(&self.table, block)) {
                report_error(
                    PgSqlErrorCode::ERRCODE_FDW_ERROR,
                    &format!("insert failed: {}", err),
                );
            }
        }
        Ok(())
    }

    fn update(&mut self, rowid: &Cell, new_row: &Row) -> Result<(), ClickHouseFdwError> {
        if let Some(ref mut client) = self.client {
            let mut sets = Vec::new();
            for (col, cell) in new_row.iter() {
                if col == &self.rowid_col {
                    continue;
                }
                if let Some(cell) = cell {
                    sets.push(format!("{} = {}", col, cell));
                } else {
                    sets.push(format!("{} = null", col));
                }
            }
            let sql = format!(
                "alter table {} update {} where {} = {}",
                self.table,
                sets.join(", "),
                self.rowid_col,
                rowid
            );

            // execute query on ClickHouse
            if let Err(err) = self.rt.block_on(client.execute(&sql)) {
                report_error(
                    PgSqlErrorCode::ERRCODE_FDW_ERROR,
                    &format!("update failed: {}", err),
                );
            }
        }
        Ok(())
    }

    fn delete(&mut self, rowid: &Cell) -> Result<(), ClickHouseFdwError> {
        if let Some(ref mut client) = self.client {
            let sql = format!(
                "alter table {} delete where {} = {}",
                self.table, self.rowid_col, rowid
            );

            // execute query on ClickHouse
            if let Err(err) = self.rt.block_on(client.execute(&sql)) {
                report_error(
                    PgSqlErrorCode::ERRCODE_FDW_ERROR,
                    &format!("delete failed: {}", err),
                );
            }
        }
        Ok(())
    }
}
