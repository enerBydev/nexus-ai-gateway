use serde::{Deserialize, Serialize};
use serde_json::Value;

/// OpenAI API request structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// OpenAI stream_options: when include_usage=true, NIM sends token counts in final chunk
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    /// NIM/vLLM chat template kwargs for model-specific features (e.g. GLM5 thinking)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Format variant for tool specifications
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolFormat {
    /// OpenAI format: { type: "function", function: { name, description, parameters } }
    OpenAI,
    /// Anthropic format: { name, description, input_schema, type? }
    Anthropic,
}

/// Tool specification with dual-format support
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: Option<String>,
    pub schema: serde_json::Value,
    pub anthropic_type: Option<String>, // e.g. "text_editor_20250424" — only for Anthropic format
    pub tool_format: ToolFormat,
}

impl serde::Serialize for ToolSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        match self.tool_format {
            ToolFormat::OpenAI => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_key("type")?;
                map.serialize_value("function")?;

                let mut function_map = serde_json::Map::new();
                function_map
                    .insert("name".to_string(), serde_json::Value::String(self.name.clone()));
                if let Some(ref description) = self.description {
                    function_map.insert(
                        "description".to_string(),
                        serde_json::Value::String(description.clone()),
                    );
                }
                function_map.insert("parameters".to_string(), self.schema.clone());

                map.serialize_key("function")?;
                map.serialize_value(&function_map)?;
                map.end()
            }
            ToolFormat::Anthropic => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_key("name")?;
                map.serialize_value(&self.name)?;
                if let Some(ref description) = self.description {
                    map.serialize_key("description")?;
                    map.serialize_value(description)?;
                }
                map.serialize_key("input_schema")?;
                map.serialize_value(&self.schema)?;
                if let Some(ref tool_type) = self.anthropic_type {
                    map.serialize_key("type")?;
                    map.serialize_value(tool_type)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for ToolSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};
        use std::fmt;
        use std::marker::PhantomData;

        struct ToolSpecVisitor {
            marker: PhantomData<fn() -> ToolSpec>,
        }

        impl<'de> Visitor<'de> for ToolSpecVisitor {
            type Value = ToolSpec;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a tool specification")
            }

            fn visit_map<V>(self, mut map: V) -> Result<ToolSpec, V::Error>
            where
                V: MapAccess<'de>,
            {
                let mut tool_type: Option<String> = None;
                let mut tool_function: Option<serde_json::Value> = None;
                let mut name: Option<String> = None;
                let mut description: Option<String> = None;
                let mut _parameters: Option<serde_json::Value> = None;
                let mut input_schema: Option<serde_json::Value> = None;

                // Try to determine format by checking for "type": "function"
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => {
                            tool_type = Some(map.next_value()?);
                        }
                        "function" => {
                            tool_function = Some(map.next_value()?);
                        }
                        "name" => {
                            name = map.next_value()?;
                        }
                        "description" => {
                            description = map.next_value()?;
                        }
                        "parameters" => {
                            _parameters = map.next_value()?;
                        }
                        "input_schema" => {
                            input_schema = map.next_value()?;
                        }
                        _ => {
                            // Skip unknown fields
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }

                // Determine format based on whether we have "type": "function"
                let tool_format = if tool_type.as_deref() == Some("function") {
                    ToolFormat::OpenAI
                } else {
                    ToolFormat::Anthropic
                };

                // Build the ToolSpec based on format
                match tool_format {
                    ToolFormat::OpenAI => {
                        if let Some(func) = tool_function {
                            if let Some(func_obj) = func.as_object() {
                                let name = func_obj
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                                    .unwrap_or_default();
                                let description = func_obj
                                    .get("description")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                let parameters = func_obj
                                    .get("parameters")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

                                Ok(ToolSpec {
                                    name,
                                    description,
                                    schema: parameters,
                                    anthropic_type: None,
                                    tool_format: ToolFormat::OpenAI,
                                })
                            } else {
                                Err(serde::de::Error::custom("Invalid function object"))
                            }
                        } else {
                            Err(serde::de::Error::custom(
                                "Missing function field for OpenAI format",
                            ))
                        }
                    }
                    ToolFormat::Anthropic => Ok(ToolSpec {
                        name: name.unwrap_or_default(),
                        description,
                        schema: input_schema
                            .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
                        anthropic_type: tool_type.filter(|t| t != "function"),
                        tool_format: ToolFormat::Anthropic,
                    }),
                }
            }
        }

        let visitor = ToolSpecVisitor { marker: PhantomData };
        deserializer.deserialize_map(visitor)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Tool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: Function,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Function {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

/// OpenAI API response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    // Issue #119: degenerate NIM responses (near-full context) send `"usage":null`.
    // Optional + default so the body still decodes; the empty-`choices` guard in
    // resilient_send turns it into a clear ContextOverflow error.
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: usize,
    pub message: ChoiceMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Phase 9: Reasoning/thinking content from NIM models
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Universal: Some NIM models (e.g. Kimi K2.5) use "reasoning" instead of "reasoning_content"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Streaming chunk structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<StreamChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChoice {
    pub index: usize,
    pub delta: Delta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<DeltaToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Universal: fallback for models using "reasoning" field (e.g. Kimi K2.5)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaToolCall {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub call_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<DeltaFunctionCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaFunctionCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}
