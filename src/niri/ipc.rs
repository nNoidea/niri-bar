use crate::niri::model::{Event, OutputInfo, WindowInfo, WorkspaceInfo};
use serde_json::json;
use std::collections::HashMap;
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Niri socket path from `$NIRI_SOCKET` (set by Niri for its clients).
pub fn get_socket_path() -> Option<String> {
    env::var("NIRI_SOCKET").ok()
}

fn connect_socket() -> Result<UnixStream, String> {
    let socket_path = get_socket_path().ok_or_else(|| "NIRI_SOCKET environment variable not set".to_string())?;
    let stream = UnixStream::connect(&socket_path)
        .map_err(|e| format!("Failed to connect to Niri socket at {}: {}", socket_path, e))?;
    let timeout = Duration::from_millis(1000);
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    Ok(stream)
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

fn execute_actions(action_payloads: &[serde_json::Value]) -> Result<(), String> {
    const MAX_LINE: u64 = 8 * 1024 * 1024;
    let mut stream = connect_socket()?;
    for payload in action_payloads {
        let req_str = build_action_request(payload.clone());
        stream
            .write_all(req_str.as_bytes())
            .map_err(|e| format!("Failed to write to Niri socket: {e}"))?;
        stream
            .flush()
            .map_err(|e| format!("Failed to flush Niri socket: {e}"))?;

        // Scoped reader per response so the mutable write borrow ends first.
        let mut line = String::new();
        {
            let reader = BufReader::new(&stream);
            reader
                .take(MAX_LINE)
                .read_line(&mut line)
                .map_err(|e| format!("Failed to read action response from Niri socket: {e}"))?;
        }
        if line.len() >= MAX_LINE as usize {
            return Err("Action response exceeds 8MB cap".to_string());
        }
        let _: serde_json::Value = parse_niri_response(&line)?;
    }
    Ok(())
}

fn execute_action(action_payload: serde_json::Value) -> Result<(), String> {
    execute_actions(&[action_payload])
}

pub fn focus_window(id: u64) -> Result<(), String> {
    execute_action(json!({ "FocusWindow": { "id": id } }))
}

pub fn close_window(id: u64) -> Result<(), String> {
    execute_action(json!({ "CloseWindow": { "id": id } }))
}

pub fn move_window_to_column(id: u64, index: usize) -> Result<(), String> {
    // One connection for focus+move: halves timeout exposure and shrinks
    // the focus-change race window (server-side focus is still not atomic).
    execute_actions(&[
        json!({ "FocusWindow": { "id": id } }),
        json!({ "MoveColumnToIndex": { "index": index } }),
    ])
}

pub fn move_window_to_monitor(id: u64, output: &str, target_slot: Option<usize>) -> Result<(), String> {
    // One connection for focus+move(+index).
    let mut actions = vec![
        json!({ "FocusWindow": { "id": id } }),
        json!({ "MoveColumnToMonitor": { "output": output } }),
    ];
    if let Some(slot) = target_slot {
        actions.push(json!({ "MoveColumnToIndex": { "index": slot } }));
    }
    execute_actions(&actions)
}

fn fetch_niri_resource<T: serde::de::DeserializeOwned>(resource_name: &str) -> Result<T, String> {
    const MAX_LINE: u64 = 8 * 1024 * 1024;
    let mut stream = connect_socket()?;
    let req = format!("\"{resource_name}\"\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("Failed to send {resource_name} request to Niri socket: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("Failed to flush Niri socket: {e}"))?;

    let reader = BufReader::new(&stream);
    let mut line = String::new();
    reader
        .take(MAX_LINE)
        .read_line(&mut line)
        .map_err(|e| format!("Failed to read {resource_name} response from Niri socket: {e}"))?;
    if line.len() >= MAX_LINE as usize {
        return Err(format!("{resource_name} response exceeds 8MB cap"));
    }

    parse_niri_response(&line)
}

pub fn fetch_outputs() -> Result<HashMap<String, OutputInfo>, String> {
    #[derive(serde::Deserialize)]
    struct OkPayload {
        #[serde(rename = "Outputs")]
        outputs: Option<HashMap<String, OutputInfo>>,
    }

    let payload: OkPayload = fetch_niri_resource("Outputs")?;
    Ok(payload.outputs.unwrap_or_default())
}

pub fn fetch_windows() -> Result<Vec<WindowInfo>, String> {
    #[derive(serde::Deserialize)]
    struct OkPayload {
        #[serde(rename = "Windows")]
        windows: Option<Vec<WindowInfo>>,
    }

    let payload: OkPayload = fetch_niri_resource("Windows")?;
    Ok(payload.windows.unwrap_or_default())
}

pub fn fetch_workspaces() -> Result<Vec<WorkspaceInfo>, String> {
    #[derive(serde::Deserialize)]
    struct OkPayload {
        #[serde(rename = "Workspaces")]
        workspaces: Option<Vec<WorkspaceInfo>>,
    }

    let payload: OkPayload = fetch_niri_resource("Workspaces")?;
    Ok(payload.workspaces.unwrap_or_default())
}

pub fn listen_event_stream<F, C>(mut on_event: F, is_running: C) -> Result<(), String>
where
    F: FnMut(Event) + Send + 'static,
    C: Fn() -> bool,
{
    let socket_path = get_socket_path().ok_or_else(|| "NIRI_SOCKET environment variable not set".to_string())?;
    let mut stream = UnixStream::connect(&socket_path)
        .map_err(|e| format!("Failed to connect to Niri socket at {}: {}", socket_path, e))?;

    let timeout = Duration::from_millis(500);
    let _ = stream.set_read_timeout(Some(timeout));

    stream
        .write_all(b"\"EventStream\"\n")
        .map_err(|e| format!("Failed to send EventStream request to Niri socket: {}", e))?;
    stream
        .flush()
        .map_err(|e| format!("Failed to flush Niri socket stream: {}", e))?;

    let reader = BufReader::new(&stream);
    for line in reader.lines() {
        if !is_running() {
            break;
        }

        match line {
            Ok(line_str) => {
                let trimmed = line_str.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Event>(trimmed) {
                    Ok(event) => on_event(event),
                    Err(e) => {
                        let snippet: String = trimmed.chars().take(500).collect();
                        log_warn!("niri", "Failed to parse IPC event: {}. Payload: {}", e, snippet);
                    }
                }
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut {
                    continue;
                }
                log_warn!("niri", "Error reading from Niri socket: {}", e);
                break;
            }
        }
    }

    Ok(())
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
}
