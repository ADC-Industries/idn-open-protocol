//! SPA pods: what is offered (EnumFormat), what buffers are wanted (Buffers), and reading
//! the format chosen.

pub const TYPE_ID: u32 = 3;
pub const TYPE_INT: u32 = 4;
pub const TYPE_LONG: u32 = 5;
pub const TYPE_RECTANGLE: u32 = 10;
pub const TYPE_FRACTION: u32 = 11;
pub const TYPE_OBJECT: u32 = 15;
pub const TYPE_CHOICE: u32 = 19;
pub const OBJECT_FORMAT: u32 = 0x40003;
pub const OBJECT_BUFFERS: u32 = 0x40004;
pub const PARAM_ENUM_FORMAT: u32 = 3;
pub const PARAM_FORMAT: u32 = 4;
pub const PARAM_BUFFERS: u32 = 5;
pub const CHOICE_NONE: u32 = 0;
pub const CHOICE_RANGE: u32 = 1;
pub const CHOICE_ENUM: u32 = 3;
pub const CHOICE_FLAGS: u32 = 4;
pub const KEY_MEDIA_TYPE: u32 = 1;
pub const KEY_MEDIA_SUBTYPE: u32 = 2;
pub const KEY_FORMAT: u32 = 0x20001;
pub const KEY_MODIFIER: u32 = 0x20002;
/// Property flags: the producer must take it; the producer leaves it to the consumer
/// to choose one (a modifier) of those it can.
pub const PROP_MANDATORY: u32 = 1 << 3;
pub const PROP_DONT_FIXATE: u32 = 1 << 4;
/// `spa_data_type`: shared memory (`MemPtr` 1, `MemFd` 2), GPU memory (`DmaBuf` 3).
pub const DATA_DMABUF: u32 = 3;
pub const KEY_SIZE: u32 = 0x20003;
pub const KEY_FRAMERATE: u32 = 0x20004;
pub const KEY_DATA_TYPE: u32 = 6;
pub const MEDIA_VIDEO: u32 = 2;
pub const SUBTYPE_RAW: u32 = 1;
pub const RGBX: u32 = 7;
pub const BGRX: u32 = 8;
pub const XRGB: u32 = 9;
pub const XBGR: u32 = 10;
pub const RGBA: u32 = 11;
pub const BGRA: u32 = 12;

fn u(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_ne_bytes());
}

fn pad(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(8) {
        v.push(0);
    }
}

/// A property `key` with `flags` whose value is `value` (a whole pod).
fn prop_flags(v: &mut Vec<u8>, key: u32, flags: u32, value: &[u8]) {
    u(v, key);
    u(v, flags);
    v.extend_from_slice(value);
    pad(v);
}

fn long(x: u64) -> Vec<u8> {
    let mut v = Vec::new();
    u(&mut v, 8);
    u(&mut v, TYPE_LONG);
    v.extend_from_slice(&x.to_ne_bytes());
    v
}

/// One of `values` (the first the default).
fn long_choice(values: &[u64]) -> Vec<u8> {
    let words: Vec<Vec<u32>> = std::iter::once(values[0])
        .chain(values.iter().copied())
        .map(|m| {
            let b = m.to_ne_bytes();
            vec![
                u32::from_ne_bytes(b[0..4].try_into().unwrap()),
                u32::from_ne_bytes(b[4..8].try_into().unwrap()),
            ]
        })
        .collect();
    choice(CHOICE_ENUM, TYPE_LONG, 8, &words)
}

fn id(x: u32) -> Vec<u8> {
    let mut v = Vec::new();
    u(&mut v, 4);
    u(&mut v, TYPE_ID);
    u(&mut v, x);
    pad(&mut v);
    v
}

/// A choice of `kind` over values of `ty`, each `size` bytes.
fn choice(kind: u32, ty: u32, size: u32, values: &[Vec<u32>]) -> Vec<u8> {
    let mut body = Vec::new();
    u(&mut body, kind);
    u(&mut body, 0);
    u(&mut body, size);
    u(&mut body, ty);
    for x in values {
        for w in x {
            u(&mut body, *w);
        }
    }
    let mut v = Vec::new();
    u(&mut v, body.len() as u32);
    u(&mut v, TYPE_CHOICE);
    v.extend_from_slice(&body);
    pad(&mut v);
    v
}

fn object(kind: u32, id: u32, props: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let with: Vec<(u32, u32, Vec<u8>)> = props.iter().map(|(k, v)| (*k, 0, v.clone())).collect();
    object_flags(kind, id, &with)
}

fn object_flags(kind: u32, id: u32, props: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    u(&mut body, kind);
    u(&mut body, id);
    for (k, f, val) in props {
        prop_flags(&mut body, *k, *f, val);
    }
    let mut v = Vec::new();
    u(&mut v, body.len() as u32);
    u(&mut v, TYPE_OBJECT);
    v.extend_from_slice(&body);
    v
}

/// Raw RGB video, any size and rate.
pub fn enum_format() -> Vec<u8> {
    let formats: Vec<Vec<u32>> = [BGRX, BGRX, RGBX, BGRA, RGBA, XRGB, XBGR]
        .iter()
        .map(|f| vec![*f])
        .collect();
    object(
        OBJECT_FORMAT,
        PARAM_ENUM_FORMAT,
        &[
            (KEY_MEDIA_TYPE, id(MEDIA_VIDEO)),
            (KEY_MEDIA_SUBTYPE, id(SUBTYPE_RAW)),
            (KEY_FORMAT, choice(CHOICE_ENUM, TYPE_ID, 4, &formats)),
            (
                KEY_SIZE,
                choice(
                    CHOICE_RANGE,
                    TYPE_RECTANGLE,
                    8,
                    &[vec![1920, 1080], vec![1, 1], vec![8192, 8192]],
                ),
            ),
            (
                KEY_FRAMERATE,
                choice(
                    CHOICE_RANGE,
                    TYPE_FRACTION,
                    8,
                    &[vec![30, 1], vec![0, 1], vec![240, 1]],
                ),
            ),
        ],
    )
}

/// `fmt` as DMA-BUF, laid out as one of `modifiers` (the producer chooses among them,
/// leaving the consumer to fixate it), or (`fixed`) as the first of them only.
pub fn enum_format_dmabuf(fmt: u32, modifiers: &[u64], fixed: bool) -> Vec<u8> {
    let (flags, value) = if fixed {
        (PROP_MANDATORY, long(modifiers[0]))
    } else {
        (PROP_MANDATORY | PROP_DONT_FIXATE, long_choice(modifiers))
    };
    object_flags(
        OBJECT_FORMAT,
        PARAM_ENUM_FORMAT,
        &[
            (KEY_MEDIA_TYPE, 0, id(MEDIA_VIDEO)),
            (KEY_MEDIA_SUBTYPE, 0, id(SUBTYPE_RAW)),
            (KEY_FORMAT, 0, id(fmt)),
            (KEY_MODIFIER, flags, value),
            (
                KEY_SIZE,
                0,
                choice(
                    CHOICE_RANGE,
                    TYPE_RECTANGLE,
                    8,
                    &[vec![1920, 1080], vec![1, 1], vec![8192, 8192]],
                ),
            ),
            (
                KEY_FRAMERATE,
                0,
                choice(
                    CHOICE_RANGE,
                    TYPE_FRACTION,
                    8,
                    &[vec![30, 1], vec![0, 1], vec![240, 1]],
                ),
            ),
        ],
    )
}

/// Buffers in GPU memory (DMA-BUF).
pub fn buffers_dmabuf() -> Vec<u8> {
    object(
        OBJECT_BUFFERS,
        PARAM_BUFFERS,
        &[(
            KEY_DATA_TYPE,
            choice(CHOICE_FLAGS, TYPE_INT, 4, &[vec![1 << DATA_DMABUF]]),
        )],
    )
}

/// Buffers in memory this process can map (not DMA-BUF).
pub fn buffers() -> Vec<u8> {
    // 1 << MemPtr | 1 << MemFd
    object(
        OBJECT_BUFFERS,
        PARAM_BUFFERS,
        &[(
            KEY_DATA_TYPE,
            choice(CHOICE_FLAGS, TYPE_INT, 4, &[vec![(1 << 1) | (1 << 2)]]),
        )],
    )
}

/// The chosen format: (video format, width, height).
#[cfg(test)]
pub fn parse_format(p: &[u8]) -> Option<(u32, u32, u32)> {
    parse_format_full(p).map(|f| (f.fmt, f.w, f.h))
}

/// A format as the producer gave it, with its DMA-BUF modifier when it has one.
#[derive(Debug, Clone, PartialEq)]
pub struct Chosen {
    pub fmt: u32,
    pub w: u32,
    pub h: u32,
    /// The modifier, fixed; or (`dont_fixate`) the default of those to choose from.
    pub modifier: Option<u64>,
    /// The modifiers to choose from (`dont_fixate`).
    pub modifiers: Vec<u64>,
    pub dont_fixate: bool,
}

/// As [`parse_format`], with the modifier.
pub fn parse_format_full(p: &[u8]) -> Option<Chosen> {
    let rd = |at: usize| -> Option<u32> {
        Some(u32::from_ne_bytes(p.get(at..at + 4)?.try_into().ok()?))
    };
    let rd64 = |at: usize| -> Option<u64> {
        Some(u64::from_ne_bytes(p.get(at..at + 8)?.try_into().ok()?))
    };
    let size = rd(0)? as usize;
    if rd(4)? != TYPE_OBJECT || p.len() < 8 + size {
        return None;
    }
    let end = 8 + size;
    let mut c = Chosen {
        fmt: 0,
        w: 0,
        h: 0,
        modifier: None,
        modifiers: Vec::new(),
        dont_fixate: false,
    };
    let mut at = 16;
    while at + 16 <= end {
        let key = rd(at)?;
        let flags = rd(at + 4)?;
        let vsize = rd(at + 8)? as usize;
        let vtype = rd(at + 12)?;
        let body = at + 16;
        if key == KEY_MODIFIER {
            c.dont_fixate = flags & PROP_DONT_FIXATE != 0;
            if vtype == TYPE_LONG {
                c.modifier = rd64(body);
            } else if vtype == TYPE_CHOICE && rd(body + 12)? == TYPE_LONG && rd(body + 8)? == 8 {
                let n = vsize.saturating_sub(16) / 8;
                let all: Vec<u64> = (0..n).filter_map(|i| rd64(body + 16 + 8 * i)).collect();
                c.modifier = all.first().copied();
                c.modifiers = all.into_iter().skip(1).collect();
            }
        }
        at = body + ((vsize + 7) & !7);
    }
    let (fmt, w, h) = parse_basic(p)?;
    c.fmt = fmt;
    c.w = w;
    c.h = h;
    Some(c)
}

fn parse_basic(p: &[u8]) -> Option<(u32, u32, u32)> {
    let rd = |at: usize| -> Option<u32> {
        Some(u32::from_ne_bytes(p.get(at..at + 4)?.try_into().ok()?))
    };
    let size = rd(0)? as usize;
    if rd(4)? != TYPE_OBJECT || p.len() < 8 + size {
        return None;
    }
    let end = 8 + size;
    let (mut fmt, mut w, mut h) = (None, None, None);
    let mut at = 16; // past the object's type and id
    while at + 16 <= end {
        let key = rd(at)?;
        let vsize = rd(at + 8)? as usize;
        let vtype = rd(at + 12)?;
        let body = at + 16;
        // A value, or the first (default) value of a choice.
        let (vt, vb) = if vtype == TYPE_CHOICE {
            (rd(body + 12)?, body + 16)
        } else {
            (vtype, body)
        };
        match key {
            KEY_FORMAT if vt == TYPE_ID => fmt = rd(vb),
            KEY_SIZE if vt == TYPE_RECTANGLE => {
                w = rd(vb);
                h = rd(vb + 4);
            }
            _ => {}
        }
        at = body + ((vsize + 7) & !7);
    }
    let _ = CHOICE_NONE;
    Some((fmt?, w?, h?))
}
