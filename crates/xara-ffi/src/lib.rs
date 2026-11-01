//! C ABI for the xara modem, declared in `include/xara.h`.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use xara_arq::{fm, hf};
use xara_host::command::{DRIVE_MIN_DB, valid_call};
use xara_host::kiss::KissServer;
use xara_host::server::{DEFAULT_ADDR, DEFAULT_PORT, Host};
use xara_modem::station::{self, Modem, Options};
use xara_modem::tnc::Tnc;
use xara_phy::fm::level::FmBandwidth;
use xara_phy::hf::level::Bandwidth;

pub const KEYED: u32 = 1 << 0;
pub const TUNING: u32 = 1 << 1;
pub const BUSY: u32 = 1 << 2;
pub const RESTART: u32 = 1 << 3;
pub const ERROR: u32 = 1 << 31;

pub const LOG_INFO: c_int = 0;
pub const LOG_DEBUG: c_int = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Hf500 = 0,
    Hf2300 = 1,
    Hf2750 = 2,
    FmNarrow = 3,
    FmWide = 4,
}

impl Mode {
    fn from_c(v: c_int) -> Option<Self> {
        Some(match v {
            0 => Self::Hf500,
            1 => Self::Hf2300,
            2 => Self::Hf2750,
            3 => Self::FmNarrow,
            4 => Self::FmWide,
            _ => return None,
        })
    }
}

#[repr(C)]
pub struct Config {
    /// A `Mode` value; an int, since C may store any value in an enum.
    pub mode: c_int,
    pub call: *const c_char,
    pub addr: *const c_char,
    pub cmd_port: u16,
    pub kiss_port: u16,
    pub drive_db: f64,
    pub tx_latency: i64,
    pub rx_lag: i64,
    pub ptt_tail: i64,
}

#[repr(C)]
pub struct Status {
    pub state: [c_char; 24],
    pub peer: [c_char; 24],
    pub bw: [c_char; 16],
    pub level: u32,
    pub attempts: u32,
    pub passband_lo: f64,
    pub passband_hi: f64,
    pub afc_hz: f64,
    pub cmd_port: u16,
    pub data_port: u16,
    pub kiss_port: u16,
}

enum Inner {
    Hf(Box<Tnc<station::hf::Station>>),
    Fm(Box<Tnc<station::fm::Station>>),
}

macro_rules! with_tnc {
    ($inner:expr, $t:ident => $e:expr) => {
        match $inner {
            Inner::Hf($t) => $e,
            Inner::Fm($t) => $e,
        }
    };
}

pub struct Handle {
    inner: Inner,
    ports: (u16, u16, u16),
    x: Vec<f64>,
    y: Vec<f64>,
    failed: bool,
}

fn build(cfg: &Config) -> Result<Handle, String> {
    let mode = Mode::from_c(cfg.mode).ok_or_else(|| format!("invalid mode {}", cfg.mode))?;
    let call = unsafe { opt_str(cfg.call) }?.ok_or("call sign is required")?;
    let call = valid_call(&call).ok_or_else(|| format!("invalid call sign {call:?}"))?;
    let addr = unsafe { opt_str(cfg.addr) }?.unwrap_or_else(|| DEFAULT_ADDR.into());
    if !(DRIVE_MIN_DB..=0.0).contains(&cfg.drive_db) {
        return Err(format!("drive_db must be {DRIVE_MIN_DB}..0 dB"));
    }
    if cfg.tx_latency < 0 || cfg.rx_lag < 0 || cfg.ptt_tail < 0 {
        return Err("latencies must not be negative".into());
    }
    let opts =
        Options { drive_db: cfg.drive_db, tx_latency: cfg.tx_latency, rx_lag: cfg.rx_lag, ptt_tail: cfg.ptt_tail };
    let host = Host::bind(&addr, cfg.cmd_port).map_err(|e| format!("command/data ports: {e}"))?;
    let (cmd, data) = (host.cmd_addr.port(), host.data_addr.port());
    let kiss = match cfg.kiss_port {
        0 => None,
        p => Some(KissServer::bind(&addr, p).map_err(|e| format!("KISS port: {e}"))?),
    };
    let kiss_port = kiss.as_ref().map_or(0, |k| k.addr.port());
    let fm_bw = match mode {
        Mode::FmNarrow => Some(FmBandwidth::Narrow),
        Mode::FmWide => Some(FmBandwidth::Wide),
        _ => None,
    };
    let mut inner = match fm_bw {
        Some(bw) => Inner::Fm(Box::new(Tnc::new(station::fm::Station::new(fm::Config::new(bw, &call), opts), host))),
        None => {
            let bw = match mode {
                Mode::Hf500 => Bandwidth::Hz500,
                Mode::Hf2750 => Bandwidth::Hz2750,
                _ => Bandwidth::Hz2300,
            };
            let mut st = station::hf::Station::new(hf::Config::new(bw, &call), opts);
            st.set_persistence(xara_arq::hf::broadcast::DEFAULT_PERSISTENCE);
            Inner::Hf(Box::new(Tnc::new(st, host)))
        }
    };
    if let Some(k) = kiss {
        with_tnc!(&mut inner, t => t.set_kiss(k));
    }
    // the console lines of the CLI (without --quiet), which LOG_INFO promises
    with_tnc!(&mut inner, t => t.set_echo(true));
    Ok(Handle { inner, ports: (cmd, data, kiss_port), x: Vec::new(), y: Vec::new(), failed: false })
}

unsafe fn opt_str(p: *const c_char) -> Result<Option<String>, String> {
    if p.is_null() {
        return Ok(None);
    }
    let s = unsafe { CStr::from_ptr(p) }.to_str().map_err(|_| "strings must be UTF-8".to_string())?;
    Ok(Some(s.to_string()))
}

// The longest prefix of s that fits in max bytes without splitting a character.
fn prefix(s: &str, max: usize) -> &str {
    let mut n = s.len().min(max);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

fn write_err(err: *mut c_char, len: usize, msg: &str) {
    if err.is_null() || len == 0 {
        return;
    }
    let msg = prefix(msg, len - 1);
    let n = msg.len();
    unsafe {
        std::ptr::copy_nonoverlapping(msg.as_ptr().cast::<c_char>(), err, n);
        *err.add(n) = 0;
    }
}

fn copy_str(dst: &mut [c_char], s: &str) {
    let s = prefix(s, dst.len() - 1);
    for (d, &b) in dst.iter_mut().zip(s.as_bytes()) {
        *d = b as c_char;
    }
    dst[s.len()] = 0;
}

/// # Safety
/// `cfg` must be null or point to a writable `xara_config`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xara_config_default(cfg: *mut Config) {
    if cfg.is_null() {
        return;
    }
    unsafe {
        cfg.write(Config {
            mode: Mode::Hf2300 as c_int,
            call: std::ptr::null(),
            addr: std::ptr::null(),
            cmd_port: DEFAULT_PORT,
            kiss_port: xara_host::kiss::DEFAULT_PORT,
            drive_db: station::DEFAULT_DRIVE_DB,
            tx_latency: 0,
            rx_lag: 0,
            ptt_tail: 0,
        });
    }
}

/// # Safety
/// `cfg` must point to a valid `xara_config`; `err` must be null or have room for `err_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xara_create(cfg: *const Config, err: *mut c_char, err_len: usize) -> *mut Handle {
    if cfg.is_null() {
        write_err(err, err_len, "no config");
        return std::ptr::null_mut();
    }
    let cfg = unsafe { &*cfg };
    match catch_unwind(AssertUnwindSafe(|| build(cfg))) {
        Ok(Ok(m)) => Box::into_raw(Box::new(m)),
        Ok(Err(e)) => {
            write_err(err, err_len, &e);
            std::ptr::null_mut()
        }
        Err(_) => {
            write_err(err, err_len, "internal error");
            std::ptr::null_mut()
        }
    }
}

/// # Safety
/// `m` must be null or come from `xara_create`, and not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xara_destroy(m: *mut Handle) {
    if !m.is_null() {
        let m = unsafe { Box::from_raw(m) };
        let _ = catch_unwind(AssertUnwindSafe(|| match m.inner {
            Inner::Hf(t) => t.close(),
            Inner::Fm(t) => t.close(),
        }));
    }
}

fn process(m: &mut Handle, input: &[f32], output: &mut [f32]) -> u32 {
    m.x.clear();
    m.x.extend(input.iter().map(|&v| f64::from(v)));
    m.y.clear();
    m.y.resize(input.len(), 0.0);
    with_tnc!(&mut m.inner, t => {
        t.process(&m.x, &mut m.y);
        let s = t.station();
        let mut flags = 0;
        if t.keyed() {
            flags |= KEYED;
        }
        if s.tuning() {
            flags |= TUNING;
        }
        if s.busy() {
            flags |= BUSY;
        }
        if t.restart_requested() {
            flags |= RESTART;
        }
        for (o, &v) in output.iter_mut().zip(&m.y) {
            *o = v as f32;
        }
        flags
    })
}

/// # Safety
/// `m` from `xara_create`; `input` and `output` must hold `n` floats (they may not overlap).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xara_process(m: *mut Handle, input: *const f32, output: *mut f32, n: usize) -> u32 {
    if m.is_null() || output.is_null() {
        return ERROR;
    }
    let output = unsafe { std::slice::from_raw_parts_mut(output, n) };
    output.fill(0.0);
    let m = unsafe { &mut *m };
    if m.failed || input.is_null() {
        return ERROR;
    }
    let input = unsafe { std::slice::from_raw_parts(input, n) };
    match catch_unwind(AssertUnwindSafe(|| process(m, input, output))) {
        Ok(f) => f,
        Err(_) => {
            m.failed = true;
            output.fill(0.0);
            ERROR
        }
    }
}

// Runs f on a modem that has not failed, marking it failed if f panics. None if it is null, failed or panicked.
fn guarded<T>(m: *mut Handle, f: impl FnOnce(&mut Handle) -> T) -> Option<T> {
    let m = unsafe { m.as_mut() }.filter(|m| !m.failed)?;
    let r = catch_unwind(AssertUnwindSafe(|| f(m))).ok();
    m.failed |= r.is_none();
    r
}

/// # Safety
/// `m` must come from `xara_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xara_set_tx_latency(m: *mut Handle, tx_latency: i64, rx_lag: i64) -> bool {
    if tx_latency < 0 || rx_lag < 0 {
        return false;
    }
    guarded(m, |m| with_tnc!(&mut m.inner, t => t.set_tx_latency(tx_latency, rx_lag))).is_some()
}

/// # Safety
/// `m` must come from `xara_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xara_force_ptt_off(m: *mut Handle, off: bool) {
    guarded(m, |m| with_tnc!(&mut m.inner, t => t.force_ptt_off(off)));
}

/// # Safety
/// `m` must come from `xara_create`; `st` must point to a writable `xara_status`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xara_get_status(m: *const Handle, st: *mut Status) {
    let Some(st) = (unsafe { st.as_mut() }) else { return };
    let Some(ports) = (unsafe { m.as_ref() }).map(|m| m.ports) else { return };
    // only reads, but a panic still marks the modem failed
    let link = guarded(m.cast_mut(), |m| with_tnc!(&m.inner, t => t.station().link()));
    *st = unsafe { std::mem::zeroed() };
    (st.cmd_port, st.data_port, st.kiss_port) = ports;
    let Some(link) = link else {
        copy_str(&mut st.state, "Error");
        st.afc_hz = f64::NAN;
        return;
    };
    copy_str(&mut st.state, &link.state);
    copy_str(&mut st.peer, &link.peer);
    copy_str(&mut st.bw, &link.bw);
    st.level = link.level as u32;
    st.attempts = link.attempts;
    (st.passband_lo, st.passband_hi) = link.passband;
    st.afc_hz = link.afc_hz.unwrap_or(f64::NAN);
}

type LogFn = unsafe extern "C" fn(*mut c_void, c_int, *const c_char);

static LOG: Mutex<Option<(LogFn, usize)>> = Mutex::new(None);

fn log_line(level: c_int, args: fmt::Arguments) {
    let Some((f, user)) = *LOG.lock().unwrap_or_else(|e| e.into_inner()) else { return };
    let mut s = args.to_string().replace('\0', " ");
    s.push('\0');
    unsafe { f(user as *mut c_void, level, s.as_ptr().cast()) };
}

fn host_log(args: fmt::Arguments) {
    log_line(LOG_INFO, args);
}

// console lines are the ones xara shows the user; the rest only go to its log file
fn modem_log(console: bool, args: fmt::Arguments) {
    log_line(if console { LOG_INFO } else { LOG_DEBUG }, args);
}

/// # Safety
/// `cb` must be safe to call from any thread with `user` until the process exits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xara_set_log(cb: Option<LogFn>, user: *mut c_void) {
    *LOG.lock().unwrap_or_else(|e| e.into_inner()) = cb.map(|f| (f, user as usize));
    xara_host::set_log(host_log);
    xara_modem::log::set_log(modem_log);
}

#[unsafe(no_mangle)]
pub extern "C" fn xara_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_cut_on_character_boundaries() {
        let mut dst = [1 as c_char; 4];
        copy_str(&mut dst, "aéé");
        assert_eq!(unsafe { CStr::from_ptr(dst.as_ptr()) }.to_str(), Ok("aé"));
        let mut err = [1 as c_char; 3];
        write_err(err.as_mut_ptr(), err.len(), "ééé");
        assert_eq!(unsafe { CStr::from_ptr(err.as_ptr()) }.to_str(), Ok("é"));
        copy_str(&mut dst, "abcdef");
        assert_eq!(unsafe { CStr::from_ptr(dst.as_ptr()) }.to_str(), Ok("abc"));
    }
}
