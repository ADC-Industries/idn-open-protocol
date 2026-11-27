//! The signing key on a YubiKey (5.2.3 or later): its OpenPGP card's signing slot, Ed25519,
//! made on the key and never leaving it; each signature needs the PIN and a touch.

use card_backend_pcsc::PcscBackend;
use openpgp_card::Card;
use openpgp_card::ocard::KeyType;
use openpgp_card::ocard::algorithm::AlgoSimple;
use openpgp_card::ocard::crypto::{PublicKeyMaterial, SigningAlgo};
use openpgp_card::ocard::data::{Fingerprint, KeyGenerationTime, TouchPolicy};
use secrecy::SecretString;
use std::io;

fn err(e: impl std::fmt::Display) -> io::Error {
    let t = e.to_string();
    // PC/SC with no reader at all (Windows starts its Smart Card service when a key is
    // plugged in; Linux needs pcscd).
    if t.contains("resource manager is not running") || t.contains("No readers") {
        return io::Error::other(
            "no YubiKey found: plug it in (on Linux, also install and start pcscd)",
        );
    }
    io::Error::other(t)
}

/// The OpenPGP cards plugged in: `(ident, its manufacturer)` — a YubiKey is `0006:…`.
pub fn list() -> io::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for b in PcscBackend::card_backends(None).map_err(err)?.flatten() {
        let Ok(mut c) = Card::new(b) else { continue };
        let tx = c.transaction().map_err(err)?;
        let aid = tx.application_identifier().map_err(err)?;
        out.push((aid.ident(), aid.manufacturer_name().to_owned()));
    }
    Ok(out)
}

/// The card `ident`, or the only one plugged in.
fn open(ident: Option<&str>) -> io::Result<Card<openpgp_card::state::Open>> {
    let cards = PcscBackend::card_backends(None).map_err(err)?;
    match ident {
        Some(i) => Card::open_by_ident(cards, i).map_err(err),
        None => {
            let all: Vec<_> = cards.flatten().collect();
            if all.len() != 1 {
                return Err(err(if all.is_empty() {
                    "no YubiKey (OpenPGP card) plugged in".to_owned()
                } else {
                    format!(
                        "{} cards plugged in: say which (--card IDENT; card-status lists them)",
                        all.len()
                    )
                }));
            }
            Card::new(all.into_iter().next().expect("one")).map_err(err)
        }
    }
}

/// The 32 bytes of an Ed25519 public key as the card gives it (with or without the 0x40).
fn ed25519(p: &PublicKeyMaterial) -> io::Result<[u8; 32]> {
    let PublicKeyMaterial::E(e) = p else {
        return Err(err("the card's signing key isn't Ed25519"));
    };
    let d = e.data();
    let d = if d.len() == 33 && d[0] == 0x40 {
        &d[1..]
    } else {
        d
    };
    d.try_into()
        .map_err(|_| err("the card's signing key isn't Ed25519"))
}

/// The OpenPGP v4 fingerprint of an EdDSA key (RFC 4880 §12.2), which the card keeps.
fn fingerprint(
    p: &PublicKeyMaterial,
    t: KeyGenerationTime,
    _: KeyType,
) -> Result<Fingerprint, openpgp_card::Error> {
    let k = ed25519(p).map_err(|e| openpgp_card::Error::InternalError(e.to_string()))?;
    let mut body = vec![4u8];
    body.extend_from_slice(&t.get().to_be_bytes());
    body.push(22); // EdDSA
    body.extend_from_slice(&[9, 0x2B, 0x06, 0x01, 0x04, 0x01, 0xDA, 0x47, 0x0F, 0x01]);
    body.extend_from_slice(&263u16.to_be_bytes()); // 0x40 and 32 bytes, in bits
    body.push(0x40);
    body.extend_from_slice(&k);
    let mut pkt = vec![0x99];
    pkt.extend_from_slice(&(body.len() as u16).to_be_bytes());
    pkt.extend_from_slice(&body);
    let d = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, &pkt);
    Ok(Fingerprint::from(
        <[u8; 20]>::try_from(d.as_ref()).expect("20 bytes"),
    ))
}

/// What a card has: its ident, firmware, and its signing key (public, hex) with whether it
/// needs a touch.
pub fn status(ident: Option<&str>) -> io::Result<String> {
    let mut c = open(ident)?;
    let mut tx = c.transaction().map_err(err)?;
    let aid = tx.application_identifier().map_err(err)?;
    let fw = tx.firmware_version().unwrap_or_default();
    let key = tx
        .public_key_material(KeyType::Signing)
        .ok()
        .and_then(|p| ed25519(&p).ok())
        .map(|k| idn_common::hex::hex(&k));
    let touch = tx
        .user_interaction_flag(KeyType::Signing)
        .ok()
        .flatten()
        .map(|u| format!("{:?}", u.touch_policy()))
        .unwrap_or_else(|| "?".into());
    Ok(format!(
        "{} ({}), firmware {}\n  signing key: {}\n  touch: {touch}",
        aid.ident(),
        aid.manufacturer_name(),
        fw.iter().map(u8::to_string).collect::<Vec<_>>().join("."),
        key.unwrap_or_else(|| "none (or not Ed25519)".into()),
    ))
}

/// Firmware 5.2.3 or later (Ed25519 in the OpenPGP card).
fn check_firmware(fw: &[u8]) -> io::Result<()> {
    if fw.len() >= 3 && (fw[0], fw[1], fw[2]) < (5, 2, 3) {
        return Err(err(format!(
            "this YubiKey's firmware is {}.{}.{}: Ed25519 signing needs 5.2.3 or later",
            fw[0], fw[1], fw[2]
        )));
    }
    Ok(())
}

/// Makes a new Ed25519 signing key on the card (replacing any there), sets it to need a
/// touch for every signature, and returns its public key.
pub fn keygen(ident: Option<&str>, admin_pin: String) -> io::Result<[u8; 32]> {
    let mut c = open(ident)?;
    let mut tx = c.transaction().map_err(err)?;
    check_firmware(&tx.firmware_version().unwrap_or_default())?;
    let mut admin = tx
        .as_admin_card(SecretString::from(admin_pin))
        .map_err(|e| err(format!("the admin PIN: {e}")))?;
    admin
        .set_algorithm(KeyType::Signing, AlgoSimple::Curve25519)
        .map_err(err)?;
    let (public, _) = admin
        .generate_key(fingerprint, KeyType::Signing)
        .map_err(err)?;
    admin
        .set_touch_policy(KeyType::Signing, TouchPolicy::On)
        .map_err(|e| {
            err(format!(
                "the key is made, but its touch couldn't be set: {e}"
            ))
        })?;
    ed25519(&public)
}

/// Changes the card's PIN (`admin` false) or admin PIN (true): the old and the new.
pub fn change_pin(ident: Option<&str>, admin: bool, old: String, new: String) -> io::Result<()> {
    let mut c = open(ident)?;
    let mut tx = c.transaction().map_err(err)?;
    let (old, new) = (SecretString::from(old), SecretString::from(new));
    if admin {
        tx.change_admin_pin(old, new)
    } else {
        tx.change_user_pin(old, new)
    }
    .map_err(|e| err(format!("changing the PIN: {e}")))
}

/// The card's signing key (public).
pub fn public(ident: Option<&str>) -> io::Result<[u8; 32]> {
    let mut c = open(ident)?;
    let mut tx = c.transaction().map_err(err)?;
    ed25519(&tx.public_key_material(KeyType::Signing).map_err(err)?)
}

/// Signs `digest` on the card with its PIN (the YubiKey then waits for its touch).
pub fn sign(ident: Option<&str>, pin: String, digest: &[u8; 32]) -> io::Result<Vec<u8>> {
    let mut c = open(ident)?;
    let mut tx = c.transaction().map_err(err)?;
    tx.verify_user_signing_pin(SecretString::from(pin))
        .map_err(|e| err(format!("the PIN: {e}")))?;
    eprintln!("idn-release: touch your YubiKey (it blinks)");
    tx.card()
        .signature_for_hash(SigningAlgo::ECC, digest)
        .map_err(|e| err(format!("signing: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_and_fingerprint() {
        assert!(check_firmware(&[5, 2, 3]).is_ok());
        assert!(check_firmware(&[5, 7, 1]).is_ok());
        assert!(check_firmware(&[5, 1, 2]).is_err());
        assert!(check_firmware(&[4, 4, 5]).is_err());
        // RFC 4880's fingerprint shape: 20 bytes, the same for the same key and time.
        use openpgp_card::ocard::algorithm::{AlgorithmAttributes, Curve, EccAttributes};
        use openpgp_card::ocard::crypto::EccType;
        let attrs =
            AlgorithmAttributes::Ecc(EccAttributes::new(EccType::EdDSA, Curve::Ed25519, None));
        let p = PublicKeyMaterial::E(openpgp_card::ocard::crypto::EccPub::new(
            vec![7u8; 32],
            attrs.clone(),
        ));
        let p40 = PublicKeyMaterial::E(openpgp_card::ocard::crypto::EccPub::new(
            [vec![0x40], vec![7u8; 32]].concat(),
            attrs,
        ));
        assert_eq!(ed25519(&p).unwrap(), [7u8; 32]);
        assert_eq!(ed25519(&p40).unwrap(), [7u8; 32]);
    }
}
