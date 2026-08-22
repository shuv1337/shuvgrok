//! First-class Sky Computer Use tools (same namespace as `read_file` / `list_dir`).
//!
//! These talk to a long-lived `bin/sky rpc` process from `agustif/sky-re`
//! (signed node + SkyComputerUseClient). They are not MCP wrappers: the
//! model sees `list_apps` / `get_app_state` / `click` as GrokBuild tools.

mod rpc;

use crate::types::output::ToolOutput;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_io::ToolInput;
use rpc::{call_sky as call_sky_rpc, load_screenshot};
use std::path::{Path, PathBuf};

/// When true, the shell should not start the MCP server of this name.
/// Native GrokBuild Sky tools already expose the same desktop methods.
pub fn suppresses_duplicate_sky_mcp(server_name: &str) -> bool {
    server_name.eq_ignore_ascii_case("sky") && std::env::var_os("SKY_KEEP_MCP").is_none()
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SkyOutput {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_b64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_path: Option<String>,
}

impl xai_tool_runtime::ToolOutput for SkyOutput {
    fn model_output(&self) -> Vec<xai_tool_runtime::ContentBlock> {
        let mut blocks = vec![xai_tool_runtime::ContentBlock::Text {
            text: self.text.clone(),
        }];
        if let (Some(mime_type), Some(data)) = (&self.screenshot_mime, &self.screenshot_b64) {
            blocks.push(xai_tool_runtime::ContentBlock::Image {
                mime_type: mime_type.clone(),
                data: data.clone(),
                media_id: None,
                filename: self
                    .screenshot_path
                    .as_ref()
                    .and_then(|path| Path::new(path).file_name())
                    .and_then(|name| name.to_str())
                    .map(str::to_owned),
                path: self.screenshot_path.clone(),
                metadata: std::collections::HashMap::new(),
            });
        }
        blocks
    }
}

impl From<SkyOutput> for ToolOutput {
    fn from(output: SkyOutput) -> Self {
        Self::Text(output.text.into())
    }
}

impl From<String> for SkyOutput {
    fn from(text: String) -> Self {
        Self {
            text,
            ..Self::default()
        }
    }
}

fn sky_result_to_output(result: rpc::SkyCallResult) -> SkyOutput {
    let mut output = SkyOutput {
        text: result.text,
        ..SkyOutput::default()
    };
    if let Some(url) = result.screenshot_url.as_deref()
        && let Some((mime, data, path)) = load_screenshot(url)
    {
        output.screenshot_mime = Some(mime);
        output.screenshot_b64 = Some(data);
        if !path.is_empty() {
            output.screenshot_path = Some(path);
        }
    }
    output
}

async fn sky(
    method: &str,
    args: serde_json::Value,
) -> Result<SkyOutput, xai_tool_runtime::ToolError> {
    call_sky_rpc(method, compact_json(args))
        .await
        .map(sky_result_to_output)
}

fn compact_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .filter(|(_, value)| !value.is_null())
                .collect(),
        ),
        other => other,
    }
}

pub(crate) fn sky_bin() -> Result<PathBuf, xai_tool_runtime::ToolError> {
    if let Ok(explicit) = std::env::var("SKY_BIN") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Ok(path);
        }
    }
    if let Ok(root) = std::env::var("SKY_STANDALONE_ROOT").or_else(|_| std::env::var("SKY_ROOT")) {
        let path = Path::new(&root).join("bin/sky");
        if path.is_file() {
            return Ok(path);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        for candidate in [
            cwd.join("bin/sky"),
            cwd.join("sky-re/bin/sky"),
            cwd.join("../sky-re/bin/sky"),
        ] {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    if let Ok(path) = which::which("sky") {
        return Ok(path);
    }
    if let Some(home) = dirs::home_dir() {
        for rel in [
            "sky-re/bin/sky",
            "code/sky-re/bin/sky",
            "src/sky-re/bin/sky",
            "Projects/sky-re/bin/sky",
            "dev/sky-re/bin/sky",
        ] {
            let path = home.join(rel);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    Err(xai_tool_runtime::ToolError::custom(
        "sky_not_found",
        "sky-standalone not found. Clone agustif/sky-re, run examples/setup.sh, set SKY_STANDALONE_ROOT.",
    ))
}

macro_rules! sky_tool {
    ($Tool:ident, $id:literal, $desc:literal, $kind:expr, $Input:ident, |$input:ident| $body:block) => {
        #[derive(Debug, Default)]
        pub struct $Tool;

        impl From<$Input> for ToolInput {
            fn from(value: $Input) -> Self {
                ToolInput::Dynamic(serde_json::to_value(value).expect("sky tool input serializes"))
            }
        }

        impl crate::types::tool_metadata::ToolMetadata for $Tool {
            fn kind(&self) -> ToolKind {
                $kind
            }
            fn tool_namespace(&self) -> ToolNamespace {
                ToolNamespace::GrokBuild
            }
            fn description_template(&self) -> &str {
                $desc
            }
            fn requires_expr(&self) -> Expr<ToolRequirement> {
                Expr::True
            }
        }

        impl xai_tool_runtime::Tool for $Tool {
            type Args = $Input;
            type Output = SkyOutput;

            fn id(&self) -> xai_tool_protocol::ToolId {
                xai_tool_protocol::ToolId::new($id).expect("valid tool id")
            }

            fn description(
                &self,
                _ctx: &::xai_tool_runtime::ListToolsContext,
            ) -> xai_tool_types::ToolDescription {
                xai_tool_types::ToolDescription::new(
                    $id,
                    crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
                )
            }

            fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
                xai_tool_protocol::ToolCapabilities {
                    is_read_only: matches!($kind, ToolKind::Read | ToolKind::List),
                    tool_scope: Some(if matches!($kind, ToolKind::Read | ToolKind::List) {
                        xai_tool_protocol::ToolScope::Read
                    } else {
                        xai_tool_protocol::ToolScope::Write
                    }),
                    ..Default::default()
                }
            }

            #[tracing::instrument(name = "tool.sky", skip_all, fields(tool = $id))]
            async fn run(
                &self,
                _ctx: xai_tool_runtime::ToolCallContext,
                $input: $Input,
            ) -> Result<SkyOutput, xai_tool_runtime::ToolError> {
                let _ = &$input;
                $body
            }
        }
    };
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ListAppsInput {}

sky_tool!(
    ListAppsTool,
    "list_apps",
    "List local macOS apps targetable by Sky Computer Use (running/recent, canonical ids). Does not launch ChatGPT.",
    ToolKind::List,
    ListAppsInput,
    |_input| {
        sky("list_apps", serde_json::json!({})).await
    }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct GetAppStateInput {
    #[schemars(description = "App display name, .app path, or bundle id from list_apps")]
    pub app: String,
    #[serde(default)]
    #[schemars(description = "Return a full accessibility tree instead of a diff")]
    pub disable_diff: Option<bool>,
}

sky_tool!(
    GetAppStateTool,
    "get_app_state",
    "Capture an app window screenshot and indexed accessibility text. Call before acting and after each action. Do not reuse stale element indexes. Does not launch ChatGPT.",
    ToolKind::Read,
    GetAppStateInput,
    |input| {
        sky(
            "get_app_state",
            serde_json::json!({
                "app": input.app,
                "disableDiff": input.disable_diff,
            }),
        )
        .await
    }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ClickInput {
    pub app: String,
    pub element_index: Option<i64>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub mouse_button: Option<String>,
    pub click_count: Option<i64>,
}

sky_tool!(
    ClickTool,
    "click",
    "Click an indexed element from the latest get_app_state tree, or a screenshot coordinate.",
    ToolKind::Other,
    ClickInput,
    |input| {
        sky(
            "click",
            serde_json::json!({
                "app": input.app,
                "element_index": input.element_index,
                "x": input.x,
                "y": input.y,
                "mouse_button": input.mouse_button,
                "click_count": input.click_count,
            }),
        )
        .await
    }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct DragInput {
    pub app: String,
    pub from_x: f64,
    pub from_y: f64,
    pub to_x: f64,
    pub to_y: f64,
}

sky_tool!(
    DragTool,
    "drag",
    "Drag between two app-window screenshot-relative coordinates.",
    ToolKind::Other,
    DragInput,
    |input| {
        sky(
            "drag",
            serde_json::json!({
                "app": input.app,
                "from_x": input.from_x,
                "from_y": input.from_y,
                "to_x": input.to_x,
                "to_y": input.to_y,
            }),
        )
        .await
    }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct PerformSecondaryActionInput {
    pub app: String,
    pub element_index: i64,
    pub action: String,
}

sky_tool!(
    PerformSecondaryActionTool,
    "perform_secondary_action",
    "Invoke a secondary accessibility action explicitly exposed for an indexed element.",
    ToolKind::Other,
    PerformSecondaryActionInput,
    |input| {
        sky(
            "perform_secondary_action",
            serde_json::json!({
                "app": input.app,
                "element_index": input.element_index,
                "action": input.action,
            }),
        )
        .await
    }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct PressKeyInput {
    pub app: String,
    pub key: String,
}

sky_tool!(
    PressKeyTool,
    "press_key",
    "Press a key or + separated X keysym-style chord (Return, Tab, Control_L+a, Super_L+d).",
    ToolKind::Other,
    PressKeyInput,
    |input| {
        sky(
            "press_key",
            serde_json::json!({
                "app": input.app,
                "key": input.key,
            }),
        )
        .await
    }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ScrollInput {
    pub app: String,
    pub element_index: i64,
    pub direction: String,
    pub pages: Option<f64>,
}

sky_tool!(
    ScrollTool,
    "scroll",
    "Scroll an indexed app element in a direction by a number of pages.",
    ToolKind::Other,
    ScrollInput,
    |input| {
        sky(
            "scroll",
            serde_json::json!({
                "app": input.app,
                "element_index": input.element_index,
                "direction": input.direction,
                "pages": input.pages,
            }),
        )
        .await
    }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SelectTextInput {
    pub app: String,
    pub element_index: i64,
    pub text: String,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    pub selection_type: Option<String>,
}

sky_tool!(
    SelectTextTool,
    "select_text",
    "Select exact text in an indexed editable element or place the cursor before/after it.",
    ToolKind::Other,
    SelectTextInput,
    |input| {
        sky(
            "select_text",
            serde_json::json!({
                "app": input.app,
                "element_index": input.element_index,
                "text": input.text,
                "prefix": input.prefix,
                "suffix": input.suffix,
                "selection_type": input.selection_type,
            }),
        )
        .await
    }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SetValueInput {
    pub app: String,
    pub element_index: i64,
    pub value: String,
}

sky_tool!(
    SetValueTool,
    "set_value",
    "Replace the value of an indexed settable accessibility element.",
    ToolKind::Other,
    SetValueInput,
    |input| {
        sky(
            "set_value",
            serde_json::json!({
                "app": input.app,
                "element_index": input.element_index,
                "value": input.value,
            }),
        )
        .await
    }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct TypeTextInput {
    pub app: String,
    pub text: String,
}

sky_tool!(
    TypeTextTool,
    "type_text",
    "Type literal text into the current focus in the specified app.",
    ToolKind::Other,
    TypeTextInput,
    |input| {
        sky(
            "type_text",
            serde_json::json!({
                "app": input.app,
                "text": input.text,
            }),
        )
        .await
    }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct BrowsersListInput {}

sky_tool!(
    BrowsersListTool,
    "browsers_list",
    "List Playwright/CDP browsers this standalone CUA can attach to.",
    ToolKind::List,
    BrowsersListInput,
    |_input| { sky("browsers_list", serde_json::json!({})).await }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct TabsListInput {}

sky_tool!(
    TabsListTool,
    "tabs_list",
    "List open pages in the Playwright browser session.",
    ToolKind::List,
    TabsListInput,
    |_input| { sky("tabs_list", serde_json::json!({})).await }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct PageGotoInput {
    pub url: String,
    pub new_tab: Option<bool>,
}

sky_tool!(
    PageGotoTool,
    "page_goto",
    "Navigate the current (or new) Playwright page to a URL.",
    ToolKind::Other,
    PageGotoInput,
    |input| {
        sky(
            "page_goto",
            serde_json::json!({
                "url": input.url,
                "new_tab": input.new_tab,
            }),
        )
        .await
    }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct PageScreenshotInput {}

sky_tool!(
    PageScreenshotTool,
    "page_screenshot",
    "Screenshot the current Playwright page. Returns a PNG image.",
    ToolKind::Read,
    PageScreenshotInput,
    |_input| { sky("page_screenshot", serde_json::json!({})).await }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct PageClickInput {
    pub selector: Option<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
}

sky_tool!(
    PageClickTool,
    "page_click",
    "Click a CSS selector or viewport coordinate on the current Playwright page.",
    ToolKind::Other,
    PageClickInput,
    |input| {
        sky(
            "page_click",
            serde_json::json!({
                "selector": input.selector,
                "x": input.x,
                "y": input.y,
            }),
        )
        .await
    }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct PageTypeInput {
    pub text: String,
    pub selector: Option<String>,
}

sky_tool!(
    PageTypeTool,
    "page_type",
    "Type text into the focused Playwright element or a CSS selector.",
    ToolKind::Other,
    PageTypeInput,
    |input| {
        sky(
            "page_type",
            serde_json::json!({
                "text": input.text,
                "selector": input.selector,
            }),
        )
        .await
    }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct PageContentInput {}

sky_tool!(
    PageContentTool,
    "page_content",
    "Read the current Playwright page URL, title, and visible text.",
    ToolKind::Read,
    PageContentInput,
    |_input| { sky("page_content", serde_json::json!({})).await }
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct PageCloseInput {}

sky_tool!(
    PageCloseTool,
    "page_close",
    "Close the Playwright browser session.",
    ToolKind::Other,
    PageCloseInput,
    |_input| { sky("page_close", serde_json::json!({})).await }
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_ids_match_sky_cli() {
        assert_eq!(
            xai_tool_runtime::Tool::id(&ListAppsTool).as_str(),
            "list_apps"
        );
        assert_eq!(
            xai_tool_runtime::Tool::id(&GetAppStateTool).as_str(),
            "get_app_state"
        );
        assert_eq!(xai_tool_runtime::Tool::id(&ClickTool).as_str(), "click");
        assert_eq!(xai_tool_runtime::Tool::id(&DragTool).as_str(), "drag");
        assert_eq!(
            xai_tool_runtime::Tool::id(&PerformSecondaryActionTool).as_str(),
            "perform_secondary_action"
        );
        assert_eq!(
            xai_tool_runtime::Tool::id(&PressKeyTool).as_str(),
            "press_key"
        );
        assert_eq!(xai_tool_runtime::Tool::id(&ScrollTool).as_str(), "scroll");
        assert_eq!(
            xai_tool_runtime::Tool::id(&SelectTextTool).as_str(),
            "select_text"
        );
        assert_eq!(
            xai_tool_runtime::Tool::id(&SetValueTool).as_str(),
            "set_value"
        );
        assert_eq!(
            xai_tool_runtime::Tool::id(&TypeTextTool).as_str(),
            "type_text"
        );
        assert_eq!(
            xai_tool_runtime::Tool::id(&BrowsersListTool).as_str(),
            "browsers_list"
        );
        assert_eq!(xai_tool_runtime::Tool::id(&TabsListTool).as_str(), "tabs_list");
        assert_eq!(xai_tool_runtime::Tool::id(&PageGotoTool).as_str(), "page_goto");
        assert_eq!(
            xai_tool_runtime::Tool::id(&PageScreenshotTool).as_str(),
            "page_screenshot"
        );
        assert_eq!(xai_tool_runtime::Tool::id(&PageClickTool).as_str(), "page_click");
        assert_eq!(xai_tool_runtime::Tool::id(&PageTypeTool).as_str(), "page_type");
        assert_eq!(
            xai_tool_runtime::Tool::id(&PageContentTool).as_str(),
            "page_content"
        );
        assert_eq!(xai_tool_runtime::Tool::id(&PageCloseTool).as_str(), "page_close");
    }

    #[test]
    fn read_tools_are_read_only() {
        assert!(xai_tool_runtime::Tool::capabilities(&ListAppsTool).is_read_only);
        assert!(xai_tool_runtime::Tool::capabilities(&GetAppStateTool).is_read_only);
        assert!(xai_tool_runtime::Tool::capabilities(&PageScreenshotTool).is_read_only);
        assert!(xai_tool_runtime::Tool::capabilities(&PageContentTool).is_read_only);
        assert!(!xai_tool_runtime::Tool::capabilities(&ClickTool).is_read_only);
        assert!(!xai_tool_runtime::Tool::capabilities(&PageGotoTool).is_read_only);
        assert!(!xai_tool_runtime::Tool::capabilities(&TypeTextTool).is_read_only);
    }

    #[test]
    fn inputs_convert_to_dynamic_tool_input() {
        let input = ToolInput::from(ListAppsInput {});
        match input {
            ToolInput::Dynamic(value) => assert!(value.is_object()),
            other => panic!("expected Dynamic, got {other:?}"),
        }
    }

    #[test]
    fn suppresses_mcp_sky_by_default() {
        assert!(suppresses_duplicate_sky_mcp("sky"));
        assert!(suppresses_duplicate_sky_mcp("SKY"));
        assert!(!suppresses_duplicate_sky_mcp("playwright"));
    }

    #[test]
    fn get_app_state_output_includes_image_block() {
        let output = SkyOutput {
            text: "AX tree".into(),
            screenshot_mime: Some("image/png".into()),
            screenshot_b64: Some("abcd".into()),
            screenshot_path: Some("/tmp/shot.png".into()),
        };
        let blocks = xai_tool_runtime::ToolOutput::model_output(&output);
        assert!(matches!(
            &blocks[0],
            xai_tool_runtime::ContentBlock::Text { text } if text == "AX tree"
        ));
        assert!(matches!(
            &blocks[1],
            xai_tool_runtime::ContentBlock::Image {
                mime_type,
                data,
                path,
                ..
            } if mime_type == "image/png" && data == "abcd" && path.as_deref() == Some("/tmp/shot.png")
        ));
    }

    #[tokio::test]
    async fn list_apps_runs_via_bin_sky() {
        if sky_bin().is_err() {
            return;
        }
        let resources = crate::types::resources::Resources::new();
        let result = xai_tool_runtime::Tool::run(
            &ListAppsTool,
            crate::types::tool_metadata::test_ctx(resources.into_shared()),
            ListAppsInput {},
        )
        .await
        .expect("list_apps should succeed when bin/sky exists");
        assert!(
            result.text.contains(".app") || result.text.contains("com."),
            "unexpected list_apps output: {}",
            result.text
        );
    }
}
