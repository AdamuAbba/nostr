from nostr_sdk import Keys, SecretKey


# ANCHOR: generate
def generate():
    keys = Keys.generate()

    public_key = keys.public_key()
    secret_key = keys.secret_key()

    print(f"Public key (hex): {public_key.to_hex()}")
    print(f"Secret key (hex): {secret_key.to_hex()}")

    print(f"Public key (bech32): {public_key.to_bech32()}")
    print(f"Secret key (bech32): {secret_key.to_bech32()}")
# ANCHOR_END: generate

# ANCHOR: restore
def restore():
    keys = Keys.parse("nsec1j4c6269y9w0q2er2xjw8sv2ehyrtfxq3jwgdlxj6qfn8z4gjsq5qfvfk99")

    secret_key = SecretKey.parse("6b911fd37cdf5c81d4c0adb1ab7fa822ed253ab0ad9aa18d77257c88b29b718e")
    keys = Keys(secret_key)
# ANCHOR_END: restore
