use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::protocol::SignerIdentity;

const SSH_NAMESPACE: &str = "buddies";

#[derive(Debug, Clone)]
pub enum LocalSigner {
    Gpg {
        key_id: String,
    },
    Ssh {
        public_key: String,
        private_key_path: PathBuf,
    },
}

impl LocalSigner {
    pub fn identity(&self) -> SignerIdentity {
        match self {
            Self::Gpg { key_id } => SignerIdentity::Gpg {
                key_id: key_id.clone(),
            },
            Self::Ssh { public_key, .. } => SignerIdentity::Ssh {
                public_key: public_key.clone(),
            },
        }
    }

    pub fn sign(&self, payload: &[u8]) -> Result<Vec<u8>> {
        match self {
            Self::Gpg { key_id } => sign_with_gpg(payload, key_id),
            Self::Ssh {
                private_key_path, ..
            } => sign_with_ssh(payload, private_key_path),
        }
    }
}

pub fn discover_git_identity() -> Result<Option<LocalSigner>> {
    let signing_key = git_config("user.signingkey")?.map(|v| v.trim().to_string());
    let Some(signing_key) = signing_key else {
        return Ok(None);
    };

    let format = git_config("gpg.format")?
        .unwrap_or_else(|| "openpgp".to_string())
        .to_ascii_lowercase();

    if format == "ssh" {
        let (public_key, private_key_path) = resolve_ssh_keys(&signing_key)?;
        return Ok(Some(LocalSigner::Ssh {
            public_key,
            private_key_path,
        }));
    }

    Ok(Some(LocalSigner::Gpg {
        key_id: signing_key,
    }))
}

pub fn discover_startup_identity(data_dir: Option<&Path>) -> Result<Option<LocalSigner>> {
    let mode = std::env::var("BUDDIES_SIGNER")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase());

    match mode.as_deref() {
        None | Some("") | Some("git") => discover_git_identity(),
        Some("none") => Ok(None),
        Some("gpg") => discover_gpg_from_env().map(Some),
        Some("ssh") => discover_ssh_from_env().map(Some),
        Some("generated") | Some("ephemeral") => {
            let signer = generated_ssh_identity(data_dir)?;
            Ok(Some(signer))
        }
        Some(other) => anyhow::bail!(
            "unsupported BUDDIES_SIGNER value '{other}', expected git|none|gpg|ssh|generated"
        ),
    }
}

pub fn verify_signature(
    identity: &SignerIdentity,
    payload: &[u8],
    signature: &[u8],
) -> Result<bool> {
    match identity {
        SignerIdentity::Gpg { key_id } => verify_with_gpg(payload, signature, key_id),
        SignerIdentity::Ssh { public_key } => verify_with_ssh(payload, signature, public_key),
    }
}

/// Check that a gpg `--status-fd` output contains a `VALIDSIG` line whose
/// signing-key or primary-key fingerprint matches the claimed key id.
///
/// Without this binding, `gpg --verify` alone accepts a signature made by
/// *any* key in the local keyring, letting a peer claim a whitelisted
/// identity while signing with their own key. Key ids (short or long) are
/// trailing fragments of the fingerprint, so the claim must be a hex suffix
/// of one of the fingerprints. Non-hex claims (e.g. an email) cannot be
/// bound and are rejected.
fn gpg_status_matches_key(status_output: &str, claimed_key_id: &str) -> bool {
    let claim = claimed_key_id
        .trim()
        .trim_start_matches("0x")
        .to_ascii_uppercase();
    if claim.len() < 8 || !claim.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }

    for line in status_output.lines() {
        let Some(rest) = line.strip_prefix("[GNUPG:] VALIDSIG ") else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let Some(sig_fpr) = fields.first() else {
            continue;
        };
        // For subkey signatures the last field is the primary key fingerprint.
        let primary_fpr = fields.last().unwrap_or(sig_fpr);
        if sig_fpr.to_ascii_uppercase().ends_with(&claim)
            || primary_fpr.to_ascii_uppercase().ends_with(&claim)
        {
            return true;
        }
    }
    false
}

fn git_config(key: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["config", "--get", key])
        .output()
        .with_context(|| format!("failed to run git config for key {key}"))?;

    if !output.status.success() {
        return Ok(None);
    }

    let value = String::from_utf8(output.stdout).context("git config returned non-utf8 output")?;
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed))
}

fn discover_gpg_from_env() -> Result<LocalSigner> {
    let key_id = std::env::var("BUDDIES_GPG_KEY_ID")
        .ok()
        .or_else(|| std::env::var("BUDDIES_SIGNING_KEY").ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("BUDDIES_SIGNER=gpg requires BUDDIES_GPG_KEY_ID or BUDDIES_SIGNING_KEY")
        })?;

    Ok(LocalSigner::Gpg { key_id })
}

fn discover_ssh_from_env() -> Result<LocalSigner> {
    let private_key_path = std::env::var("BUDDIES_SSH_PRIVATE_KEY")
        .ok()
        .or_else(|| std::env::var("BUDDIES_SIGNING_KEY").ok())
        .map(PathBuf::from)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "BUDDIES_SIGNER=ssh requires BUDDIES_SSH_PRIVATE_KEY or BUDDIES_SIGNING_KEY"
            )
        })?;

    if !private_key_path.exists() {
        anyhow::bail!(
            "configured SSH private key not found: {}",
            private_key_path.display()
        );
    }

    let public_key = match std::env::var("BUDDIES_SSH_PUBLIC_KEY") {
        Ok(value) => resolve_public_key_value(&value)?,
        Err(_) => {
            let default_pub = PathBuf::from(format!("{}.pub", private_key_path.display()));
            if !default_pub.exists() {
                anyhow::bail!(
                    "BUDDIES_SSH_PUBLIC_KEY not set and inferred pubkey missing: {}",
                    default_pub.display()
                );
            }
            fs::read_to_string(default_pub)
                .context("failed to read inferred SSH public key")?
                .trim()
                .to_string()
        }
    };

    Ok(LocalSigner::Ssh {
        public_key,
        private_key_path,
    })
}

fn generated_ssh_identity(data_dir: Option<&Path>) -> Result<LocalSigner> {
    let base_dir = data_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::temp_dir().join("buddies"));
    fs::create_dir_all(&base_dir)
        .with_context(|| format!("failed to create identity directory {}", base_dir.display()))?;

    let private_key_path = base_dir.join("identity_ed25519");
    let public_key_path = base_dir.join("identity_ed25519.pub");

    if !private_key_path.exists() || !public_key_path.exists() {
        let output = Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "buddies-generated",
                "-f",
                path_str(&private_key_path)?,
            ])
            .output()
            .context("failed to invoke ssh-keygen for generated identity")?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to generate SSH identity: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    let public_key = fs::read_to_string(&public_key_path)
        .with_context(|| {
            format!(
                "failed to read generated SSH public key {}",
                public_key_path.display()
            )
        })?
        .trim()
        .to_string();

    Ok(LocalSigner::Ssh {
        public_key,
        private_key_path,
    })
}

fn resolve_public_key_value(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.starts_with("ssh-") {
        return Ok(trimmed.to_string());
    }
    let path = PathBuf::from(trimmed);
    if !path.exists() {
        anyhow::bail!("BUDDIES_SSH_PUBLIC_KEY must be an inline ssh key or an existing file path");
    }
    Ok(fs::read_to_string(path)
        .context("failed to read BUDDIES_SSH_PUBLIC_KEY file")?
        .trim()
        .to_string())
}

fn resolve_ssh_keys(signing_key: &str) -> Result<(String, PathBuf)> {
    if signing_key.starts_with("ssh-") {
        anyhow::bail!(
            "git user.signingkey contains an inline SSH public key; buddies needs a private key path"
        );
    }

    let path = PathBuf::from(signing_key);
    if !path.exists() {
        anyhow::bail!("ssh signing key path does not exist: {}", path.display());
    }

    let (pub_path, priv_path) = if path.extension().and_then(|s| s.to_str()) == Some("pub") {
        let candidate = PathBuf::from(signing_key.trim_end_matches(".pub"));
        if !candidate.exists() {
            anyhow::bail!(
                "ssh signing private key not found next to public key: {}",
                candidate.display()
            );
        }
        (path, candidate)
    } else {
        let candidate_pub = PathBuf::from(format!("{}.pub", path.display()));
        if !candidate_pub.exists() {
            anyhow::bail!(
                "ssh signing public key not found next to private key: {}",
                candidate_pub.display()
            );
        }
        (candidate_pub, path)
    };

    let public_key = fs::read_to_string(pub_path)
        .context("failed to read SSH public key")?
        .trim()
        .to_string();

    Ok((public_key, priv_path))
}

fn sign_with_gpg(payload: &[u8], key_id: &str) -> Result<Vec<u8>> {
    let temp = unique_temp_path("buddies-gpg-sign");
    let sig = unique_temp_path("buddies-gpg-sign.sig");

    fs::write(&temp, payload).context("failed to write temporary gpg payload")?;

    let output = Command::new("gpg")
        .args([
            "--batch",
            "--yes",
            "--local-user",
            key_id,
            "--detach-sign",
            "--output",
            path_str(&sig)?,
            path_str(&temp)?,
        ])
        .output()
        .context("failed to invoke gpg for signing")?;

    let _ = fs::remove_file(&temp);

    if !output.status.success() {
        let _ = fs::remove_file(&sig);
        anyhow::bail!(
            "gpg signing failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let signature = fs::read(&sig).context("failed to read gpg signature output")?;
    let _ = fs::remove_file(&sig);
    Ok(signature)
}

fn verify_with_gpg(payload: &[u8], signature: &[u8], key_id: &str) -> Result<bool> {
    let temp = unique_temp_path("buddies-gpg-verify");
    let sig = unique_temp_path("buddies-gpg-verify.sig");
    fs::write(&temp, payload).context("failed to write temporary gpg payload")?;
    fs::write(&sig, signature).context("failed to write temporary gpg signature")?;

    let output = Command::new("gpg")
        .args([
            "--batch",
            "--status-fd",
            "1",
            "--verify",
            path_str(&sig)?,
            path_str(&temp)?,
        ])
        .output()
        .context("failed to invoke gpg for verification")?;

    let _ = fs::remove_file(&temp);
    let _ = fs::remove_file(&sig);

    if !output.status.success() {
        return Ok(false);
    }
    let status_output = String::from_utf8_lossy(&output.stdout);
    Ok(gpg_status_matches_key(&status_output, key_id))
}

fn sign_with_ssh(payload: &[u8], private_key_path: &Path) -> Result<Vec<u8>> {
    let temp = unique_temp_path("buddies-ssh-sign");
    fs::write(&temp, payload).context("failed to write temporary ssh payload")?;

    let output = Command::new("ssh-keygen")
        .args([
            "-Y",
            "sign",
            "-f",
            path_str(private_key_path)?,
            "-n",
            SSH_NAMESPACE,
            path_str(&temp)?,
        ])
        .output()
        .context("failed to invoke ssh-keygen for signing")?;

    if !output.status.success() {
        let _ = fs::remove_file(&temp);
        anyhow::bail!(
            "ssh signing failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let sig_path = PathBuf::from(format!("{}.sig", temp.display()));
    let signature = fs::read(&sig_path).context("failed to read ssh signature output")?;

    let _ = fs::remove_file(&temp);
    let _ = fs::remove_file(&sig_path);
    Ok(signature)
}

fn verify_with_ssh(payload: &[u8], signature: &[u8], public_key: &str) -> Result<bool> {
    let sig = unique_temp_path("buddies-ssh-verify.sig");
    let allowed = unique_temp_path("buddies-ssh-allowed");
    fs::write(&sig, signature).context("failed to write temporary ssh signature")?;
    fs::write(&allowed, format!("buddies {public_key}\n"))
        .context("failed to write temporary allowed signers")?;

    let mut child = Command::new("ssh-keygen")
        .args([
            "-Y",
            "verify",
            "-f",
            path_str(&allowed)?,
            "-I",
            "buddies",
            "-n",
            SSH_NAMESPACE,
            "-s",
            path_str(&sig)?,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to invoke ssh-keygen for verification")?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(payload)
            .context("failed to stream payload to ssh-keygen verify")?;
    }

    let status = child
        .wait()
        .context("ssh-keygen verification process failed")?;

    let _ = fs::remove_file(&sig);
    let _ = fs::remove_file(&allowed);
    Ok(status.success())
}

fn unique_temp_path(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()))
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid utf-8: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::gpg_status_matches_key;

    const FPR: &str = "4AEA0EB10C1A0897E30AF98498DA02D0F4B25E51";
    const PRIMARY: &str = "AB12CD34EF56AB78CD90EF12AB34CD56EF78AB90";

    fn status(sig_fpr: &str, primary_fpr: &str) -> String {
        format!(
            "[GNUPG:] NEWSIG\n\
             [GNUPG:] GOODSIG 98DA02D0F4B25E51 Alice <alice@example.com>\n\
             [GNUPG:] VALIDSIG {sig_fpr} 2026-07-03 1751500000 0 4 0 1 8 00 {primary_fpr}\n\
             [GNUPG:] TRUST_ULTIMATE 0 pgp\n"
        )
    }

    #[test]
    fn accepts_full_fingerprint_and_key_id_suffixes() {
        let out = status(FPR, FPR);
        assert!(gpg_status_matches_key(&out, FPR));
        // long (16) and short (8) key ids are fingerprint suffixes
        assert!(gpg_status_matches_key(&out, "98DA02D0F4B25E51"));
        assert!(gpg_status_matches_key(&out, "F4B25E51"));
        // case-insensitive, optional 0x prefix
        assert!(gpg_status_matches_key(&out, "98da02d0f4b25e51"));
        assert!(gpg_status_matches_key(&out, "0x98DA02D0F4B25E51"));
    }

    #[test]
    fn accepts_primary_key_fingerprint_for_subkey_signatures() {
        let out = status(FPR, PRIMARY);
        assert!(gpg_status_matches_key(&out, PRIMARY));
        assert!(gpg_status_matches_key(&out, "AB34CD56EF78AB90"));
    }

    #[test]
    fn rejects_key_id_of_a_different_key() {
        let out = status(FPR, FPR);
        assert!(!gpg_status_matches_key(&out, PRIMARY));
        assert!(!gpg_status_matches_key(&out, "0000000000000000"));
    }

    #[test]
    fn rejects_when_validsig_line_is_missing_or_claim_is_not_hex() {
        assert!(!gpg_status_matches_key(
            "[GNUPG:] BADSIG 98DA02D0F4B25E51\n",
            FPR
        ));
        let out = status(FPR, FPR);
        // Non-hex claims (e.g. an email) cannot be bound to a fingerprint.
        assert!(!gpg_status_matches_key(&out, "alice@example.com"));
        assert!(!gpg_status_matches_key(&out, ""));
    }
}
