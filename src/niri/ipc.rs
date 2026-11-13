use crate::niri::model::{Event, OutputInfo, WindowInfo, WorkspaceInfo};
use serde_json::json;
use std::collections::HashMap;
use std::env;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

const MAX_IPC_LINE: u64 = 8 * 1024 * 1024;
const IPC_TIMEOUT: Duration = Duration::from_millis(1000);

/// Niri socket path from `$NIRI_SOCKET` (set by Niri for its clients).
pub fn get_socket_path() -> Option<String> {
    env::var("NIRI_SOCKET").ok()
}

#[derive(serde::Deserialize)]
enum NiriResponse<R> {
    Ok(R),
    Err(String),
}

/// Encode an action payload as a newline-terminated Niri request.
pub fn build_action_request(action_payload: serde_json::Value) -> String {
    let req = json!({ "Action": action_payload });
    format!("{req}\n")
}

/// Parse a `{"Ok": …}` / `{"Err": …}` Niri response line.
/// Parse errors include a 500-char snippet (never unbounded payloads/titles).
pub fn parse_niri_response<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, String> {
    match serde_json::from_str::<NiriResponse<T>>(line.trim()) {
        Ok(NiriResponse::Ok(val)) => Ok(val),
        Ok(NiriResponse::Err(err)) => Err(format!("Niri error: {err}")),
        Err(e) => {
            // Truncate: responses can be large (window lists) and may contain
            // window titles; never dump unbounded payloads into logs/errors.
            let snippet: String = line.trim().chars().take(500).collect();
            Err(format!("Failed to parse response: {e}. Line: {snippet}"))
        }
    }
}

async fn write_and_read_line<R: tokio::io::AsyncBufRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    req_bytes: &[u8],
    line: &mut String,
    label: &str,
) -> Result<(), String> {
    writer
        .write_all(req_bytes)
        .await
        .map_err(|e| format!("Failed to send {label} to Niri socket: {e}"))?;
    writer
        .flush()
        .await
        .map_err(|e| format!("Failed to flush Niri socket: {e}"))?;
    line.clear();
    reader
        .take(MAX_IPC_LINE)
        .read_line(line)
        .await
        .map_err(|e| format!("Failed to read {label} response from Niri socket: {e}"))?;
    if line.len() >= MAX_IPC_LINE as usize {
        return Err(format!("{label} response exceeds 8MB cap"));
    }
    Ok(())
}

pub(crate) async fn execute_actions_at(socket_path: &str, action_payloads: &[serde_json::Value]) -> Result<(), String> {
    tokio::time::timeout(IPC_TIMEOUT, async {
        let mut stream = tokio::net::UnixStream::connect(socket_path)
            .await
            .map_err(|e| format!("Failed to connect to Niri socket at {socket_path}: {e}"))?;
        let (rd, mut wr) = stream.split();
        let mut reader = tokio::io::BufReader::new(rd);
        let mut line = String::new();

        for payload in action_payloads {
            let req_str = build_action_request(payload.clone());
            write_and_read_line(&mut reader, &mut wr, req_str.as_bytes(), &mut line, "action").await?;
            let _: serde_json::Value = parse_niri_response(&line)?;
        }
        Ok(())
    })
    .await
    .map_err(|_| "Timed out executing action on Niri socket".to_string())?
}

async fn execute_actions(action_payloads: &[serde_json::Value]) -> Result<(), String> {
    let socket_path = get_socket_path().ok_or_else(|| "NIRI_SOCKET environment variable not set".to_string())?;
    execute_actions_at(&socket_path, action_payloads).await
}

async fn execute_action(action_payload: serde_json::Value) -> Result<(), String> {
    execute_actions(&[action_payload]).await
}

pub async fn focus_window(id: u64) -> Result<(), String> {
    execute_action(json!({ "FocusWindow": { "id": id } })).await
}

pub async fn close_window(id: u64) -> Result<(), String> {
    execute_action(json!({ "CloseWindow": { "id": id } })).await
}

pub async fn move_window_to_column(id: u64, index: usize) -> Result<(), String> {
    // One connection for focus+move: halves timeout exposure and shrinks
    // the focus-change race window (server-side focus is still not atomic).
    execute_actions(&[
        json!({ "FocusWindow": { "id": id } }),
        json!({ "MoveColumnToIndex": { "index": index } }),
    ])
    .await
}

pub async fn move_window_to_monitor(id: u64, output: &str, target_slot: Option<usize>) -> Result<(), String> {
    // One connection for focus+move(+index).
    let mut actions = vec![
        json!({ "FocusWindow": { "id": id } }),
        json!({ "MoveColumnToMonitor": { "output": output } }),
    ];
    if let Some(slot) = target_slot {
        actions.push(json!({ "MoveColumnToIndex": { "index": slot } }));
    }
    execute_actions(&actions).await
}

pub(crate) async fn fetch_niri_resource_at<T: serde::de::DeserializeOwned>(
    socket_path: &str,
    resource_name: &str,
) -> Result<T, String> {
    let line = tokio::time::timeout(IPC_TIMEOUT, async {
        let mut stream = tokio::net::UnixStream::connect(socket_path)
            .await
            .map_err(|e| format!("Failed to connect to Niri socket at {socket_path}: {e}"))?;
        let (rd, mut wr) = stream.split();
        let mut reader = tokio::io::BufReader::new(rd);
        let mut line = String::new();
        let req = format!("\"{resource_name}\"\n");
        write_and_read_line(&mut reader, &mut wr, req.as_bytes(), &mut line, resource_name).await?;
        Ok::<String, String>(line)
    })
    .await
    .map_err(|_| format!("Timed out fetching {resource_name} from Niri socket"))??;

    parse_niri_response(&line)
}

async fn fetch_niri_resource<T: serde::de::DeserializeOwned>(resource_name: &str) -> Result<T, String> {
    let socket_path = get_socket_path().ok_or_else(|| "NIRI_SOCKET environment variable not set".to_string())?;
    fetch_niri_resource_at(&socket_path, resource_name).await
}

pub async fn fetch_outputs() -> Result<HashMap<String, OutputInfo>, String> {
    #[derive(serde::Deserialize)]
    struct OkPayload {
        #[serde(rename = "Outputs")]
        outputs: Option<HashMap<String, OutputInfo>>,
    }

    let payload: OkPayload = fetch_niri_resource("Outputs").await?;
    Ok(payload.outputs.unwrap_or_default())
}

pub async fn fetch_windows() -> Result<Vec<WindowInfo>, String> {
    #[derive(serde::Deserialize)]
    struct OkPayload {
        #[serde(rename = "Windows")]
        windows: Option<Vec<WindowInfo>>,
    }

    let payload: OkPayload = fetch_niri_resource("Windows").await?;
    Ok(payload.windows.unwrap_or_default())
}

pub async fn fetch_workspaces() -> Result<Vec<WorkspaceInfo>, String> {
    #[derive(serde::Deserialize)]
    struct OkPayload {
        #[serde(rename = "Workspaces")]
        workspaces: Option<Vec<WorkspaceInfo>>,
    }

    let payload: OkPayload = fetch_niri_resource("Workspaces").await?;
    Ok(payload.workspaces.unwrap_or_default())
}

pub async fn open_event_stream() -> Result<tokio::io::Lines<tokio::io::BufReader<tokio::net::UnixStream>>, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let socket_path = get_socket_path().ok_or_else(|| "NIRI_SOCKET environment variable not set".to_string())?;
    let mut stream = tokio::net::UnixStream::connect(&socket_path)
        .await
        .map_err(|e| format!("Failed to connect to Niri socket at {}: {}", socket_path, e))?;

    stream
        .write_all(b"\"EventStream\"\n")
        .await
        .map_err(|e| format!("Failed to send EventStream request to Niri socket: {}", e))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("Failed to flush Niri socket stream: {}", e))?;

    let reader = tokio::io::BufReader::new(stream);
    Ok(reader.lines())
}

pub fn parse_event_line(line_str: &str) -> Option<Event> {
    let trimmed = line_str.trim();
    if trimmed.is_empty() {
        return None;
    }
    match serde_json::from_str::<Event>(trimmed) {
        Ok(event) => Some(event),
        Err(e) => {
            let snippet: String = trimmed.chars().take(500).collect();
            log_warn!("niri", "Failed to parse IPC event: {}. Payload: {}", e, snippet);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_action_request() {
        let focus_req = build_action_request(json!({ "FocusWindow": { "id": 42 } }));
        assert_eq!(focus_req, "{\"Action\":{\"FocusWindow\":{\"id\":42}}}\n");

        let move_req = build_action_request(json!({ "MoveColumnToMonitor": { "output": "eDP-1" } }));
        assert_eq!(
            move_req,
            "{\"Action\":{\"MoveColumnToMonitor\":{\"output\":\"eDP-1\"}}}\n"
        );
    }

    #[test]
    fn test_parse_niri_response_ok() {
        let line =
            "{\"Ok\": {\"Windows\": [{\"id\": 1, \"is_focused\": true, \"layout\": null, \"workspace_id\": 1}]}}\n";
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct WinList {
            #[serde(rename = "Windows")]
            windows: Vec<WindowInfo>,
        }
        let parsed: WinList = parse_niri_response(line).expect("Should parse Ok response");
        assert_eq!(parsed.windows.len(), 1);
        assert_eq!(parsed.windows[0].id, 1);
    }

    #[test]
    fn test_parse_niri_response_err() {
        let line = "{\"Err\": \"no window with id 99\"}";
        let parsed: Result<serde_json::Value, String> = parse_niri_response(line);
        assert!(parsed.is_err());
        assert!(parsed.unwrap_err().contains("no window with id 99"));
    }

    #[test]
    fn test_parse_niri_response_malformed() {
        let line = "invalid json payload";
        let parsed: Result<serde_json::Value, String> = parse_niri_response(line);
        assert!(parsed.is_err());
        assert!(parsed.unwrap_err().contains("Failed to parse response"));
    }

    #[test]
    fn test_execute_actions_async_on_unix_socket() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = std::env::temp_dir().join(format!("niri-bar-ipc-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock_path = dir.join("niri.sock");
        let _ = std::fs::remove_file(&sock_path);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (rd, mut wr) = stream.into_split();
                let mut lines = BufReader::new(rd).lines();
                let mut received = Vec::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    received.push(line);
                    wr.write_all(b"{\"Ok\":\"Handled\"}\n").await.unwrap();
                }
                received
            });

            execute_actions_at(
                &sock_path.to_string_lossy(),
                &[
                    json!({ "FocusWindow": { "id": 7 } }),
                    json!({ "MoveColumnToIndex": { "index": 3 } }),
                ],
            )
            .await
            .expect("Async execute_actions_at must succeed");

            let received = server.await.unwrap();
            assert_eq!(received.len(), 2);
            assert_eq!(received[0], "{\"Action\":{\"FocusWindow\":{\"id\":7}}}");
            assert_eq!(received[1], "{\"Action\":{\"MoveColumnToIndex\":{\"index\":3}}}");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_fetch_niri_resource_at_unix_socket() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = std::env::temp_dir().join(format!("niri-bar-ipc-fetch-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock_path = dir.join("niri.sock");
        let _ = std::fs::remove_file(&sock_path);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (rd, mut wr) = stream.into_split();
                let mut lines = BufReader::new(rd).lines();
                let req = lines.next_line().await.unwrap().unwrap();
                wr.write_all(b"{\"Ok\":{\"Workspaces\":[{\"id\":9,\"idx\":1,\"output\":\"eDP-1\",\"is_active\":true,\"is_focused\":true}]}}\n")
                    .await
                    .unwrap();
                req
            });

            #[derive(serde::Deserialize)]
            struct Payload {
                #[serde(rename = "Workspaces")]
                workspaces: Vec<WorkspaceInfo>,
            }
            let fetched: Payload = fetch_niri_resource_at(&sock_path.to_string_lossy(), "Workspaces")
                .await
                .expect("fetch_niri_resource_at must succeed");
            assert_eq!(fetched.workspaces.len(), 1);
            assert_eq!(fetched.workspaces[0].id, 9);
            assert_eq!(server.await.unwrap(), "\"Workspaces\"");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
