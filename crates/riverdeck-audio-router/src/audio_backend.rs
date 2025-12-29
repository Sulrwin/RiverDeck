use std::sync::{Arc, Mutex};
use std::collections::HashMap;

/// Information about an audio application/stream
#[derive(Debug, Clone)]
pub struct ApplicationInfo {
    pub process_id: u32,
    pub executable_file: String,
    pub display_name: String,
    pub volume: f32, // 0.0 to 1.0
    pub mute: bool,
    pub activity: ActivityState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityState {
    Active,
    InactiveShort,
    InactiveLong,
}

impl From<ActivityState> for u32 {
    fn from(state: ActivityState) -> Self {
        match state {
            ActivityState::Active => 0,
            ActivityState::InactiveShort => 1,
            ActivityState::InactiveLong => 2,
        }
    }
}

/// Information about an audio device
#[derive(Debug, Clone)]
pub struct AudioDevice {
    pub id: String,
    pub friendly_name: String,
    pub volume: f32, // 0.0 to 1.0
    pub mute: bool,
    pub is_default: bool,
    pub is_default_communication: bool,
}

/// Trait for audio backend implementations (PulseAudio, PipeWire, etc)
pub trait AudioBackend: Send + Sync {
    /// List all applications with audio streams
    fn list_applications(&self) -> Vec<ApplicationInfo>;

    /// Get volume for a specific application by PID
    fn get_app_volume(&self, pid: u32) -> Option<f32>;

    /// Set volume for a specific application by PID (0.0 to 1.0)
    fn set_app_volume(&self, pid: u32, volume: f32) -> anyhow::Result<()>;

    /// Get mute state for a specific application by PID
    fn get_app_mute(&self, pid: u32) -> Option<bool>;

    /// Set mute state for a specific application by PID
    fn set_app_mute(&self, pid: u32, mute: bool) -> anyhow::Result<()>;

    /// List all audio devices (input and output)
    fn list_devices(&self) -> Vec<AudioDevice>;

    /// Get system default device
    fn get_system_default_device(&self) -> Option<AudioDevice>;

    /// Get volume for a specific device
    fn get_device_volume(&self, device_id: &str) -> Option<f32>;

    /// Set volume for a specific device (0.0 to 1.0)
    fn set_device_volume(&self, device_id: &str, volume: f32) -> anyhow::Result<()>;

    /// Get mute state for a specific device
    fn get_device_mute(&self, device_id: &str) -> Option<bool>;

    /// Set mute state for a specific device
    fn set_device_mute(&self, device_id: &str, mute: bool) -> anyhow::Result<()>;
}

/// Structure to track a PulseAudio sink input (application audio stream)
#[derive(Debug, Clone)]
struct SinkInputInfo {
    index: u32,
    pid: Option<u32>,
    name: String,
    volume: f32,
    mute: bool,
    corked: bool, // paused/inactive
}

/// PulseAudio backend implementation
pub struct PulseAudioBackend {
    sink_inputs: Arc<Mutex<HashMap<u32, SinkInputInfo>>>,
    sinks: Arc<Mutex<HashMap<u32, AudioDevice>>>,
}

impl PulseAudioBackend {
    pub fn new() -> Arc<Self> {
        let backend = Arc::new(Self {
            sink_inputs: Arc::new(Mutex::new(HashMap::new())),
            sinks: Arc::new(Mutex::new(HashMap::new())),
        });

        // Spawn background thread to monitor PulseAudio
        let backend_clone = backend.clone();
        std::thread::spawn(move || {
            backend_clone.monitor_pulseaudio();
        });

        backend
    }

    fn monitor_pulseaudio(&self) {
        log::info!("PulseAudio monitor thread started");
        let mut error_count = 0;
        loop {
            match self.update_pulseaudio_state() {
                Ok(_) => {
                    if error_count > 0 {
                        log::info!("PulseAudio monitor recovered after {} errors", error_count);
                        error_count = 0;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(e) => {
                    error_count += 1;
                    if error_count <= 3 {
                        log::error!("PulseAudio monitor error #{}: {:#}", error_count, e);
                    } else if error_count == 4 {
                        log::error!("PulseAudio monitor: suppressing further error messages (failed {} times)", error_count);
                    }
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        }
    }

    fn update_pulseaudio_state(&self) -> anyhow::Result<()> {
        use std::process::Command;

        // Get list of sink inputs using pactl
        let output = Command::new("pactl")
            .args(["list", "sink-inputs"])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("pactl list sink-inputs failed with exit code {:?}: {}", output.status.code(), stderr);
            return Err(anyhow::anyhow!("pactl command failed: {}", stderr));
        }
        
        log::trace!("pactl list sink-inputs succeeded");

        let output_str = String::from_utf8_lossy(&output.stdout);
        let mut sink_inputs = HashMap::new();

        let mut current_input: Option<SinkInputInfo> = None;
        let mut current_index: Option<u32> = None;

        for line in output_str.lines() {
            let line = line.trim();

            if line.starts_with("Sink Input #") {
                // Save previous input
                if let (Some(index), Some(input)) = (current_index, current_input.take()) {
                    sink_inputs.insert(index, input);
                }

                // Parse new index
                if let Some(index_str) = line.strip_prefix("Sink Input #") {
                    current_index = index_str.parse().ok();
                    current_input = Some(SinkInputInfo {
                        index: current_index.unwrap_or(0),
                        pid: None,
                        name: String::new(),
                        volume: 1.0,
                        mute: false,
                        corked: false,
                    });
                }
            } else if let Some(ref mut input) = current_input {
                if line.starts_with("application.process.id = ") {
                    if let Some(pid_str) = line.strip_prefix("application.process.id = \"").and_then(|s| s.strip_suffix("\"")) {
                        input.pid = pid_str.parse().ok();
                    }
                } else if line.starts_with("application.name = ") {
                    if let Some(name_str) = line.strip_prefix("application.name = \"").and_then(|s| s.strip_suffix("\"")) {
                        input.name = name_str.to_string();
                    }
                } else if line.starts_with("Volume:") {
                    // Parse volume percentage (e.g., "0: 100% 1: 100%")
                    if let Some(vol_str) = line.split('/').next() {
                        if let Some(pct_str) = vol_str.split('%').next() {
                            if let Some(pct) = pct_str.split_whitespace().last() {
                                if let Ok(vol_int) = pct.parse::<u32>() {
                                    input.volume = (vol_int as f32) / 100.0;
                                }
                            }
                        }
                    }
                } else if line.starts_with("Mute: ") {
                    input.mute = line.contains("yes");
                } else if line.starts_with("Corked: ") {
                    input.corked = line.contains("yes");
                }
            }
        }

        // Save last input
        if let (Some(index), Some(input)) = (current_index, current_input) {
            sink_inputs.insert(index, input);
        }

        *self.sink_inputs.lock().unwrap() = sink_inputs;
        Ok(())
    }

    fn get_sink_input_by_pid(&self, pid: u32) -> Option<SinkInputInfo> {
        let inputs = self.sink_inputs.lock().unwrap();
        inputs.values()
            .find(|input| input.pid == Some(pid))
            .cloned()
    }

    fn set_sink_input_volume_by_index(&self, index: u32, volume: f32) -> anyhow::Result<()> {
        use std::process::Command;
        
        let volume_pct = (volume * 100.0).round() as u32;
        let output = Command::new("pactl")
            .args(["set-sink-input-volume", &index.to_string(), &format!("{}%", volume_pct)])
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!("Failed to set volume: {}", String::from_utf8_lossy(&output.stderr)));
        }

        Ok(())
    }

    fn set_sink_input_mute_by_index(&self, index: u32, mute: bool) -> anyhow::Result<()> {
        use std::process::Command;
        
        let mute_str = if mute { "1" } else { "0" };
        let output = Command::new("pactl")
            .args(["set-sink-input-mute", &index.to_string(), mute_str])
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!("Failed to set mute: {}", String::from_utf8_lossy(&output.stderr)));
        }

        Ok(())
    }
}

impl Default for PulseAudioBackend {
    fn default() -> Self {
        Self {
            sink_inputs: Arc::new(Mutex::new(HashMap::new())),
            sinks: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl AudioBackend for PulseAudioBackend {
    fn list_applications(&self) -> Vec<ApplicationInfo> {
        let inputs = self.sink_inputs.lock().unwrap();
        
        inputs.values()
            .filter_map(|input| {
                input.pid.map(|pid| {
                    // Try to get executable name from /proc
                    let exe = std::fs::read_link(format!("/proc/{}/exe", pid))
                        .ok()
                        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
                        .unwrap_or_else(|| input.name.clone());

                    ApplicationInfo {
                        process_id: pid,
                        executable_file: exe.clone(),
                        display_name: input.name.clone(),
                        volume: input.volume,
                        mute: input.mute,
                        activity: if input.corked {
                            ActivityState::InactiveShort
                        } else {
                            ActivityState::Active
                        },
                    }
                })
            })
            .collect()
    }

    fn get_app_volume(&self, pid: u32) -> Option<f32> {
        self.get_sink_input_by_pid(pid).map(|input| input.volume)
    }

    fn set_app_volume(&self, pid: u32, volume: f32) -> anyhow::Result<()> {
        let input = self.get_sink_input_by_pid(pid)
            .ok_or_else(|| anyhow::anyhow!("Application with PID {} not found", pid))?;
        
        self.set_sink_input_volume_by_index(input.index, volume)
    }

    fn get_app_mute(&self, pid: u32) -> Option<bool> {
        self.get_sink_input_by_pid(pid).map(|input| input.mute)
    }

    fn set_app_mute(&self, pid: u32, mute: bool) -> anyhow::Result<()> {
        let input = self.get_sink_input_by_pid(pid)
            .ok_or_else(|| anyhow::anyhow!("Application with PID {} not found", pid))?;
        
        self.set_sink_input_mute_by_index(input.index, mute)
    }

    fn list_devices(&self) -> Vec<AudioDevice> {
        let sinks = self.sinks.lock().unwrap();
        sinks.values().cloned().collect()
    }

    fn get_system_default_device(&self) -> Option<AudioDevice> {
        let sinks = self.sinks.lock().unwrap();
        sinks.values().find(|d| d.is_default).cloned()
    }

    fn get_device_volume(&self, device_id: &str) -> Option<f32> {
        let sinks = self.sinks.lock().unwrap();
        sinks.values()
            .find(|d| d.id == device_id)
            .map(|d| d.volume)
    }

    fn set_device_volume(&self, device_id: &str, volume: f32) -> anyhow::Result<()> {
        use std::process::Command;
        
        let volume_pct = (volume * 100.0).round() as u32;
        let output = Command::new("pactl")
            .args(["set-sink-volume", device_id, &format!("{}%", volume_pct)])
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!("Failed to set device volume"));
        }

        Ok(())
    }

    fn get_device_mute(&self, device_id: &str) -> Option<bool> {
        let sinks = self.sinks.lock().unwrap();
        sinks.values()
            .find(|d| d.id == device_id)
            .map(|d| d.mute)
    }

    fn set_device_mute(&self, device_id: &str, mute: bool) -> anyhow::Result<()> {
        use std::process::Command;
        
        let mute_str = if mute { "1" } else { "0" };
        let output = Command::new("pactl")
            .args(["set-sink-mute", device_id, mute_str])
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!("Failed to set device mute"));
        }

        Ok(())
    }
}
