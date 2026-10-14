//! Installation credentials for a client running in an agent sandbox.
//!
//! An installation that runs agents in sandboxes writes `/etc/lore/sandbox.toml`,
//! which names its Lore server, and `/run/lore/token`, which only the group the
//! `lore` binary is installed setgid to may read. Before it parses arguments or
//! reads any user configuration, the client reads the token, gives up that group
//! for good, makes itself undumpable and pins the token to the named server. The
//! agent running the client can use the token and cannot read it.

use std::fs::File;
use std::io::ErrorKind;
use std::io::Read;
use std::os::unix::fs::MetadataExt;

use serde::Deserialize;

const CONFIG_PATH: &str = "/etc/lore/sandbox.toml";
const TOKEN_PATH: &str = "/run/lore/token";

/// Environment variables a sandboxed client keeps. Lore reads `LORE_*` variables
/// that move its auth store, global state and service, and the libraries it links
/// read others, so every variable outside this list is removed.
const KEPT_VARIABLES: &[&str] = &[
    "COLORTERM",
    "COLUMNS",
    "EDITOR",
    "HOME",
    "LANG",
    "LANGUAGE",
    "LESS",
    "LINES",
    "LOGNAME",
    "NO_COLOR",
    "PAGER",
    "PATH",
    "PWD",
    "SHELL",
    "TERM",
    "TZ",
    "USER",
    "VISUAL",
];
const KEPT_PREFIXES: &[&str] = &["LC_", "XDG_"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxConfig {
    /// The Lore server the token is pinned to, as `lore://host:port`.
    remote: String,
}

/// Reads the installation's credentials when it configured a sandbox, and
/// returns whether it did.
///
/// Drops the setgid group whether or not a sandbox is configured. Must run
/// first in `main`, while the process has one thread, because it edits the
/// environment.
pub fn load() -> Result<bool, String> {
    let installation = read_installation_files();
    drop_group()?;
    let Some((config, token)) = installation? else {
        return Ok(false);
    };
    if File::open(TOKEN_PATH).is_ok() {
        return Err(format!(
            "{TOKEN_PATH} is readable without the lore binary's group; the agent could read the token"
        ));
    }
    set_undumpable()?;
    sanitize_environment();
    lore::pinned::pin(&config.remote, token)?;
    Ok(true)
}

fn read_installation_files() -> Result<Option<(SandboxConfig, String)>, String> {
    let config = match File::open(CONFIG_PATH) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("{CONFIG_PATH}: {err}")),
    };
    let config: SandboxConfig = toml::from_str(&read_owned_by_root(config, CONFIG_PATH, 0o022)?)
        .map_err(|err| format!("{CONFIG_PATH}: {err}"))?;
    let token = File::open(TOKEN_PATH).map_err(|err| format!("{TOKEN_PATH}: {err}"))?;
    let token = read_owned_by_root(token, TOKEN_PATH, 0o037)?
        .trim()
        .to_string();
    Ok(Some((config, token)))
}

/// Reads `file`, which must be a regular file owned by root that grants none of
/// the `forbidden` mode bits.
fn read_owned_by_root(mut file: File, path: &str, forbidden: u32) -> Result<String, String> {
    let metadata = file.metadata().map_err(|err| format!("{path}: {err}"))?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & forbidden != 0 {
        return Err(format!(
            "{path} must be a regular file owned by root with mode {:o} or stricter",
            0o666 & !forbidden
        ));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|err| format!("{path}: {err}"))?;
    Ok(text)
}

/// Sets the real, effective and saved group to the caller's real group, so
/// nothing later in the process can open files as the setgid group.
fn drop_group() -> Result<(), String> {
    // SAFETY: the calls take and return plain integers.
    let gid = unsafe { libc::getgid() };
    // SAFETY: as above.
    if unsafe { libc::setresgid(gid, gid, gid) } != 0 {
        return Err(format!(
            "dropping the lore binary's group: {}",
            std::io::Error::last_os_error()
        ));
    }
    let (mut real, mut effective, mut saved) = (0, 0, 0);
    // SAFETY: the pointers refer to live locals.
    let read = unsafe { libc::getresgid(&mut real, &mut effective, &mut saved) };
    if read != 0 || real != gid || effective != gid || saved != gid {
        return Err("the lore binary's group is still held".to_string());
    }
    Ok(())
}

/// Denies the agent's processes, which run as the same user, `ptrace` and
/// `/proc/<pid>/mem` access to the process holding the token.
fn set_undumpable() -> Result<(), String> {
    // SAFETY: PR_SET_DUMPABLE takes integer arguments only.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(format!(
            "making the process undumpable: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn sanitize_environment() {
    for (name, _) in std::env::vars_os() {
        let keep = name.to_str().is_some_and(|name| {
            KEPT_VARIABLES.contains(&name)
                || KEPT_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        });
        if !keep {
            // SAFETY: `load` runs first in `main`, before any other thread exists.
            unsafe { std::env::remove_var(&name) };
        }
    }
}
