use std::process::{Command, Stdio};

pub fn run_command_spawn(cmd: &str, args: &[&str]) -> std::io::Result<()> {
    Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?; // non-blocking spawn

    Ok(())
}

