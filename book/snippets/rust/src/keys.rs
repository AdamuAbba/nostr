use nostr_sdk::prelude::*;

// ANCHOR: generate
pub fn generate() -> Result<()> {
    let keys = Keys::generate();

    let public_key = keys.public_key();
    let secret_key = keys.secret_key();

    println!("Public key (hex): {}", public_key);
    println!("Secret key (hex): {}", secret_key.to_secret_hex());

    println!("Public key (bech32): {}", public_key.to_bech32()?);
    println!("Secret key (bech32): {}", secret_key.to_bech32()?);

    Ok(())
}
// ANCHOR_END: generate

// ANCHOR: restore
pub fn restore() -> Result<()> {
    // Parse keys directly from secret key
    let keys = Keys::parse("nsec1j4c6269y9w0q2er2xjw8sv2ehyrtfxq3jwgdlxj6qfn8z4gjsq5qfvfk99")?;

    // Parse secret key and construct keys
    let secret_key =
        SecretKey::parse("nsec1j4c6269y9w0q2er2xjw8sv2ehyrtfxq3jwgdlxj6qfn8z4gjsq5qfvfk99")?;
    let keys = Keys::new(secret_key);

    // Restore from bech32
    let secret_key =
        SecretKey::from_bech32("nsec1j4c6269y9w0q2er2xjw8sv2ehyrtfxq3jwgdlxj6qfn8z4gjsq5qfvfk99")?;
    let keys = Keys::new(secret_key);

    // Restore from hex
    let secret_key =
        SecretKey::from_hex("6b911fd37cdf5c81d4c0adb1ab7fa822ed253ab0ad9aa18d77257c88b29b718e")?;
    let keys = Keys::new(secret_key);

    Ok(())
}
// ANCHOR_END: restore
