//! `idn` runs its commands: by subcommand, under an old name, and says which there are.

use std::process::Command;

fn idn() -> Command {
    Command::new(env!("CARGO_BIN_EXE_idn"))
}

fn text(c: &mut Command) -> (i32, String) {
    let o = c.output().unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
    )
}

#[test]
fn commands_and_old_names() {
    let (code, out) = text(idn().arg("help"));
    assert_eq!(code, 0);
    for c in [
        "send", "recv", "find", "mixer", "audio", "node", "switcher", "route",
    ] {
        assert!(out.contains(&format!("  {c} ")), "{c} missing:\n{out}");
    }
    // A command inside: its own usage and version, its arguments after the command.
    let (code, out) = text(idn().args(["find", "--help"]));
    assert_eq!(code, 0);
    assert!(out.contains("--seconds"), "{out}");
    let (_, out) = text(idn().args(["send", "--version"]));
    assert!(out.starts_with("idn send "), "{out}");
    let (code, out) = text(idn().args(["nosuch"]));
    assert_eq!(code, 2);
    assert!(out.contains("no command \"nosuch\""), "{out}");
    // Under an old name: that command.
    #[cfg(unix)]
    {
        let dir = idn_common::testing::TempDir::new("idn-cli");
        let link = dir.join("idn-doctor");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_idn"), &link).unwrap();
        let (code, out) = text(Command::new(&link).arg("--version"));
        assert_eq!(code, 0);
        assert!(out.starts_with("idn doctor "), "{out}");
    }
}
