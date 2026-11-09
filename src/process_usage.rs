use eframe::egui;
use std::{
    process::Command,
    sync::mpsc::{self, Receiver, TryRecvError},
    time::{Duration, Instant},
};

const INTERVAL: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(super) struct ProcessUsage {
    last_poll: Option<Instant>,
    pending: Option<Receiver<Option<Sample>>>,
    sample: Option<Sample>,
}

struct Sample {
    cpu_percent: f32,
    rss_kib: u64,
}

impl ProcessUsage {
    pub(super) fn poll(&mut self, ctx: &egui::Context) {
        if let Some(pending) = &self.pending {
            match pending.try_recv() {
                Ok(sample) => self.sample = sample,
                Err(TryRecvError::Disconnected) => self.sample = None,
                Err(TryRecvError::Empty) => return,
            }
            self.pending = None;
        }
        if self.last_poll.is_some_and(|last| last.elapsed() < INTERVAL) {
            return;
        }

        self.last_poll = Some(Instant::now());
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        self.pending = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(sample());
            ctx.request_repaint();
        });
    }

    pub(super) fn label(&self) -> String {
        self.sample.as_ref().map_or_else(
            || "App CPU —  ·  RAM —".into(),
            |sample| {
                format!(
                    "App CPU {:.1}%  ·  RAM {:.1} MiB",
                    sample.cpu_percent,
                    sample.rss_kib as f64 / 1024.0
                )
            },
        )
    }
}

fn sample() -> Option<Sample> {
    let output = Command::new("/bin/ps")
        .env("LC_ALL", "C")
        .args([
            "-p",
            &std::process::id().to_string(),
            "-o",
            "%cpu=",
            "-o",
            "rss=",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut fields = std::str::from_utf8(&output.stdout).ok()?.split_whitespace();
    Some(Sample {
        cpu_percent: fields.next()?.parse().ok()?,
        rss_kib: fields.next()?.parse().ok()?,
    })
}
