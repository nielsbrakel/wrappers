use crate::stats;
use pgrx::{datum::datetime_support::to_timestamp, pg_sys, prelude::PgSqlErrorCode, JsonB};
use reqwest::{self, header, StatusCode, Url};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use serde_json::{json, Map as JsonMap, Number, Value as JsonValue};
use std::collections::HashMap;

use supabase_wrappers::prelude::*;

fn create_client(api_key: &str) -> ClientWithMiddleware {
    let mut headers = header::HeaderMap::new();
    let value = format!("Bearer {}", api_key);
    let mut auth_value = header::HeaderValue::from_str(&value).unwrap();
    auth_value.set_sensitive(true);
    headers.insert(header::AUTHORIZATION, auth_value);
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap();
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
    ClientBuilder::new(client)
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build()
}

fn body_to_rows(
    resp_body: &str,
    normal_cols: Vec<(&str, &str)>,
    tgt_cols: &[Column],
) -> (Vec<Row>, Option<String>, Option<bool>) {
    let mut result = Vec::new();
    let value: JsonValue = serde_json::from_str(resp_body).unwrap();
    let is_list = value
        .as_object()
        .and_then(|v| v.get("object"))
        .and_then(|v| v.as_str())
        .map(|v| v == "list")
        .unwrap_or_default();

    let is_balance = value
        .as_object()
        .and_then(|v| v.get("object"))
        .and_then(|v| v.as_str())
        .map(|v| v == "balance")
        .unwrap_or_default();

    let single_wrapped: Vec<JsonValue> = if is_list {
        Vec::new()
    } else if is_balance {
        // specially transform balance object to 2 rows
        let mut ret = Vec::new();
        for bal_type in &["available", "pending"] {
            let mut obj = value
                .as_object()
                .and_then(|v| v.get(*bal_type))
                .and_then(|v| v.as_array())
                .map(|v| v[0].as_object().unwrap().clone())
                .unwrap();
            obj.insert(
                "balance_type".to_string(),
                JsonValue::String(bal_type.to_string()),
            );
            ret.push(JsonValue::Object(obj));
        }
        ret
    } else {
        // wrap a single object in vec
        value
            .as_object()
            .map(|v| vec![JsonValue::Object(v.clone())])
            .unwrap()
    };
    let objs = if is_list {
        value
            .as_object()
            .and_then(|v| v.get("data"))
            .and_then(|v| v.as_array())
            .unwrap()
    } else {
        &single_wrapped
    };
    let mut cursor: Option<String> = None;

    for obj in objs {
        let mut row = Row::new();

        // extract normal columns
        for tgt_col in tgt_cols {
            if let Some((col_name, col_type)) = normal_cols.iter().find(|(c, _)| c == &tgt_col.name)
            {
                let cell = obj
                    .as_object()
                    .and_then(|v| v.get(*col_name))
                    .and_then(|v| match *col_type {
                        "bool" => v.as_bool().map(Cell::Bool),
                        "i64" => v.as_i64().map(Cell::I64),
                        "f64" => v.as_f64().map(Cell::F64),
                        "json" => v.as_i64().map(Cell::Json),
                        "string" => v.as_str().map(|a| Cell::String(a.to_owned())),
                        "timestamp" => v.as_i64().map(|a| {
                            let ts = to_timestamp(a as f64);
                            Cell::Timestamp(ts.to_utc())
                        }),
                        _ => None,
                    });
                row.push(col_name, cell);
            } else if &tgt_col.name == "attrs" {
                // put all properties into 'attrs' JSON column
                let attrs = serde_json::from_str(&obj.to_string()).unwrap();
                row.push("attrs", Some(Cell::Json(JsonB(attrs))));
            }
        }

        result.push(row);
    }

    // get last object's id as cursor
    if let Some(last_obj) = objs.last() {
        cursor = last_obj
            .as_object()
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .map(|v| v.to_owned());
    }

    // get 'has_more' attribute
    let has_more = value
        .as_object()
        .and_then(|v| v.get("has_more"))
        .and_then(|v| v.as_bool());

    (result, cursor, has_more)
}

fn row_to_body(row: &Row) -> JsonValue {
    let mut map = JsonMap::new();

    for (col_name, cell) in row.iter() {
        if let Some(cell) = cell {
            let col_name = col_name.to_owned();

            match cell {
                Cell::Bool(v) => {
                    map.insert(col_name, JsonValue::Bool(*v));
                }
                Cell::I64(v) => {
                    map.insert(col_name, JsonValue::Number(Number::from(*v)));
                }
                Cell::F64(v) => {
                    map.insert(col_name, JsonValue::Number(Decimal::from(*v)));
                }
                Cell::String(v) => {
                    map.insert(col_name, JsonValue::String(v.to_string()));
                }
                Cell::Json(v) => {
                    if col_name == "attrs" {
                        if let Some(m) = v.0.clone().as_object_mut() {
                            map.append(m)
                        }
                    } else {
                        map.insert(col_name, v);
                    }
                }
                _ => {
                    report_error(
                        PgSqlErrorCode::ERRCODE_FDW_INVALID_DATA_TYPE,
                        &format!("field type {:?} not supported", cell),
                    );
                    return JsonValue::Null;
                }
            }
        }
    }

    JsonValue::Object(map)
}

fn pushdown_quals(
    url: &mut Url,
    obj: &str,
    quals: &[Qual],
    fields: Vec<&str>,
    page_size: i64,
    cursor: &Option<String>,
) {
    // for scan with a single id query param, optimized to single object GET request
    if quals.len() == 1 {
        let qual = &quals[0];
        if qual.field == "id" && qual.operator == "=" && !qual.use_or {
            if let Value::Cell(Cell::String(id)) = &qual.value {
                let new_path = format!("{}/{}", url.path(), id);
                url.set_path(&new_path);
                url.set_query(None);
                return;
            }
        }
    }

    // pushdown quals
    for qual in quals {
        for field in &fields {
            if qual.field == *field && qual.operator == "=" && !qual.use_or {
                if let Value::Cell(cell) = &qual.value {
                    match cell {
                        Cell::Bool(b) => {
                            url.query_pairs_mut()
                                .append_pair(field, b.to_string().as_str());
                        }
                        Cell::String(s) => {
                            url.query_pairs_mut().append_pair(field, s);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // add pagination parameters except for 'balance' object
    if obj != "balance" {
        url.query_pairs_mut()
            .append_pair("limit", &format!("{}", page_size));
        if let Some(ref cursor) = cursor {
            url.query_pairs_mut().append_pair("starting_after", cursor);
        }
    }
}

// get stats metadata
#[inline]
fn get_stats_metadata() -> JsonB {
    stats::get_metadata(StripeFdw::FDW_NAME).unwrap_or(JsonB(json!({
        "request_cnt": 0i64,
    })))
}

// save stats metadata
#[inline]
fn set_stats_metadata(stats_metadata: JsonB) {
    stats::set_metadata(StripeFdw::FDW_NAME, Some(stats_metadata));
}

// increase stats metadata 'request_cnt' by 1
#[inline]
fn inc_stats_request_cnt(stats_metadata: &mut JsonB) {
    if let Some(v) = stats_metadata.0.get_mut("request_cnt") {
        *v = (v.as_i64().unwrap() + 1).into();
    };
}

macro_rules! report_request_error {
    ($err:ident) => {{
        report_error(
            PgSqlErrorCode::ERRCODE_FDW_ERROR,
            &format!("request failed: {}", $err),
        );
        return;
    }};
}

#[wrappers_fdw(
    version = "0.1.7",
    author = "Supabase",
    website = "https://github.com/supabase/wrappers/tree/main/wrappers/src/fdw/stripe_fdw"
)]
pub(crate) struct StripeFdw {
    rt: Runtime,
    base_url: Url,
    client: Option<ClientWithMiddleware>,
    scan_result: Option<Vec<Row>>,
    obj: String,
    rowid_col: String,
}

impl StripeFdw {
    const FDW_NAME: &str = "StripeFdw";

    fn build_url(
        &self,
        obj: &str,
        quals: &[Qual],
        page_size: i64,
        cursor: &Option<String>,
    ) -> Option<Url> {
        let mut url = self.base_url.join(obj).unwrap();

        // pushdown quals other than id
        // ref: https://stripe.com/docs/api/[object]/list
        let fields = match obj {
            // Core resources
            "balance" => vec![],
            "balance_transactions" => vec!["type"],
            "charges" => vec!["customer"],
            "customers" => vec!["email"],
            "disputes" => vec!["charge", "payment_intent"],
            "events" => vec!["type"],
            "files" => vec!["purpose"],
            "file_links" => vec![],
            "mandates" => vec![],
            "payment_intents" => vec!["customer"],
            "setup_attempts" => vec!["setup_intent"],
            "setup_intents" => vec!["customer", "payment_method"],
            "payouts" => vec!["status"],
            "refunds" => vec!["charge", "payment_intent"],
            "tokens" => vec![],

            // Products
            "products" => vec!["active"],
            "prices" => vec!["active", "currency", "product", "type"],
            "coupons" => vec![],
            "promotion_codes" => vec![],
            "tax_codes" => vec![],
            "tax_rates" => vec!["active"],
            "shipping_rates" => vec!["active", "created", "currency"],

            // Checkout
            "checkout/sessions" => vec!["customer", "payment_intent", "subscription"],

            // Billing
            "invoices" => vec!["customer", "status", "subscription"],
            "subscriptions" => vec!["customer", "price", "status"],

            // Connect
            "accounts" => vec![],
            "topups" => vec!["status"],
            "transfers" => vec!["destination"],
            _ => {
                report_error(
                    PgSqlErrorCode::ERRCODE_FDW_TABLE_NOT_FOUND,
                    &format!("'{}' object is not implemented", obj),
                );
                return None;
            }
        };
        pushdown_quals(&mut url, obj, quals, fields, page_size, cursor);

        Some(url)
    }

    // convert response body text to rows
    fn resp_to_rows(
        &self,
        obj: &str,
        resp_body: &str,
        tgt_cols: &[Column],
    ) -> (Vec<Row>, Option<String>, Option<bool>) {
        match obj {
            // Core resources
            "balance" => body_to_rows(
                resp_body,
                vec![
                    ("balance_type", "string"),
                    ("amount", "i64"),
                    ("currency", "string"),
                ],
                tgt_cols,
            ),
            "balance_transactions" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("amount", "i64"),
                    ("currency", "string"),
                    ("description", "string"),
                    ("fee", "i64"),
                    ("net", "i64"),
                    ("status", "string"),
                    ("type", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "charges" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("amount", "i64"),
                    ("currency", "string"),
                    ("customer", "string"),
                    ("description", "string"),
                    ("invoice", "string"),
                    ("payment_intent", "string"),
                    ("status", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "customers" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("email", "string"),
                    ("name", "string"),
                    ("description", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "disputes" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("amount", "i64"),
                    ("currency", "string"),
                    ("charge", "string"),
                    ("payment_intent", "string"),
                    ("reason", "string"),
                    ("status", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "events" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("type", "string"),
                    ("api_version", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "files" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("filename", "string"),
                    ("purpose", "string"),
                    ("title", "string"),
                    ("size", "i64"),
                    ("type", "string"),
                    ("url", "string"),
                    ("created", "timestamp"),
                    ("expires_at", "timestamp"),
                ],
                tgt_cols,
            ),
            "file_links" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("file", "string"),
                    ("url", "string"),
                    ("created", "timestamp"),
                    ("expired", "bool"),
                    ("expires_at", "timestamp"),
                ],
                tgt_cols,
            ),
            "mandates" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("payment_method", "string"),
                    ("status", "string"),
                    ("type", "string"),
                ],
                tgt_cols,
            ),
            "payment_intents" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("customer", "string"),
                    ("amount", "i64"),
                    ("currency", "string"),
                    ("payment_method", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "payouts" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("amount", "i64"),
                    ("currency", "string"),
                    ("arrival_date", "timestamp"),
                    ("description", "string"),
                    ("statement_descriptor", "string"),
                    ("status", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "refunds" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("amount", "i64"),
                    ("currency", "string"),
                    ("charge", "string"),
                    ("payment_intent", "string"),
                    ("reason", "string"),
                    ("status", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "setup_attempts" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("application", "string"),
                    ("customer", "string"),
                    ("on_behalf_of", "string"),
                    ("payment_method", "string"),
                    ("setup_intent", "string"),
                    ("status", "string"),
                    ("usage", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "setup_intents" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("client_secret", "string"),
                    ("customer", "string"),
                    ("description", "string"),
                    ("payment_method", "string"),
                    ("status", "string"),
                    ("usage", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "tokens" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("type", "string"),
                    ("client_ip", "string"),
                    ("used", "bool"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),

            // Products
            "products" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("name", "string"),
                    ("active", "bool"),
                    ("default_price", "string"),
                    ("description", "string"),
                    ("created", "timestamp"),
                    ("updated", "timestamp"),
                ],
                tgt_cols,
            ),
            "prices" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("active", "bool"),
                    ("currency", "string"),
                    ("product", "string"),
                    ("unit_amount", "i64"),
                    ("type", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "coupons" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("amount_off", "i64"),
                    ("currency", "string"),
                    ("duration", "string"),
                    ("duration_in_months", "i64"),
                    ("max_redemptions", "i64"),
                    ("name", "string"),
                    ("percent_off", "f64"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "promotion_codes" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("code", "string"),
                    ("coupon", "string"),
                    ("active", "bool"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "tax_codes" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("description", "string"),
                    ("name", "string"),
                ],
                tgt_cols,
            ),
            "tax_rates" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("active", "bool"),
                    ("country", "string"),
                    ("description", "string"),
                    ("display_name", "string"),
                    ("inclusive", "bool"),
                    ("percentage", "f64"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "shipping_rates" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("active", "bool"),
                    ("display_name", "string"),
                    ("amount", "string"),
                    ("type", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),

            // Checkout
            "checkout/sessions" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("customer", "string"),
                    ("payment_intent", "string"),
                    ("subscription", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),

            // Billing
            "invoices" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("customer", "string"),
                    ("subscription", "string"),
                    ("status", "string"),
                    ("total", "i64"),
                    ("currency", "string"),
                    ("period_start", "timestamp"),
                    ("period_end", "timestamp"),
                ],
                tgt_cols,
            ),
            "subscriptions" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("customer", "string"),
                    ("currency", "string"),
                    ("current_period_start", "timestamp"),
                    ("current_period_end", "timestamp"),
                ],
                tgt_cols,
            ),

            // Connect
            "accounts" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("business_type", "string"),
                    ("country", "string"),
                    ("email", "string"),
                    ("type", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "topups" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("amount", "i64"),
                    ("currency", "string"),
                    ("description", "string"),
                    ("status", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "transfers" => body_to_rows(
                resp_body,
                vec![
                    ("id", "string"),
                    ("amount", "i64"),
                    ("currency", "string"),
                    ("description", "string"),
                    ("destination", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            _ => {
                report_error(
                    PgSqlErrorCode::ERRCODE_FDW_TABLE_NOT_FOUND,
                    &format!("'{}' object is not implemented", obj),
                );
                (Vec::new(), None, None)
            }
        }
    }
}

impl ForeignDataWrapper for StripeFdw {
    fn new(options: &HashMap<String, String>) -> Self {
        let base_url = options
            .get("api_url")
            .map(|t| t.to_owned())
            // Ensure trailing slash is always present, otherwise /v1 will get obliterated when
            // joined with object
            .map(|s| {
                if s.ends_with('/') {
                    s
                } else {
                    format!("{}/", s)
                }
            })
            .unwrap_or_else(|| "https://api.stripe.com/v1/".to_string());
        let client = match options.get("api_key") {
            Some(api_key) => Some(create_client(api_key)),
            None => require_option("api_key_id", options)
                .and_then(|key_id| get_vault_secret(&key_id))
                .map(|api_key| create_client(&api_key)),
        };

        stats::inc_stats(Self::FDW_NAME, stats::Metric::CreateTimes, 1);

        StripeFdw {
            rt: create_async_runtime(),
            base_url: Url::parse(&base_url).unwrap(),
            client,
            scan_result: None,
            obj: String::default(),
            rowid_col: String::default(),
        }
    }

    fn begin_scan(
        &mut self,
        quals: &[Qual],
        columns: &[Column],
        _sorts: &[Sort],
        limit: &Option<Limit>,
        options: &HashMap<String, String>,
    ) {
        let obj = if let Some(name) = require_option("object", options) {
            name
        } else {
            return;
        };

        if let Some(client) = &self.client {
            let page_size = 100; // maximum page size limit for Stripe API
            let page_cnt = if let Some(limit) = limit {
                if limit.count == 0 {
                    return;
                }
                (limit.offset + limit.count) / page_size + 1
            } else {
                // if no limit specified, fetch all records
                i64::MAX
            };
            let mut page = 0;
            let mut result = Vec::new();
            let mut cursor: Option<String> = None;
            let mut stats_metadata = get_stats_metadata();

            while page < page_cnt {
                // build url
                let url = self.build_url(&obj, quals, page_size, &cursor);
                if url.is_none() {
                    return;
                }
                let url = url.unwrap();

                // make api call
                inc_stats_request_cnt(&mut stats_metadata);
                match self.rt.block_on(client.get(url).send()) {
                    Ok(resp) => {
                        stats::inc_stats(
                            Self::FDW_NAME,
                            stats::Metric::BytesIn,
                            resp.content_length().unwrap_or(0) as i64,
                        );

                        if resp.status() == StatusCode::NOT_FOUND {
                            // if it is 404 error, we should treat it as an empty
                            // result rather than a request error
                            break;
                        }

                        match resp.error_for_status() {
                            Ok(resp) => {
                                let body = self.rt.block_on(resp.text()).unwrap();
                                let (rows, starting_after, has_more) =
                                    self.resp_to_rows(&obj, &body, columns);
                                if rows.is_empty() {
                                    break;
                                }
                                result.extend(rows);
                                match has_more {
                                    Some(has_more) => {
                                        if !has_more {
                                            break;
                                        }
                                    }
                                    None => break,
                                }
                                cursor = starting_after;
                            }
                            Err(err) => report_request_error!(err),
                        }
                    }
                    Err(err) => report_request_error!(err),
                }

                page += 1;
            }

            // save stats
            stats::inc_stats(Self::FDW_NAME, stats::Metric::RowsIn, result.len() as i64);
            stats::inc_stats(Self::FDW_NAME, stats::Metric::RowsOut, result.len() as i64);
            set_stats_metadata(stats_metadata);

            self.scan_result = Some(result);
        }
    }

    fn iter_scan(&mut self, row: &mut Row) -> Option<()> {
        if let Some(ref mut result) = self.scan_result {
            if !result.is_empty() {
                return result
                    .drain(0..1)
                    .last()
                    .map(|src_row| row.replace_with(src_row));
            }
        }
        None
    }

    fn end_scan(&mut self) {
        self.scan_result.take();
    }

    fn begin_modify(&mut self, options: &HashMap<String, String>) {
        self.obj = require_option("object", options).unwrap_or_default();
        self.rowid_col = require_option("rowid_column", options).unwrap_or_default();
    }

    fn insert(&mut self, src: &Row) {
        if let Some(ref mut client) = self.client {
            let url = self.base_url.join(&self.obj).unwrap();
            let body = row_to_body(src);
            if body.is_null() {
                return;
            }

            let mut stats_metadata = get_stats_metadata();

            // call Stripe API
            inc_stats_request_cnt(&mut stats_metadata);
            match self.rt.block_on(client.post(url).form(&body).send()) {
                Ok(resp) => match resp.error_for_status() {
                    Ok(resp) => {
                        stats::inc_stats(
                            Self::FDW_NAME,
                            stats::Metric::BytesIn,
                            resp.content_length().unwrap_or(0) as i64,
                        );
                        let body = self.rt.block_on(resp.text()).unwrap();
                        let json: JsonValue = serde_json::from_str(&body).unwrap();
                        if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
                            report_info(&format!("inserted {} {}", self.obj, id));
                        }
                    }
                    Err(err) => report_request_error!(err),
                },
                Err(err) => report_request_error!(err),
            }

            set_stats_metadata(stats_metadata);
        }
    }

    fn update(&mut self, rowid: &Cell, new_row: &Row) {
        if let Some(ref mut client) = self.client {
            let mut stats_metadata = get_stats_metadata();

            match rowid {
                Cell::String(rowid) => {
                    let url = self
                        .base_url
                        .join(&format!("{}/", self.obj))
                        .unwrap()
                        .join(rowid)
                        .unwrap();
                    let body = row_to_body(new_row);
                    if body.is_null() {
                        return;
                    }

                    // call Stripe API
                    inc_stats_request_cnt(&mut stats_metadata);
                    match self.rt.block_on(client.post(url).form(&body).send()) {
                        Ok(resp) => match resp.error_for_status() {
                            Ok(resp) => {
                                stats::inc_stats(
                                    Self::FDW_NAME,
                                    stats::Metric::BytesIn,
                                    resp.content_length().unwrap_or(0) as i64,
                                );
                                let body = self.rt.block_on(resp.text()).unwrap();
                                let json: JsonValue = serde_json::from_str(&body).unwrap();
                                if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
                                    report_info(&format!("updated {} {}", self.obj, id));
                                }
                            }
                            Err(err) => report_request_error!(err),
                        },
                        Err(err) => report_request_error!(err),
                    }
                }
                _ => unreachable!(),
            }

            set_stats_metadata(stats_metadata);
        }
    }

    fn delete(&mut self, rowid: &Cell) {
        if let Some(ref mut client) = self.client {
            let mut stats_metadata = get_stats_metadata();

            match rowid {
                Cell::String(rowid) => {
                    let url = self
                        .base_url
                        .join(&format!("{}/", self.obj))
                        .unwrap()
                        .join(rowid)
                        .unwrap();

                    // call Stripe API
                    inc_stats_request_cnt(&mut stats_metadata);
                    match self.rt.block_on(client.delete(url).send()) {
                        Ok(resp) => match resp.error_for_status() {
                            Ok(resp) => {
                                stats::inc_stats(
                                    Self::FDW_NAME,
                                    stats::Metric::BytesIn,
                                    resp.content_length().unwrap_or(0) as i64,
                                );
                                let body = self.rt.block_on(resp.text()).unwrap();
                                let json: JsonValue = serde_json::from_str(&body).unwrap();
                                if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
                                    report_info(&format!("deleted {} {}", self.obj, id));
                                }
                            }
                            Err(err) => report_request_error!(err),
                        },
                        Err(err) => report_request_error!(err),
                    }
                }
                _ => unreachable!(),
            }

            set_stats_metadata(stats_metadata);
        }
    }

    fn end_modify(&mut self) {}

    fn validator(options: Vec<Option<String>>, catalog: Option<pg_sys::Oid>) {
        if let Some(oid) = catalog {
            if oid == FOREIGN_TABLE_RELATION_ID {
                check_options_contain(&options, "object");
            }
        }
    }
}
