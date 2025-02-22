fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)
        .include_file("googleapis.rs")
        .compile(
            &[
                "googleapis/google/cloud/bigquery/storage/v1/arrow.proto",
                "googleapis/google/cloud/bigquery/storage/v1/avro.proto",
                "googleapis/google/cloud/bigquery/storage/v1/storage.proto",
                "googleapis/google/cloud/bigquery/storage/v1/stream.proto",
            ],
            &["googleapis"],
        )?;
    Ok(())
}
