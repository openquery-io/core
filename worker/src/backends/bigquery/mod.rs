use super::{Backend, Probe};
use crate::common::*;

use yup_oauth2::GetToken;

use google_bigquery2::{
    GetQueryResultsResponse, Job as BigQueryJob, TableListTables, TableReference,
};

use bigquery_storage::client::{Client as BigQueryStorageClient, Error as ClientError};
use bigquery_storage::proto::bigquery_storage::read_rows_response::Rows;
use bigquery_storage::proto::bigquery_storage::read_session::Schema as ReadSchema;
use bigquery_storage::proto::bigquery_storage::{ReadRowsRequest, ReadRowsResponse};

use derive_more::From;

use crate::gcp::errors::GcpError;
use crate::gcp::{
    bigquery::{BigQuery as BigQueryClient, JobBuilder, TableRef},
    oauth::Connector,
    Client,
};
use crate::Result;

use crate::opt::expr::Expr;
use crate::opt::expr::ExprTree;
use crate::opt::{
    plan::Step, rel::*, CompositionError, Context, ContextError, ContextKey, DataType, Domain,
    ExprAnsatz, ExprMeta, ExprT, HashAlgorithm, Mode, RelAnsatz, ToAnsatz, ValidateError,
    ValidateResult,
};
use sqlparser::ast::Value::SingleQuotedString;

mod probe;

use probe::BigQueryProbe;
use sqlparser::ast;

pub struct ExprExtraMeta {
    min: String,
    max: String,
}

pub struct BigQuery<O = ()> {
    dataset: DatasetId,
    staging: DatasetId,
    big_query: BigQueryClient<O>,
    storage: BigQueryStorage<O>,
}

#[derive(Clone)]
pub struct DatasetId {
    project_id: String,
    dataset_id: String,
}

impl DatasetId {
    fn into_ident(self) -> Vec<String> {
        vec![self.project_id, self.dataset_id]
    }
}

impl BigQuery<()> {
    pub fn new(resource: BigQueryBackend) -> Result<BigQuery<impl GetToken>> {
        let connector = Connector::builder()
            .with_key_file(Path::new(&resource.service_account_key))
            .set_allow_from_metadata(false)
            .build()
            .map_err(|e| BackendError::from(e))?;

        let gcp_client = Client::new(connector);

        let big_query =
            BigQueryClient::new_with_credentials(&resource.staging_project_id, gcp_client);

        let storage_connector = Connector::builder()
            .with_key_file(Path::new(&resource.service_account_key))
            .set_allow_from_metadata(false)
            .build()
            .map_err(|e| BackendError::from(e))?;

        let storage = BigQueryStorage::new(storage_connector);

        let dataset = DatasetId {
            project_id: resource.project_id.clone(),
            dataset_id: resource.dataset_id.clone(),
        };

        let staging = DatasetId {
            project_id: if resource.staging_project_id.is_empty() {
                resource.project_id
            } else {
                resource.staging_project_id
            },
            dataset_id: if resource.staging_dataset_id.is_empty() {
                resource.dataset_id
            } else {
                resource.staging_dataset_id
            },
        };

        let exec = BigQuery {
            dataset,
            staging,
            big_query,
            storage,
        };

        Ok(exec)
    }
}

impl<O> BigQuery<O>
where
    O: GetToken + Send + Sync + 'static,
{
    pub(self) fn to_inner(&self) -> &BigQueryClient<O> {
        &self.big_query
    }

    fn in_context(&self, key: &ContextKey) -> Result<TableRef> {
        let res = TableRef::new(
            &self.dataset.project_id,
            &self.dataset.dataset_id,
            key.name(),
        );
        res.map_err(|e| e.into())
    }

    fn in_staging(&self, key: &ContextKey) -> Result<TableRef> {
        let res = TableRef::new(
            &self.staging.project_id,
            &self.staging.dataset_id,
            key.name(),
        );
        res.map_err(|e| e.into())
    }

    #[deprecated(note = "Visible for testing")]
    pub async fn get_context_from_backend(
        &self,
        project_id: &str,
        dataset_id: &str,
    ) -> Result<Context<TableMeta>> {
        let tables = self
            .to_inner()
            .list_tables(project_id, dataset_id)
            .await?
            .tables
            .ok_or(Error::new("expected a tables list"))?;

        let mut ctx = Context::new();

        for table in tables {
            let table_id = match table {
                TableListTables {
                    table_reference:
                        Some(TableReference {
                            table_id: Some(table_id),
                            ..
                        }),
                    ..
                } => table_id,
                _ => return Err(Error::new("expected a table_id")),
            };

            let context_key = ContextKey::with_name(&table_id)
                .and_prefix(dataset_id)
                .and_prefix(project_id);
            let table_meta = self.probe(&context_key).await?.to_meta().await?;
            ctx.insert(context_key, table_meta);
        }

        Ok(ctx)
    }

    async fn run_query(&self, query_str: &str) -> Result<BigQueryJob> {
        self.to_inner()
            .run_query(
                &self.staging.project_id,
                &self.staging.dataset_id,
                query_str,
            )
            .await
            .map_err(|e| e.into())
    }

    async fn run_query_and_get_results(&self, query_str: &str) -> Result<GetQueryResultsResponse> {
        self.to_inner()
            .run_query_and_get_results(
                &self.staging.project_id,
                &self.staging.dataset_id,
                query_str,
            )
            .await
            .map_err(|e| e.into())
    }

    async fn lite_query(&self, query_str: &str) -> Result<GetQueryResultsResponse> {
        self.to_inner()
            .lite_query(query_str, &self.staging.project_id)
            .await
            .map_err(|e| e.into())
    }

    pub(self) fn job_builder(&self) -> JobBuilder {
        let mut builder = JobBuilder::default();
        builder.project_id(&self.staging.project_id);
        builder
    }
}

#[async_trait]
impl<O> Backend for BigQuery<O>
where
    O: GetToken + Send + Sync + 'static,
{
    async fn compute(&self, step: Step) -> Result<()> {
        // take RelT into a SQL string and execute against BQ
        let ctx = step
            .ctx
            .into_iter()
            .map(|(ck, meta)| {
                let in_source = meta
                    .source
                    .ok_or(Error::new("a table had no associated context_key"))?;
                let table_ref = self.in_context(&in_source)?;
                Ok((ck, table_ref))
            })
            .collect::<Result<_>>()?;

        let rel_t = BigQueryRelT::wrap(step.rel_t, &ctx);
        let query: sqlparser::ast::Query = rel_t
            .to_ansatz()
            .map_err(|compositon_err| BackendError {
                kind: BackendErrorKind::Unknown as i32,
                source: "BigQuery".to_string(),
                description: compositon_err.to_string(),
            })?
            .into();
        let query_str = query.to_string();

        let output = self.in_staging(&step.promise)?;

        let mut builder = JobBuilder::default();
        builder
            .project_id(&self.staging.project_id.clone())
            .query(&query_str, output);
        let job_request = builder.build()?;
        self.big_query.run_to_completion(job_request).await?;

        Ok(())
    }

    async fn probe<'a>(&'a self, key: &'a ContextKey) -> Result<Box<dyn Probe + 'a>> {
        let table_ref = self.in_context(key)?;
        let probe = BigQueryProbe::new(self, table_ref).await?;
        Ok(Box::new(probe))
    }

    async fn get_schema(&self, ctx_key: &ContextKey) -> Result<ArrowSchema> {
        let table_ref = self.in_staging(ctx_key)?;
        self.storage
            .get_schema(&table_ref)
            .await
            .map_err(|e| e.into())
    }

    async fn get_records(&self, ctx_key: &ContextKey) -> Result<ContentStream<ArrowRecordBatch>> {
        let table_ref = self.in_staging(ctx_key)?;
        self.storage
            .stream_rows(&table_ref)
            .await
            .map_err(|e| e.into())
    }
}

impl From<GcpError> for Error {
    fn from(gcpe: GcpError) -> Self {
        Error {
            reason: format!("GcpError: {}", gcpe.to_string()),
            ..Default::default()
        }
    }
}

impl From<ContextError> for Error {
    fn from(ce: ContextError) -> Self {
        Error {
            reason: format!("ContextError: {}", ce.to_string()),
            ..Default::default()
        }
    }
}

fn table_ref_to_context_key(table_ref: &TableRef) -> ContextKey {
    let (project_id, dataset_id, table_id) = table_ref.unwrap();
    ContextKey::with_name(table_id)
        .and_prefix(dataset_id)
        .and_prefix(project_id)
}

pub struct BigQueryRelT<'a> {
    root: RelT,
    ctx: &'a Context<TableRef>,
}

impl<'a> BigQueryRelT<'a> {
    fn wrap(root: RelT, ctx: &'a Context<TableRef>) -> Self {
        Self { ctx, root }
    }

    fn to_ansatz(self) -> std::result::Result<RelAnsatz, String> {
        let ctx = self.ctx;
        // Hotfix (FIXME): will not work if self.root is sole leaf
        self.root.try_fold(&mut |child| match child {
            GenericRel::Table(Table(key)) => {
                let table_ref = ctx
                    .get(&key)
                    .map_err(|e| e.into_table_error().to_string())?;
                let bq_key = table_ref_to_context_key(&table_ref);
                Ok(GenericRel::<ExprT, RelAnsatz>::Table(Table(bq_key))
                    .to_ansatz()
                    .unwrap())
            }
            child => child
                .map_expressions(&|expr_t| BigQueryExprT::wrap(expr_t.clone()))
                .to_ansatz()
                .map_err(|e| e.to_string()),
        })
    }
}

pub struct BigQueryExprT {
    root: ExprT,
}

impl BigQueryExprT {
    fn expr_ansatz(node: Expr<ExprAnsatz>) -> std::result::Result<ExprAnsatz, CompositionError> {
        match node {
            Expr::Hash(crate::opt::expr::Hash { algo, expr, salt }) => {
                let salt_literal = ast::Expr::Value(SingleQuotedString(base64::encode(&salt)));

                let ast_expr = expr.into();

                let concat = ast::Expr::Function(ast::Function {
                    name: ast::ObjectName(vec!["CONCAT".to_string()]),
                    args: vec![salt_literal, ast_expr],
                    over: None,
                    distinct: false,
                });

                let bq_algo = match algo {
                    HashAlgorithm::SHA256 => "SHA256".to_string(),
                };

                let hash = ast::Expr::Function(ast::Function {
                    name: ast::ObjectName(vec![bq_algo]),
                    args: vec![concat],
                    over: None,
                    distinct: false,
                });

                let base_64_wrapper = ast::Expr::Function(ast::Function {
                    name: ast::ObjectName(vec!["TO_BASE64".to_string()]),
                    args: vec![hash],
                    over: None,
                    distinct: false,
                });
                Ok(ExprAnsatz::Expr(base_64_wrapper))
            }
            _ => node.to_ansatz(),
        }
    }
}

impl ToAnsatz for BigQueryExprT {
    type Ansatz = ExprAnsatz;

    fn to_ansatz(self) -> std::result::Result<Self::Ansatz, CompositionError> {
        self.root.try_fold(&mut |root| Self::expr_ansatz(root))
    }
}
impl BigQueryExprT {
    fn wrap(root: ExprT) -> Self {
        Self { root }
    }
}

#[derive(From, Debug)]
pub enum BigQueryStorageError {
    BigQueryStorage(ClientError),
    MissingSchema,
    #[from(ignore)]
    MalformedData(String),
}

impl Into<Error> for BigQueryStorageError {
    fn into(self) -> Error {
        Error {
            reason: format!("{:?}", self),
            ..Default::default()
        }
    }
}

pub struct BigQueryStorage<T>(BigQueryStorageClient<T>);

impl<T> BigQueryStorage<T>
where
    T: GetToken + Send + Sync + 'static,
{
    pub fn new(connector: T) -> Self {
        let client = BigQueryStorageClient::new(connector);
        Self(client)
    }

    pub async fn get_schema(
        &self,
        table_ref: &TableRef,
    ) -> std::result::Result<ArrowSchema, BigQueryStorageError> {
        let (project_id, dataset_id, table_id) = table_ref.unwrap();

        let read_session = self
            .0
            .create_read_session(project_id, dataset_id, table_id)
            .await?;

        let schema = read_session
            .schema
            .ok_or(BigQueryStorageError::MissingSchema)?;

        match schema {
            ReadSchema::ArrowSchema(arrow_schema) => Ok(ArrowSchema {
                serialized_schema: arrow_schema.serialized_schema,
            }),
            _ => Err(BigQueryStorageError::MalformedData(
                "Was expecting Arrow Schema, got Avro.".to_string(),
            )),
        }
    }

    pub async fn stream_rows(
        &self,
        table_ref: &TableRef,
    ) -> std::result::Result<ContentStream<ArrowRecordBatch>, BigQueryStorageError> {
        let (project_id, dataset_id, table_id) = table_ref.unwrap();
        let read_session = self
            .0
            .create_read_session(project_id, dataset_id, table_id)
            .await?;

        let mut streams = Vec::<tonic::Streaming<ReadRowsResponse>>::new();
        for stream in read_session.streams.iter() {
            let read_row_request = ReadRowsRequest {
                read_stream: stream.name.clone(),
                offset: 0,
            };
            let mut row_response_stream = self.0.read_rows(read_row_request.clone()).await.unwrap();
            streams.push(row_response_stream);
        }

        // Retrieves rows from streams
        // TODO parallelize
        let stream = async_stream::try_stream! {
            for mut row_response_stream in streams {
                while let Ok(Some(row_response)) = row_response_stream.message().await {
                    match row_response.rows.unwrap() {
                        Rows::ArrowRecordBatch(arrow_record_batch) => {

                            let serialized_record_batch = arrow_record_batch.serialized_record_batch;
                            let row_count = arrow_record_batch.row_count;

                            yield ArrowRecordBatch {
                                serialized_record_batch,
                                row_count
                            }

                        }
                        _ => { unimplemented!("Todo") }
                    }
                }
            }
        };

        Ok(Box::pin(stream) as ContentStream<ArrowRecordBatch>)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::gcp::Client;

    use tokio::runtime::Runtime;

    const PROJECT_ID: &'static str = "openquery-dev";
    const TEST_DATASET_ID: &'static str = "yelp";
    const STAGING_DATASET_ID: &'static str = "cache";

    const BIGQUERY_SCOPE_PREFIX: &'static str = "parallax_bigquery_testing";

    pub fn mk_context() -> Context<TableMeta> {
        let big_query = mk_big_query();
        let mut rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(async {
            big_query
                .get_context_from_backend(PROJECT_ID, TEST_DATASET_ID)
                .await
                .unwrap()
        });
        out
    }

    pub fn mk_big_query() -> BigQuery<impl GetToken> {
        let gcp_connector = crate::gcp::tests::mk_connector();
        let gcp_client = Client::new(gcp_connector);
        let client = BigQueryClient::new_with_credentials(PROJECT_ID, gcp_client);

        let storage_connector = crate::gcp::tests::mk_connector();
        let storage_client = BigQueryStorage::new(storage_connector);
        BigQuery {
            dataset: DatasetId {
                project_id: PROJECT_ID.to_string(),
                dataset_id: TEST_DATASET_ID.to_string(),
            },
            staging: DatasetId {
                project_id: PROJECT_ID.to_string(),
                dataset_id: STAGING_DATASET_ID.to_string(),
            },
            big_query: client,
            storage: storage_client,
        }
    }

    #[test]
    fn bigquery_meta_with_domain() {
        let client = mk_big_query();
        Runtime::new().unwrap().block_on(async {
            let test_table = ContextKey::with_name("business").and_prefix("yelp");
            let probe = client.probe(&test_table).await.unwrap();
            let test_key = ContextKey::with_name("review_count");
            let domain = probe.domain(&test_key).await.unwrap();
            assert_eq!(
                domain,
                Domain::Discrete {
                    max: 8348,
                    min: 3,
                    step: 1,
                }
            );
        });
    }
}
