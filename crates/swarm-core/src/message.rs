//! The normalized conversation model every provider session speaks. Providers
//! translate their native wire format to and from this form.

use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactRef;
use crate::time::now_millis;

string_enum! {
        /// The sender of a chat message.
        pub enum ChatRole {
        System => "system",
        User => "user",
        Assistant => "assistant",
        Tool => "tool",
    }
}

string_enum! {
        /// The kind of message, recorded for auditing and rendering.
        pub enum ChatMessageKind {
        Text => "text",
        ToolResult => "tool_result",
    }
}

/// A tool call requested by an assistant message (native function calling).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// One message in an agent conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub kind: ChatMessageKind,
    pub content: String,
    /// When `role` is `Tool`, the tool-call id this message answers.
    pub tool_call_id: Option<String>,
    /// Tool calls requested by an assistant message. Empty for every other role.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<MessageToolCall>,
    /// Artifact refs cited by this message, so large outputs are referenced, not inlined.
    pub artifact_refs: Vec<ArtifactRef>,
    pub created_at: i64,
}

impl ChatMessage {
    fn base(role: ChatRole, kind: ChatMessageKind, content: impl Into<String>) -> Self {
        Self {
            role,
            kind,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            artifact_refs: Vec::new(),
            created_at: now_millis(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::base(ChatRole::System, ChatMessageKind::Text, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::base(ChatRole::User, ChatMessageKind::Text, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::base(ChatRole::Assistant, ChatMessageKind::Text, content)
    }

    pub fn assistant_with_tool_calls(content: impl Into<String>, tool_calls: Vec<MessageToolCall>) -> Self {
        let mut m = Self::base(ChatRole::Assistant, ChatMessageKind::Text, content);
        m.tool_calls = tool_calls;
        m
    }

    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        let mut m = Self::base(ChatRole::Tool, ChatMessageKind::ToolResult, content);
        m.tool_call_id = Some(call_id.into());
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_set_role() {
        assert_eq!(ChatMessage::user("hi").role, ChatRole::User);
        assert_eq!(ChatMessage::system("sys").role, ChatRole::System);
        let t = ChatMessage::tool("c1", "out");
        assert_eq!(t.role, ChatRole::Tool);
        assert_eq!(t.tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn tool_calls_round_trip_and_legacy_json_deserializes() {
        let m = ChatMessage::assistant_with_tool_calls(
            "calling tools",
            vec![MessageToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({"path": "x.txt"}),
            }],
        );
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("tool_calls"));
        let back: ChatMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tool_calls.len(), 1);
        assert_eq!(back.tool_calls[0].name, "write_file");
        assert_eq!(back.tool_calls[0].arguments, serde_json::json!({"path": "x.txt"}));

        // A checkpoint saved before `tool_calls` existed must still load.
        let mut old = serde_json::to_value(&ChatMessage::assistant("hi")).unwrap();
        old.as_object_mut().unwrap().remove("tool_calls");
        let legacy: ChatMessage = serde_json::from_value(old).unwrap();
        assert!(legacy.tool_calls.is_empty());
    }
}
