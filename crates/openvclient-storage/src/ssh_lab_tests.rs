//! Live tests against the SSH lab (`scripts/ssh-lab.sh up`): every login
//! method, key type and format, jump hosts, tunnels, SFTP edge cases and the
//! terminal. Skipped unless OPENVCLIENT_SSH_LAB_KEYS points at the lab's keys.
//! Every case is run and reported; the test fails at the end if any failed.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::*;

const MAIN: u16 = 52230;
const PAM: u16 = 52231;
const TWO_STEP: u16 = 52232;
const OTP: u16 = 52233;
const LEGACY: u16 = 52234;

fn keys() -> Option<PathBuf> {
    std::env::var_os("OPENVCLIENT_SSH_LAB_KEYS").map(PathBuf::from).filter(|p| p.join("ed25519").exists())
}

fn params(port: u16, user: &str, auth: &str, secret: Option<&str>, extra: &[(&str, String)]) -> ConnectParams {
    let mut v = BTreeMap::new();
    v.insert("host".into(), "127.0.0.1".into());
    v.insert("port".into(), port.to_string());
    v.insert("user".into(), user.into());
    v.insert("auth".into(), auth.into());
    for (k, val) in extra {
        v.insert((*k).into(), val.clone());
    }
    ConnectParams { values: v, secret: secret.map(str::to_owned), read_only: false, notifier: None }
}

/// Connect, pinning the host key the first time (as the UI does after the user confirms).
async fn connect(p: ConnectParams) -> Result<SessionHandle> {
    let p = trust_for_test(p).await?;
    SshDriver::default().connect(p).await
}

struct Report(Vec<(String, std::result::Result<String, String>)>);

impl Report {
    fn check(&mut self, name: &str, r: std::result::Result<String, String>) {
        eprintln!(
            "{} {name}: {}",
            if r.is_ok() { "PASS" } else { "FAIL" },
            match &r {
                Ok(m) | Err(m) => m,
            }
        );
        self.0.push((name.to_string(), r));
    }
    async fn login(&mut self, name: &str, p: ConnectParams) -> Option<SessionHandle> {
        match connect(p).await {
            Ok(h) => match h.base().ping().await {
                Ok(_) => {
                    self.check(name, Ok("logged in".into()));
                    Some(h)
                }
                Err(e) => {
                    self.check(name, Err(format!("logged in but ping failed: {e}")));
                    None
                }
            },
            Err(e) => {
                self.check(name, Err(e.to_string()));
                None
            }
        }
    }
    async fn refused(&mut self, name: &str, p: ConnectParams, must_contain: &str) {
        match connect(p).await {
            Ok(_) => self.check(name, Err("logged in but should have been refused".into())),
            Err(e) if e.to_string().to_lowercase().contains(&must_contain.to_lowercase()) => self.check(name, Ok(format!("refused: {e}"))),
            Err(e) => self.check(name, Err(format!("refused with an unhelpful message: {e}"))),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ssh_lab() {
    let Some(k) = keys() else {
        eprintln!("skipping: set OPENVCLIENT_SSH_LAB_KEYS (scripts/ssh-lab.sh up)");
        return;
    };
    let key = |name: &str| k.join(name).display().to_string();
    let mut r = Report(Vec::new());

    // ---- passwords ---------------------------------------------------------------------
    r.login("password", params(MAIN, "pwuser", "password", Some("pwpass"), &[])).await;
    r.refused("wrong password", params(MAIN, "pwuser", "password", Some("nope"), &[]), "authentication failed").await;
    r.login("password via PAM keyboard-interactive", params(PAM, "pamuser", "password", Some("pampw"), &[])).await;
    r.login("keyboard-interactive", params(PAM, "pamuser", "keyboard_interactive", Some("pampw"), &[])).await;
    // Backup codes work once; take an unused one from the lab container.
    let container = std::env::var("OPENVCLIENT_SSH_LAB_CONTAINER").unwrap_or_else(|_| "ovc-lab-main".into());
    let code = std::process::Command::new("docker")
        .args(["exec", &container, "sh", "-c", "tail -n 1 /home/otpuser/.google_authenticator"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|c| c.len() == 8 && c.chars().all(|ch| ch.is_ascii_digit()));
    match code {
        Some(code) => {
            r.login("password + one-time code", params(OTP, "otpuser", "keyboard_interactive", Some("otppw"), &[("otp", code)])).await;
        }
        None => eprintln!("SKIP password + one-time code: no unused backup code left (restart the lab)"),
    }
    r.refused("wrong one-time code", params(OTP, "otpuser", "keyboard_interactive", Some("otppw"), &[("otp", "00000000".into())]), "authentication failed").await;
    r.login("legacy server (ssh-rsa host key, group14-sha1)", params(LEGACY, "pwuser", "password", Some("pwpass"), &[])).await;

    // ---- keys ----------------------------------------------------------------------------
    for (name, pass) in [
        ("ed25519", None),
        ("ecdsa256", None),
        ("ecdsa384", None),
        ("ecdsa521", None),
        ("rsa2048", None),
        ("rsa4096", None),
        ("rsa_pem", None),
        ("ecdsa_pem", None),
        ("rsa_pkcs8", None),
        ("ed25519_enc", Some("keypass")),
        ("rsa_enc", Some("keypass")),
        ("rsa_pem_enc", Some("keypass")),
    ] {
        r.login(&format!("key {name}"), params(MAIN, "keyuser", "key", pass, &[("key_path", key(name))])).await;
    }
    r.refused("encrypted key, wrong passphrase", params(MAIN, "keyuser", "key", Some("wrong"), &[("key_path", key("ed25519_enc"))]), "passphrase").await;
    r.refused("encrypted key, no passphrase", params(MAIN, "keyuser", "key", None, &[("key_path", key("rsa_enc"))]), "passphrase").await;
    r.refused("key not authorised", params(MAIN, "pwuser", "key", None, &[("key_path", key("ed25519"))]), "authentication failed").await;
    for name in ["ed25519", "rsa_pem", "rsa_pkcs8"] {
        let pem = std::fs::read_to_string(k.join(name)).unwrap();
        r.login(&format!("pasted key {name}"), params(MAIN, "keyuser", "key_text", Some(&pem), &[])).await;
    }
    r.login("certificate", params(MAIN, "certuser", "certificate", None, &[("key_path", key("certkey")), ("cert_path", key("certkey-cert.pub"))])).await;
    r.login("key + password (two-step)", params(TWO_STEP, "keyuser", "key", Some("keypw"), &[("key_path", key("ed25519"))])).await;

    // ---- agent ---------------------------------------------------------------------------
    let agent = std::process::Command::new("ssh-agent").arg("-s").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    if let Some(sock) = agent.as_deref().and_then(|s| s.split("SSH_AUTH_SOCK=").nth(1)).and_then(|s| s.split(';').next()) {
        let added = std::process::Command::new("ssh-add").arg(k.join("ed25519")).env("SSH_AUTH_SOCK", sock).output().map(|o| o.status.success()).unwrap_or(false);
        // SAFETY: test-only; no other thread reads SSH_AUTH_SOCK concurrently in this test.
        unsafe { std::env::set_var("SSH_AUTH_SOCK", sock) };
        if added {
            r.login("ssh-agent", params(MAIN, "keyuser", "agent", None, &[])).await;
        }
    }

    // ---- jump host -----------------------------------------------------------------------
    let jump = |auth: &str, secret: Option<&'static str>, jx: Vec<(&'static str, String)>| {
        let mut extra = vec![("jump_host", "127.0.0.1".to_string()), ("jump_port", MAIN.to_string()), ("jump_user", "keyuser".to_string())];
        extra.extend(jx);
        let mut p = params(22, "inner", auth, secret, &extra.iter().map(|(a, b)| (*a, b.clone())).collect::<Vec<_>>());
        p.values.insert("host".into(), "inner".into());
        p
    };
    // The jump host's key must be pinned first (the error says which one).
    async fn connect_jump(mut p: ConnectParams) -> Result<SessionHandle> {
        for _ in 0..3 {
            match SshDriver::default().connect(p.clone()).await {
                Err(e) if openvclient_net::unknown_host_key(&e).is_some() => {
                    let fp = openvclient_net::unknown_host_key(&e).unwrap();
                    let k = if e.message.contains(JUMP_MARK) { JUMP_FINGERPRINT } else { FINGERPRINT };
                    p.values.insert(k.into(), fp);
                }
                other => return other,
            }
        }
        SshDriver::default().connect(p).await
    }
    for (name, p) in [
        ("jump host (key) → inner (password)", jump("password", Some("innerpw"), vec![("jump_key", key("jumpkey"))])),
        ("jump host (agent) → inner (key)", jump("key", None, vec![("key_path", key("ed25519"))])),
    ] {
        match connect_jump(p).await {
            Ok(h) => {
                let got = h.resource().unwrap().console(None, "cat ~/where.txt").await;
                r.check(
                    name,
                    match got {
                        Ok(Content::Text(t)) if t.contains("inner-host") => Ok("reached the inner host".into()),
                        other => Err(format!("connected but wrong host: {other:?}")),
                    },
                );
            }
            Err(e) => r.check(name, Err(e.to_string())),
        }
    }

    // ---- tunnel through SSH to a private service --------------------------------------------
    {
        let spec = |fp: Option<String>| openvclient_net::ProxySpec::Ssh {
            host: "127.0.0.1".into(),
            port: MAIN,
            user: "pwuser".into(),
            auth: openvclient_net::SshAuth::Password("pwpass".into()),
            known_fingerprint: fp,
            timeout: Duration::from_secs(10),
        };
        let fp = match openvclient_net::Tunnel::open(spec(None), "labredis", 6379).await {
            Err(e) => openvclient_net::unknown_host_key(&e),
            Ok(_) => None,
        };
        let res = async {
            let t = openvclient_net::Tunnel::open(spec(fp), "labredis", 6379).await.map_err(|e| e.to_string())?;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut s = tokio::net::TcpStream::connect(t.local).await.map_err(|e| e.to_string())?;
            s.write_all(b"PING\r\n").await.map_err(|e| e.to_string())?;
            let mut b = [0u8; 16];
            let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut b)).await.map_err(|_| "timeout".to_string())?.map_err(|e| e.to_string())?;
            let got = String::from_utf8_lossy(&b[..n]).trim().to_string();
            if got == "+PONG" { Ok("redis answered through the tunnel".to_string()) } else { Err(got) }
        }
        .await;
        r.check("SSH tunnel to a private Redis", res);
    }

    // ---- SFTP-only account -----------------------------------------------------------------
    if let Some(h) = r.login("SFTP-only account (ForceCommand internal-sftp)", params(MAIN, "sftponly", "password", Some("sftppw"), &[])).await {
        let s = h.resource().unwrap().clone();
        let l = s.list_dir(None, "").await;
        r.check(
            "SFTP-only: list home",
            match l {
                Ok(l) if l.entries.iter().any(|e| e.name == "hello.txt") => Ok("listed".into()),
                other => Err(format!("{other:?}")),
            },
        );
    }

    // ---- SFTP edge cases ---------------------------------------------------------------------
    if let Some(h) = r.login("files account", params(MAIN, "pwuser", "password", Some("pwpass"), &[])).await {
        let s = h.resource().unwrap().clone();
        let home = s.root_dir().unwrap();
        let l = s.list_dir(Some(&home), "").await.unwrap();
        let find = |n: &str| l.entries.iter().find(|e| e.name == n).cloned();
        r.check(
            "lists unicode, spaces and hidden names",
            if find("résumé ✓.txt").is_some() && find("file with spaces.txt").is_some() && find(".env").is_some() {
                Ok("all present".into())
            } else {
                Err(format!("{:?}", l.entries.iter().map(|e| &e.name).collect::<Vec<_>>()))
            },
        );
        r.check(
            "symlink to a folder is a folder",
            match find("link-to-dir") {
                Some(e) if e.is_dir => Ok("folder".into()),
                other => Err(format!("{other:?}")),
            },
        );
        r.check(
            "symlink to a file is a file with its size",
            match find("link-to-leaf") {
                Some(e) if !e.is_dir && e.size == Some(5) => Ok("file, 5 B".into()),
                other => Err(format!("{other:?}")),
            },
        );
        if let Some(d) = find("link-to-dir") {
            r.check(
                "open a symlinked folder",
                match s.list_dir(Some(&d.id), "").await {
                    Ok(l) if l.entries.iter().any(|e| e.name == "deeper") => Ok("listed".into()),
                    other => Err(format!("{other:?}")),
                },
            );
        }
        if let Some(e) = find("résumé ✓.txt") {
            r.check(
                "preview a unicode-named file",
                match s.open(&e.id, OpenOptions::default()).await {
                    Ok(Content::Text(t)) if t.contains("unicode ok") => Ok("read".into()),
                    other => Err(format!("{other:?}")),
                },
            );
        }
        if let Some(e) = find("no-access") {
            r.check(
                "unreadable folder gives a clear error",
                match s.list_dir(Some(&e.id), "").await {
                    Err(err) if err.to_string().to_lowercase().contains("permission") => Ok(format!("{err}")),
                    other => Err(format!("{other:?}")),
                },
            );
        }
        // Large download, then upload it back under a unicode name.
        let dir = std::env::temp_dir().join(format!("ovc-ssh-lab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let local = dir.join("big.bin");
        if let Some(e) = find("big.bin") {
            let mut a = Args::new();
            a.insert("file".into(), local.display().to_string());
            let res = s.run_action(&e.id, "download", a).await;
            r.check(
                "download 20 MiB",
                match (res, std::fs::metadata(&local).map(|m| m.len())) {
                    (Ok(_), Ok(20_971_520)) => Ok("20 MiB on disk".into()),
                    other => Err(format!("{other:?}")),
                },
            );
        }
        let up = dir.join("über upload ✓.bin");
        let _ = std::fs::copy(&local, &up);
        let mut a = Args::new();
        a.insert("file".into(), up.display().to_string());
        let res = s.run_action(&home, "upload", a).await;
        let after = s.list_dir(Some(&home), "über").await.ok();
        r.check(
            "upload 20 MiB with a unicode name",
            match (res, after.as_ref().and_then(|l| l.entries.first().map(|e| e.size))) {
                (Ok(_), Some(Some(20_971_520))) => Ok("uploaded".into()),
                other => Err(format!("{other:?}")),
            },
        );
        // Folder lifecycle: create, add a file, rename, delete (not empty).
        for leftover in ["lab folder", "lab folder renamed"] {
            let _ = s.run_action(&format!("d:{}", join(home.strip_prefix("d:").unwrap(), leftover)), "rmdir", Args::new()).await;
        }
        let mut a = Args::new();
        a.insert("name".into(), "lab folder".into());
        let made = s.run_action(&home, "mkdir", a).await;
        let folder = format!("d:{}", join(home.strip_prefix("d:").unwrap(), "lab folder"));
        let mut a = Args::new();
        a.insert("file".into(), dir.join("big.bin").display().to_string());
        let filled = s.run_action(&folder, "upload", a).await;
        let mut a = Args::new();
        a.insert("name".into(), "lab folder renamed".into());
        let renamed = s.run_action(&folder, "rename", a).await;
        let moved = format!("d:{}", join(home.strip_prefix("d:").unwrap(), "lab folder renamed"));
        let deleted = s.run_action(&moved, "rmdir", Args::new()).await;
        r.check(
            "folder: create, fill, rename, delete with contents",
            match (made, filled, renamed, deleted) {
                (Ok(_), Ok(_), Ok(_), Ok(_)) => Ok("all steps".into()),
                other => Err(format!("{other:?}")),
            },
        );
        // Renaming onto an existing name says so.
        let mut a = Args::new();
        a.insert("name".into(), "file with spaces.txt".into());
        let clash = match find("résumé ✓.txt") {
            Some(e) => s.run_action(&e.id, "rename", a).await,
            None => Err(Error::other("missing")),
        };
        r.check(
            "rename onto an existing name explains why",
            match clash {
                Err(e) if e.message.contains("already exists") => Ok(e.message),
                other => Err(format!("{other:?}")),
            },
        );
        if let Some(Some(e)) = after.map(|l| l.entries.into_iter().next()) {
            let _ = s.run_action(&e.id, "rm", Args::new()).await;
        }
        let _ = std::fs::remove_dir_all(&dir);

        // Terminal: output, resize, exit.
        let res = async {
            let term = s.open_terminal(80, 24).await.map_err(|e| e.to_string())?;
            term.input.send(TermInput::Resize { cols: 132, rows: 40 }).map_err(|e| e.to_string())?;
            term.input.send(TermInput::Data(b"stty size; echo done-$((6*7))\n".to_vec())).map_err(|e| e.to_string())?;
            let mut seen = String::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && !seen.contains("done-42") {
                if let Ok(TermEvent::Data(d)) = term.output.recv_timeout(Duration::from_millis(200)) {
                    seen.push_str(&String::from_utf8_lossy(&d));
                }
            }
            if seen.contains("40 132") && seen.contains("done-42") { Ok("output and resize".to_string()) } else { Err(seen) }
        }
        .await;
        r.check("terminal output and resize", res);
    }

    let failed: Vec<_> = r.0.iter().filter(|(_, x)| x.is_err()).map(|(n, x)| format!("{n}: {}", x.as_ref().unwrap_err())).collect();
    eprintln!("\n{} of {} SSH lab checks passed", r.0.len() - failed.len(), r.0.len());
    assert!(failed.is_empty(), "failed:\n{}", failed.join("\n"));
}
