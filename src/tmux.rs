use std::ffi::CStr;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::model::short_id;

const LAST_LINES_OF_A_DEAD_PANE: usize = 20;
const CLIENT_GONE_WAIT: Duration = Duration::from_secs(2);
const CLIENT_GONE_POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

pub const DETACHED_SIZE: TerminalSize = TerminalSize {
    cols: 220,
    rows: 50,
};

pub struct AttachedClient(tokio::process::Child);

#[derive(Clone)]
pub struct Tmux {
    socket: String,
    log_dir: PathBuf,
}

impl Tmux {
    pub fn new(socket: &str, log_dir: &Path) -> Self {
        Tmux {
            socket: socket.to_string(),
            log_dir: log_dir.to_path_buf(),
        }
    }

    pub fn socket(&self) -> &str {
        &self.socket
    }

    pub fn log_path(&self, session: &str) -> PathBuf {
        self.log_dir.join(format!("{session}.log"))
    }

    pub async fn start(&self, session: &str) -> Result<(), String> {
        let shell = login_shell()?;
        self.create_private_log(session)?;
        let _ = self
            .run(&["kill-session", "-t", &exact_session(session)])
            .await;
        let (cols, rows) = (
            DETACHED_SIZE.cols.to_string(),
            DETACHED_SIZE.rows.to_string(),
        );
        self.run(&[
            "start-server",
            ";",
            "set-option",
            "-wg",
            "remain-on-exit",
            "on",
            ";",
            "new-session",
            "-d",
            "-s",
            session,
            "-x",
            &cols,
            "-y",
            &rows,
            &shell,
            "-l",
        ])
        .await?;
        let pipe = format!("cat >> '{}'", self.log_path(session).display());
        self.run(&["pipe-pane", "-t", &exact_pane(session), &pipe])
            .await
            .map(|_| ())
    }

    pub async fn stop(&self, session: &str) {
        let _ = self
            .run(&["kill-session", "-t", &exact_session(session)])
            .await;
        let _ = std::fs::remove_file(self.log_path(session));
    }

    pub async fn is_alive(&self, session: &str) -> bool {
        self.pane_dead(session).await == Some(false)
    }

    pub async fn dead_pane_output(&self, session: &str) -> Option<String> {
        if self.pane_dead(session).await != Some(true) {
            return None;
        }
        let screen = self
            .run(&["capture-pane", "-p", "-J", "-t", &exact_pane(session)])
            .await
            .ok()?;
        let lines: Vec<&str> = screen.trim_end().lines().collect();
        Some(lines[lines.len().saturating_sub(LAST_LINES_OF_A_DEAD_PANE)..].join("\n"))
    }

    pub async fn is_on_the_alternate_screen(&self, session: &str) -> bool {
        self.run(&[
            "display-message",
            "-p",
            "-t",
            &exact_pane(session),
            "#{alternate_on}",
        ])
        .await
        .is_ok_and(|flag| flag.trim() == "1")
    }

    pub async fn screen(&self, session: &str) -> Option<String> {
        self.run(&["capture-pane", "-p", "-t", &exact_pane(session)])
            .await
            .ok()
    }

    async fn pane_dead(&self, session: &str) -> Option<bool> {
        self.run(&[
            "list-panes",
            "-t",
            &exact_pane(session),
            "-F",
            "#{pane_dead}",
        ])
        .await
        .ok()
        .map(|flags| flags.lines().next() == Some("1"))
    }

    pub async fn type_lines(&self, session: &str, text: &str) -> Result<(), String> {
        let each_line_and_its_enter: String = text
            .replace("\r\n", "\n")
            .split('\n')
            .map(|line| format!("{line}\n"))
            .collect();
        self.paste_through_stdin(session, &each_line_and_its_enter)
            .await
    }

    pub async fn send_hidden_line(&self, session: &str, line: &str) -> Result<(), String> {
        self.paste_through_stdin(session, &format!("{line}\n"))
            .await
    }

    pub async fn interrupt(&self, session: &str) -> Result<(), String> {
        self.run(&["send-keys", "-t", &exact_pane(session), "C-c"])
            .await
            .map(|_| ())
    }

    pub async fn send_sigurg_to_the_terminal(&self, session: &str) -> Result<(), String> {
        let terminal = self.pane_terminal(session).await?;
        tokio::task::spawn_blocking(move || send_sigurg_to_processes_on(terminal.trim()))
            .await
            .map_err(|e| e.to_string())?
    }

    async fn pane_terminal(&self, session: &str) -> Result<String, String> {
        self.run(&[
            "display-message",
            "-p",
            "-t",
            &exact_pane(session),
            "#{pane_tty}",
        ])
        .await
    }

    async fn paste_through_stdin(&self, session: &str, text: &str) -> Result<(), String> {
        let buffer = format!("aidev-{}", short_id());
        self.run_with_stdin(&["load-buffer", "-b", &buffer, "-"], text)
            .await?;
        let pasted = self
            .run(&[
                "paste-buffer",
                "-d",
                "-b",
                &buffer,
                "-t",
                &exact_pane(session),
            ])
            .await;
        if pasted.is_err() {
            let _ = self.run(&["delete-buffer", "-b", &buffer]).await;
        }
        pasted.map(|_| ())
    }

    pub fn log_len(&self, session: &str) -> usize {
        std::fs::metadata(self.log_path(session))
            .map(|meta| meta.len() as usize)
            .unwrap_or(0)
    }

    pub fn log_since(&self, session: &str, offset: usize) -> String {
        let Ok(mut file) = std::fs::File::open(self.log_path(session)) else {
            return String::new();
        };
        let mut bytes = Vec::new();
        if file.seek(SeekFrom::Start(offset as u64)).is_ok() {
            let _ = file.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn create_private_log(&self, session: &str) -> Result<(), String> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.log_dir)
            .map_err(|e| e.to_string())?;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.log_path(session))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub async fn has_attached_client(&self, session: &str) -> bool {
        !self.client_pids(session).await.is_empty()
    }

    async fn client_pids(&self, session: &str) -> Vec<u32> {
        self.run(&[
            "list-clients",
            "-t",
            &exact_session(session),
            "-F",
            "#{client_pid}",
        ])
        .await
        .map(|pids| {
            pids.lines()
                .filter_map(|pid| pid.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
    }

    pub fn attach_client(
        &self,
        session: &str,
        size: TerminalSize,
    ) -> Result<(pty_process::Pty, AttachedClient), String> {
        let (pty, pts) = pty_process::open().map_err(|e| format!("pty: {e}"))?;
        pty.resize(pty_size(size))
            .map_err(|e| format!("pty: {e}"))?;
        let process = pty_process::Command::new("tmux")
            .args([
                "-L",
                &self.socket,
                "-f",
                "/dev/null",
                "attach-session",
                "-t",
                &exact_session(session),
            ])
            .env("TERM", "xterm-256color")
            .kill_on_drop(true)
            .spawn(pts)
            .map_err(|e| format!("tmux attach: {e}"))?;
        Ok((pty, AttachedClient(process)))
    }

    pub async fn detach_client(&self, session: &str, AttachedClient(mut process): AttachedClient) {
        let pid = process.id();
        let _ = process.kill().await;
        if let Some(pid) = pid {
            self.wait_until_the_server_drops(session, pid).await;
        }
        self.restore_size(session).await;
    }

    async fn wait_until_the_server_drops(&self, session: &str, pid: u32) {
        let deadline = tokio::time::Instant::now() + CLIENT_GONE_WAIT;
        while self.client_pids(session).await.contains(&pid)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(CLIENT_GONE_POLL).await;
        }
    }

    pub async fn restore_size(&self, session: &str) {
        if self.has_attached_client(session).await {
            return;
        }
        let window = exact_pane(session);
        let (cols, rows) = (
            DETACHED_SIZE.cols.to_string(),
            DETACHED_SIZE.rows.to_string(),
        );
        let _ = self
            .run(&[
                "resize-window",
                "-t",
                &window,
                "-x",
                &cols,
                "-y",
                &rows,
                ";",
                "set-option",
                "-wu",
                "-t",
                &window,
                "window-size",
            ])
            .await;
    }

    async fn run(&self, args: &[&str]) -> Result<String, String> {
        let child = self
            .command(args)
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| format!("tmux: {e}"))?;
        finished(args, child.wait_with_output().await)
    }

    async fn run_with_stdin(&self, args: &[&str], input: &str) -> Result<String, String> {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| format!("tmux: {e}"))?;
        let mut stdin = child.stdin.take().expect("stdin was piped");
        stdin
            .write_all(input.as_bytes())
            .await
            .map_err(|e| format!("tmux: {e}"))?;
        drop(stdin);
        finished(args, child.wait_with_output().await)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("tmux");
        command
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

fn finished(args: &[&str], output: std::io::Result<Output>) -> Result<String, String> {
    let output = output.map_err(|e| format!("tmux: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "tmux {}: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn login_shell() -> Result<String, String> {
    let uid = unsafe { libc::getuid() };
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() || entry.pw_shell.is_null() {
        return Err(format!("no passwd entry for uid {uid}"));
    }
    let shell = unsafe { CStr::from_ptr(entry.pw_shell) }
        .to_string_lossy()
        .into_owned();
    Ok(if shell.is_empty() {
        "/bin/sh".into()
    } else {
        shell
    })
}

pub fn pty_size(size: TerminalSize) -> pty_process::Size {
    pty_process::Size::new(size.rows, size.cols)
}

fn send_sigurg_to_processes_on(terminal: &str) -> Result<(), String> {
    let device = std::fs::metadata(terminal)
        .map_err(|e| format!("{terminal}: {e}"))?
        .rdev();
    for pid in processes_on_the_terminal(proc_tty_nr(device)) {
        unsafe { libc::kill(pid, libc::SIGURG) };
    }
    Ok(())
}

fn processes_on_the_terminal(tty_nr: u64) -> Vec<libc::pid_t> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .filter(|pid: &libc::pid_t| terminal_of_process(*pid) == Some(tty_nr))
        .collect()
}

fn terminal_of_process(pid: libc::pid_t) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_the_command_name = &stat[stat.rfind(')')? + 1..];
    after_the_command_name
        .split_whitespace()
        .nth(4)?
        .parse()
        .ok()
}

fn proc_tty_nr(device: u64) -> u64 {
    let major = libc::major(device) as u64;
    let minor = libc::minor(device) as u64;
    (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
}

fn exact_session(session: &str) -> String {
    format!("={session}")
}

fn exact_pane(session: &str) -> String {
    format!("={session}:")
}

pub fn strip_terminal_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some('(' | ')' | '*' | '+') => {
                    chars.next();
                }
                _ => {}
            },
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colors_and_carriage_returns() {
        assert_eq!(strip_terminal_escapes("\x1b[32mok\x1b[0m\r\n"), "ok\n");
    }

    #[test]
    fn strips_titles_ended_by_either_terminator_and_charset_switches() {
        assert_eq!(strip_terminal_escapes("\x1b]0;title\x1b\\ok"), "ok");
        assert_eq!(strip_terminal_escapes("\x1b]0;title\x07ok"), "ok");
        assert_eq!(strip_terminal_escapes("\x1b(Bok"), "ok");
    }
}
