//! SPA POD, the binary format PipeWire negotiates in: just enough of it to
//! offer the formats Latch reads, ask for buffers and metadata, and read
//! the format a producer settled on. The layout is SPA's (spa/pod/pod.h):
//! every POD is a `u32` body size and a `u32` type, then the body, padded
//! to eight bytes; SPA's builder and parser are inline functions in its
//! headers, so nothing of them can be loaded from libpipewire.

use anyhow::{bail, ensure, Context};

pub const TYPE_ID: u32 = 3;
pub const TYPE_INT: u32 = 4;
pub const TYPE_LONG: u32 = 5;
pub const TYPE_RECTANGLE: u32 = 10;
pub const TYPE_FRACTION: u32 = 11;
pub const TYPE_OBJECT: u32 = 15;
pub const TYPE_CHOICE: u32 = 19;

pub const CHOICE_RANGE: u32 = 1;
pub const CHOICE_ENUM: u32 = 3;
pub const CHOICE_FLAGS: u32 = 4;

pub const PROP_MANDATORY: u32 = 1 << 3;
pub const PROP_DONT_FIXATE: u32 = 1 << 4;

pub const OBJECT_FORMAT: u32 = 0x40003;
pub const OBJECT_PARAM_BUFFERS: u32 = 0x40004;
pub const OBJECT_PARAM_META: u32 = 0x40005;

pub const PARAM_ENUM_FORMAT: u32 = 3;
pub const PARAM_FORMAT: u32 = 4;
pub const PARAM_BUFFERS: u32 = 5;
pub const PARAM_META: u32 = 6;

pub const FORMAT_MEDIA_TYPE: u32 = 1;
pub const FORMAT_MEDIA_SUBTYPE: u32 = 2;
pub const FORMAT_VIDEO_FORMAT: u32 = 0x20001;
pub const FORMAT_VIDEO_MODIFIER: u32 = 0x20002;
pub const FORMAT_VIDEO_SIZE: u32 = 0x20003;
pub const FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;

pub const MEDIA_TYPE_VIDEO: u32 = 2;
pub const MEDIA_SUBTYPE_RAW: u32 = 1;

pub const BUFFERS_DATA_TYPE: u32 = 6;
pub const META_TYPE: u32 = 1;
pub const META_SIZE: u32 = 2;

pub const META_HEADER: u32 = 1;
pub const META_VIDEO_DAMAGE: u32 = 3;
pub const META_VIDEO_TRANSFORM: u32 = 8;

pub const DATA_MEM_PTR: u32 = 1;
pub const DATA_MEM_FD: u32 = 2;
pub const DATA_DMA_BUF: u32 = 3;

/// `DRM_FORMAT_MOD_LINEAR`: rows in memory one after another, which the
/// CPU can read through a mapping. Tiled layouts need the GPU.
pub const MODIFIER_LINEAR: u64 = 0;

/// `struct spa_meta_header` and `struct spa_meta_region`.
pub const META_HEADER_SIZE: i32 = 32;
pub const META_REGION_SIZE: i32 = 16;
/// Damage rectangles asked for per buffer; a producer with more sends
/// their bounding box in the last one.
pub const DAMAGE_REGIONS: i32 = 16;

/// The `spa_video_format`s Latch reads, all four bytes a pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgrx,
    Bgra,
    Rgbx,
    Rgba,
}

impl PixelFormat {
    /// Preferred first: the byte order the encoder's conversion reads.
    pub const OFFERED: [PixelFormat; 4] = [Self::Bgrx, Self::Bgra, Self::Rgbx, Self::Rgba];

    pub fn spa(self) -> u32 {
        match self {
            Self::Rgbx => 7,
            Self::Bgrx => 8,
            Self::Rgba => 11,
            Self::Bgra => 12,
        }
    }

    pub fn from_spa(id: u32) -> Option<Self> {
        Self::OFFERED.into_iter().find(|f| f.spa() == id)
    }

    /// Red and blue swap places on the way to BGRA.
    pub fn swaps_red_and_blue(self) -> bool {
        matches!(self, Self::Rgbx | Self::Rgba)
    }
}

/// One value inside a POD, as far as Latch writes or reads them.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Id(u32),
    Int(i32),
    Long(i64),
    Rectangle(u32, u32),
    Fraction(u32, u32),
    /// SPA's choice: `values[0]` is the default; then a range's minimum
    /// and maximum, or an enum's alternatives, or flags.
    Choice {
        kind: u32,
        values: Vec<Value>,
    },
    /// A type Latch neither writes nor reads.
    Other(u32),
}

impl Value {
    fn type_and_body(&self) -> (u32, Vec<u8>) {
        match self {
            Value::Id(v) => (TYPE_ID, v.to_ne_bytes().to_vec()),
            Value::Int(v) => (TYPE_INT, v.to_ne_bytes().to_vec()),
            Value::Long(v) => (TYPE_LONG, v.to_ne_bytes().to_vec()),
            Value::Rectangle(w, h) => (TYPE_RECTANGLE, [w.to_ne_bytes(), h.to_ne_bytes()].concat()),
            Value::Fraction(n, d) => (TYPE_FRACTION, [n.to_ne_bytes(), d.to_ne_bytes()].concat()),
            Value::Choice { kind, values } => {
                let mut body = Vec::new();
                body.extend(kind.to_ne_bytes());
                body.extend(0u32.to_ne_bytes());
                let children: Vec<_> = values.iter().map(Value::type_and_body).collect();
                let (child_type, child_size) = children
                    .first()
                    .map(|(t, b)| (*t, b.len() as u32))
                    .unwrap_or((TYPE_INT, 4));
                body.extend(child_size.to_ne_bytes());
                body.extend(child_type.to_ne_bytes());
                for (_, child) in children {
                    body.extend(child);
                }
                (TYPE_CHOICE, body)
            }
            Value::Other(t) => (*t, Vec::new()),
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        let (kind, body) = self.type_and_body();
        out.extend((body.len() as u32).to_ne_bytes());
        out.extend(kind.to_ne_bytes());
        out.extend(&body);
        pad(out);
    }

    /// The value, or a choice's default.
    pub fn fixed(&self) -> &Value {
        match self {
            Value::Choice { values, .. } if !values.is_empty() => &values[0],
            other => other,
        }
    }

    /// Every value a property may take: a choice's alternatives (an enum's
    /// default repeats one of them), or the value itself.
    pub fn alternatives(&self) -> Vec<&Value> {
        match self {
            Value::Choice {
                kind: CHOICE_ENUM,
                values,
            } if values.len() > 1 => values[1..].iter().collect(),
            Value::Choice { values, .. } => values.iter().take(1).collect(),
            other => vec![other],
        }
    }
}

fn pad(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(8) {
        out.push(0);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Prop {
    pub key: u32,
    pub flags: u32,
    pub value: Value,
}

impl Prop {
    pub fn new(key: u32, value: Value) -> Self {
        Self {
            key,
            flags: 0,
            value,
        }
    }
    pub fn with_flags(key: u32, flags: u32, value: Value) -> Self {
        Self { key, flags, value }
    }
}

/// An object POD (`SPA_TYPE_Object`): a parameter or a format.
#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    pub kind: u32,
    pub id: u32,
    pub props: Vec<Prop>,
}

impl Object {
    pub fn prop(&self, key: u32) -> Option<&Prop> {
        self.props.iter().find(|p| p.key == key)
    }

    /// The POD, eight-byte aligned so that its `int64_t`s can be read in
    /// place, as `struct spa_pod *` callers expect.
    pub fn build(&self) -> Pod {
        let mut body = Vec::new();
        body.extend(self.kind.to_ne_bytes());
        body.extend(self.id.to_ne_bytes());
        for prop in &self.props {
            body.extend(prop.key.to_ne_bytes());
            body.extend(prop.flags.to_ne_bytes());
            prop.value.write(&mut body);
        }
        let mut bytes = Vec::with_capacity(body.len() + 8);
        bytes.extend((body.len() as u32).to_ne_bytes());
        bytes.extend(TYPE_OBJECT.to_ne_bytes());
        bytes.extend(body);
        pad(&mut bytes);
        let mut words = vec![0u64; bytes.len() / 8];
        for (word, chunk) in words.iter_mut().zip(bytes.as_chunks::<8>().0) {
            *word = u64::from_ne_bytes(*chunk);
        }
        Pod(words)
    }

    /// Reads an object POD: its header and the `size` bytes after it.
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let (size, kind, body) = header(bytes)?;
        ensure!(kind == TYPE_OBJECT, "not an object POD (type {kind})");
        let body = &body[..size];
        ensure!(body.len() >= 8, "short object POD");
        let mut object = Object {
            kind: word(body, 0),
            id: word(body, 4),
            props: Vec::new(),
        };
        let mut at = 8;
        while at + 16 <= body.len() {
            let key = word(body, at);
            let flags = word(body, at + 4);
            let (size, _, _) = header(&body[at + 8..])?;
            let value = parse_value(&body[at + 8..])?;
            object.props.push(Prop { key, flags, value });
            at += 8 + 8 + size.next_multiple_of(8);
        }
        Ok(object)
    }
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

/// A POD's body size, type and the bytes from its body on.
fn header(bytes: &[u8]) -> anyhow::Result<(usize, u32, &[u8])> {
    ensure!(bytes.len() >= 8, "short POD header");
    let size = word(bytes, 0) as usize;
    let kind = word(bytes, 4);
    ensure!(
        bytes.len() >= 8 + size,
        "POD of {size} bytes overruns its buffer"
    );
    Ok((size, kind, &bytes[8..]))
}

fn parse_body(kind: u32, body: &[u8]) -> anyhow::Result<Value> {
    let need = |n: usize| -> anyhow::Result<()> {
        ensure!(body.len() >= n, "short POD body for type {kind}");
        Ok(())
    };
    Ok(match kind {
        TYPE_ID => {
            need(4)?;
            Value::Id(word(body, 0))
        }
        TYPE_INT => {
            need(4)?;
            Value::Int(word(body, 0) as i32)
        }
        TYPE_LONG => {
            need(8)?;
            Value::Long(i64::from_ne_bytes(body[..8].try_into()?))
        }
        TYPE_RECTANGLE => {
            need(8)?;
            Value::Rectangle(word(body, 0), word(body, 4))
        }
        TYPE_FRACTION => {
            need(8)?;
            Value::Fraction(word(body, 0), word(body, 4))
        }
        TYPE_CHOICE => {
            need(16)?;
            let choice = word(body, 0);
            let child_size = word(body, 8) as usize;
            let child_type = word(body, 12);
            ensure!(child_size > 0, "choice of empty values");
            let values = body[16..]
                .chunks_exact(child_size)
                .map(|child| parse_body(child_type, child))
                .collect::<anyhow::Result<_>>()?;
            Value::Choice {
                kind: choice,
                values,
            }
        }
        other => Value::Other(other),
    })
}

fn parse_value(bytes: &[u8]) -> anyhow::Result<Value> {
    let (size, kind, body) = header(bytes)?;
    parse_body(kind, &body[..size])
}

/// A built POD, kept in `u64`s for its alignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pod(Vec<u64>);

impl Pod {
    pub fn as_ptr(&self) -> *const u8 {
        self.0.as_ptr().cast()
    }
    #[cfg(test)]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: a u64 slice viewed as its bytes.
        unsafe { std::slice::from_raw_parts(self.as_ptr(), self.0.len() * 8) }
    }
}

/// What Latch offers a screencast producer: the formats it reads, at any
/// size, at up to `fps`. With `dmabuf`, the same in DMA-BUFs whose layout
/// the CPU reads (`modifiers`; only [`MODIFIER_LINEAR`] is useful), which
/// must be listed first: a producer takes the first offer it can meet.
pub fn enum_format(dmabuf: Option<&[u64]>, fps: u32) -> Object {
    let formats = std::iter::once(PixelFormat::OFFERED[0])
        .chain(PixelFormat::OFFERED)
        .map(|f| Value::Id(f.spa()))
        .collect();
    let mut props = vec![
        Prop::new(FORMAT_MEDIA_TYPE, Value::Id(MEDIA_TYPE_VIDEO)),
        Prop::new(FORMAT_MEDIA_SUBTYPE, Value::Id(MEDIA_SUBTYPE_RAW)),
        Prop::new(
            FORMAT_VIDEO_FORMAT,
            Value::Choice {
                kind: CHOICE_ENUM,
                values: formats,
            },
        ),
    ];
    if let Some(modifiers) = dmabuf {
        let mut values: Vec<Value> = modifiers.iter().map(|&m| Value::Long(m as i64)).collect();
        let value = if values.len() == 1 {
            // Fixed: nothing left for the producer to choose.
            values.remove(0)
        } else {
            values.insert(0, values[0].clone());
            Value::Choice {
                kind: CHOICE_ENUM,
                values,
            }
        };
        let flags = if modifiers.len() == 1 {
            PROP_MANDATORY
        } else {
            PROP_MANDATORY | PROP_DONT_FIXATE
        };
        props.push(Prop::with_flags(FORMAT_VIDEO_MODIFIER, flags, value));
    }
    props.push(Prop::new(
        FORMAT_VIDEO_SIZE,
        Value::Choice {
            kind: CHOICE_RANGE,
            values: vec![
                Value::Rectangle(1920, 1080),
                Value::Rectangle(1, 1),
                Value::Rectangle(16384, 16384),
            ],
        },
    ));
    props.push(Prop::new(
        FORMAT_VIDEO_FRAMERATE,
        Value::Choice {
            kind: CHOICE_RANGE,
            values: vec![
                Value::Fraction(fps.clamp(1, 1000), 1),
                Value::Fraction(0, 1),
                Value::Fraction(fps.clamp(1, 1000), 1),
            ],
        },
    ));
    Object {
        kind: OBJECT_FORMAT,
        id: PARAM_ENUM_FORMAT,
        props,
    }
}

/// The format a producer settled on (`SPA_PARAM_Format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    /// Present for DMA-BUF buffers.
    pub modifier: Option<u64>,
}

/// What to do with a format a producer proposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settle {
    /// Use it: ask for buffers and metadata.
    Use(Negotiated),
    /// The producer left the DMA-BUF layout open: offer again with only
    /// the linear layout, which the CPU reads, or without DMA-BUF.
    Fixate { linear: bool },
}

pub fn settle(format: &Object) -> anyhow::Result<Settle> {
    ensure!(
        format.kind == OBJECT_FORMAT,
        "not a format (object type {:#x})",
        format.kind
    );
    let id = |key| match format.prop(key).map(|p| p.value.fixed()) {
        Some(Value::Id(v)) => Some(*v),
        _ => None,
    };
    ensure!(
        id(FORMAT_MEDIA_TYPE) == Some(MEDIA_TYPE_VIDEO)
            && id(FORMAT_MEDIA_SUBTYPE) == Some(MEDIA_SUBTYPE_RAW),
        "the screencast is not raw video"
    );
    let raw = id(FORMAT_VIDEO_FORMAT).context("the screencast format has no pixel format")?;
    let pixel = PixelFormat::from_spa(raw).with_context(|| {
        format!("the screencast sends pixel format {raw}, which Latch does not read")
    })?;
    let Some(Value::Rectangle(width, height)) =
        format.prop(FORMAT_VIDEO_SIZE).map(|p| p.value.fixed())
    else {
        bail!("the screencast format has no size");
    };
    ensure!(
        (1..=16384).contains(width) && (1..=16384).contains(height),
        "invalid screencast size {width} × {height}"
    );
    let modifier = match format.prop(FORMAT_VIDEO_MODIFIER) {
        None => None,
        Some(prop) => {
            let choices: Vec<u64> = prop
                .value
                .alternatives()
                .into_iter()
                .filter_map(|v| match v {
                    Value::Long(m) => Some(*m as u64),
                    _ => None,
                })
                .collect();
            let open = prop.flags & PROP_DONT_FIXATE != 0 || choices.len() > 1;
            if open {
                return Ok(Settle::Fixate {
                    linear: choices.contains(&MODIFIER_LINEAR),
                });
            }
            match choices.first() {
                Some(&MODIFIER_LINEAR) => Some(MODIFIER_LINEAR),
                Some(other) => bail!("the screencast DMA-BUF layout {other:#x} is not linear"),
                None => bail!("the screencast DMA-BUF has no layout"),
            }
        }
    };
    Ok(Settle::Use(Negotiated {
        format: pixel,
        width: *width,
        height: *height,
        modifier,
    }))
}

/// `SPA_PARAM_Buffers` for the settled format: DMA-BUFs when it has a
/// modifier, otherwise memory (shared memory or a pointer).
pub fn buffers(format: &Negotiated) -> Object {
    let types = if format.modifier.is_some() {
        1 << DATA_DMA_BUF
    } else {
        (1 << DATA_MEM_FD) | (1 << DATA_MEM_PTR)
    };
    Object {
        kind: OBJECT_PARAM_BUFFERS,
        id: PARAM_BUFFERS,
        props: vec![Prop::new(
            BUFFERS_DATA_TYPE,
            Value::Choice {
                kind: CHOICE_FLAGS,
                values: vec![Value::Int(types)],
            },
        )],
    }
}

/// `SPA_PARAM_Meta` for buffer headers and damage rectangles.
pub fn metas() -> [Object; 3] {
    let meta = |kind, size| Object {
        kind: OBJECT_PARAM_META,
        id: PARAM_META,
        props: vec![
            Prop::new(META_TYPE, Value::Id(kind)),
            Prop::new(META_SIZE, size),
        ],
    };
    [
        meta(META_HEADER, Value::Int(META_HEADER_SIZE)),
        meta(
            META_VIDEO_DAMAGE,
            Value::Choice {
                kind: CHOICE_RANGE,
                values: vec![
                    Value::Int(META_REGION_SIZE * DAMAGE_REGIONS),
                    Value::Int(META_REGION_SIZE),
                    Value::Int(META_REGION_SIZE * DAMAGE_REGIONS),
                ],
            },
        ),
        meta(META_VIDEO_TRANSFORM, Value::Int(4)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(pod: &Pod) -> Vec<u32> {
        pod.bytes()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_ne_bytes(*c))
            .collect()
    }

    #[test]
    fn an_object_has_spa_layout() {
        let pod = Object {
            kind: OBJECT_PARAM_META,
            id: PARAM_META,
            props: vec![
                Prop::new(META_TYPE, Value::Id(META_HEADER)),
                Prop::new(META_SIZE, Value::Int(32)),
            ],
        }
        .build();
        // Header, object type and id, then key, flags and an eight-byte
        // padded POD per property: what SPA_POD_BUILDER writes.
        assert_eq!(
            words(&pod),
            [
                56,
                TYPE_OBJECT,
                OBJECT_PARAM_META,
                PARAM_META, //
                META_TYPE,
                0,
                4,
                TYPE_ID,
                META_HEADER,
                0, //
                META_SIZE,
                0,
                4,
                TYPE_INT,
                32,
                0,
            ]
        );
        assert_eq!(pod.as_ptr() as usize % 8, 0);
    }

    #[test]
    fn a_choice_lists_its_child_type_once() {
        let pod = Object {
            kind: OBJECT_FORMAT,
            id: PARAM_ENUM_FORMAT,
            props: vec![Prop::new(
                FORMAT_VIDEO_SIZE,
                Value::Choice {
                    kind: CHOICE_RANGE,
                    values: vec![
                        Value::Rectangle(1920, 1080),
                        Value::Rectangle(1, 1),
                        Value::Rectangle(16384, 16384),
                    ],
                },
            )],
        }
        .build();
        assert_eq!(
            words(&pod),
            [
                64,
                TYPE_OBJECT,
                OBJECT_FORMAT,
                PARAM_ENUM_FORMAT, //
                FORMAT_VIDEO_SIZE,
                0,
                40,
                TYPE_CHOICE,
                CHOICE_RANGE,
                0,
                8,
                TYPE_RECTANGLE, //
                1920,
                1080,
                1,
                1,
                16384,
                16384,
            ]
        );
    }

    #[test]
    fn what_is_built_parses_back() {
        for object in [
            enum_format(None, 60),
            enum_format(Some(&[MODIFIER_LINEAR]), 120),
            enum_format(Some(&[MODIFIER_LINEAR, 0x0100_0000_0000_0001]), 30),
            buffers(&Negotiated {
                format: PixelFormat::Bgrx,
                width: 64,
                height: 64,
                modifier: Some(0),
            }),
            metas()[1].clone(),
        ] {
            assert_eq!(Object::parse(object.build().bytes()).unwrap(), object);
        }
    }

    #[test]
    fn the_offer_prefers_bgrx_and_marks_the_linear_layout_mandatory() {
        let offer = enum_format(Some(&[MODIFIER_LINEAR]), 60);
        let format = offer.prop(FORMAT_VIDEO_FORMAT).unwrap();
        assert_eq!(format.value.fixed(), &Value::Id(PixelFormat::Bgrx.spa()));
        assert_eq!(format.value.alternatives().len(), 4);
        let modifier = offer.prop(FORMAT_VIDEO_MODIFIER).unwrap();
        assert_eq!(modifier.flags, PROP_MANDATORY);
        assert_eq!(modifier.value, Value::Long(0));
        let shm = enum_format(None, 60);
        assert!(shm.prop(FORMAT_VIDEO_MODIFIER).is_none());
        let Some(Value::Choice { values, .. }) = shm.prop(FORMAT_VIDEO_FRAMERATE).map(|p| &p.value)
        else {
            panic!("framerate is a range");
        };
        assert_eq!(values[0], Value::Fraction(60, 1));
    }

    fn proposal(format: u32, modifier: Option<Prop>) -> Object {
        let mut props = vec![
            Prop::new(FORMAT_MEDIA_TYPE, Value::Id(MEDIA_TYPE_VIDEO)),
            Prop::new(FORMAT_MEDIA_SUBTYPE, Value::Id(MEDIA_SUBTYPE_RAW)),
            Prop::new(FORMAT_VIDEO_FORMAT, Value::Id(format)),
            Prop::new(FORMAT_VIDEO_SIZE, Value::Rectangle(2560, 1440)),
            Prop::new(FORMAT_VIDEO_FRAMERATE, Value::Fraction(0, 1)),
        ];
        props.extend(modifier);
        Object {
            kind: OBJECT_FORMAT,
            id: PARAM_FORMAT,
            props,
        }
    }

    #[test]
    fn a_shared_memory_format_is_used_as_it_is() {
        let parsed = Object::parse(proposal(12, None).build().bytes()).unwrap();
        assert_eq!(
            settle(&parsed).unwrap(),
            Settle::Use(Negotiated {
                format: PixelFormat::Bgra,
                width: 2560,
                height: 1440,
                modifier: None,
            })
        );
        let b = buffers(&Negotiated {
            format: PixelFormat::Bgra,
            width: 1,
            height: 1,
            modifier: None,
        });
        assert_eq!(
            b.prop(BUFFERS_DATA_TYPE).unwrap().value.fixed(),
            &Value::Int((1 << DATA_MEM_FD) | (1 << DATA_MEM_PTR))
        );
    }

    #[test]
    fn a_dmabuf_layout_is_fixated_to_linear_or_refused() {
        let fixed = proposal(
            7,
            Some(Prop::with_flags(
                FORMAT_VIDEO_MODIFIER,
                PROP_MANDATORY,
                Value::Long(0),
            )),
        );
        let Settle::Use(n) = settle(&fixed).unwrap() else {
            panic!("a fixed linear layout is used");
        };
        assert_eq!((n.format, n.modifier), (PixelFormat::Rgbx, Some(0)));
        assert!(n.format.swaps_red_and_blue());
        let open = proposal(
            8,
            Some(Prop::with_flags(
                FORMAT_VIDEO_MODIFIER,
                PROP_MANDATORY | PROP_DONT_FIXATE,
                Value::Choice {
                    kind: CHOICE_ENUM,
                    values: vec![Value::Long(5), Value::Long(5), Value::Long(0)],
                },
            )),
        );
        assert_eq!(settle(&open).unwrap(), Settle::Fixate { linear: true });
        let tiled_only = proposal(
            8,
            Some(Prop::with_flags(
                FORMAT_VIDEO_MODIFIER,
                PROP_DONT_FIXATE,
                Value::Choice {
                    kind: CHOICE_ENUM,
                    values: vec![Value::Long(5), Value::Long(5), Value::Long(6)],
                },
            )),
        );
        assert_eq!(
            settle(&tiled_only).unwrap(),
            Settle::Fixate { linear: false }
        );
        let tiled = proposal(8, Some(Prop::new(FORMAT_VIDEO_MODIFIER, Value::Long(5))));
        assert!(settle(&tiled).is_err());
    }

    #[test]
    fn formats_latch_does_not_read_are_refused() {
        assert!(settle(&proposal(2, None)).is_err(), "I420");
        let mut no_size = proposal(8, None);
        no_size.props.retain(|p| p.key != FORMAT_VIDEO_SIZE);
        assert!(settle(&no_size).is_err());
        let mut audio = proposal(8, None);
        audio.props[0].value = Value::Id(1);
        assert!(settle(&audio).is_err());
    }

    #[test]
    fn truncated_pods_are_errors_not_panics() {
        let pod = enum_format(Some(&[0]), 60).build();
        let bytes = pod.bytes();
        for len in 0..bytes.len() {
            let _ = Object::parse(&bytes[..len]);
        }
        let mut lying = bytes.to_vec();
        lying[0..4].copy_from_slice(&(u32::MAX).to_ne_bytes());
        assert!(Object::parse(&lying).is_err());
    }
}
