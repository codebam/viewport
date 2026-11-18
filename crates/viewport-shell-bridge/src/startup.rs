// SPDX-License-Identifier: MIT
//
// The startup every shell repeats, before any engine is involved: the log
// subscriber, the random hex a token or a directory name is made of, the
// private directory it names, and the extra engine arguments the environment
// may carry. None of it knows what an engine is, so it lives here once rather
// than once per backend — where the private directory's copies had drifted.

use std::io::Read as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::PathBuf;

use anyhow::{anyhow, Context as _, Result};

/// Random bytes, as hex, from the kernel's own source.
///
/// Not a crate: this is the one random number a shell process needs, and the
/// kernel's own source of them is a file.
pub fn random_hex(bytes: usize) -> Result<String> {
    let mut bytes = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut bytes)
        .context("reading /dev/urandom")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// A fresh directory only this process can enter, under `/tmp`.
///
/// `DirBuilder::mode(0o700).create` is the whole point: it fails when the name
/// exists, so a directory or symlink somebody else made at that path cannot be
/// adopted. Retried because the suffix is random and a collision means
/// something already holds the name, not that the filesystem is broken.
pub fn private_dir(prefix: &str) -> Result<PathBuf> {
    for _ in 0..8 {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", random_hex(8)?));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }
    Err(anyhow!(
        "could not create a private {prefix} directory under {}",
        std::env::temp_dir().display()
    ))
}

/// The log subscriber every shell starts with: the environment's filter when
/// it set one, `info` otherwise.
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

/// The extra command-line arguments the environment variable `var` carries,
/// split on whitespace; empty when it is unset.
///
/// Each backend names its own (`VIEWPORT_CHROMIUM_ARGS`, `VIEWPORT_CEF_ARGS`,
/// `VIEWPORT_SERVOSHELL_ARGS`) — the same lever for the same purpose, one
/// spelling of the parsing.
pub fn extra_args_env(var: &str) -> Vec<String> {
    std::env::var(var)
        .into_iter()
        .flat_map(|s| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_hex_is_the_length_and_charset_it_says() {
        for bytes in [8, 16] {
            let hex = random_hex(bytes).expect("urandom");
            assert_eq!(hex.len(), bytes * 2);
            assert!(
                hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{hex}"
            );
        }
    }

    #[test]
    fn two_random_hexes_are_not_the_same() {
        let first = random_hex(16).expect("urandom");
        let second = random_hex(16).expect("urandom");
        assert_ne!(first, second);
    }

    #[test]
    fn a_private_dir_is_0700_unique_and_a_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let first = private_dir("viewport-bridge-test").expect("a private dir");
        let second = private_dir("viewport-bridge-test").expect("another");
        assert_ne!(first, second);
        for dir in [&first, &second] {
            assert!(dir.is_dir(), "{dir:?}");
            assert!(dir.starts_with(std::env::temp_dir()), "{dir:?}");
            let mode = std::fs::metadata(dir)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "{dir:?}");
        }
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    #[test]
    fn extra_args_split_on_whitespace_and_unset_is_empty() {
        // A name no other test or real session uses, so setting it here
        // pollutes nothing.
        let var = "VIEWPORT_BRIDGE_TEST_EXTRA_ARGS_9d41";
        std::env::remove_var(var);
        assert!(extra_args_env(var).is_empty());

        std::env::set_var(var, "");
        assert!(extra_args_env(var).is_empty());

        std::env::set_var(var, "  --one\ttwo  --three=four \n");
        assert_eq!(extra_args_env(var), ["--one", "two", "--three=four"]);
        std::env::remove_var(var);
    }
}
