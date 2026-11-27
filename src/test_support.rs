use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

/// Writes an executable script at `path` from a child process. Written from
/// the test process instead, a process another test is starting could
/// inherit the open file until it execs, and running the script in that
/// window fails with "Text file busy".
pub(crate) fn write_script(path: &Path, script: &str) {
    let mut writer = Command::new("sh")
        .arg("-c")
        .arg(r#"cat > "$1" && chmod 755 "$1""#)
        .arg("sh")
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    writer
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    assert!(writer.wait().unwrap().success());
}
