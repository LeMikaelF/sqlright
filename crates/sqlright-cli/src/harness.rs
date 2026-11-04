//! Run the harness binary on one test case, outside AFL++.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Run `harness --replay <case>` and return its output. A harness that
/// does not finish in `timeout` is killed; a crash or a kill adds an
/// `ERROR:` line so the oracle treats the open result block as an error.
pub fn run(harness: &Path, case_text: &str, timeout: Duration) -> Result<String> {
    let dir = std::env::temp_dir().join(format!("sqlright-replay-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let case_path = dir.join("case.sql");
    std::fs::write(&case_path, case_text)?;
    let mut child = Command::new(harness)
        .arg("--replay")
        .arg(&case_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot start the harness {}", harness.display()))?;
    // Read stdout on a thread so a large output cannot block the child.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        buf
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut output = reader.join().unwrap_or_default();
    match status {
        None => output.push_str("ERROR:  the harness was killed after the timeout\n"),
        Some(st) if !st.success() => output.push_str(&format!("ERROR:  the harness stopped with {st}\n")),
        Some(_) => {}
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(output)
}
