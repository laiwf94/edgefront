//! Running a line-oriented driver program over a whole script.

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;

/// What a driver printed for one script.
#[derive(Debug)]
pub struct Transcript {
    /// The driver's exit status.
    pub status: ExitStatus,
    /// Everything it printed on stdout.
    pub stdout: String,
    /// Everything it printed on stderr.
    pub stderr: String,
}

impl Transcript {
    /// The lines printed on stdout.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.stdout.lines()
    }

    /// Whether the driver exited with status 0.
    pub fn success(&self) -> bool {
        self.status.success()
    }
}

/// Run `program` with `script` on its stdin and collect what it prints.
///
/// The script is written from a separate thread while the output is read,
/// so a driver that answers as it reads never blocks on a full pipe, however
/// long the script. A driver that exits early is not an I/O error: the
/// broken pipe is ignored and its exit status says what happened. Output
/// that is not UTF-8 is an [`io::ErrorKind::InvalidData`] error.
pub fn run(program: &Path, script: &str) -> io::Result<Transcript> {
    let mut child = Command::new(program)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input = script.as_bytes().to_vec();
    let writer = thread::spawn(move || match stdin.write_all(&input) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    });
    let output = child.wait_with_output()?;
    writer.join().expect("stdin writer panicked")?;
    Ok(Transcript {
        status: output.status,
        stdout: utf8(output.stdout, "stdout")?,
        stderr: utf8(output.stderr, "stderr")?,
    })
}

fn utf8(bytes: Vec<u8>, stream: &str) -> io::Result<String> {
    String::from_utf8(bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("driver {stream} is not UTF-8: {e}"),
        )
    })
}
