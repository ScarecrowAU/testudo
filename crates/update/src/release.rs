//! The stable channel: signed GitHub release archives.

use std::io::Read;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::install::{BINARIES, Bundle};
use crate::{Kind, PUBLIC_KEY, REPO, TARGET, Update, UpdateChannel, VERSION, agent};

/// Release archives are tens of MB; refuse anything absurd.
const MAX_ARCHIVE: u64 = 512 << 20;

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub fn check() -> Result<Option<Update>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let release: Release = match agent().get(&url).header("Accept", "application/vnd.github+json").call() {
        Ok(mut r) => r.body_mut().read_json()?,
        // No release published yet.
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(e) => return Err(anyhow!(e).context("asking GitHub for the latest release")),
    };
    pick(&release, VERSION, TARGET)
}

/// The update `release` offers over `current`, if it's newer.
fn pick(release: &Release, current: &str, target: &str) -> Result<Option<Update>> {
    let version = semver::Version::parse(release.tag_name.trim_start_matches('v'))
        .with_context(|| format!("release tag {}", release.tag_name))?;
    if version <= semver::Version::parse(current)? {
        return Ok(None);
    }
    let archive = format!("testudo-{}-{target}.tar.gz", release.tag_name);
    let url_of = |name: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.clone())
    };
    let (Some(archive_url), Some(sig_url)) = (url_of(&archive), url_of(&format!("{archive}.minisig"))) else {
        bail!("Testudo {version} has no signed build for {target}");
    };
    Ok(Some(Update {
        channel: UpdateChannel::Stable,
        label: version.to_string(),
        url: release.html_url.clone(),
        kind: Kind::Release { archive_url, sig_url },
    }))
}

pub fn fetch(archive_url: &str, sig_url: &str, log: &mut dyn FnMut(&str)) -> Result<Bundle> {
    log("Downloading");
    let archive = download(archive_url, MAX_ARCHIVE)?;
    let sig = String::from_utf8(download(sig_url, 64 << 10)?).context("signature isn't text")?;
    log("Checking the signature");
    verify(&archive, &sig, PUBLIC_KEY)?;
    unpack(&archive)
}

fn download(url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut r = agent().get(url).call().with_context(|| format!("downloading {url}"))?;
    let mut out = Vec::new();
    r.body_mut()
        .with_config()
        .limit(limit)
        .reader()
        .read_to_end(&mut out)
        .with_context(|| format!("downloading {url}"))?;
    Ok(out)
}

fn verify(data: &[u8], sig: &str, public_key: &str) -> Result<()> {
    let key = minisign_verify::PublicKey::from_base64(public_key).map_err(|e| anyhow!("public key: {e}"))?;
    let sig = minisign_verify::Signature::decode(sig).map_err(|e| anyhow!("signature: {e}"))?;
    key.verify(data, &sig, false)
        .map_err(|e| anyhow!("the download's signature doesn't match Testudo's release key ({e})"))
}

/// The binaries and adapters from a release archive, which holds one
/// top-level directory with `testudo`, `testudo-cli` and `adapters/`. Only
/// those names are taken, so nothing outside the install can be written.
fn unpack(archive: &[u8]) -> Result<Bundle> {
    let mut bundle = Bundle::default();
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries().context("reading the archive")? {
        let mut entry = entry.context("reading the archive")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.to_string_lossy().into_owned();
        let Some((_top, rest)) = path.split_once('/') else {
            continue;
        };
        let slot = if BINARIES.contains(&rest) {
            &mut bundle.binaries
        } else if let Some(name) = rest.strip_prefix("adapters/")
            && !name.contains('/')
            && (name.ends_with(".mjs") || name == "package.json")
        {
            &mut bundle.adapters
        } else {
            continue;
        };
        let name = rest.rsplit('/').next().unwrap_or(rest).to_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        slot.push((name, data));
    }
    bundle.check()?;
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, assets: &[&str]) -> Release {
        Release {
            tag_name: tag.into(),
            html_url: format!("https://github.com/{REPO}/releases/tag/{tag}"),
            assets: assets
                .iter()
                .map(|n| Asset {
                    name: n.to_string(),
                    browser_download_url: format!("https://example.test/{n}"),
                })
                .collect(),
        }
    }

    #[test]
    fn offers_only_newer_signed_builds_for_this_target() {
        let t = "x86_64-unknown-linux-gnu";
        let a = "testudo-v0.2.0-x86_64-unknown-linux-gnu.tar.gz";
        let sig = format!("{a}.minisig");
        let r = release("v0.2.0", &[a, &sig, "testudo-v0.2.0-aarch64-apple-darwin.tar.gz"]);

        let u = pick(&r, "0.1.0", t).unwrap().unwrap();
        assert_eq!(u.label, "0.2.0");
        assert_eq!(
            u.kind,
            Kind::Release {
                archive_url: format!("https://example.test/{a}"),
                sig_url: format!("https://example.test/{sig}"),
            }
        );
        assert_eq!(pick(&r, "0.2.0", t).unwrap(), None, "same version");
        assert_eq!(pick(&r, "0.10.0", t).unwrap(), None, "older, compared as semver");
        assert!(pick(&r, "0.1.0", "aarch64-apple-darwin").is_err(), "unsigned");
        assert!(pick(&release("v0.2.0", &[a]), "0.1.0", t).is_err(), "no signature");
    }

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (path, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, path, *data).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn unpacks_only_the_install_files() {
        let a = archive(&[
            ("testudo-v0.2.0/testudo", b"gui"),
            ("testudo-v0.2.0/testudo-cli", b"cli"),
            ("testudo-v0.2.0/README.md", b"readme"),
            ("testudo-v0.2.0/adapters/preload.mjs", b"pre"),
            ("testudo-v0.2.0/adapters/package.json", b"{}"),
            ("testudo-v0.2.0/adapters/sub/evil.mjs", b"no"),
        ]);
        let b = unpack(&a).unwrap();
        let names = |v: &Vec<(String, Vec<u8>)>| v.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
        assert_eq!(names(&b.binaries), ["testudo", "testudo-cli"]);
        assert_eq!(names(&b.adapters), ["preload.mjs", "package.json"]);
        assert_eq!(b.binaries[0].1, b"gui");

        let missing = archive(&[("t/testudo", b"gui"), ("t/adapters/preload.mjs", b"pre")]);
        assert!(unpack(&missing).is_err());
    }

    #[test]
    fn verifies_signatures() {
        let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let pk = kp.pk.to_base64();
        let data = b"archive bytes";
        let sig = minisign::sign(None, &kp.sk, std::io::Cursor::new(data), None, None)
            .unwrap()
            .to_string();
        verify(data, &sig, &pk).unwrap();
        assert!(verify(b"tampered", &sig, &pk).is_err());
        let other = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        assert!(verify(data, &sig, &other.pk.to_base64()).is_err());
        // The built-in key parses.
        minisign_verify::PublicKey::from_base64(PUBLIC_KEY).unwrap();
    }
}
