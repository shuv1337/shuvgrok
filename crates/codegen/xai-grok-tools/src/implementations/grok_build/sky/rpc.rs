use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::sky_bin;

const DEFAULT_TIMEOUT_SECS: u64 = 180;

#[derive(Debug, Clone)]
pub struct SkyCallResult {
    pub text: String,
    pub screenshot_url: Option<String>,
}

struct SkyRpc {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

fn timeout_secs() -> u64 {
    std::env::var("SKY_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
}

fn rpc_slot() -> &'static Mutex<Option<SkyRpc>> {
    static SLOT: OnceLock<Mutex<Option<SkyRpc>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

impl SkyRpc {
    fn spawn() -> Result<Self, xai_tool_runtime::ToolError> {
        let bin = sky_bin()?;
        let mut command = Command::new(&bin);
        command
            .arg("rpc")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("sky").expect("id"),
                format!("failed to spawn {} rpc: {error}", bin.display()),
            )
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("sky").expect("id"),
                "sky rpc stdin missing",
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("sky").expect("id"),
                "sky rpc stdout missing",
            )
        })?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            _ => false,
        }
    }

    async fn call(
        &mut self,
        method: &str,
        args: serde_json::Value,
    ) -> Result<SkyCallResult, xai_tool_runtime::ToolError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = serde_json::json!({
            "id": id.to_string(),
            "method": method,
            "args": args,
        });
        let mut line = request.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.map_err(|error| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("sky").expect("id"),
                format!("sky rpc write failed: {error}"),
            )
        })?;
        self.stdin.flush().await.map_err(|error| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("sky").expect("id"),
                format!("sky rpc flush failed: {error}"),
            )
        })?;
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(timeout_secs()), async {
            loop {
                response.clear();
                let n = self.stdout.read_line(&mut response).await.map_err(|error| {
                    xai_tool_runtime::ToolError::execution(
                        xai_tool_protocol::ToolId::new("sky").expect("id"),
                        format!("sky rpc read failed: {error}"),
                    )
                })?;
                if n == 0 {
                    return Err(xai_tool_runtime::ToolError::execution(
                        xai_tool_protocol::ToolId::new("sky").expect("id"),
                        "sky rpc closed",
                    ));
                }
                let trimmed = response.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|error| {
                    xai_tool_runtime::ToolError::execution(
                        xai_tool_protocol::ToolId::new("sky").expect("id"),
                        format!("sky rpc invalid json: {error}"),
                    )
                })?;
                if value.get("id").and_then(|id| id.as_str()) != Some(&id.to_string()) {
                    continue;
                }
                if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
                    let error = value
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("sky rpc failed");
                    return Err(xai_tool_runtime::ToolError::execution(
                        xai_tool_protocol::ToolId::new("sky").expect("id"),
                        error.to_owned(),
                    ));
                }
                let result = value.get("result").cloned().unwrap_or(serde_json::json!({}));
                let text = result
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("ok")
                    .to_owned();
                let screenshot_url = result
                    .get("screenshot")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                return Ok(SkyCallResult {
                    text,
                    screenshot_url,
                });
            }
        })
        .await
        .map_err(|_| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("sky").expect("id"),
                format!("sky {method} timed out after {}s", timeout_secs()),
            )
        })?
    }
}

pub async fn call_sky(
    method: &str,
    args: serde_json::Value,
) -> Result<SkyCallResult, xai_tool_runtime::ToolError> {
    let mut slot = rpc_slot().lock().await;
    if slot.as_mut().is_some_and(|rpc| !rpc.is_alive()) {
        *slot = None;
    }
    if slot.is_none() {
        *slot = Some(SkyRpc::spawn()?);
    }
    slot.as_mut()
        .expect("rpc just installed")
        .call(method, args)
        .await
}

pub fn path_from_file_url(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if rest.is_empty() {
        return None;
    }
    Some(PathBuf::from(rest))
}

pub fn load_screenshot(url: &str) -> Option<(String, String, String)> {
    if let Some(caps) = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
    {
        return Some((caps.0.to_owned(), caps.1.to_owned(), String::new()));
    }
    let path = path_from_file_url(url)?;
    let bytes = std::fs::read(&path).ok()?;
    Some((
        mime_for_path(&path).to_owned(),
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
        path.display().to_string(),
    ))
}

fn mime_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "image/png",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_file_urls() {
        assert_eq!(
            path_from_file_url("file:///tmp/shot.png").as_deref(),
            Some(Path::new("/tmp/shot.png"))
        );
        assert_eq!(path_from_file_url("https://example.com/a.png"), None);
    }

    #[test]
    fn parses_data_urls() {
        let loaded = load_screenshot("data:image/png;base64,abcd").expect("data url");
        assert_eq!(loaded.0, "image/png");
        assert_eq!(loaded.1, "abcd");
    }
}
