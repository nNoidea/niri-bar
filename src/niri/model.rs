use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LogicalOutput {
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub scale: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutputInfo {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub make: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub physical_size: Option<[i32; 2]>,
    #[serde(default)]
    pub logical: Option<LogicalOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WorkspaceInfo {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub is_focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WindowLayout {
    #[serde(default)]
    pub pos_in_scrolling_layout: Option<(usize, usize)>,
}

fn truncate_opt(s: Option<String>) -> Option<String> {
    s.map(|v| {
        if v.len() <= 512 {
            v
        } else {
            v.chars().take(512).collect()
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WindowInfo {
    #[serde(default)]
    pub id: u64,
    #[serde(default, deserialize_with = "de_truncate_opt")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "de_truncate_opt")]
    pub app_id: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<u64>,
    #[serde(default)]
    pub is_focused: bool,
    #[serde(default)]
    pub layout: Option<WindowLayout>,
}

fn de_truncate_opt<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(d)?;
    Ok(truncate_opt(opt))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    WorkspacesChanged {
        workspaces: Vec<WorkspaceInfo>,
    },
    WorkspaceActivated {
        id: u64,
        focused: bool,
    },
    OutputsChanged {
        outputs: Option<HashMap<String, OutputInfo>>,
    },
    WindowsChanged {
        windows: Vec<WindowInfo>,
    },
    WindowOpenedOrChanged {
        window: WindowInfo,
    },
    WindowClosed {
        id: u64,
    },
    WindowFocusChanged {
        id: Option<u64>,
    },
    WorkspaceActiveWindowChanged {
        workspace_id: u64,
        active_window_id: Option<u64>,
    },
    WindowLayoutsChanged {
        changes: Vec<(u64, WindowLayout)>,
    },
    Other,
}

impl Event {
    pub fn privacy_summary(&self) -> String {
        match self {
            Event::WorkspacesChanged { workspaces } => {
                format!("WorkspacesChanged(count: {})", workspaces.len())
            }
            Event::WorkspaceActivated { id, focused } => {
                format!("WorkspaceActivated(id: {}, focused: {})", id, focused)
            }
            Event::OutputsChanged { outputs } => {
                let count = outputs.as_ref().map(|o| o.len()).unwrap_or(0);
                format!("OutputsChanged(count: {})", count)
            }
            Event::WindowsChanged { windows } => {
                format!("WindowsChanged(count: {})", windows.len())
            }
            Event::WindowOpenedOrChanged { window } => {
                format!(
                    "WindowOpenedOrChanged(id: {}, app: {:?}, ws: {:?}, focused: {})",
                    window.id,
                    window.app_id.as_deref().unwrap_or("unknown"),
                    window.workspace_id,
                    window.is_focused
                )
            }
            Event::WindowClosed { id } => format!("WindowClosed(id: {})", id),
            Event::WindowFocusChanged { id } => format!("WindowFocusChanged(id: {:?})", id),
            Event::WorkspaceActiveWindowChanged {
                workspace_id,
                active_window_id,
            } => {
                format!(
                    "WorkspaceActiveWindowChanged(ws: {}, active_window: {:?})",
                    workspace_id, active_window_id
                )
            }
            Event::WindowLayoutsChanged { changes } => {
                format!("WindowLayoutsChanged(count: {})", changes.len())
            }
            Event::Other => "Other".to_string(),
        }
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct EventVisitor;

        impl<'de> serde::de::Visitor<'de> for EventVisitor {
            type Value = Event;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a Niri event map")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                #[derive(Deserialize)]
                struct WorkspacesPayload {
                    workspaces: Vec<WorkspaceInfo>,
                }
                #[derive(Deserialize)]
                struct WorkspaceActivatedPayload {
                    id: u64,
                    focused: bool,
                }
                #[derive(Deserialize)]
                struct OutputsPayload {
                    outputs: Option<HashMap<String, OutputInfo>>,
                }
                #[derive(Deserialize)]
                struct WindowsPayload {
                    windows: Vec<WindowInfo>,
                }
                #[derive(Deserialize)]
                struct WindowOpenedOrChangedPayload {
                    window: WindowInfo,
                }
                #[derive(Deserialize)]
                struct WindowClosedPayload {
                    id: u64,
                }
                #[derive(Deserialize)]
                struct WindowFocusChangedPayload {
                    id: Option<u64>,
                }
                #[derive(Deserialize)]
                struct WorkspaceActiveWindowChangedPayload {
                    workspace_id: u64,
                    active_window_id: Option<u64>,
                }
                #[derive(Deserialize)]
                struct WindowLayoutsChangedPayload {
                    changes: Vec<(u64, WindowLayout)>,
                }

                let mut result = Event::Other;

                while let Some(key) = map.next_key::<&str>()? {
                    match key {
                        "WorkspacesChanged" => {
                            let p: WorkspacesPayload = map.next_value()?;
                            result = Event::WorkspacesChanged {
                                workspaces: p.workspaces,
                            };
                        }
                        "WorkspaceActivated" => {
                            let p: WorkspaceActivatedPayload = map.next_value()?;
                            result = Event::WorkspaceActivated {
                                id: p.id,
                                focused: p.focused,
                            };
                        }
                        "OutputsChanged" => {
                            let p: OutputsPayload = map.next_value()?;
                            result = Event::OutputsChanged { outputs: p.outputs };
                        }
                        "WindowsChanged" => {
                            let p: WindowsPayload = map.next_value()?;
                            result = Event::WindowsChanged { windows: p.windows };
                        }
                        "WindowOpenedOrChanged" => {
                            let p: WindowOpenedOrChangedPayload = map.next_value()?;
                            result = Event::WindowOpenedOrChanged { window: p.window };
                        }
                        "WindowClosed" => {
                            let p: WindowClosedPayload = map.next_value()?;
                            result = Event::WindowClosed { id: p.id };
                        }
                        "WindowFocusChanged" => {
                            let p: WindowFocusChangedPayload = map.next_value()?;
                            result = Event::WindowFocusChanged { id: p.id };
                        }
                        "WorkspaceActiveWindowChanged" => {
                            let p: WorkspaceActiveWindowChangedPayload = map.next_value()?;
                            result = Event::WorkspaceActiveWindowChanged {
                                workspace_id: p.workspace_id,
                                active_window_id: p.active_window_id,
                            };
                        }
                        "WindowLayoutsChanged" => {
                            let p: WindowLayoutsChangedPayload = map.next_value()?;
                            result = Event::WindowLayoutsChanged { changes: p.changes };
                        }
                        _ => {
                            let _ = map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }

                Ok(result)
            }
        }

        deserializer.deserialize_map(EventVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_info_lean_fields() {
        let ws = WorkspaceInfo {
            id: 1,
            output: Some("eDP-1".to_string()),
            is_active: true,
            is_focused: true,
        };
        assert_eq!(ws.id, 1);
        assert_eq!(ws.output.as_deref(), Some("eDP-1"));
        assert!(ws.is_active);
        assert!(ws.is_focused);
    }

    #[test]
    fn test_deserialize_workspaces_changed() {
        let json = r#"{"WorkspacesChanged":{"workspaces":[{"id":1,"output":"eDP-1","is_active":true,"is_focused":true,"active_window_id":null}]}}"#;
        let event: Event = serde_json::from_str(json).unwrap();
        match event {
            Event::WorkspacesChanged { workspaces } => {
                assert_eq!(workspaces.len(), 1);
                assert_eq!(workspaces[0].id, 1);
                assert_eq!(workspaces[0].output.as_deref(), Some("eDP-1"));
            }
            _ => panic!("Expected WorkspacesChanged variant"),
        }
    }

    #[test]
    fn test_deserialize_workspace_activated() {
        let json = r#"{"WorkspaceActivated":{"id":2,"focused":true}}"#;
        let event: Event = serde_json::from_str(json).unwrap();
        match event {
            Event::WorkspaceActivated { id, focused } => {
                assert_eq!(id, 2);
                assert!(focused);
            }
            _ => panic!("Expected WorkspaceActivated variant"),
        }
    }

    #[test]
    fn test_deserialize_window_layouts_changed() {
        let json = r#"{"WindowLayoutsChanged":{"changes":[[42,{"pos_in_scrolling_layout":[1,0]}]]}}"#;
        let event: Event = serde_json::from_str(json).unwrap();
        match event {
            Event::WindowLayoutsChanged { changes } => {
                assert_eq!(changes.len(), 1);
                assert_eq!(changes[0].0, 42);
                assert_eq!(changes[0].1.pos_in_scrolling_layout, Some((1, 0)));
            }
            _ => panic!("Expected WindowLayoutsChanged variant"),
        }
    }

    #[test]
    fn test_deserialize_unknown_event_to_other() {
        let json = r#"{"FutureNiriEvent":{"unknown_field":123}}"#;
        let event: Event = serde_json::from_str(json).unwrap();
        match event {
            Event::Other => {}
            _ => panic!("Expected Other variant for unknown event"),
        }
    }

    #[test]
    fn test_deserialize_window_opened_or_changed() {
        let json = r#"{"WindowOpenedOrChanged":{"window":{"id":100,"title":"Terminal","app_id":"kitty","workspace_id":1,"is_focused":true,"layout":{"pos_in_scrolling_layout":[0,0]}}}}"#;
        let event: Event = serde_json::from_str(json).unwrap();
        match event {
            Event::WindowOpenedOrChanged { window } => {
                assert_eq!(window.id, 100);
                assert_eq!(window.title.as_deref(), Some("Terminal"));
                assert_eq!(window.app_id.as_deref(), Some("kitty"));
                assert_eq!(window.workspace_id, Some(1));
                assert!(window.is_focused);
                assert_eq!(window.layout.unwrap().pos_in_scrolling_layout, Some((0, 0)));
            }
            _ => panic!("Expected WindowOpenedOrChanged variant"),
        }
    }

    #[test]
    fn test_deserialize_window_closed() {
        let json = r#"{"WindowClosed":{"id":100}}"#;
        let event: Event = serde_json::from_str(json).unwrap();
        match event {
            Event::WindowClosed { id } => {
                assert_eq!(id, 100);
            }
            _ => panic!("Expected WindowClosed variant"),
        }
    }

    #[test]
    fn test_deserialize_window_focus_changed() {
        let json = r#"{"WindowFocusChanged":{"id":100}}"#;
        let event: Event = serde_json::from_str(json).unwrap();
        match event {
            Event::WindowFocusChanged { id } => {
                assert_eq!(id, Some(100));
            }
            _ => panic!("Expected WindowFocusChanged variant"),
        }

        let json_null = r#"{"WindowFocusChanged":{"id":null}}"#;
        let event_null: Event = serde_json::from_str(json_null).unwrap();
        match event_null {
            Event::WindowFocusChanged { id } => {
                assert_eq!(id, None);
            }
            _ => panic!("Expected WindowFocusChanged variant"),
        }
    }

    #[test]
    fn test_deserialize_workspace_active_window_changed() {
        let json = r#"{"WorkspaceActiveWindowChanged":{"workspace_id":2,"active_window_id":55}}"#;
        let event: Event = serde_json::from_str(json).unwrap();
        match event {
            Event::WorkspaceActiveWindowChanged {
                workspace_id,
                active_window_id,
            } => {
                assert_eq!(workspace_id, 2);
                assert_eq!(active_window_id, Some(55));
            }
            _ => panic!("Expected WorkspaceActiveWindowChanged variant"),
        }
    }

    #[test]
    fn test_deserialize_outputs_changed() {
        let json = r#"{"OutputsChanged":{"outputs":{"DP-1":{"name":"DP-1","make":"Dell","model":"U2720Q","physical_size":[600,340],"logical":{"x":0,"y":0,"width":3840,"height":2160,"scale":1.5}}}}}"#;
        let event: Event = serde_json::from_str(json).unwrap();
        match event {
            Event::OutputsChanged { outputs } => {
                let map = outputs.unwrap();
                assert_eq!(map.len(), 1);
                let out = map.get("DP-1").unwrap();
                assert_eq!(out.name.as_deref(), Some("DP-1"));
                assert_eq!(out.make.as_deref(), Some("Dell"));
                let logical = out.logical.as_ref().unwrap();
                assert_eq!(logical.width, 3840);
                assert_eq!(logical.scale, Some(1.5));
            }
            _ => panic!("Expected OutputsChanged variant"),
        }
    }

    #[test]
    fn test_privacy_summary() {
        let closed = Event::WindowClosed { id: 10 };
        assert_eq!(closed.privacy_summary(), "WindowClosed(id: 10)");

        let activated = Event::WorkspaceActivated { id: 1, focused: true };
        assert_eq!(activated.privacy_summary(), "WorkspaceActivated(id: 1, focused: true)");

        let other = Event::Other;
        assert_eq!(other.privacy_summary(), "Other");
    }
}
