use pulseaudio::protocol::{self, Prop, Props, SubscriptionMask};
use std::ffi::{CStr, CString};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{SinkInfo, VolumeModule};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PulseAction {
    SetVolumeAndUnmute(u32),
    SetMute(bool),
    CycleSink,
}

pub(super) struct PulseSession {
    stream: tokio::net::UnixStream,
    protocol_version: u16,
    seq: u32,
    buf: Vec<u8>,
    pub(super) pending_event: bool,
    default_sink_name: Option<CString>,
    default_channels: u8,
}

impl PulseSession {
    pub(super) async fn connect(stream: tokio::net::UnixStream) -> Result<Self, String> {
        let mut session = Self {
            stream,
            protocol_version: protocol::MAX_VERSION,
            seq: 0,
            buf: Vec::with_capacity(4096),
            pending_event: false,
            default_sink_name: None,
            default_channels: 2,
        };

        let cookie = pulseaudio::cookie_path_from_env()
            .and_then(|p| std::fs::read(p).ok())
            .unwrap_or_default();

        let auth = protocol::AuthParams {
            version: protocol::MAX_VERSION,
            supports_shm: false,
            supports_memfd: false,
            cookie,
        };
        let auth_seq = session.write_cmd(&protocol::Command::Auth(auth)).await?;
        session.wait_for_reply_frame(auth_seq).await?;
        let (_, auth_reply) =
            protocol::read_reply_message::<protocol::AuthReply>(&mut &session.buf[..], protocol::MAX_VERSION)
                .map_err(|e| e.to_string())?;
        session.protocol_version = protocol::MAX_VERSION.min(auth_reply.version);

        let mut props = Props::new();
        props.set(Prop::ApplicationName, c"niri-bar");
        let name_seq = session.write_cmd(&protocol::Command::SetClientName(props)).await?;
        session.wait_for_reply_frame(name_seq).await?;
        let _ = protocol::read_reply_message::<protocol::SetClientNameReply>(
            &mut &session.buf[..],
            session.protocol_version,
        )
        .map_err(|e| e.to_string())?;

        let sub_mask = SubscriptionMask::SINK | SubscriptionMask::SERVER;
        let sub_seq = session.write_cmd(&protocol::Command::Subscribe(sub_mask)).await?;
        session.wait_for_ack(sub_seq).await?;

        Ok(session)
    }

    async fn write_cmd(&mut self, cmd: &protocol::Command) -> Result<u32, String> {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        self.buf.clear();
        protocol::write_command_message(&mut self.buf, seq, cmd, self.protocol_version).map_err(|e| e.to_string())?;
        self.stream.write_all(&self.buf).await.map_err(|e| e.to_string())?;
        Ok(seq)
    }

    pub(super) async fn read_next_frame(&mut self) -> Result<(), String> {
        let mut hdr = [0u8; protocol::DESCRIPTOR_SIZE];
        self.stream.read_exact(&mut hdr).await.map_err(|e| e.to_string())?;
        let desc = protocol::read_descriptor(&mut &hdr[..]).map_err(|e| e.to_string())?;
        let len = desc.length as usize;
        if len > protocol::MAX_MEMBLOCKQ_LENGTH {
            return Err(format!("PulseAudio frame too large: {len} bytes"));
        }
        self.buf.clear();
        self.buf.reserve(protocol::DESCRIPTOR_SIZE + len);
        self.buf.extend_from_slice(&hdr);
        self.buf.resize(protocol::DESCRIPTOR_SIZE + len, 0);
        self.stream
            .read_exact(&mut self.buf[protocol::DESCRIPTOR_SIZE..])
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn wait_for_reply_frame(&mut self, expected_seq: u32) -> Result<(), String> {
        loop {
            self.read_next_frame().await?;
            let mut payload = &self.buf[protocol::DESCRIPTOR_SIZE..];
            let mut ts = protocol::TagStructReader::new(&mut payload, self.protocol_version);
            let cmd_tag: protocol::CommandTag = ts.read_enum().map_err(|e| e.to_string())?;
            let seq = ts.read_u32().map_err(|e| e.to_string())?;
            match cmd_tag {
                protocol::CommandTag::SubscribeEvent => {
                    self.pending_event = true;
                }
                protocol::CommandTag::Reply if seq == expected_seq => return Ok(()),
                protocol::CommandTag::Error if seq == expected_seq => {
                    let err: protocol::PulseError = ts.read_enum().map_err(|e| e.to_string())?;
                    return Err(format!("PulseAudio server error: {err}"));
                }
                other => {
                    log_debug!("volume", "Ignoring PulseAudio frame {other:?} (seq {seq})");
                }
            }
        }
    }

    async fn wait_for_ack(&mut self, expected_seq: u32) -> Result<(), String> {
        self.wait_for_reply_frame(expected_seq).await?;
        let _ = protocol::read_ack_message(&mut &self.buf[..]).map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn query_sinks_and_default(&mut self) -> Result<(protocol::SinkInfoList, Option<CString>), String> {
        let srv_seq = self.write_cmd(&protocol::Command::GetServerInfo).await?;
        self.wait_for_reply_frame(srv_seq).await?;
        let (_, srv_info) =
            protocol::read_reply_message::<protocol::ServerInfo>(&mut &self.buf[..], self.protocol_version)
                .map_err(|e| e.to_string())?;

        let list_seq = self.write_cmd(&protocol::Command::GetSinkInfoList).await?;
        self.wait_for_reply_frame(list_seq).await?;
        let (_, sinks) =
            protocol::read_reply_message::<protocol::SinkInfoList>(&mut &self.buf[..], self.protocol_version)
                .map_err(|e| e.to_string())?;

        Ok((sinks, srv_info.default_sink_name))
    }

    pub(super) async fn fetch_state(&mut self) -> Result<(u32, bool, SinkInfo), String> {
        let (sinks, default_name) = self.query_sinks_and_default().await?;
        let selected = default_name
            .as_deref()
            .and_then(|def| sinks.iter().find(|s| s.name.as_c_str() == def))
            .or_else(|| sinks.first())
            .ok_or_else(|| "PulseAudio returned no sinks".to_string())?;

        self.default_sink_name = Some(selected.name.clone());
        let ch = selected.cvolume.channels().len();
        self.default_channels = u8::try_from(ch).unwrap_or(2).max(1);

        Ok(VolumeModule::from_pulse_sink(selected))
    }

    pub(super) async fn set_default_mute(&mut self, mute: bool) -> Result<(), String> {
        let sink_name = self
            .default_sink_name
            .clone()
            .unwrap_or_else(|| protocol::DEFAULT_SINK.to_owned());
        let cmd = protocol::Command::SetSinkMute(protocol::SetDeviceMuteParams {
            device_index: None,
            device_name: Some(sink_name),
            mute,
        });
        let seq = self.write_cmd(&cmd).await?;
        self.wait_for_ack(seq).await
    }

    pub(super) async fn handle_action(&mut self, action: PulseAction) -> Result<(), String> {
        match action {
            PulseAction::SetVolumeAndUnmute(target_vol) => {
                self.set_default_mute(false).await?;
                let sink_name = self
                    .default_sink_name
                    .clone()
                    .unwrap_or_else(|| protocol::DEFAULT_SINK.to_owned());
                let cmd = protocol::Command::SetSinkVolume(protocol::SetDeviceVolumeParams {
                    device_index: None,
                    device_name: Some(sink_name),
                    volume: VolumeModule::pct_to_channel_volume(target_vol, self.default_channels),
                });
                let seq = self.write_cmd(&cmd).await?;
                self.wait_for_ack(seq).await
            }
            PulseAction::SetMute(mute) => self.set_default_mute(mute).await,
            PulseAction::CycleSink => {
                let (sinks, default_name) = self.query_sinks_and_default().await?;
                if let Some(next_name) = VolumeModule::next_sink_name(&sinks, default_name.as_deref()) {
                    let seq = self.write_cmd(&protocol::Command::SetDefaultSink(next_name)).await?;
                    self.wait_for_ack(seq).await?;
                }
                Ok(())
            }
        }
    }
}

impl VolumeModule {
    pub fn resolve_pulse_socket() -> Option<PathBuf> {
        if let Some(p) = pulseaudio::socket_path_from_env() {
            return Some(p);
        }
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            let candidate = PathBuf::from(xdg).join("pulse/native");
            if candidate.exists() {
                return Some(candidate);
            }
        }
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("Uid:") {
                    if let Some(uid_str) = rest.split_whitespace().next() {
                        let candidate = PathBuf::from(format!("/run/user/{uid_str}/pulse/native"));
                        if candidate.exists() {
                            return Some(candidate);
                        }
                    }
                }
            }
        }
        None
    }

    pub fn pct_to_channel_volume(pct: u32, num_channels: u8) -> protocol::ChannelVolume {
        let channels = num_channels.clamp(1, 32);
        let norm = f64::from(protocol::Volume::NORM.as_u32());
        let raw = ((f64::from(pct) / 100.0) * norm).round() as u32;
        let vol = protocol::Volume::from_u32_clamped(raw);
        let mut cv = protocol::ChannelVolume::empty();
        for _ in 0..channels {
            cv.push(vol);
        }
        cv
    }

    pub fn channel_volume_to_pct(cv: &protocol::ChannelVolume) -> u32 {
        let chans = cv.channels();
        if chans.is_empty() {
            return 0;
        }
        let sum: u64 = chans.iter().map(|v| u64::from(v.as_u32())).sum();
        let avg = sum as f64 / chans.len() as f64;
        let norm = f64::from(protocol::Volume::NORM.as_u32());
        ((avg / norm) * 100.0).round() as u32
    }

    fn prop_str(props: &Props, prop: Prop) -> Option<String> {
        let bytes = props.get(prop)?;
        let trimmed = bytes.strip_suffix(&[0]).unwrap_or(bytes);
        let s = String::from_utf8_lossy(trimmed).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    pub fn from_pulse_sink(pa_sink: &protocol::SinkInfo) -> (u32, bool, SinkInfo) {
        let vol = Self::channel_volume_to_pct(&pa_sink.cvolume);
        let muted = pa_sink.muted;
        let name = pa_sink.name.to_string_lossy().into_owned();
        let description = pa_sink
            .description
            .as_ref()
            .map(|d| d.to_string_lossy().trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| Self::prop_str(&pa_sink.props, Prop::DeviceDescription))
            .unwrap_or_else(|| name.clone());

        let form_factor = Self::prop_str(&pa_sink.props, Prop::DeviceFormFactor);
        let icon_name = Self::prop_str(&pa_sink.props, Prop::DeviceIconName);
        let bus = Self::prop_str(&pa_sink.props, Prop::DeviceBus);
        let active_port = pa_sink
            .ports
            .get(pa_sink.active_port)
            .map(|p| p.name.to_string_lossy().into_owned());

        let (sink_type, is_bluetooth) = Self::classify_sink(
            &name,
            &description,
            form_factor.as_deref(),
            icon_name.as_deref(),
            bus.as_deref(),
            active_port.as_deref(),
        );

        (
            vol,
            muted,
            SinkInfo {
                name,
                description,
                sink_type,
                is_bluetooth,
            },
        )
    }

    pub fn next_sink_name(sinks: &[protocol::SinkInfo], current_default: Option<&CStr>) -> Option<CString> {
        if sinks.is_empty() {
            return None;
        }
        let cur_idx = current_default
            .and_then(|def| sinks.iter().position(|s| s.name.as_c_str() == def))
            .unwrap_or(0);
        let next_idx = (cur_idx + 1) % sinks.len();
        Some(sinks[next_idx].name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::volume::SinkType;

    #[test]
    fn test_pulse_volume_conversions() {
        for pct in [0u32, 1, 25, 43, 50, 80, 100, 150] {
            let cv = VolumeModule::pct_to_channel_volume(pct, 2);
            assert_eq!(cv.channels().len(), 2);
            assert_eq!(VolumeModule::channel_volume_to_pct(&cv), pct);
        }
    }

    #[test]
    fn test_from_pulse_sink_extracts_metadata_and_volume() {
        use pulseaudio::protocol::port_info::*;

        let mut props = Props::new();
        props.set(Prop::DeviceFormFactor, c"headphone");
        props.set(Prop::DeviceBus, c"bluetooth");
        props.set(Prop::DeviceIconName, c"audio-headphones-bluetooth");

        let pa_sink = protocol::SinkInfo {
            index: 42,
            name: c"bluez_output.01_02.1".to_owned(),
            description: Some(c"WH-1000XM4".to_owned()),
            props,
            cvolume: VolumeModule::pct_to_channel_volume(75, 2),
            muted: true,
            ports: vec![PortInfo {
                name: c"headphone-output".to_owned(),
                description: None,
                priority: 100,
                available: PortAvailable::Yes,
                dir: PortDirection::Input,
                availability_group: None,
                port_type: PortType::Headphones,
            }],
            active_port: 0,
            ..Default::default()
        };

        let (vol, muted, info) = VolumeModule::from_pulse_sink(&pa_sink);
        assert_eq!(vol, 75);
        assert!(muted);
        assert_eq!(info.name, "bluez_output.01_02.1");
        assert_eq!(info.description, "WH-1000XM4");
        assert_eq!(info.sink_type, SinkType::Bluetooth);
        assert!(info.is_bluetooth);
    }

    #[test]
    fn test_next_sink_name_cycles_around() {
        let empty: Vec<protocol::SinkInfo> = Vec::new();
        assert_eq!(VolumeModule::next_sink_name(&empty, None), None);

        let s1 = protocol::SinkInfo {
            name: c"sink_a".to_owned(),
            ..Default::default()
        };
        let s2 = protocol::SinkInfo {
            name: c"sink_b".to_owned(),
            ..Default::default()
        };
        let sinks = vec![s1, s2];

        assert_eq!(
            VolumeModule::next_sink_name(&sinks, Some(c"sink_a")),
            Some(c"sink_b".to_owned())
        );
        assert_eq!(
            VolumeModule::next_sink_name(&sinks, Some(c"sink_b")),
            Some(c"sink_a".to_owned())
        );
        assert_eq!(
            VolumeModule::next_sink_name(&sinks, Some(c"unknown")),
            Some(c"sink_b".to_owned())
        );
    }

    #[test]
    fn test_pulse_session_over_unix_stream_pair() {
        crate::runtime::handle().block_on(async move {
            let (client_sock, mut server_sock) =
                tokio::net::UnixStream::pair().expect("UnixStream::pair must succeed in tests");

            let server_task = crate::runtime::spawn(async move {
                let ver = protocol::MAX_VERSION;
                let mut hdr = [0u8; protocol::DESCRIPTOR_SIZE];

                // 1. Auth
                server_sock.read_exact(&mut hdr).await.expect("read auth hdr");
                let desc = protocol::read_descriptor(&mut &hdr[..]).expect("parse auth desc");
                let mut buf = vec![0u8; protocol::DESCRIPTOR_SIZE + desc.length as usize];
                buf[..protocol::DESCRIPTOR_SIZE].copy_from_slice(&hdr);
                server_sock
                    .read_exact(&mut buf[protocol::DESCRIPTOR_SIZE..])
                    .await
                    .expect("read auth body");
                let (seq, cmd) = protocol::read_command_message(&mut &buf[..], ver).expect("parse auth cmd");
                assert!(matches!(cmd, protocol::Command::Auth(_)));
                let mut out = Vec::new();
                protocol::write_reply_message(
                    &mut out,
                    seq,
                    &protocol::AuthReply {
                        version: ver,
                        use_shm: false,
                        use_memfd: false,
                    },
                    ver,
                )
                .expect("encode auth reply");
                server_sock.write_all(&out).await.expect("write auth reply");

                // 2. SetClientName
                server_sock.read_exact(&mut hdr).await.expect("read name hdr");
                let desc = protocol::read_descriptor(&mut &hdr[..]).expect("parse name desc");
                buf.resize(protocol::DESCRIPTOR_SIZE + desc.length as usize, 0);
                buf[..protocol::DESCRIPTOR_SIZE].copy_from_slice(&hdr);
                server_sock
                    .read_exact(&mut buf[protocol::DESCRIPTOR_SIZE..])
                    .await
                    .expect("read name body");
                let (seq, cmd) = protocol::read_command_message(&mut &buf[..], ver).expect("parse name cmd");
                assert!(matches!(cmd, protocol::Command::SetClientName(_)));
                out.clear();
                protocol::write_reply_message(&mut out, seq, &protocol::SetClientNameReply { client_id: 1 }, ver)
                    .expect("encode name reply");
                server_sock.write_all(&out).await.expect("write name reply");

                // 3. Subscribe
                server_sock.read_exact(&mut hdr).await.expect("read sub hdr");
                let desc = protocol::read_descriptor(&mut &hdr[..]).expect("parse sub desc");
                buf.resize(protocol::DESCRIPTOR_SIZE + desc.length as usize, 0);
                buf[..protocol::DESCRIPTOR_SIZE].copy_from_slice(&hdr);
                server_sock
                    .read_exact(&mut buf[protocol::DESCRIPTOR_SIZE..])
                    .await
                    .expect("read sub body");
                let (seq, cmd) = protocol::read_command_message(&mut &buf[..], ver).expect("parse sub cmd");
                assert_eq!(
                    cmd,
                    protocol::Command::Subscribe(SubscriptionMask::SINK | SubscriptionMask::SERVER)
                );
                out.clear();
                protocol::write_ack_message(&mut out, seq).expect("encode sub ack");
                server_sock.write_all(&out).await.expect("write sub ack");

                // 4. GetServerInfo
                server_sock.read_exact(&mut hdr).await.expect("read srv hdr");
                let desc = protocol::read_descriptor(&mut &hdr[..]).expect("parse srv desc");
                buf.resize(protocol::DESCRIPTOR_SIZE + desc.length as usize, 0);
                buf[..protocol::DESCRIPTOR_SIZE].copy_from_slice(&hdr);
                server_sock
                    .read_exact(&mut buf[protocol::DESCRIPTOR_SIZE..])
                    .await
                    .expect("read srv body");
                let (seq, cmd) = protocol::read_command_message(&mut &buf[..], ver).expect("parse srv cmd");
                assert_eq!(cmd, protocol::Command::GetServerInfo);
                out.clear();
                let srv_info = protocol::ServerInfo {
                    default_sink_name: Some(c"alsa_output.speaker".to_owned()),
                    ..Default::default()
                };
                protocol::write_reply_message(&mut out, seq, &srv_info, ver).expect("encode srv reply");
                server_sock.write_all(&out).await.expect("write srv reply");

                // 5. GetSinkInfoList
                server_sock.read_exact(&mut hdr).await.expect("read sinks hdr");
                let desc = protocol::read_descriptor(&mut &hdr[..]).expect("parse sinks desc");
                buf.resize(protocol::DESCRIPTOR_SIZE + desc.length as usize, 0);
                buf[..protocol::DESCRIPTOR_SIZE].copy_from_slice(&hdr);
                server_sock
                    .read_exact(&mut buf[protocol::DESCRIPTOR_SIZE..])
                    .await
                    .expect("read sinks body");
                let (seq, cmd) = protocol::read_command_message(&mut &buf[..], ver).expect("parse sinks cmd");
                assert_eq!(cmd, protocol::Command::GetSinkInfoList);
                out.clear();
                let mut sink = protocol::SinkInfo::new_dummy(1);
                sink.name = c"alsa_output.speaker".to_owned();
                sink.description = Some(c"Built-in Speaker".to_owned());
                sink.cvolume = VolumeModule::pct_to_channel_volume(65, 2);
                sink.muted = false;
                let sinks: protocol::SinkInfoList = vec![sink];
                protocol::write_reply_message(&mut out, seq, &sinks, ver).expect("encode sinks reply");
                server_sock.write_all(&out).await.expect("write sinks reply");
            });

            let mut session = PulseSession::connect(client_sock).await.expect("handshake succeeds");
            let (vol, muted, sink) = session.fetch_state().await.expect("fetch_state succeeds");
            assert_eq!(vol, 65);
            assert!(!muted);
            assert_eq!(sink.name, "alsa_output.speaker");
            assert_eq!(sink.description, "Built-in Speaker");
            assert_eq!(sink.sink_type, SinkType::Speaker);
            server_task.await.expect("server task finishes cleanly");
        });
    }
}
