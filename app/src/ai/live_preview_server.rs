//! Runs a project's own dev server for the Live Preview.
//!
//! This is the only part of the preview that executes project code, so it never
//! starts on its own: the panel spawns it from an explicit user action, and
//! dropping the handle kills the process tree.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::live_preview::{extract_server_url, DevServerPlan};

/// How many log lines are kept for the panel's output pane.
const MAX_LOG_LINES: usize = 400;

#[derive(Debug, Clone, Default)]
pub struct DevServerOutput {
    pub lines: Vec<String>,
    pub url: Option<String>,
    /// Set once the process ends, so the panel can report a server that died
    /// on startup instead of showing a URL that will never answer.
    pub exited: Option<i32>,
}

/// A running dev server. Killing it is idempotent, and the `Drop` impl makes
/// sure closing the preview never leaves an orphan node process behind.
pub struct DevServerHandle {
    plan: DevServerPlan,
    child: Arc<Mutex<Option<Child>>>,
    output: Arc<Mutex<DevServerOutput>>,
    stopped: Arc<AtomicBool>,
}

impl DevServerHandle {
    pub fn start(root: &Path, plan: &DevServerPlan) -> Result<Self, String> {
        let mut command = build_command(&plan.command, &plan.args);
        command
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Vite/Next print ANSI-decorated boxes; asking for plain output keeps
            // the URL detection and the log pane readable.
            .env("FORCE_COLOR", "0")
            .env("NO_COLOR", "1")
            .env("BROWSER", "none");

        let mut child = command
            .spawn()
            .map_err(|error| format!("Could not start `{}`: {error}", plan.display_command()))?;

        let output = Arc::new(Mutex::new(DevServerOutput::default()));
        let stopped = Arc::new(AtomicBool::new(false));

        if let Some(stdout) = child.stdout.take() {
            spawn_reader(BufReader::new(stdout), output.clone(), stopped.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(BufReader::new(stderr), output.clone(), stopped.clone());
        }

        Ok(Self {
            plan: plan.clone(),
            child: Arc::new(Mutex::new(Some(child))),
            output,
            stopped,
        })
    }

    /// A snapshot of everything the server has printed so far, plus the URL it
    /// announced (falling back to the framework's default port).
    pub fn snapshot(&self) -> DevServerOutput {
        let mut output = self.output.lock().map(|out| out.clone()).unwrap_or_default();
        if output.url.is_none() && output.exited.is_none() {
            // Some servers never print a URL; the plan's default port is the
            // best guess until one shows up.
            output.url = Some(self.plan.default_url());
        }
        output
    }

    pub fn is_running(&self) -> bool {
        if self.stopped.load(Ordering::Relaxed) {
            return false;
        }
        let Ok(mut guard) = self.child.lock() else {
            return false;
        };
        let Some(child) = guard.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                if let Ok(mut output) = self.output.lock() {
                    output.exited = Some(status.code().unwrap_or(-1));
                }
                false
            }
            Ok(None) => true,
            Err(_) => false,
        }
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        let Ok(mut guard) = self.child.lock() else {
            return;
        };
        let Some(mut child) = guard.take() else {
            return;
        };
        kill_process_tree(&mut child);
        let _ = child.wait();
    }
}

impl Drop for DevServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_reader<R: BufRead + Send + 'static>(
    reader: R,
    output: Arc<Mutex<DevServerOutput>>,
    stopped: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        for line in reader.lines() {
            if stopped.load(Ordering::Relaxed) {
                return;
            }
            let Ok(line) = line else { return };
            let line = strip_ansi(&line);
            let Ok(mut output) = output.lock() else {
                return;
            };
            if output.url.is_none() {
                if let Some(url) = extract_server_url(&line) {
                    output.url = Some(url);
                }
            }
            output.lines.push(line);
            if output.lines.len() > MAX_LOG_LINES {
                let overflow = output.lines.len() - MAX_LOG_LINES;
                output.lines.drain(..overflow);
            }
        }
    });
}

/// npm/yarn/pnpm are shell scripts on Windows, so they have to go through the
/// shell rather than being exec'd directly.
fn build_command(program: &str, args: &[String]) -> Command {
    #[cfg(target_os = "windows")]
    {
        let mut command = Command::new("cmd");
        command.arg("/C").arg(program).args(args);
        command
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut command = Command::new(program);
        command.args(args);
        command
    }
}

/// A dev server spawns workers; killing only the launcher would leave the port
/// bound, so take the whole tree down.
fn kill_process_tree(child: &mut Child) {
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

/// Dev servers print progress with ANSI escapes even when asked not to; the log
/// pane renders plain text.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            out.push(character);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn strips_ansi_sequences() {
        assert_eq!(
            strip_ansi("\u{1b}[32mready\u{1b}[0m - started server on http://localhost:3000"),
            "ready - started server on http://localhost:3000"
        );
    }

    #[test]
    fn leaves_plain_text_alone() {
        assert_eq!(strip_ansi("VITE v5.0.0  ready in 300 ms"), "VITE v5.0.0  ready in 300 ms");
    }
}
