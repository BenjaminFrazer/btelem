//! Schema decode.

use crate::*;

/// Field primitive type. Numeric values match `enum btelem_type` in C.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FieldType {
    U8 = 0,
    U16 = 1,
    U32 = 2,
    U64 = 3,
    I8 = 4,
    I16 = 5,
    I32 = 6,
    I64 = 7,
    F32 = 8,
    F64 = 9,
    Bool = 10,
    Bytes = 11,
    Enum = 12,
    Bitfield = 13,
}

impl FieldType {
    fn from_u8(b: u8) -> Result<Self> {
        Ok(match b {
            0 => Self::U8,
            1 => Self::U16,
            2 => Self::U32,
            3 => Self::U64,
            4 => Self::I8,
            5 => Self::I16,
            6 => Self::I32,
            7 => Self::I64,
            8 => Self::F32,
            9 => Self::F64,
            10 => Self::Bool,
            11 => Self::Bytes,
            12 => Self::Enum,
            13 => Self::Bitfield,
            other => return Err(WireError::UnknownType(other)),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldDef {
    pub name: String,
    pub offset: u16,
    pub size: u16,
    pub ty: FieldType,
    pub count: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SchemaEntry {
    pub id: u16,
    pub name: String,
    pub description: String,
    pub payload_size: u16,
    pub fields: Vec<FieldDef>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumDef {
    pub schema_id: u16,
    pub field_index: u16,
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BitDef {
    pub name: String,
    pub start: u8,
    pub width: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BitfieldDef {
    pub schema_id: u16,
    pub field_index: u16,
    pub bits: Vec<BitDef>,
}

/// Fully-decoded schema descriptor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Schema {
    pub entries: Vec<SchemaEntry>,
    pub enums: Vec<EnumDef>,
    pub bitfields: Vec<BitfieldDef>,
}

impl Schema {
    /// Decode a serialised schema blob (the bytes that follow the u32 length
    /// prefix on the wire).
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let endian = read_u8(buf, 0)?;
        if endian != 0 {
            return Err(WireError::BadEndian(endian));
        }
        let entry_count = read_u16(buf, 1)? as usize;
        if entry_count > MAX_SCHEMA_ENTRIES {
            return Err(WireError::TooManyEntries(entry_count as u16));
        }

        let mut pos = SCHEMA_HEADER_SIZE;
        let mut entries = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            entries.push(decode_schema_entry(buf, pos)?);
            pos += SCHEMA_WIRE_SIZE;
        }

        // Optional enum section
        let mut enums = Vec::new();
        if pos + 2 <= buf.len() {
            let n = read_u16(buf, pos)? as usize;
            pos += 2;
            enums.reserve(n);
            for _ in 0..n {
                enums.push(decode_enum(buf, pos)?);
                pos += ENUM_WIRE_SIZE;
            }
        }

        // Optional bitfield section
        let mut bitfields = Vec::new();
        if pos + 2 <= buf.len() {
            let n = read_u16(buf, pos)? as usize;
            pos += 2;
            bitfields.reserve(n);
            for _ in 0..n {
                bitfields.push(decode_bitfield(buf, pos)?);
                pos += BITFIELD_WIRE_SIZE;
            }
        }

        Ok(Schema {
            entries,
            enums,
            bitfields,
        })
    }

    /// Find an entry by id.
    pub fn entry(&self, id: u16) -> Option<&SchemaEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Look up enum labels for a (schema_id, field_index) pair.
    pub fn enum_labels(&self, schema_id: u16, field_index: u16) -> Option<&[String]> {
        self.enums
            .iter()
            .find(|e| e.schema_id == schema_id && e.field_index == field_index)
            .map(|e| e.labels.as_slice())
    }

    /// Look up bitfield definition for a (schema_id, field_index) pair.
    pub fn bitfield(&self, schema_id: u16, field_index: u16) -> Option<&BitfieldDef> {
        self.bitfields
            .iter()
            .find(|b| b.schema_id == schema_id && b.field_index == field_index)
    }
}

fn decode_schema_entry(buf: &[u8], pos: usize) -> Result<SchemaEntry> {
    let id = read_u16(buf, pos)?;
    let payload_size = read_u16(buf, pos + 2)?;
    let field_count = read_u16(buf, pos + 4)? as usize;
    if field_count > MAX_FIELDS {
        return Err(WireError::TooManyFields(field_count as u16));
    }
    let name = read_cstr(buf, pos + 6, NAME_MAX)?;
    let description = read_cstr(buf, pos + 6 + NAME_MAX, DESC_MAX)?;

    let fpos = pos + SCHEMA_WIRE_HEADER_SIZE;
    let mut fields = Vec::with_capacity(field_count);
    for i in 0..field_count {
        let off = fpos + i * FIELD_WIRE_SIZE;
        let fname = read_cstr(buf, off, NAME_MAX)?;
        let foff = read_u16(buf, off + NAME_MAX)?;
        let fsize = read_u16(buf, off + NAME_MAX + 2)?;
        let fty = FieldType::from_u8(read_u8(buf, off + NAME_MAX + 4)?)?;
        let fcount = read_u8(buf, off + NAME_MAX + 5)?;
        fields.push(FieldDef {
            name: fname,
            offset: foff,
            size: fsize,
            ty: fty,
            count: fcount,
        });
    }
    Ok(SchemaEntry {
        id,
        name,
        description,
        payload_size,
        fields,
    })
}

fn decode_enum(buf: &[u8], pos: usize) -> Result<EnumDef> {
    let schema_id = read_u16(buf, pos)?;
    let field_index = read_u16(buf, pos + 2)?;
    let label_count = read_u8(buf, pos + 4)? as usize;
    let labels_pos = pos + 5;
    need(buf, labels_pos + ENUM_MAX_VALUES * ENUM_LABEL_MAX)?;
    let mut labels = Vec::with_capacity(label_count);
    for i in 0..label_count.min(ENUM_MAX_VALUES) {
        labels.push(read_cstr(
            buf,
            labels_pos + i * ENUM_LABEL_MAX,
            ENUM_LABEL_MAX,
        )?);
    }
    Ok(EnumDef {
        schema_id,
        field_index,
        labels,
    })
}

fn decode_bitfield(buf: &[u8], pos: usize) -> Result<BitfieldDef> {
    let schema_id = read_u16(buf, pos)?;
    let field_index = read_u16(buf, pos + 2)?;
    let bit_count = read_u8(buf, pos + 4)? as usize;
    let names_pos = pos + 5;
    let starts_pos = names_pos + BITFIELD_MAX_BITS * BIT_NAME_MAX;
    let widths_pos = starts_pos + BITFIELD_MAX_BITS;
    need(buf, widths_pos + BITFIELD_MAX_BITS)?;

    let mut bits = Vec::with_capacity(bit_count);
    for i in 0..bit_count.min(BITFIELD_MAX_BITS) {
        let name = read_cstr(buf, names_pos + i * BIT_NAME_MAX, BIT_NAME_MAX)?;
        let start = buf[starts_pos + i];
        let width = buf[widths_pos + i];
        bits.push(BitDef { name, start, width });
    }
    Ok(BitfieldDef {
        schema_id,
        field_index,
        bits,
    })
}
