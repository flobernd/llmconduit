use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use futures::StreamExt;
use futures::stream;
use pretty_assertions::assert_eq;
use resp2chat::config::Config;
use resp2chat::engine::Gateway;
use resp2chat::models::chat::ChatChunkChoice;
use resp2chat::models::chat::ChatCompletionChunk;
use resp2chat::models::chat::ChatCompletionRequest;
use resp2chat::models::chat::ChatCompletionTokensDetails;
use resp2chat::models::chat::ChatCompletionUsage;
use resp2chat::models::chat::ChatDelta;
use resp2chat::models::chat::ChatFunctionCall;
use resp2chat::models::chat::ChatPromptTokensDetails;
use resp2chat::models::chat::ChatToolCall;
use resp2chat::models::responses::ContentItem;
use resp2chat::models::responses::ResponseItem;
use resp2chat::models::responses::ResponsesRequest;
use resp2chat::models::responses::ToolSpec;
use resp2chat::monitor::MonitorHub;
use resp2chat::replay::ReplayStore;
use resp2chat::search::SearchClient;
use resp2chat::upstream::UpstreamClient;
use resp2chat::upstream::UpstreamStream;
use serde_json::Map as JsonMap;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower::ServiceExt;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[derive(Clone, Default)]
struct MockUpstream {
    requests: Arc<Mutex<Vec<ChatCompletionRequest>>>,
    responses: Arc<Mutex<VecDeque<Vec<Result<ChatCompletionChunk, resp2chat::error::AppError>>>>>,
}

impl MockUpstream {
    async fn push_response(
        &self,
        chunks: Vec<Result<ChatCompletionChunk, resp2chat::error::AppError>>,
    ) {
        self.responses.lock().await.push_back(chunks);
    }

    async fn requests(&self) -> Vec<ChatCompletionRequest> {
        self.requests.lock().await.clone()
    }
}

#[async_trait]
impl UpstreamClient for MockUpstream {
    async fn stream_chat_completion(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<UpstreamStream, resp2chat::error::AppError> {
        self.requests.lock().await.push(request.clone());
        let chunks = self
            .responses
            .lock()
            .await
            .pop_front()
            .expect("queued upstream response");
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn list_models(&self) -> Result<reqwest::Response, resp2chat::error::AppError> {
        Err(resp2chat::error::AppError::internal("unused in this test"))
    }
}

#[derive(Clone, Default)]
struct MockSearch {
    queries: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl SearchClient for MockSearch {
    async fn search(&self, query: &str) -> Result<String, resp2chat::error::AppError> {
        self.queries.lock().await.push(query.to_string());
        Ok(format!("Search result for {query}"))
    }
}

#[tokio::test]
async fn streams_function_call_turn() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(tool_call_chunk(
            "chat-1",
            "call_fn_1",
            "echo",
            "{\"value\":\"hi\"}",
        ))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let request = ResponsesRequest {
        model: "glm-5.1".to_string(),
        instructions: String::new(),
        input: vec![user_message("hello")],
        tools: vec![ToolSpec::Function {
            name: "echo".to_string(),
            description: "Echo back a value".to_string(),
            strict: false,
            parameters: json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"]
            }),
        }],
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: None,
        store: false,
        stream: true,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        previous_response_id: None,
    };

    let events = collect_stream(gateway.stream_responses(request).await.expect("stream"));
    let events = events.await;

    assert_eq!(
        event_names(&events),
        vec![
            "response.created",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(events[1]["item"]["type"].as_str(), Some("function_call"));
    assert_eq!(events[1]["item"]["name"].as_str(), Some("echo"));

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].parallel_tool_calls, false);
    assert_eq!(requests[0].tools.as_ref().map(Vec::len), Some(1));
}

#[tokio::test]
async fn uses_configured_upstream_model_override() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![
            Ok(content_chunk("chat-1", "hello")),
            Ok(usage_chunk("chat-1", 12, 5, 17, Some(3), Some(2))),
        ])
        .await;
    let gateway = test_gateway_with_config(
        upstream.clone(),
        MockSearch::default(),
        Config {
            bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
            upstream_base_url: "http://127.0.0.1:8000/v1".parse().expect("url"),
            upstream_api_key: None,
            upstream_model: Some("grok-4".to_string()),
            upstream_chat_kwargs: JsonMap::new(),
            model_profiles: std::collections::BTreeMap::new(),
            brave_base_url: "https://example.com/".parse().expect("url"),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout: std::time::Duration::from_secs(30),
        },
    );

    let _ = collect_stream(
        gateway
            .stream_responses(base_request(vec![user_message("hello")]))
            .await
            .expect("stream"),
    )
    .await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "grok-4");
    assert_eq!(
        requests[0].extra_body.get("stream_options"),
        Some(&json!({ "include_usage": true }))
    );
}

#[tokio::test]
async fn returns_final_usage_on_response_completed() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![
            Ok(content_chunk("chat-1", "hello")),
            Ok(usage_chunk("chat-1", 12, 5, 17, Some(3), Some(2))),
        ])
        .await;
    let gateway = test_gateway(upstream, MockSearch::default());

    let events = collect_stream(
        gateway
            .stream_responses(base_request(vec![user_message("hello")]))
            .await
            .expect("stream"),
    )
    .await;

    assert_eq!(
        events.last().and_then(|event| event["_event"].as_str()),
        Some("response.completed")
    );
    assert_eq!(
        events
            .last()
            .and_then(|event| event["response"]["usage"]["input_tokens"].as_u64()),
        Some(12)
    );
    assert_eq!(
        events.last().and_then(|event| {
            event["response"]["usage"]["input_tokens_details"]["cached_tokens"].as_u64()
        }),
        Some(3)
    );
    assert_eq!(
        events
            .last()
            .and_then(|event| event["response"]["usage"]["output_tokens"].as_u64()),
        Some(5)
    );
    assert_eq!(
        events.last().and_then(|event| {
            event["response"]["usage"]["output_tokens_details"]["reasoning_tokens"].as_u64()
        }),
        Some(2)
    );
    assert_eq!(
        events
            .last()
            .and_then(|event| event["response"]["usage"]["total_tokens"].as_u64()),
        Some(17)
    );
}

#[tokio::test]
async fn replays_reasoning_into_follow_up_request() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![
            Ok(reasoning_chunk("chat-1", "think step")),
            Ok(content_chunk("chat-1", "hello")),
        ])
        .await;
    upstream
        .push_response(vec![Ok(content_chunk("chat-2", "follow up done"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let first_request = base_request(vec![user_message("hello")]);
    let first_events = collect_stream(
        gateway
            .clone()
            .stream_responses(first_request.clone())
            .await
            .expect("first stream"),
    )
    .await;
    let public_items = done_items(&first_events);

    let mut second_input = first_request.input;
    second_input.extend(public_items);
    second_input.push(user_message("again"));
    let second_request = base_request(second_input);

    let _ = collect_stream(
        gateway
            .stream_responses(second_request)
            .await
            .expect("second stream"),
    )
    .await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 2);
    let second_messages = &requests[1].messages;
    assert_eq!(second_messages.len(), 3);
    assert_eq!(second_messages[1].role, "assistant");
    assert_eq!(
        second_messages[1].reasoning_content.as_deref(),
        Some("think step")
    );
    assert_eq!(
        second_messages[1]
            .content
            .as_ref()
            .and_then(|value| value.as_str()),
        Some("hello")
    );
}

#[tokio::test]
async fn forwards_configured_upstream_chat_kwargs() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "hello"))])
        .await;
    let gateway = test_gateway_with_config(
        upstream.clone(),
        MockSearch::default(),
        Config {
            bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
            upstream_base_url: "http://127.0.0.1:8000/v1".parse().expect("url"),
            upstream_api_key: None,
            upstream_model: Some("GLM-5.1".to_string()),
            upstream_chat_kwargs: JsonMap::from_iter([(
                "clear_thinking".to_string(),
                json!(false),
            )]),
            model_profiles: std::collections::BTreeMap::new(),
            brave_base_url: "https://example.com/".parse().expect("url"),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout: std::time::Duration::from_secs(30),
        },
    );

    let _ = collect_stream(
        gateway
            .stream_responses(base_request(vec![user_message("hello")]))
            .await
            .expect("stream"),
    )
    .await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].extra_body.get("clear_thinking"),
        Some(&json!(false))
    );
}

#[tokio::test]
async fn forwards_profile_specific_upstream_chat_kwargs_for_backend_model() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "hello"))])
        .await;
    let gateway = test_gateway_with_config(
        upstream.clone(),
        MockSearch::default(),
        Config {
            bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
            upstream_base_url: "http://127.0.0.1:8000/v1".parse().expect("url"),
            upstream_api_key: None,
            upstream_model: None,
            upstream_chat_kwargs: JsonMap::new(),
            model_profiles: std::collections::BTreeMap::from([(
                "Kimi-K2.6".to_string(),
                resp2chat::config::ModelProfile {
                    upstream_model: None,
                    upstream_chat_kwargs: JsonMap::from_iter([(
                        "chat_template_kwargs".to_string(),
                        json!({
                            "thinking": true,
                            "preserve_thinking": true
                        }),
                    )]),
                },
            )]),
            brave_base_url: "https://example.com/".parse().expect("url"),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout: std::time::Duration::from_secs(30),
        },
    );

    let mut request = base_request(vec![user_message("hello")]);
    request.model = "Kimi-K2.6".to_string();

    let _ = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "Kimi-K2.6");
    assert_eq!(
        requests[0].extra_body.get("chat_template_kwargs"),
        Some(&json!({
            "thinking": true,
            "preserve_thinking": true
        }))
    );
}

#[tokio::test]
async fn hides_web_search_loop_but_replays_internal_tool_result() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(tool_call_chunk(
            "chat-1",
            "call_ws_1",
            "web_search",
            "{\"query\":\"weather seattle\"}",
        ))])
        .await;
    upstream
        .push_response(vec![Ok(content_chunk("chat-2", "It is rainy."))])
        .await;
    upstream
        .push_response(vec![Ok(content_chunk("chat-3", "Follow up done."))])
        .await;
    let search = MockSearch::default();
    let gateway = test_gateway(upstream.clone(), search.clone());

    let mut first_request = base_request(vec![user_message("weather?")]);
    first_request.tools = vec![ToolSpec::WebSearch {
        external_web_access: Some(true),
        filters: None,
        user_location: None,
        search_context_size: None,
        search_content_types: None,
    }];
    let first_events = collect_stream(
        gateway
            .clone()
            .stream_responses(first_request.clone())
            .await
            .expect("first stream"),
    )
    .await;

    assert_eq!(
        event_names(&first_events),
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_item.done",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(
        first_events[2]["item"]["type"].as_str(),
        Some("web_search_call")
    );
    assert_eq!(
        search.queries.lock().await.as_slice(),
        &["weather seattle".to_string()]
    );

    let mut second_input = first_request.input;
    second_input.extend(done_items(&first_events));
    second_input.push(user_message("why?"));
    let second_request = base_request(second_input);
    let _ = collect_stream(
        gateway
            .stream_responses(second_request)
            .await
            .expect("second stream"),
    )
    .await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].messages.len(), 3);
    assert_eq!(requests[1].messages[2].role, "tool");
    assert_eq!(
        requests[1].messages[2]
            .content
            .as_ref()
            .and_then(|value| value.as_str()),
        Some("Search result for weather seattle")
    );
    assert_eq!(requests[2].messages.len(), 5);
    assert_eq!(requests[2].messages[2].role, "tool");
    assert_eq!(requests[2].messages[3].role, "assistant");
}

#[tokio::test]
async fn degrades_gracefully_when_web_search_replay_baseline_is_missing() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "Recovered follow up."))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let request = base_request(vec![
        user_message("weather?"),
        ResponseItem::WebSearchCall {
            id: Some("ws_old_1".to_string()),
            status: Some("completed".to_string()),
            action: Some(resp2chat::models::responses::WebSearchAction::Search {
                query: Some("weather seattle".to_string()),
                queries: None,
            }),
        },
        ResponseItem::message_text("assistant", "It is rainy."),
        user_message("why?"),
    ]);

    let events = collect_stream(
        gateway
            .stream_responses(request)
            .await
            .expect("stream should not fail"),
    )
    .await;

    assert_eq!(
        event_names(&events),
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed",
        ]
    );

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages.len(), 5);
    assert_eq!(requests[0].messages[1].role, "assistant");
    assert_eq!(requests[0].messages[2].role, "tool");
    assert_eq!(
        requests[0].messages[2].tool_call_id.as_deref(),
        Some("ws_old_1")
    );
    assert_eq!(
        requests[0].messages[2]
            .content
            .as_ref()
            .and_then(|value| value.as_str()),
        Some(
            "Previous web_search completed in an earlier turn, but the original tool result is unavailable because replay state was missing. Query: weather seattle"
        )
    );
}

#[tokio::test]
async fn proxies_models_endpoint_with_etag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"etag-1\"")
                .set_body_json(json!({
                    "data": [{"id": "glm-5.1"}]
                })),
        )
        .mount(&server)
        .await;

    let config = Config {
        bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
        upstream_base_url: format!("{}/v1/", server.uri()).parse().expect("url"),
        upstream_api_key: None,
        upstream_model: None,
        upstream_chat_kwargs: JsonMap::new(),
        model_profiles: std::collections::BTreeMap::new(),
        brave_base_url: "https://example.com/".parse().expect("url"),
        brave_api_key: None,
        brave_max_results: 5,
        request_timeout: std::time::Duration::from_secs(30),
    };
    let app = resp2chat::build_app(config);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok()),
        Some("\"etag-1\"")
    );
}

#[tokio::test]
async fn proxies_models_endpoint_with_upstream_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer upstream-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "glm-5.1"}]
        })))
        .mount(&server)
        .await;

    let config = Config {
        bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
        upstream_base_url: format!("{}/v1/", server.uri()).parse().expect("url"),
        upstream_api_key: Some("upstream-secret".to_string()),
        upstream_model: None,
        upstream_chat_kwargs: JsonMap::new(),
        model_profiles: std::collections::BTreeMap::new(),
        brave_base_url: "https://example.com/".parse().expect("url"),
        brave_api_key: None,
        brave_max_results: 5,
        request_timeout: std::time::Duration::from_secs(30),
    };
    let app = resp2chat::build_app(config);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);
}

fn test_gateway(upstream: MockUpstream, search: MockSearch) -> Arc<Gateway> {
    test_gateway_with_config(
        upstream,
        search,
        Config {
            bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
            upstream_base_url: "http://127.0.0.1:8000/v1".parse().expect("url"),
            upstream_api_key: None,
            upstream_model: None,
            upstream_chat_kwargs: JsonMap::new(),
            model_profiles: std::collections::BTreeMap::new(),
            brave_base_url: "https://example.com/".parse().expect("url"),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout: std::time::Duration::from_secs(30),
        },
    )
}

fn test_gateway_with_config(
    upstream: MockUpstream,
    search: MockSearch,
    config: Config,
) -> Arc<Gateway> {
    Arc::new(Gateway::new(
        config,
        ReplayStore::new(),
        Arc::new(upstream),
        Arc::new(search),
        MonitorHub::new(128),
    ))
}

fn base_request(input: Vec<ResponseItem>) -> ResponsesRequest {
    ResponsesRequest {
        model: "glm-5.1".to_string(),
        instructions: String::new(),
        input,
        tools: Vec::new(),
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: Some(resp2chat::models::responses::ReasoningRequest {
            effort: Some("medium".to_string()),
            summary: None,
        }),
        store: false,
        stream: true,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        previous_response_id: None,
    }
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
    }
}

fn content_chunk(id: &str, content: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        usage: None,
        choices: vec![ChatChunkChoice {
            index: 0,
            delta: ChatDelta {
                content: Some(content.to_string()),
                reasoning_content: None,
                tool_calls: None,
            },
            finish_reason: None,
        }],
    }
}

fn reasoning_chunk(id: &str, reasoning: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        usage: None,
        choices: vec![ChatChunkChoice {
            index: 0,
            delta: ChatDelta {
                content: None,
                reasoning_content: Some(reasoning.to_string()),
                tool_calls: None,
            },
            finish_reason: None,
        }],
    }
}

fn tool_call_chunk(id: &str, call_id: &str, name: &str, arguments: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        usage: None,
        choices: vec![ChatChunkChoice {
            index: 0,
            delta: ChatDelta {
                content: None,
                reasoning_content: None,
                tool_calls: Some(vec![ChatToolCall {
                    id: Some(call_id.to_string()),
                    index: Some(0),
                    kind: "function".to_string(),
                    function: ChatFunctionCall {
                        name: Some(name.to_string()),
                        arguments: Some(serde_json::Value::String(arguments.to_string())),
                    },
                }]),
            },
            finish_reason: Some("tool_calls".to_string()),
        }],
    }
}

fn usage_chunk(
    id: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    cached_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        usage: Some(ChatCompletionUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            prompt_tokens_details: cached_tokens
                .map(|cached_tokens| ChatPromptTokensDetails { cached_tokens }),
            completion_tokens_details: reasoning_tokens
                .map(|reasoning_tokens| ChatCompletionTokensDetails { reasoning_tokens }),
            reasoning_tokens: None,
        }),
        choices: Vec::new(),
    }
}

async fn collect_stream(
    stream: tokio_stream::wrappers::ReceiverStream<resp2chat::engine::SseEvent>,
) -> Vec<serde_json::Value> {
    stream
        .map(|event| {
            let mut value = event.data;
            if let serde_json::Value::Object(map) = &mut value {
                map.insert("_event".to_string(), serde_json::Value::String(event.event));
            }
            value
        })
        .collect()
        .await
}

fn event_names(events: &[serde_json::Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| {
            event["_event"]
                .as_str()
                .expect("event name should be present")
        })
        .collect()
}

fn done_items(events: &[serde_json::Value]) -> Vec<ResponseItem> {
    events
        .iter()
        .filter(|event| event["_event"] == "response.output_item.done")
        .map(|event| serde_json::from_value(event["item"].clone()).expect("response item"))
        .collect()
}

// ---------------------------------------------------------------------------
// Anthropic /v1/messages integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn anthropic_messages_streams_text_response() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![
            Ok(content_chunk("chat-1", "Hello")),
            Ok(content_chunk("chat-1", " there")),
        ])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());
    let app = resp2chat::build_app_from_gateway(gateway);

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "stream": true,
        "messages": [
            { "role": "user", "content": "Hi" }
        ]
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).expect("serialize")))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let body_text = String::from_utf8(body_bytes.to_vec()).expect("utf8");

    // Verify key Anthropic SSE events are present
    assert!(
        body_text.contains("event: message_start"),
        "missing message_start"
    );
    assert!(
        body_text.contains("event: content_block_start"),
        "missing content_block_start"
    );
    assert!(
        body_text.contains("event: content_block_delta"),
        "missing content_block_delta"
    );
    assert!(
        body_text.contains("event: content_block_stop"),
        "missing content_block_stop"
    );
    assert!(
        body_text.contains("event: message_delta"),
        "missing message_delta"
    );
    assert!(
        body_text.contains("event: message_stop"),
        "missing message_stop"
    );

    // Verify the text content was streamed
    assert!(body_text.contains("Hello"), "missing text content");
    assert!(body_text.contains(" there"), "missing second text delta");

    // Verify the upstream received a chat completions request
    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].stream, true);
}

#[tokio::test]
async fn anthropic_messages_streams_tool_use_response() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(tool_call_chunk(
            "chat-1",
            "call_weather",
            "get_weather",
            "{\"location\":\"Seattle\"}",
        ))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());
    let app = resp2chat::build_app_from_gateway(gateway);

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "stream": true,
        "messages": [
            { "role": "user", "content": "What's the weather?" }
        ],
        "tools": [
            {
                "name": "get_weather",
                "description": "Get the weather",
                "input_schema": {
                    "type": "object",
                    "properties": { "location": { "type": "string" } },
                    "required": ["location"]
                }
            }
        ]
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).expect("serialize")))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let body_text = String::from_utf8(body_bytes.to_vec()).expect("utf8");

    assert!(
        body_text.contains("event: message_start"),
        "missing message_start"
    );
    assert!(
        body_text.contains("event: content_block_start"),
        "missing content_block_start"
    );
    assert!(
        body_text.contains("event: content_block_stop"),
        "missing content_block_stop"
    );
    assert!(
        body_text.contains("event: message_stop"),
        "missing message_stop"
    );

    // Should have tool_use stop reason
    assert!(
        body_text.contains("tool_use"),
        "missing tool_use stop reason"
    );

    // Should contain the tool call info
    assert!(body_text.contains("get_weather"), "missing tool name");
    assert!(body_text.contains("call_weather"), "missing call id");
}

#[tokio::test]
async fn anthropic_messages_converts_system_prompt() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "done"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());
    let app = resp2chat::build_app_from_gateway(gateway);

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "stream": true,
        "system": "You are a helpful assistant.",
        "messages": [
            { "role": "user", "content": "Hi" }
        ]
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).expect("serialize")))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);

    // Must consume the body to drive the SSE stream and spawn the upstream request
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024).await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    // The system prompt should have been converted to a system message
    assert_eq!(requests[0].messages[0].role, "system");
    assert_eq!(
        requests[0].messages[0]
            .content
            .as_ref()
            .and_then(|v| v.as_str()),
        Some("You are a helpful assistant.")
    );
}

#[tokio::test]
async fn anthropic_messages_returns_non_streaming_json() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "Hello"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());
    let app = resp2chat::build_app_from_gateway(gateway);

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "stream": false,
        "messages": [
            { "role": "user", "content": "Hi" }
        ]
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).expect("serialize")))
                .expect("request"),
        )
        .await
        .expect("response");

    // Should return 200 with JSON body (non-streaming)
    assert_eq!(response.status(), 200);
    let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .expect("read body");
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("valid json");
    assert_eq!(json["type"], "message");
    assert_eq!(json["role"], "assistant");
}

#[tokio::test]
async fn anthropic_messages_converts_tool_result_history() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-2", "It's 72°F in Seattle."))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());
    let app = resp2chat::build_app_from_gateway(gateway);

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "stream": true,
        "messages": [
            { "role": "user", "content": "What's the weather in Seattle?" },
            { "role": "assistant", "content": [
                { "type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": { "location": "Seattle" } }
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "toolu_1", "content": "72°F sunny" }
            ]}
        ],
        "tools": [
            {
                "name": "get_weather",
                "description": "Get the weather",
                "input_schema": {
                    "type": "object",
                    "properties": { "location": { "type": "string" } },
                    "required": ["location"]
                }
            }
        ]
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).expect("serialize")))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);

    // Must consume the body to drive the SSE stream and spawn the upstream request
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024).await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    // Should have: system(if from instructions), user, assistant+tool_call, tool_result, then current
    // Verify tool_result was converted to a tool message
    let tool_msgs: Vec<_> = requests[0]
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .collect();
    assert_eq!(tool_msgs.len(), 1);
    assert_eq!(tool_msgs[0].tool_call_id.as_deref(), Some("toolu_1"));
    assert_eq!(
        tool_msgs[0].content.as_ref().and_then(|v| v.as_str()),
        Some("72°F sunny")
    );
}
