use crate::command::{Command, parse};
use crate::writer::Writer;
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const DEFAULT_ADDR: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8300;
pub const MAX_LINE: usize = 1024;
pub const DATA_CHUNK: usize = 65536;
pub const INPUT_QUEUE: usize = 64;
pub(crate) const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
const WAKE_TIMEOUT: Duration = Duration::from_millis(100);
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum Input {
    Command(Command),
    Wrong(String),
    Data(Vec<u8>),
    Connected,
    Closed,
}

#[derive(Clone, Copy)]
enum Port {
    Cmd,
    Data,
}

#[derive(Default)]
struct Clients {
    cmd: Option<Writer>,
    data: Option<Writer>,
}

impl Clients {
    fn slot(&mut self, p: Port) -> &mut Option<Writer> {
        match p {
            Port::Cmd => &mut self.cmd,
            Port::Data => &mut self.data,
        }
    }
}

type Shared = Arc<Mutex<Clients>>;

#[derive(Default)]
struct Hold {
    on: Mutex<bool>,
    cv: Condvar,
}

impl Hold {
    fn set(&self, on: bool) {
        *self.on.lock().unwrap() = on;
        self.cv.notify_all();
    }

    fn wait(&self) {
        drop(self.cv.wait_while(self.on.lock().unwrap(), |on| *on).unwrap());
    }
}

pub struct Host {
    pub input: Receiver<Input>,
    clients: Shared,
    hold: Arc<Hold>,
    closed: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub cmd_addr: SocketAddr,
    pub data_addr: SocketAddr,
}

impl Host {
    pub fn bind(addr: &str, port: u16) -> io::Result<Self> {
        let cl = bind_fixed(addr, port)?;
        let data_port = match port {
            0 => 0,
            p => p.checked_add(1).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "port must be < 65535"))?,
        };
        let dl = bind_fixed(addr, data_port)?;
        let (tx, input) = sync_channel(INPUT_QUEUE);
        let clients = Shared::default();
        let hold = Arc::<Hold>::default();
        let closed = Arc::<AtomicBool>::default();
        let mut h = Self {
            input,
            clients: clients.clone(),
            hold: hold.clone(),
            closed: closed.clone(),
            threads: Vec::new(),
            cmd_addr: cl.local_addr()?,
            data_addr: dl.local_addr()?,
        };
        h.threads.push(serve(cl, Port::Cmd, clients.clone(), tx.clone(), hold.clone(), closed.clone())?);
        h.threads.push(serve(dl, Port::Data, clients, tx, hold, closed)?);
        Ok(h)
    }

    pub fn hold_data(&self, on: bool) {
        self.hold.set(on);
    }

    pub fn line(&self, s: &str) {
        self.send(Port::Cmd, format!("{s}\r").as_bytes());
    }

    pub fn data(&self, b: &[u8]) {
        self.send(Port::Data, b);
    }

    fn send(&self, p: Port, b: &[u8]) {
        if let Some(w) = self.clients.lock().unwrap().slot(p) {
            w.send(b);
        }
    }

    pub fn has_cmd_client(&self) -> bool {
        self.clients.lock().unwrap().cmd.is_some()
    }

    pub fn has_data_client(&self) -> bool {
        self.clients.lock().unwrap().data.is_some()
    }
}

impl Host {
    /// Disconnects the clients and waits (bounded) until both ports can be bound again. A plain drop leaves
    /// connected clients alone and frees the ports when they leave.
    pub fn close(mut self) {
        // stop accepting first, so no client attaches after the disconnect below
        self.stop();
        let input = &self.input;
        if !join(&mut self.threads, || while input.try_recv().is_ok() {}) {
            crate::say(format_args!("host ports {} and {} are still listening", self.cmd_addr, self.data_addr));
        }
        let c = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        c.cmd.iter().chain(&c.data).for_each(Writer::close);
    }

    fn stop(&self) {
        self.hold.set(false);
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        wake(self.cmd_addr);
        wake(self.data_addr);
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop();
    }
}

// Waits (bounded) for accept threads to drop their listeners, so the ports can be bound again at once. False
// if one is still running (it is left detached).
pub(crate) fn join(threads: &mut Vec<JoinHandle<()>>, mut drain: impl FnMut()) -> bool {
    let until = Instant::now() + JOIN_TIMEOUT;
    while threads.iter().any(|t| !t.is_finished()) && Instant::now() < until {
        drain();
        thread::sleep(Duration::from_millis(1));
    }
    let all = threads.iter().all(JoinHandle::is_finished);
    for t in threads.drain(..).filter(JoinHandle::is_finished) {
        let _ = t.join();
    }
    all
}

pub(crate) fn wake(mut a: SocketAddr) {
    if a.ip().is_unspecified() {
        a.set_ip(match a.ip() {
            IpAddr::V4(_) => Ipv4Addr::LOCALHOST.into(),
            IpAddr::V6(_) => Ipv6Addr::LOCALHOST.into(),
        });
    }
    let _ = TcpStream::connect_timeout(&a, WAKE_TIMEOUT);
}

#[cfg(target_os = "linux")]
fn set_reuse(l: &TcpListener, on: bool) -> io::Result<()> {
    let v: libc::c_int = on.into();
    let len = std::mem::size_of_val(&v) as libc::socklen_t;
    // SAFETY: l is an open socket and v outlives the call.
    let r =
        unsafe { libc::setsockopt(l.as_raw_fd(), libc::SOL_SOCKET, libc::SO_REUSEADDR, (&raw const v).cast(), len) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
struct Paused;

#[cfg(target_os = "linux")]
impl Paused {
    fn start(l: &TcpListener) -> io::Result<Self> {
        set_reuse(l, false)?;
        // SAFETY: l is an open socket; shutdown takes no pointers.
        if unsafe { libc::shutdown(l.as_raw_fd(), libc::SHUT_RD) } != 0 {
            let e = io::Error::last_os_error();
            let _ = set_reuse(l, true);
            return Err(e);
        }
        Ok(Self)
    }

    fn resume(&mut self, l: &TcpListener) -> io::Result<()> {
        set_reuse(l, true)?;
        // SAFETY: l is an open socket; listen takes no pointers.
        if unsafe { libc::listen(l.as_raw_fd(), 128) } != 0 {
            let e = io::Error::last_os_error();
            let _ = set_reuse(l, false);
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
use crate::bouncer::Bouncer as Paused;

fn bind_fixed(addr: &str, port: u16) -> io::Result<TcpListener> {
    if port != 0 {
        return TcpListener::bind((addr, port));
    }
    for _ in 0..16 {
        let p = TcpListener::bind((addr, 0))?.local_addr()?.port();
        if let Ok(l) = TcpListener::bind((addr, p)) {
            return Ok(l);
        }
    }
    Err(io::Error::new(io::ErrorKind::AddrInUse, "no free port"))
}

fn serve(
    l: TcpListener,
    port: Port,
    clients: Shared,
    tx: SyncSender<Input>,
    hold: Arc<Hold>,
    closed: Arc<AtomicBool>,
) -> io::Result<JoinHandle<()>> {
    let name = match port {
        Port::Cmd => "host-cmd-accept",
        Port::Data => "host-data-accept",
    };
    let closed = move || closed.load(Ordering::SeqCst);
    thread::Builder::new().name(name.into()).spawn(move || {
        while !closed() {
            let s = match l.accept() {
                Ok((s, _)) => s,
                Err(_) => {
                    thread::sleep(ACCEPT_BACKOFF);
                    continue;
                }
            };
            if closed() {
                return;
            }
            let paused = Paused::start(&l)
                .inspect_err(|e| crate::say(format_args!("{name} cannot refuse a second client: {e}")))
                .ok();
            let Ok(ended) = attach(s, port, &clients, &tx, &hold) else { return };
            if closed() {
                return;
            }
            if let Some(mut p) = paused {
                if let Err(e) = p.resume(&l) {
                    crate::say(format_args!("{name} cannot listen again, retrying: {e}"));
                    while !closed() && p.resume(&l).is_err() {
                        thread::sleep(ACCEPT_BACKOFF);
                    }
                }
            }
            if ended && tx.send(Input::Closed).is_err() {
                return;
            }
        }
    })
}

fn attach(s: TcpStream, port: Port, clients: &Shared, tx: &SyncSender<Input>, hold: &Hold) -> Result<bool, ()> {
    let _ = s.set_nodelay(true);
    let Ok(w) = Writer::spawn(&s) else { return Ok(false) };
    *clients.lock().unwrap().slot(port) = Some(w);
    tx.send(Input::Connected).map_err(drop)?;
    match port {
        Port::Cmd => read_commands(s, tx),
        Port::Data => read_data(s, tx, hold),
    }
    let mut c = clients.lock().unwrap();
    let ended = c.slot(port).is_some();
    if ended {
        for w in [c.cmd.take(), c.data.take()].into_iter().flatten() {
            w.close();
        }
    }
    Ok(ended)
}

fn read_commands(mut s: TcpStream, tx: &SyncSender<Input>) {
    let mut buf = [0u8; 4096];
    let mut line = Vec::new();
    let mut long = false;
    while let Ok(n @ 1..) = s.read(&mut buf) {
        for &b in &buf[..n] {
            match b {
                b'\r' => {
                    let l = String::from_utf8_lossy(&line).into_owned();
                    line.clear();
                    if std::mem::take(&mut long) {
                        continue;
                    }
                    let msg = parse(&l).map_or(Input::Wrong(l), Input::Command);
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
                b'\n' => {}
                _ if line.len() >= MAX_LINE => {
                    line.clear();
                    long = true;
                }
                _ if long => {}
                _ => line.push(b),
            }
        }
    }
}

fn read_data(mut s: TcpStream, tx: &SyncSender<Input>, hold: &Hold) {
    let mut buf = vec![0; DATA_CHUNK];
    loop {
        hold.wait();
        let Ok(n @ 1..) = s.read(&mut buf) else { return };
        if tx.send(Input::Data(buf[..n].to_vec())).is_err() {
            return;
        }
    }
}
