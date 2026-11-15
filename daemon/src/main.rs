#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]

use std::env;
use std::fmt;
use std::fs::{self, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SYSFS_POWER_SUPPLY: &str = "/sys/class/power_supply";
const CONFIG_FILE: &str = "/etc/u4-charge.conf";
const STATE_FILE: &str = "/var/lib/u4-charge/state.conf";
const SOCKET_PATH: &str = "/run/u4-charge/control.sock";
const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

const VALID_START_THRESHOLDS: &[u8] = &[40, 50, 60, 70, 80, 95];
const VALID_END_THRESHOLDS: &[u8] = &[60, 70, 80, 90, 100];

const TICK_INTERVAL_SECS: u64 = 3;
const WAKE_SKEW_THRESHOLD_SECS: u64 = 12;
const FULL_CONFIRM_TICKS: u8 = 2;
const UNPLUG_CONFIRM_TICKS: u8 = 2;
const OVERCHARGE_WATCHDOG_TICKS: u8 = 5;
const WATCHDOG_COOLDOWN_SECS: u64 = 60;
const SOCKET_TIMEOUT_MS: u64 = 500;
const MAX_SOCKET_CMD_BYTES: u64 = 64;

#[derive(Debug)]
enum Error {
    Io(io::Error),
    ParseInt(std::num::ParseIntError),
    InvalidThreshold(String),
    SysfsNotFound(String),
    Protocol(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "Ошибка ввода-вывода: {e}"),
            Error::ParseInt(e) => write!(f, "Ошибка разбора числа: {e}"),
            Error::InvalidThreshold(msg) => write!(f, "Некорректный порог: {msg}"),
            Error::SysfsNotFound(msg) => write!(f, "Интерфейс sysfs не найден: {msg}"),
            Error::Protocol(msg) => write!(f, "Ошибка протокола сокета: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<std::num::ParseIntError> for Error {
    fn from(e: std::num::ParseIntError) -> Self {
        Error::ParseInt(e)
    }
}

#[derive(Debug, Clone)]
struct Config {
    target_start: u8,
    target_end: u8,
    balance_interval_days: u64,
    balance_soak_minutes: u64,
    full_timeout_hours: u64,
    notify_on_start: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            target_start: 60,
            target_end: 80,
            balance_interval_days: 0, // Realistic default: auto-balance disabled unless opted in
            balance_soak_minutes: 90,
            full_timeout_hours: 6,
            notify_on_start: true,
        }
    }
}

impl Config {
    fn load() -> Self {
        Self::load_from(Path::new(CONFIG_FILE))
    }

    fn load_from(path: &Path) -> Self {
        let mut cfg = Config::default();
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = trimmed.split_once('=') {
                    let (k, v) = (k.trim(), v.trim());
                    match k {
                        "START_THRESHOLD" => {
                            if let Ok(val) = v.parse::<u8>() {
                                cfg.target_start = val;
                            }
                        }
                        "END_THRESHOLD" => {
                            if let Ok(val) = v.parse::<u8>() {
                                cfg.target_end = val;
                            }
                        }
                        "BALANCE_INTERVAL_DAYS" => {
                            if let Ok(val) = v.parse::<u64>() {
                                cfg.balance_interval_days = val;
                            }
                        }
                        "BALANCE_SOAK_MINUTES" => {
                            if let Ok(val) = v.parse::<u64>() {
                                cfg.balance_soak_minutes = val;
                            }
                        }
                        "FULL_TIMEOUT_HOURS" => {
                            if let Ok(val) = v.parse::<u64>() {
                                cfg.full_timeout_hours = val.max(1);
                            }
                        }
                        "NOTIFY_ON_START" => {
                            cfg.notify_on_start = matches!(v, "true" | "1" | "yes");
                        }
                        _ => {}
                    }
                }
            }
        }
        if !is_valid_pair(cfg.target_start, cfg.target_end) {
            cfg.target_start = 60;
            cfg.target_end = 80;
        }
        cfg
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveMode {
    Normal,
    FullOnce,
    BalanceOnce,
}

impl ActiveMode {
    fn as_str(self) -> &'static str {
        match self {
            ActiveMode::Normal => "Normal",
            ActiveMode::FullOnce => "FullOnce",
            ActiveMode::BalanceOnce => "BalanceOnce",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "FullOnce" => ActiveMode::FullOnce,
            "BalanceOnce" => ActiveMode::BalanceOnce,
            _ => ActiveMode::Normal,
        }
    }
}

#[derive(Debug, Clone)]
struct DaemonState {
    mode: ActiveMode,
    mode_started_epoch: u64,
    soak_started_epoch: Option<u64>,
    last_balance_timestamp: u64,
    boot_id: String,
    active_start: u8,
    active_end: u8,
    config_start: u8,
    config_end: u8,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            mode: ActiveMode::Normal,
            mode_started_epoch: 0,
            soak_started_epoch: None,
            last_balance_timestamp: 0,
            boot_id: String::new(),
            active_start: 60,
            active_end: 80,
            config_start: 60,
            config_end: 80,
        }
    }
}

impl DaemonState {
    fn load_from(path: &Path, default_start: u8, default_end: u8) -> Self {
        let mut st = DaemonState {
            active_start: default_start,
            active_end: default_end,
            config_start: default_start,
            config_end: default_end,
            ..DaemonState::default()
        };
        let mut has_saved_config = false;
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines() {
                if let Some((k, v)) = line.trim().split_once('=') {
                    let (k, v) = (k.trim(), v.trim());
                    match k {
                        "MODE" => st.mode = ActiveMode::from_str(v),
                        "MODE_STARTED_EPOCH" => {
                            if let Ok(val) = v.parse::<u64>() {
                                st.mode_started_epoch = val;
                            }
                        }
                        "SOAK_STARTED_EPOCH" => {
                            st.soak_started_epoch = v.parse::<u64>().ok().filter(|&x| x > 0);
                        }
                        "LAST_BALANCE_TIMESTAMP" => {
                            if let Ok(val) = v.parse::<u64>() {
                                st.last_balance_timestamp = val;
                            }
                        }
                        "BOOT_ID" => st.boot_id = v.to_string(),
                        "ACTIVE_START" => {
                            if let Ok(val) = v.parse::<u8>() {
                                st.active_start = val;
                            }
                        }
                        "ACTIVE_END" => {
                            if let Ok(val) = v.parse::<u8>() {
                                st.active_end = val;
                            }
                        }
                        "CONFIG_START" => {
                            if let Ok(val) = v.parse::<u8>() {
                                st.config_start = val;
                                has_saved_config = true;
                            }
                        }
                        "CONFIG_END" => {
                            if let Ok(val) = v.parse::<u8>() {
                                st.config_end = val;
                                has_saved_config = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // If admin edited /etc/u4-charge.conf since last run, adopt the new config thresholds.
        if has_saved_config && (st.config_start != default_start || st.config_end != default_end) {
            st.active_start = default_start;
            st.active_end = default_end;
            st.config_start = default_start;
            st.config_end = default_end;
        } else if !is_valid_pair(st.active_start, st.active_end) {
            st.active_start = default_start;
            st.active_end = default_end;
        }
        st
    }

    fn save_to(&self, path: &Path) -> Result<(), Error> {
        let parent = path.parent().unwrap_or_else(|| Path::new("/var/lib/u4-charge"));
        if !parent.exists() {
            fs::create_dir_all(parent)?;
        }
        let soak_val = self.soak_started_epoch.unwrap_or(0);
        let content = format!(
            "MODE={}\nMODE_STARTED_EPOCH={}\nSOAK_STARTED_EPOCH={}\nLAST_BALANCE_TIMESTAMP={}\nBOOT_ID={}\nACTIVE_START={}\nACTIVE_END={}\nCONFIG_START={}\nCONFIG_END={}\n",
            self.mode.as_str(),
            self.mode_started_epoch,
            soak_val,
            self.last_balance_timestamp,
            self.boot_id,
            self.active_start,
            self.active_end,
            self.config_start,
            self.config_end
        );
        let tmp_path = parent.join(format!(".state.tmp.{}", std::process::id()));
        fs::write(&tmp_path, content)?;
        fs::rename(&tmp_path, path)?;
        Ok(())
    }

    fn clear_oneshot_if_stale(&mut self, current_boot_id: &str, now_epoch: u64, timeout_secs: u64) -> bool {
        if self.mode == ActiveMode::Normal {
            if self.boot_id != current_boot_id && !current_boot_id.is_empty() {
                self.boot_id = current_boot_id.to_string();
                return true;
            }
            return false;
        }
        let boot_changed = !current_boot_id.is_empty() && self.boot_id != current_boot_id;
        let expired = self.mode_started_epoch == 0
            || now_epoch.saturating_sub(self.mode_started_epoch) >= timeout_secs;
        if boot_changed || expired {
            self.mode = ActiveMode::Normal;
            self.mode_started_epoch = 0;
            self.soak_started_epoch = None;
            if !current_boot_id.is_empty() {
                self.boot_id = current_boot_id.to_string();
            }
            return true;
        }
        false
    }
}

fn is_valid_pair(start: u8, end: u8) -> bool {
    VALID_START_THRESHOLDS.contains(&start) && VALID_END_THRESHOLDS.contains(&end) && start < end
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_boot_id() -> String {
    fs::read_to_string(BOOT_ID_PATH)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn detect_wake(
    last_wall: SystemTime,
    last_mono: Instant,
    now_wall: SystemTime,
    now_mono: Instant,
) -> bool {
    // NTP Step Guard: backwards system-clock jumps return Err and are safely ignored.
    if let Ok(wall_delta) = now_wall.duration_since(last_wall) {
        let mono_delta = now_mono.saturating_duration_since(last_mono);
        return wall_delta > mono_delta + Duration::from_secs(WAKE_SKEW_THRESHOLD_SECS);
    }
    false
}

struct PowerSupply {
    root: PathBuf,
}

impl PowerSupply {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn system() -> Self {
        Self::new(PathBuf::from(SYSFS_POWER_SUPPLY))
    }

    fn bat_dir(&self) -> PathBuf {
        self.root.join("BAT0")
    }

    fn read_attr_string(&self, dev: &Path, attr: &str) -> Result<String, Error> {
        let path = dev.join(attr);
        let s = fs::read_to_string(&path)
            .map_err(|e| Error::SysfsNotFound(format!("{}: {e}", path.display())))?;
        Ok(s.trim().to_string())
    }

    fn write_attr_string(&self, dev: &Path, attr: &str, val: &str) -> Result<(), Error> {
        let path = dev.join(attr);
        fs::write(&path, val).map_err(|e| {
            Error::Io(io::Error::new(
                e.kind(),
                format!("Ошибка записи '{val}' в {}: {e}", path.display()),
            ))
        })
    }

    fn read_u8(&self, attr: &str) -> Result<u8, Error> {
        self.read_attr_string(&self.bat_dir(), attr)?
            .parse::<u8>()
            .map_err(Error::from)
    }

    fn read_i64(&self, attr: &str) -> Result<i64, Error> {
        self.read_attr_string(&self.bat_dir(), attr)?
            .parse::<i64>()
            .map_err(Error::from)
    }

    fn is_bat_available(&self) -> bool {
        self.bat_dir().join("status").exists()
    }

    fn has_charge_control(&self) -> bool {
        let bat = self.bat_dir();
        bat.join("charge_control_end_threshold").exists()
            && bat.join("charge_control_start_threshold").exists()
            && bat.join("charge_type").exists()
    }

    fn ac_online(&self) -> Option<bool> {
        for candidate in ["AC", "AC0", "ADP0", "ADP1"] {
            let p = self.root.join(candidate).join("online");
            if let Ok(val) = fs::read_to_string(&p) {
                return Some(val.trim() == "1");
            }
        }
        if let Ok(entries) = fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Ok(kind) = fs::read_to_string(path.join("type"))
                    && kind.trim() == "Mains"
                    && let Ok(val) = fs::read_to_string(path.join("online"))
                {
                    return Some(val.trim() == "1");
                }
            }
        }
        None
    }

    fn get_start_threshold(&self) -> Result<u8, Error> {
        self.read_u8("charge_control_start_threshold")
    }

    fn get_end_threshold(&self) -> Result<u8, Error> {
        self.read_u8("charge_control_end_threshold")
    }

    fn get_charge_type(&self) -> Result<String, Error> {
        self.read_attr_string(&self.bat_dir(), "charge_type")
    }

    fn set_thresholds(&self, start: u8, end: u8) -> Result<(), Error> {
        if !VALID_START_THRESHOLDS.contains(&start) {
            return Err(Error::InvalidThreshold(format!(
                "Стартовый порог {start}% не поддерживается (допустимо: {VALID_START_THRESHOLDS:?})"
            )));
        }
        if !VALID_END_THRESHOLDS.contains(&end) {
            return Err(Error::InvalidThreshold(format!(
                "Конечный порог {end}% не поддерживается (допустимо: {VALID_END_THRESHOLDS:?})"
            )));
        }
        if start >= end {
            return Err(Error::InvalidThreshold(format!(
                "START ({start}%) должен быть строго меньше END ({end}%)"
            )));
        }
        let bat = self.bat_dir();
        let cur_start = self.get_start_threshold().unwrap_or(0);
        let cur_end = self.get_end_threshold().unwrap_or(100);
        let cur_type = self.get_charge_type().unwrap_or_default();
        // Write order avoids intermediate start >= end clamping in kernel helper
        if start < cur_end {
            if cur_start != start {
                self.write_attr_string(&bat, "charge_control_start_threshold", &start.to_string())?;
            }
            if cur_end != end {
                self.write_attr_string(&bat, "charge_control_end_threshold", &end.to_string())?;
            }
        } else {
            if cur_end != end {
                self.write_attr_string(&bat, "charge_control_end_threshold", &end.to_string())?;
            }
            if cur_start != start {
                self.write_attr_string(&bat, "charge_control_start_threshold", &start.to_string())?;
            }
        }
        if cur_type != "Custom" {
            self.write_attr_string(&bat, "charge_type", "Custom")?;
        }
        Ok(())
    }

    fn set_standard_charging(&self) -> Result<(), Error> {
        let bat = self.bat_dir();
        if self.get_charge_type().unwrap_or_default() != "Standard" {
            self.write_attr_string(&bat, "charge_type", "Standard")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SocketCommand {
    Status,
    FullOnce,
    BalanceOnce,
    Cancel,
    Set { start: u8, end: u8 },
}

fn parse_socket_command(raw: &str) -> Result<SocketCommand, Error> {
    let trimmed = raw.trim();
    match trimmed {
        "STATUS" => Ok(SocketCommand::Status),
        "FULL_ONCE" => Ok(SocketCommand::FullOnce),
        "BALANCE_ONCE" => Ok(SocketCommand::BalanceOnce),
        "CANCEL" => Ok(SocketCommand::Cancel),
        other => {
            if let Some(rest) = other.strip_prefix("SET ") {
                let mut parts = rest.split_whitespace();
                if let (Some(s_str), Some(e_str), None) =
                    (parts.next(), parts.next(), parts.next())
                    && let (Ok(start), Ok(end)) = (s_str.parse::<u8>(), e_str.parse::<u8>())
                {
                    if !is_valid_pair(start, end) {
                        return Err(Error::InvalidThreshold(format!(
                            "Недопустимые пороги START={start}%, END={end}%"
                        )));
                    }
                    return Ok(SocketCommand::Set { start, end });
                }
            }
            Err(Error::Protocol(format!("Неизвестная команда: {other}")))
        }
    }
}

struct RuntimeTicks {
    full_confirm_ticks: u8,
    unplug_ticks: u8,
    overcharge_ticks: u8,
    last_watchdog_resync: Option<Instant>,
    recheck_forced: bool,
}

impl RuntimeTicks {
    fn new() -> Self {
        Self {
            full_confirm_ticks: 0,
            unplug_ticks: 0,
            overcharge_ticks: 0,
            last_watchdog_resync: None,
            recheck_forced: true,
        }
    }

    fn reset_counters(&mut self) {
        self.full_confirm_ticks = 0;
        self.unplug_ticks = 0;
        self.overcharge_ticks = 0;
    }
}

struct DaemonEngine {
    ps: PowerSupply,
    cfg: Config,
    state: DaemonState,
    state_path: PathBuf,
    ticks: RuntimeTicks,
}

impl DaemonEngine {
    fn new(ps: PowerSupply, cfg: Config, state: DaemonState, state_path: PathBuf) -> Self {
        Self {
            ps,
            cfg,
            state,
            state_path,
            ticks: RuntimeTicks::new(),
        }
    }

    fn format_status_line(&self, now_epoch: u64) -> String {
        let bat = self.ps.bat_dir();
        let cap = self.ps.read_u8("capacity").unwrap_or(0);
        let status = self
            .ps
            .read_attr_string(&bat, "status")
            .unwrap_or_else(|_| "Unknown".to_string());
        let current_ma = self.ps.read_i64("current_now").unwrap_or(0) / 1000;
        let (balance_due, days_since) = self.balance_due_info(now_epoch);
        format!(
            "MODE={}\nSTART={}\nEND={}\nCAP={}\nSTATUS={}\nCURRENT_MA={}\nBALANCE_DUE={}\nDAYS_SINCE_BALANCE={}\nNOTIFY_ON_START={}\n",
            self.state.mode.as_str(),
            self.state.active_start,
            self.state.active_end,
            cap,
            status,
            current_ma,
            if balance_due { 1 } else { 0 },
            days_since,
            if self.cfg.notify_on_start { 1 } else { 0 },
        )
    }

    fn balance_due_info(&self, now_epoch: u64) -> (bool, u64) {
        if self.cfg.balance_interval_days == 0 {
            return (false, 0);
        }
        if self.state.last_balance_timestamp == 0 {
            return (true, 0);
        }
        let elapsed_secs = now_epoch.saturating_sub(self.state.last_balance_timestamp);
        let days = elapsed_secs / 86400;
        (elapsed_secs >= self.cfg.balance_interval_days * 86400, days)
    }

    fn handle_command(&mut self, cmd: SocketCommand, now_epoch: u64) -> String {
        match cmd {
            SocketCommand::Status => self.format_status_line(now_epoch),
            SocketCommand::FullOnce => {
                self.state.mode = ActiveMode::FullOnce;
                self.state.mode_started_epoch = now_epoch;
                self.state.soak_started_epoch = None;
                self.ticks.reset_counters();
                self.ticks.recheck_forced = true;
                let _ = self.state.save_to(&self.state_path);
                if self.ps.has_charge_control()
                    && let Err(e) = self.ps.set_standard_charging()
                {
                    return format!("ERR {e}\n");
                }
                "OK FULL_ONCE\n".to_string()
            }
            SocketCommand::BalanceOnce => {
                self.state.mode = ActiveMode::BalanceOnce;
                self.state.mode_started_epoch = now_epoch;
                self.state.soak_started_epoch = None;
                self.ticks.reset_counters();
                self.ticks.recheck_forced = true;
                let _ = self.state.save_to(&self.state_path);
                if self.ps.has_charge_control()
                    && let Err(e) = self.ps.set_standard_charging()
                {
                    return format!("ERR {e}\n");
                }
                "OK BALANCE_ONCE\n".to_string()
            }
            SocketCommand::Cancel => {
                self.switch_to_normal(now_epoch, "отменено пользователем")
                    .unwrap_or_else(|e| format!("ERR {e}\n"))
            }
            SocketCommand::Set { start, end } => {
                if !is_valid_pair(start, end) {
                    return format!("ERR Недопустимые пороги {start}..{end}%\n");
                }
                self.state.active_start = start;
                self.state.active_end = end;
                self.ticks.recheck_forced = false;
                match self.switch_to_normal(now_epoch, "установлено через сокет") {
                    Ok(_) => format!("OK SET {start} {end}\n"),
                    Err(e) => format!("ERR {e}\n"),
                }
            }
        }
    }

    fn switch_to_normal(&mut self, _now_epoch: u64, reason: &str) -> Result<String, Error> {
        self.state.mode = ActiveMode::Normal;
        self.state.mode_started_epoch = 0;
        self.state.soak_started_epoch = None;
        self.ticks.reset_counters();
        let _ = self.state.save_to(&self.state_path);
        if self.ps.has_charge_control() {
            self.ps
                .set_thresholds(self.state.active_start, self.state.active_end)?;
        }
        println!(
            "[u4-charge] Возврат лимитов {}..{}% [Custom] ({reason})",
            self.state.active_start, self.state.active_end
        );
        Ok("OK NORMAL\n".to_string())
    }

    fn step(&mut self, now_epoch: u64, woke_from_sleep: bool) {
        if !self.ps.has_charge_control() {
            return;
        }

        if woke_from_sleep {
            self.ticks.recheck_forced = true;
            println!("[u4-charge] Обнаружен выход из сна: проверка состояния EC и порогов...");
        }

        let bat = self.ps.bat_dir();
        let bat_cap = self.ps.read_u8("capacity").unwrap_or(0);
        let bat_status = self.ps.read_attr_string(&bat, "status").unwrap_or_default();
        let current_ma = self.ps.read_i64("current_now").unwrap_or(0) / 1000;
        let ac_online = self.ps.ac_online();

        match self.state.mode {
            ActiveMode::FullOnce => {
                let _ = self.ps.set_standard_charging();

                if ac_online == Some(false) {
                    self.ticks.unplug_ticks = self.ticks.unplug_ticks.saturating_add(1);
                } else {
                    self.ticks.unplug_ticks = 0;
                }

                if bat_cap >= 99 && (bat_status == "Full" || current_ma.abs() <= 50) {
                    self.ticks.full_confirm_ticks = self.ticks.full_confirm_ticks.saturating_add(1);
                } else {
                    self.ticks.full_confirm_ticks = 0;
                }

                let timeout_secs = self.cfg.full_timeout_hours.saturating_mul(3600);
                if self.ticks.full_confirm_ticks >= FULL_CONFIRM_TICKS {
                    let _ = self.switch_to_normal(now_epoch, "достигнут заряд 100%");
                } else if self.ticks.unplug_ticks >= UNPLUG_CONFIRM_TICKS {
                    let _ = self.switch_to_normal(now_epoch, "кабель питания отключён");
                } else if now_epoch.saturating_sub(self.state.mode_started_epoch) >= timeout_secs {
                    let _ = self.switch_to_normal(now_epoch, "истёк тайм-аут режима Full");
                }
            }
            ActiveMode::BalanceOnce => {
                let _ = self.ps.set_standard_charging();

                if ac_online == Some(false) {
                    self.ticks.unplug_ticks = self.ticks.unplug_ticks.saturating_add(1);
                    if self.ticks.unplug_ticks >= UNPLUG_CONFIRM_TICKS {
                        let _ = self.switch_to_normal(now_epoch, "балансировка прервана: AC отключён");
                        return;
                    }
                } else {
                    self.ticks.unplug_ticks = 0;
                }

                match self.state.soak_started_epoch {
                    None => {
                        if bat_cap >= 100 || bat_status == "Full" {
                            println!(
                                "[u4-charge] Балансировка: 100% достигнуто, старт выдержки {} мин...",
                                self.cfg.balance_soak_minutes
                            );
                            self.state.soak_started_epoch = Some(now_epoch);
                            let _ = self.state.save_to(&self.state_path);
                        }
                    }
                    Some(soak_start) => {
                        let soak_secs = self.cfg.balance_soak_minutes.saturating_mul(60);
                        if now_epoch.saturating_sub(soak_start) >= soak_secs {
                            self.state.last_balance_timestamp = now_epoch;
                            let _ = self.switch_to_normal(now_epoch, "балансировка завершена");
                        }
                    }
                }
            }
            ActiveMode::Normal => {
                let cur_start = self.ps.get_start_threshold().unwrap_or(0);
                let cur_end = self.ps.get_end_threshold().unwrap_or(0);
                let cur_type = self.ps.get_charge_type().unwrap_or_default();

                // Coexist peacefully with KDE PowerDevil / manual `u4-charge set`:
                // if user changed thresholds while Custom remained active, adopt them.
                if !self.ticks.recheck_forced
                    && cur_type == "Custom"
                    && (cur_start != self.state.active_start || cur_end != self.state.active_end)
                    && is_valid_pair(cur_start, cur_end)
                {
                    println!(
                        "[u4-charge] Принято внешнее изменение порогов (KDE / sysfs): {}..{}% -> {}..{}%",
                        self.state.active_start, self.state.active_end, cur_start, cur_end
                    );
                    self.state.active_start = cur_start;
                    self.state.active_end = cur_end;
                    let _ = self.state.save_to(&self.state_path);
                }

                let reset_to_driver_default =
                    cur_type == "Standard" && cur_start == 40 && cur_end == 100;
                let needs_apply = self.ticks.recheck_forced
                    || cur_type != "Custom"
                    || cur_start != self.state.active_start
                    || cur_end != self.state.active_end
                    || reset_to_driver_default;

                if needs_apply {
                    match self.ps.set_thresholds(self.state.active_start, self.state.active_end) {
                        Ok(()) => {
                            if self.ticks.recheck_forced {
                                println!(
                                    "[u4-charge] Пороги синхронизированы: {}..{}% [Custom]",
                                    self.state.active_start, self.state.active_end
                                );
                            }
                            self.ticks.recheck_forced = false;
                        }
                        Err(e) => eprintln!("[u4-charge] Ошибка синхронизации порогов: {e}"),
                    }
                }

                // Safety watchdog: if charging continues well past active_end while AC is online,
                // toggle Standard -> Custom to recover EC state.
                if ac_online == Some(true)
                    && bat_cap >= self.state.active_end
                    && bat_status == "Charging"
                    && current_ma > 100
                {
                    self.ticks.overcharge_ticks = self.ticks.overcharge_ticks.saturating_add(1);
                    let cooldown_ok = self
                        .ticks
                        .last_watchdog_resync
                        .is_none_or(|t| t.elapsed() >= Duration::from_secs(WATCHDOG_COOLDOWN_SECS));
                    if self.ticks.overcharge_ticks >= OVERCHARGE_WATCHDOG_TICKS && cooldown_ok {
                        eprintln!(
                            "[u4-charge] Watchdog: зарядка продолжается на {}% >= {}% ({} mA), выполняем перевзвод EC...",
                            bat_cap, self.state.active_end, current_ma
                        );
                        let _ = self.ps.set_standard_charging();
                        thread::sleep(Duration::from_millis(250));
                        let _ = self.ps.set_thresholds(self.state.active_start, self.state.active_end);
                        self.ticks.overcharge_ticks = 0;
                        self.ticks.last_watchdog_resync = Some(Instant::now());
                    }
                } else {
                    self.ticks.overcharge_ticks = 0;
                }
            }
        }
    }
}

enum EngineMsg {
    Command {
        cmd: SocketCommand,
        reply_tx: mpsc::SyncSender<String>,
    },
}

fn handle_client(
    mut stream: UnixStream,
    tx: &mpsc::Sender<EngineMsg>,
    wake_pair: &(Mutex<bool>, Condvar),
) {
    let timeout = Some(Duration::from_millis(SOCKET_TIMEOUT_MS));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);

    let mut buf = String::new();
    let read_res = (&mut stream).take(MAX_SOCKET_CMD_BYTES).read_to_string(&mut buf);
    if read_res.is_err() {
        return;
    }

    let response = match parse_socket_command(&buf) {
        Ok(cmd) => {
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            if tx.send(EngineMsg::Command { cmd, reply_tx }).is_ok() {
                let (lock, cvar) = wake_pair;
                if let Ok(mut pending) = lock.lock() {
                    *pending = true;
                    cvar.notify_one();
                }
                reply_rx
                    .recv_timeout(Duration::from_millis(1000))
                    .unwrap_or_else(|_| "ERR timeout\n".to_string())
            } else {
                "ERR daemon shutdown\n".to_string()
            }
        }
        Err(e) => format!("ERR {e}\n"),
    };

    let _ = stream.write_all(response.as_bytes());
}

fn spawn_socket_server(
    socket_path: &Path,
    tx: mpsc::Sender<EngineMsg>,
    wake_pair: Arc<(Mutex<bool>, Condvar)>,
) -> Result<(), Error> {
    if socket_path.exists() {
        let _ = fs::remove_file(socket_path);
    }
    if let Some(parent) = socket_path.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    // Pure Safe Rust permission hardening; systemd RuntimeDirectory + Group=wheel provide ownership
    fs::set_permissions(socket_path, Permissions::from_mode(0o660))?;

    thread::spawn(move || {
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => handle_client(stream, &tx, &wake_pair),
                Err(e) => eprintln!("[u4-charge] Ошибка accept сокета: {e}"),
            }
        }
    });
    Ok(())
}

fn send_socket_request(cmd: &str) -> Result<String, Error> {
    let mut stream = UnixStream::connect(SOCKET_PATH)?;
    let timeout = Some(Duration::from_millis(1500));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    stream.write_all(cmd.as_bytes())?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply)
}

fn parse_kv_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.trim().split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

fn run_notify_client() -> Result<(), Error> {
    let mut reply_opt = None;
    for _ in 0..30 {
        if Path::new(SOCKET_PATH).exists()
            && let Ok(resp) = send_socket_request("STATUS\n")
        {
            reply_opt = Some(resp);
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }

    let Some(status_text) = reply_opt else {
        return Ok(());
    };

    let mode = parse_kv_value(&status_text, "MODE").unwrap_or("Normal");
    let start = parse_kv_value(&status_text, "START").unwrap_or("60");
    let end = parse_kv_value(&status_text, "END").unwrap_or("80");
    let cap = parse_kv_value(&status_text, "CAP").unwrap_or("?");
    let balance_due = parse_kv_value(&status_text, "BALANCE_DUE") == Some("1");
    let days_since = parse_kv_value(&status_text, "DAYS_SINCE_BALANCE").unwrap_or("?");
    let notify_on_start = parse_kv_value(&status_text, "NOTIFY_ON_START") == Some("1");

    if balance_due && mode == "Normal" {
        let out = Command::new("timeout")
            .args([
                "30",
                "notify-send",
                "-u",
                "normal",
                "-t",
                "15000",
                "-a",
                "u4-charge",
                "-A",
                "balance=Запустить балансировку (100%)",
                "-A",
                "skip=Отложить",
                "Балансировка батареи U4",
                &format!("Прошло {days_since} дн. Запустить калибровку до 100%?"),
            ])
            .output()?;
        if String::from_utf8_lossy(&out.stdout).trim() == "balance" {
            let _ = send_socket_request("BALANCE_ONCE\n");
        }
        return Ok(());
    }

    if notify_on_start && mode == "Normal" {
        let out = Command::new("timeout")
            .args([
                "20",
                "notify-send",
                "-u",
                "low",
                "-t",
                "10000",
                "-a",
                "u4-charge",
                "-A",
                "full=Зарядить до 100% (разово)",
                &format!("Батарея U4 защищена ({start}–{end}%)"),
                &format!("Текущий заряд: {cap}%. Игнорируйте это уведомление, чтобы оставить лимит."),
            ])
            .output()?;
        if String::from_utf8_lossy(&out.stdout).trim() == "full"
            && let Ok(r) = send_socket_request("FULL_ONCE\n")
            && r.starts_with("OK")
        {
            let _ = Command::new("notify-send")
                .args([
                    "-u",
                    "low",
                    "-t",
                    "5000",
                    "-a",
                    "u4-charge",
                    "Разовый заряд до 100% включён",
                    &format!("Лимит {start}–{end}% вернётся сам при 100% или отключении кабеля."),
                ])
                .status();
        }
    }

    Ok(())
}

fn run_daemon(ps: PowerSupply) -> Result<(), Error> {
    println!("[u4-charge] Демон управления зарядом U4 запущен (tick={TICK_INTERVAL_SECS}s)");

    let cfg = Config::load();
    let state_path = PathBuf::from(STATE_FILE);
    let mut state = DaemonState::load_from(&state_path, cfg.target_start, cfg.target_end);
    let boot_id = read_boot_id();
    let now = now_epoch_secs();
    if state.clear_oneshot_if_stale(&boot_id, now, cfg.full_timeout_hours.saturating_mul(3600)) {
        let _ = state.save_to(&state_path);
    }

    let mut engine = DaemonEngine::new(ps, cfg, state, state_path);
    let (tx, rx) = mpsc::channel::<EngineMsg>();
    let wake_pair = Arc::new((Mutex::new(false), Condvar::new()));

    if let Err(e) = spawn_socket_server(Path::new(SOCKET_PATH), tx, Arc::clone(&wake_pair)) {
        eprintln!("[u4-charge] Предупреждение: не удалось открыть сокет {SOCKET_PATH}: {e}");
    }

    let mut last_wall = SystemTime::now();
    let mut last_mono = Instant::now();

    loop {
        while let Ok(EngineMsg::Command { cmd, reply_tx }) = rx.try_recv() {
            let reply = engine.handle_command(cmd, now_epoch_secs());
            let _ = reply_tx.send(reply);
        }

        let now_wall = SystemTime::now();
        let now_mono = Instant::now();
        let woke = detect_wake(last_wall, last_mono, now_wall, now_mono);
        last_wall = now_wall;
        last_mono = now_mono;

        engine.step(now_epoch_secs(), woke);

        let (lock, cvar) = &*wake_pair;
        if let Ok(mut pending) = lock.lock() {
            if !*pending {
                let (guard, _) = cvar
                    .wait_timeout(pending, Duration::from_secs(TICK_INTERVAL_SECS))
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                pending = guard;
            }
            *pending = false;
        } else {
            thread::sleep(Duration::from_secs(TICK_INTERVAL_SECS));
        }
    }
}

fn print_status(ps: &PowerSupply) -> Result<(), Error> {
    println!("================================================================================");
    println!(" СОСТОЯНИЕ АККУМУЛЯТОРА И СИСТЕМЫ ПИТАНИЯ (u4-charge)");
    println!("================================================================================");

    if !ps.is_bat_available() {
        println!("Батарея BAT0 не найдена в sysfs!");
        return Ok(());
    }

    let bat = ps.bat_dir();
    let status = ps.read_attr_string(&bat, "status").unwrap_or_else(|_| "Unknown".into());
    let capacity = ps.read_u8("capacity").unwrap_or(0);
    let voltage_now = ps.read_i64("voltage_now").unwrap_or(0) / 1000;
    let current_now = ps.read_i64("current_now").unwrap_or(0) / 1000;
    let charge_now = ps.read_i64("charge_now").unwrap_or(0) / 1000;
    let charge_full = ps.read_i64("charge_full").unwrap_or(0) / 1000;
    let charge_design = ps.read_i64("charge_full_design").unwrap_or(0) / 1000;
    let ac = ps.ac_online();
    let power_mw = (voltage_now * current_now.abs()) / 1000;

    println!(
        "Питание от сети (AC) : {}",
        match ac {
            Some(true) => "Подключено",
            Some(false) => "От батареи",
            None => "Неизвестно",
        }
    );
    println!("Текущий уровень      : {capacity}%");
    println!(
        "Состояние BAT0       : {}",
        match status.as_str() {
            "Charging" => format!("ЗАРЯДКА ({current_now} mA / {:.2} W)", power_mw as f64 / 1000.0),
            "Discharging" => format!("РАЗРЯДКА ({current_now} mA / {:.2} W)", power_mw as f64 / 1000.0),
            "Not charging" => "НЕ ЗАРЯЖАЕТСЯ (Лимит достигнут / байпас)".into(),
            "Full" => "ПОЛНОСТЬЮ ЗАРЯЖЕН".into(),
            other => other.to_string(),
        }
    );
    println!("Напряжение шины      : {:.3} V", voltage_now as f64 / 1000.0);
    println!("Емкость              : {charge_now} / {charge_full} mAh (Design: {charge_design} mAh)");

    println!("--------------------------------------------------------------------------------");
    if ps.has_charge_control() {
        let start_thr = ps.get_start_threshold()?;
        let end_thr = ps.get_end_threshold()?;
        let charge_type = ps.get_charge_type().unwrap_or_else(|_| "Standard".into());
        println!("Аппаратный режим EC  : {charge_type} (START={start_thr}%, STOP={end_thr}%)");
    } else {
        println!("Аппаратный режим EC  : charge_control_* недоступны (проверьте модуль clevo-drivers-u4)");
    }

    if let Ok(sock_status) = send_socket_request("STATUS\n") {
        let mode = parse_kv_value(&sock_status, "MODE").unwrap_or("Normal");
        let t_start = parse_kv_value(&sock_status, "START").unwrap_or("?");
        let t_end = parse_kv_value(&sock_status, "END").unwrap_or("?");
        println!("Демон u4-charge      : АКТИВЕН (режим={mode}, цель={t_start}..{t_end}%)");
    } else {
        println!("Демон u4-charge      : не отвечает (или сокет недоступен)");
    }
    println!("================================================================================");
    Ok(())
}

fn print_help() {
    println!("u4-charge - Управление порогом заряда для Gigabyte U4 / RC14UD");
    println!();
    println!("Команды:");
    println!("  u4-charge status                     Показать текущее состояние");
    println!("  u4-charge set --start <N> --end <M>  Установить пороги (START: 40..95, END: 60..100)");
    println!("  u4-charge full                       Разовая дозарядка до 100% (автовозврат к лимиту)");
    println!("  u4-charge balance                    Разовая балансировка 100% + soak (по запросу)");
    println!("  u4-charge cancel                     Отменить разовый full/balance и вернуть лимит");
    println!("  u4-charge notify                     Проверить статус и показать KDE уведомление");
    println!("  u4-charge daemon                     Запуск фоновой службы systemd");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let ps = PowerSupply::system();

    if args.len() < 2 {
        if let Err(e) = print_status(&ps) {
            eprintln!("Ошибка: {e}");
            std::process::exit(1);
        }
        return;
    }

    match args[1].as_str() {
        "status" => {
            if let Err(e) = print_status(&ps) {
                eprintln!("Ошибка: {e}");
                std::process::exit(1);
            }
        }
        "set" => {
            let mut start_val: Option<u8> = None;
            let mut end_val: Option<u8> = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--start" if i + 1 < args.len() => {
                        start_val = args[i + 1].parse::<u8>().ok();
                        i += 1;
                    }
                    "--end" if i + 1 < args.len() => {
                        end_val = args[i + 1].parse::<u8>().ok();
                        i += 1;
                    }
                    _ => {}
                }
                i += 1;
            }
            match (start_val, end_val) {
                (Some(s), Some(e)) => {
                    if !is_valid_pair(s, e) {
                        eprintln!(
                            "Ошибка: Недопустимые пороги START={s}% (допустимо: {VALID_START_THRESHOLDS:?}), END={e}% (допустимо: {VALID_END_THRESHOLDS:?}, START < END)"
                        );
                        std::process::exit(1);
                    }
                    if let Ok(reply) = send_socket_request(&format!("SET {s} {e}\n")) {
                        if reply.starts_with("OK") {
                            println!("Пороги установлены: START={s}%, STOP={e}% [Custom]");
                        } else {
                            eprintln!("Ошибка демона: {}", reply.trim());
                            std::process::exit(1);
                        }
                    } else if let Err(err) = ps.set_thresholds(s, e) {
                        eprintln!("Ошибка: {err}");
                        std::process::exit(1);
                    } else {
                        println!("Пороги установлены напрямую: START={s}%, STOP={e}% [Custom]");
                    }
                }
                _ => {
                    eprintln!("Укажите оба флага: u4-charge set --start <%> --end <%>");
                    std::process::exit(1);
                }
            }
        }
        "full" => {
            if let Ok(reply) = send_socket_request("FULL_ONCE\n") {
                print!("{reply}");
            } else if let Err(err) = ps.set_standard_charging() {
                eprintln!("Ошибка: {err}");
                std::process::exit(1);
            } else {
                println!("Режим Standard (100%) включён напрямую (демон не запущен).");
            }
        }
        "balance" => {
            match send_socket_request("BALANCE_ONCE\n") {
                Ok(reply) => print!("{reply}"),
                Err(e) => {
                    eprintln!("Для балансировки требуется активный демон u4-charge: {e}");
                    std::process::exit(1);
                }
            }
        }
        "cancel" => {
            match send_socket_request("CANCEL\n") {
                Ok(reply) => print!("{reply}"),
                Err(e) => {
                    eprintln!("Не удалось связаться с демоном u4-charge: {e}");
                    std::process::exit(1);
                }
            }
        }
        "notify" => {
            if let Err(e) = run_notify_client() {
                eprintln!("Ошибка уведомления: {e}");
                std::process::exit(1);
            }
        }
        "daemon" => {
            if let Err(e) = run_daemon(ps) {
                eprintln!("Ошибка демона: {e}");
                std::process::exit(1);
            }
        }
        "help" | "--help" | "-h" => print_help(),
        other => {
            eprintln!("Неизвестная команда: {other}");
            print_help();
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(tag: &str) -> Self {
            let path = env::temp_dir().join(format!("u4_charge_{tag}_{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            let _ = fs::create_dir_all(path.join("BAT0"));
            let _ = fs::create_dir_all(path.join("AC"));
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct MockBattery<'a> {
        start: u8,
        end: u8,
        ctype: &'a str,
        cap: u8,
        status: &'a str,
        curr_ua: i64,
        ac: &'a str,
    }

    fn setup_mock_ps(root: &Path, mock: MockBattery<'_>) {
        let bat = root.join("BAT0");
        let _ = fs::write(bat.join("charge_control_start_threshold"), format!("{}\n", mock.start));
        let _ = fs::write(bat.join("charge_control_end_threshold"), format!("{}\n", mock.end));
        let _ = fs::write(bat.join("charge_type"), format!("{}\n", mock.ctype));
        let _ = fs::write(bat.join("capacity"), format!("{}\n", mock.cap));
        let _ = fs::write(bat.join("status"), format!("{}\n", mock.status));
        let _ = fs::write(bat.join("current_now"), format!("{}\n", mock.curr_ua));
        let _ = fs::write(root.join("AC").join("online"), format!("{}\n", mock.ac));
    }

    #[test]
    fn test_thresholds_write_order_and_validation() {
        let guard = TempDirGuard::new("thr");
        setup_mock_ps(
            &guard.path,
            MockBattery {
                start: 40,
                end: 100,
                ctype: "Standard",
                cap: 70,
                status: "Charging",
                curr_ua: 2000000,
                ac: "1",
            },
        );
        let ps = PowerSupply::new(guard.path.clone());

        assert!(ps.set_thresholds(60, 80).is_ok());
        assert_eq!(ps.get_start_threshold().ok(), Some(60));
        assert_eq!(ps.get_end_threshold().ok(), Some(80));
        assert_eq!(ps.get_charge_type().ok().as_deref(), Some("Custom"));

        // Raising start past previous end (80 -> 95) should work safely too
        assert!(ps.set_thresholds(80, 90).is_ok());
        assert_eq!(ps.get_start_threshold().ok(), Some(80));
        assert_eq!(ps.get_end_threshold().ok(), Some(90));

        assert!(ps.set_thresholds(80, 80).is_err());
        assert!(ps.set_thresholds(45, 80).is_err());
        assert!(ps.set_thresholds(40, 50).is_err());
    }

    #[test]
    fn test_oneshot_full_auto_resets_on_full_and_unplug_and_timeout() {
        let guard = TempDirGuard::new("full");
        setup_mock_ps(
            &guard.path,
            MockBattery {
                start: 60,
                end: 80,
                ctype: "Custom",
                cap: 85,
                status: "Charging",
                curr_ua: 2000000,
                ac: "1",
            },
        );
        let cfg = Config::default();
        let st_path = guard.path.join("state.conf");
        let st = DaemonState::load_from(&st_path, 60, 80);
        let mut eng = DaemonEngine::new(PowerSupply::new(guard.path.clone()), cfg, st, st_path);

        assert_eq!(eng.handle_command(SocketCommand::FullOnce, 1000), "OK FULL_ONCE\n");
        assert_eq!(eng.ps.get_charge_type().ok().as_deref(), Some("Standard"));

        // Transient 1-tick unplug should NOT cancel FullOnce (debounce)
        setup_mock_ps(
            &guard.path,
            MockBattery {
                start: 60,
                end: 80,
                ctype: "Standard",
                cap: 86,
                status: "Discharging",
                curr_ua: 1000000,
                ac: "0",
            },
        );
        eng.step(1003, false);
        assert_eq!(eng.state.mode, ActiveMode::FullOnce);

        // Plugging back resets unplug_ticks
        setup_mock_ps(
            &guard.path,
            MockBattery {
                start: 60,
                end: 80,
                ctype: "Standard",
                cap: 99,
                status: "Full",
                curr_ua: 0,
                ac: "1",
            },
        );
        eng.step(1006, false);
        assert_eq!(eng.state.mode, ActiveMode::FullOnce); // 1 confirm tick

        eng.step(1009, false); // 2nd confirm tick -> reverts to Custom 60..80
        assert_eq!(eng.state.mode, ActiveMode::Normal);
        assert_eq!(eng.ps.get_charge_type().ok().as_deref(), Some("Custom"));

        // Extended unplug (>= 2 ticks) cancels FullOnce
        assert_eq!(eng.handle_command(SocketCommand::FullOnce, 2000), "OK FULL_ONCE\n");
        setup_mock_ps(
            &guard.path,
            MockBattery {
                start: 60,
                end: 80,
                ctype: "Standard",
                cap: 90,
                status: "Discharging",
                curr_ua: 1000000,
                ac: "0",
            },
        );
        eng.step(2003, false);
        eng.step(2006, false);
        assert_eq!(eng.state.mode, ActiveMode::Normal);
        assert_eq!(eng.ps.get_charge_type().ok().as_deref(), Some("Custom"));

        // Timeout reverts FullOnce
        assert_eq!(eng.handle_command(SocketCommand::FullOnce, 3000), "OK FULL_ONCE\n");
        setup_mock_ps(
            &guard.path,
            MockBattery {
                start: 60,
                end: 80,
                ctype: "Standard",
                cap: 95,
                status: "Charging",
                curr_ua: 500000,
                ac: "1",
            },
        );
        eng.step(3000 + 6 * 3600 + 1, false);
        assert_eq!(eng.state.mode, ActiveMode::Normal);
    }

    #[test]
    fn test_powerdevil_coexistence_and_sleep_recovery() {
        let guard = TempDirGuard::new("kde");
        setup_mock_ps(
            &guard.path,
            MockBattery {
                start: 60,
                end: 80,
                ctype: "Custom",
                cap: 75,
                status: "Not charging",
                curr_ua: 0,
                ac: "1",
            },
        );
        let cfg = Config::default();
        let st_path = guard.path.join("state.conf");
        let st = DaemonState::load_from(&st_path, 60, 80);
        let mut eng = DaemonEngine::new(PowerSupply::new(guard.path.clone()), cfg, st, st_path);

        eng.step(100, false); // initial sync clears recheck_forced

        // Simulate user dragging PowerDevil slider to 50..70% Custom
        setup_mock_ps(
            &guard.path,
            MockBattery {
                start: 50,
                end: 70,
                ctype: "Custom",
                cap: 75,
                status: "Not charging",
                curr_ua: 0,
                ac: "1",
            },
        );
        eng.step(103, false);
        assert_eq!(eng.state.active_start, 50);
        assert_eq!(eng.state.active_end, 70);

        // Simulate EC reset to Standard 40/100 on resume from suspend
        setup_mock_ps(
            &guard.path,
            MockBattery {
                start: 40,
                end: 100,
                ctype: "Standard",
                cap: 75,
                status: "Charging",
                curr_ua: 2500000,
                ac: "1",
            },
        );
        eng.step(200, true);
        assert_eq!(eng.ps.get_charge_type().ok().as_deref(), Some("Custom"));
        assert_eq!(eng.ps.get_start_threshold().ok(), Some(50));
        assert_eq!(eng.ps.get_end_threshold().ok(), Some(70));
    }

    #[test]
    fn test_overcharge_watchdog_triggers_after_5_ticks() {
        let guard = TempDirGuard::new("wd");
        setup_mock_ps(
            &guard.path,
            MockBattery {
                start: 60,
                end: 80,
                ctype: "Custom",
                cap: 85,
                status: "Charging",
                curr_ua: 2500000,
                ac: "1",
            },
        );
        let cfg = Config::default();
        let st_path = guard.path.join("state.conf");
        let st = DaemonState::load_from(&st_path, 60, 80);
        let mut eng = DaemonEngine::new(PowerSupply::new(guard.path.clone()), cfg, st, st_path);

        for i in 0..5 {
            eng.step(100 + i * 3, false);
        }
        assert!(eng.ticks.last_watchdog_resync.is_some());
        assert_eq!(eng.ticks.overcharge_ticks, 0);
        assert_eq!(eng.ps.get_charge_type().ok().as_deref(), Some("Custom"));
    }

    #[test]
    fn test_ntp_step_guard_and_socket_parser() {
        let mono_now = Instant::now();
        let mono_later = mono_now + Duration::from_secs(3);
        let wall_now = SystemTime::now();
        // Backwards wall jump (NTP step back) must not panic and must return false
        let wall_backwards = wall_now - Duration::from_secs(60);
        assert!(!detect_wake(wall_now, mono_now, wall_backwards, mono_later));
        // Forward sleep jump (> 12s excess) must return true
        let wall_slept = wall_now + Duration::from_secs(30);
        assert!(detect_wake(wall_now, mono_now, wall_slept, mono_later));

        assert_eq!(parse_socket_command("STATUS\n").ok(), Some(SocketCommand::Status));
        assert_eq!(parse_socket_command("FULL_ONCE\r\n").ok(), Some(SocketCommand::FullOnce));
        assert_eq!(parse_socket_command("BALANCE_ONCE").ok(), Some(SocketCommand::BalanceOnce));
        assert_eq!(parse_socket_command("CANCEL\n").ok(), Some(SocketCommand::Cancel));
        assert_eq!(
            parse_socket_command("SET 50 70\n").ok(),
            Some(SocketCommand::Set { start: 50, end: 70 })
        );
        assert!(parse_socket_command("SET 80 60\n").is_err());
        assert!(parse_socket_command("rm -rf /").is_err());
    }

    #[test]
    fn test_socket_set_and_config_override() {
        let guard = TempDirGuard::new("cfg");
        setup_mock_ps(
            &guard.path,
            MockBattery {
                start: 60,
                end: 80,
                ctype: "Custom",
                cap: 75,
                status: "Not charging",
                curr_ua: 0,
                ac: "1",
            },
        );
        let cfg = Config::default();
        let st_path = guard.path.join("state.conf");
        let st = DaemonState::load_from(&st_path, cfg.target_start, cfg.target_end);
        let mut eng = DaemonEngine::new(PowerSupply::new(guard.path.clone()), cfg, st, st_path.clone());

        assert_eq!(
            eng.handle_command(SocketCommand::Set { start: 50, end: 70 }, 500),
            "OK SET 50 70\n"
        );
        assert_eq!(eng.state.active_start, 50);
        assert_eq!(eng.state.active_end, 70);
        assert_eq!(eng.ps.get_start_threshold().ok(), Some(50));
        assert_eq!(eng.ps.get_end_threshold().ok(), Some(70));

        // Reload with same /etc config preserves SET thresholds
        let reloaded_same = DaemonState::load_from(&st_path, 60, 80);
        assert_eq!(reloaded_same.active_start, 50);
        assert_eq!(reloaded_same.active_end, 70);

        // Reload after editing /etc config adopts the new config thresholds
        let reloaded_edited = DaemonState::load_from(&st_path, 40, 60);
        assert_eq!(reloaded_edited.active_start, 40);
        assert_eq!(reloaded_edited.active_end, 60);
    }
}
