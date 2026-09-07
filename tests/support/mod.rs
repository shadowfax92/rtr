//! PTY driver for the real picker-to-native-process handoff.
//! Each run owns a controlling terminal and a child guard, so assertion failures
//! cannot leave an interactive RTR process running against the fixture.
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rtr::paths::Paths;

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn drive_picker(paths: &Paths, root: &Path, args: &[&str], ready: &str, keys: &[u8]) -> String {
    drive_picker_steps(paths, root, args, &[(ready, keys)])
}

pub fn drive_picker_steps(
    paths: &Paths,
    root: &Path,
    args: &[&str],
    steps: &[(&str, &[u8])],
) -> String {
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let mut size = libc::winsize {
        ws_row: 36,
        ws_col: 140,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty initializes fresh owned descriptors and reads a valid size.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        },
        0
    );
    // SAFETY: these descriptors were just allocated and each gets one owner.
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let mut initial = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: slave is live and tcgetattr initializes the supplied termios.
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), initial.as_mut_ptr()) },
        0
    );
    let initial = unsafe { initial.assume_init() };
    let mut command = Command::new(env!("CARGO_BIN_EXE_rtr"));
    command
        .args(args)
        .current_dir(root)
        .env("RTR_CONFIG_DIR", &paths.config_dir)
        .env("RTR_STATE_DIR", &paths.state_dir)
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    // SAFETY: only async-signal-safe libc calls run in this post-fork child.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY.into(), 0) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut process = Process(command.spawn().unwrap());
    drop(slave);
    // SAFETY: fcntl updates only this test's live PTY descriptor.
    unsafe {
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        assert!(flags >= 0);
        assert_eq!(
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
            0
        );
    }

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut output = Vec::new();
    // Ratatui emits only changed cells. Recognize what a person sees after
    // cursor updates, not contiguous bytes that may omit unchanged spaces.
    let mut terminal = vt100::Parser::new(size.ws_row, size.ws_col, 0);
    let mut step = 0;
    let status = loop {
        let mut chunk = [0; 8192];
        loop {
            match master.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    terminal.process(&chunk[..n]);
                    output.extend_from_slice(&chunk[..n]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        if let Some((ready, keys)) = steps.get(step) {
            if terminal.screen().contents().contains(ready) {
                master.write_all(keys).unwrap();
                step += 1;
            }
        }
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "picker timed out at step {step}, waiting for {:?}; screen:\n{}",
            steps.get(step).map(|(ready, _)| ready),
            terminal.screen().contents()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = String::from_utf8_lossy(&output).into_owned();
    assert_eq!(
        step,
        steps.len(),
        "picker exited before finishing the key sequence: {output}"
    );
    assert!(status.success(), "picker failed ({status}): {output}");
    let mut restored = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: the master descriptor stays live after the child exits.
    assert_eq!(
        unsafe { libc::tcgetattr(master.as_raw_fd(), restored.as_mut_ptr()) },
        0
    );
    let restored = unsafe { restored.assume_init() };
    assert_eq!(
        restored.c_lflag & (libc::ICANON | libc::ECHO),
        initial.c_lflag & (libc::ICANON | libc::ECHO),
        "raw terminal mode leaked after picker exit"
    );
    output
}
