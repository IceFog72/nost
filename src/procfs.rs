use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub struct Procfs {
    users: HashMap<u32, String>,
}

impl Procfs {
    pub fn new() -> Self {
        Self {
            users: load_users(),
        }
    }

    pub fn metadata(&self, pid: u32) -> (String, String) {
        let user = self
            .uid_for_pid(pid)
            .and_then(|uid| self.users.get(&uid).cloned())
            .unwrap_or_else(|| "?".to_string());
        let name = process_name(pid).unwrap_or_else(|| "?".to_string());
        (user, name)
    }

    fn uid_for_pid(&self, pid: u32) -> Option<u32> {
        let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:") {
                return rest.split_whitespace().next()?.parse().ok();
            }
        }
        None
    }
}

fn load_users() -> HashMap<u32, String> {
    let mut users = HashMap::new();
    let Ok(passwd) = fs::read_to_string("/etc/passwd") else {
        return users;
    };

    for line in passwd.lines() {
        let mut fields = line.split(':');
        let Some(name) = fields.next() else { continue };
        let _password = fields.next();
        let Some(uid) = fields.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        users.insert(uid, name.to_string());
    }
    users
}

fn process_name(pid: u32) -> Option<String> {
    if let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) {
        if !cmdline.is_empty() {
            let args: Vec<&[u8]> = cmdline
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .collect();
            if let Some(first) = args.first() {
                let first_text = String::from_utf8_lossy(first);
                let executable = basename(first_text.as_ref());
                if executable == "python" || executable.starts_with("python3") {
                    if let Some(second) = args.get(1) {
                        let second_text = String::from_utf8_lossy(second);
                        let script = basename(second_text.as_ref());
                        if !script.is_empty() {
                            return Some(format!("{executable} {script}"));
                        }
                    }
                }
                if !executable.is_empty() {
                    return Some(executable);
                }
            }
        }
    }

    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

fn basename(value: &str) -> String {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::basename;

    #[test]
    fn extracts_basename() {
        assert_eq!(basename("/usr/bin/python3"), "python3");
        assert_eq!(basename("firefox"), "firefox");
    }
}
