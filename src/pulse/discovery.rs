//! Where the sound server listens and which cookie to show it, in libpulse's order.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::{Error, ErrorKind};

/// The cookie's length in the protocol.
pub const COOKIE_LENGTH: usize = 256;

/// Reads the process's environment.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// The sockets to try, in order. `$PULSE_SERVER` alone counts when it's set, as for libpulse.
/// Its entries are separated by spaces, may start with `{machine id}`, and are `unix:PATH`,
/// a bare absolute path, or a network address, which isn't supported.
pub fn candidates(env: Env<'_>, uid: Option<u32>) -> Result<Vec<PathBuf>, Error> {
    if let Some(servers) = env("PULSE_SERVER").filter(|value| !value.is_empty()) {
        let servers = servers.to_string_lossy().into_owned();
        let paths: Vec<PathBuf> = servers
            .split_whitespace()
            .map(|entry| match entry.strip_prefix('{') {
                Some(rest) => rest.split_once('}').map_or("", |(_, address)| address),
                None => entry,
            })
            .filter_map(|address| {
                let path = address.strip_prefix("unix:").unwrap_or(address);
                path.starts_with('/').then(|| PathBuf::from(path))
            })
            .collect();
        if paths.is_empty() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!(
                    "PULSE_SERVER names only network servers ({servers}), which aren't supported"
                ),
            ));
        }
        return Ok(paths);
    }
    let mut paths = Vec::new();
    if let Some(dir) = env("PULSE_RUNTIME_PATH").filter(|value| !value.is_empty()) {
        paths.push(Path::new(&dir).join("native"));
    }
    if let Some(dir) = env("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        paths.push(Path::new(&dir).join("pulse/native"));
    }
    if let Some(uid) = uid {
        paths.push(PathBuf::from(format!("/run/user/{uid}/pulse/native")));
    }
    paths.push(PathBuf::from("/run/pulse/native"));
    paths.dedup();
    Ok(paths)
}

/// Why no server answered. PipeWire without pipewire-pulse gets a message of its own, since
/// taking the device from under PipeWire would break every other program's audio.
pub fn no_server(env: Env<'_>, uid: Option<u32>) -> Error {
    if let Some(servers) = env("PULSE_SERVER").filter(|value| !value.is_empty()) {
        return Error::new(
            ErrorKind::NoServer,
            format!(
                "no sound server answers at PULSE_SERVER ({})",
                servers.to_string_lossy()
            ),
        );
    }
    let mut dirs: Vec<PathBuf> = ["PIPEWIRE_RUNTIME_DIR", "XDG_RUNTIME_DIR"]
        .into_iter()
        .filter_map(|name| {
            env(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .collect();
    dirs.extend(uid.map(|uid| PathBuf::from(format!("/run/user/{uid}"))));
    let pipewire = env("PIPEWIRE_REMOTE")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "pipewire-0".into());
    if dirs.iter().any(|dir| dir.join(&pipewire).exists()) {
        Error::new(
            ErrorKind::NoServer,
            "PipeWire is running without its PulseAudio service, so install pipewire-pulse to record",
        )
    } else {
        Error::new(ErrorKind::NoServer, "no sound server is running")
    }
}

/// The first cookie file that holds a whole cookie, else zeros. pipewire-pulse checks only
/// the length, and PulseAudio lets a client of its own user in by the socket's credentials.
pub fn cookie(env: Env<'_>) -> Vec<u8> {
    let home = env("HOME").map(PathBuf::from);
    let config = env("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".config")));
    let paths = [
        env("PULSE_COOKIE").map(PathBuf::from),
        config.map(|dir| dir.join("pulse/cookie")),
        home.map(|home| home.join(".pulse-cookie")),
    ];
    paths
        .into_iter()
        .flatten()
        .filter_map(|path| std::fs::read(path).ok())
        .find(|bytes| bytes.len() >= COOKIE_LENGTH)
        .map(|mut bytes| {
            bytes.truncate(COOKIE_LENGTH);
            bytes
        })
        .unwrap_or_else(|| vec![0; COOKIE_LENGTH])
}

/// This process's user id, from who owns its `/proc` entry.
pub fn uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|meta| meta.uid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn the_runtime_dirs_come_in_libpulses_order() {
        let env = env_of(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("PULSE_RUNTIME_PATH", "/tmp/p"),
        ]);
        assert_eq!(
            candidates(&env, Some(1000)).unwrap(),
            [
                PathBuf::from("/tmp/p/native"),
                "/run/user/1000/pulse/native".into(),
                "/run/pulse/native".into()
            ]
        );
    }

    #[test]
    fn pulse_server_alone_counts_and_network_entries_are_skipped() {
        let env = env_of(&[
            (
                "PULSE_SERVER",
                "tcp:host:4713 {abc}unix:/run/flatpak/pulse/native /tmp/other",
            ),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ]);
        assert_eq!(
            candidates(&env, Some(1000)).unwrap(),
            [
                PathBuf::from("/run/flatpak/pulse/native"),
                "/tmp/other".into()
            ]
        );
        let env = env_of(&[("PULSE_SERVER", "tcp:host:4713")]);
        assert_eq!(
            candidates(&env, None).unwrap_err().kind(),
            ErrorKind::Unsupported
        );
    }

    #[test]
    fn pipewire_without_its_pulse_service_says_so() {
        let dir = std::env::temp_dir().join(format!("capta-pw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env = env_of(&[("XDG_RUNTIME_DIR", dir.to_str().unwrap())]);
        assert_eq!(
            no_server(&env, None).to_string(),
            "no sound server is running"
        );
        std::fs::write(dir.join("pipewire-0"), b"").unwrap();
        assert!(no_server(&env, None).to_string().contains("pipewire-pulse"));
        let env = env_of(&[
            ("XDG_RUNTIME_DIR", dir.to_str().unwrap()),
            ("PULSE_SERVER", "/tmp/x"),
        ]);
        assert!(
            no_server(&env, None)
                .to_string()
                .contains("PULSE_SERVER (/tmp/x)")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_cookie_comes_from_the_first_whole_file_or_is_zeros() {
        let dir = std::env::temp_dir().join(format!("capta-cookie-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("config/pulse")).unwrap();
        let short = dir.join("short");
        std::fs::write(&short, [1; 10]).unwrap();
        let env = env_of(&[
            ("PULSE_COOKIE", short.to_str().unwrap()),
            ("XDG_CONFIG_HOME", dir.join("config").to_str().unwrap()),
            ("HOME", dir.to_str().unwrap()),
        ]);
        assert_eq!(cookie(&env), vec![0; COOKIE_LENGTH]);
        std::fs::write(dir.join("config/pulse/cookie"), [7; 300]).unwrap();
        assert_eq!(cookie(&env), vec![7; COOKIE_LENGTH]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
