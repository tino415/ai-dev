use std::path::{Path, PathBuf};
use std::process::Output;

use tokio::process::Command;

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
        std::fs::create_dir_all(&self.log_dir).map_err(|e| e.to_string())?;
        std::fs::write(self.log_path(session), "").map_err(|e| e.to_string())?;
        self.run(&["new-session", "-d", "-s", session, "-x", "220", "-y", "50"])
            .await?;
        let pipe = format!("cat >> '{}'", self.log_path(session).display());
        self.run(&["pipe-pane", "-t", session, &pipe]).await
    }

    pub async fn stop(&self, session: &str) {
        let _ = self.run(&["kill-session", "-t", session]).await;
    }

    pub async fn is_alive(&self, session: &str) -> bool {
        self.run(&["has-session", "-t", session]).await.is_ok()
    }

    pub async fn type_lines(&self, session: &str, text: &str) -> Result<(), String> {
        for line in text.split('\n') {
            if !line.is_empty() {
                self.run(&["send-keys", "-t", session, "-l", "--", line]).await?;
            }
            self.run(&["send-keys", "-t", session, "Enter"]).await?;
        }
        Ok(())
    }

    pub fn log_len(&self, session: &str) -> usize {
        std::fs::metadata(self.log_path(session))
            .map(|meta| meta.len() as usize)
            .unwrap_or(0)
    }

    pub fn log_since(&self, session: &str, offset: usize) -> String {
        let bytes = std::fs::read(self.log_path(session)).unwrap_or_default();
        String::from_utf8_lossy(bytes.get(offset..).unwrap_or_default()).into_owned()
    }

    async fn run(&self, args: &[&str]) -> Result<(), String> {
        let output: Output = Command::new("tmux")
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .args(args)
            .output()
            .await
            .map_err(|e| format!("tmux: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "tmux {}: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
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
                    for next in chars.by_ref() {
                        if next == '\x07' {
                            break;
                        }
                    }
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
}
