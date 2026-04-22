use crate::error::AppError;
use crate::error::AppResult;
use crate::models::anthropic::AnthropicContent;
use crate::models::anthropic::AnthropicContentBlock;
use crate::models::anthropic::AnthropicImageSource;
use crate::models::anthropic::AnthropicRequest;
use crate::models::anthropic::AnthropicSystemContent;
use crate::models::anthropic::AnthropicThinking;
use crate::models::anthropic::AnthropicTool;
use crate::models::responses::ContentItem;
use crate::models::responses::ReasoningContentItem;
use crate::models::responses::ReasoningRequest;
use crate::models::responses::ResponseItem;
use crate::models::responses::ResponsesRequest;
use crate::models::responses::ToolSpec;
use serde_json::Value;
use uuid::Uuid;

pub fn convert_request(request: AnthropicRequest) -> AppResult<ResponsesRequest> {
    let instructions = extract_system_text(&request.system);
    let input = convert_messages(&request.messages)?;
    let tools = convert_tools(&request.tools);
    let reasoning = convert_thinking(&request.thinking);

    Ok(ResponsesRequest {
        model: request.model,
        instructions,
        input,
        tools,
        tool_choice: "auto".to_string(),
        parallel_tool_calls: false,
        reasoning,
        store: false,
        stream: true,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        previous_response_id: None,
    })
}

fn extract_system_text(system: &Option<AnthropicSystemContent>) -> String {
    match system {
        Some(AnthropicSystemContent::Text(text)) => text.clone(),
        Some(AnthropicSystemContent::Blocks(blocks)) => blocks
            .iter()
            .filter_map(|block| match block {
                crate::models::anthropic::AnthropicTextBlock::Text { text } => Some(text.clone()),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
}

fn convert_thinking(thinking: &Option<AnthropicThinking>) -> Option<ReasoningRequest> {
    thinking.as_ref().and_then(|t| match t {
        AnthropicThinking::Enabled { .. } => Some(ReasoningRequest {
            effort: Some("medium".to_string()),
            summary: None,
        }),
        AnthropicThinking::Disabled => None,
        AnthropicThinking::Adaptive { .. } => Some(ReasoningRequest {
            effort: None,
            summary: None,
        }),
    })
}

fn convert_messages(
    messages: &[crate::models::anthropic::AnthropicMessage],
) -> AppResult<Vec<ResponseItem>> {
    let mut items = Vec::new();
    for message in messages {
        convert_message(&message.role, &message.content, &mut items)?;
    }
    Ok(items)
}

fn convert_message(
    role: &str,
    content: &AnthropicContent,
    items: &mut Vec<ResponseItem>,
) -> AppResult<()> {
    match content {
        AnthropicContent::Text(text) => {
            if !text.is_empty() {
                items.push(text_message_item(role, text));
            }
        }
        AnthropicContent::Blocks(blocks) => {
            // Collect text/image content into a single message item,
            // then emit tool_use / tool_result items separately.
            let mut content_items = Vec::new();
            for block in blocks {
                match block {
                    AnthropicContentBlock::Text { text } => {
                        if role == "user" {
                            content_items.push(ContentItem::InputText { text: text.clone() });
                        } else {
                            content_items.push(ContentItem::OutputText { text: text.clone() });
                        }
                    }
                    AnthropicContentBlock::Image { source } => {
                        content_items.push(ContentItem::InputImage {
                            image_url: image_source_to_url(source),
                        });
                    }
                    _ => {}
                }
            }
            if !content_items.is_empty() {
                items.push(ResponseItem::Message {
                    id: None,
                    role: role.to_string(),
                    content: content_items,
                    phase: None,
                });
            }
            for block in blocks {
                match block {
                    AnthropicContentBlock::ToolUse { id, name, input } => {
                        let arguments = serde_json::to_string(input).map_err(|err| {
                            AppError::bad_request(format!(
                                "failed to serialize tool_use input: {err}"
                            ))
                        })?;
                        items.push(ResponseItem::FunctionCall {
                            id: None,
                            name: name.clone(),
                            namespace: None,
                            arguments,
                            call_id: id.clone(),
                        });
                    }
                    AnthropicContentBlock::ToolResult {
                        tool_use_id,
                        content: result_content,
                        ..
                    } => {
                        items.push(ResponseItem::FunctionCallOutput {
                            call_id: tool_use_id.clone(),
                            output: extract_tool_result_content(result_content),
                        });
                    }
                    AnthropicContentBlock::Thinking { thinking } => {
                        items.push(ResponseItem::Reasoning {
                            id: format!("rsn_{}", Uuid::new_v4().simple()),
                            summary: Vec::new(),
                            content: Some(vec![ReasoningContentItem::ReasoningText {
                                text: thinking.clone(),
                            }]),
                            encrypted_content: None,
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn text_message_item(role: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: if role == "user" {
            vec![ContentItem::InputText {
                text: text.to_string(),
            }]
        } else {
            vec![ContentItem::OutputText {
                text: text.to_string(),
            }]
        },
        phase: None,
    }
}

fn image_source_to_url(source: &AnthropicImageSource) -> String {
    match source.kind.as_str() {
        "base64" => {
            let media_type = source.media_type.as_deref().unwrap_or("image/png");
            let data = source.data.as_deref().unwrap_or("");
            format!("data:{media_type};base64,{data}")
        }
        "url" => source.url.clone().unwrap_or_default(),
        _ => String::new(),
    }
}

fn extract_tool_result_content(content: &Option<AnthropicContent>) -> Value {
    match content {
        Some(AnthropicContent::Text(text)) => Value::String(text.clone()),
        Some(AnthropicContent::Blocks(blocks)) => {
            let text_parts: Vec<String> = blocks
                .iter()
                .filter_map(|block| match block {
                    AnthropicContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect();
            if text_parts.len() == 1 {
                Value::String(text_parts.into_iter().next().unwrap())
            } else if text_parts.is_empty() {
                Value::Null
            } else {
                Value::String(text_parts.join("\n"))
            }
        }
        None => Value::Null,
    }
}

fn convert_tools(tools: &Option<Vec<AnthropicTool>>) -> Vec<ToolSpec> {
    match tools {
        Some(tools) => tools
            .iter()
            .map(|tool| ToolSpec::Function {
                name: tool.name.clone(),
                description: tool.description.clone().unwrap_or_default(),
                strict: false,
                parameters: tool.input_schema.clone(),
            })
            .collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::anthropic::*;
    use crate::models::responses::ResponseItem;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn converts_simple_text_request() {
        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: Some(1024),
            system: Some(AnthropicSystemContent::Text("Be helpful".to_string())),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text("Hello".to_string()),
            }],
            tools: None,
            tool_choice: None,
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
        };

        let result = convert_request(request).expect("convert");
        assert_eq!(result.model, "claude-3-5-sonnet-20241022");
        assert_eq!(result.instructions, "Be helpful");
        assert_eq!(result.input.len(), 1);
        assert_eq!(result.stream, true);
    }

    #[test]
    fn converts_tool_use_and_result_history() {
        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: Some(1024),
            system: None,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicContent::Text("What is the weather?".to_string()),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: AnthropicContent::Blocks(vec![
                        AnthropicContentBlock::Text {
                            text: "Let me check.".to_string(),
                        },
                        AnthropicContentBlock::ToolUse {
                            id: "toolu_1".to_string(),
                            name: "get_weather".to_string(),
                            input: json!({"location": "Seattle"}),
                        },
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicContent::Blocks(vec![AnthropicContentBlock::ToolResult {
                        tool_use_id: "toolu_1".to_string(),
                        content: Some(AnthropicContent::Text("72°F sunny".to_string())),
                        is_error: None,
                    }]),
                },
            ],
            tools: None,
            tool_choice: None,
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
        };

        let result = convert_request(request).expect("convert");
        // user text, assistant text, function_call, function_call_output
        assert_eq!(result.input.len(), 4);

        let user_msg = &result.input[0];
        assert!(matches!(user_msg, ResponseItem::Message { role, .. } if role == "user"));

        let asst_text = &result.input[1];
        assert!(matches!(asst_text, ResponseItem::Message { role, .. } if role == "assistant"));

        let fn_call = &result.input[2];
        assert!(
            matches!(fn_call, ResponseItem::FunctionCall { name, call_id, .. }
            if name == "get_weather" && call_id == "toolu_1")
        );

        let fn_output = &result.input[3];
        assert!(
            matches!(fn_output, ResponseItem::FunctionCallOutput { call_id, .. } if call_id == "toolu_1")
        );
    }

    #[test]
    fn converts_tools_to_function_specs() {
        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: Some(1024),
            system: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text("Hi".to_string()),
            }],
            tools: Some(vec![AnthropicTool {
                name: "get_weather".to_string(),
                description: Some("Get weather".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": { "location": { "type": "string" } },
                    "required": ["location"]
                }),
            }]),
            tool_choice: None,
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
        };

        let result = convert_request(request).expect("convert");
        assert_eq!(result.tools.len(), 1);
        assert!(
            matches!(&result.tools[0], ToolSpec::Function { name, .. } if name == "get_weather")
        );
    }

    #[test]
    fn converts_thinking_enabled_to_reasoning() {
        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: Some(16000),
            system: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text("Think".to_string()),
            }],
            tools: None,
            tool_choice: None,
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: Some(AnthropicThinking::Enabled {
                budget_tokens: Some(10000),
            }),
        };

        let result = convert_request(request).expect("convert");
        assert!(result.reasoning.is_some());
        assert_eq!(
            result.reasoning.as_ref().unwrap().effort.as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn accepts_non_streaming_request() {
        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: Some(1024),
            system: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text("Hello".to_string()),
            }],
            tools: None,
            tool_choice: None,
            stream: false,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
        };

        let result = convert_request(request);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().stream, true);
    }

    #[test]
    fn converts_base64_image_to_data_url() {
        let source = AnthropicImageSource {
            kind: "base64".to_string(),
            media_type: Some("image/png".to_string()),
            data: Some("iVBORw0KGgo=".to_string()),
            url: None,
        };
        assert_eq!(
            image_source_to_url(&source),
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn converts_url_image_source() {
        let source = AnthropicImageSource {
            kind: "url".to_string(),
            media_type: None,
            data: None,
            url: Some("https://example.com/img.png".to_string()),
        };
        assert_eq!(image_source_to_url(&source), "https://example.com/img.png");
    }
}
