use std::ffi::{CStr, c_char, c_int};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;
use xara_ffi::*;

fn config(call: &CStr, mode: Mode) -> Config {
    let mut c = std::mem::MaybeUninit::<Config>::uninit();
    let mut c = unsafe {
        xara_config_default(c.as_mut_ptr());
        c.assume_init()
    };
    (c.call, c.mode, c.cmd_port, c.kiss_port) = (call.as_ptr(), mode as c_int, 0, 0);
    c
}

fn create(c: &Config) -> Result<*mut Handle, String> {
    let mut err = [0 as c_char; 128];
    let m = unsafe { xara_create(c, err.as_mut_ptr(), err.len()) };
    if m.is_null() { Err(unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned()) } else { Ok(m) }
}

fn status(m: *const Handle) -> Status {
    let mut st = unsafe { std::mem::zeroed::<Status>() };
    unsafe { xara_get_status(m, &mut st) };
    st
}

fn text(s: &[c_char]) -> String {
    unsafe { CStr::from_ptr(s.as_ptr()) }.to_string_lossy().into_owned()
}

#[test]
fn rejects_bad_config() {
    let mut c = config(c"N0CALL", Mode::Hf2300);
    c.call = std::ptr::null();
    assert!(create(&c).unwrap_err().contains("call sign"));
    let mut c = config(c"N0 CALL", Mode::Hf2300);
    assert!(create(&c).unwrap_err().contains("invalid call"));
    c.call = c"N0CALL-ABC".as_ptr();
    assert!(create(&c).unwrap_err().contains("invalid call"));
    c.call = c"N0CALL".as_ptr();
    c.mode = 5;
    assert!(create(&c).unwrap_err().contains("invalid mode"));
    c.mode = -1;
    assert!(create(&c).unwrap_err().contains("invalid mode"));
    c.mode = Mode::Hf500 as c_int;
    c.drive_db = 3.0;
    assert!(create(&c).unwrap_err().contains("drive_db"));
}

#[test]
fn rejects_negative_latency() {
    let m = create(&config(c"N0CALL", Mode::Hf2300)).unwrap();
    unsafe {
        assert!(!xara_set_tx_latency(m, -1, 0));
        assert!(!xara_set_tx_latency(m, 0, -1));
        assert!(xara_set_tx_latency(m, 4800, 960));
        xara_destroy(m);
    }
}

// A modem with a KISS port; ports probed as free can be taken by a parallel test before we bind them, so retry.
fn create_with_kiss(c: &mut Config) -> *mut Handle {
    for _ in 0..20 {
        c.kiss_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        match create(c) {
            Ok(m) => return m,
            Err(e) if e.contains("KISS port") => continue,
            Err(e) => panic!("{e}"),
        }
    }
    panic!("no free KISS port");
}

#[test]
fn ports_are_released_on_destroy() {
    let mut c = config(c"N0CALL", Mode::FmNarrow);
    let m = create(&c).unwrap();
    let st = status(m);
    assert!(st.cmd_port != 0 && st.data_port != 0);
    unsafe { xara_destroy(m) };
    c.cmd_port = st.cmd_port;
    let mut m = create_with_kiss(&mut c);
    let kiss = c.kiss_port;
    for i in 0..2 {
        if i > 0 {
            m = create(&c).unwrap();
        }
        assert_eq!((status(m).cmd_port, status(m).kiss_port), (st.cmd_port, kiss));
        // clients still connected must not keep the ports bound
        let _cmd = TcpStream::connect(("127.0.0.1", st.cmd_port)).unwrap();
        let _kiss = TcpStream::connect(("127.0.0.1", kiss)).unwrap();
        let x = [0.0f32; 480];
        let mut y = [0.0f32; 480];
        unsafe { xara_process(m, x.as_ptr(), y.as_mut_ptr(), x.len()) };
        unsafe { xara_destroy(m) };
    }
}

#[test]
fn idle_modem_listens_quietly() {
    let m = create(&config(c"N0CALL", Mode::Hf2300)).unwrap();
    let (x, mut y) = (vec![0.0f32; 480], vec![1.0f32; 480]);
    for _ in 0..100 {
        let f = unsafe { xara_process(m, x.as_ptr(), y.as_mut_ptr(), x.len()) };
        assert_eq!(f & (KEYED | ERROR), 0);
        assert!(y.iter().all(|&v| v == 0.0));
    }
    let st = status(m);
    assert_eq!(text(&st.state), "Listen");
    unsafe { xara_destroy(m) };
}

fn command(port: u16) -> TcpStream {
    let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_nonblocking(true).unwrap();
    s
}

fn poll(s: &mut TcpStream, seen: &mut String) {
    let mut buf = [0u8; 1024];
    while let Ok(n @ 1..) = s.read(&mut buf) {
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
}

#[test]
fn two_modems_connect_back_to_back() {
    let a = create(&config(c"N0AAA", Mode::Hf2300)).unwrap();
    let b = create(&config(c"N0BBB", Mode::Hf2300)).unwrap();
    let (mut ca, mut cb) = (command(status(a).cmd_port), command(status(b).cmd_port));
    let (mut seen_a, mut seen_b) = (String::new(), String::new());
    let (mut ab, mut ba) = (vec![0.0f32; 480], vec![0.0f32; 480]);
    let (mut out_a, mut out_b) = (vec![0.0f32; 480], vec![0.0f32; 480]);
    let mut sent = false;
    let mut keyed = false;
    for _ in 0..100 * 60 {
        let fa = unsafe { xara_process(a, ba.as_ptr(), out_a.as_mut_ptr(), 480) };
        let fb = unsafe { xara_process(b, ab.as_ptr(), out_b.as_mut_ptr(), 480) };
        assert_eq!((fa | fb) & ERROR, 0);
        keyed |= fa & KEYED != 0;
        (ab, ba) = (out_a.clone(), out_b.clone());
        if !sent {
            // the host threads hand commands over asynchronously; give them a moment
            std::thread::sleep(Duration::from_millis(20));
            ca.write_all(b"CONNECT N0AAA N0BBB\r").unwrap();
            sent = true;
        }
        poll(&mut ca, &mut seen_a);
        poll(&mut cb, &mut seen_b);
        if seen_a.contains("CONNECTED ") && seen_b.contains("CONNECTED ") {
            break;
        }
    }
    assert!(keyed, "A never keyed");
    assert!(seen_a.contains("CONNECTED N0AAA N0BBB"), "A saw {seen_a:?}");
    assert!(seen_b.contains("CONNECTED N0AAA N0BBB"), "B saw {seen_b:?}");
    assert_eq!(text(&status(a).peer), "N0BBB");
    unsafe {
        xara_destroy(a);
        xara_destroy(b);
    }
}
