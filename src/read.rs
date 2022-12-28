use crate::googleapis::google::cloud::bigquery::storage::v1::{
    read_rows_response::Rows, read_session::Schema, ArrowRecordBatch, ArrowSchema, ReadRowsResponse,
};
use crate::Error;
use futures::future::ready;
use futures::stream::{StreamExt, TryStreamExt};
use tonic::Streaming;

/// End of stream marker for Arrow streams.
const EOS_MARKER: [u8; 8] = [255, 255, 255, 255, 0, 0, 0, 0];

/// Wrapper around a 'read rows response' to buffer an Arrow IPC stream in
/// memory.
pub struct BufferedArrowIpcReader {
    schema: Schema,
    stream: Streaming<ReadRowsResponse>,
}

impl BufferedArrowIpcReader {
    pub(crate) fn new(schema: Schema, stream: Streaming<ReadRowsResponse>) -> Self {
        Self { schema, stream }
    }

    pub async fn into_vec(self) -> Result<Vec<u8>, Error> {
        let mut serialized_arrow_stream = self
            .stream
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

        // Signal end of stream.
        buf.extend(EOS_MARKER);

        Ok(buf)
    }
}
