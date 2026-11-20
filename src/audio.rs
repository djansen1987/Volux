use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::slice;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use windows::Win32::Devices::Properties::DEVPKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, PROPERTYKEY, S_OK};
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AudioSessionStateActive, AudioSessionStateExpired, DEVICE_STATE_ACTIVE, EDataFlow,
    IAudioCaptureClient, IAudioClient, IAudioSessionControl, IAudioSessionControl2,
    IAudioSessionManager2, IChannelAudioVolume, IMMDevice, IMMDeviceEnumerator, ISimpleAudioVolume,
    MMDeviceEnumerator, WAVEFORMATEXTENSIBLE, eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ, StructuredStorage,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::{BOOL, Interface, PWSTR};

use crate::state::{AudioChannels, AudioCommand, AudioDevice, AudioFlow, AudioSnapshot, Session};

const REFRESH_INTERVAL: Duration = Duration::from_millis(25);
const SPECTRUM_SAMPLE_COUNT: usize = 1024;
const SPECTRUM_BAND_COUNT: usize = 64;
const FLOAT_SUBFORMAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

struct LoopbackAnalyzer {
    _client: IAudioClient,
    capture: IAudioCaptureClient,
    channels: usize,
    sample_rate: usize,
    samples: VecDeque<f32>,
    previous: Vec<f32>,
    peaks: Vec<f32>,
    peak_hold_frames: Vec<u16>,
}

impl LoopbackAnalyzer {
    unsafe fn open() -> Result<Self> {
        let device = unsafe { endpoint() }?;
        let client = unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }?;
        let format_pointer = unsafe { client.GetMixFormat() }?;
        let format = unsafe { std::ptr::read_unaligned(format_pointer) };
        let is_float = if format.wFormatTag == 3 {
            true
        } else if format.wFormatTag == 0xfffe {
            let extensible =
                unsafe { std::ptr::read_unaligned(format_pointer.cast::<WAVEFORMATEXTENSIBLE>()) };
            let subformat: windows::core::GUID =
                unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(extensible.SubFormat)) };
            subformat == FLOAT_SUBFORMAT
        } else {
            false
        };
        let supported = is_float && format.wBitsPerSample == 32 && format.nChannels > 0;
        if !supported {
            unsafe { CoTaskMemFree(Some(format_pointer.cast())) };
            return Err(anyhow!("The output mix format is not 32-bit float audio"));
        }

        let initialize_result = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                1_000_000,
                0,
                format_pointer,
                None,
            )
        };
        let channels = format.nChannels as usize;
        let sample_rate = format.nSamplesPerSec as usize;
        unsafe { CoTaskMemFree(Some(format_pointer.cast())) };
        initialize_result?;

        let capture = unsafe { client.GetService::<IAudioCaptureClient>() }?;
        unsafe { client.Start() }?;
        Ok(Self {
            _client: client,
            capture,
            channels,
            sample_rate,
            samples: VecDeque::with_capacity(SPECTRUM_SAMPLE_COUNT * 2),
            previous: vec![0.0; SPECTRUM_BAND_COUNT],
            peaks: vec![0.0; SPECTRUM_BAND_COUNT],
            peak_hold_frames: vec![0; SPECTRUM_BAND_COUNT],
        })
    }

    unsafe fn sample(&mut self) -> Result<(Vec<f32>, Vec<f32>)> {
        loop {
            let packet_frames = unsafe { self.capture.GetNextPacketSize() }?;
            if packet_frames == 0 {
                break;
            }

            let mut data = std::ptr::null_mut();
            let mut frames = 0;
            let mut flags = 0;
            unsafe {
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
            }?;

            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                self.samples
                    .extend(std::iter::repeat_n(0.0, frames as usize));
            } else {
                let sample_count = frames as usize * self.channels;
                let interleaved =
                    unsafe { slice::from_raw_parts(data.cast::<f32>(), sample_count) };
                for frame in interleaved.chunks_exact(self.channels) {
                    self.samples
                        .push_back(frame.iter().sum::<f32>() / self.channels as f32);
                }
            }
            unsafe { self.capture.ReleaseBuffer(frames) }?;
        }

        while self.samples.len() > SPECTRUM_SAMPLE_COUNT * 2 {
            self.samples.pop_front();
        }
        if self.samples.len() < SPECTRUM_SAMPLE_COUNT {
            return Ok((self.previous.clone(), self.peaks.clone()));
        }

        let input = self
            .samples
            .iter()
            .rev()
            .take(SPECTRUM_SAMPLE_COUNT)
            .rev()
            .copied()
            .collect::<Vec<_>>();
        let mut current = spectrum_bands(&input, self.sample_rate);
        for (value, previous) in current.iter_mut().zip(&mut self.previous) {
            *previous = *previous * 0.72 + *value * 0.28;
            *value = *previous;
        }
        for (index, live) in current.iter().enumerate() {
            if *live >= self.peaks[index] {
                self.peaks[index] = *live;
                self.peak_hold_frames[index] = 0;
            } else if self.peak_hold_frames[index] < 30 {
                self.peak_hold_frames[index] += 1;
            } else {
                self.peaks[index] *= 0.965;
            }
        }
        Ok((self.previous.clone(), self.peaks.clone()))
    }
}

impl Drop for LoopbackAnalyzer {
    fn drop(&mut self) {
        unsafe {
            let _ = self._client.Stop();
        }
    }
}

fn spectrum_bands(samples: &[f32], sample_rate: usize) -> Vec<f32> {
    let sample_count = samples.len();
    if sample_count == 0 || sample_rate == 0 {
        return vec![0.0; SPECTRUM_BAND_COUNT];
    }

    let window = samples
        .iter()
        .enumerate()
        .map(|(index, sample)| {
            let phase = std::f32::consts::TAU * index as f32 / (sample_count - 1).max(1) as f32;
            sample * (0.5 - 0.5 * phase.cos())
        })
        .collect::<Vec<_>>();
    let maximum_frequency = (sample_rate as f32 * 0.5).min(20_000.0);
    let minimum_frequency = 20.0_f32.min(maximum_frequency);

    (0..SPECTRUM_BAND_COUNT)
        .map(|band| {
            let position = band as f32 / (SPECTRUM_BAND_COUNT - 1) as f32;
            let frequency =
                minimum_frequency * (maximum_frequency / minimum_frequency).powf(position);
            let coefficient = 2.0 * (std::f32::consts::TAU * frequency / sample_rate as f32).cos();
            let mut previous = 0.0;
            let mut previous_previous = 0.0;
            for sample in &window {
                let current = sample + coefficient * previous - previous_previous;
                previous_previous = previous;
                previous = current;
            }
            let power = (previous * previous + previous_previous * previous_previous
                - coefficient * previous * previous_previous)
                .max(0.0);
            let magnitude = power.sqrt() * 2.0 / sample_count as f32;
            ((20.0 * magnitude.max(1.0e-8).log10() + 72.0) / 72.0).clamp(0.0, 1.0)
        })
        .collect()
}

pub fn start() -> AudioChannels {
    let (command_tx, command_rx) = crossbeam_channel::unbounded();
    let (snapshot_tx, snapshot_rx) = crossbeam_channel::bounded(1);

    thread::spawn(move || run(command_rx, snapshot_tx));

    AudioChannels {
        commands: command_tx,
        snapshots: snapshot_rx,
    }
}

fn run(commands: Receiver<AudioCommand>, snapshots: Sender<AudioSnapshot>) {
    unsafe {
        let init_result = CoInitializeEx(None, COINIT_MULTITHREADED).ok();
        if let Err(error) = init_result {
            let _ = snapshots.send(AudioSnapshot {
                error: Some(format!("Could not initialize Windows audio: {error}")),
                ..AudioSnapshot::default()
            });
            return;
        }

        let mut loopback = LoopbackAnalyzer::open().ok();

        loop {
            let snapshot = collect_snapshot(&mut loopback).unwrap_or_else(|error| AudioSnapshot {
                error: Some(error.to_string()),
                ..AudioSnapshot::default()
            });
            let _ = snapshots.try_send(snapshot);

            let Ok(first_command) = (match commands.recv_timeout(REFRESH_INTERVAL) {
                Ok(command) => Ok(command),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(error) => Err(error),
            }) else {
                break;
            };

            let mut pending = vec![first_command];
            pending.extend(commands.try_iter());
            if pending
                .iter()
                .any(|command| matches!(command, AudioCommand::Shutdown))
            {
                break;
            }

            let mut command_error = None;
            for command in pending {
                if !matches!(command, AudioCommand::Refresh)
                    && let Err(error) = execute_command(command)
                {
                    command_error = Some(error.to_string());
                }
            }
            let mut snapshot =
                collect_snapshot(&mut loopback).unwrap_or_else(|error| AudioSnapshot {
                    error: Some(error.to_string()),
                    ..AudioSnapshot::default()
                });
            if let Some(error) = command_error {
                snapshot.error = Some(error);
            }
            let _ = snapshots.try_send(snapshot);
        }

        CoUninitialize();
    }
}

unsafe fn endpoint() -> Result<IMMDevice> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .context("Could not create the audio device enumerator")?;
    unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
        .context("Could not open the default output device")
}

unsafe fn endpoint_volume() -> Result<IAudioEndpointVolume> {
    unsafe { endpoint()?.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }
        .context("Could not open the output volume control")
}

fn audio_flow(data_flow: EDataFlow) -> AudioFlow {
    if data_flow == eCapture {
        AudioFlow::Input
    } else {
        AudioFlow::Output
    }
}

unsafe fn default_device_id(data_flow: EDataFlow) -> Option<String> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.ok()?;
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(data_flow, eConsole) }.ok()?;
    unsafe { device_id(&device) }.ok()
}

unsafe fn active_devices(data_flow: EDataFlow) -> Result<Vec<(IMMDevice, String, String, bool)>> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
    let devices = unsafe { enumerator.EnumAudioEndpoints(data_flow, DEVICE_STATE_ACTIVE) }?;
    let count = unsafe { devices.GetCount() }?;
    let default_id = unsafe { default_device_id(data_flow) };
    let mut result = Vec::with_capacity(count as usize);

    for index in 0..count {
        let device = unsafe { devices.Item(index) }?;
        let id = unsafe { device_id(&device) }?;
        let name = unsafe { device_name(&device) }.unwrap_or_else(|_| {
            if data_flow == eCapture {
                "Audio input".to_string()
            } else {
                "Audio output".to_string()
            }
        });
        let is_default = default_id.as_deref() == Some(id.as_str());
        result.push((device, id, name, is_default));
    }

    Ok(result)
}

unsafe fn device_id(device: &IMMDevice) -> Result<String> {
    let raw = unsafe { device.GetId() }?;
    let value = unsafe { raw.to_string() }.context("Could not read audio device ID")?;
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    Ok(value)
}

unsafe fn device_name(device: &IMMDevice) -> Result<String> {
    let properties = unsafe { device.OpenPropertyStore(STGM_READ) }?;
    let key = &DEVPKEY_Device_FriendlyName as *const _ as *const PROPERTYKEY;
    let mut value = unsafe { properties.GetValue(key) }?;
    let variant = unsafe { &value.Anonymous.Anonymous };
    if variant.vt != VT_LPWSTR {
        unsafe { StructuredStorage::PropVariantClear(&mut value).ok() };
        return Err(anyhow!("Audio device name is not a string"));
    }

    let pointer = unsafe { *(&variant.Anonymous as *const _ as *const *const u16) };
    if pointer.is_null() {
        unsafe { StructuredStorage::PropVariantClear(&mut value).ok() };
        return Err(anyhow!("Audio device name is empty"));
    }
    let mut length = 0;
    while length < 32_768 && unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    let name = if length < 32_768 {
        OsString::from_wide(unsafe { slice::from_raw_parts(pointer, length) })
            .to_string_lossy()
            .into_owned()
    } else {
        String::new()
    };
    unsafe { StructuredStorage::PropVariantClear(&mut value).ok() };
    if name.is_empty() {
        Err(anyhow!("Audio device name is empty"))
    } else {
        Ok(name)
    }
}

unsafe fn read_device(
    endpoint: &IMMDevice,
    id: String,
    name: String,
    data_flow: EDataFlow,
    is_default: bool,
) -> Result<AudioDevice> {
    let volume = unsafe { endpoint.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }?;
    let master_volume = unsafe { volume.GetMasterVolumeLevelScalar() }?;
    let muted = unsafe { volume.GetMute() }?.as_bool();
    let (balance, balance_supported) = if unsafe { volume.GetChannelCount() }.unwrap_or(0) >= 2 {
        let left = unsafe { volume.GetChannelVolumeLevelScalar(0) }.unwrap_or(1.0);
        let right = unsafe { volume.GetChannelVolumeLevelScalar(1) }.unwrap_or(1.0);
        (balance_from_channels(left, right), true)
    } else {
        (0.0, false)
    };
    let peak = unsafe { endpoint.Activate::<IAudioMeterInformation>(CLSCTX_ALL, None) }
        .and_then(|meter| unsafe { meter.GetPeakValue() })
        .unwrap_or_default()
        .clamp(0.0, 1.0);

    Ok(AudioDevice {
        id,
        name,
        flow: audio_flow(data_flow),
        is_default,
        volume: master_volume,
        peak,
        muted,
        balance,
        balance_supported,
        spectrum: Vec::new(),
        spectrum_peak: Vec::new(),
    })
}

unsafe fn find_device(id: &str) -> Result<IMMDevice> {
    for data_flow in [eRender, eCapture] {
        for (device, device_id, _, _) in unsafe { active_devices(data_flow) }? {
            if device_id == id {
                return Ok(device);
            }
        }
    }
    Err(anyhow!("Audio device is no longer available"))
}

unsafe fn collect_snapshot(loopback: &mut Option<LoopbackAnalyzer>) -> Result<AudioSnapshot> {
    let master_muted = unsafe { endpoint_volume() }
        .and_then(|output| unsafe { output.GetMute() }.map_err(Into::into))
        .map(|muted| muted.as_bool())
        .unwrap_or(false);
    let mut devices = Vec::new();
    let mut sessions = Vec::new();
    let mut ignored = Vec::new();
    let window_titles = unsafe { window_titles() };

    for data_flow in [eRender, eCapture] {
        let endpoints = unsafe { active_devices(data_flow) }?;
        for (endpoint, id, name, is_default) in endpoints {
            match unsafe { read_device(&endpoint, id.clone(), name.clone(), data_flow, is_default) }
            {
                Ok(device) => devices.push(device),
                Err(error) => {
                    ignored.push(format!("{name}: {error}"));
                    continue;
                }
            }

            let manager =
                match unsafe { endpoint.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) } {
                    Ok(manager) => manager,
                    Err(error) => {
                        ignored.push(format!("{name} sessions: {error}"));
                        continue;
                    }
                };
            let enumerator = unsafe { manager.GetSessionEnumerator() }?;
            let count = unsafe { enumerator.GetCount() }?;
            for index in 0..count {
                let session = unsafe { enumerator.GetSession(index) }?;
                match unsafe {
                    read_session(session, &window_titles, &id, &name, audio_flow(data_flow))
                } {
                    Ok(session_snapshot) => sessions.push(session_snapshot),
                    Err(error) if error.to_string() == "system sounds session" => {}
                    Err(error) if error.to_string() == "expired audio session" => {}
                    Err(error) if error.to_string() == "volux session" => {}
                    Err(error) => ignored.push(format!("{name} #{}: {error}", index + 1)),
                }
            }
        }
    }

    let spectrum = loopback
        .as_mut()
        .and_then(|analyzer| unsafe { analyzer.sample() }.ok());
    if let Some((device, (live, peak))) = devices
        .iter_mut()
        .find(|device| device.flow == AudioFlow::Output && device.is_default)
        .zip(spectrum)
    {
        device.spectrum = live;
        device.spectrum_peak = peak;
    }

    sessions.sort_by(|left, right| {
        right.active.cmp(&left.active).then_with(|| {
            left.process_name
                .to_lowercase()
                .cmp(&right.process_name.to_lowercase())
        })
    });

    Ok(AudioSnapshot {
        master_muted,
        devices,
        sessions,
        error: if ignored.is_empty() {
            None
        } else {
            Some(format!(
                "Windows did not expose {} audio session{}: {}",
                ignored.len(),
                if ignored.len() == 1 { "" } else { "s" },
                ignored.join(", ")
            ))
        },
    })
}

unsafe fn read_session(
    control: IAudioSessionControl,
    window_titles: &HashMap<u32, String>,
    device_id: &str,
    device_name: &str,
    flow: AudioFlow,
) -> Result<Session> {
    let control2 = control.cast::<IAudioSessionControl2>()?;
    if unsafe { control2.IsSystemSoundsSession() } == S_OK {
        return Err(anyhow!("system sounds session"));
    }
    let state = unsafe { control.GetState() }?;
    if state == AudioSessionStateExpired {
        return Err(anyhow!("expired audio session"));
    }

    let process_id = unsafe { control2.GetProcessId() }?;
    if process_id == std::process::id() {
        return Err(anyhow!("volux session"));
    }
    let process_name = process_name(process_id);
    let display_name = display_name(&control);
    let window_title = window_titles.get(&process_id).cloned();
    let visible_name = friendly_app_name(
        &process_name,
        display_name.as_deref(),
        window_title.as_deref(),
        process_id,
    );
    let simple = control.cast::<ISimpleAudioVolume>()?;
    let volume = unsafe { simple.GetMasterVolume() }?;
    let peak = control
        .cast::<IAudioMeterInformation>()
        .and_then(|meter| unsafe { meter.GetPeakValue() })
        .unwrap_or_default()
        .clamp(0.0, 1.0);
    let muted = unsafe { simple.GetMute() }?.as_bool();
    let (balance, balance_supported) = match control.cast::<IChannelAudioVolume>() {
        Ok(channels) if unsafe { channels.GetChannelCount() }.unwrap_or(0) >= 2 => {
            let left = unsafe { channels.GetChannelVolume(0) }.unwrap_or(1.0);
            let right = unsafe { channels.GetChannelVolume(1) }.unwrap_or(1.0);
            (balance_from_channels(left, right), true)
        }
        _ => (0.0, false),
    };

    Ok(Session {
        id: format!("{device_id}\t{process_id}"),
        device_id: device_id.to_string(),
        device_name: device_name.to_string(),
        flow,
        process_name: visible_name,
        display_name: window_title
            .or(display_name)
            .unwrap_or_else(|| "Audio application".to_string()),
        volume,
        peak,
        muted,
        active: state == AudioSessionStateActive,
        balance,
        balance_supported,
    })
}

unsafe fn execute_command(command: AudioCommand) -> Result<()> {
    match command {
        AudioCommand::SetMasterMuted(value) => {
            unsafe { endpoint_volume()?.SetMute(value, std::ptr::null()) }?;
        }
        AudioCommand::SetDeviceVolume { id, value } => {
            unsafe {
                find_device(&id)?
                    .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)?
                    .SetMasterVolumeLevelScalar(value.clamp(0.0, 1.0), std::ptr::null())?
            };
        }
        AudioCommand::SetDeviceMuted { id, value } => {
            unsafe {
                find_device(&id)?
                    .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)?
                    .SetMute(value, std::ptr::null())?
            };
        }
        AudioCommand::SetDeviceBalance { id, value } => {
            let volume =
                unsafe { find_device(&id)?.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)? };
            let master_volume = unsafe { volume.GetMasterVolumeLevelScalar() }?;
            let (left, right) = channels_from_balance_at_volume(value, master_volume);
            if unsafe { volume.GetChannelCount() }? < 2 {
                return Err(anyhow!("This device does not expose stereo channels"));
            }
            unsafe {
                volume.SetChannelVolumeLevelScalar(0, left, std::ptr::null())?;
                volume.SetChannelVolumeLevelScalar(1, right, std::ptr::null())?;
            }
        }
        AudioCommand::SetVolume { id, value } => {
            unsafe {
                with_session(&id, |control| {
                    let simple = control.cast::<ISimpleAudioVolume>()?;
                    simple.SetMasterVolume(value.clamp(0.0, 1.0), std::ptr::null())?;
                    Ok(())
                })
            }?;
        }
        AudioCommand::SetMuted { id, value } => {
            unsafe {
                with_session(&id, |control| {
                    let simple = control.cast::<ISimpleAudioVolume>()?;
                    simple.SetMute(value, std::ptr::null())?;
                    Ok(())
                })
            }?;
        }
        AudioCommand::SetBalance { id, value } => {
            unsafe {
                with_session(&id, |control| {
                    let channels = control.cast::<IChannelAudioVolume>()?;
                    if channels.GetChannelCount()? < 2 {
                        return Err(anyhow!("This session does not expose stereo channels"));
                    }
                    let (left, right) = channels_from_balance(value);
                    channels.SetChannelVolume(0, left, std::ptr::null())?;
                    channels.SetChannelVolume(1, right, std::ptr::null())?;
                    Ok(())
                })
            }?;
        }
        AudioCommand::Refresh | AudioCommand::Shutdown => {}
    }
    Ok(())
}

unsafe fn with_session(
    id: &str,
    action: impl FnOnce(IAudioSessionControl) -> Result<()>,
) -> Result<()> {
    let (wanted_device_id, wanted_process_id) = id
        .split_once('\t')
        .ok_or_else(|| anyhow!("Invalid audio session identifier"))?;
    let device = unsafe { find_device(wanted_device_id) }?;
    let manager = unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) }?;
    let enumerator = unsafe { manager.GetSessionEnumerator() }?;
    {
        let count = unsafe { enumerator.GetCount() }?;
        for index in 0..count {
            let session = unsafe { enumerator.GetSession(index) }?;
            let session2 = session.cast::<IAudioSessionControl2>()?;
            if unsafe { session2.GetProcessId() }?.to_string() == wanted_process_id {
                return action(session);
            }
        }
    }

    Err(anyhow!("Audio application is no longer running"))
}

fn display_name(control: &IAudioSessionControl) -> Option<String> {
    unsafe {
        let raw = control.GetDisplayName().ok()?;
        let value = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0.cast()));
        value.filter(|value| !value.is_empty())
    }
}

fn process_name(process_id: u32) -> String {
    unsafe {
        let process = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) {
            Ok(process) => process,
            Err(_) => return String::new(),
        };
        let mut buffer = vec![0u16; 1024];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(process);

        if result.is_err() {
            return String::new();
        }

        let path = OsString::from_wide(&buffer[..length as usize])
            .to_string_lossy()
            .into_owned();
        std::path::Path::new(&path)
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or(&path)
            .to_string()
    }
}

fn friendly_app_name(
    executable: &str,
    audio_display_name: Option<&str>,
    window_title: Option<&str>,
    process_id: u32,
) -> String {
    let normalized = executable.trim().to_lowercase();
    let generic = normalized.is_empty()
        || normalized.starts_with("process ")
        || matches!(
            normalized.as_str(),
            "msedgewebview2" | "audiodg" | "svchost"
        );

    if generic {
        if let Some(title) = usable_window_title(window_title) {
            return title.to_string();
        }
        if let Some(display) = audio_display_name.map(str::trim).filter(|name| {
            !name.is_empty()
                && !name.chars().all(char::is_numeric)
                && !name.eq_ignore_ascii_case("Audio application")
        }) {
            return display.to_string();
        }
        if normalized == "msedgewebview2" {
            return "Web content".to_string();
        }
        return format!("Audio app · {process_id}");
    }

    executable.trim().to_string()
}

fn usable_window_title(title: Option<&str>) -> Option<&str> {
    title
        .map(str::trim)
        .filter(|title| !title.is_empty() && title.len() <= 160 && *title != "Default IME")
}

unsafe fn window_titles() -> HashMap<u32, String> {
    struct WindowContext {
        titles: HashMap<u32, String>,
    }

    unsafe extern "system" fn collect(hwnd: HWND, parameter: LPARAM) -> BOOL {
        let context = unsafe { &mut *(parameter.0 as *mut WindowContext) };
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return true.into();
        }

        let mut process_id = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
        if process_id == 0 {
            return true.into();
        }

        let mut buffer = [0u16; 512];
        let length = unsafe { GetWindowTextW(hwnd, &mut buffer) };
        if length <= 0 {
            return true.into();
        }

        let title = String::from_utf16_lossy(&buffer[..length as usize])
            .trim()
            .to_string();
        if usable_window_title(Some(&title)).is_some() {
            context.titles.entry(process_id).or_insert_with(|| title);
        }
        true.into()
    }

    let mut context = WindowContext {
        titles: HashMap::new(),
    };
    let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut context as *mut _ as isize)) };
    context.titles
}

pub fn channels_from_balance(balance: f32) -> (f32, f32) {
    let balance = balance.clamp(-1.0, 1.0);
    if balance < 0.0 {
        (1.0, 1.0 + balance)
    } else {
        (1.0 - balance, 1.0)
    }
}

pub fn channels_from_balance_at_volume(balance: f32, volume: f32) -> (f32, f32) {
    let (left, right) = channels_from_balance(balance);
    let volume = volume.clamp(0.0, 1.0);
    (left * volume, right * volume)
}

fn balance_from_channels(left: f32, right: f32) -> f32 {
    if right < left {
        (right - left) / left.max(f32::EPSILON)
    } else {
        (right - left) / right.max(f32::EPSILON)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SPECTRUM_BAND_COUNT, balance_from_channels, channels_from_balance,
        channels_from_balance_at_volume, friendly_app_name, spectrum_bands,
    };

    #[test]
    fn balance_clamps_and_preserves_the_loudest_channel() {
        assert_eq!(channels_from_balance(-1.5), (1.0, 0.0));
        assert_eq!(channels_from_balance(-0.5), (1.0, 0.5));
        assert_eq!(channels_from_balance(0.0), (1.0, 1.0));
        assert_eq!(channels_from_balance(0.5), (0.5, 1.0));
        assert_eq!(channels_from_balance(1.5), (0.0, 1.0));
    }

    #[test]
    fn balance_round_trips_common_channel_pairs() {
        for expected in [-1.0, -0.5, 0.0, 0.5, 1.0] {
            let (left, right) = channels_from_balance(expected);
            assert!((balance_from_channels(left, right) - expected).abs() < 0.0001);
        }
    }

    #[test]
    fn endpoint_balance_preserves_master_volume() {
        assert_eq!(channels_from_balance_at_volume(-0.5, 0.4), (0.4, 0.2));
        assert_eq!(channels_from_balance_at_volume(0.5, 0.4), (0.2, 0.4));
        assert_eq!(channels_from_balance_at_volume(0.0, 0.4), (0.4, 0.4));
    }

    #[test]
    fn spectrum_analyzer_places_energy_in_expected_band() {
        let sample_rate = 48_000;
        let samples = (0..1024)
            .map(|index| {
                (std::f32::consts::TAU * 1_000.0 * index as f32 / sample_rate as f32).sin()
            })
            .collect::<Vec<_>>();
        let bands = spectrum_bands(&samples, sample_rate);

        assert_eq!(bands.len(), SPECTRUM_BAND_COUNT);
        let strongest = bands
            .iter()
            .enumerate()
            .max_by(|left, right| left.1.total_cmp(right.1))
            .map(|(index, _)| index)
            .unwrap();
        assert!(
            (30..=40).contains(&strongest),
            "strongest band: {strongest}"
        );
    }

    #[test]
    fn generic_webview_uses_its_window_title() {
        assert_eq!(
            friendly_app_name(
                "msedgewebview2",
                Some("Audio application"),
                Some("Meeting Room"),
                1234,
            ),
            "Meeting Room"
        );
    }

    #[test]
    fn inaccessible_process_uses_display_or_pid_fallback() {
        assert_eq!(
            friendly_app_name("", Some("VoIP Client"), None, 9108),
            "VoIP Client"
        );
        assert_eq!(
            friendly_app_name("", Some("Audio application"), None, 9108),
            "Audio app · 9108"
        );
        assert_eq!(
            friendly_app_name("msedgewebview2", None, None, 9108),
            "Web content"
        );
    }
}
