//! Signs a release archive, as the release workflow does:
//!
//!     MINISIGN_SECRET_KEY="$(cat key)" cargo run -p anything-update --example sign -- FILE
//!
//! Writes `FILE.minisig` with the file's name as the trusted comment, then
//! checks it against the key built into Anything.

use std::path::Path;

fn main() {
    let path = std::env::args().nth(1).expect("usage: sign FILE");
    let name = Path::new(&path).file_name().and_then(|n| n.to_str()).expect("file name").to_owned();
    let secret = std::env::var("MINISIGN_SECRET_KEY").expect("MINISIGN_SECRET_KEY is not set");
    let sk = minisign::SecretKeyBox::from_string(&secret)
        .and_then(|b| b.into_unencrypted_secret_key())
        .expect("MINISIGN_SECRET_KEY is not an unencrypted minisign secret key");
    let data = std::fs::read(&path).expect("read file");
    let signature = minisign::sign(None, &sk, std::io::Cursor::new(&data), Some(&name), None).expect("sign");
    let signature = signature.to_string();
    std::fs::write(format!("{path}.minisig"), &signature).expect("write signature");
    anything_update::verify(&data, &signature, anything_update::PUBLIC_KEY, &name)
        .unwrap_or_else(|e| panic!("the signature does not verify with packaging/minisign.pub: {e}"));
    println!("signed {name}");
}
