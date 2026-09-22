use criterion::{black_box, criterion_group, criterion_main, Criterion};
use syscity::providers::{Message, Role, ToolCall};

fn bench_message_serialization(c: &mut Criterion) {
    let msg = Message::assistant(
        "This is a moderately long assistant response that contains some reasoning about the \
         problem at hand.",
    )
    .with_tool_calls(vec![ToolCall {
        id: "call_abc123".to_string(),
        call_type: "function".to_string(),
        function: syscity::providers::FunctionCall {
            name: "shell".to_string(),
            arguments: "{\"command\":\"ls -la\"}".to_string(),
        },
        index: None,
        result: None,
    }]);

    c.bench_function("message_serialize", |b| {
        b.iter(|| {
            let _ = serde_json::to_string(black_box(&msg)).unwrap();
        })
    });
}

fn bench_message_deserialization(c: &mut Criterion) {
    let json = r#"{"role":"assistant","content":"Hello!","tool_calls":[{"id":"call_1","call_type":"function","function":{"name":"shell","arguments":"{}"}}]}"#;

    c.bench_function("message_deserialize", |b| {
        b.iter(|| {
            let _: Message = serde_json::from_str(black_box(json)).unwrap();
        })
    });
}

fn bench_batch_message_serialization(c: &mut Criterion) {
    let messages: Vec<Message> = (0..100)
        .map(|i| match i % 4 {
            0 => Message::system("You are helpful."),
            1 => Message::user(format!("Question {}", i)),
            2 => Message::assistant(format!("Answer {}", i)),
            _ => Message::tool("output", "call_123"),
        })
        .collect();

    c.bench_function("batch_message_serialize_100", |b| {
        b.iter(|| {
            let _ = serde_json::to_string(black_box(&messages)).unwrap();
        })
    });
}

fn bench_role_roundtrip(c: &mut Criterion) {
    let roles = vec![Role::System, Role::User, Role::Assistant, Role::Tool];

    c.bench_function("role_roundtrip", |b| {
        b.iter(|| {
            for role in &roles {
                let json = serde_json::to_string(black_box(role)).unwrap();
                let _: Role = serde_json::from_str(&json).unwrap();
            }
        })
    });
}

criterion_group!(
    serialization_benches,
    bench_message_serialization,
    bench_message_deserialization,
    bench_batch_message_serialization,
    bench_role_roundtrip,
);
criterion_main!(serialization_benches);
