//! Reading one MCP JSON-RPC message: its method, the tool a call names and its arguments, the
//! tools a list answers with, and whether a call's result is an error. Shared by everything
//! that sits between an MCP client and a server.

use serde_json::{Map, Value};

pub const TOOLS_CALL: &str = "tools/call";
pub const TOOLS_LIST: &str = "tools/list";

/// A `tools/call` that cannot be read as one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BadToolCall {
    #[error("tools/call names no tool")]
    NoToolName,
    #[error("tool {tool}: arguments must be an object")]
    Arguments { tool: String },
}

/// The method a request or notification names.
pub fn method(message: &Value) -> Option<&str> {
    message.get("method").and_then(Value::as_str)
}

/// A JSON-RPC response from the client to a server request: an `id` and a `result` or `error`.
pub fn is_response(message: &Value) -> bool {
    message.get("id").is_some()
        && (message.get("result").is_some() || message.get("error").is_some())
}

/// The tool a `tools/call` names and its arguments, an empty object when absent.
pub fn tool_call(message: &mut Value) -> Result<(String, &mut Map<String, Value>), BadToolCall> {
    let params = message
        .get_mut("params")
        .and_then(Value::as_object_mut)
        .ok_or(BadToolCall::NoToolName)?;
    let tool = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or(BadToolCall::NoToolName)?
        .to_string();
    let arguments = params
        .entry("arguments")
        .or_insert_with(|| Value::Object(Map::new()));
    if arguments.is_null() {
        *arguments = Value::Object(Map::new());
    }
    match arguments.as_object_mut() {
        Some(args) => Ok((tool, args)),
        None => Err(BadToolCall::Arguments { tool }),
    }
}

/// The tools a `tools/list` answer carries; `None` when `message` is not one.
pub fn listed_tools(message: &mut Value) -> Option<&mut Vec<Value>> {
    message
        .pointer_mut("/result/tools")
        .and_then(Value::as_array_mut)
}

/// The first text block of a `tools/call` answer's result.
pub fn result_text(answer: &Value) -> Option<&str> {
    answer
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
}

/// Whether a `tools/call` answer is a protocol error or a result marked `isError`.
pub fn call_failed(answer: &Value) -> bool {
    match answer.get("result") {
        Some(result) => result.get("isError").and_then(Value::as_bool) == Some(true),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::mcp_message::{
        BadToolCall, call_failed, is_response, listed_tools, method, result_text, tool_call,
    };

    #[test]
    fn a_call_yields_its_tool_and_arguments() {
        let mut call = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "crucible_draft_save", "arguments": {"draft_id": "triage"}}});
        assert_eq!(method(&call), Some("tools/call"));
        let (tool, arguments) = tool_call(&mut call).unwrap();
        assert_eq!(tool, "crucible_draft_save");
        assert_eq!(arguments["draft_id"], "triage");
        arguments.insert("injected".into(), json!(true));
        assert_eq!(call["params"]["arguments"]["injected"], true);
    }

    #[test]
    fn absent_or_null_arguments_read_as_an_empty_object() {
        for mut call in [
            json!({"method": "tools/call", "params": {"name": "whoami"}}),
            json!({"method": "tools/call", "params": {"name": "whoami", "arguments": null}}),
        ] {
            let (tool, arguments) = tool_call(&mut call).unwrap();
            assert_eq!(tool, "whoami");
            assert!(arguments.is_empty());
            assert_eq!(call["params"]["arguments"], json!({}));
        }
    }

    #[test]
    fn a_call_without_a_tool_or_with_odd_arguments_is_refused() {
        for mut call in [
            json!({"method": "tools/call"}),
            json!({"method": "tools/call", "params": []}),
            json!({"method": "tools/call", "params": {"arguments": {}}}),
            json!({"method": "tools/call", "params": {"name": 7}}),
        ] {
            let refused = tool_call(&mut call).map(|(tool, _)| tool);
            assert_eq!(refused, Err(BadToolCall::NoToolName), "{call}");
        }
        let mut listy = json!({"method": "tools/call", "params": {"name": "t", "arguments": [1]}});
        let refused = tool_call(&mut listy).unwrap_err();
        assert_eq!(refused, BadToolCall::Arguments { tool: "t".into() });
        assert_eq!(refused.to_string(), "tool t: arguments must be an object");
        assert_eq!(
            BadToolCall::NoToolName.to_string(),
            "tools/call names no tool"
        );
    }

    #[test]
    fn responses_and_requests_are_told_apart() {
        assert!(is_response(&json!({"id": 1, "result": {}})));
        assert!(is_response(&json!({"id": 1, "error": {"code": -1}})));
        assert!(!is_response(&json!({"result": {}})));
        assert!(!is_response(&json!({"id": 1, "method": "ping"})));
        assert_eq!(method(&json!({"id": 1, "result": {}})), None);
        assert_eq!(method(&json!({"method": 4})), None);
    }

    #[test]
    fn a_tool_list_is_found_only_in_a_list_answer() {
        let mut list = json!({"id": 1, "result": {"tools": [{"name": "a"}, {"name": "b"}]}});
        let tools = listed_tools(&mut list).unwrap();
        tools.retain(|tool| tool["name"] == "b");
        assert_eq!(list["result"]["tools"], json!([{"name": "b"}]));
        assert!(listed_tools(&mut json!({"id": 1, "result": {"content": []}})).is_none());
        assert!(listed_tools(&mut json!({"id": 1, "result": {"tools": {}}})).is_none());
    }

    #[test]
    fn a_result_fails_when_marked_or_missing() {
        let fine = json!({"id": 1, "result": {"content": [{"type": "text", "text": "saved"}]}});
        assert!(!call_failed(&fine));
        assert_eq!(result_text(&fine), Some("saved"));
        let marked = json!({"id": 1, "result": {"content": [], "isError": true}});
        assert!(call_failed(&marked));
        assert_eq!(result_text(&marked), None);
        assert!(call_failed(&json!({"id": 1, "error": {"code": -32001}})));
    }
}
