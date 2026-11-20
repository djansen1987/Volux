use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use crate::state::{AudioDevice, AudioFlow, Session};

const STARTUP_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_VALUE: &str = "Volux";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub start_at_login: bool,
    #[serde(default, skip_serializing)]
    pub preset: Vec<PresetEntry>,
    #[serde(default)]
    pub presets: Vec<Preset>,
    #[serde(default)]
    pub hidden_devices: Vec<String>,
    #[serde(default)]
    pub hidden_sessions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    #[serde(default)]
    pub entries: Vec<PresetEntry>,
    #[serde(default)]
    pub devices: Vec<PresetDevice>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PresetEntry {
    pub process_name: String,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default = "default_flow")]
    pub flow: AudioFlow,
    pub volume: f32,
    pub muted: bool,
    pub balance: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PresetDevice {
    pub id: String,
    pub name: String,
    pub flow: AudioFlow,
    pub volume: f32,
    pub muted: bool,
    pub balance: f32,
}

fn default_flow() -> AudioFlow {
    AudioFlow::Output
}

impl From<&Session> for PresetEntry {
    fn from(session: &Session) -> Self {
        Self {
            process_name: session.process_name.clone(),
            device_name: session.device_name.clone(),
            device_id: session.device_id.clone(),
            flow: session.flow,
            volume: session.volume,
            muted: session.muted,
            balance: session.balance,
        }
    }
}

impl From<&AudioDevice> for PresetDevice {
    fn from(device: &AudioDevice) -> Self {
        Self {
            id: device.id.clone(),
            name: device.name.clone(),
            flow: device.flow,
            volume: device.volume,
            muted: device.muted,
            balance: device.balance,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("Could not read {}", path.display()))?;
        serde_json::from_str(&contents).context("The configuration file is invalid")
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Could not create {}", parent.display()))?;
        }
        let contents = serde_json::to_string_pretty(self)?;
        fs::write(&path, contents).with_context(|| format!("Could not write {}", path.display()))
    }
}

pub fn set_start_at_login(enabled: bool) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(STARTUP_KEY)
        .context("Could not open the Windows startup registry key")?;

    if enabled {
        let executable = std::env::current_exe().context("Could not locate the app executable")?;
        let command = format!("\"{}\"", executable.display());
        key.set_value(STARTUP_VALUE, &command)
            .context("Could not enable startup with Windows")?;
    } else {
        match key.delete_value(STARTUP_VALUE) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Could not disable startup with Windows"),
        }
    }

    Ok(())
}

pub fn config_path() -> Result<PathBuf> {
    ProjectDirs::from("dev", "Volux", "Volux")
        .map(|dirs| dirs.config_dir().join("config.json"))
        .context("Could not locate the Windows application-data folder")
}

#[cfg(test)]
mod tests {
    use super::{Config, Preset, PresetDevice, PresetEntry};
    use crate::state::AudioFlow;

    #[test]
    fn preset_round_trips_as_json() {
        let preset = PresetEntry {
            process_name: "browser".to_string(),
            device_name: "Speakers".to_string(),
            device_id: "device-id".to_string(),
            flow: AudioFlow::Output,
            volume: 0.42,
            muted: false,
            balance: -0.25,
        };

        let encoded = serde_json::to_string(&preset).unwrap();
        assert_eq!(
            serde_json::from_str::<PresetEntry>(&encoded).unwrap(),
            preset
        );
    }

    #[test]
    fn old_single_preset_format_still_loads() {
        let config = serde_json::from_str::<Config>(
            r#"{
                "start_at_login": false,
                "preset": [{
                    "process_name": "browser",
                    "volume": 0.42,
                    "muted": false,
                    "balance": -0.25
                }]
            }"#,
        )
        .unwrap();

        assert_eq!(config.preset.len(), 1);
        assert_eq!(config.preset[0].flow, AudioFlow::Output);
        assert!(config.preset[0].device_name.is_empty());
    }

    #[test]
    fn named_presets_round_trip() {
        let config = Config {
            start_at_login: true,
            preset: Vec::new(),
            presets: vec![Preset {
                name: "Meetings".to_string(),
                entries: vec![PresetEntry {
                    process_name: "browser".to_string(),
                    device_name: "Headset".to_string(),
                    device_id: "device-id".to_string(),
                    flow: AudioFlow::Output,
                    volume: 0.35,
                    muted: false,
                    balance: 0.0,
                }],
                devices: vec![
                    PresetDevice {
                        id: "output-id".to_string(),
                        name: "Headset speakers".to_string(),
                        flow: AudioFlow::Output,
                        volume: 0.6,
                        muted: false,
                        balance: 0.0,
                    },
                    PresetDevice {
                        id: "input-id".to_string(),
                        name: "Headset microphone".to_string(),
                        flow: AudioFlow::Input,
                        volume: 0.8,
                        muted: false,
                        balance: 0.0,
                    },
                ],
            }],
            hidden_devices: Vec::new(),
            hidden_sessions: Vec::new(),
        };

        let encoded = serde_json::to_string(&config).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&encoded).unwrap(), config);
    }
}
