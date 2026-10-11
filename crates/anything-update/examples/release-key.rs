//! Makes the key pair releases are signed with. Run once:
//!
//!     cargo run -p anything-update --example release-key -- ~/.config/anything-release.key
//!
//! The public key goes to `packaging/minisign.pub` (built into Anything);
//! the secret key to the given file, which is never overwritten. Put its
//! contents into the repository secret `MINISIGN_SECRET_KEY`, then keep the
//! file somewhere safe and offline.

use std::{fs, io::Write as _, os::unix::fs::OpenOptionsExt as _, path::Path};

fn main() {
    let secret_path = std::env::args().nth(1).expect("usage: release-key SECRET_KEY_FILE");
    let public_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/minisign.pub");
    let pair = minisign::KeyPair::generate_unencrypted_keypair().expect("key pair");
    let secret = pair.sk.to_box(Some("Anything release key")).expect("secret key").to_string();
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&secret_path)
        .unwrap_or_else(|e| panic!("{secret_path}: {e} (an existing key is never overwritten)"));
    file.write_all(secret.as_bytes()).expect("write secret key");
    fs::write(&public_path, pair.pk.to_box().expect("public key").to_string()).expect("write public key");
    println!("secret key: {secret_path}");
    println!("public key: packaging/minisign.pub");
}
