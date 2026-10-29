//! systemd notifications (`Type=notify`): READY / WATCHDOG reach the notify socket.
//! (Its own test binary: it sets NOTIFY_SOCKET for the process.)
#[cfg(target_os = "linux")]
#[test]
fn notifications_reach_systemd() {
    use std::os::unix::net::UnixDatagram;
    let dir = std::env::temp_dir().join(format!("cw-notify-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("notify.sock");
    let _ = std::fs::remove_file(&path);
    let sock = UnixDatagram::bind(&path).unwrap();
    assert!(!cookie_web::watch::sd_notify("READY=1"), "no socket configured: nothing sent");
    // SAFETY: this test binary has a single test, so no other thread reads the environment.
    unsafe { std::env::set_var("NOTIFY_SOCKET", &path) };
    assert!(cookie_web::watch::sd_notify("READY=1"));
    let mut buf = [0u8; 64];
    let n = sock.recv(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"READY=1");
    let _ = std::fs::remove_dir_all(&dir);
}
