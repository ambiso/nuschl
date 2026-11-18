// SPDX-License-Identifier: EUPL-1.2
use super::*;

const CHEAP: Cost = Cost {
    memory_kib: 64,
    passes: 1,
};

#[test]
fn hashes_verify_and_differ_by_salt() {
    let first = hash_now("correct horse", CHEAP).unwrap();
    let second = hash_now("correct horse", CHEAP).unwrap();
    assert!(first.starts_with("$argon2id$v=19$m=64,t=1,p=1$"), "{first}");
    assert_ne!(first, second);
    assert!(verify_now("correct horse", &first));
    assert!(!verify_now("correct horsf", &first));
    assert!(!verify_now("correct horse", "not a hash"));
    assert!(!verify_now(&"x".repeat(MAX_PASSWORD_BYTES + 1), &first));
    assert!(hash_now(&"x".repeat(MAX_PASSWORD_BYTES + 1), CHEAP).is_none());
}

#[test]
fn murmur_hashes_verify() {
    // PBKDF2-HMAC-SHA384 as Murmur's `PBKDF2::getHash` computes it (8-byte
    // hex salt, 48-byte hex output). The vector is from Python's
    // hashlib.pbkdf2_hmac("sha384", b"hunter2", bytes.fromhex("0011223344556677"), 1000).
    let stored = murmur_pbkdf2(
        1000,
        "0011223344556677",
        "c2fa5023e808bd1adafccf5f995d5147a0d17e0113e9bda3fa8380faa58088467346ba1eb6bbf7e6374da78caa9b784c",
    )
    .unwrap();
    assert!(is_imported(&stored));
    assert!(verify_now("hunter2", &stored));
    assert!(!verify_now("hunter3", &stored));
    let derived = [7_u8; 48];
    // SHA-1 of "password", as `getLegacySHA1Hash` stores it.
    let legacy = murmur_sha1("5baa61e4c9b93f3f0682250b6cf8331b7ee68fd8").unwrap();
    assert!(verify_now("password", &legacy));
    assert!(!verify_now("Password", &legacy));
    // Malformed or too expensive rows are refused at import.
    assert!(murmur_sha1("5baa61e4").is_none());
    assert!(murmur_pbkdf2(0, "00", &hex::encode(derived)).is_none());
    assert!(
        murmur_pbkdf2(
            i64::from(MAX_MURMUR_ITERATIONS) + 1,
            "00",
            &hex::encode(derived)
        )
        .is_none()
    );
    assert!(murmur_pbkdf2(1000, "zz", &hex::encode(derived)).is_none());
    assert!(!verify_now("x", "$murmur-pbkdf2-sha384$i=1$00"));
}

#[test]
fn other_algorithms_are_refused() {
    let argon2i = Argon2::new(
        Algorithm::Argon2i,
        Version::V0x13,
        Params::new(64, 1, 1, None).unwrap(),
    );
    let salt = SaltString::encode_b64(&[1; 16]).unwrap();
    let weak = argon2i.hash_password(b"pw", &salt).unwrap().to_string();
    assert!(!verify_now("pw", &weak));
}
