//! Open Sound Control 1.0 (the published specification): messages and bundles on the
//! wire, for the mixer's OSC control (idn-mixer §M.16) and the switcher's cues that send
//! OSC (idn-switcher §S.27). No I/O here.

/// An OSC argument.
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    /// `i`: a 32-bit integer (`h` read into it, clamped).
    Int(i32),
    /// `f`: a 32-bit float (`d` read into it).
    Float(f32),
    /// `s` (or `S`): a string.
    Str(String),
    /// `T` or `F`.
    Bool(bool),
    /// Nil, impulse, a blob or a time tag: present but with no value we use.
    Nil,
}

/// An OSC message.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// The address pattern, `/ch/3/fader`.
    pub addr: String,
    /// Its arguments, in order.
    pub args: Vec<Arg>,
}

impl Message {
    /// A message to `addr` with `args`.
    pub fn new(addr: impl Into<String>, args: Vec<Arg>) -> Message {
        Message {
            addr: addr.into(),
            args,
        }
    }
}

/// The largest packet sent: messages are put in bundles up to this.
pub const PACKET: usize = 1200;

fn padded(n: usize) -> usize {
    (n + 4) & !3
}

/// An OSC string at `p[at..]`: its text and where the next field starts.
fn string_at(p: &[u8], at: usize) -> Result<(String, usize), String> {
    let rest = p.get(at..).ok_or("truncated")?;
    let end = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or("unterminated string")?;
    let s = std::str::from_utf8(&rest[..end]).map_err(|_| "not UTF-8")?;
    Ok((s.to_owned(), at + padded(end)))
}

fn word<const N: usize>(p: &[u8], at: usize) -> Result<[u8; N], String> {
    p.get(at..at + N)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| "truncated".into())
}

/// The messages in a packet: one, or a bundle's (bundles inside bundles too; time tags
/// are ignored, everything is done at once).
pub fn decode(p: &[u8]) -> Result<Vec<Message>, String> {
    let mut out = Vec::new();
    decode_into(p, &mut out, 0)?;
    Ok(out)
}

fn decode_into(p: &[u8], out: &mut Vec<Message>, depth: usize) -> Result<(), String> {
    if p.starts_with(b"#bundle\0") {
        if depth > 8 {
            return Err("bundles nested too deep".into());
        }
        let mut at = 16;
        while at < p.len() {
            // (Sizes from the sender: negative or past the end is an error, not an
            // overflow.)
            let n = u32::try_from(i32::from_be_bytes(word(p, at)?))
                .map_err(|_| "a negative bundle element size")? as usize;
            let end = (at + 4).checked_add(n).ok_or("truncated bundle")?;
            let part = p.get(at + 4..end).ok_or("truncated bundle")?;
            decode_into(part, out, depth + 1)?;
            at = end;
        }
        return Ok(());
    }
    let (addr, mut at) = string_at(p, 0)?;
    if !addr.starts_with('/') {
        return Err("not an OSC message".into());
    }
    let mut args = Vec::new();
    // A message without a type tag string (old senders) has no arguments.
    if at < p.len() {
        let (tags, next) = string_at(p, at)?;
        at = next;
        for t in tags.strip_prefix(',').ok_or("no type tags")?.chars() {
            args.push(match t {
                'i' => {
                    let v = i32::from_be_bytes(word(p, at)?);
                    at += 4;
                    Arg::Int(v)
                }
                'f' => {
                    let v = f32::from_be_bytes(word(p, at)?);
                    at += 4;
                    Arg::Float(v)
                }
                'h' => {
                    let v = i64::from_be_bytes(word(p, at)?);
                    at += 8;
                    Arg::Int(v.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
                }
                'd' => {
                    let v = f64::from_be_bytes(word(p, at)?);
                    at += 8;
                    Arg::Float(v as f32)
                }
                's' | 'S' => {
                    let (s, next) = string_at(p, at)?;
                    at = next;
                    Arg::Str(s)
                }
                'b' => {
                    let n = u32::try_from(i32::from_be_bytes(word(p, at)?))
                        .map_err(|_| "a negative blob size")? as usize;
                    at = at
                        .checked_add(4 + n.div_ceil(4) * 4)
                        .filter(|&e| e <= p.len())
                        .ok_or("truncated blob")?;
                    Arg::Nil
                }
                't' => {
                    at += 8;
                    Arg::Nil
                }
                'c' | 'r' | 'm' => {
                    at += 4;
                    Arg::Nil
                }
                'T' => Arg::Bool(true),
                'F' => Arg::Bool(false),
                'N' | 'I' => Arg::Nil,
                // Arrays' brackets: their contents are read as plain arguments.
                '[' | ']' => continue,
                other => return Err(format!("unknown type tag {other:?}")),
            });
        }
        if at > p.len() {
            return Err("truncated".into());
        }
    }
    out.push(Message { addr, args });
    Ok(())
}

fn put_string(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
    b.resize(b.len() + padded(s.len()) - s.len(), 0);
}

/// A message on the wire.
pub fn encode(m: &Message) -> Vec<u8> {
    let mut b = Vec::new();
    put_string(&mut b, &m.addr);
    let mut tags = String::from(",");
    for a in &m.args {
        tags.push(match a {
            Arg::Int(_) => 'i',
            Arg::Float(_) => 'f',
            Arg::Str(_) => 's',
            Arg::Bool(true) => 'T',
            Arg::Bool(false) => 'F',
            Arg::Nil => 'N',
        });
    }
    put_string(&mut b, &tags);
    for a in &m.args {
        match a {
            Arg::Int(v) => b.extend_from_slice(&v.to_be_bytes()),
            Arg::Float(v) => b.extend_from_slice(&v.to_be_bytes()),
            Arg::Str(s) => put_string(&mut b, s),
            Arg::Bool(_) | Arg::Nil => {}
        }
    }
    b
}

/// Packets holding these messages: one message alone, more in bundles ("immediately")
/// of up to [`PACKET`] bytes.
pub fn packets(msgs: &[Message]) -> Vec<Vec<u8>> {
    if let [m] = msgs {
        return vec![encode(m)];
    }
    let start = || {
        let mut b = b"#bundle\0".to_vec();
        b.extend_from_slice(&1u64.to_be_bytes());
        b
    };
    let mut out = Vec::new();
    let mut b = start();
    for m in msgs {
        let e = encode(m);
        if b.len() > 16 && b.len() + 4 + e.len() > PACKET {
            out.push(std::mem::replace(&mut b, start()));
        }
        b.extend_from_slice(&(e.len() as i32).to_be_bytes());
        b.extend_from_slice(&e);
    }
    if b.len() > 16 {
        out.push(b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_and_bundles_round_trip() {
        let m = Message::new(
            "/ch/3/fader",
            vec![
                Arg::Float(-6.5),
                Arg::Int(2),
                Arg::Str("abc".into()),
                Arg::Bool(true),
            ],
        );
        let e = encode(&m);
        assert_eq!(e.len() % 4, 0);
        assert_eq!(decode(&e).unwrap(), vec![m.clone()]);
        // A bundle of many splits into packets that each decode.
        let many: Vec<Message> = (1..=200)
            .map(|i| Message::new(format!("/ch/{i}/mute"), vec![Arg::Int(i % 2)]))
            .collect();
        let ps = packets(&many);
        assert!(ps.len() > 1 && ps.iter().all(|p| p.len() <= PACKET));
        let back: Vec<Message> = ps.iter().flat_map(|p| decode(p).unwrap()).collect();
        assert_eq!(back, many);
        // A message with no type tags (an old sender) and a truncated one.
        assert_eq!(decode(b"/clear_solo\0").unwrap()[0].args, vec![]);
        assert!(decode(&e[..e.len() - 2]).is_err());
    }
}
