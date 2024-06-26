// Copyright 2024 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::rc::Rc;

use fixedbitset::FixedBitSet;
use pgwire::pg_field_descriptor::PgFieldDescriptor;
use pgwire::pg_response::{PgResponse, StatementType};
use risingwave_common::session_config::QueryMode;
use risingwave_common::util::epoch::Epoch;
use risingwave_sqlparser::ast::{DeclareCursorStatement, ObjectName, Query, Since, Statement};

use super::query::{gen_batch_plan_by_statement, gen_batch_plan_fragmenter, BatchQueryPlanResult};
use super::util::{convert_epoch_to_logstore_i64, convert_unix_millis_to_logstore_i64};
use super::RwPgResponse;
use crate::error::{ErrorCode, Result};
use crate::handler::query::create_stream;
use crate::handler::HandlerArgs;
use crate::optimizer::plan_node::{generic, BatchLogSeqScan};
use crate::optimizer::property::{Order, RequiredDist};
use crate::optimizer::PlanRoot;
use crate::{Binder, OptimizerContext, PgResponseStream, PlanRef, TableCatalog};

pub async fn handle_declare_cursor(
    handle_args: HandlerArgs,
    stmt: DeclareCursorStatement,
) -> Result<RwPgResponse> {
    match stmt.declare_cursor {
        risingwave_sqlparser::ast::DeclareCursor::Query(query) => {
            handle_declare_query_cursor(handle_args, stmt.cursor_name, query).await
        }
        risingwave_sqlparser::ast::DeclareCursor::Subscription(sub_name, rw_timestamp) => {
            handle_declare_subscription_cursor(
                handle_args,
                sub_name,
                stmt.cursor_name,
                rw_timestamp,
            )
            .await
        }
    }
}
async fn handle_declare_subscription_cursor(
    handle_args: HandlerArgs,
    sub_name: ObjectName,
    cursor_name: ObjectName,
    rw_timestamp: Option<Since>,
) -> Result<RwPgResponse> {
    let session = handle_args.session.clone();
    let db_name = session.database();
    let (schema_name, cursor_name) =
        Binder::resolve_schema_qualified_name(db_name, cursor_name.clone())?;

    let cursor_from_subscription_name = sub_name.0.last().unwrap().real_value().clone();
    let subscription =
        session.get_subscription_by_name(schema_name, &cursor_from_subscription_name)?;
    // Start the first query of cursor, which includes querying the table and querying the subscription's logstore
    let start_rw_timestamp = match rw_timestamp {
        Some(risingwave_sqlparser::ast::Since::TimestampMsNum(start_rw_timestamp)) => {
            check_cursor_unix_millis(start_rw_timestamp, subscription.get_retention_seconds()?)?;
            Some(convert_unix_millis_to_logstore_i64(start_rw_timestamp))
        }
        Some(risingwave_sqlparser::ast::Since::ProcessTime) => {
            Some(convert_epoch_to_logstore_i64(Epoch::now().0))
        }
        Some(risingwave_sqlparser::ast::Since::Begin) => {
            let min_unix_millis =
                Epoch::now().as_unix_millis() - subscription.get_retention_seconds()? * 1000;
            Some(convert_unix_millis_to_logstore_i64(min_unix_millis))
        }
        None => None,
    };
    // Create cursor based on the response
    session
        .get_cursor_manager()
        .add_subscription_cursor(
            cursor_name.clone(),
            start_rw_timestamp,
            subscription,
            &handle_args,
        )
        .await?;

    Ok(PgResponse::empty_result(StatementType::DECLARE_CURSOR))
}

fn check_cursor_unix_millis(unix_millis: u64, retention_seconds: u64) -> Result<()> {
    let now = Epoch::now().as_unix_millis();
    let min_unix_millis = now - retention_seconds * 1000;
    if unix_millis > now {
        return Err(ErrorCode::CatalogError(
            "rw_timestamp is too large, need to be less than the current unix_millis"
                .to_string()
                .into(),
        )
        .into());
    }
    if unix_millis < min_unix_millis {
        return Err(ErrorCode::CatalogError("rw_timestamp is too small, need to be large than the current unix_millis - subscription's retention time".to_string().into()).into());
    }
    Ok(())
}

async fn handle_declare_query_cursor(
    handle_args: HandlerArgs,
    cursor_name: ObjectName,
    query: Box<Query>,
) -> Result<RwPgResponse> {
    let (row_stream, pg_descs) =
        create_stream_for_cursor(handle_args.clone(), Statement::Query(query)).await?;
    handle_args
        .session
        .get_cursor_manager()
        .add_query_cursor(cursor_name, row_stream, pg_descs)
        .await?;
    Ok(PgResponse::empty_result(StatementType::DECLARE_CURSOR))
}

pub async fn create_stream_for_cursor(
    handle_args: HandlerArgs,
    stmt: Statement,
) -> Result<(PgResponseStream, Vec<PgFieldDescriptor>)> {
    let session = handle_args.session.clone();
    let plan_fragmenter_result = {
        let context = OptimizerContext::from_handler_args(handle_args);
        let plan_result = gen_batch_plan_by_statement(&session, context.into(), stmt)?;
        gen_batch_plan_fragmenter(&session, plan_result)?
    };
    create_stream(session, plan_fragmenter_result, vec![]).await
}

pub fn create_batch_plan_for_cursor(
    table_catalog: std::sync::Arc<TableCatalog>,
    handle_args: HandlerArgs,
    old_epoch: u64,
    new_epoch: u64,
) -> Result<BatchQueryPlanResult> {
    let context = OptimizerContext::from_handler_args(handle_args.clone());
    let out_col_idx = table_catalog
        .columns
        .iter()
        .enumerate()
        .filter(|(_, v)| !v.is_hidden)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let core = generic::LogScan::new(
        table_catalog.name.clone(),
        out_col_idx,
        Rc::new(table_catalog.table_desc()),
        Rc::new(context),
        old_epoch,
        new_epoch,
    );
    let batch_log_seq_scan = BatchLogSeqScan::new(core);
    let out_fields = FixedBitSet::from_iter(0..batch_log_seq_scan.core().schema().len());
    let out_names = batch_log_seq_scan.core().column_names();
    // Here we just need a plan_root to call the method, only out_fields and out_names will be used
    let mut plan_root = PlanRoot::new(
        PlanRef::from(batch_log_seq_scan.clone()),
        RequiredDist::single(),
        Order::default(),
        out_fields,
        out_names,
    );
    let schema = batch_log_seq_scan.core().schema().clone();
    let (batch_log_seq_scan, query_mode) = match handle_args.session.config().query_mode() {
        QueryMode::Auto => (
            plan_root.gen_batch_distributed_plan(PlanRef::from(batch_log_seq_scan))?,
            QueryMode::Local,
        ),
        QueryMode::Local => (
            plan_root.gen_batch_local_plan(PlanRef::from(batch_log_seq_scan))?,
            QueryMode::Local,
        ),
        QueryMode::Distributed => (
            plan_root.gen_batch_distributed_plan(PlanRef::from(batch_log_seq_scan))?,
            QueryMode::Distributed,
        ),
    };
    Ok(BatchQueryPlanResult {
        plan: batch_log_seq_scan,
        query_mode,
        schema,
        stmt_type: StatementType::SELECT,
        dependent_relations: table_catalog.dependent_relations.clone(),
    })
}
