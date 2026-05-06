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
    Ok(Config {
        mode,
        colors: color_args,
        color: None,
        brightness,
        speed,
        delay,
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

fn cmd_daemon() -> Result<()> {
    let (handle, model) = open_mic()?;
    if matches!(model, Model::Quadcast2S) {
        bail!("Quadcast 2S uses a different protocol — not implemented yet");
    }
    let running = install_signal_handler()?;
    let mut cfg = load_config()?;
    let mut frames = generate_frames(&cfg)?;
    let mut last_mtime = config_mtime();
    let mut last_poll = SystemTime::now();
    let header = build_header_packet();
    eprintln!(
        "[quadcastctl] daemon driving {} with {} ({} frames)",
        model.name(),
        cfg.summary(),
        frames.len()
    );
    let mut idx = 0usize;
    while running.load(Ordering::SeqCst) {
        let packet = build_data_packet(&frames[idx % frames.len()]);
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
            let now_mtime = config_mtime();
            if now_mtime != last_mtime {
                last_mtime = now_mtime;
                match load_config().and_then(|c| {
                    let f = generate_frames(&c)?;
                    Ok((c, f))
                }) {
                    Ok((new_cfg, new_frames)) => {
                        cfg = new_cfg;
                        frames = new_frames;
                        idx = 0;
                        eprintln!(
                            "[quadcastctl] reloaded: {} ({} frames)",
                            cfg.summary(),
                            frames.len()
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
    let cfg = Config {
        mode: Mode::Solid,
        colors: vec![hex.clone()],
        color: None,
        brightness,
        speed: 81,
        delay: 10,
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
    }
}
