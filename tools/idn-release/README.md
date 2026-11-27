# idn-release

Makes and checks the signed release files the IDN Site Manager hands to machines
(spec/extensions/idn-node.md §N.23). The signing key is on a YubiKey (5.2.3 or later):
made there, never leaving it; each signature needs its PIN and a touch.

## Until the YubiKeys come: a key file

`make-file-key.cmd` (or `idn-release keygen E:\idn-release.key`): a key in a file on a USB
stick, sealed with a passphrase (12+ characters); it prints the public key for
`release-keys.txt` (`key-public FILE` prints it again, no passphrase needed). To sign,
drag the key file onto a release folder's `sign.cmd` (or `make --key FILE`): it asks the
passphrase. Keep the stick unplugged and locked away, the passphrase written down
elsewhere. Once both YubiKeys are listed, its key comes off `release-keys.txt`.

## Once, for each YubiKey (a main one and a spare)

On the Windows PC, with the YubiKey plugged in, from a release folder (or any folder with
`idn-release.exe`):

1. `make-key.cmd` — or by hand:
   - `idn-release card-status`: the YubiKey is seen (its firmware must be 5.2.3 or later).
   - `idn-release card-pins`: change the PIN (factory 123456, 6+ characters) and the
     admin PIN (factory 12345678, 8+). Three wrong PINs lock it (the admin PIN unlocks);
     three wrong admin PINs and the OpenPGP part must be reset (its key is then gone).
   - `idn-release card-keygen`: a new signing key on the YubiKey (one there is replaced).
     It prints the public key: 64 hex characters.
2. Put both YubiKeys' public keys in `release-keys.txt` and build: every program from then
   on installs releases signed by either. Keep the spare locked away.

## Each release

1. `scripts/package.sh TAG`, then `scripts/release.sh TAG "What is new"`: the folder
   `Installers/idn-release-VER-TAG/` (the Linux programs, the Windows installer,
   `idn-release.exe`, `sign.cmd`, `make-key.cmd`).
2. Copy it to the Windows PC; plug in the YubiKey; run `sign.cmd`: its PIN, then touch the
   YubiKey when it blinks. Out: `idn-VER-TAG.idnrelease`.
3. Upload it on the Site Manager's Updates card.

`idn-release show FILE` checks a release against this build's keys. `--key KEYFILE` (and
`keygen KEYFILE`) sign with a key in a file instead — for tests only.

Linux builds need the PC/SC library (`libpcsclite`; to build, `PCSC_LIB_DIR` may point at
a folder with `libpcsclite.so`); running there needs `pcscd`.
