//! The main module of this crate.
//! # Example
//! To build a [`Client`](Client) you just need an [`Authenticator`](yup_oauth2::authenticator::Authenticator). For example, if you want to use a service account:
//! ```rust
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // 1. Load the desired secret (here, a service account key)
//!     let sa_key = yup_oauth2::read_service_account_key("clientsecret.json")
//!         .await?;
//!
//!     // 2. Create an Authenticator
//!     let auth = yup_oauth2::ServiceAccountAuthenticator::builder(sa_key)
//!         .build()
//!         .await?;
//!
//!     // 3. Create a Client
//!     let mut client = bigquery_storage::Client::new(auth).await?;
//!
//!     Ok(())
//! }
//! ```
use crate::googleapis::google::cloud::bigquery::storage::v1::{
    big_query_read_client::BigQueryReadClient,
    read_session::{TableModifiers, TableReadOptions},
    CreateReadSessionRequest, DataFormat, ReadRowsRequest, ReadRowsResponse,
    ReadSession as BigQueryReadSession, ReadStream,
};
use crate::BufferedArrowIpcReader;
use crate::Error;
use hyper::client::connect::Connect;
use prost_types::Timestamp;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig};
use tonic::{Request, Streaming};
use yup_oauth2::authenticator::Authenticator;

static API_ENDPOINT: &str = "https://bigquerystorage.googleapis.com";
static API_DOMAIN: &str = "bigquerystorage.googleapis.com";
static API_SCOPE: &str = "https://www.googleapis.com/auth/bigquery";

/// A fully qualified BigQuery table. This requires a `project_id`, a `dataset_id`
/// and a `table_id`. Only alphanumerical and underscores are allowed for `dataset_id`
/// and `table_id`.
pub struct Table {
    project_id: String,
    dataset_id: String,
    table_id: String,
}

impl Table {
    pub fn new(project_id: &str, dataset_id: &str, table_id: &str) -> Self {
        Self {
            project_id: project_id.to_string(),
            dataset_id: dataset_id.to_string(),
            table_id: table_id.to_string(),
        }
    }
}

impl std::fmt::Display for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "projects/{}/datasets/{}/tables/{}",
            self.project_id, self.dataset_id, self.table_id
        )
    }
}

macro_rules! read_session_builder {
    {
        $(
            $(#[$m:meta])*
            $field:ident: $ty:path,
        )*
    } => {
        #[derive(Default)]
        struct ReadSessionBuilderOpts {
            $(
                $field: Option<$ty>,
            )*
        }

        /// A builder for [`ReadSession`](crate::client::ReadSession).
        /// When in doubt about what a field does, please refer to [`CreateReadSessionRequest`](crate::googleapis::CreateReadSessionRequest) and the [official API](https://cloud.google.com/bigquery/docs/reference/storage/rpc/google.cloud.bigquery.storage.v1) documentation.
        pub struct ReadSessionBuilder<T> {
            client: Client<T>,
            table: Table,
            opts: ReadSessionBuilderOpts
        }

        impl<T> ReadSessionBuilder<T> {
            fn new(client: Client<T>, table: Table) -> Self {
                let opts = ReadSessionBuilderOpts::default();
                Self { client, table, opts }
            }

            $(
                $(#[$m])*
                pub fn $field(mut self, $field: $ty) -> Self {
                    self.opts.$field = Some($field);
                    self
                }
            )*
        }
    };
}

read_session_builder! {
    #[doc = "Sets the data format of the output data. Defaults to Arrow if not set."]
    data_format: DataFormat,
    #[doc = "Sets the snapshot time of the table. If not set, interpreted as now."]
    snapshot_time: Timestamp,
    #[doc = "Names of the fields in the table that should be read. If empty or not set, all fields will be read. If the specified field is a nested field, all the sub-fields in the field will be selected. The output field order is unrelated to the order of fields in selected_fields."]
    selected_fields: Vec<String>,
    #[doc = "SQL text filtering statement, similar to a `WHERE` clause in a query. Aggregates are not supported.\n"]
    #[doc = "Examples: \n
- `int_field > 5` \n
- `date_field = CAST('2014-9-27' as DATE)` \n
- `nullable_field is not NULL` \n
- `st_equals(geo_field, st_geofromtext(\"POINT(2, 2)\"))` \n
- `numeric_field BETWEEN 1.0 AND 5.0`"]
    row_restriction: String,
    #[doc = "Max initial number of streams. If unset or zero, the server will provide a value of streams so as to produce reasonable throughput. Must be non-negative. The number of streams may be lower than the requested number, depending on the amount parallelism that is reasonable for the table. Error will be returned if the max count is greater than the current system max limit of 1,000."]
    max_stream_count: i32,
    #[doc = "The request project that owns the session. If not set, defaults to the project owning the table to be read."]
    parent_project_id: String,
}

impl<C> ReadSessionBuilder<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    /// Build the [`ReadSession`](ReadSession). This will hit Google's API and
    /// prepare the desired read streams.
    pub async fn build(mut self) -> Result<ReadSession<C>, Error> {
        let table = self.table.to_string();

        let mut inner = BigQueryReadSession {
            table,
            ..Default::default()
        };

        let data_format = self.opts.data_format.unwrap_or(DataFormat::Arrow);
        inner.set_data_format(data_format);

        if let Some(snapshot_time) = self.opts.snapshot_time {
            inner.table_modifiers = Some(TableModifiers {
                snapshot_time: Some(snapshot_time),
            });
        }

        let mut tro = TableReadOptions::default();
        if let Some(selected_fields) = self.opts.selected_fields {
            tro.selected_fields = selected_fields;
        }

        if let Some(row_restriction) = self.opts.row_restriction {
            tro.row_restriction = row_restriction;
        }
        inner.read_options = Some(tro);

        let parent_project_id = self.opts.parent_project_id.unwrap_or(self.table.project_id);
        let parent = format!("projects/{}", parent_project_id);
        let max_stream_count = self.opts.max_stream_count.unwrap_or_default();

        let req = CreateReadSessionRequest {
            parent,
            read_session: Some(inner),
            max_stream_count,
            preferred_min_stream_count: 1, // TODO: Make this configurable.
        };

        let inner = self.client.create_read_session(req).await?;
        let total_streams = inner.streams.len();

        Ok(ReadSession {
            client: self.client,
            inner,
            total_streams,
        })
    }
}

/// A practical wrapper around a [BigQuery Storage read session](https://cloud.google.com/bigquery/docs/reference/storage#create_a_session).
/// Do not create it manually, use [`Client::read_session_builder`](Client::read_session_builder) instead.
pub struct ReadSession<C> {
    client: Client<C>,
    inner: BigQueryReadSession,
    total_streams: usize,
}

impl<C> ReadSession<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    /// Returns the total number of streams available. Will always return the
    /// same value as the initial length of streams (not equivalent to
    /// `self.inner.streams.len()`).
    pub fn len_streams(&self) -> usize {
        self.total_streams
    }

    /// Take the next stream in this read session. Returns `None` when all streams have been taken.
    pub async fn next_stream(&mut self) -> Result<Option<BufferedArrowIpcReader>, Error> {
        match self.inner.streams.pop() {
            Some(ReadStream { name }) => {
                let rows_stream = self.client.read_stream_rows(&name).await?;
                let schema = self
                    .inner
                    .schema
                    .clone()
                    .ok_or_else(|| Error::invalid("empty schema response"))?;
                Ok(Some(BufferedArrowIpcReader::new(schema, rows_stream)))
            }
            None => Ok(None),
        }
    }
}

/// The main object of this crate.
pub struct Client<C> {
    auth: Authenticator<C>,
    big_query_read_client: BigQueryReadClient<Channel>,
}

impl<C> Client<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    /// Create a new client using `auth` as a token generator.
    pub async fn new(auth: Authenticator<C>) -> Result<Self, Error> {
        let tls_config = ClientTlsConfig::new().domain_name(API_DOMAIN);
        let channel = Channel::from_static(API_ENDPOINT)
            .tls_config(tls_config)?
            .connect()
            .await?;
        let big_query_read_client = BigQueryReadClient::new(channel);
        Ok(Self {
            auth,
            big_query_read_client,
        })
    }

    /// Create a new [`ReadSessionBuilder`](ReadSessionBuilder).
    pub fn read_session_builder(self, table: Table) -> ReadSessionBuilder<C> {
        ReadSessionBuilder::new(self, table)
    }

    async fn new_request<D>(&self, t: D, params: &str) -> Result<Request<D>, Error> {
        let token = self.auth.token(&[API_SCOPE]).await?;
        let bearer_token = format!("Bearer {}", token.as_str());
        let bearer_value: MetadataValue<_> = bearer_token.parse()?;
        let mut req = Request::new(t);
        let meta = req.metadata_mut();
        meta.insert("authorization", bearer_value);
        meta.insert("x-goog-request-params", params.parse()?);
        Ok(req)
    }

    async fn create_read_session(
        &mut self,
        req: CreateReadSessionRequest,
    ) -> Result<BigQueryReadSession, Error> {
        let table_uri = &req.read_session.as_ref().unwrap().table;
        let params = format!("read_session.table={}", table_uri);
        let wrapped = self.new_request(req, &params).await?;

        let read_session = self
            .big_query_read_client
            .create_read_session(wrapped)
            .await?
            .into_inner();
        Ok(read_session)
    }

    async fn read_stream_rows(
        &mut self,
        stream: &str,
    ) -> Result<Streaming<ReadRowsResponse>, Error> {
        let req = ReadRowsRequest {
            read_stream: stream.to_string(),
            offset: 0, // TODO
        };
        let params = format!("read_stream={}", req.read_stream);
        let wrapped = self.new_request(req, &params).await?;
        let read_rows_response = self
            .big_query_read_client
            .read_rows(wrapped)
            .await?
            .into_inner();
        Ok(read_rows_response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::ipc::reader::StreamReader as ArrowStreamReader;
    use std::io::Cursor;
    use yup_oauth2::authenticator::{DefaultHyperClient, HyperClientBuilder};

    fn test_project() -> String {
        std::env::var("GCP_PROJECT_ID").unwrap()
    }

    type DefaultConnector = <DefaultHyperClient as HyperClientBuilder>::Connector;

    async fn create_test_client() -> Client<DefaultConnector> {
        let sa_key = yup_oauth2::read_service_account_key("clientsecret.json")
            .await
            .unwrap();
        let auth = yup_oauth2::ServiceAccountAuthenticator::builder(sa_key)
            .build()
            .await
            .unwrap();

        Client::new(auth).await.unwrap()
    }

    // bigquery-public-data.austin_bikeshare.bikeshare_stations
    //
    // station_id        INTEGER   NULLABLE
    // name              STRING    NULLABLE
    // status            STRING    NULLABLE
    // address           STRING    NULLABLE
    // alternate_name    STRING    NULLABLE
    // city_asset_number INTEGER   NULLABLE
    // property_type     STRING    NULLABLE
    // number_of_docks   INTEGER   NULLABLE
    // power_type        STRING    NULLABLE
    // footprint_length  INTEGER   NULLABLE
    // footprint_width   FLOAT     NULLABLE
    // notes             STRING    NULLABLE
    // council_district  INTEGER   NULLABLE
    // modified_date     TIMESTAMP NULLABLE

    #[tokio::test]
    async fn read_a_table_with_arrow() {
        let test_table = Table::new(
            "bigquery-public-data",
            "austin_bikeshare",
            "bikeshare_stations",
        );

        let client = create_test_client().await;

        let mut read_session = client
            .read_session_builder(test_table)
            .parent_project_id(test_project())
            .build()
            .await
            .unwrap();

        let mut num_rows = 0;

        while let Some(stream_reader) = read_session.next_stream().await.unwrap() {
            let buf = stream_reader.into_vec().await.unwrap();
            let mut reader = ArrowStreamReader::try_new(Cursor::new(buf), None).unwrap();
            while let Some(record_batch) = reader.next() {
                num_rows += record_batch.unwrap().num_rows();
            }
        }

        assert_eq!(num_rows, 102);
    }

    #[tokio::test]
    async fn projection() {
        let test_table = Table::new(
            "bigquery-public-data",
            "austin_bikeshare",
            "bikeshare_stations",
        );

        let client = create_test_client().await;

        let mut read_session = client
            .read_session_builder(test_table)
            .parent_project_id(test_project())
            .selected_fields(vec!["address".to_string(), "power_type".to_string()])
            .build()
            .await
            .unwrap();

        let mut at_least_one = false;
        while let Some(stream) = read_session.next_stream().await.unwrap() {
            let buf = stream.into_vec().await.unwrap();
            let reader = ArrowStreamReader::try_new(Cursor::new(buf), None).unwrap();

            let schema = reader.schema();
            let schema_names: Vec<_> = schema.fields.iter().map(|f| f.name().as_str()).collect();
            assert_eq!(vec!["address", "power_type"], schema_names);
            at_least_one = true;
        }

        assert!(at_least_one);
    }
}
