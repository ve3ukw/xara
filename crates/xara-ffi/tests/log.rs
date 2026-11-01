// The log callback is set for the whole process, so this test has a process of its own
use std::ffi::{CStr, c_char, c_int, c_void};
use std::io::Write;
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::Duration;
use xara_ffi::*;

static LINES: Mutex<Vec<(c_int, String)>> = Mutex::new(Vec::new());

unsafe extern "C" fn collect(_user: *mut c_void, level: c_int, line: *const c_char) {
    let line = unsafe { CStr::from_ptr(line) }.to_string_lossy().into_owned();
    LINES.lock().unwrap().push((level, line));
}

#[test]
fn console_lines_are_info() {
    unsafe { xara_set_log(Some(collect), std::ptr::null_mut()) };
    let mut c = std::mem::MaybeUninit::<Config>::uninit();
    let mut c = unsafe {
        xara_config_default(c.as_mut_ptr());
        c.assume_init()
    };
    (c.call, c.mode, c.cmd_port, c.kiss_port) = (c"N0CALL".as_ptr(), Mode::FmNarrow as c_int, 0, 0);
    let mut err = [0 as c_char; 128];
    let m = unsafe { xara_create(&c, err.as_mut_ptr(), err.len()) };
    assert!(!m.is_null());
    let mut st = unsafe { std::mem::zeroed::<Status>() };
    unsafe { xara_get_status(m, &mut st) };
    let mut s = TcpStream::connect(("127.0.0.1", st.cmd_port)).unwrap();
    s.write_all(b"MYCALL N0CALL\r").unwrap();

    // the command is echoed on the console when the modem takes it from the host, as it runs
    let (x, mut y) = (vec![0.0f32; 480], vec![0.0f32; 480]);
    let echoed = || LINES.lock().unwrap().iter().any(|(l, s)| *l == LOG_INFO && s.to_uppercase().contains("> MYCALL"));
    for _ in 0..200 {
        unsafe { xara_process(m, x.as_ptr(), y.as_mut_ptr(), x.len()) };
        if echoed() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(s);
    unsafe { xara_destroy(m) };
    assert!(echoed(), "{:?}", LINES.lock().unwrap());
}
