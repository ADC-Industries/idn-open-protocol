//! Signed releases (G10, idn-node.md §N.23): one file that carries a version's packages
//! for each system, signed with ADC Industries' release key, which is kept offline. The
//! Site Manager keeps the file (a [`Store`]) and hands each machine its package; every
//! machine checks the signature itself against the keys built into it (`release-keys.txt`)
//! and the package against the signed manifest before installing it
//! ([`crate::update::fetch_package`]).
//!
//! The file: `IDNRELEASE/1\n`, the manifest's length (u32, big-endian), the manifest (JSON:
//! `version`, `notes`, `created`, `key`, `product` (`media`, `industrial`, or another
//! product's own, such as `example-player`: a machine installs only its own product's),
//! `track` (`stable`, the default, or `beta`), `packages`: `[{platform, name, size,
//! sha256}]`), the Ed25519 signature (64 bytes) over the SHA-256 digest of the magic line
//! and the manifest (32 bytes: what a YubiKey signs), then the packages one after another,
//! in the manifest's order.

use idn_common::sync::LockOk as _;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The file's first line.
pub const MAGIC: &[u8] = b"IDNRELEASE/1\n";
/// IDN Media's releases.
pub const MEDIA: &str = "media";
/// IDN Industrial's releases.
pub const INDUSTRIAL: &str = "industrial";
/// The longest manifest taken.
const MAX_MANIFEST: u32 = 1 << 20;

fn bad(m: impl std::fmt::Display) -> io::Error {
    idn_common::err::invalid_data(format!("release: {m}"))
}

/// A caller's or an uploader's mistake (a version that can't be a file's name, a file that
/// isn't a release this store takes): a panel answers it as the person's to fix.
fn invalid(m: impl std::fmt::Display) -> io::Error {
    idn_common::err::invalid_input(format!("release: {m}"))
}

/// This build's system, as a release names its packages: `windows-x86_64`,
/// `linux-x86_64`, `linux-aarch64`, `macos-aarch64`, ….
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The keys this build trusts (`release-keys.txt`), and any a test added.
pub fn trusted_keys() -> Vec<[u8; 32]> {
    let mut k = parse_keys(include_str!("../../../release-keys.txt"));
    k.extend(TEST_KEYS.lock_ok().iter().copied());
    k
}

static TEST_KEYS: std::sync::Mutex<Vec<[u8; 32]>> = std::sync::Mutex::new(Vec::new());

/// Tests only: trusts one more key in this process (a release made with a key of the
/// test's own). The programs never call it.
#[doc(hidden)]
pub fn trust_for_tests(key: [u8; 32]) {
    TEST_KEYS.lock_ok().push(key);
}

fn parse_keys(text: &str) -> Vec<[u8; 32]> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| idn_common::hex::unhex(l)?.try_into().ok())
        .collect()
}

/// One system's package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// `windows-x86_64` (the installer, run with `/S /UPDATE`), `linux-x86_64` (a
    /// `.tar.gz` of the programs), ….
    pub platform: String,
    /// Its file's name as it was given.
    pub name: String,
    /// Bytes.
    pub size: u64,
    /// SHA-256, hex.
    pub sha256: String,
}

/// What a release says of itself (signed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The version, as the release machine named it (`0.3.0-rv9`).
    pub version: String,
    /// What is new, for the Site Manager's Updates page.
    pub notes: String,
    /// Unix seconds.
    pub created: u64,
    /// The public key that signed it, hex.
    pub key: String,
    /// The product it is of: `media` (IDN Media), `industrial` (IDN Industrial) or another
    /// product's own ([`is_host_product`]). Each product's programs install only their own.
    pub product: String,
    /// A package for each system.
    pub packages: Vec<Package>,
    /// Its release track: `stable` (manifests without one are) or
    /// `beta`. A machine on the stable track takes stable releases only.
    pub track: String,
}

/// The stable release track (every machine takes it).
pub const STABLE: &str = "stable";
/// The beta release track (machines set to it only).
pub const BETA: &str = "beta";

/// Whether a release of `track` may go to a machine on `machine_track`.
pub fn track_fits(track: &str, machine_track: &str) -> bool {
    track == STABLE || machine_track == BETA
}

/// In a [`Store::products`] list: every product that isn't IDN's own
/// ([`is_host_product`]: those the program hosting the IDN Node installs),
/// whatever its name.
pub const HOST_PRODUCTS: &str = "*host";

/// A product that isn't IDN's own (`example-player`, …): installed by the program hosting
/// the IDN Node (its launcher), handed the package (one release
/// system for IDN and the programs that host it).
pub fn is_host_product(product: &str) -> bool {
    product != MEDIA && product != INDUSTRIAL
}

impl Manifest {
    /// As the file keeps it.
    pub fn to_json(&self) -> Value {
        let track = if self.track.is_empty() {
            STABLE
        } else {
            self.track.as_str()
        };
        json!({
            "version": self.version, "notes": self.notes, "created": self.created, "key": self.key,
            "product": self.product, "track": track,
            "packages": self.packages.iter().map(|p| json!({"platform": p.platform, "name": p.name, "size": p.size, "sha256": p.sha256})).collect::<Vec<_>>(),
        })
    }

    /// From [`Manifest::to_json`].
    pub fn from_json(v: &Value) -> io::Result<Manifest> {
        let s = |k: &str| {
            v[k].as_str()
                .map(str::to_owned)
                .ok_or_else(|| bad(format!("no {k}")))
        };
        let packages = v["packages"]
            .as_array()
            .ok_or_else(|| bad("no packages"))?
            .iter()
            .map(|p| {
                Ok(Package {
                    platform: p["platform"]
                        .as_str()
                        .ok_or_else(|| bad("a package's platform"))?
                        .to_owned(),
                    name: p["name"].as_str().unwrap_or("").to_owned(),
                    size: p["size"].as_u64().ok_or_else(|| bad("a package's size"))?,
                    sha256: p["sha256"]
                        .as_str()
                        .ok_or_else(|| bad("a package's sha256"))?
                        .to_owned(),
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Manifest {
            version: s("version")?,
            notes: v["notes"].as_str().unwrap_or("").to_owned(),
            created: v["created"].as_u64().unwrap_or(0),
            key: s("key")?,
            product: s("product").and_then(|p| {
                if p.is_empty() {
                    Err(bad("an empty product"))
                } else {
                    Ok(p)
                }
            })?,
            track: match v["track"].as_str() {
                None | Some("") | Some(STABLE) => STABLE.to_owned(),
                Some(BETA) => BETA.to_owned(),
                Some(t) => return Err(bad(format!("track {t:?}: stable or beta"))),
            },
            packages,
        })
    }

    /// This system's package.
    pub fn package(&self, platform: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.platform == platform)
    }
}

/// A key file with a passphrase: `IDNRELEASEKEY/1\n`, the public key (32 bytes, so it can be
/// read without the passphrase), a 16-byte salt, a 12-byte nonce, then the PKCS#8 key
/// sealed with ChaCha20-Poly1305 under Argon2id of the passphrase (256 MiB, 3 passes: a
/// stolen file is slow to guess at).
const KEY_MAGIC: &[u8] = b"IDNRELEASEKEY/1\n";

fn file_key(passphrase: &str, salt: &[u8]) -> io::Result<ring::aead::LessSafeKey> {
    let params = argon2::Params::new(256 * 1024, 3, 1, Some(32)).map_err(bad)?;
    let mut k = [0u8; 32];
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, &mut k)
        .map_err(bad)?;
    Ok(ring::aead::LessSafeKey::new(
        ring::aead::UnboundKey::new(&ring::aead::CHACHA20_POLY1305, &k).expect("a 32-byte key"),
    ))
}

/// Makes a signing key in a file (readable by this account only), sealed with `passphrase`
/// if given (plain PKCS#8 otherwise: tests), and returns its public key (for
/// `release-keys.txt`). Never over an existing file.
pub fn keygen(path: &Path, passphrase: Option<&str>) -> io::Result<[u8; 32]> {
    let rng = ring::rand::SystemRandom::new();
    let doc = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| bad("making the key"))?;
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the key file is there already",
        ));
    }
    let k = Ed25519KeyPair::from_pkcs8(doc.as_ref()).map_err(|_| bad("the new key"))?;
    let public: [u8; 32] = k.public_key().as_ref().try_into().expect("32 bytes");
    let bytes = match passphrase {
        None => doc.as_ref().to_vec(),
        Some(p) => {
            let salt = crate::random::secure_random::<16>();
            let nonce = crate::random::secure_random::<12>();
            let mut sealed = doc.as_ref().to_vec();
            file_key(p, &salt)?
                .seal_in_place_append_tag(
                    ring::aead::Nonce::assume_unique_for_key(nonce),
                    ring::aead::Aad::from(KEY_MAGIC),
                    &mut sealed,
                )
                .map_err(|_| bad("sealing the key"))?;
            [KEY_MAGIC, &public, &salt, &nonce, &sealed].concat()
        }
    };
    idn_common::fsutil::write_private(path, &bytes)?;
    Ok(public)
}

/// Whether a key file needs a passphrase, and its public key (read without one).
pub fn key_file_info(path: &Path) -> io::Result<(bool, [u8; 32])> {
    let b = std::fs::read(path)?;
    if let Some(rest) = b.strip_prefix(KEY_MAGIC) {
        let public: [u8; 32] = rest
            .get(..32)
            .and_then(|p| p.try_into().ok())
            .ok_or_else(|| bad("the key file is cut short"))?;
        return Ok((true, public));
    }
    let k = Ed25519KeyPair::from_pkcs8(&b).map_err(|_| bad("not a release key file"))?;
    Ok((false, k.public_key().as_ref().try_into().expect("32 bytes")))
}

/// The key in a file, opened with its passphrase if it has one.
fn load_key(path: &Path, passphrase: Option<&str>) -> io::Result<Ed25519KeyPair> {
    let b = std::fs::read(path)?;
    let Some(rest) = b.strip_prefix(KEY_MAGIC) else {
        return Ed25519KeyPair::from_pkcs8(&b)
            .map_err(|_| bad("the key file isn't an Ed25519 key"));
    };
    let p = passphrase.ok_or_else(|| bad("the key file has a passphrase"))?;
    if rest.len() < 32 + 16 + 12 + 16 {
        return Err(bad("the key file is cut short"));
    }
    let (salt, nonce) = (&rest[32..48], &rest[48..60]);
    let mut sealed = rest[60..].to_vec();
    let plain = file_key(p, salt)?
        .open_in_place(
            ring::aead::Nonce::try_assume_unique_for_key(nonce).map_err(|_| bad("the key file"))?,
            ring::aead::Aad::from(KEY_MAGIC),
            &mut sealed,
        )
        .map_err(|_| bad("wrong passphrase (or the key file is damaged)"))?;
    let k =
        Ed25519KeyPair::from_pkcs8(plain).map_err(|_| bad("the key file isn't an Ed25519 key"))?;
    if k.public_key().as_ref() != &rest[..32] {
        return Err(bad("the key file's two halves don't match"));
    }
    Ok(k)
}

/// What a release's signature covers: SHA-256 of the magic line and the manifest.
pub fn signed_digest(manifest: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(MAGIC);
    h.update(manifest);
    h.finalize().into()
}

/// Writes a release of `packages` (platform, file) signed with the key in `key_file` (its
/// passphrase if it has one).
pub fn make(
    key_file: &Path,
    passphrase: Option<&str>,
    product: &str,
    version: &str,
    notes: &str,
    packages: &[(String, PathBuf)],
    out: &Path,
) -> io::Result<Manifest> {
    let key = load_key(key_file, passphrase)?;
    let public: [u8; 32] = key.public_key().as_ref().try_into().expect("32 bytes");
    make_signed(
        public,
        &mut |digest| Ok(key.sign(digest).as_ref().to_vec()),
        product,
        version,
        notes,
        packages,
        out,
    )
}

/// [`make`] with the signature made elsewhere (a YubiKey): `sign` gets the 32-byte digest
/// and gives its Ed25519 signature by the key whose public half is `public`. The signature
/// is checked before the file is written.
pub fn make_signed(
    public: [u8; 32],
    sign: &mut dyn FnMut(&[u8; 32]) -> io::Result<Vec<u8>>,
    product: &str,
    version: &str,
    notes: &str,
    packages: &[(String, PathBuf)],
    out: &Path,
) -> io::Result<Manifest> {
    if version.trim().is_empty() || packages.is_empty() {
        return Err(bad("a version and at least one package"));
    }
    if product != MEDIA && product != INDUSTRIAL {
        return Err(bad(format!(
            "a product is {MEDIA} or {INDUSTRIAL}, not {product:?}"
        )));
    }
    let mut list = Vec::new();
    for (platform, path) in packages {
        if list.iter().any(|p: &Package| &p.platform == platform) {
            return Err(bad(format!("two packages for {platform}")));
        }
        let (size, sha256) = hash_file(path)?;
        list.push(Package {
            platform: platform.clone(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            size,
            sha256,
        });
    }
    // `product@beta`: a release on the beta track.
    let (product, track) = match product.split_once('@') {
        Some((p, BETA)) => (p, BETA),
        Some((_, t)) => return Err(bad(format!("track {t:?}: stable or beta"))),
        None => (product, STABLE),
    };
    let m = Manifest {
        version: version.trim().to_owned(),
        notes: notes.to_owned(),
        created: idn_common::epoch::now().as_secs(),
        key: idn_common::hex::hex(&public),
        product: product.to_owned(),
        packages: list,
        track: track.to_owned(),
    };
    let mbytes = m.to_json().to_string().into_bytes();
    let sig = sign(&signed_digest(&mbytes))?;
    UnparsedPublicKey::new(&ED25519, public)
        .verify(&signed_digest(&mbytes), &sig)
        .map_err(|_| bad("the signature made doesn't match the key"))?;
    // Written beside `out` and put in its place only once every package copied is the
    // one signed: a package changed during the PIN and the touch (a build still writing
    // it) was signed over other bytes, and `out` the same as a package was emptied before
    // it was read (bug round E, E5-8).
    let canon = |p: &Path| std::fs::canonicalize(p).ok();
    if packages
        .iter()
        .any(|(_, p)| canon(p).is_some() && canon(p) == canon(out))
    {
        return Err(bad("the release file can't be one of its packages"));
    }
    let mut tmp_name = out.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".part");
    let tmp = out.with_file_name(tmp_name);
    let written = (|| -> io::Result<()> {
        let mut o = io::BufWriter::new(File::create(&tmp)?);
        o.write_all(MAGIC)?;
        o.write_all(&(mbytes.len() as u32).to_be_bytes())?;
        o.write_all(&mbytes)?;
        o.write_all(sig.as_ref())?;
        let mut buf = vec![0u8; 1 << 20];
        for ((_, path), p) in packages.iter().zip(&m.packages) {
            let mut f = File::open(path)?;
            let (mut h, mut size) = (Sha256::new(), 0u64);
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                size += n as u64;
                o.write_all(&buf[..n])?;
            }
            if size != p.size || idn_common::hex::hex(&h.finalize()) != p.sha256 {
                return Err(bad(format!(
                    "{} changed while the release was signed: make it again",
                    p.name
                )));
            }
        }
        o.flush()?;
        o.get_ref().sync_all()
    })();
    match written {
        Ok(()) => std::fs::rename(&tmp, out)?,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    Ok(m)
}

/// Checks a manifest's signature against `keys`: the manifest, if it is signed by one.
pub fn verify(manifest: &[u8], signature: &[u8], keys: &[[u8; 32]]) -> io::Result<Manifest> {
    let m = Manifest::from_json(&serde_json::from_slice(manifest).map_err(bad)?)?;
    let key: [u8; 32] = idn_common::hex::unhex(&m.key)
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| bad("its key"))?;
    if !keys.contains(&key) {
        return Err(bad(if keys.is_empty() {
            "this build trusts no release key yet (release-keys.txt)".to_owned()
        } else {
            format!(
                "signed with a key this build doesn't trust ({}…)",
                &m.key[..16]
            )
        }));
    }
    UnparsedPublicKey::new(&ED25519, key)
        .verify(&signed_digest(manifest), signature)
        .map_err(|_| bad("the signature doesn't match: changed, or not ours"))?;
    Ok(m)
}

/// A release file, its signature checked.
#[derive(Debug, Clone)]
pub struct Release {
    /// What it says of itself.
    pub manifest: Manifest,
    /// The manifest as signed (for a machine to check it itself).
    pub manifest_bytes: Vec<u8>,
    /// Its signature (Ed25519, 64 bytes).
    pub signature: Vec<u8>,
    path: PathBuf,
    data_start: u64,
}

impl Release {
    /// Opens `path`, checking the signature against `keys` and the file's length.
    pub fn open(path: &Path, keys: &[[u8; 32]]) -> io::Result<Release> {
        let mut f = File::open(path)?;
        let mut magic = vec![0u8; MAGIC.len()];
        f.read_exact(&mut magic)?;
        if magic != MAGIC {
            return Err(bad("not an IDN release"));
        }
        let mut n = [0u8; 4];
        f.read_exact(&mut n)?;
        let n = u32::from_be_bytes(n);
        if n > MAX_MANIFEST {
            return Err(bad("a manifest too long"));
        }
        let mut manifest_bytes = vec![0u8; n as usize];
        f.read_exact(&mut manifest_bytes)?;
        let mut signature = vec![0u8; 64];
        f.read_exact(&mut signature)?;
        let manifest = verify(&manifest_bytes, &signature, keys)?;
        let data_start = (MAGIC.len() + 4 + n as usize + 64) as u64;
        let total: u64 = manifest.packages.iter().map(|p| p.size).sum();
        if f.metadata()?.len() != data_start + total {
            return Err(bad("cut short, or more than its packages"));
        }
        Ok(Release {
            manifest,
            manifest_bytes,
            signature,
            path: path.to_owned(),
            data_start,
        })
    }

    /// Where `platform`'s package starts in the file, and its size.
    pub fn span(&self, platform: &str) -> Option<(u64, u64)> {
        let mut at = self.data_start;
        for p in &self.manifest.packages {
            if p.platform == platform {
                return Some((at, p.size));
            }
            at += p.size;
        }
        None
    }

    /// Up to `len` bytes of `platform`'s package from `offset` (a machine fetches it in
    /// pieces).
    pub fn read_package(&self, platform: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let (start, size) = self
            .span(platform)
            .ok_or_else(|| bad(format!("no package for {platform}")))?;
        if offset >= size {
            return Ok(Vec::new());
        }
        let n = (size - offset).min(len as u64) as usize;
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(start + offset))?;
        let mut b = vec![0u8; n];
        f.read_exact(&mut b)?;
        Ok(b)
    }
}

/// A file's size and SHA-256 (hex), read in pieces (a package is too large to hold whole).
fn hash_file(path: &Path) -> io::Result<(u64, String)> {
    let mut f = File::open(path)?;
    let (mut h, mut size, mut buf) = (Sha256::new(), 0u64, vec![0u8; 1 << 20]);
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        size += n as u64;
    }
    Ok((size, idn_common::hex::hex(&h.finalize())))
}

/// Checks a downloaded package against its manifest entry (size and SHA-256).
pub fn check_package(path: &Path, p: &Package) -> io::Result<()> {
    let (size, sha256) = hash_file(path)?;
    if size != p.size || sha256 != p.sha256 {
        return Err(bad(
            "the package doesn't match its release (changed, or cut short)",
        ));
    }
    Ok(())
}

/// Whether `version` may name a release's file: 1 to 60 of letters, digits, `.`, `-`, `_`
/// and `+`, not starting with `.` (no folder, nothing hidden, the same on every system),
/// and not a Windows device name before its first dot (`CON`, `PRN`, `AUX`, `NUL`,
/// `COM0`–`COM9`, `LPT0`–`LPT9`, any case: `nul.idnrelease` is the null device there, not
/// a file).
pub fn safe_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 60
        && !version.starts_with('.')
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-_+".contains(c))
        && !windows_device(version.split('.').next().unwrap_or(version))
}

/// Whether `stem` (a name before its first dot) is a Windows device name, any case. (The
/// superscript-digit forms, `COM¹`, can't reach here: [`safe_version`] takes ASCII only.)
fn windows_device(stem: &str) -> bool {
    let s = stem.to_ascii_uppercase();
    matches!(s.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (s.len() == 4
            && (s.starts_with("COM") || s.starts_with("LPT"))
            && s.as_bytes()[3].is_ascii_digit())
}

/// The releases a Site Manager keeps, one file each: `DIR/<version>.idnrelease`. Every
/// file is checked as it comes in ([`Store::add`]) and again each time it is opened: signed
/// by one of the store's keys, whole, of a product the store takes ([`Store::products`]), and
/// holding the version its name says (a file renamed by hand is refused, not offered under
/// another version). The machines fetch their packages from it in pieces
/// ([`Store::piece`]). IDN Media's Site Manager and IDN Industrial's use it.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
    keys: Vec<[u8; 32]>,
    /// The products taken ([`Store::products`]); `None`: any.
    products: Option<Vec<String>>,
    max_piece: usize,
    max_size: u64,
}

impl Store {
    /// The largest piece sent unless [`Store::max_piece`] says otherwise (IDN Media's panel
    /// takes up to 5 MB a request).
    pub const PIECE: usize = 4 << 20;
    /// The largest release taken unless [`Store::max_size`] says otherwise (its owner sets
    /// its own: IDN Media's Site Manager takes more, for the host products' releases).
    pub const MAX_SIZE: u64 = 1 << 30;
    /// The upload [`Store::upload_piece`] writes, in the store's folder.
    pub const UPLOAD: &'static str = "upload.part";

    /// The releases in `dir` (made when the first is added), checked against this build's
    /// keys ([`trusted_keys`]), of any product.
    pub fn new(dir: impl Into<PathBuf>) -> Store {
        Store {
            dir: dir.into(),
            keys: trusted_keys(),
            products: None,
            max_piece: Self::PIECE,
            max_size: Self::MAX_SIZE,
        }
    }

    /// Checked against `keys` instead (tests, with a key of their own).
    pub fn with_keys(mut self, keys: Vec<[u8; 32]>) -> Store {
        self.keys = keys;
        self
    }

    /// Takes releases of `product` only (IDN Industrial's Site Manager: [`INDUSTRIAL`]):
    /// [`Store::products`] with one.
    pub fn only_product(self, product: &str) -> Store {
        self.products(&[product])
    }

    /// Takes releases of these products only, each a product's name or [`HOST_PRODUCTS`]
    /// (IDN Media's Site Manager: [`MEDIA`] and [`HOST_PRODUCTS`]); others are refused as
    /// they are uploaded, and one kept by hand isn't opened or offered.
    pub fn products(mut self, products: &[&str]) -> Store {
        self.products = Some(products.iter().map(|p| (*p).to_owned()).collect());
        self
    }

    /// The largest piece [`Store::piece`] sends (at least 1 byte).
    pub fn max_piece(mut self, bytes: usize) -> Store {
        self.max_piece = bytes.max(1);
        self
    }

    /// The largest release file taken (an upload, a file added); [`Store::MAX_SIZE`]
    /// unless set.
    pub fn max_size(mut self, bytes: u64) -> Store {
        self.max_size = bytes;
        self
    }

    /// The folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where `version`'s file is (or would be); `None` for a version that can't be a
    /// file's name ([`safe_version`]).
    pub fn path(&self, version: &str) -> Option<PathBuf> {
        safe_version(version).then(|| self.dir.join(format!("{version}.idnrelease")))
    }

    /// `version`'s release, checked. Not one: `InvalidInput` for a name that can't be a
    /// version, `NotFound` for none kept, `InvalidData` for a file that doesn't check out.
    pub fn open(&self, version: &str) -> io::Result<Release> {
        let p = self
            .path(version)
            .ok_or_else(|| invalid("not a version (letters, digits and . - _ +)"))?;
        let r = Release::open(&p, &self.keys)?;
        if r.manifest.version != version {
            return Err(bad(format!(
                "{} holds {}, not {version}",
                p.display(),
                r.manifest.version
            )));
        }
        // (Kept here by hand: a file that doesn't check out, not the caller's mistake.)
        self.fits(&r.manifest)
            .map_err(idn_common::err::invalid_data)?;
        Ok(r)
    }

    /// Whether this store takes a release of `m`'s product.
    fn fits(&self, m: &Manifest) -> io::Result<()> {
        let Some(list) = &self.products else {
            return Ok(());
        };
        let takes =
            |p: &String| *p == m.product || (p == HOST_PRODUCTS && is_host_product(&m.product));
        if list.iter().any(takes) {
            return Ok(());
        }
        let names: Vec<&str> = list
            .iter()
            .map(|p| {
                if p == HOST_PRODUCTS {
                    "a host product"
                } else {
                    p.as_str()
                }
            })
            .collect();
        let hint = match m.product.as_str() {
            INDUSTRIAL => " (IDN Industrial's releases go to the Site Manager | Industrial)",
            MEDIA => " (IDN Media's releases go to IDN Media's Site Manager)",
            _ => "",
        };
        Err(invalid(format!(
            "a release of {}, not of {}: this Site Manager doesn't take it{hint}",
            m.product,
            names.join(" or ")
        )))
    }

    /// The releases kept that check out, newest first (by their signed time; the same time
    /// by version). A file that doesn't check out is left out of the list, not removed.
    pub fn list(&self) -> Vec<Release> {
        let mut v: Vec<Release> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().into_string().ok()?;
                self.open(n.strip_suffix(".idnrelease")?).ok()
            })
            .collect();
        v.sort_by(|a, b| {
            (b.manifest.created, &b.manifest.version)
                .cmp(&(a.manifest.created, &a.manifest.version))
        });
        v
    }

    /// The newest release kept of `product` that a machine on `track` may take.
    pub fn newest_for(&self, product: &str, track: &str) -> Option<Release> {
        self.list()
            .into_iter()
            .find(|r| r.manifest.product == product && track_fits(&r.manifest.track, track))
    }

    /// Takes the release file at `from` (an upload, in the same file system): kept as its
    /// version's file (one there already is replaced) if it checks out. Refused (removed,
    /// `InvalidInput` with the reason): not signed by one of the store's keys, damaged,
    /// larger than [`Store::max_size`], another product's, or a version that can't be a
    /// file's name. Its manifest.
    pub fn add(&self, from: &Path) -> io::Result<Manifest> {
        let checked = (|| -> io::Result<(Manifest, PathBuf)> {
            if std::fs::metadata(from)?.len() > self.max_size {
                return Err(invalid(format!("larger than {} MB", self.max_size >> 20)));
            }
            let r = Release::open(from, &self.keys).map_err(idn_common::err::invalid_input)?;
            self.fits(&r.manifest)?;
            let to = self.path(&r.manifest.version).ok_or_else(|| {
                invalid(format!(
                    "its version {:?} can't be a file's name (letters, digits and . - _ +)",
                    r.manifest.version
                ))
            })?;
            Ok((r.manifest, to))
        })();
        let (m, to) = match checked {
            Ok(x) => x,
            Err(e) => {
                let _ = std::fs::remove_file(from);
                return Err(e);
            }
        };
        std::fs::create_dir_all(&self.dir)?;
        std::fs::rename(from, &to)?;
        Ok(m)
    }

    /// Removes `version`'s file: `false` when there was none.
    pub fn remove(&self, version: &str) -> io::Result<bool> {
        let p = self
            .path(version)
            .ok_or_else(|| invalid("not a version (letters, digits and . - _ +)"))?;
        match std::fs::remove_file(p) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Up to `len` bytes (at most [`Store::max_piece`], at least 1) of `version`'s package
    /// for `platform` from `offset`, for a machine fetching it ([`crate::update::fetch_package`]).
    /// Who may ask is the caller's to check.
    pub fn piece(
        &self,
        version: &str,
        platform: &str,
        offset: u64,
        len: usize,
    ) -> io::Result<Vec<u8>> {
        self.open(version)?
            .read_package(platform, offset, len.clamp(1, self.max_piece))
    }

    /// A piece of an upload ([`Store::UPLOAD`]) at `offset`: 0 starts it again; any other
    /// must be where the upload ends (pieces in order only). [`Store::upload_done`] takes it.
    pub fn upload_piece(&self, offset: u64, body: &[u8]) -> io::Result<()> {
        if offset.saturating_add(body.len() as u64) > self.max_size {
            return Err(invalid(format!(
                "a release is at most {} MB",
                self.max_size >> 20
            )));
        }
        std::fs::create_dir_all(&self.dir)?;
        let part = self.dir.join(Self::UPLOAD);
        let mut f = if offset == 0 {
            File::create(&part)?
        } else {
            std::fs::OpenOptions::new().write(true).open(&part)?
        };
        if f.metadata()?.len() != offset {
            return Err(invalid(
                "the upload's pieces came out of order: start again",
            ));
        }
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(body)
    }

    /// The upload whole: [`Store::add`]ed.
    pub fn upload_done(&self) -> io::Result<Manifest> {
        self.add(&self.dir.join(Self::UPLOAD))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use idn_common::testing::TempDir;

    /// Every key line in release-keys.txt is a key (a typo would drop it without a word).
    #[test]
    fn release_keys_all_parse() {
        let text = include_str!("../../../release-keys.txt");
        let lines = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .count();
        assert_eq!(trusted_keys().len(), lines);
    }

    #[test]
    fn signed_released_and_checked() {
        let dir = TempDir::new("release");
        let key = dir.join("release.key");
        let public = keygen(&key, None).unwrap();
        assert!(keygen(&key, None).is_err(), "never over an existing key");
        let (win, lin) = (dir.join("idn-setup.exe"), dir.join("idn.tar.gz"));
        std::fs::write(&win, vec![7u8; 300_000]).unwrap();
        std::fs::write(&lin, b"linux programs").unwrap();
        let out = dir.join("idn-0.3.0.idnrelease");
        let m = make(
            &key,
            None,
            MEDIA,
            "0.3.0",
            "Simple mode, starters",
            &[
                ("windows-x86_64".into(), win.clone()),
                ("linux-x86_64".into(), lin.clone()),
            ],
            &out,
        )
        .unwrap();
        assert_eq!(m.packages[0].size, 300_000);
        // Trusted: opened; each package where the manifest says.
        let r = Release::open(&out, &[public]).unwrap();
        assert_eq!(r.manifest, m);
        assert_eq!(
            r.read_package("linux-x86_64", 0, 1 << 20).unwrap(),
            b"linux programs"
        );
        assert_eq!(r.read_package("linux-x86_64", 6, 3).unwrap(), b"pro");
        let mut got = Vec::new();
        for off in (0..300_000).step_by(65_536) {
            got.extend(r.read_package("windows-x86_64", off, 65_536).unwrap());
        }
        let back = dir.join("got.exe");
        std::fs::write(&back, &got).unwrap();
        check_package(&back, m.package("windows-x86_64").unwrap()).unwrap();
        // A machine checks the manifest itself.
        verify(&r.manifest_bytes, &r.signature, &[public]).unwrap();
        // Not trusted: another key, or none.
        let other = keygen(&dir.join("other.key"), None).unwrap();
        assert!(
            Release::open(&out, &[other])
                .unwrap_err()
                .to_string()
                .contains("doesn't trust")
        );
        assert!(
            Release::open(&out, &[])
                .unwrap_err()
                .to_string()
                .contains("no release key")
        );
        // Changed: the manifest (a byte of it), a package, cut short.
        let mut b = std::fs::read(&out).unwrap();
        let at = MAGIC.len() + 4 + 20;
        b[at] ^= 1;
        std::fs::write(dir.join("x1"), &b).unwrap();
        assert!(Release::open(&dir.join("x1"), &[public]).is_err());
        let mut b = std::fs::read(&out).unwrap();
        let last = b.len() - 1;
        b[last] ^= 1;
        std::fs::write(dir.join("x2"), &b).unwrap();
        let r2 = Release::open(&dir.join("x2"), &[public]).unwrap();
        let lin2 = dir.join("lin2");
        std::fs::write(&lin2, r2.read_package("linux-x86_64", 0, 1 << 20).unwrap()).unwrap();
        assert!(check_package(&lin2, m.package("linux-x86_64").unwrap()).is_err());
        b.pop();
        std::fs::write(dir.join("x3"), &b).unwrap();
        assert!(Release::open(&dir.join("x3"), &[public]).is_err());
        // A key file with a passphrase: its public key without it; signing only with it.
        let sealed = dir.join("sealed.key");
        let sp = keygen(&sealed, Some("a long passphrase")).unwrap();
        assert_eq!(key_file_info(&sealed).unwrap(), (true, sp));
        assert_eq!(key_file_info(&key).unwrap(), (false, public));
        let pk = [("linux-x86_64".to_owned(), lin.clone())];
        let o2 = dir.join("sealed.idnrelease");
        assert!(
            make(&sealed, None, MEDIA, "0.3.1", "", &pk, &o2)
                .unwrap_err()
                .to_string()
                .contains("passphrase")
        );
        assert!(
            make(&sealed, Some("wrong"), MEDIA, "0.3.1", "", &pk, &o2)
                .unwrap_err()
                .to_string()
                .contains("wrong passphrase")
        );
        make(
            &sealed,
            Some("a long passphrase"),
            MEDIA,
            "0.3.1",
            "",
            &pk,
            &o2,
        )
        .unwrap();
        Release::open(&o2, &[sp]).unwrap();
        // The built-in list's form.
        assert_eq!(
            parse_keys(&format!(
                "# a comment\n\n{}\nzz\n",
                idn_common::hex::hex(&public)
            )),
            vec![public]
        );
        assert!(platform().contains('-'));
    }

    #[test]
    fn a_release_says_its_product_and_track() {
        let m = Manifest {
            version: "0.3.0".into(),
            notes: String::new(),
            created: 1,
            key: "00".repeat(32),
            product: MEDIA.into(),
            packages: vec![],
            track: STABLE.into(),
        };
        assert_eq!(m.to_json()["product"], MEDIA);
        assert_eq!(m.to_json()["track"], STABLE);
        assert_eq!(Manifest::from_json(&m.to_json()).unwrap(), m);
        let i = Manifest {
            product: INDUSTRIAL.into(),
            track: BETA.into(),
            ..m.clone()
        };
        assert_eq!(Manifest::from_json(&i.to_json()).unwrap(), i);
        // A manifest that names no product is refused (none is taken as IDN Media's).
        let mut v = m.to_json();
        v.as_object_mut().unwrap().remove("product");
        assert!(Manifest::from_json(&v).is_err());
        v["product"] = json!("");
        assert!(Manifest::from_json(&v).is_err());
        // No track: stable.
        let mut v = m.to_json();
        v.as_object_mut().unwrap().remove("track");
        assert_eq!(Manifest::from_json(&v).unwrap().track, STABLE);
    }

    /// A release of `product`/`version` with one Linux package, made in `dir`: its path.
    fn a_release(dir: &Path, key: &Path, product: &str, version: &str) -> PathBuf {
        let pkg = dir.join(format!("pkg-{version}"));
        std::fs::write(&pkg, format!("programs of {version}")).unwrap();
        let out = dir.join(format!("made-{version}"));
        make(
            key,
            None,
            product,
            version,
            "",
            &[("linux-x86_64".into(), pkg)],
            &out,
        )
        .unwrap();
        out
    }

    #[test]
    fn versions_as_file_names() {
        for v in [
            "0.3.0",
            "0.3.0-rv9",
            "1.0.0+build.5",
            "v2_beta",
            "0.2.0-Ind4",
            // Only like a device name: a longer stem, or one after the first dot.
            "CONSOLE",
            "com10",
            "COMX",
            "LPT",
            "1.0.nul",
            "v1.CON",
        ] {
            assert!(safe_version(v), "{v}");
        }
        let long = "1".repeat(61);
        for v in [
            "",
            "../x",
            "a/b",
            ".hidden",
            "a b",
            "x\\y",
            "é",
            long.as_str(),
            // Windows device names, any case, as the stem before the first dot.
            "CON",
            "con",
            "Prn.1",
            "aux.2.0",
            "NUL",
            "nul.idnrelease",
            "COM1",
            "com9.0",
            "COM0",
            "lpt1",
            "LPT9.1.0",
            "Lpt0",
            "COM¹",
        ] {
            assert!(!safe_version(v), "{v}");
        }
    }

    #[test]
    fn a_store_keeps_checked_releases_by_version() {
        let dir = TempDir::new("release-store");
        let key = dir.join("release.key");
        let public = keygen(&key, None).unwrap();
        let store = Store::new(dir.join("releases"))
            .with_keys(vec![public])
            .max_piece(4);
        assert!(store.list().is_empty());
        // Added: kept as its version's file, the upload gone.
        let up = a_release(&dir, &key, MEDIA, "0.3.0");
        assert_eq!(store.add(&up).unwrap().version, "0.3.0");
        assert!(!up.exists() && store.path("0.3.0").unwrap().is_file());
        let r = store.open("0.3.0").unwrap();
        assert_eq!(r.manifest.product, MEDIA);
        // Pieces: at most the store's largest.
        assert_eq!(
            store.piece("0.3.0", "linux-x86_64", 0, 100).unwrap(),
            b"prog"
        );
        assert_eq!(store.piece("0.3.0", "linux-x86_64", 4, 0).unwrap(), b"r");
        assert!(store.piece("0.3.0", "windows-x86_64", 0, 4).is_err());
        assert_eq!(
            store
                .piece("0.9.9", "linux-x86_64", 0, 4)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        // Newest first (made in the same second: by version); the newest for a product
        // and track.
        store.add(&a_release(&dir, &key, MEDIA, "0.3.1")).unwrap();
        let versions: Vec<String> = store
            .list()
            .into_iter()
            .map(|r| r.manifest.version)
            .collect();
        assert_eq!(versions, ["0.3.1", "0.3.0"]);
        assert_eq!(
            store.newest_for(MEDIA, STABLE).unwrap().manifest.version,
            "0.3.1"
        );
        assert!(store.newest_for(INDUSTRIAL, STABLE).is_none());
        // A file renamed by hand: not offered under the name it was given.
        std::fs::copy(store.path("0.3.0").unwrap(), store.path("0.4.0").unwrap()).unwrap();
        assert!(
            store
                .open("0.4.0")
                .unwrap_err()
                .to_string()
                .contains("not 0.4.0")
        );
        assert_eq!(store.list().len(), 2);
        // Names that leave the folder: refused before any file is touched.
        for v in ["../x", "a/b", ""] {
            assert_eq!(
                store.open(v).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{v}"
            );
            assert!(store.remove(v).is_err() && store.path(v).is_none(), "{v}");
        }
        // Removed: once.
        assert!(store.remove("0.4.0").unwrap());
        assert!(!store.remove("0.4.0").unwrap());
    }

    #[test]
    fn a_store_refuses_what_doesnt_check_out() {
        let dir = TempDir::new("release-store-refused");
        let key = dir.join("release.key");
        let public = keygen(&key, None).unwrap();
        let store = Store::new(dir.join("releases"))
            .with_keys(vec![public])
            .only_product(INDUSTRIAL);
        // Another product's: refused, the upload removed.
        let up = a_release(&dir, &key, MEDIA, "0.3.0");
        let e = store.add(&up).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert!(e.to_string().contains("not of industrial"), "{e}");
        assert!(!up.exists());
        // Signed with a key the store doesn't trust.
        let other = dir.join("other.key");
        keygen(&other, None).unwrap();
        let up = a_release(&dir, &other, INDUSTRIAL, "0.3.0");
        assert_eq!(
            store.add(&up).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!up.exists());
        // Changed after it was signed (its last byte).
        let up = a_release(&dir, &key, INDUSTRIAL, "0.3.0");
        let mut b = std::fs::read(&up).unwrap();
        *b.last_mut().unwrap() ^= 1;
        std::fs::write(&up, &b).unwrap();
        store.add(&up).unwrap();
        // (The package's bytes are checked by the machine, against the signed manifest.)
        let piece = store.piece("0.3.0", "linux-x86_64", 0, 100).unwrap();
        let got = dir.join("got");
        std::fs::write(&got, piece).unwrap();
        let m = store.open("0.3.0").unwrap().manifest;
        assert!(check_package(&got, m.package("linux-x86_64").unwrap()).is_err());
        // Larger than the store takes.
        let small = store.clone().max_size(10);
        let up = a_release(&dir, &key, INDUSTRIAL, "0.3.1");
        assert!(small.add(&up).unwrap_err().to_string().contains("larger"));
        assert!(!up.exists());
        // A kept file of another product (put there by hand): not opened, not listed.
        let media = a_release(&dir, &key, MEDIA, "0.5.0");
        std::fs::rename(&media, store.path("0.5.0").unwrap()).unwrap();
        assert_eq!(
            store.open("0.5.0").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn a_store_takes_the_products_it_lists() {
        let dir = TempDir::new("release-store-products");
        let key = dir.join("release.key");
        let public = keygen(&key, None).unwrap();
        // IDN Media's Site Manager: its own and the host products', never Industrial's.
        let store = Store::new(dir.join("releases"))
            .with_keys(vec![public])
            .products(&[MEDIA, HOST_PRODUCTS]);
        let up = a_release(&dir, &key, INDUSTRIAL, "0.3.0");
        let e = store.add(&up).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert!(e.to_string().contains("Site Manager | Industrial"), "{e}");
        assert!(!up.exists());
        store.add(&a_release(&dir, &key, MEDIA, "0.3.1")).unwrap();
        // A host product's (named by its maker, not by IDN).
        let host = a_release(&dir, &key, MEDIA, "0.3.2");
        let mut r = Release::open(&host, &[public]).unwrap().manifest;
        r.product = "example-player".into();
        assert!(store.fits(&r).is_ok());
        r.product = INDUSTRIAL.into();
        assert!(store.fits(&r).is_err());
        // One store, one product: the host products not taken unless listed.
        let only = store.clone().only_product(MEDIA);
        r.product = "example-player".into();
        assert!(only.fits(&r).is_err());
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn a_store_takes_uploads_in_order() {
        let dir = TempDir::new("release-store-upload");
        let key = dir.join("release.key");
        let public = keygen(&key, None).unwrap();
        let store = Store::new(dir.join("releases")).with_keys(vec![public]);
        let part = store.dir().join(Store::UPLOAD);
        store.upload_piece(0, b"abc").unwrap();
        assert!(store.upload_piece(10, b"x").is_err());
        store.upload_piece(3, b"def").unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), b"abcdef");
        // Not a release: refused, and gone.
        assert!(store.upload_done().is_err());
        assert!(!part.exists());
        // A release in pieces.
        let b = std::fs::read(a_release(&dir, &key, MEDIA, "0.3.0")).unwrap();
        for (i, c) in b.chunks(100).enumerate() {
            store.upload_piece(i as u64 * 100, c).unwrap();
        }
        assert_eq!(store.upload_done().unwrap().version, "0.3.0");
        assert!(store.open("0.3.0").is_ok() && !part.exists());
        // Larger than the store takes: refused before it is written.
        let small = store.clone().max_size(5);
        assert!(small.upload_piece(0, b"123456").is_err());
    }
}
