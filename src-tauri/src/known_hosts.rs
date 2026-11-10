//! Host keys and certificate authorities from the user's OpenSSH known_hosts
//! files (#2): the ones `UserKnownHostsFile` in `~/.ssh/config` names, else
//! `~/.ssh/known_hosts` and `~/.ssh/known_hosts2`. Read when a connection
//! starts, only while "Use settings from ~/.ssh/config" is on; never written.
//!
//! As OpenSSH:
//!   - A host key listed for the host is trusted without the first-time
//!     prompt. Entries are looked up as `host`, or `[host]:port` when the port
//!     isn't 22; hashed entries (`|1|salt|hash`, Ubuntu's default) count.
//!   - A key that isn't listed, for a host with another key of the same kind
//!     listed, gets the "changed" warning, not the plain prompt (OpenSSH
//!     refuses it). With only other kinds listed it's a new key to the file: a
//!     server has one of each kind, so the first-time prompt, and a log line.
//!   - A host CERTIFICATE is trusted when an `@cert-authority` line for the
//!     host names the CA that signed it, it's a host certificate, it's within
//!     its validity period, its signature checks out, and it's issued for the
//!     name connected to (or for any host). CA lines are matched against the
//!     bare host name as well as `[host]:port`, which is what Teleport's
//!     `*.cluster` lines on port 3022 rely on.
//!   - A key listed `@revoked` for the host is refused, as the host's key or
//!     as the CA of its certificate.

use russh::keys::ssh_key::certificate::CertType;
use russh::keys::ssh_key::HashAlg;
use russh::keys::{Certificate, PublicKey};

/// The largest known_hosts file read.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// What the known_hosts files say about one host.
#[derive(Default)]
pub(crate) struct HostTrust {
    /// The name certificates must be issued for: the host connected to.
    name: String,
    /// CA keys trusted for this host, with the file that says so.
    cas: Vec<(PublicKey, String)>,
    /// Host keys listed for this host, with the file.
    keys: Vec<(PublicKey, String)>,
    revoked: Vec<PublicKey>,
}

/// What to do with the key a server offered.
#[derive(Debug, PartialEq)]
pub(crate) enum Verdict {
    /// Trusted; the reason goes to the connect log.
    Trusted(String),
    /// Refused; the reason goes to the connect log.
    Refused(String),
    /// The files list another key of this kind for this host, and not this
    /// one. What the app has on file still decides whether it's trusted, but
    /// it's not a first-time host: the "changed" warning, with `listed` as what
    /// was trusted before. The note goes to the log.
    Changed { note: String, listed: Vec<String> },
    /// The files say nothing that decides it: the usual first-time / known
    /// check runs. A note (keys of other kinds listed) goes to the log.
    Undecided(Option<String>),
}

impl HostTrust {
    /// Whether any CA is trusted for this host (then certificates are asked for).
    pub(crate) fn has_cas(&self) -> bool {
        !self.cas.is_empty()
    }

    /// Judge a plain host key.
    pub(crate) fn check_key(&self, key: &PublicKey) -> Verdict {
        if self.is_revoked(key) {
            return Verdict::Refused("this host key is marked @revoked in your known_hosts".into());
        }
        if let Some((_, file)) = self.keys.iter().find(|(k, _)| k.key_data() == key.key_data()) {
            return Verdict::Trusted(format!("Host key found in {file}. Verified."));
        }
        // "Changed" takes a key of the same kind. A server has one of each kind
        // and shows the one asked for, so a line of another kind (an old ECDSA
        // line, usually) says nothing about this one.
        let kind = |k: &PublicKey| crate::ssh_manager::key_type_family(k.algorithm().as_str());
        let Some((_, file)) = self.keys.iter().find(|(k, _)| kind(k) == kind(key)) else {
            return Verdict::Undecided(
                self.keys.first().map(|(_, file)| format!("{file} lists only other kinds of key for this host, so this one is new to it.")),
            );
        };
        let mut listed = Vec::new();
        for (k, _) in &self.keys {
            let line = format!("{} ({})", crate::ssh_manager::host_key_fingerprint(k), k.algorithm());
            if !listed.contains(&line) {
                listed.push(line);
            }
        }
        Verdict::Changed {
            note: format!("{file} lists a different key for this host; it may have changed, or this isn't the same server."),
            listed,
        }
    }

    /// Judge a host certificate at `now` (seconds since the epoch).
    pub(crate) fn check_certificate(&self, cert: &Certificate, now: u64) -> Verdict {
        if self.cas.is_empty() {
            return Verdict::Refused(
                "no @cert-authority line in your known_hosts covers this host, so its certificate can't be checked".into(),
            );
        }
        if cert.cert_type() != CertType::Host {
            return Verdict::Refused("the server offered a user certificate as its host key".into());
        }
        let ca = cert.signature_key();
        let Some((ca_key, file)) = self.cas.iter().find(|(k, _)| k.key_data() == ca) else {
            return Verdict::Refused("its certificate is signed by a CA your known_hosts doesn't trust for this host".into());
        };
        let cert_key = PublicKey::from(cert.public_key().clone());
        if self.is_revoked(ca_key) || self.is_revoked(&cert_key) {
            return Verdict::Refused("its certificate (or the CA that signed it) is marked @revoked in your known_hosts".into());
        }
        let fingerprint = ca_key.fingerprint(HashAlg::Sha256);
        if let Err(e) = cert.validate_at(now, [&fingerprint]) {
            return Verdict::Refused(format!("its certificate isn't valid ({e})"));
        }
        let principals = cert.valid_principals();
        if !principals.is_empty() && !principals.iter().any(|p| p.eq_ignore_ascii_case(&self.name)) {
            return Verdict::Refused(format!(
                "its certificate isn't issued for {} (it names {})",
                self.name,
                principals.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        Verdict::Trusted(format!("Host certificate signed by a CA trusted in {file}. Verified."))
    }

    fn is_revoked(&self, key: &PublicKey) -> bool {
        self.revoked.iter().any(|k| k.key_data() == key.key_data())
    }
}

/// What the known_hosts files `files` (paths already expanded; `None` for the
/// defaults) say about `host:port`. `None` when the switch is off or no file
/// is readable.
pub(crate) async fn for_host(files: Option<Vec<String>>, host: &str, port: u16) -> Option<HostTrust> {
    if !crate::ssh_config::enabled() {
        return None;
    }
    let files = match files {
        Some(f) => f,
        None => {
            let home = crate::ssh_config::home_dir()?;
            let ssh = std::path::Path::new(&home).join(".ssh");
            vec![
                ssh.join("known_hosts").to_string_lossy().into_owned(),
                ssh.join("known_hosts2").to_string_lossy().into_owned(),
            ]
        }
    };
    let host = host.to_string();
    tokio::task::spawn_blocking(move || {
        let mut trust = HostTrust { name: host.to_ascii_lowercase(), ..Default::default() };
        let mut any = false;
        for file in &files {
            if let Some(text) = read_file(file) {
                any = true;
                parse_into(&mut trust, &text, &display_path(file), &host, port);
            }
        }
        any.then_some(trust)
    })
    .await
    .ok()?
}

fn read_file(path: &str) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// A path for the log, with the home folder shown as `~`.
fn display_path(path: &str) -> String {
    match crate::ssh_config::home_dir() {
        Some(home) if !home.is_empty() && path.starts_with(&home) => format!("~{}", &path[home.len()..]).replace('\\', "/"),
        _ => path.to_string(),
    }
}

/// Add what `text` says about `host:port` to `trust`.
fn parse_into(trust: &mut HostTrust, text: &str, label: &str, host: &str, port: u16) {
    let host = host.to_ascii_lowercase();
    // Plain entries are per port; CA lines also match the bare name.
    let with_port = if port == 22 { host.clone() } else { format!("[{host}]:{port}") };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some(first) = fields.next() else { continue };
        let (marker, hosts) = if first.starts_with('@') { (Some(first), fields.next()) } else { (None, Some(first)) };
        let (Some(hosts), Some(key_type), Some(b64)) = (hosts, fields.next(), fields.next()) else { continue };
        let Ok(key) = PublicKey::from_openssh(&format!("{key_type} {b64}")) else { continue };
        match marker {
            Some("@cert-authority") => {
                if hosts_match(hosts, &with_port) || hosts_match(hosts, &host) {
                    trust.cas.push((key, label.to_string()));
                }
            }
            Some("@revoked") => {
                if hosts_match(hosts, &with_port) || hosts_match(hosts, &host) || hosts == "*" {
                    trust.revoked.push(key);
                }
            }
            Some(_) => {}
            None => {
                if hosts_match(hosts, &with_port) {
                    trust.keys.push((key, label.to_string()));
                }
            }
        }
    }
}

/// A known_hosts host field: a hashed entry, or comma-separated patterns where
/// a `!` pattern that matches rules the name out.
fn hosts_match(field: &str, name: &str) -> bool {
    if let Some(hashed) = field.strip_prefix("|1|") {
        return hashed_matches(hashed, name);
    }
    let mut matched = false;
    for pattern in field.split(',') {
        if let Some(negated) = pattern.strip_prefix('!') {
            if crate::ssh_config::pattern_matches(negated, name) {
                return false;
            }
        } else if crate::ssh_config::pattern_matches(pattern, name) {
            matched = true;
        }
    }
    matched
}

/// `|1|salt|hash`: HMAC-SHA1 of the name keyed with the salt (both base64).
fn hashed_matches(hashed: &str, name: &str) -> bool {
    use base64::Engine;
    use hmac::{Hmac, KeyInit, Mac};
    let Some((salt, hash)) = hashed.split_once('|') else { return false };
    let b64 = base64::engine::general_purpose::STANDARD;
    let (Ok(salt), Ok(hash)) = (b64.decode(salt), b64.decode(hash)) else { return false };
    let Ok(mut mac) = <Hmac<sha1::Sha1> as KeyInit>::new_from_slice(&salt) else { return false };
    mac.update(name.as_bytes());
    mac.verify_slice(&hash).is_ok()
}

/// A key as it'd be written in known_hosts (`type base64`), for tests.
#[cfg(test)]
fn line_key(key: &PublicKey) -> String {
    use russh::keys::PublicKeyBase64;
    format!("{} {}", key.algorithm(), key.public_key_base64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::ssh_key::certificate::Builder;
    use russh::keys::{Algorithm, EcdsaCurve, PrivateKey};

    fn key() -> PrivateKey {
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap()
    }

    fn trust(text: &str, host: &str, port: u16) -> HostTrust {
        let mut t = HostTrust { name: host.to_ascii_lowercase(), ..Default::default() };
        parse_into(&mut t, text, "~/.ssh/known_hosts", host, port);
        t
    }

    fn host_cert(ca: &PrivateKey, host_key: &PrivateKey, principals: &[&str], valid: (u64, u64), cert_type: CertType) -> Certificate {
        let mut b = Builder::new_with_random_nonce(&mut rand::rng(), host_key.public_key(), valid.0, valid.1).unwrap();
        b.cert_type(cert_type).unwrap();
        for p in principals {
            b.valid_principal(*p).unwrap();
        }
        if principals.is_empty() {
            b.all_principals_valid().unwrap();
        }
        b.sign(ca).unwrap()
    }

    const NOW: u64 = 1_800_000_000;
    const VALID: (u64, u64) = (NOW - 3600, NOW + 3600);

    #[test]
    fn a_listed_host_key_is_trusted_by_port() {
        let k = key();
        let pk = k.public_key().clone();
        let line = line_key(&pk);
        assert!(matches!(trust(&format!("db.example.com,10.0.0.5 {line}\n"), "db.example.com", 22).check_key(&pk), Verdict::Trusted(_)));
        assert!(matches!(trust(&format!("[db.example.com]:2222 {line}\n"), "db.example.com", 2222).check_key(&pk), Verdict::Trusted(_)));
        // The port-22 entry doesn't vouch for port 2222.
        assert_eq!(trust(&format!("db.example.com {line}\n"), "db.example.com", 2222).check_key(&pk), Verdict::Undecided(None));
        // Another key of this kind listed: not a refusal, but not a first-time
        // host either, and the prompt says what's listed. The same key twice
        // shows once.
        let other = key();
        let other_line = line_key(other.public_key());
        let shown = format!("{} ({})", crate::ssh_manager::host_key_fingerprint(other.public_key()), other.public_key().algorithm());
        let text = format!("db.example.com {other_line}\ndb.example.com {other_line}\n");
        assert!(matches!(trust(&text, "db.example.com", 22).check_key(&pk), Verdict::Changed { listed, .. } if listed == [shown.clone()]));
        assert!(matches!(trust(&format!("*.example.com,!bad.example.com {line}\n"), "x.example.com", 22).check_key(&pk), Verdict::Trusted(_)));
        assert_eq!(trust(&format!("*.example.com,!bad.example.com {line}\n"), "bad.example.com", 22).check_key(&pk), Verdict::Undecided(None));
    }

    #[test]
    fn only_a_key_of_the_same_kind_makes_it_a_change() {
        let ed25519 = key();
        let other_ed25519 = key();
        let ecdsa = PrivateKey::random(&mut rand::rng(), Algorithm::Ecdsa { curve: EcdsaCurve::NistP256 }).unwrap();
        let p384 = PrivateKey::random(&mut rand::rng(), Algorithm::Ecdsa { curve: EcdsaCurve::NistP384 }).unwrap();
        let listed = |keys: &[&PrivateKey]| keys.iter().map(|k| format!("db.example.com {}\n", line_key(k.public_key()))).collect::<String>();
        let judged = |text: String, offered: &PrivateKey| trust(&text, "db.example.com", 22).check_key(offered.public_key());

        // The usual old file: an ECDSA line, and a server that shows Ed25519.
        // No change, so the first-time prompt, and a note in the log.
        assert!(matches!(judged(listed(&[&ecdsa]), &ed25519), Verdict::Undecided(Some(_))));
        assert!(matches!(judged(listed(&[&ed25519]), &ecdsa), Verdict::Undecided(Some(_))));
        // Each curve is a kind of its own, as in OpenSSH.
        assert!(matches!(judged(listed(&[&ecdsa]), &p384), Verdict::Undecided(Some(_))));
        // Another key of the kind shown, with or without other kinds beside it.
        assert!(matches!(judged(listed(&[&other_ed25519]), &ed25519), Verdict::Changed { .. }));
        assert!(matches!(judged(listed(&[&ecdsa, &other_ed25519]), &ed25519), Verdict::Changed { .. }));
        // The key itself beats all that.
        assert!(matches!(judged(listed(&[&other_ed25519, &ecdsa, &ed25519]), &ed25519), Verdict::Trusted(_)));
    }

    #[test]
    fn hashed_entries_match() {
        // `ssh-keygen -H` form for "db.example.com" with a fixed salt.
        use base64::Engine;
        use hmac::{Hmac, KeyInit, Mac};
        let salt = b"0123456789abcdefghij";
        let mut mac = <Hmac<sha1::Sha1> as KeyInit>::new_from_slice(salt).unwrap();
        mac.update(b"db.example.com");
        let b64 = base64::engine::general_purpose::STANDARD;
        let field = format!("|1|{}|{}", b64.encode(salt), b64.encode(mac.finalize().into_bytes()));
        let k = key();
        let pk = k.public_key().clone();
        let text = format!("{field} {}\n", line_key(&pk));
        assert!(matches!(trust(&text, "db.example.com", 22).check_key(&pk), Verdict::Trusted(_)));
        assert_eq!(trust(&text, "other.example.com", 22).check_key(&pk), Verdict::Undecided(None));
    }

    #[test]
    fn revoked_keys_are_refused() {
        let k = key();
        let pk = k.public_key().clone();
        let line = line_key(&pk);
        let t = trust(&format!("db.example.com {line}\n@revoked * {line}\n"), "db.example.com", 22);
        assert!(matches!(t.check_key(&pk), Verdict::Refused(_)));
    }

    #[test]
    fn host_certificates_follow_openssh() {
        let ca = key();
        let host_key = key();
        let ca_line = format!("@cert-authority sublab.test,*.sublab.test {}\n", line_key(ca.public_key()));
        // Teleport: port 3022, CA lines for *.cluster still apply.
        let t = trust(&ca_line, "node1.sublab.test", 3022);
        assert!(t.has_cas());
        let good = host_cert(&ca, &host_key, &["node1.sublab.test", "node1"], VALID, CertType::Host);
        assert!(matches!(t.check_certificate(&good, NOW), Verdict::Trusted(_)), "{:?}", t.check_certificate(&good, NOW));
        // Issued for any host.
        let any = host_cert(&ca, &host_key, &[], VALID, CertType::Host);
        assert!(matches!(t.check_certificate(&any, NOW), Verdict::Trusted(_)));
        // Not for this name.
        let other = host_cert(&ca, &host_key, &["node2.sublab.test"], VALID, CertType::Host);
        assert!(matches!(t.check_certificate(&other, NOW), Verdict::Refused(r) if r.contains("isn't issued for node1.sublab.test")));
        // Expired / not yet valid.
        let old = host_cert(&ca, &host_key, &["node1.sublab.test"], (NOW - 7200, NOW - 3600), CertType::Host);
        assert!(matches!(t.check_certificate(&old, NOW), Verdict::Refused(_)));
        // A user certificate as a host key.
        let user = host_cert(&ca, &host_key, &["node1.sublab.test"], VALID, CertType::User);
        assert!(matches!(t.check_certificate(&user, NOW), Verdict::Refused(_)));
        // Another CA.
        let rogue = host_cert(&key(), &host_key, &["node1.sublab.test"], VALID, CertType::Host);
        assert!(matches!(t.check_certificate(&rogue, NOW), Verdict::Refused(r) if r.contains("doesn't trust")));
        // A host the CA line doesn't cover.
        let t2 = trust(&ca_line, "db.example.com", 22);
        assert!(!t2.has_cas());
        assert!(matches!(t2.check_certificate(&good, NOW), Verdict::Refused(_)));
        // A revoked CA.
        let t3 = trust(&format!("{ca_line}@revoked * {}\n", line_key(ca.public_key())), "node1.sublab.test", 22);
        assert!(matches!(t3.check_certificate(&good, NOW), Verdict::Refused(r) if r.contains("@revoked")));
    }

    #[test]
    fn junk_lines_are_skipped() {
        let k = key();
        let pk = k.public_key().clone();
        let text = format!("# comment\n\n@cert-authority\nhost ssh-ed25519 notbase64\nbroken\n@weird h {}\nh {}\n", line_key(&pk), line_key(&pk));
        let t = trust(&text, "h", 22);
        assert!(matches!(t.check_key(&pk), Verdict::Trusted(_)));
        assert!(!t.has_cas());
    }
}
