// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Implementation of the SparkConnectServiceClient

use std::sync::Arc;

use tokio::sync::RwLock;

use tonic::codec::Streaming;
use tonic::codegen::{Body, Bytes, StdError};
use tonic::transport::Channel;

use crate::spark;
use spark::execute_plan_response::ResponseType;
use spark::spark_connect_service_client::SparkConnectServiceClient;

use arrow::compute::concat_batches;
use arrow::error::ArrowError;
use arrow::record_batch::RecordBatch;
use arrow_ipc::reader::StreamReader;

use uuid::Uuid;

use crate::errors::SparkError;

mod builder;
mod config;
mod middleware;

pub use builder::ChannelBuilder;
pub use config::Config;
pub use middleware::{HeadersLayer, HeadersMiddleware};

pub type SparkClient = SparkConnectClient<HeadersMiddleware<Channel>>;

#[allow(dead_code)]
#[derive(Default, Debug, Clone)]
pub(crate) struct ResponseHandler {
    metrics: Option<spark::execute_plan_response::Metrics>,
    observed_metrics: Option<spark::execute_plan_response::ObservedMetrics>,
    pub(crate) schema: Option<spark::DataType>,
    batches: Vec<RecordBatch>,
    pub(crate) sql_command_result: Option<spark::execute_plan_response::SqlCommandResult>,
    pub(crate) write_stream_operation_start_result: Option<spark::WriteStreamOperationStartResult>,
    pub(crate) streaming_query_command_result: Option<spark::StreamingQueryCommandResult>,
    pub(crate) get_resources_command_result: Option<spark::GetResourcesCommandResult>,
    pub(crate) streaming_query_manager_command_result:
        Option<spark::StreamingQueryManagerCommandResult>,
    pub(crate) result_complete: bool,
    total_count: isize,
}

#[derive(Default, Debug, Clone)]
pub(crate) struct AnalyzeHandler {
    pub(crate) schema: Option<spark::DataType>,
    pub(crate) explain: Option<String>,
    pub(crate) tree_string: Option<String>,
    pub(crate) is_local: Option<bool>,
    pub(crate) is_streaming: Option<bool>,
    pub(crate) input_files: Option<Vec<String>>,
    pub(crate) spark_version: Option<String>,
    pub(crate) ddl_parse: Option<spark::DataType>,
    pub(crate) same_semantics: Option<bool>,
    pub(crate) semantic_hash: Option<i32>,
    pub(crate) get_storage_level: Option<spark::StorageLevel>,
}

/// Client wrapper to handle submitting requests and handling responses from the [SparkConnectServiceClient]
#[derive(Clone, Debug)]
pub struct SparkConnectClient<T> {
    stub: Arc<RwLock<SparkConnectServiceClient<T>>>,
    builder: ChannelBuilder,
    session_id: String,
    operation_id: Option<String>,
    response_id: Option<String>,
    pub(crate) handler: ResponseHandler,
    pub(crate) analyzer: AnalyzeHandler,
    pub(crate) user_context: Option<spark::UserContext>,
    pub(crate) tags: Vec<String>,
    pub(crate) use_reattachable_execute: bool,
}

impl<T> SparkConnectClient<T>
where
    T: tonic::client::GrpcService<tonic::body::BoxBody>,
    T::Error: Into<StdError>,
    T::ResponseBody: Body<Data = Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error: Into<StdError> + Send,
{
    pub fn new(stub: Arc<RwLock<SparkConnectServiceClient<T>>>, builder: ChannelBuilder) -> Self {
        let user_ref = builder.user_id.clone().unwrap_or("".to_string());
        let session_id = builder.session_id.to_string();

        SparkConnectClient {
            stub,
            builder,
            session_id,
            operation_id: None,
            response_id: None,
            handler: ResponseHandler::default(),
            analyzer: AnalyzeHandler::default(),
            user_context: Some(spark::UserContext {
                user_id: user_ref.clone(),
                user_name: user_ref,
                extensions: vec![],
            }),
            tags: vec![],
            use_reattachable_execute: true,
        }
    }

    /// Session ID
    pub fn session_id(&self) -> String {
        self.session_id.clone()
    }

    /// Change the reattachable execute value
    pub fn set_reattachable_execute(&mut self, setting: bool) -> Result<(), SparkError> {
        self.use_reattachable_execute = setting;
        Ok(())
    }

    fn request_options(&self) -> Vec<spark::execute_plan_request::RequestOption> {
        if self.use_reattachable_execute {
            let reattach_opt = spark::ReattachOptions { reattachable: true };
            let request_opt = spark::execute_plan_request::RequestOption {
                request_option: Some(
                    spark::execute_plan_request::request_option::RequestOption::ReattachOptions(
                        reattach_opt,
                    ),
                ),
            };

            return vec![request_opt];
        };

        vec![]
    }

    pub fn execute_plan_request_with_metadata(&mut self) -> spark::ExecutePlanRequest {
        let operation_id = Uuid::new_v4().to_string();

        self.operation_id = Some(operation_id.clone());

        spark::ExecutePlanRequest {
            session_id: self.session_id(),
            user_context: self.user_context.clone(),
            operation_id: Some(operation_id),
            plan: None,
            client_type: self.builder.user_agent.clone(),
            request_options: self.request_options(),
            tags: self.tags.clone(),
        }
    }

    pub fn analyze_plan_request_with_metadata(&self) -> spark::AnalyzePlanRequest {
        spark::AnalyzePlanRequest {
            session_id: self.session_id(),
            user_context: self.user_context.clone(),
            client_type: self.builder.user_agent.clone(),
            analyze: None,
        }
    }

    pub async fn execute_and_fetch(
        &mut self,
        req: spark::ExecutePlanRequest,
    ) -> Result<(), SparkError> {
        let mut client = self.stub.write().await;

        let mut stream = client.execute_plan(req).await?.into_inner();
        drop(client);

        // clear out any prior responses
        self.handler = ResponseHandler::default();

        self.process_stream(&mut stream).await?;

        if self.use_reattachable_execute && self.handler.result_complete {
            self.release_all().await?
        }

        Ok(())
    }

    async fn reattach_execute(&mut self) -> Result<(), SparkError> {
        let mut client = self.stub.write().await;

        let req = spark::ReattachExecuteRequest {
            session_id: self.session_id(),
            user_context: self.user_context.clone(),
            operation_id: self.operation_id.clone().unwrap(),
            client_type: self.builder.user_agent.clone(),
            last_response_id: self.response_id.clone(),
        };

        let mut stream = client.reattach_execute(req).await?.into_inner();
        drop(client);

        self.process_stream(&mut stream).await?;

        if self.use_reattachable_execute && self.handler.result_complete {
            self.release_all().await?
        }

        Ok(())
    }

    async fn process_stream(
        &mut self,
        stream: &mut Streaming<spark::ExecutePlanResponse>,
    ) -> Result<(), SparkError> {
        while let Some(_resp) = match stream.message().await {
            Ok(Some(msg)) => {
                self.handle_response(msg.clone())?;
                Some(msg)
            }
            Ok(None) => {
                if self.use_reattachable_execute && !self.handler.result_complete {
                    Box::pin(self.reattach_execute()).await?;
                }
                None
            }
            Err(err) => {
                if self.use_reattachable_execute && self.response_id.is_some() {
                    self.release_until().await?;
                }
                return Err(err.into());
            }
        } {}

        Ok(())
    }

    async fn release_until(&mut self) -> Result<(), SparkError> {
        let release_until = spark::release_execute_request::ReleaseUntil {
            response_id: self.response_id.clone().unwrap(),
        };

        self.release_execute(Some(spark::release_execute_request::Release::ReleaseUntil(
            release_until,
        )))
        .await
    }

    async fn release_all(&mut self) -> Result<(), SparkError> {
        let release_all = spark::release_execute_request::ReleaseAll {};

        self.release_execute(Some(spark::release_execute_request::Release::ReleaseAll(
            release_all,
        )))
        .await
    }

    async fn release_execute(
        &mut self,
        release: Option<spark::release_execute_request::Release>,
    ) -> Result<(), SparkError> {
        let mut client = self.stub.write().await;

        let req = spark::ReleaseExecuteRequest {
            session_id: self.session_id(),
            user_context: self.user_context.clone(),
            operation_id: self.operation_id.clone().unwrap(),
            client_type: self.builder.user_agent.clone(),
            release,
        };

        let _resp = client.release_execute(req).await?.into_inner();

        Ok(())
    }

    pub async fn analyze(
        &mut self,
        analyze: spark::analyze_plan_request::Analyze,
    ) -> Result<&mut Self, SparkError> {
        let mut req = self.analyze_plan_request_with_metadata();

        req.analyze = Some(analyze);

        // clear out any prior responses
        self.analyzer = AnalyzeHandler::default();

        let mut client = self.stub.write().await;
        let resp = client.analyze_plan(req).await?.into_inner();
        drop(client);

        self.handle_analyze(resp)
    }

    fn validate_tag(&self, tag: &str) -> Result<(), SparkError> {
        if tag.contains(',') {
            return Err(SparkError::AnalysisException(
                "Spark Connect tag can not contain ',' ".to_string(),
            ));
        };

        if tag.is_empty() {
            return Err(SparkError::AnalysisException(
                "Spark Connect tag can not an empty string ".to_string(),
            ));
        };

        Ok(())
    }

    pub fn add_tag(&mut self, tag: &str) -> Result<(), SparkError> {
        self.validate_tag(tag)?;
        self.tags.push(tag.to_string());
        Ok(())
    }

    pub fn remove_tag(&mut self, tag: &str) -> Result<(), SparkError> {
        self.validate_tag(tag)?;
        self.tags.retain(|t| t != tag);
        Ok(())
    }

    pub fn get_tags(&self) -> &Vec<String> {
        &self.tags
    }

    pub fn clear_tags(&mut self) {
        self.tags = vec![];
    }

    pub async fn config_request(
        &self,
        operation: spark::config_request::Operation,
    ) -> Result<spark::ConfigResponse, SparkError> {
        let operation = spark::ConfigRequest {
            session_id: self.session_id(),
            user_context: self.user_context.clone(),
            client_type: self.builder.user_agent.clone(),
            operation: Some(operation),
        };

        let mut client = self.stub.write().await;

        let resp = client.config(operation).await?.into_inner();

        Ok(resp)
    }

    pub async fn interrupt_request(
        &self,
        interrupt_type: spark::interrupt_request::InterruptType,
        id_or_tag: Option<String>,
    ) -> Result<spark::InterruptResponse, SparkError> {
        let mut req = spark::InterruptRequest {
            session_id: self.session_id(),
            user_context: self.user_context.clone(),
            client_type: self.builder.user_agent.clone(),
            interrupt_type: 0,
            interrupt: None,
        };

        match interrupt_type {
            spark::interrupt_request::InterruptType::All => {
                req.interrupt_type = interrupt_type.into();
            }
            spark::interrupt_request::InterruptType::Tag => {
                let tag = id_or_tag.expect("Tag can not be empty");
                let interrupt = spark::interrupt_request::Interrupt::OperationTag(tag);
                req.interrupt_type = interrupt_type.into();
                req.interrupt = Some(interrupt);
            }
            spark::interrupt_request::InterruptType::OperationId => {
                let op_id = id_or_tag.expect("Operation ID can not be empty");
                let interrupt = spark::interrupt_request::Interrupt::OperationId(op_id);
                req.interrupt_type = interrupt_type.into();
                req.interrupt = Some(interrupt);
            }
            spark::interrupt_request::InterruptType::Unspecified => {
                return Err(SparkError::AnalysisException(
                    "Interrupt Type was not specified".to_string(),
                ))
            }
        };

        let mut client = self.stub.write().await;

        let resp = client.interrupt(req).await?.into_inner();

        Ok(resp)
    }

    fn handle_response(&mut self, resp: spark::ExecutePlanResponse) -> Result<(), SparkError> {
        self.validate_session(&resp.session_id)?;

        self.operation_id = Some(resp.operation_id);
        self.response_id = Some(resp.response_id);

        if let Some(schema) = &resp.schema {
            self.handler.schema = Some(schema.clone());
        }
        if let Some(metrics) = &resp.metrics {
            self.handler.metrics = Some(metrics.clone());
        }
        if let Some(data) = resp.response_type {
            match data {
                ResponseType::ArrowBatch(res) => {
                    self.deserialize(res.data.as_slice(), res.row_count)?
                }
                ResponseType::SqlCommandResult(sql_cmd) => {
                    self.handler.sql_command_result = Some(sql_cmd.clone())
                }
                ResponseType::WriteStreamOperationStartResult(write_stream_op) => {
                    self.handler.write_stream_operation_start_result = Some(write_stream_op)
                }
                ResponseType::StreamingQueryCommandResult(stream_qry_cmd) => {
                    self.handler.streaming_query_command_result = Some(stream_qry_cmd)
                }
                ResponseType::GetResourcesCommandResult(resource_cmd) => {
                    self.handler.get_resources_command_result = Some(resource_cmd)
                }
                ResponseType::StreamingQueryManagerCommandResult(stream_qry_mngr_cmd) => {
                    self.handler.streaming_query_manager_command_result = Some(stream_qry_mngr_cmd)
                }
                ResponseType::ResultComplete(_) => self.handler.result_complete = true,
                ResponseType::Extension(_) => {
                    unimplemented!("extension response types are not implemented")
                }
            }
        }
        Ok(())
    }

    fn handle_analyze(
        &mut self,
        resp: spark::AnalyzePlanResponse,
    ) -> Result<&mut Self, SparkError> {
        self.validate_session(&resp.session_id)?;
        if let Some(result) = resp.result {
            match result {
                spark::analyze_plan_response::Result::Schema(schema) => {
                    self.analyzer.schema = schema.schema
                }
                spark::analyze_plan_response::Result::Explain(explain) => {
                    self.analyzer.explain = Some(explain.explain_string)
                }
                spark::analyze_plan_response::Result::TreeString(tree_string) => {
                    self.analyzer.tree_string = Some(tree_string.tree_string)
                }
                spark::analyze_plan_response::Result::IsLocal(is_local) => {
                    self.analyzer.is_local = Some(is_local.is_local)
                }
                spark::analyze_plan_response::Result::IsStreaming(is_streaming) => {
                    self.analyzer.is_streaming = Some(is_streaming.is_streaming)
                }
                spark::analyze_plan_response::Result::InputFiles(input_files) => {
                    self.analyzer.input_files = Some(input_files.files)
                }
                spark::analyze_plan_response::Result::SparkVersion(spark_version) => {
                    self.analyzer.spark_version = Some(spark_version.version)
                }
                spark::analyze_plan_response::Result::DdlParse(ddl_parse) => {
                    self.analyzer.ddl_parse = ddl_parse.parsed
                }
                spark::analyze_plan_response::Result::SameSemantics(same_semantics) => {
                    self.analyzer.same_semantics = Some(same_semantics.result)
                }
                spark::analyze_plan_response::Result::SemanticHash(semantic_hash) => {
                    self.analyzer.semantic_hash = Some(semantic_hash.result)
                }
                spark::analyze_plan_response::Result::Persist(_) => {}
                spark::analyze_plan_response::Result::Unpersist(_) => {}
                spark::analyze_plan_response::Result::GetStorageLevel(level) => {
                    self.analyzer.get_storage_level = level.storage_level
                }
            }
        }

        Ok(self)
    }

    fn validate_session(&self, session_id: &str) -> Result<(), SparkError> {
        if self.builder.session_id.to_string() != session_id {
            return Err(SparkError::AnalysisException(format!(
                "Received incorrect session identifier for request: {0} != {1}",
                self.builder.session_id, session_id
            )));
        }
        Ok(())
    }

    fn deserialize(&mut self, res: &[u8], row_count: i64) -> Result<(), SparkError> {
        let reader = StreamReader::try_new(res, None)?;
        for batch in reader {
            let record = batch?;
            if record.num_rows() != row_count as usize {
                return Err(SparkError::ArrowError(ArrowError::IpcError(format!(
                    "Expected {} rows in arrow batch but got {}",
                    row_count,
                    record.num_rows()
                ))));
            };
            self.handler.batches.push(record);
            self.handler.total_count += row_count as isize;
        }
        Ok(())
    }

    pub async fn execute_command(&mut self, plan: spark::Plan) -> Result<(), SparkError> {
        let mut req = self.execute_plan_request_with_metadata();

        req.plan = Some(plan);

        self.execute_and_fetch(req).await?;

        Ok(())
    }

    pub(crate) async fn execute_command_and_fetch(
        &mut self,
        plan: spark::Plan,
    ) -> Result<ResponseHandler, SparkError> {
        let mut req = self.execute_plan_request_with_metadata();

        req.plan = Some(plan);

        self.execute_and_fetch(req).await?;

        Ok(self.handler.clone())
    }

    #[allow(clippy::wrong_self_convention)]
    pub async fn to_arrow(&mut self, plan: spark::Plan) -> Result<RecordBatch, SparkError> {
        let mut req = self.execute_plan_request_with_metadata();

        req.plan = Some(plan);

        self.execute_and_fetch(req).await?;

        Ok(concat_batches(
            &self.handler.batches[0].schema(),
            &self.handler.batches,
        )?)
    }

    #[allow(clippy::wrong_self_convention)]
    pub(crate) async fn to_first_value(&mut self, plan: spark::Plan) -> Result<String, SparkError> {
        let rows = self.to_arrow(plan).await?;
        let col = rows.column(0);

        let data: &arrow::array::StringArray = match col.data_type() {
            arrow::datatypes::DataType::Utf8 => col.as_any().downcast_ref().unwrap(),
            _ => unimplemented!("only Utf8 data types are currently handled currently."),
        };

        Ok(data.value(0).to_string())
    }

    pub fn schema(&self) -> Result<spark::DataType, SparkError> {
        self.analyzer
            .schema
            .to_owned()
            .ok_or_else(|| SparkError::AnalysisException("Schema response is empty".to_string()))
    }

    pub fn explain(&self) -> Result<String, SparkError> {
        self.analyzer
            .explain
            .to_owned()
            .ok_or_else(|| SparkError::AnalysisException("Explain response is empty".to_string()))
    }

    pub fn tree_string(&self) -> Result<String, SparkError> {
        self.analyzer.tree_string.to_owned().ok_or_else(|| {
            SparkError::AnalysisException("Tree String response is empty".to_string())
        })
    }

    pub fn is_local(&self) -> Result<bool, SparkError> {
        self.analyzer
            .is_local
            .to_owned()
            .ok_or_else(|| SparkError::AnalysisException("Is Local response is empty".to_string()))
    }

    pub fn is_streaming(&self) -> Result<bool, SparkError> {
        self.analyzer.is_streaming.to_owned().ok_or_else(|| {
            SparkError::AnalysisException("Is Streaming response is empty".to_string())
        })
    }

    pub fn input_files(&self) -> Result<Vec<String>, SparkError> {
        self.analyzer.input_files.to_owned().ok_or_else(|| {
            SparkError::AnalysisException("Input Files response is empty".to_string())
        })
    }

    pub fn spark_version(&mut self) -> Result<String, SparkError> {
        self.analyzer.spark_version.to_owned().ok_or_else(|| {
            SparkError::AnalysisException("Spark Version resonse is empty".to_string())
        })
    }

    pub fn ddl_parse(&self) -> Result<spark::DataType, SparkError> {
        self.analyzer
            .ddl_parse
            .to_owned()
            .ok_or_else(|| SparkError::AnalysisException("DDL parse response is empty".to_string()))
    }

    pub fn same_semantics(&self) -> Result<bool, SparkError> {
        self.analyzer.same_semantics.to_owned().ok_or_else(|| {
            SparkError::AnalysisException("Same Semantics response is empty".to_string())
        })
    }

    pub fn semantic_hash(&self) -> Result<i32, SparkError> {
        self.analyzer.semantic_hash.to_owned().ok_or_else(|| {
            SparkError::AnalysisException("Semantic Hash response is empty".to_string())
        })
    }

    pub fn get_storage_level(&self) -> Result<spark::StorageLevel, SparkError> {
        self.analyzer.get_storage_level.to_owned().ok_or_else(|| {
            SparkError::AnalysisException("Storage Level response is empty".to_string())
        })
    }
}
