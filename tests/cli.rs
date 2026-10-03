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
                assert!(row.ends_with('│') || row.ends_with('╮') || row.ends_with('╯'));
            }
        }
    }
}
