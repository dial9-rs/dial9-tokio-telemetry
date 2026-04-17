use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use dial9_trace_format::types::FieldValueRef;
use dial9_tracing::Dial9TokioLayer;
use std::time::Duration;
use tracing_subscriber::prelude::*;

/// Verify that span enter/exit events appear in the trace with correct names,
/// fields, and parent span IDs.
#[test]
fn span_events_appear_in_trace() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let writer = RotatingWriter::single_file(&trace_path).unwrap();
    let (runtime, guard) = TracedRuntime::build_and_start(builder, writer).unwrap();

    // Install the tracing subscriber with our layer.
    // Must be global so worker threads see it.
    let subscriber = tracing_subscriber::registry().with(Dial9TokioLayer::new());
    tracing::subscriber::set_global_default(subscriber).expect("failed to set global subscriber");

    runtime.block_on(async {
        #[tracing::instrument(fields(user_id = 42))]
        async fn handle_request() {
            inner_op("redis").await;
            inner_op("redis").await;
        }

        #[tracing::instrument]
        async fn inner_op(backend: &str) {
            tokio::task::yield_now().await;
        }

        for _ in 0..3 {
            tokio::spawn(handle_request()).await.unwrap();
        }

        // Wait for flush cycle
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(runtime);
    drop(guard);

    // Decode the trace and find span events
    let sealed_path = dir.path().join("trace.0.bin");
    let data = std::fs::read(&sealed_path).unwrap();
    let mut decoder = dial9_trace_format::decoder::Decoder::new(&data).unwrap();

    let mut enter_count = 0u32;
    let mut exit_count = 0u32;
    let mut enter_names: Vec<String> = Vec::new();
    let mut saw_user_id_field = false;
    let mut saw_parent_span_id = false;

    decoder
        .for_each_event(|ev| match ev.name {
            "SpanEnterEvent" => {
                enter_count += 1;
                for (field_def, field_val) in ev.schema.fields.iter().zip(ev.fields.iter()) {
                    if field_def.name == "span_name"
                        && let FieldValueRef::PooledString(id) = field_val
                        && let Some(name) = ev.string_pool.get(*id)
                    {
                        enter_names.push(name.to_owned());
                    }
                    if field_def.name == "fields"
                        && let FieldValueRef::StringMap(map_ref) = field_val
                    {
                        for (k, _v) in map_ref.iter() {
                            if k == "user_id" {
                                saw_user_id_field = true;
                            }
                        }
                    }
                    if field_def.name == "parent_span_id"
                        && let FieldValueRef::Varint(v) = field_val
                        && *v > 0
                    {
                        saw_parent_span_id = true;
                    }
                }
            }
            "SpanExitEvent" => {
                exit_count += 1;
            }
            _ => {}
        })
        .unwrap();

    // 3 iterations x (1 handle_request + 2 inner_op) = 9 spans
    // inner_op yields, so it gets entered twice per call (2 polls)
    // handle_request also gets re-entered across the inner yields
    assert!(
        enter_count >= 9,
        "expected at least 9 span enters, got {enter_count}"
    );
    assert_eq!(enter_count, exit_count, "enter/exit count mismatch");

    assert!(
        enter_names.contains(&"handle_request".to_string()),
        "expected handle_request span, got: {enter_names:?}"
    );
    assert!(
        enter_names.contains(&"inner_op".to_string()),
        "expected inner_op span, got: {enter_names:?}"
    );

    assert!(
        saw_user_id_field,
        "expected user_id field on handle_request span"
    );
    assert!(
        saw_parent_span_id,
        "expected parent_span_id on inner_op spans (child of handle_request)"
    );
}
