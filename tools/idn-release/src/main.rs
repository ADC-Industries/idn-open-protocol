//! idn-release: makes and checks signed IDN releases (G10, idn-node.md §N.23), with the
//! signing key on a YubiKey (or, for tests, in a file). For ADC Industries' release
//! machine; not for churches.

mod card;

use idn_common::hex::hex;
use idn_open_protocol::release;
use std::path::PathBuf;

/// A PIN typed without showing it (or from the environment variable, for scripts).
fn pin(prompt: &str, var: &str) -> String {
    if !var.is_empty()
        && let Some(p) = idn_common::switches::value(var)
    {
        return p;
    }
    rpassword::prompt_password(prompt).unwrap_or_else(|e| fail(&format!("reading the PIN: {e}")))
}

const USAGE: &str = "usage: idn-release card-status [--card IDENT]
       idn-release card-pins [--card IDENT]
       idn-release card-keygen [--card IDENT]
       idn-release make (--card [IDENT] | --key KEYFILE) --version VERSION [--notes TEXT]
                        [--product media|industrial] --package PLATFORM=FILE [--package …] OUT.idnrelease
       idn-release show FILE.idnrelease [--key HEX]
       idn-release keygen KEYFILE [--no-passphrase]
       idn-release key-public KEYFILE

  card-status lists the YubiKeys (OpenPGP cards) plugged in and their signing keys.
  card-pins changes the YubiKey's PIN (6+ characters) and admin PIN (8+): do it first,
  away from the factory 123456 and 12345678.
  card-keygen makes a new signing key on the YubiKey (Ed25519; it never leaves the key;
  each signature needs its PIN and a touch) and prints its public key for
  release-keys.txt: asks for the YubiKey's admin PIN (IDN_CARD_ADMIN_PIN). make signs the
  packages (PLATFORM: windows-x86_64 for the installer, linux-x86_64 for a .tar.gz of
  the programs) into one release file, on the YubiKey (asks for its PIN, IDN_CARD_PIN;
  then touch it), or with a key file (--key: asks its passphrase, IDN_KEY_PASSPHRASE).
  keygen makes a key in a file sealed with a passphrase (until a YubiKey; --no-passphrase:
  tests only) and prints its public key; key-public prints it again (no passphrase needed).
  show checks a release against this build's keys (or --key).";

fn fail(m: &str) -> ! {
    eprintln!("idn-release: {m}");
    std::process::exit(1)
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    match a.first().map(String::as_str) {
        // A key in a file, sealed with a passphrase (until the YubiKeys come; --no-passphrase:
        // tests only).
        Some("keygen") if (2..=3).contains(&a.len()) => {
            let plain = a.iter().any(|x| x == "--no-passphrase");
            let path = a[1..]
                .iter()
                .find(|x| !x.starts_with("--"))
                .unwrap_or_else(|| fail(USAGE));
            let pass = (!plain).then(|| {
                let p = pin(
                    "A passphrase for the key file (12 characters or more): ",
                    "",
                );
                if p.chars().count() < 12 {
                    fail("the passphrase: 12 characters or more");
                }
                if pin("The passphrase again: ", "") != p {
                    fail("the two passphrases differ");
                }
                p
            });
            match release::keygen(&PathBuf::from(path), pass.as_deref()) {
                Ok(k) => {
                    println!("{}", hex(&k));
                    eprintln!(
                        "idn-release: the key is in {path}{}; the public key above goes in release-keys.txt",
                        if plain {
                            " (no passphrase: for tests only)"
                        } else {
                            ", sealed with the passphrase"
                        }
                    );
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Some("key-public") if a.len() == 2 => match release::key_file_info(&PathBuf::from(&a[1])) {
            Ok((sealed, k)) => {
                println!("{}", hex(&k));
                eprintln!(
                    "idn-release: {}",
                    if sealed {
                        "a passphrase protects it"
                    } else {
                        "no passphrase"
                    }
                );
            }
            Err(e) => fail(&e.to_string()),
        },
        Some("card-status") => {
            let ident = a
                .iter()
                .position(|x| x == "--card")
                .and_then(|i| a.get(i + 1))
                .map(String::as_str);
            match card::list() {
                Ok(l) if l.is_empty() => fail("no YubiKey (OpenPGP card) plugged in"),
                Ok(l) if ident.is_none() => {
                    for (id, _) in &l {
                        match card::status(Some(id)) {
                            Ok(s) => println!("{s}"),
                            Err(e) => println!("{id}: {e}"),
                        }
                    }
                }
                Ok(_) => println!(
                    "{}",
                    card::status(ident).unwrap_or_else(|e| fail(&e.to_string()))
                ),
                Err(e) => fail(&e.to_string()),
            }
        }
        Some("card-pins") => {
            let ident = a
                .iter()
                .position(|x| x == "--card")
                .and_then(|i| a.get(i + 1))
                .map(String::as_str);
            for (admin, what, min, factory) in [
                (false, "PIN", 6, "123456"),
                (true, "admin PIN", 8, "12345678"),
            ] {
                let old = pin(
                    &format!("The YubiKey's {what} now (factory {factory}): "),
                    "",
                );
                let new = pin(&format!("Its new {what} ({min} characters or more): "), "");
                let again = pin(&format!("The new {what} again: "), "");
                if new != again {
                    fail(&format!("the two new {what}s differ: nothing changed"));
                }
                if new.chars().count() < min {
                    fail(&format!("the {what}: {min} characters or more"));
                }
                card::change_pin(ident, admin, old, new).unwrap_or_else(|e| fail(&e.to_string()));
                eprintln!("idn-release: the {what} is changed");
            }
        }
        Some("card-keygen") => {
            let ident = a
                .iter()
                .position(|x| x == "--card")
                .and_then(|i| a.get(i + 1))
                .map(String::as_str);
            eprintln!(
                "idn-release: this makes a NEW signing key on the YubiKey; one there now is gone for good.\n\
                 Releases signed with it would no longer install anywhere it isn't trusted. Type yes to go on:"
            );
            let mut yes = String::new();
            let _ = std::io::stdin().read_line(&mut yes);
            if yes.trim() != "yes" {
                fail("nothing changed");
            }
            let admin = pin(
                "The YubiKey's admin PIN (OpenPGP; factory 12345678): ",
                "IDN_CARD_ADMIN_PIN",
            );
            match card::keygen(ident, admin) {
                Ok(k) => {
                    println!("{}", hex(&k));
                    eprintln!(
                        "idn-release: the key is made on the YubiKey; each signature needs its PIN and a touch.\n\
                         The public key above goes in release-keys.txt."
                    );
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Some("make") => {
            let (mut key, mut version, mut notes, mut packages, mut out) =
                (None, None, String::new(), Vec::new(), None);
            // IDN Media's by default; IDN Industrial's machines take only `industrial`.
            let mut product = release::MEDIA.to_owned();
            // --card [IDENT]: the YubiKey (the only one plugged in, or that one).
            let mut on_card: Option<Option<String>> = None;
            let mut i = 1;
            while i < a.len() {
                let v = || a.get(i + 1).cloned().unwrap_or_else(|| fail(USAGE));
                match a[i].as_str() {
                    "--key" => key = Some(PathBuf::from(v())),
                    "--card" => {
                        match a
                            .get(i + 1)
                            .filter(|x| !x.starts_with('-') && !x.ends_with(".idnrelease"))
                        {
                            Some(id) => {
                                on_card = Some(Some(id.clone()));
                                i += 2;
                            }
                            None => {
                                on_card = Some(None);
                                i += 1;
                            }
                        }
                        continue;
                    }
                    "--version" => version = Some(v()),
                    "--notes" => notes = v(),
                    "--product" => product = v(),
                    "--package" => {
                        let p = v();
                        let (pl, f) = p
                            .split_once('=')
                            .unwrap_or_else(|| fail("--package PLATFORM=FILE"));
                        packages.push((pl.to_owned(), PathBuf::from(f)));
                    }
                    o if !o.starts_with('-') && out.is_none() => {
                        out = Some(PathBuf::from(o));
                        i += 1;
                        continue;
                    }
                    _ => fail(USAGE),
                }
                i += 2;
            }
            let (Some(version), Some(out)) = (version, out) else {
                fail(USAGE)
            };
            let made = match (key, on_card) {
                (Some(key), None) => {
                    let (sealed, public) =
                        release::key_file_info(&key).unwrap_or_else(|e| fail(&e.to_string()));
                    if !release::trusted_keys().contains(&public) {
                        eprintln!(
                            "idn-release: note: this key ({}…) isn't in this build's release-keys.txt",
                            &hex(&public)[..16]
                        );
                    }
                    let pass =
                        sealed.then(|| pin("The key file's passphrase: ", "IDN_KEY_PASSPHRASE"));
                    release::make(
                        &key,
                        pass.as_deref(),
                        &product,
                        &version,
                        &notes,
                        &packages,
                        &out,
                    )
                }
                (None, Some(ident)) => {
                    let ident = ident.as_deref();
                    let public = card::public(ident).unwrap_or_else(|e| fail(&e.to_string()));
                    if !release::trusted_keys().contains(&public) {
                        eprintln!(
                            "idn-release: note: this YubiKey's key ({}…) isn't in this build's release-keys.txt",
                            &hex(&public)[..16]
                        );
                    }
                    let p = pin(
                        "The YubiKey's PIN (OpenPGP; factory 123456): ",
                        "IDN_CARD_PIN",
                    );
                    release::make_signed(
                        public,
                        &mut |digest| card::sign(ident, p.clone(), digest),
                        &product,
                        &version,
                        &notes,
                        &packages,
                        &out,
                    )
                }
                _ => fail("give --card (a YubiKey) or --key KEYFILE"),
            };
            match made {
                Ok(m) => {
                    println!(
                        "{} {} ({}): {}",
                        out.display(),
                        m.version,
                        m.product,
                        m.packages
                            .iter()
                            .map(|p| format!("{} ({} bytes)", p.platform, p.size))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Some("show") if a.len() >= 2 => {
            let keys = match a.iter().position(|x| x == "--key") {
                Some(k) => {
                    let h = a.get(k + 1).unwrap_or_else(|| fail(USAGE));
                    let b: Vec<u8> = (0..h.len())
                        .step_by(2)
                        .filter_map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
                        .collect();
                    vec![
                        b.try_into()
                            .unwrap_or_else(|_| fail("--key: 64 hex digits")),
                    ]
                }
                None => release::trusted_keys(),
            };
            match release::Release::open(&PathBuf::from(&a[1]), &keys) {
                Ok(r) => {
                    let m = &r.manifest;
                    println!("{} (signed by {}…), {}", m.version, &m.key[..16], m.notes);
                    for p in &m.packages {
                        println!(
                            "  {}: {} ({} bytes, sha256 {})",
                            p.platform, p.name, p.size, p.sha256
                        );
                    }
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2)
        }
    }
}
