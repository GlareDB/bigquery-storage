use crate::googleapis::google::cloud::bigquery::storage::v1::{
    read_rows_response::Rows, read_session::Schema, ArrowRecordBatch, ArrowSchema, ReadRowsResponse,
};
use crate::Error;
use futures::future::ready;
use futures::stream::{StreamExt, TryStreamExt};
use std::io::Cursor;
use tonic::Streaming;

#[cfg(feature = "arrow")]
use arrow::ipc::reader::StreamReader as ArrowStreamReader;

#[cfg(feature = "arrow")]
pub type DefaultArrowStreamReader = ArrowStreamReader<Cursor<Vec<u8>>>;

/// A wrapper around a [BigQuery Storage stream](https://cloud.google.com/bigquery/docs/reference/storage#read_from_a_session_stream).
pub struct RowsStreamReader {
    schema: Schema,
    upstream: Streaming<ReadRowsResponse>,
}

impl RowsStreamReader {
    pub(crate) fn new(schema: Schema, upstream: Streaming<ReadRowsResponse>) -> Self {
        Self { schema, upstream }
    }

    /// Consume the entire stream into an Arrow [StreamReader](arrow::ipc::reader::StreamReader).
    #[cfg(feature = "arrow")]
    pub async fn into_arrow_reader(self) -> Result<DefaultArrowStreamReader, Error> {
        let mut serialized_arrow_stream = self
            .upstream
            .map_err(|e| e.into())
            .and_then(|resp| {
                let ReadRowsResponse { rows, .. } = resp;
                let out = rows
                    .ok_or_else(|| Error::invalid("no rows received"))
                    .and_then(|rows| match rows {
                        Rows::ArrowRecordBatch(ArrowRecordBatch {
                            serialized_record_batch,
                            ..
                        }) => Ok(serialized_record_batch),
                        _ => {
                            let err = Error::invalid("expected arrow record batch");
                            Err(err)
                        }
                    });
                ready(out)
            })
            .boxed();

        let serialized_schema = match self.schema {
            Schema::ArrowSchema(ArrowSchema { serialized_schema }) => serialized_schema,
            _ => return Err(Error::invalid("expected arrow schema")),
        };

        let mut buf = Vec::new();
        buf.extend(serialized_schema.as_slice());

        while let Some(msg) = serialized_arrow_stream.next().await {
            let msg = msg?;
            buf.extend(msg.as_slice());
        }

        // Arrow StreamReader expects a zero message to signal the end
        // of the stream. Gotta give the people what they want.
        buf.extend([0u8; 4]);

        let reader = ArrowStreamReader::try_new(Cursor::new(buf), None)?;

        Ok(reader)
    }
}
