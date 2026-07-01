//! macOS keychain trust for the rtr CA via the `security` CLI.
//!
//! `codex` (and anything using rustls-platform-verifier / Security.framework)
//! checks the OS trust store, so this is the lever that makes interception work
//! for those tools. Login-domain trust needs no sudo; system-domain (`-d`) does.
//!
//! The argv builders are pure so the flag/keychain wiring is unit-tested without
//! mutating the real keychain; the `install`/`remove`/`is_trusted` wrappers shell
//! out and run only from the trust/untrust/status command paths.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Domain {
    Login,
    System,
}

impl Domain {
    pub fn label(self) -> &'static str {
        match self {
            Domain::Login => "login",
            Domain::System => "system",
        }
    }
}

const SYSTEM_KEYCHAIN: &str = "/Library/Keychains/System.keychain";

pub fn login_keychain(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Keychains")
        .join("login.keychain-db")
}

/// System-domain changes touch the admin trust store and require sudo.
pub fn needs_sudo(domain: Domain) -> bool {
    matches!(domain, Domain::System)
}

fn strs(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// `security` argv (no leading sudo) to add the CA as a trusted root.
pub fn add_trusted_argv(domain: Domain, login_keychain: &Path, cert: &Path) -> Vec<String> {
    let cert = cert.to_string_lossy();
    match domain {
        Domain::Login => {
            let kc = login_keychain.to_string_lossy();
            strs(&[
                "security",
                "add-trusted-cert",
                "-r",
                "trustRoot",
                "-k",
                &kc,
                &cert,
            ])
        }
        Domain::System => strs(&[
            "security",
            "add-trusted-cert",
            "-d",
            "-r",
            "trustRoot",
            "-k",
            SYSTEM_KEYCHAIN,
            &cert,
        ]),
    }
}

/// `security` argv (no leading sudo) to remove the CA's trust settings.
pub fn remove_trusted_argv(domain: Domain, cert: &Path) -> Vec<String> {
    let cert = cert.to_string_lossy();
    match domain {
        Domain::Login => strs(&["security", "remove-trusted-cert", &cert]),
        Domain::System => strs(&["security", "remove-trusted-cert", "-d", &cert]),
    }
}

/// `security verify-cert` succeeds only if the cert chains to a trusted root.
pub fn parse_verify_trusted(output: &str) -> bool {
    output.to_ascii_lowercase().contains("successful")
}

fn run(domain: Domain, argv: &[String]) -> Result<()> {
    let mut cmd = if needs_sudo(domain) {
        let mut c = Command::new("sudo");
        c.args(argv);
        c
    } else {
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        c
    };
    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("running `{}`: {e}", argv.join(" ")))?;
    if !status.success() {
        bail!("`{}` exited with {status}", argv.join(" "));
    }
    Ok(())
}

pub fn install(domain: Domain, login_keychain: &Path, cert: &Path) -> Result<()> {
    run(domain, &add_trusted_argv(domain, login_keychain, cert))
}

pub fn remove(domain: Domain, cert: &Path) -> Result<()> {
    run(domain, &remove_trusted_argv(domain, cert))
}

/// Whether the CA cert currently chains to a trusted root for this user.
pub fn is_trusted(cert: &Path) -> bool {
    match Command::new("security")
        .arg("verify-cert")
        .arg("-c")
        .arg(cert)
        .output()
    {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            parse_verify_trusted(&text)
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_trust_argv_has_no_admin_flag_and_targets_login_keychain() {
        let kc = PathBuf::from("/Users/x/Library/Keychains/login.keychain-db");
        let argv = add_trusted_argv(Domain::Login, &kc, Path::new("/c/ca.pem"));
        assert!(!argv.contains(&"-d".to_string()), "login must not use -d");
        assert!(argv.contains(&"trustRoot".to_string()));
        let k = argv.iter().position(|a| a == "-k").unwrap();
        assert_eq!(argv[k + 1], kc.to_string_lossy());
        assert_eq!(argv.last().unwrap(), "/c/ca.pem");
        assert!(!needs_sudo(Domain::Login));
    }

    #[test]
    fn system_trust_argv_uses_admin_flag_and_system_keychain_and_sudo() {
        let argv = add_trusted_argv(
            Domain::System,
            Path::new("/ignored"),
            Path::new("/c/ca.pem"),
        );
        assert!(argv.contains(&"-d".to_string()), "system must use -d");
        let k = argv.iter().position(|a| a == "-k").unwrap();
        assert_eq!(argv[k + 1], SYSTEM_KEYCHAIN);
        assert!(needs_sudo(Domain::System));
    }

    #[test]
    fn remove_argv_matches_domain() {
        assert!(
            !remove_trusted_argv(Domain::Login, Path::new("/c/ca.pem")).contains(&"-d".to_string())
        );
        assert!(
            remove_trusted_argv(Domain::System, Path::new("/c/ca.pem")).contains(&"-d".to_string())
        );
    }

    #[test]
    fn verify_output_parsing() {
        assert!(parse_verify_trusted(
            "...certificate verification successful."
        ));
        assert!(!parse_verify_trusted(
            "CSSMERR_TP_NOT_TRUSTED: the certificate was not trusted"
        ));
        assert!(!parse_verify_trusted(""));
    }

    #[test]
    fn login_keychain_path_derives_from_home() {
        assert_eq!(
            login_keychain(Path::new("/Users/x")),
            PathBuf::from("/Users/x/Library/Keychains/login.keychain-db")
        );
    }
}
