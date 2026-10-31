use crate::writer::Writer;
use std::io::{self, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;

pub const FEND: u8 = 0xC0;
pub(crate) const FESC: u8 = 0xDB;
pub(crate) const TFEND: u8 = 0xDC;
pub(crate) const TFESC: u8 = 0xDD;
pub(crate) const MAX_CLIENTS: usize = 10;
pub const DEFAULT_PORT: u16 = 8100;
pub const INPUT_QUEUE: usize = 64;
const MAX_FRAME: usize = 5000;
const MIN_FRAME_WITH_FENDS: usize = 19;
const CMD_ANY_LENGTH: u8 = 2;

pub fn escape(cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![FEND, cmd];
    for &b in data {
        match b {
            FEND => out.extend([FESC, TFEND]),
            FESC => out.extend([FESC, TFESC]),
            _ => out.push(b),
        }
    }
    out.push(FEND);
    out
}

pub(crate) fn unescape(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        match (body[i], body.get(i + 1)) {
            (FESC, Some(&TFEND)) => out.push(FEND),
            (FESC, Some(&TFESC)) => out.push(FESC),
            (b, _) => {
                out.push(b);
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    out
}

#[derive(Clone, Debug, Default)]
pub struct Deframer {
    acc: Vec<u8>,
    long: bool,
}

impl Deframer {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        for &b in bytes {
            if b != FEND {
                if self.acc.len() < MAX_FRAME {
                    self.acc.push(b);
                } else {
                    self.long = true;
                }
                continue;
            }
            let f = std::mem::take(&mut self.acc);
            let long = std::mem::take(&mut self.long);
            if let Some(&cmd) = f.first() {
                if !long && cmd <= CMD_ANY_LENGTH && (cmd == CMD_ANY_LENGTH || f.len() + 2 >= MIN_FRAME_WITH_FENDS) {
                    out.push((cmd, unescape(&f[1..])));
                }
            }
        }
        out
    }
}

type Clients = Arc<Mutex<Vec<Option<(u64, Writer)>>>>;

pub struct KissServer {
    pub input: Receiver<(u8, Vec<u8>)>,
    clients: Clients,
    closed: Arc<AtomicBool>,
    accept: Vec<thread::JoinHandle<()>>,
    pub addr: SocketAddr,
}

impl KissServer {
    pub fn bind(addr: &str, port: u16) -> io::Result<Self> {
        let l = TcpListener::bind((addr, port))?;
        let (tx, input) = sync_channel(INPUT_QUEUE);
        let clients: Clients = Arc::new(Mutex::new((0..MAX_CLIENTS).map(|_| None).collect()));
        let closed = Arc::<AtomicBool>::default();
        let mut s =
            Self { input, clients: clients.clone(), closed: closed.clone(), accept: Vec::new(), addr: l.local_addr()? };
        let t = thread::Builder::new().name("kiss-accept".into()).spawn(move || {
            for id in 0u64..=u64::MAX {
                let s = match l.accept() {
                    _ if closed.load(Ordering::SeqCst) => return,
                    Ok((s, _)) => s,
                    Err(_) => {
                        thread::sleep(crate::server::ACCEPT_BACKOFF);
                        continue;
                    }
                };
                let _ = s.set_nodelay(true);
                {
                    let mut g = clients.lock().unwrap();
                    let Some(slot) = g.iter_mut().find(|c| c.is_none()) else { continue };
                    let Ok(w) = Writer::spawn(&s) else { continue };
                    *slot = Some((id, w));
                }
                let (tx, c) = (tx.clone(), clients.clone());
                if thread::Builder::new().spawn(move || read(s, id, tx, c)).is_err() {
                    drop_client(&clients, id);
                }
            }
        })?;
        s.accept.push(t);
        Ok(s)
    }

    pub fn has_clients(&self) -> bool {
        self.clients.lock().unwrap().iter().any(Option::is_some)
    }

    pub fn send(&self, cmd: u8, data: &[u8]) {
        let frame = escape(cmd, data);
        for (_, w) in self.clients.lock().unwrap().iter().flatten() {
            w.send(&frame);
        }
    }
}

impl KissServer {
    /// Disconnects the clients as well as freeing the port.
    pub fn close(mut self) {
        // stop accepting first, so no client attaches after the disconnect below
        self.stop();
        for (_, w) in self.clients.lock().unwrap_or_else(|e| e.into_inner()).iter().flatten() {
            w.close();
        }
    }

    fn stop(&mut self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        crate::server::wake(self.addr);
        if !crate::server::join(&mut self.accept, || {}) {
            crate::say(format_args!("KISS port {} is still listening", self.addr));
        }
    }
}

// Frees the port (bounded wait); clients stay connected until they leave, as for Host.
impl Drop for KissServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn read(mut s: TcpStream, id: u64, tx: SyncSender<(u8, Vec<u8>)>, clients: Clients) {
    let mut d = Deframer::default();
    let mut buf = vec![0; 4096];
    while let Ok(n @ 1..) = s.read(&mut buf) {
        for f in d.feed(&buf[..n]) {
            if tx.send(f).is_err() {
                return;
            }
        }
    }
    drop_client(&clients, id);
}

fn drop_client(clients: &Clients, id: u64) {
    for c in clients.lock().unwrap().iter_mut() {
        if c.as_ref().is_some_and(|(i, _)| *i == id) {
            *c = None;
        }
    }
}
