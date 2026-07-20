//! cryptsetup backend: open LUKS or VeraCrypt volumes (file or device).
//! We already shelled out to cryptsetup; this just adds LUKS alongside the
//! existing VeraCrypt path and a detector.

use std::io::{self, ErrorKind, Write};
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Luks,
    Veracrypt,
}

impl Backend {
    /// Parse an explicit `backend = ...` config/CLI value.
    pub fn parse(s: &str) -> Option<Backend> {
        match s {
            "luks" => Some(Backend::Luks),
            "veracrypt" => Some(Backend::Veracrypt),
            _ => None,
        }
    }
}

/// Detect the backend of `source`. LUKS has an unencrypted header that
/// `cryptsetup isLuks` recognises; a VeraCrypt header is encrypted and can't be
/// probed without the password, so anything that isn't LUKS is treated as VC.
pub fn detect(source: &Path) -> Backend {
    let is_luks = Command::new(crate::tool("cryptsetup"))
        .arg("isLuks")
        .arg(source)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if is_luks {
        Backend::Luks
    } else {
        Backend::Veracrypt
    }
}

/// argv for `cryptsetup open` of `source` as `dm_name` for `backend`.
pub fn open_args(source: &Path, backend: Backend, dm_name: &str) -> Vec<String> {
    let s = source.to_string_lossy().into_owned();
    match backend {
        Backend::Luks => vec!["open".into(), s, dm_name.into()],
        Backend::Veracrypt => vec![
            "--type".into(),
            "tcrypt".into(),
            "--veracrypt".into(),
            "open".into(),
            s,
            dm_name.into(),
        ],
    }
}

/// Run `cryptsetup open`.
///
/// With `passphrase = Some(bytes)` the passphrase is fed on cryptsetup's **stdin**
/// (NOT `--key-file=-`: for VeraCrypt/tcrypt `--key-file` is a *keyfile*, not the
/// passphrase, so that would break GUI unlock of a VeraCrypt vault). When stdin
/// isn't a tty cryptsetup reads the passphrase as a line. This works for both
/// LUKS and tcrypt and tolerates a trailing newline. With `None`, cryptsetup
/// prompts interactively on the tty, the terminal CLI path.
pub fn open(
    source: &Path,
    backend: Backend,
    dm_name: &str,
    passphrase: Option<&[u8]>,
) -> io::Result<()> {
    let args = open_args(source, backend, dm_name);
    let st = match passphrase {
        Some(pass) => {
            let mut child = Command::new(crate::tool("cryptsetup"))
                .args(&args)
                .stdin(Stdio::piped())
                .spawn()?;
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(pass)?; // drop -> EOF
            child.wait()?
        }
        None => Command::new(crate::tool("cryptsetup")).args(&args).status()?,
    };
    if !st.success() {
        return Err(io::Error::new(
            ErrorKind::Other,
            format!(
                "cryptsetup open failed (wrong passphrase?): rc={}",
                st.code().unwrap_or(-1)
            ),
        ));
    }
    Ok(())
}

/// Run `cryptsetup close` (idempotent at the call site).
pub fn close(dm_name: &str) -> io::Result<()> {
    let st = Command::new(crate::tool("cryptsetup")).args(["close", dm_name]).status()?;
    if !st.success() {
        return Err(io::Error::new(
            ErrorKind::Other,
            format!("cryptsetup close failed: rc={}", st.code().unwrap_or(-1)),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luks_open_args() {
        assert_eq!(
            open_args(Path::new("/dev/sdb1"), Backend::Luks, "veracage-aabbccddeeff"),
            vec!["open", "/dev/sdb1", "veracage-aabbccddeeff"]
        );
    }

    #[test]
    fn veracrypt_open_args() {
        assert_eq!(
            open_args(Path::new("/home/u/x.vc"), Backend::Veracrypt, "veracage-001122334455"),
            vec!["--type", "tcrypt", "--veracrypt", "open", "/home/u/x.vc", "veracage-001122334455"]
        );
    }

    #[test]
    fn backend_parse() {
        assert_eq!(Backend::parse("luks"), Some(Backend::Luks));
        assert_eq!(Backend::parse("veracrypt"), Some(Backend::Veracrypt));
        assert_eq!(Backend::parse("auto"), None);
        assert_eq!(Backend::parse(""), None);
    }
}
