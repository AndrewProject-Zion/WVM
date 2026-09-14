//! Append-only journal.
//!
//! Every request, every policy decision, and every result is recorded — **including denials**.
//! A denial is frequently the most interesting record in the file: it is evidence that the
//! capability boundary did its job, and it is the thing an operator needs when an agent behaves
//! unexpectedly.
//!
//! The format is JSON Lines: one self-contained object per line, so a truncated final line is
//! recoverable and the file can be tailed with standard tools.

#![allow(dead_code)] // `append` is wired in when the request path lands (M2).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One journal record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Record {
    /// Unix epoch milliseconds. Wall-clock, because a human reads this file.
    pub at_ms: u64,
    /// What happened, as a closed set so downstream tooling can match on it.
    pub event: Event,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// The daemon started.
    Started { version: String },
    /// A request was received.
    Request {
        subject: String,
        verb: String,
        op: String,
    },
    /// A request was refused before dispatch. Carries the reason.
    Denied {
        subject: String,
        verb: String,
        reason: String,
    },
    /// A request completed.
    Completed {
        subject: String,
        verb: String,
        ok: bool,
        detail: String,
    },
    /// A VM state transition.
    VmState {
        vm: String,
        from: String,
        to: String,
    },
}

impl Record {
    pub fn new(event: Event) -> Self {
        Record {
            at_ms: now_ms(),
            event,
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Handle to the journal file.
pub struct Journal {
    path: PathBuf,
    file: File,
}

impl Journal {
    /// Default location: `$XDG_STATE_HOME/wvm/journal.jsonl`, falling back to
    /// `~/.local/state/wvm/journal.jsonl`.
    pub fn open_default() -> Result<Self> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .context("neither XDG_STATE_HOME nor HOME is set; cannot locate the journal")?;

        Self::open(base.join("wvm").join("journal.jsonl"))
    }

    /// Open (creating parent directories) the journal at an explicit path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating journal directory {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening journal {}", path.display()))?;
        Ok(Journal { path, file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a record. Flushed immediately: a journal that loses its last entry on a crash is
    /// not an audit log.
    pub fn append(&mut self, record: &Record) -> Result<()> {
        let line = serde_json::to_string(record)?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        Ok(())
    }

    /// Read the last `limit` records, oldest first.
    ///
    /// A malformed trailing line (a partial write from an interrupted process) is skipped rather
    /// than failing the whole read — the earlier records are still valid evidence.
    pub fn tail(&self, limit: usize) -> Result<Vec<Record>> {
        let file = File::open(&self.path)
            .with_context(|| format!("reading journal {}", self.path.display()))?;
        let reader = BufReader::new(file);

        let mut records: Vec<Record> = Vec::new();
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(record) = serde_json::from_str::<Record>(&line) {
                records.push(record);
            }
        }

        if records.len() > limit {
            records.drain(..records.len() - limit);
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// A unique scratch path per test, so tests never collide.
    fn tmp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "wvm-journal-test-{}-{}.jsonl",
            tag,
            std::process::id()
        ));
        p
    }

    #[test]
    fn append_then_tail_round_trips() {
        let path = tmp_path("roundtrip");
        let _ = std::fs::remove_file(&path);

        let mut j = Journal::open(&path).unwrap();
        j.append(&Record::new(Event::Started {
            version: "0.1.0".into(),
        }))
        .unwrap();
        j.append(&Record::new(Event::Request {
            subject: "agent".into(),
            verb: "exec".into(),
            op: "exec".into(),
        }))
        .unwrap();

        let got = j.tail(10).unwrap();
        assert_eq!(got.len(), 2);
        assert!(matches!(got[0].event, Event::Started { .. }));
        assert!(matches!(got[1].event, Event::Request { .. }));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn denials_are_recorded() {
        // The whole point of the journal: a refusal must be as visible as a success.
        let path = tmp_path("denial");
        let _ = std::fs::remove_file(&path);

        let mut j = Journal::open(&path).unwrap();
        j.append(&Record::new(Event::Denied {
            subject: "nobody".into(),
            verb: "exec".into(),
            reason: "verb_not_granted".into(),
        }))
        .unwrap();

        let got = j.tail(10).unwrap();
        assert_eq!(got.len(), 1);
        match &got[0].event {
            Event::Denied {
                subject, reason, ..
            } => {
                assert_eq!(subject, "nobody");
                assert_eq!(reason, "verb_not_granted");
            }
            other => panic!("expected Denied, got {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tail_respects_limit_and_keeps_newest() {
        let path = tmp_path("limit");
        let _ = std::fs::remove_file(&path);

        let mut j = Journal::open(&path).unwrap();
        for i in 0..10 {
            j.append(&Record::new(Event::VmState {
                vm: format!("vm{i}"),
                from: "off".into(),
                to: "on".into(),
            }))
            .unwrap();
        }

        let got = j.tail(3).unwrap();
        assert_eq!(got.len(), 3);
        // Newest three: vm7, vm8, vm9 — oldest first.
        for (record, expected) in got.iter().zip(["vm7", "vm8", "vm9"]) {
            match &record.event {
                Event::VmState { vm, .. } => assert_eq!(vm, expected),
                other => panic!("expected VmState, got {other:?}"),
            }
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_trailing_line_is_skipped() {
        let path = tmp_path("malformed");
        let _ = std::fs::remove_file(&path);

        let mut j = Journal::open(&path).unwrap();
        j.append(&Record::new(Event::Started {
            version: "0.1.0".into(),
        }))
        .unwrap();
        drop(j);

        // Simulate an interrupted write.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"at_ms\":123,\"event\":{\"kind\":").unwrap();
        drop(f);

        let j = Journal::open(&path).unwrap();
        let got = j.tail(10).unwrap();
        assert_eq!(got.len(), 1, "the valid record must survive a torn write");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn records_are_one_line_each() {
        let path = tmp_path("oneline");
        let _ = std::fs::remove_file(&path);

        let mut j = Journal::open(&path).unwrap();
        j.append(&Record::new(Event::Completed {
            subject: "agent".into(),
            verb: "capture".into(),
            ok: true,
            detail: "png 1920x1080".into(),
        }))
        .unwrap();
        drop(j);

        let mut contents = String::new();
        File::open(&path)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert_eq!(contents.lines().count(), 1);

        let _ = std::fs::remove_file(&path);
    }
}
