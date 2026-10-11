//! New versions of Anything: finding the latest release on GitHub, checking
//! its signature and unpacking it.
//!
//! A release has, per platform, an archive with the three programs
//! (`anything-<version>-<arch>-linux.tar.gz`), a Debian package and an
//! AppImage, each with a minisign signature (`<file>.minisig`) whose trusted
//! comment is the file's name. Only a file signed with [`PUBLIC_KEY`] under
//! its own name is ever used, so neither a swapped file nor an older release
//! under a new name gets through.

pub mod layout;

use std::{io::Read as _, time::Duration};

/// Where releases are published.
pub const REPOSITORY: &str = "srcmkr/anything";
/// The key releases are signed with (minisign). Empty while there is none:
/// then nothing verifies and nothing is installed.
pub const PUBLIC_KEY: &str = include_str!("../../../packaging/minisign.pub");
/// The programs in an archive, as they are installed.
pub const PROGRAMS: [&str; 3] = ["anything", "anythingd", "any"];

const TIMEOUT: Duration = Duration::from_secs(10);
/// Far above any real archive; a bound for what is read into memory.
const MAX_DOWNLOAD: u64 = 200 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// Without a leading `v`.
    pub version: String,
    /// The release page, for people.
    pub page: String,
    /// Its files: name and download address.
    pub assets: Vec<(String, String)>,
}

impl Release {
    /// The download address of the file `name`, if the release has it.
    pub fn asset(&self, name: &str) -> Option<&str> {
        self.assets.iter().find(|(n, _)| n == name).map(|(_, url)| url.as_str())
    }
}

/// The first three numbers of a version: `v1.2.3-beta` is `[1, 2, 3]`.
fn numbers(version: &str) -> [u64; 3] {
    let mut out = [0; 3];
    let parts = version.trim_start_matches(['v', 'V']).split(['.', '-', '+']);
    for (slot, part) in out.iter_mut().zip(parts) {
        *slot = part.parse().unwrap_or(0);
    }
    out
}

/// Whether `latest` is a higher version than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    numbers(latest) > numbers(current)
}

/// `x86_64` or `aarch64`.
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// The archive name for `version` on this platform.
pub fn archive_name(version: &str) -> String {
    format!("anything-{version}-{}-linux.tar.gz", arch())
}

/// The Debian package for `version` on this platform.
pub fn deb_name(version: &str) -> String {
    let arch = match arch() {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("anything_{version}_{arch}.deb")
}

/// The AppImage for `version` on this platform.
pub fn appimage_name(version: &str) -> String {
    format!("Anything-{version}-{}.AppImage", arch())
}

/// Reads GitHub's answer for the latest release.
pub fn parse_release(json: &str) -> Option<Release> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let version = value["tag_name"].as_str()?.trim_start_matches(['v', 'V']).to_owned();
    let page = value["html_url"].as_str()?.to_owned();
    let assets = value["assets"]
        .as_array()
        .map(|assets| {
            assets
                .iter()
                .filter_map(|a| Some((a["name"].as_str()?.to_owned(), a["browser_download_url"].as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    Some(Release { version, page, assets })
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT * 6))
        .timeout_connect(Some(TIMEOUT))
        .user_agent(concat!("anything/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Asks GitHub for the latest release. `None` when offline or anything else
/// goes wrong: an update check never gets in the way.
pub fn latest() -> Option<Release> {
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let mut response = agent().get(&url).header("Accept", "application/vnd.github+json").call().ok()?;
    let body = response.body_mut().with_config().limit(1 << 20).read_to_string().ok()?;
    parse_release(&body)
}

/// Downloads `url` into memory.
pub fn download(url: &str) -> Result<Vec<u8>, String> {
    let mut response = agent().get(url).call().map_err(|e| format!("download failed: {e}"))?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_DOWNLOAD)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("download failed: {e}"))?;
    Ok(bytes)
}

/// Downloads the file `name` of `release` and its signature, and checks that
/// it was signed with [`PUBLIC_KEY`] under that name.
pub fn fetch_verified(release: &Release, name: &str) -> Result<Vec<u8>, String> {
    let (Some(url), Some(signature_url)) = (release.asset(name), release.asset(&format!("{name}.minisig"))) else {
        return Err(format!("release {} has no {name}", release.version));
    };
    let data = download(url)?;
    let signature = String::from_utf8(download(signature_url)?).map_err(|_| "unreadable signature".to_owned())?;
    verify(&data, &signature, PUBLIC_KEY, name)?;
    Ok(data)
}

/// Checks that `archive` was signed with `public_key` under the name `name`.
pub fn verify(archive: &[u8], signature: &str, public_key: &str, name: &str) -> Result<(), String> {
    let key = minisign_verify::PublicKey::decode(public_key.trim())
        .or_else(|_| minisign_verify::PublicKey::from_base64(public_key.trim()))
        .map_err(|_| "no valid release key in this build".to_owned())?;
    let signature = minisign_verify::Signature::decode(signature).map_err(|_| "unreadable signature".to_owned())?;
    key.verify(archive, &signature, false).map_err(|_| "the signature does not match".to_owned())?;
    if signature.trusted_comment() != name {
        return Err("the signature is for a different file".into());
    }
    Ok(())
}

/// The three programs from a verified archive, in the order of [`PROGRAMS`].
/// Anything else in the archive is ignored; links, folders outside it and
/// missing programs are refused.
pub fn unpack(archive: &[u8]) -> Result<[Vec<u8>; 3], String> {
    let mut found: [Option<Vec<u8>>; 3] = Default::default();
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries().map_err(|e| format!("broken archive: {e}"))? {
        let mut entry = entry.map_err(|e| format!("broken archive: {e}"))?;
        let path = entry.path().map_err(|e| format!("broken archive: {e}"))?.into_owned();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(slot) = PROGRAMS.iter().position(|p| *p == name) else { continue };
        if !entry.header().entry_type().is_file() {
            return Err(format!("{name} in the archive is not a plain file"));
        }
        let mut bytes = Vec::new();
        entry.by_ref().take(MAX_DOWNLOAD).read_to_end(&mut bytes).map_err(|e| format!("broken archive: {e}"))?;
        found[slot] = Some(bytes);
    }
    let [Some(a), Some(b), Some(c)] = found else {
        return Err("the archive lacks a program".into());
    };
    Ok([a, b, c])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.9", "0.1.10"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.2.0-beta", "0.2.0"));
        assert!(!is_newer("garbage", "0.0.1"));
    }

    #[test]
    fn reads_github_answer() {
        let name = archive_name("0.2.0");
        let json = format!(
            r#"{{"tag_name":"v0.2.0","html_url":"https://example.org/r","assets":[
                {{"name":"{name}","browser_download_url":"https://example.org/a"}},
                {{"name":"{name}.minisig","browser_download_url":"https://example.org/s"}},
                {{"name":"other.tar.gz","browser_download_url":"https://example.org/o"}}]}}"#
        );
        let release = parse_release(&json).unwrap();
        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.page, "https://example.org/r");
        assert_eq!(release.asset(&name), Some("https://example.org/a"));
        assert_eq!(release.asset(&format!("{name}.minisig")), Some("https://example.org/s"));
        let bare = parse_release(r#"{"tag_name":"v0.3.0","html_url":"https://example.org/r","assets":[]}"#).unwrap();
        assert_eq!(bare.asset(&archive_name("0.3.0")), None);
        assert_eq!(parse_release("not json"), None);
    }

    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (path, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn signed(data: &[u8], comment: &str) -> (String, String) {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let signature = minisign::sign(Some(&pair.pk), &pair.sk, std::io::Cursor::new(data), Some(comment), None).unwrap();
        (pair.pk.to_box().unwrap().to_string(), signature.to_string())
    }

    #[test]
    fn only_signed_archives_under_their_own_name() {
        let name = archive_name("0.2.0");
        let data = archive(&[("anything-0.2.0/anything", b"app"), ("anything-0.2.0/anythingd", b"service"), ("anything-0.2.0/any", b"cli")]);
        let (key, signature) = signed(&data, &name);
        assert_eq!(verify(&data, &signature, &key, &name), Ok(()));
        // Another release's archive passed off under this name.
        assert!(verify(&data, &signature, &key, &archive_name("0.3.0")).is_err());
        // A changed archive.
        let mut changed = data.clone();
        *changed.last_mut().unwrap() ^= 1;
        assert!(verify(&changed, &signature, &key, &name).is_err());
        // Signed with someone else's key.
        let (other_key, _) = signed(b"x", "x");
        assert!(verify(&data, &signature, &other_key, &name).is_err());
        // No key in this build.
        assert!(verify(&data, &signature, "", &name).is_err());

        let [app, service, cli] = unpack(&data).unwrap();
        assert_eq!((app.as_slice(), service.as_slice(), cli.as_slice()), (&b"app"[..], &b"service"[..], &b"cli"[..]));
        assert!(unpack(&archive(&[("anything-0.2.0/anything", b"app")])).is_err());
        assert!(unpack(b"not an archive").is_err());
    }
}
