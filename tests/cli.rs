use std::process::Command;

fn strip_ansi(value: &str) -> String {
    let mut output = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for control in chars.by_ref() {
                if ('@'..='~').contains(&control) {
                    break;
                }
            }
        } else {
            output.push(ch);
        }
    }
    output
}

#[test]
fn demo_frames_fit_terminal_and_do_not_scroll_the_last_row() {
    for (width, height) in [(65, 13), (65, 19), (80, 25), (120, 40)] {
        for color in [false, true] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_nost"));
            command
                .args(["--demo", "--once"])
                .env("COLUMNS", width.to_string())
                .env("LINES", height.to_string())
                .env("TERM", "xterm-direct")
                .env_remove("NO_COLOR");
            if !color {
                command.arg("--no-color");
            }
            let output = command.output().expect("run demo CLI");
            assert!(output.status.success(), "{:?}", output.stderr);
            let output = String::from_utf8(output.stdout).expect("UTF-8 frame");
            let frame = output
                .split_once("\x1b[?2026h")
                .expect("synchronized frame begins")
                .1
                .split_once("\x1b[?2026l")
                .expect("synchronized frame ends")
                .0;
            assert!(frame.ends_with("\x1b[J"));
            assert!(!frame.ends_with("\n\x1b[J"), "final border must not scroll");
            let plain = strip_ansi(frame);
            let rows: Vec<_> = plain.lines().collect();
            assert!(rows.len() <= height, "{width}x{height}: too many rows");
            for row in rows {
                assert!(
                    row.chars().count() < width,
                    "{width}x{height}, color={color}: row exceeds usable width: {row:?}"
                );
                assert!(row.ends_with('│') || row.ends_with('╮') || row.ends_with('╯') || row.ends_with('┤'));
            }
        }
    }
}

#[test]
fn sigtstp_restores_terminal_then_reenters_on_continue() {
    use std::process::Stdio;
    use std::thread::sleep;
    use std::time::Duration;

    let child = Command::new(env!("CARGO_BIN_EXE_nost"))
        .args(["--demo", "--no-color", "--interval", "0.1"])
        .env("COLUMNS", "80")
        .env("LINES", "25")
        .stdout(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn demo");
    let pid = child.id().to_string();
    let signal = |name: &str| {
        let status = Command::new("kill").args([&format!("-{name}"), &pid]).status().expect("kill");
        assert!(status.success());
    };
    sleep(Duration::from_millis(300));
    signal("TSTP");
    sleep(Duration::from_millis(300));
    signal("CONT");
    sleep(Duration::from_millis(300));
    signal("TERM");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success(), "{:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // enter, leave (suspend), enter (resume), leave (exit)
    assert_eq!(stdout.matches("\x1b[?1049h").count(), 2, "alt screen entered on start and resume");
    assert_eq!(stdout.matches("\x1b[?1049l").count(), 2, "alt screen left on suspend and exit");
    let suspend_at = stdout.find("\x1b[?1049l").unwrap();
    let resume_at = stdout.rfind("\x1b[?1049h").unwrap();
    assert!(suspend_at < resume_at);
    assert!(stdout[..resume_at].contains("\x1b[?25h"), "cursor shown while suspended");
}

#[test]
fn demo_json_is_one_escape_free_line() {
    let output = Command::new(env!("CARGO_BIN_EXE_nost"))
        .args(["--demo", "--json"])
        .output()
        .expect("run demo json");
    assert!(output.status.success(), "{:?}", output.stderr);
    let text = String::from_utf8(output.stdout).expect("UTF-8");
    assert_eq!(text.lines().count(), 1);
    assert!(!text.contains('\x1b'));
    assert!(text.starts_with("{\"schema\":1,"));
}
