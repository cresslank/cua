// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.
//! Modern NSS uses a 32-byte AES SDR key. The historical Firefox writer assumes
//! 3DES and truncates it, producing fields that pass its own decryptor but fail
//! the real browser. Keep the old Firefox provider unchanged; Zen admits the
//! actual destination SDR key's algorithm and verifies it in the contained lane.
use super::*;
use base64::Engine as _;
use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use rand::RngCore;

pub(super) fn install(
    profile: &Path,
    items: &[LoginItem],
    record: &mut ImportRecord,
) -> Result<usize> {
    if !profile.join("key4.db").exists() {
        return crate::firefox_items::install_logins(profile, items, record);
    }
    let db = rusqlite::Connection::open(profile.join("key4.db"))
        .map_err(|_| fail("cannot open staged destination NSS key"))?;
    let (salt, check): (Vec<u8>, Vec<u8>) = db
        .query_row(
            "SELECT item1,item2 FROM metaData WHERE id='password'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| fail("destination NSS password entry is missing"))?;
    let (sealed, id): (Vec<u8>, Vec<u8>) = db.query_row(
        "SELECT a11,a102 FROM nssPrivate WHERE a102=x'f8000000000000000000000000000001' AND a11 IS NOT NULL", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|_| fail("destination NSS SDR key identity is unsupported"))?;
    let master = cua_teleport_bundle::firefox_nss::master_key(&salt, &check, &sealed, b"")?;
    drop(db);
    if !matches!(master.len(), 24 | 32) {
        return Err(fail("unsupported destination NSS SDR key length"));
    }
    let path = profile.join("logins.json");
    let mut doc: Value = if path.exists() {
        json(&fs::read(&path)?)?
    } else {
        serde_json::json!({"nextId":1,"logins":[],"version":3})
    };
    let mut next = doc["nextId"]
        .as_i64()
        .ok_or_else(|| fail("corrupt destination login IDs"))?;
    let now = crate::ledger::now_ms();
    let mut rng = rand::rngs::OsRng;
    for item in items {
        let mut guid = [0u8; 16];
        rng.fill_bytes(&mut guid);
        guid[6] = (guid[6] & 15) | 64;
        guid[8] = (guid[8] & 63) | 128;
        let h = hex::encode(guid);
        let guid = format!(
            "{{{}-{}-{}-{}-{}}}",
            &h[..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..]
        );
        let entry = serde_json::json!({"id":next,"hostname":item.origin,"httpRealm":null,"formSubmitURL":item.origin,
            "usernameField":"","passwordField":"","encryptedUsername":encrypt(&master,&id,item.username.as_bytes(),&mut rng)?,
            "encryptedPassword":encrypt(&master,&id,&item.password,&mut rng)?,"guid":guid,"encType":1,
            "timeCreated":now,"timeLastUsed":now,"timePasswordChanged":now,"timesUsed":1});
        next = next
            .checked_add(1)
            .ok_or_else(|| fail("destination login IDs exhausted"))?;
        doc["logins"]
            .as_array_mut()
            .ok_or_else(|| fail("corrupt destination login array"))?
            .push(entry);
    }
    doc["nextId"] = next.into();
    fs::write(&path, serde_json::to_vec(&doc)?)?;
    record.file_written(&path);
    Ok(items.len())
}

fn tlv(tag: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    if data.len() < 128 {
        out.push(data.len() as u8);
    } else {
        let bytes = data.len().to_be_bytes();
        let start = bytes
            .iter()
            .position(|b| *b != 0)
            .expect("positive DER length");
        out.push(0x80 | (bytes.len() - start) as u8);
        out.extend_from_slice(&bytes[start..]);
    }
    out.extend_from_slice(data);
    out
}

fn encrypt(master: &[u8], id: &[u8], plain: &[u8], rng: &mut impl RngCore) -> Result<String> {
    if master.len() == 24 {
        let mut iv = [0u8; 8];
        rng.fill_bytes(&mut iv);
        return cua_teleport_bundle::firefox_nss::encrypt_sdr(master, id, &iv, plain);
    }
    let mut iv = [0u8; 16];
    rng.fill_bytes(&mut iv);
    let cipher = cbc::Encryptor::<aes::Aes256>::new_from_slices(master, &iv)
        .map_err(|_| fail("invalid destination AES SDR key"))?
        .encrypt_padded_vec_mut::<Pkcs7>(plain);
    let alg = [
        tlv(6, &[0x60, 0x86, 0x48, 1, 0x65, 3, 4, 1, 0x2a]),
        tlv(4, &iv),
    ]
    .concat();
    let body = [tlv(4, id), tlv(0x30, &alg), tlv(4, &cipher)].concat();
    Ok(base64::engine::general_purpose::STANDARD.encode(tlv(0x30, &body)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdr_algorithms_round_trip_include_long_der_lengths() {
        let plain = vec![b'x'; 1000];
        for len in [24, 32] {
            let master = vec![0x23; len];
            let encrypted = encrypt(&master, &[0xf8; 16], &plain, &mut rand::rngs::OsRng).unwrap();
            let decrypted =
                cua_teleport_bundle::firefox_nss::decrypt_sdr(&master, &encrypted).unwrap();
            assert_eq!(&*decrypted, &plain);
        }
    }
}
