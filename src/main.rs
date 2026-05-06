use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use rusb::{Device, DeviceHandle, GlobalContext};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

const VID_KINGSTON: u16 = 0x0951;
const VID_HP: u16 = 0x03f0;

const PACKET_SIZE: usize = 64;
const RGB_CODE: u8 = 0x81;
const HEADER_CODE: u8 = 0x04;
const DISPLAY_CODE: u8 = 0xf2;
const PACKET_CNT: u8 = 0x01;

const CTRL_REQUEST_TYPE_OUT: u8 = 0x21;
const CTRL_REQUEST_OUT: u8 = 0x09;
const CTRL_VALUE: u16 = 0x0300;
const CTRL_INDEX: u16 = 0x0000;

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(1);
const REFRESH_INTERVAL: Duration = Duration::from_micros(55_000);
const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(1);

const LAUNCHD_LABEL: &str = "com.miketineo.quadcastctl";

// Mode timing constants — matched to upstream rgbmodes.h so the visual feel
// is the same as the C tool / NGenuity.
const MIN_CYCL_TR: u32 = 12;
const MAX_CYCL_TR: u32 = 128;
const MIN_LGHT_BL: u32 = 1;
const MAX_LGHT_BL: u32 = 9;
const MIN_LGHT_UP: u32 = 3;
const MAX_LGHT_UP: u32 = 10;
const MIN_LGHT_DOWN: u32 = 21;
const MAX_LGHT_DOWN: u32 = 131;

// SPEED_RANGE(MIN, MAX, SPD) = MIN + (MAX-MIN)*(100-SPD)/100  (slower at lower spd)
fn speed_range(min: u32, max: u32, spd: u8) -> u32 {
    let spd = spd.min(100) as u32;
    min + (max - min) * (100 - spd) / 100
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Model {
    QuadcastS,
    Duocast,
    Quadcast2,
    Quadcast2S,
}

impl Model {
    fn from_ids(vid: u16, pid: u16) -> Option<Self> {
        match (vid, pid) {
            (VID_KINGSTON, 0x171f) => Some(Model::QuadcastS),
            (VID_HP, 0x0f8b)
            | (VID_HP, 0x028c)
            | (VID_HP, 0x048c)
            | (VID_HP, 0x068c) => Some(Model::QuadcastS),
            (VID_HP, 0x098c) => Some(Model::Duocast),
            (VID_HP, 0x09af) => Some(Model::Quadcast2),
            (VID_HP, 0x02b5) => Some(Model::Quadcast2S),
            _ => None,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Model::QuadcastS => "HyperX Quadcast S",
            Model::Duocast => "HyperX Duocast",
            Model::Quadcast2 => "HyperX Quadcast 2",
            Model::Quadcast2S => "HyperX Quadcast 2S",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Color {
    r: u8,
    g: u8,
    b: u8,
}

const BLACK: Color = Color { r: 0, g: 0, b: 0 };

impl Color {
    fn from_hex(s: &str) -> Result<Self> {
        let s = s.strip_prefix('#').unwrap_or(s);
        if s.is_empty() || s.len() > 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("invalid hex color {s:?}: expected 1-6 hex digits");
        }
        let n = u32::from_str_radix(s, 16)?;
        Ok(Color {
            r: ((n >> 16) & 0xff) as u8,
            g: ((n >> 8) & 0xff) as u8,
            b: (n & 0xff) as u8,
        })
    }

    fn scaled(self, brightness: u8) -> Self {
        let scale = |c: u8| ((c as u32 * brightness as u32) / 100) as u8;
        Color {
            r: scale(self.r),
            g: scale(self.g),
            b: scale(self.b),
        }
    }
}

fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let lerp = |x: u8, y: u8| {
        let v = x as f32 + (y as f32 - x as f32) * t;
        v.round().clamp(0.0, 255.0) as u8
    };
    Color {
        r: lerp(a.r, b.r),
        g: lerp(a.g, b.g),
        b: lerp(a.b, b.b),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Solid,
    Blink,
    Cycle,
    Wave,
    Pulse,
    Lightning,
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Solid
    }
}

impl Mode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "solid" => Some(Mode::Solid),
            "blink" => Some(Mode::Blink),
            "cycle" => Some(Mode::Cycle),
            "wave" => Some(Mode::Wave),
            "pulse" => Some(Mode::Pulse),
            "lightning" => Some(Mode::Lightning),
            _ => None,
        }
    }
}

const BUILTIN_PRESETS: &[(&str, &str)] = &[
    ("red", "ff0000"),
    ("orange", "ff9a33"),
    ("yellow", "ffff00"),
    ("green", "00ff00"),
    ("cyan", "00ffff"),
    ("blue", "0000ff"),
    ("purple", "9933ff"),
    ("magenta", "ff00ff"),
    ("pink", "ff69b4"),
    ("white", "ffffff"),
    ("off", "000000"),
    ("hivenet", "ff9a33"),
];
#[derive(Debug, Default, Serialize, Deserialize)]
struct UserPresets {
    #[serde(default)]
    presets: BTreeMap<String, String>,
}

fn presets_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("presets.toml"))
}

fn load_user_presets() -> UserPresets {
    let path = match presets_path() {
        Ok(p) => p,
        Err(_) => return UserPresets::default(),
    };
    if !path.exists() {
        return UserPresets::default();
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_user_presets(p: &UserPresets) -> Result<()> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir)?;
    let path = presets_path()?;
    fs::write(&path, toml::to_string_pretty(p)?)?;
    Ok(())
}

fn resolve_color_or_preset(input: &str) -> Result<String> {
    if Color::from_hex(input).is_ok() {
        return Ok(input.strip_prefix('#').unwrap_or(input).to_string());
    }
    let user = load_user_presets();
    if let Some(hex) = user.presets.get(input) {
        return Ok(hex.clone());
    }
    for (name, hex) in BUILTIN_PRESETS {
        if *name == input {
            return Ok((*hex).to_string());
        }
    }
    bail!("{input:?} is not a valid hex color or known preset. Try `quadcastctl preset list`.")
}

fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow!("no config dir on this platform"))?;
    Ok(base.join("quadcastctl"))
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Config {
    #[serde(default)]
    mode: Mode,
    #[serde(default)]
    colors: Vec<String>,
    /// Legacy single-color field; used when `colors` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    color: Option<String>,
    #[serde(default = "default_brightness")]
    brightness: u8,
    #[serde(default = "default_speed")]
    speed: u8,
    #[serde(default = "default_delay")]
    delay: u8,
    /// When set, the daemon switches to this color/preset whenever the system
    /// reports the input device as muted (hardware tap, Control Center, app, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    on_mute: Option<String>,
}

fn default_brightness() -> u8 {
    100
}
fn default_speed() -> u8 {
    81
}
fn default_delay() -> u8 {
    10
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Solid,
            colors: vec!["ff9a33".into()],
            color: None,
            brightness: 100,
            speed: 81,
            delay: 10,
            on_mute: None,
        }
    }
}

impl Config {
    fn effective_colors(&self) -> Vec<String> {
        if !self.colors.is_empty() {
            self.colors.clone()
        } else if let Some(c) = &self.color {
            vec![c.clone()]
        } else {
            vec!["ff9a33".into()]
        }
    }

    fn resolved_colors(&self) -> Result<Vec<Color>> {
        if self.brightness > 100 {
            bail!("brightness must be 0-100");
        }
        let mut out = Vec::new();
        for c in self.effective_colors() {
            let hex = resolve_color_or_preset(&c)?;
            out.push(Color::from_hex(&hex)?.scaled(self.brightness));
        }
        Ok(out)
    }

    fn summary(&self) -> String {
        let colors = self.effective_colors().join(",");
        format!(
            "{:?} colors=[{}] brightness={} speed={} delay={}",
            self.mode, colors, self.brightness, self.speed, self.delay
        )
    }
}

fn load_config() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?)
}

fn save_config(cfg: &Config) -> Result<()> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = config_path()?;
    let text = toml::to_string_pretty(cfg).context("serializing config")?;
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn config_mtime() -> Option<SystemTime> {
    let path = config_path().ok()?;
    fs::metadata(&path).and_then(|m| m.modified()).ok()
}

// --- Frame generation -------------------------------------------------------

type Frame = [u8; 8];

fn frame_pair(upper: Color, lower: Color) -> Frame {
    [
        RGB_CODE, upper.r, upper.g, upper.b, RGB_CODE, lower.r, lower.g, lower.b,
    ]
}

fn zip_frames(upper: &[Color], lower: &[Color]) -> Vec<Frame> {
    if upper.is_empty() && lower.is_empty() {
        return vec![frame_pair(BLACK, BLACK)];
    }
    let len = upper.len().max(lower.len());
    let pick = |seq: &[Color], i: usize| -> Color {
        if seq.is_empty() {
            BLACK
        } else {
            seq[i % seq.len()]
        }
    };
    (0..len)
        .map(|i| frame_pair(pick(upper, i), pick(lower, i)))
        .collect()
}

fn gen_solid(colors: &[Color]) -> Vec<Color> {
    vec![*colors.first().unwrap_or(&BLACK)]
}

fn gen_cycle(colors: &[Color], speed: u8) -> Vec<Color> {
    if colors.is_empty() {
        return vec![BLACK];
    }
    if colors.len() == 1 {
        return colors.to_vec();
    }
    let tr_len = speed_range(MIN_CYCL_TR, MAX_CYCL_TR, speed) as usize;
    let mut out = Vec::with_capacity(colors.len() * tr_len);
    for i in 0..colors.len() {
        let start = colors[i];
        let end = colors[(i + 1) % colors.len()];
        for k in 0..tr_len {
            let t = k as f32 / tr_len as f32;
            out.push(lerp_color(start, end, t));
        }
    }
    out
}

fn gen_blink(colors: &[Color], speed: u8, delay: u8) -> Vec<Color> {
    if colors.is_empty() {
        return vec![BLACK];
    }
    let on = (101u32.saturating_sub(speed as u32)) as usize;
    let off = delay as usize;
    let mut out = Vec::new();
    for c in colors {
        for _ in 0..on {
            out.push(*c);
        }
        for _ in 0..off {
            out.push(BLACK);
        }
    }
    if out.is_empty() {
        out.push(*colors.first().unwrap_or(&BLACK));
    }
    out
}

/// Pulse / lightning shared shape: per color, do a [black-hold, fade-up, fade-down].
/// `synchronous=true` → both diodes do the same thing at the same time (pulse).
/// `synchronous=false` → upper holds black during lower's fade and vice versa (lightning).
fn gen_lightning_one(colors: &[Color], speed: u8, synchronous: bool, is_lower: bool) -> Vec<Color> {
    if colors.is_empty() {
        return vec![BLACK];
    }
    let bl = speed_range(MIN_LGHT_BL, MAX_LGHT_BL, speed) as usize;
    let up = speed_range(MIN_LGHT_UP, MAX_LGHT_UP, speed) as usize;
    let down = speed_range(MIN_LGHT_DOWN, MAX_LGHT_DOWN, speed) as usize;
    let mut out = Vec::new();
    for c in colors {
        // Lower channel pre-pause when async (waits while upper finishes its prior pulse)
        if is_lower && !synchronous {
            for _ in 0..bl {
                out.push(BLACK);
            }
        }
        // fade up
        for k in 1..=up {
            let t = k as f32 / up.max(1) as f32;
            out.push(lerp_color(BLACK, *c, t));
        }
        // fade down
        for k in 1..=down {
            let t = k as f32 / down.max(1) as f32;
            out.push(lerp_color(*c, BLACK, t));
        }
        // Upper or both finish with hold-black
        if synchronous || !is_lower {
            for _ in 0..bl {
                out.push(BLACK);
            }
        }
    }
    out
}

fn shifted<T: Clone>(v: &[T]) -> Vec<T> {
    if v.len() <= 1 {
        return v.to_vec();
    }
    let mut out: Vec<T> = v[1..].to_vec();
    out.push(v[0].clone());
    out
}

fn generate_frames(cfg: &Config) -> Result<Vec<Frame>> {
    let colors = cfg.resolved_colors()?;
    let frames = match cfg.mode {
        Mode::Solid => {
            let seq = gen_solid(&colors);
            zip_frames(&seq, &seq)
        }
        Mode::Blink => {
            let seq = gen_blink(&colors, cfg.speed, cfg.delay);
            zip_frames(&seq, &seq)
        }
        Mode::Cycle => {
            let seq = gen_cycle(&colors, cfg.speed);
            zip_frames(&seq, &seq)
        }
        Mode::Wave => {
            let upper = gen_cycle(&colors, cfg.speed);
            let lower = gen_cycle(&shifted(&colors), cfg.speed);
            zip_frames(&upper, &lower)
        }
        Mode::Pulse => {
            let seq = gen_lightning_one(&colors, cfg.speed, true, false);
            zip_frames(&seq, &seq)
        }
        Mode::Lightning => {
            let upper = gen_lightning_one(&colors, cfg.speed, false, false);
            let lower = gen_lightning_one(&colors, cfg.speed, false, true);
            zip_frames(&upper, &lower)
        }
    };
    if frames.is_empty() {
        return Ok(vec![frame_pair(BLACK, BLACK)]);
    }
    Ok(frames)
}

// --- USB / device -----------------------------------------------------------

fn find_mic() -> Result<(Device<GlobalContext>, Model)> {
    for dev in rusb::devices().context("listing USB devices")?.iter() {
        let desc = dev.device_descriptor().context("reading descriptor")?;
        if let Some(model) = Model::from_ids(desc.vendor_id(), desc.product_id()) {
            return Ok((dev, model));
        }
    }
    Err(anyhow!("no compatible HyperX microphone found"))
}

fn open_mic() -> Result<(DeviceHandle<GlobalContext>, Model)> {
    let (dev, model) = find_mic()?;
    let handle = dev.open().context("opening USB device")?;
    let _ = handle.set_auto_detach_kernel_driver(true);
    for iface in [0u8, 1u8] {
        if let Err(e) = handle.claim_interface(iface) {
            match e {
                rusb::Error::Busy => bail!(
                    "interface {iface} is busy — another quadcastctl/quadcastrgb is running?"
                ),
                rusb::Error::NoDevice => bail!("device disconnected"),
                _ => eprintln!("note: could not claim interface {iface}: {e} (continuing)"),
            }
        }
    }
    Ok((handle, model))
}

fn build_header_packet() -> [u8; PACKET_SIZE] {
    let mut p = [0u8; PACKET_SIZE];
    p[0] = HEADER_CODE;
    p[1] = DISPLAY_CODE;
    p[8] = PACKET_CNT;
    p
}

fn build_data_packet(frame: &Frame) -> [u8; PACKET_SIZE] {
    let mut p = [0u8; PACKET_SIZE];
    p[..8].copy_from_slice(frame);
    p
}

fn send_control_packet(handle: &DeviceHandle<GlobalContext>, packet: &[u8]) -> Result<()> {
    let n = handle
        .write_control(
            CTRL_REQUEST_TYPE_OUT,
            CTRL_REQUEST_OUT,
            CTRL_VALUE,
            CTRL_INDEX,
            packet,
            TRANSFER_TIMEOUT,
        )
        .context("control transfer")?;
    if n != packet.len() {
        bail!("short control transfer: {} of {}", n, packet.len());
    }
    Ok(())
}

// --- Core Audio FFI ---------------------------------------------------------
//
// We ask the macOS Audio HAL whether the default input device is muted.
// macOS forwards the Quadcast S's hardware tap-to-mute to this property,
// AND it captures software mutes (Control Center, app-driven mute, etc).
// One signal covers everything we care about.

mod core_audio {
    use std::os::raw::c_void;

    pub type AudioObjectID = u32;
    pub type OSStatus = i32;
    pub type AudioObjectPropertySelector = u32;
    pub type AudioObjectPropertyScope = u32;
    pub type AudioObjectPropertyElement = u32;

    pub const SYSTEM_OBJECT: AudioObjectID = 1;
    pub const ELEMENT_MAIN: AudioObjectPropertyElement = 0;

    // FourCharCodes are big-endian 4-byte ASCII packed into a u32.
    const fn fcc(s: &[u8; 4]) -> u32 {
        ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
    }

    pub const SCOPE_GLOBAL: AudioObjectPropertyScope = fcc(b"glob");
    pub const SCOPE_INPUT: AudioObjectPropertyScope = fcc(b"inpt");
    pub const SELECTOR_DEFAULT_INPUT_DEVICE: AudioObjectPropertySelector = fcc(b"dIn ");
    pub const SELECTOR_MUTE: AudioObjectPropertySelector = fcc(b"mute");
    pub const SELECTOR_NAME: AudioObjectPropertySelector = fcc(b"lnam");
    pub const SELECTOR_DEVICES: AudioObjectPropertySelector = fcc(b"dev#");
    pub const SELECTOR_STREAMS: AudioObjectPropertySelector = fcc(b"stm#");

    #[repr(C)]
    pub struct AudioObjectPropertyAddress {
        pub selector: AudioObjectPropertySelector,
        pub scope: AudioObjectPropertyScope,
        pub element: AudioObjectPropertyElement,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        pub fn AudioObjectGetPropertyData(
            object: AudioObjectID,
            address: *const AudioObjectPropertyAddress,
            qualifier_size: u32,
            qualifier_data: *const c_void,
            data_size: *mut u32,
            data: *mut c_void,
        ) -> OSStatus;

        pub fn AudioObjectGetPropertyDataSize(
            object: AudioObjectID,
            address: *const AudioObjectPropertyAddress,
            qualifier_size: u32,
            qualifier_data: *const c_void,
            data_size: *mut u32,
        ) -> OSStatus;

        pub fn AudioObjectSetPropertyData(
            object: AudioObjectID,
            address: *const AudioObjectPropertyAddress,
            qualifier_size: u32,
            qualifier_data: *const c_void,
            data_size: u32,
            data: *const c_void,
        ) -> OSStatus;

        pub fn AudioObjectHasProperty(
            object: AudioObjectID,
            address: *const AudioObjectPropertyAddress,
        ) -> bool;
    }
}

fn ca_list_devices() -> Result<Vec<u32>> {
    use core_audio::*;
    let addr = AudioObjectPropertyAddress {
        selector: SELECTOR_DEVICES,
        scope: SCOPE_GLOBAL,
        element: ELEMENT_MAIN,
    };
    let mut size: u32 = 0;
    let st = unsafe {
        AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, &addr, 0, std::ptr::null(), &mut size)
    };
    if st != 0 {
        bail!("AudioObjectGetPropertyDataSize(devices) failed: status={st}");
    }
    let count = (size / 4) as usize;
    let mut ids = vec![0u32; count];
    let st = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            ids.as_mut_ptr() as *mut std::ffi::c_void,
        )
    };
    if st != 0 {
        bail!("AudioObjectGetPropertyData(devices) failed: status={st}");
    }
    Ok(ids)
}

fn ca_device_has_input_streams(device: u32) -> bool {
    use core_audio::*;
    let addr = AudioObjectPropertyAddress {
        selector: SELECTOR_STREAMS,
        scope: SCOPE_INPUT,
        element: ELEMENT_MAIN,
    };
    let mut size: u32 = 0;
    let st = unsafe {
        AudioObjectGetPropertyDataSize(device, &addr, 0, std::ptr::null(), &mut size)
    };
    if st != 0 {
        return false;
    }
    size > 0
}

fn ca_find_quadcast_device() -> Result<Option<(u32, String)>> {
    for id in ca_list_devices()? {
        if !ca_device_has_input_streams(id) {
            continue;
        }
        let name = ca_input_device_name(id).unwrap_or_default();
        let lower = name.to_lowercase();
        if lower.contains("quadcast") || lower.contains("hyperx") || lower.contains("duocast") {
            return Ok(Some((id, name)));
        }
    }
    Ok(None)
}

fn ca_default_input_device() -> Result<u32> {
    use core_audio::*;
    let addr = AudioObjectPropertyAddress {
        selector: SELECTOR_DEFAULT_INPUT_DEVICE,
        scope: SCOPE_GLOBAL,
        element: ELEMENT_MAIN,
    };
    let mut device_id: u32 = 0;
    let mut size: u32 = 4;
    let status = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut device_id as *mut u32 as *mut std::ffi::c_void,
        )
    };
    if status != 0 {
        bail!("AudioObjectGetPropertyData(default input) failed: status={status}");
    }
    Ok(device_id)
}

fn ca_input_device_name(device: u32) -> Result<String> {
    use core_audio::*;
    let addr = AudioObjectPropertyAddress {
        selector: SELECTOR_NAME,
        scope: SCOPE_GLOBAL,
        element: ELEMENT_MAIN,
    };
    // Property is a CFString reference packed into a pointer-sized slot.
    // To stay dep-free we ask for the name via the legacy 'name' selector
    // returning a UTF-8 buffer instead.
    const SELECTOR_NAME_ASCII: u32 = u32::from_be_bytes(*b"name");
    let addr_ascii = AudioObjectPropertyAddress {
        selector: SELECTOR_NAME_ASCII,
        scope: SCOPE_GLOBAL,
        element: ELEMENT_MAIN,
    };
    let _ = addr; // silence unused warning when we fall through
    let mut buf = [0u8; 256];
    let mut size: u32 = buf.len() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            &addr_ascii,
            0,
            std::ptr::null(),
            &mut size,
            buf.as_mut_ptr() as *mut std::ffi::c_void,
        )
    };
    if status != 0 {
        return Ok(format!("<device {device}>"));
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(size as usize);
    Ok(String::from_utf8_lossy(&buf[..end]).into_owned())
}

fn ca_read_input_mute(device: u32) -> Result<Option<bool>> {
    use core_audio::*;
    let addr = AudioObjectPropertyAddress {
        selector: SELECTOR_MUTE,
        scope: SCOPE_INPUT,
        element: ELEMENT_MAIN,
    };
    if !unsafe { AudioObjectHasProperty(device, &addr) } {
        return Ok(None);
    }
    let mut value: u32 = 0;
    let mut size: u32 = 4;
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut value as *mut u32 as *mut std::ffi::c_void,
        )
    };
    if status != 0 {
        bail!("AudioObjectGetPropertyData(mute) failed: status={status}");
    }
    Ok(Some(value != 0))
}

fn ca_set_input_mute(device: u32, muted: bool) -> Result<()> {
    use core_audio::*;
    let addr = AudioObjectPropertyAddress {
        selector: SELECTOR_MUTE,
        scope: SCOPE_INPUT,
        element: ELEMENT_MAIN,
    };
    if !unsafe { AudioObjectHasProperty(device, &addr) } {
        bail!("input device does not expose a settable mute property");
    }
    let value: u32 = if muted { 1 } else { 0 };
    let status = unsafe {
        AudioObjectSetPropertyData(
            device,
            &addr,
            0,
            std::ptr::null(),
            4,
            &value as *const u32 as *const std::ffi::c_void,
        )
    };
    if status != 0 {
        bail!("AudioObjectSetPropertyData(mute) failed: status={status}");
    }
    Ok(())
}

fn cmd_set_mute(muted: bool) -> Result<()> {
    let (id, name) = ca_find_quadcast_device()?
        .ok_or_else(|| anyhow!("HyperX Quadcast input device not found in Core Audio"))?;
    ca_set_input_mute(id, muted)?;
    println!(
        "{name}: {}",
        if muted { "MUTED" } else { "unmuted" }
    );
    Ok(())
}

fn cmd_mute_toggle() -> Result<()> {
    let (id, _) = ca_find_quadcast_device()?
        .ok_or_else(|| anyhow!("HyperX Quadcast input device not found in Core Audio"))?;
    let cur = ca_read_input_mute(id)?.unwrap_or(false);
    cmd_set_mute(!cur)
}

fn cmd_mute_color(value: Option<&str>) -> Result<()> {
    let mut cfg = load_config()?;
    match value {
        None => {
            match &cfg.on_mute {
                Some(c) => println!("on_mute = {c}"),
                None => println!("on_mute is not set (daemon ignores mute state)"),
            }
            return Ok(());
        }
        Some("none") | Some("off-override") | Some("clear") => {
            cfg.on_mute = None;
            save_config(&cfg)?;
            println!("cleared on_mute");
        }
        Some(v) => {
            // Validate it resolves to a real color.
            let hex = resolve_color_or_preset(v)?;
            Color::from_hex(&hex)?;
            cfg.on_mute = Some(v.to_string());
            save_config(&cfg)?;
            println!("on_mute = {v}");
        }
    }
    Ok(())
}

fn cmd_audio_state() -> Result<()> {
    let default_id = ca_default_input_device().ok();
    let default_name = default_id.and_then(|id| ca_input_device_name(id).ok());
    if let (Some(id), Some(name)) = (default_id, default_name.as_ref()) {
        println!("system default input device: {name} (id={id})");
    }

    println!("\nAll input devices and their mute property state:");
    let mut quadcast: Option<(u32, String)> = None;
    for id in ca_list_devices()? {
        if !ca_device_has_input_streams(id) {
            continue;
        }
        let name = ca_input_device_name(id).unwrap_or_else(|_| format!("<{id}>"));
        let mute = match ca_read_input_mute(id) {
            Ok(Some(m)) => format!("mute={}", if m { "MUTED" } else { "unmuted" }),
            Ok(None) => "no mute property".into(),
            Err(e) => format!("err: {e}"),
        };
        println!("  [{id:>3}] {name}  ({mute})");
        let lower = name.to_lowercase();
        if quadcast.is_none()
            && (lower.contains("quadcast") || lower.contains("hyperx") || lower.contains("duocast"))
        {
            quadcast = Some((id, name));
        }
    }

    let (qid, qname) = match quadcast {
        Some(q) => q,
        None => {
            bail!(
                "no HyperX Quadcast / Duocast input device found in Core Audio. \
                 The mic may be on the USB bus but not registered as an audio device — \
                 check System Settings > Sound > Input."
            );
        }
    };

    println!(
        "\nWatching {qname} (id={qid}) every 200ms. Tap the mute button on the mic, or mute via macOS Control Center, or mute in a call app."
    );
    println!("Ctrl-C to stop.\n");
    let running = install_signal_handler()?;
    let mut last = ca_read_input_mute(qid)?;
    while running.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
        let cur = match ca_read_input_mute(qid) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("read error: {e}");
                continue;
            }
        };
        if cur != last {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            println!(
                "{now}ms: mute changed {} -> {}",
                last.map(|m| if m { "MUTED" } else { "unmuted" })
                    .unwrap_or("?"),
                cur.map(|m| if m { "MUTED" } else { "unmuted" })
                    .unwrap_or("?")
            );
            last = cur;
        }
    }
    Ok(())
}

fn install_signal_handler() -> Result<Arc<AtomicBool>> {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
        .context("installing signal handler")?;
    Ok(running)
}

// --- Commands ---------------------------------------------------------------

#[derive(Parser)]
#[command(name = "quadcastctl", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum PresetCmd {
    /// List built-in and user-defined presets.
    List,
    /// Add or override a preset.
    Add { name: String, color: String },
    /// Remove a user preset.
    Remove { name: String },
}

#[derive(Subcommand)]
enum Cmd {
    /// List connected HyperX microphones.
    List,
    /// Update the persisted config. Daemon picks up changes within ~1s.
    /// Examples: `set red` (solid), `set cycle red green blue`, `set pulse hivenet`.
    Set {
        /// Either a color/preset (sets solid mode) or one of: solid, blink, cycle, wave, pulse, lightning.
        first: String,
        /// Additional colors for non-solid modes.
        colors: Vec<String>,
        #[arg(short, long, default_value_t = 100)]
        brightness: u8,
        #[arg(short, long, default_value_t = 81)]
        speed: u8,
        #[arg(short, long, default_value_t = 10)]
        delay: u8,
    },
    /// Run a foreground loop using the given args (same syntax as `set`). Ctrl-C to stop.
    Solid {
        first: String,
        colors: Vec<String>,
        #[arg(short, long, default_value_t = 100)]
        brightness: u8,
        #[arg(short, long, default_value_t = 81)]
        speed: u8,
        #[arg(short, long, default_value_t = 10)]
        delay: u8,
    },
    /// Print the current config file contents.
    Show,
    /// Open the macOS system color picker; chosen color becomes a solid setting.
    Pick {
        #[arg(short, long, default_value_t = 100)]
        brightness: u8,
    },
    /// Manage named color presets.
    Preset {
        #[command(subcommand)]
        action: PresetCmd,
    },
    /// Run as daemon: read config, drive lights, hot-reload on config change.
    Daemon,
    /// Install launchd LaunchAgent.
    Install,
    /// Remove the launchd LaunchAgent.
    Uninstall,
    /// Start the launchd-managed daemon.
    Start,
    /// Stop the launchd-managed daemon.
    Stop,
    /// Restart the launchd-managed daemon.
    Restart,
    /// Show launchd job status.
    Status,
    /// Probe macOS Core Audio for the input device's mute state.
    /// Useful to verify the hardware tap-to-mute is reflected in the system.
    AudioState,
    /// Set, clear, or show the color/preset shown when the mic is muted.
    /// Examples: `mute-color red`, `mute-color none`, `mute-color`.
    MuteColor {
        /// A color, preset, or `none` to disable mute-override. Omit to print current value.
        value: Option<String>,
    },
    /// Mute the Quadcast input device via Core Audio (system-wide).
    Mute,
    /// Unmute the Quadcast input device via Core Audio (system-wide).
    Unmute,
    /// Toggle the Quadcast input device's mute state.
    MuteToggle,
}

/// Build a Config from CLI args. If `first` is a mode name, it's the mode and
/// `colors` are the colors. Otherwise `first` is treated as the (only) color
/// in solid mode.
fn config_from_args(
    first: &str,
    colors: &[String],
    brightness: u8,
    speed: u8,
    delay: u8,
) -> Result<Config> {
    let (mode, color_args) = if let Some(m) = Mode::parse(first) {
        (m, colors.to_vec())
    } else {
        if !colors.is_empty() {
            bail!(
                "extra colors given but first arg {first:?} is not a mode. \
                 Use a mode name (solid, blink, cycle, wave, pulse, lightning) \
                 to pass multiple colors."
            );
        }
        (Mode::Solid, vec![first.to_string()])
    };
    if color_args.is_empty() {
        bail!("at least one color required");
    }
    // Validate every color resolves now so the daemon doesn't fail on reload.
    for c in &color_args {
        let hex = resolve_color_or_preset(c)?;
        Color::from_hex(&hex)?;
    }
    if brightness > 100 {
        bail!("brightness must be 0-100");
    }
    if speed > 100 {
        bail!("speed must be 0-100");
    }
    if delay > 100 {
        bail!("delay must be 0-100");
    }
    // Preserve on_mute across `set` invocations so the user only has to set it once.
    let existing_on_mute = load_config().ok().and_then(|c| c.on_mute);
    Ok(Config {
        mode,
        colors: color_args,
        color: None,
        brightness,
        speed,
        delay,
        on_mute: existing_on_mute,
    })
}

fn cmd_list() -> Result<()> {
    let (dev, model) = find_mic()?;
    let desc = dev.device_descriptor()?;
    println!(
        "{} — VID:PID {:04x}:{:04x} (bus {} address {})",
        model.name(),
        desc.vendor_id(),
        desc.product_id(),
        dev.bus_number(),
        dev.address()
    );
    Ok(())
}

fn cmd_set(
    first: &str,
    colors: &[String],
    brightness: u8,
    speed: u8,
    delay: u8,
) -> Result<()> {
    let cfg = config_from_args(first, colors, brightness, speed, delay)?;
    save_config(&cfg)?;
    println!("wrote {} ({})", config_path()?.display(), cfg.summary());
    Ok(())
}

fn cmd_solid_foreground(
    first: &str,
    colors: &[String],
    brightness: u8,
    speed: u8,
    delay: u8,
) -> Result<()> {
    let cfg = config_from_args(first, colors, brightness, speed, delay)?;
    let frames = generate_frames(&cfg)?;
    let (handle, model) = open_mic()?;
    if matches!(model, Model::Quadcast2S) {
        bail!("Quadcast 2S uses a different protocol — not implemented yet");
    }
    eprintln!(
        "Driving {} with {} ({} frames). Ctrl-C to stop.",
        model.name(),
        cfg.summary(),
        frames.len()
    );
    let running = install_signal_handler()?;
    let header = build_header_packet();
    let mut idx = 0usize;
    while running.load(Ordering::SeqCst) {
        let packet = build_data_packet(&frames[idx % frames.len()]);
        send_control_packet(&handle, &header)?;
        send_control_packet(&handle, &packet)?;
        std::thread::sleep(REFRESH_INTERVAL);
        idx = idx.wrapping_add(1);
    }
    eprintln!("Stopped.");
    Ok(())
}

fn cmd_show() -> Result<()> {
    let path = config_path()?;
    if !path.exists() {
        println!("no config at {} (using defaults)", path.display());
        let cfg = Config::default();
        println!("{}", toml::to_string_pretty(&cfg)?);
        return Ok(());
    }
    let text = fs::read_to_string(&path)?;
    println!("{}:\n{}", path.display(), text);
    Ok(())
}

fn build_mute_frames(cfg: &Config) -> Option<Vec<Frame>> {
    let preset = cfg.on_mute.as_deref()?;
    let hex = resolve_color_or_preset(preset).ok()?;
    let color = Color::from_hex(&hex).ok()?.scaled(cfg.brightness);
    Some(vec![frame_pair(color, color)])
}

fn cmd_daemon() -> Result<()> {
    let (handle, model) = open_mic()?;
    if matches!(model, Model::Quadcast2S) {
        bail!("Quadcast 2S uses a different protocol — not implemented yet");
    }
    let running = install_signal_handler()?;
    let mut cfg = load_config()?;
    let mut normal_frames = generate_frames(&cfg)?;
    let mut mute_frames = build_mute_frames(&cfg);
    let quadcast_audio = ca_find_quadcast_device().ok().flatten();
    let mut muted = quadcast_audio
        .as_ref()
        .and_then(|(id, _)| ca_read_input_mute(*id).ok().flatten())
        .unwrap_or(false);
    let mut last_mtime = config_mtime();
    let mut last_poll = SystemTime::now();
    let header = build_header_packet();
    eprintln!(
        "[quadcastctl] daemon driving {} with {} ({} frames)",
        model.name(),
        cfg.summary(),
        normal_frames.len()
    );
    if let Some((id, name)) = &quadcast_audio {
        eprintln!(
            "[quadcastctl] watching audio device {name} (id={id}) for mute. on_mute={}",
            cfg.on_mute.as_deref().unwrap_or("(unset)")
        );
    } else {
        eprintln!("[quadcastctl] mic not present in Core Audio; mute integration disabled");
    }
    let mut idx = 0usize;
    while running.load(Ordering::SeqCst) {
        let active_frames = if muted && mute_frames.is_some() {
            mute_frames.as_ref().unwrap()
        } else {
            &normal_frames
        };
        let packet = build_data_packet(&active_frames[idx % active_frames.len()]);
        send_control_packet(&handle, &header)?;
        send_control_packet(&handle, &packet)?;
        std::thread::sleep(REFRESH_INTERVAL);
        idx = idx.wrapping_add(1);

        if last_poll
            .elapsed()
            .map(|e| e >= CONFIG_POLL_INTERVAL)
            .unwrap_or(true)
        {
            last_poll = SystemTime::now();
            // Mute state poll
            if let Some((id, _)) = &quadcast_audio {
                let now_mute = ca_read_input_mute(*id).ok().flatten().unwrap_or(false);
                if now_mute != muted {
                    muted = now_mute;
                    idx = 0;
                    eprintln!(
                        "[quadcastctl] mute -> {}",
                        if muted { "MUTED" } else { "unmuted" }
                    );
                }
            }
            // Config-file mtime poll
            let now_mtime = config_mtime();
            if now_mtime != last_mtime {
                last_mtime = now_mtime;
                match load_config().and_then(|c| {
                    let f = generate_frames(&c)?;
                    Ok((c, f))
                }) {
                    Ok((new_cfg, new_frames)) => {
                        cfg = new_cfg;
                        normal_frames = new_frames;
                        mute_frames = build_mute_frames(&cfg);
                        idx = 0;
                        eprintln!(
                            "[quadcastctl] reloaded: {} ({} frames, on_mute={})",
                            cfg.summary(),
                            normal_frames.len(),
                            cfg.on_mute.as_deref().unwrap_or("(unset)")
                        );
                    }
                    Err(e) => {
                        eprintln!("[quadcastctl] config reload failed, keeping previous: {e:#}");
                    }
                }
            }
        }
    }
    eprintln!("[quadcastctl] daemon exiting cleanly");
    Ok(())
}

fn pick_color_macos() -> Result<Color> {
    let script = r#"
        try
            set c to choose color
            set r to item 1 of c
            set g to item 2 of c
            set b to item 3 of c
            return (r as text) & "," & (g as text) & "," & (b as text)
        on error number -128
            return "CANCELLED"
        end try
    "#;
    let out = Command::new("osascript")
        .args(["-e", script])
        .output()
        .context("running osascript (macOS only)")?;
    if !out.status.success() {
        bail!(
            "osascript failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s == "CANCELLED" {
        bail!("color picker cancelled");
    }
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 3 {
        bail!("unexpected osascript output: {s:?}");
    }
    let parse = |p: &str| -> Result<u8> {
        let n: u32 = p.trim().parse().context("parsing color component")?;
        Ok((n / 257).min(255) as u8)
    };
    Ok(Color {
        r: parse(parts[0])?,
        g: parse(parts[1])?,
        b: parse(parts[2])?,
    })
}

fn cmd_pick(brightness: u8) -> Result<()> {
    if brightness > 100 {
        bail!("brightness must be 0-100");
    }
    let color = pick_color_macos()?;
    let hex = format!("{:02x}{:02x}{:02x}", color.r, color.g, color.b);
    let existing_on_mute = load_config().ok().and_then(|c| c.on_mute);
    let cfg = Config {
        mode: Mode::Solid,
        colors: vec![hex.clone()],
        color: None,
        brightness,
        speed: 81,
        delay: 10,
        on_mute: existing_on_mute,
    };
    save_config(&cfg)?;
    println!("picked #{hex} (brightness {brightness}) — daemon picks up within ~1s");
    Ok(())
}

fn cmd_preset_list() -> Result<()> {
    let user = load_user_presets();
    println!("Built-in:");
    for (name, hex) in BUILTIN_PRESETS {
        let overridden = user.presets.contains_key(*name);
        if overridden {
            println!("  {name:10} {hex}  (overridden by user)");
        } else {
            println!("  {name:10} {hex}");
        }
    }
    if !user.presets.is_empty() {
        println!("\nUser:");
        for (name, hex) in &user.presets {
            println!("  {name:10} {hex}");
        }
    }
    Ok(())
}

fn cmd_preset_add(name: &str, color: &str) -> Result<()> {
    if name.is_empty() || name.contains(char::is_whitespace) {
        bail!("preset name must be non-empty and contain no whitespace");
    }
    let hex = resolve_color_or_preset(color)?;
    let mut user = load_user_presets();
    user.presets.insert(name.to_string(), hex.clone());
    save_user_presets(&user)?;
    println!("added preset {name} = {hex}");
    Ok(())
}

fn cmd_preset_remove(name: &str) -> Result<()> {
    let mut user = load_user_presets();
    if user.presets.remove(name).is_some() {
        save_user_presets(&user)?;
        println!("removed user preset {name}");
    } else if BUILTIN_PRESETS.iter().any(|(n, _)| *n == name) {
        bail!(
            "{name:?} is a built-in preset; built-ins cannot be removed (override with `preset add`)"
        );
    } else {
        bail!("no preset named {name:?}");
    }
    Ok(())
}

// --- launchd ----------------------------------------------------------------

fn launchd_plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

fn launchd_log_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    Ok(home.join("Library/Logs/quadcastctl"))
}

fn render_plist(exe: &Path, log_dir: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>5</integer>
    <key>StandardOutPath</key>
    <string>{log_dir}/quadcastctl.out.log</string>
    <key>StandardErrorPath</key>
    <string>{log_dir}/quadcastctl.err.log</string>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        exe = exe.display(),
        log_dir = log_dir.display(),
    )
}

fn launchctl_domain() -> String {
    let uid = unsafe { libc::getuid() };
    format!("gui/{uid}")
}

fn launchctl_target() -> String {
    format!("{}/{LAUNCHD_LABEL}", launchctl_domain())
}

fn run_launchctl(args: &[&str]) -> Result<()> {
    let status = Command::new("launchctl")
        .args(args)
        .status()
        .with_context(|| format!("running launchctl {args:?}"))?;
    if !status.success() {
        bail!("launchctl {args:?} failed: {status}");
    }
    Ok(())
}

fn cmd_install() -> Result<()> {
    let exe = std::env::current_exe().context("locating own exe")?;
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    let plist_path = launchd_plist_path()?;
    let log_dir = launchd_log_dir()?;
    fs::create_dir_all(&log_dir).with_context(|| format!("creating {}", log_dir.display()))?;
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&plist_path, render_plist(&exe, &log_dir))
        .with_context(|| format!("writing {}", plist_path.display()))?;
    println!("wrote {}", plist_path.display());

    let domain = launchctl_domain();
    let _ = Command::new("launchctl")
        .args(["bootout", &launchctl_target()])
        .status();
    run_launchctl(&["bootstrap", &domain, plist_path.to_str().unwrap()])?;
    run_launchctl(&["enable", &launchctl_target()])?;
    run_launchctl(&["kickstart", "-k", &launchctl_target()])?;
    println!("daemon installed and started.");
    println!("logs: {}/quadcastctl.{{out,err}}.log", log_dir.display());
    Ok(())
}

fn cmd_uninstall() -> Result<()> {
    let plist_path = launchd_plist_path()?;
    let _ = Command::new("launchctl")
        .args(["bootout", &launchctl_target()])
        .status();
    if plist_path.exists() {
        fs::remove_file(&plist_path)
            .with_context(|| format!("removing {}", plist_path.display()))?;
        println!("removed {}", plist_path.display());
    } else {
        println!("plist not present at {}", plist_path.display());
    }
    Ok(())
}

fn cmd_start() -> Result<()> {
    run_launchctl(&["kickstart", &launchctl_target()])
}

fn cmd_stop() -> Result<()> {
    run_launchctl(&["kill", "TERM", &launchctl_target()])
}

fn cmd_restart() -> Result<()> {
    run_launchctl(&["kickstart", "-k", &launchctl_target()])
}

fn cmd_status() -> Result<()> {
    let status = Command::new("launchctl")
        .args(["print", &launchctl_target()])
        .status()
        .context("running launchctl print")?;
    if !status.success() {
        bail!("launchctl print failed (daemon may not be installed)");
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::List => cmd_list(),
        Cmd::Set {
            first,
            colors,
            brightness,
            speed,
            delay,
        } => cmd_set(&first, &colors, brightness, speed, delay),
        Cmd::Solid {
            first,
            colors,
            brightness,
            speed,
            delay,
        } => cmd_solid_foreground(&first, &colors, brightness, speed, delay),
        Cmd::Show => cmd_show(),
        Cmd::Pick { brightness } => cmd_pick(brightness),
        Cmd::Preset { action } => match action {
            PresetCmd::List => cmd_preset_list(),
            PresetCmd::Add { name, color } => cmd_preset_add(&name, &color),
            PresetCmd::Remove { name } => cmd_preset_remove(&name),
        },
        Cmd::Daemon => cmd_daemon(),
        Cmd::Install => cmd_install(),
        Cmd::Uninstall => cmd_uninstall(),
        Cmd::Start => cmd_start(),
        Cmd::Stop => cmd_stop(),
        Cmd::Restart => cmd_restart(),
        Cmd::Status => cmd_status(),
        Cmd::AudioState => cmd_audio_state(),
        Cmd::MuteColor { value } => cmd_mute_color(value.as_deref()),
        Cmd::Mute => cmd_set_mute(true),
        Cmd::Unmute => cmd_set_mute(false),
        Cmd::MuteToggle => cmd_mute_toggle(),
    }
}
