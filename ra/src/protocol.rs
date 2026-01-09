use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub(crate) struct ChatCompletionResponse {
    pub(crate) choices: Vec<Choice>,
    #[serde(default)]
    pub(crate) usage: Option<Usage>,
}

#[derive(Deserialize)]
pub(crate) struct Choice {
    pub(crate) message: ChatMessage,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ChatMessage {
    pub(crate) role: String,
    pub(crate) content: Option<String>,
    #[serde(default)]
    pub(crate) tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) call_type: String,
    pub(crate) function: ToolFunction,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ToolFunction {
    pub(crate) name: String,
    pub(crate) arguments: String,
}

#[derive(Clone, Deserialize)]
pub(crate) struct Usage {
    #[serde(default)]
    pub(crate) prompt_tokens: i64,
    #[serde(default)]
    pub(crate) completion_tokens: i64,
    #[serde(default)]
    pub(crate) total_tokens: i64,
    #[serde(default)]
    pub(crate) prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    pub(crate) completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct PromptTokensDetails {
    #[serde(default)]
    pub(crate) cached_tokens: Option<i64>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct CompletionTokensDetails {
    #[serde(default)]
    pub(crate) reasoning_tokens: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct ApiErrorResponse {
    pub(crate) error: ApiErrorDetail,
}

#[derive(Deserialize)]
pub(crate) struct ApiErrorDetail {
    pub(crate) message: String,
}

#[derive(Clone, Default, Serialize)]
pub(crate) struct TokenUsage {
    pub(crate) input_tokens: i64,
    pub(crate) cached_input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) reasoning_output_tokens: i64,
    pub(crate) total_tokens: i64,
}

impl TokenUsage {
    pub(crate) fn add_assign(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_output_tokens += other.reasoning_output_tokens;
        self.total_tokens += other.total_tokens;
    }
}

pub(crate) struct CompletionResult {
    pub(crate) message: ChatMessage,
    pub(crate) usage: Option<Usage>,
}
