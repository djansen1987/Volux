use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AudioFlow {
    Output,
    Input,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub device_id: String,
    pub device_name: String,
    pub flow: AudioFlow,
    pub process_name: String,
    pub display_name: String,
    pub volume: f32,
    pub peak: f32,
    pub muted: bool,
    pub active: bool,
    pub balance: f32,
    pub balance_supported: bool,
}

#[derive(Clone, Debug)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub flow: AudioFlow,
    pub is_default: bool,
    pub volume: f32,
    pub peak: f32,
    pub muted: bool,
    pub balance: f32,
    pub balance_supported: bool,
    pub spectrum: Vec<f32>,
    pub spectrum_peak: Vec<f32>,
}

#[derive(Clone, Debug, Default)]
pub struct AudioSnapshot {
    pub master_muted: bool,
    pub devices: Vec<AudioDevice>,
    pub sessions: Vec<Session>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub enum AudioCommand {
    Refresh,
    SetMasterMuted(bool),
    SetDeviceVolume { id: String, value: f32 },
    SetDeviceMuted { id: String, value: bool },
    SetDeviceBalance { id: String, value: f32 },
    SetVolume { id: String, value: f32 },
    SetMuted { id: String, value: bool },
    SetBalance { id: String, value: f32 },
    Shutdown,
}

pub struct AudioChannels {
    pub commands: Sender<AudioCommand>,
    pub snapshots: Receiver<AudioSnapshot>,
}
