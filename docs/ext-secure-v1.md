# Extension: secure transport (`x_idn_secure`, version 1) — DRAFT

Status: design draft, not implemented. Depends on spec §1.9.1 and §4.4: how stock NDI
peers treat unknown attributes, elements and ALPNs. Nothing here changes behaviour
towards stock NDI peers.

## 1. Goals

The NDI wire protocol has no security. The control channel is obfuscated with keyless
scrambling (spec §1.3), and QUIC media runs with packet protection disabled (§4.2). This
extension adds, between two peers that both implement it:

- **confidentiality and integrity** of control messages, metadata and media;
- **authentication** of both peers;
- **a policy switch** that refuses unauthenticated peers, so that an attacker cannot simply
  strip the extension (downgrade).

Non-goals: access control within a site beyond "member of the site or not"; hiding *that*
a source exists (discovery stays as it is).

## 2. Compatibility rule

Every element of this extension is invisible to a peer that doesn't advertise it:

- New attributes use the prefix `x_idn_`. NDI peers ignore unknown attributes (§1.9,
  "SHOULD ignore"; confirmed in §1.9.1).
- A peer uses an extension feature only after the other side has advertised the same
  feature. Without that, the session is plain NDI, exactly as the spec describes.

## 3. Trust model — DECISION NEEDED

| Option | How peers authenticate | Pros | Cons |
|---|---|---|---|
| **A. Site CA** (recommended) | Each device has a certificate issued by a site-local CA; peers require certificates chained to that CA, in mutual TLS | Standard tooling; per-device revocation and identity; the device name can be bound into the certificate | Needs a small CA setup step per site |
| B. Site pre-shared key | All devices hold one secret; TLS 1.3 external PSK | Simplest to deploy | One leak compromises the site; no per-device identity; external PSK is poorly supported by common TLS libraries |
| C. Trust on first use | Pin the peer's certificate fingerprint on first contact | No setup | Weak against an attacker present at first contact; confusing UX |

Recommendation: **A**, with a helper tool (`idn-ca init`, `idn-ca issue <device>`) so that
setup takes a minute. Option C could be offered later as a convenience mode.

## 4. Negotiation (control channel)

1. Both peers send `ndi_version` as usual, adding `x_idn_secure="1"`.
2. If **both** advertised it, the receiver sends kind 1 `<x_idn_starttls v="1"/>` as its
   next message and stops sending plain frames. The sender answers
   `<x_idn_starttls_ok v="1"/>` and also stops.
3. Both sides then run a TLS 1.3 handshake **on the same TCP connection** (mutual
   certificates per §3, ALPN `idn-control/1`). From then on, the control frames of §1.2
   flow inside TLS, unchanged, scrambling included.
4. Before step 3 nothing secret is exchanged. The `ndi_version` attributes are only an
   offer.

**Policy.** A peer configured as `require` closes the connection when the other side
didn't advertise `x_idn_secure`, or when the TLS handshake fails. A peer configured as
`prefer` (the default) falls back to plain NDI. `require` defeats downgrade: an attacker
who strips the attribute only causes a refused connection.

## 5. Media

### 5.1 QUIC (default transport)

- The receiver advertises its QUIC port as usual and adds `x_idn_secure_quic="1"` in
  `ndi_enabled_streams`. This is sent inside the TLS control channel, so it cannot be
  stripped.
- The sender connects with **real QUIC v1 + TLS 1.3** (RFC 9000/9001), ALPN
  `idn-media/1`, using mutual certificates as in §3. There is no stub handshake and no
  `0xBAAD`.
- Inside, the streams are exactly as in chapter 2: the 16-byte token, then the `msg_len`
  framing.
- A receiver that advertised `x_idn_secure_quic` MUST refuse stub-handshake connections
  from that sender, because those could be an attacker's injection.

### 5.2 Other transports

UDP unicast, multicast and multi-TCP carry no security in version 1. A `require`-policy
peer MUST NOT advertise them. A later version could add DTLS, or AEAD per packet with keys
derived from the control channel's TLS exporter.

### 5.3 Media on the control connection (§1.6)

This is protected automatically, because it is inside the control channel's TLS.

## 6. Discovery

Unchanged in version 1. mDNS and the Discovery Server stay plain. Optionally, a TXT key
`x_idn_secure=1` lets finders show a lock icon. Stock finders ignore unknown TXT keys
(§5.1.3).

## 7. Open points

- The trust model (§3) — the user's decision.
- The certificate name binding: the NDI full name `HOST (source)` in a SAN URI such as
  `ndi:HOST%20(source)`?
- Key and cert storage on devices, and rotation.
- Whether to protect discovery too: signed TXT records, or a TLS-only Discovery Server
  connection.

## 8. Implementation sketch

- TLS via `rustls`. Real QUIC for media via `quinn` (with rustls), kept separate from the
  stub-profile QUIC code in `src/quic/`.
- A control-channel wrapper: after the STARTTLS exchange, `Frame` read and write go
  through `rustls::Stream`.
- CLI: `--secure prefer|require|off`, `--cert`, `--key`, `--ca`.
- Tests:
  - ours against ours in every policy combination;
  - ours against stock NDI (lab; spec §1.9.1): `prefer` must behave exactly as plain NDI;
  - downgrade: stripping `x_idn_secure` against `require` must be refused.
