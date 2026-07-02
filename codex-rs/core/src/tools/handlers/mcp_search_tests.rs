use super::*;
use codex_tools::LoadableToolSpec;
use codex_tools::ToolSearchSourceInfo;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn search_info_uses_mcp_tool_metadata_and_parameter_names() {
    let handler = McpHandler::new(tool_info()).expect("MCP tool spec should build");
    let search_info = handler.search_info().expect("MCP search info");

    assert_eq!(
        search_info.entry.search_text,
        "mcp__calendar___create_event _create_event createEvent codex-apps Create event Create a calendar event. Calendar Plan events. Calendar plugin start_time attendees"
    );
    assert_eq!(
        search_info.source_info,
        Some(ToolSearchSourceInfo {
            name: "Calendar".to_string(),
            description: Some("Plan events.".to_string()),
        })
    );
}

#[test]
fn search_text_indexes_parameter_descriptions_and_nested_schema() {
    // A tool whose name and parameter names do not contain the search keyword,
    // but whose parameter description does. Before the fix only parameter
    // *names* were indexed, so this keyword would miss in BM25 retrieval.
    let mut info = tool_info();
    info.tool = rmcp::model::Tool::new(
        "createEvent",
        "Create a calendar event.",
        Arc::new(rmcp::model::object(json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "object",
                    "description": "The venue address as a zephyr-format geocode.",
                    "properties": {
                        "lat": { "type": "number", "description": "latitude" },
                        "lng": { "type": "number", "description": "longitude" }
                    }
                }
            },
            "additionalProperties": false
        }))),
    )
    .with_title("Create event");
    let handler = McpHandler::new(info).expect("MCP tool spec should build");
    let search_info = handler.search_info().expect("MCP search info");
    let text = search_info.entry.search_text;

    // Top-level parameter name + its description are indexed.
    assert!(
        text.contains("location"),
        "parameter name should be indexed: {text}"
    );
    assert!(
        text.contains("zephyr"),
        "parameter description should be indexed: {text}"
    );
    // Nested property descriptions are also indexed recursively.
    assert!(
        text.contains("latitude") && text.contains("longitude"),
        "nested descriptions should be indexed: {text}"
    );
}

#[test]
fn search_info_uses_connector_name_for_output_namespace_description() {
    let mut tool_info = tool_info();
    tool_info.namespace_description = None;
    let handler = McpHandler::new(tool_info).expect("MCP tool spec should build");
    let search_info = handler.search_info().expect("MCP search info");

    let LoadableToolSpec::Namespace(namespace) = search_info.entry.output else {
        panic!("expected namespace search output");
    };
    assert_eq!(namespace.description, "Tools for working with Calendar.");
    assert_eq!(
        search_info.source_info,
        Some(ToolSearchSourceInfo {
            name: "Calendar".to_string(),
            description: None,
        })
    );
}

fn tool_info() -> ToolInfo {
    ToolInfo {
        server_name: "codex-apps".to_string(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: "_create_event".to_string(),
        callable_namespace: "mcp__calendar__".to_string(),
        namespace_description: Some("Plan events.".to_string()),
        tool: rmcp::model::Tool::new(
            "createEvent",
            "Create a calendar event.",
            Arc::new(rmcp::model::object(json!({
                "type": "object",
                "properties": {
                    "start_time": { "type": "string" },
                    "attendees": { "type": "string" }
                },
                "additionalProperties": false
            }))),
        )
        .with_title("Create event"),
        connector_id: None,
        connector_name: Some("Calendar".to_string()),
        plugin_display_names: vec![" Calendar plugin ".to_string(), " ".to_string()],
    }
}
