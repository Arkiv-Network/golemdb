//! Optional `perf record` around chosen commits (`profile_at`), for flamegraphs
//! at different database sizes. perf attaches to this process, all threads.
//!
//! perf starts with sampling off and is switched on and off through its control
//! fifos, waiting for its ack each time, so the recording covers exactly one branch.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub struct Recording {
    perf: Child,
    ctl: File,
    acks: Receiver<String>,
    fifos: PathBuf,
}

pub fn check() -> Result<()> {
    let out = std::env::temp_dir().join(format!("golemdb-perf-check-{}", std::process::id()));
    let result = start(&out).and_then(Recording::stop);
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_dir_all(fifos_dir());
    result.context("this scenario profiles commits (`profile_at`), but perf cannot record it")
}

pub fn start(out: &Path) -> Result<Recording> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let fifos = fifos_dir();
    std::fs::create_dir_all(&fifos)?;
    let (ctl, ack) = (fifos.join("ctl"), fifos.join("ack"));
    if !Command::new("mkfifo")
        .arg(&ctl)
        .arg(&ack)
        .status()?
        .success()
    {
        bail!("mkfifo failed");
    }
    let mut perf = match Command::new("perf")
        .args([
            "record",
            "-q",
            "-e",
            "cpu-clock",
            "-F",
            "999",
            "--call-graph",
            "fp",
            "-D",
            "-1",
        ])
        .arg(format!(
            "--control=fifo:{},{}",
            ctl.display(),
            ack.display()
        ))
        .arg("-p")
        .arg(std::process::id().to_string())
        .arg("-o")
        .arg(out)
        .stdout(Stdio::null())
        .stderr(File::create(fifos.join("stderr"))?)
        .spawn()
    {
        Ok(perf) => perf,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("perf is not installed"),
        Err(e) => return Err(e).context("starting perf"),
    };
    // Opening a fifo blocks until perf opens the other end, and perf may exit
    // instead, so the fifos are opened and read on a thread while this one
    // watches perf (see `wait`).
    let (opened_tx, opened) = channel();
    let (acks_tx, acks) = channel();
    std::thread::spawn(move || -> std::io::Result<()> {
        let ctl = OpenOptions::new().write(true).open(&ctl)?;
        let mut ack = BufReader::new(File::open(&ack)?);
        let _ = opened_tx.send(ctl);
        let mut line = String::new();
        while ack.read_line(&mut line)? > 0 {
            if acks_tx.send(std::mem::take(&mut line)).is_err() {
                break;
            }
        }
        Ok(())
    });
    let ctl = wait(&mut perf, &fifos, &opened)?;
    let mut recording = Recording {
        perf,
        ctl,
        acks,
        fifos,
    };
    recording.command("enable")?;
    Ok(recording)
}

fn fifos_dir() -> PathBuf {
    std::env::temp_dir().join(format!("golemdb-perf-{}", std::process::id()))
}

/// The next message from the fifo thread, or an error with perf's own message
/// if perf exits or stops answering first.
fn wait<T>(perf: &mut Child, fifos: &Path, from: &Receiver<T>) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match from.recv_timeout(Duration::from_millis(50)) {
            Ok(message) => return Ok(message),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(Duration::from_millis(50)),
        }
        if let Some(status) = perf.try_wait()? {
            bail!("perf exited ({status}):\n{}", stderr(fifos));
        }
        if Instant::now() >= deadline {
            bail!("perf did not answer within 30 s");
        }
    }
}

fn stderr(fifos: &Path) -> String {
    let text = std::fs::read_to_string(fifos.join("stderr")).unwrap_or_default();
    format!(
        "  {}\nIf it is about permissions: sudo sysctl kernel.perf_event_paranoid=-1",
        text.trim().replace('\n', "\n  ")
    )
}

impl Recording {
    fn command(&mut self, command: &str) -> Result<()> {
        writeln!(self.ctl, "{command}")?;
        let reply = wait(&mut self.perf, &self.fifos, &self.acks)?;
        // perf terminates each reply with a NUL byte.
        if reply.trim_matches(|c: char| c == '\0' || c.is_whitespace()) != "ack" {
            bail!("perf did not ack {command:?}: {reply:?}");
        }
        Ok(())
    }

    /// Stops sampling, then SIGINT makes perf finish writing its file.
    pub fn stop(mut self) -> Result<()> {
        self.command("disable")?;
        Command::new("kill")
            .args(["-INT", &self.perf.id().to_string()])
            .status()?;
        let status = self.perf.wait()?;
        // perf writes its file, then re-raises the SIGINT to exit by it.
        if !status.success() && status.signal() != Some(2) {
            bail!(
                "perf failed writing its file ({status}):\n{}",
                stderr(&self.fifos)
            );
        }
        std::fs::remove_dir_all(&self.fifos)?;
        Ok(())
    }
}
