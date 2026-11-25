//! Self-update from GitHub Releases, opt-in: `crack update` (or `crack update --check`),
//! and with `update_check = true` a once-a-day notice in the window.
//!
//! A release is applied only when its `SHA256SUMS` carries a valid minisign signature
//! from the key built into crack, and the downloaded archive matches its sum. The old
//! binary is kept next to the new one (`crack.old`) to roll back. Copies installed by
//! Nix update through Nix; an AppImage replaces its own file. Builds only update to
//! the same build type, and air-gapped builds have no updater.

#![cfg_attr(not(feature = "cloud"), allow(dead_code))]

use std::path::{Path, PathBuf};

const REPO: &str = "LiveLaughLizard/crack";

/// The release signing key (minisign). Releases not signed by it are never applied.
const PUBLIC_KEY: &str = "RWSB2eYkIqYziyN/g52gBI5oDby9+F9PLP3jTQFlPsGzWlnrtoDgae+Z";

/// This binary's build type, as release archives name it.
pub fn flavor() -> &'static str {
    if cfg!(feature = "force-sandbox") {
        "strict"
    } else if !cfg!(feature = "cloud") {
        "airgap"
    } else if !cfg!(feature = "china") {
        "no-china"
    } else {
        "default"
    }
}

/// `1.2.3` → (1, 2, 3); a `v` prefix and pre-release suffixes are ignored.
fn version(text: &str) -> Option<(u64, u64, u64)> {
    let core = text
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((
        parts.next()??,
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
    ))
}

pub struct Release {
    pub version: String,
    pub assets: Vec<(String, String)>,
    pub notes_url: String,
}

/// The newest release, if it's newer than this binary.
pub async fn newer(http: &reqwest::Client) -> anyhow::Result<Option<Release>> {
    let response = http
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .header("User-Agent", concat!("crack/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None); // no releases yet
    }
    anyhow::ensure!(
        response.status().is_success(),
        "GitHub answered {}",
        response.status()
    );
    let json: serde_json::Value = response.json().await?;
    let tag = json["tag_name"].as_str().unwrap_or_default().to_owned();
    let (Some(latest), Some(current)) = (version(&tag), version(env!("CARGO_PKG_VERSION"))) else {
        anyhow::bail!("couldn't read the release version '{tag}'");
    };
    if latest <= current {
        return Ok(None);
    }
    let assets = json["assets"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    Some((
                        a["name"].as_str()?.to_owned(),
                        a["browser_download_url"].as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(Release {
        version: tag.trim_start_matches('v').to_owned(),
        assets,
        notes_url: json["html_url"].as_str().unwrap_or_default().to_owned(),
    }))
}

/// How this copy was installed, which decides how it updates.
#[derive(Debug, PartialEq)]
pub enum Install {
    /// A plain binary we can replace.
    Binary(PathBuf),
    /// An AppImage: replace the image file itself.
    AppImage(PathBuf),
    /// Managed by Nix: update through Nix.
    Nix,
}

pub fn install_kind() -> anyhow::Result<Install> {
    if let Some(image) = std::env::var_os("APPIMAGE") {
        return Ok(Install::AppImage(PathBuf::from(image)));
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    if exe.starts_with("/nix/store") {
        return Ok(Install::Nix);
    }
    Ok(Install::Binary(exe))
}

/// The asset this copy needs from a release.
fn asset_name(release: &Release, install: &Install) -> String {
    let arch = std::env::consts::ARCH;
    match install {
        Install::AppImage(_) => format!("crack-{}-{}-{arch}.AppImage", release.version, flavor()),
        _ => format!("crack-{}-{}-{arch}-linux.tar.gz", release.version, flavor()),
    }
}

async fn download(http: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let response = http
        .get(url)
        .header("User-Agent", concat!("crack/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(600))
        .send()
        .await?;
    anyhow::ensure!(
        response.status().is_success(),
        "download failed: {}",
        response.status()
    );
    Ok(response.bytes().await?.to_vec())
}

/// Checks `SHA256SUMS` against its signature, then `bytes` against its line for `name`.
pub fn verify(
    sums: &[u8],
    signature: &str,
    name: &str,
    bytes: &[u8],
    key: &str,
) -> anyhow::Result<()> {
    let public = minisign_verify::PublicKey::from_base64(key)
        .map_err(|e| anyhow::anyhow!("bad built-in key: {e}"))?;
    let signature = minisign_verify::Signature::decode(signature)
        .map_err(|e| anyhow::anyhow!("the release signature is malformed: {e}"))?;
    public.verify(sums, &signature, false).map_err(|_| {
        anyhow::anyhow!("SHA256SUMS isn't signed by crack's release key: not updating")
    })?;
    let text = String::from_utf8_lossy(sums);
    let expected = text
        .lines()
        .find_map(|line| {
            let (sum, file) = line.split_once(char::is_whitespace)?;
            (file.trim().trim_start_matches('*') == name).then(|| sum.to_ascii_lowercase())
        })
        .ok_or_else(|| anyhow::anyhow!("{name} isn't listed in SHA256SUMS"))?;
    let actual = {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(bytes))
    };
    anyhow::ensure!(
        actual == expected,
        "{name} doesn't match its checksum: not updating"
    );
    Ok(())
}

/// Replaces `target` with `new_file`, keeping the old one as `<name>.old`.
fn replace(target: &Path, new_file: &Path) -> anyhow::Result<()> {
    let old = target.with_file_name(format!(
        "{}.old",
        target.file_name().unwrap_or_default().to_string_lossy()
    ));
    let _ = std::fs::remove_file(&old);
    std::fs::rename(target, &old)?;
    if let Err(error) = std::fs::rename(new_file, target) {
        // Put the old one back.
        let _ = std::fs::rename(&old, target);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// `crack update`: finds, verifies and installs a newer release. Returns what happened.
pub async fn run(http: &reqwest::Client, check_only: bool) -> anyhow::Result<String> {
    anyhow::ensure!(
        cfg!(target_os = "linux"),
        "self-update is only for Linux builds so far; download the new release from GitHub"
    );
    let Some(release) = newer(http).await? else {
        return Ok(format!(
            "crack {} is the latest version.",
            env!("CARGO_PKG_VERSION")
        ));
    };
    if check_only {
        return Ok(format!(
            "crack {} is available (you have {}): {}\nInstall it with `crack update`.",
            release.version,
            env!("CARGO_PKG_VERSION"),
            release.notes_url
        ));
    }
    let install = install_kind()?;
    if install == Install::Nix {
        anyhow::bail!(
            "this crack was installed with Nix; update it through Nix (crack {} is out)",
            release.version
        );
    }
    let name = asset_name(&release, &install);
    let url_of = |file: &str| {
        release
            .assets
            .iter()
            .find(|(n, _)| n == file)
            .map(|(_, u)| u.clone())
            .ok_or_else(|| anyhow::anyhow!("the release has no {file}"))
    };
    let bytes = download(http, &url_of(&name)?).await?;
    let sums = download(http, &url_of("SHA256SUMS")?).await?;
    let signature = String::from_utf8(download(http, &url_of("SHA256SUMS.minisig")?).await?)?;
    verify(&sums, &signature, &name, &bytes, PUBLIC_KEY)?;

    let (target, staged) = match &install {
        Install::AppImage(image) => {
            let staged = image.with_extension("new");
            std::fs::write(&staged, &bytes)?;
            (image.clone(), staged)
        }
        Install::Binary(exe) => {
            let dir = exe.parent().unwrap_or(Path::new(".")).join(".crack-update");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir)?;
            let archive = dir.join(&name);
            std::fs::write(&archive, &bytes)?;
            let status = std::process::Command::new("tar")
                .args(["-xzf"])
                .arg(&archive)
                .arg("-C")
                .arg(&dir)
                .status()?;
            anyhow::ensure!(status.success(), "couldn't unpack {name}");
            let inner = name.trim_end_matches(".tar.gz");
            let binary = dir.join(inner).join("crack");
            anyhow::ensure!(binary.is_file(), "{name} has no crack binary");
            let staged = exe.with_file_name("crack.new");
            std::fs::rename(&binary, &staged)?;
            let _ = std::fs::remove_dir_all(&dir);
            (exe.clone(), staged)
        }
        Install::Nix => unreachable!(),
    };
    make_executable(&staged)?;
    replace(&target, &staged)?;
    Ok(format!(
        "Updated to crack {} (the previous version is kept as {}.old). Restart crack to use it.",
        release.version,
        target.display()
    ))
}

/// With `update_check = true`: at most once a day, whether a newer release exists.
pub async fn daily_notice(http: &reqwest::Client) -> Option<String> {
    let store = crate::store::get();
    let now = chrono::Utc::now().timestamp();
    let last: i64 = store
        .kv("update-checked")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if now - last < 86_400 {
        return None;
    }
    store.set_kv("update-checked", now.to_string());
    match newer(http).await {
        Ok(Some(release)) => Some(format!(
            "crack {} is available: run `crack update`",
            release.version
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert!(version("v0.2.0") > version("0.1.9"));
        assert_eq!(version("1.2"), Some((1, 2, 0)));
        assert_eq!(version("0.3.0-rc1"), Some((0, 3, 0)));
        assert!(version("garbage").is_none());
    }

    /// A key, sums and signature made with `minisign -G -W` and `minisign -S` for tests.
    #[test]
    fn verifies_signed_sums_and_refuses_tampering() {
        let Ok(fixture) = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/data/minisign-fixture.txt"),
        ) else {
            panic!("missing tests/data/minisign-fixture.txt");
        };
        let mut parts = fixture.split("\n===\n");
        let key = parts.next().unwrap().trim();
        // The separator took the signed file's final newline.
        let sums = format!("{}\n", parts.next().unwrap());
        let sums = sums.as_str();
        let signature = parts.next().unwrap();
        let payload = b"crack binary bytes";
        verify(
            sums.as_bytes(),
            signature,
            "crack-9.9.9-default-x86_64-linux.tar.gz",
            payload,
            key,
        )
        .unwrap();
        // A different file, a changed sums file, or another key all fail.
        assert!(
            verify(
                sums.as_bytes(),
                signature,
                "crack-9.9.9-default-x86_64-linux.tar.gz",
                b"evil",
                key
            )
            .is_err()
        );
        let tampered = sums.replace('a', "b");
        assert!(
            verify(
                tampered.as_bytes(),
                signature,
                "crack-9.9.9-default-x86_64-linux.tar.gz",
                payload,
                key
            )
            .is_err()
        );
        assert!(
            verify(
                sums.as_bytes(),
                signature,
                "crack-9.9.9-default-x86_64-linux.tar.gz",
                payload,
                PUBLIC_KEY
            )
            .is_err()
        );
    }
}
