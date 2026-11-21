use std::ffi::CStr;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::model::short_id;

const LAST_LINES_OF_A_DEAD_PANE: usize = 20;

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
        let _ = self.run(&["kill-session", "-t", &exact_session(session)]).await;
        self.run(&[
            "start-server", ";",
            "set-option", "-wg", "remain-on-exit", "on", ";",
            "new-session", "-d", "-s", session, "-x", "220", "-y", "50", &shell, "-l",
        ])
        .await?;
        let pipe = format!("cat >> '{}'", self.log_path(session).display());
        self.run(&["pipe-pane", "-t", &exact_pane(session), &pipe]).await.map(|_| ())
    }

    pub async fn stop(&self, session: &str) {
        let _ = self.run(&["kill-session", "-t", &exact_session(session)]).await;
        let _ = std::fs::remove_file(self.log_path(session));
    }

    pub async fn is_alive(&self, session: &str) -> bool {
        self.pane_dead(session).await == Some(false)
    }

    pub async fn dead_pane_output(&self, session: &str) -> Option<String> {
        if self.pane_dead(session).await != Some(true) {
            return None;
        }
        let screen = self.run(&["capture-pane", "-p", "-J", "-t", &exact_pane(session)]).await.ok()?;
        let lines: Vec<&str> = screen.trim_end().lines().collect();
        Some(lines[lines.len().saturating_sub(LAST_LINES_OF_A_DEAD_PANE)..].join("\n"))
    }

    async fn pane_dead(&self, session: &str) -> Option<bool> {
        self.run(&["list-panes", "-t", &exact_pane(session), "-F", "#{pane_dead}"])
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
        self.paste_through_stdin(session, &each_line_and_its_enter).await
    }

    pub async fn send_hidden_line(&self, session: &str, line: &str) -> Result<(), String> {
        self.paste_through_stdin(session, &format!("{line}\n")).await
    }

    pub async fn interrupt(&self, session: &str) -> Result<(), String> {
        self.run(&["send-keys", "-t", &exact_pane(session), "C-c"]).await.map(|_| ())
    }

    async fn paste_through_stdin(&self, session: &str, text: &str) -> Result<(), String> {
        let buffer = format!("aidev-{}", short_id());
        self.run_with_stdin(&["load-buffer", "-b", &buffer, "-"], text).await?;
        let pasted = self.run(&["paste-buffer", "-d", "-b", &buffer, "-t", &exact_pane(session)]).await;
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
        self.run(&["list-clients", "-t", &exact_session(session), "-F", "#{client_tty}"])
            .await
            .is_ok_and(|clients| !clients.trim().is_empty())
    }

    async fn run(&self, args: &[&str]) -> Result<String, String> {
        let child = self.command(args).stdin(Stdio::null()).spawn().map_err(|e| format!("tmux: {e}"))?;
        finished(args, child.wait_with_output().await)
    }

    async fn run_with_stdin(&self, args: &[&str], input: &str) -> Result<String, String> {
        let mut child = self.command(args).stdin(Stdio::piped()).spawn().map_err(|e| format!("tmux: {e}"))?;
        let mut stdin = child.stdin.take().expect("stdin was piped");
        stdin.write_all(input.as_bytes()).await.map_err(|e| format!("tmux: {e}"))?;
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
    let status = unsafe { libc::getpwuid_r(uid, &mut entry, buffer.as_mut_ptr(), buffer.len(), &mut found) };
    if status != 0 || found.is_null() || entry.pw_shell.is_null() {
        return Err(format!("no passwd entry for uid {uid}"));
    }
    let shell = unsafe { CStr::from_ptr(entry.pw_shell) }.to_string_lossy().into_owned();
    Ok(if shell.is_empty() { "/bin/sh".into() } else { shell })
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
