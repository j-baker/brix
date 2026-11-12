//! SSH keys that sign Git commits (`git -c gpg.format=ssh commit -S`),
//! for tests of commit verification (`verifyCommit`, `publicKey`,
//! `publicKeys`).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::git::GitRepo;

/// A key pair `ssh-keygen` made, without a passphrase or comment.
pub struct SshKey {
    /// The private key's file, which signs.
    pub private: PathBuf,
    /// The public key's type as OpenSSH writes it (`ssh-ed25519`,
    /// `ssh-rsa`, `ecdsa-sha2-nistp256`), which is Nix's `keytype` for the
    /// first two.
    pub openssh_type: String,
    /// The public key's base64 blob, what Nix's `publicKey` takes.
    pub key: String,
}

impl SshKey {
    /// A new key of `ssh-keygen`'s type `kind` (`ed25519`, `rsa`, `ecdsa`)
    /// in `dir/name` and `dir/name.pub`. RSA keys have 2048 bits, which is
    /// quicker to make than the default.
    pub fn generate(dir: &Path, name: &str, kind: &str) -> SshKey {
        let private = dir.join(name);
        let mut cmd = Command::new("ssh-keygen");
        cmd.args(["-q", "-t", kind, "-N", "", "-C", ""]);
        if kind == "rsa" {
            cmd.args(["-b", "2048"]);
        }
        let out = cmd
            .arg("-f")
            .arg(&private)
            .output()
            .expect("the `ssh-keygen` command must be on PATH");
        assert!(
            out.status.success(),
            "ssh-keygen -t {kind} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let public_file = dir.join(format!("{name}.pub"));
        let public = std::fs::read_to_string(&public_file)
            .unwrap_or_else(|e| panic!("reading {}: {e}", public_file.display()));
        let mut fields = public.split_whitespace();
        let (Some(openssh_type), Some(key)) = (fields.next(), fields.next()) else {
            panic!(
                "{} is not an OpenSSH public key: {public:?}",
                public_file.display()
            );
        };
        SshKey {
            openssh_type: openssh_type.to_owned(),
            key: key.to_owned(),
            private,
        }
    }
}

impl GitRepo {
    /// Commits everything in the working tree with an SSH signature by
    /// `key`; the new revision. Signing runs `ssh-keygen`.
    pub fn commit_signed(&self, key: &SshKey) -> String {
        let signing_key = format!("user.signingKey={}", key.private.display());
        self.git(&["add", "-A"]);
        self.git(&[
            "-c",
            "gpg.format=ssh",
            "-c",
            &signing_key,
            "commit",
            "-q",
            "-S",
            "-m",
            "signed",
        ]);
        self.git(&["rev-parse", "HEAD"])
    }
}
