use dial9_trace_format::{InternedString, StackFrames, TraceEvent};
struct AllFieldTypes {
    a_u8: u8,
    b_u16: u16,
    c_u32: u32,
    d_u64: u64,
    e_i64: i64,
    f_f64: f64,
    g_bool: bool,
    h_string: String,
    i_bytes: Vec<u8>,
    j_interned: InternedString,
    k_frames: StackFrames,
    l_map: Vec<(String, String)>,
}
pub struct AllFieldTypesRef<'a> {
    pub a_u8: <u8 as ::dial9_trace_format::TraceField>::Ref<'a>,
    pub b_u16: <u16 as ::dial9_trace_format::TraceField>::Ref<'a>,
    pub c_u32: <u32 as ::dial9_trace_format::TraceField>::Ref<'a>,
    pub d_u64: <u64 as ::dial9_trace_format::TraceField>::Ref<'a>,
    pub e_i64: <i64 as ::dial9_trace_format::TraceField>::Ref<'a>,
    pub f_f64: <f64 as ::dial9_trace_format::TraceField>::Ref<'a>,
    pub g_bool: <bool as ::dial9_trace_format::TraceField>::Ref<'a>,
    pub h_string: <String as ::dial9_trace_format::TraceField>::Ref<'a>,
    pub i_bytes: <Vec<u8> as ::dial9_trace_format::TraceField>::Ref<'a>,
    pub j_interned: <InternedString as ::dial9_trace_format::TraceField>::Ref<'a>,
    pub k_frames: <StackFrames as ::dial9_trace_format::TraceField>::Ref<'a>,
    pub l_map: <Vec<(String, String)> as ::dial9_trace_format::TraceField>::Ref<'a>,
}
#[automatically_derived]
impl<'a> ::core::fmt::Debug for AllFieldTypesRef<'a> {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        let names: &'static _ = &[
            "a_u8",
            "b_u16",
            "c_u32",
            "d_u64",
            "e_i64",
            "f_f64",
            "g_bool",
            "h_string",
            "i_bytes",
            "j_interned",
            "k_frames",
            "l_map",
        ];
        let values: &[&dyn ::core::fmt::Debug] = &[
            &self.a_u8,
            &self.b_u16,
            &self.c_u32,
            &self.d_u64,
            &self.e_i64,
            &self.f_f64,
            &self.g_bool,
            &self.h_string,
            &self.i_bytes,
            &self.j_interned,
            &self.k_frames,
            &&self.l_map,
        ];
        ::core::fmt::Formatter::debug_struct_fields_finish(
            f,
            "AllFieldTypesRef",
            names,
            values,
        )
    }
}
#[automatically_derived]
impl<'a> ::core::clone::Clone for AllFieldTypesRef<'a> {
    #[inline]
    fn clone(&self) -> AllFieldTypesRef<'a> {
        AllFieldTypesRef {
            a_u8: ::core::clone::Clone::clone(&self.a_u8),
            b_u16: ::core::clone::Clone::clone(&self.b_u16),
            c_u32: ::core::clone::Clone::clone(&self.c_u32),
            d_u64: ::core::clone::Clone::clone(&self.d_u64),
            e_i64: ::core::clone::Clone::clone(&self.e_i64),
            f_f64: ::core::clone::Clone::clone(&self.f_f64),
            g_bool: ::core::clone::Clone::clone(&self.g_bool),
            h_string: ::core::clone::Clone::clone(&self.h_string),
            i_bytes: ::core::clone::Clone::clone(&self.i_bytes),
            j_interned: ::core::clone::Clone::clone(&self.j_interned),
            k_frames: ::core::clone::Clone::clone(&self.k_frames),
            l_map: ::core::clone::Clone::clone(&self.l_map),
        }
    }
}
impl ::dial9_trace_format::TraceEvent for AllFieldTypes {
    type Ref<'a> = AllFieldTypesRef<'a>;
    fn event_name() -> &'static str {
        "AllFieldTypes"
    }
    fn field_defs() -> Vec<::dial9_trace_format::schema::FieldDef> {
        <[_]>::into_vec(
            ::alloc::boxed::box_new([
                ::dial9_trace_format::schema::FieldDef {
                    name: "a_u8".to_string(),
                    field_type: <u8 as ::dial9_trace_format::TraceField>::field_type(),
                },
                ::dial9_trace_format::schema::FieldDef {
                    name: "b_u16".to_string(),
                    field_type: <u16 as ::dial9_trace_format::TraceField>::field_type(),
                },
                ::dial9_trace_format::schema::FieldDef {
                    name: "c_u32".to_string(),
                    field_type: <u32 as ::dial9_trace_format::TraceField>::field_type(),
                },
                ::dial9_trace_format::schema::FieldDef {
                    name: "d_u64".to_string(),
                    field_type: <u64 as ::dial9_trace_format::TraceField>::field_type(),
                },
                ::dial9_trace_format::schema::FieldDef {
                    name: "e_i64".to_string(),
                    field_type: <i64 as ::dial9_trace_format::TraceField>::field_type(),
                },
                ::dial9_trace_format::schema::FieldDef {
                    name: "f_f64".to_string(),
                    field_type: <f64 as ::dial9_trace_format::TraceField>::field_type(),
                },
                ::dial9_trace_format::schema::FieldDef {
                    name: "g_bool".to_string(),
                    field_type: <bool as ::dial9_trace_format::TraceField>::field_type(),
                },
                ::dial9_trace_format::schema::FieldDef {
                    name: "h_string".to_string(),
                    field_type: <String as ::dial9_trace_format::TraceField>::field_type(),
                },
                ::dial9_trace_format::schema::FieldDef {
                    name: "i_bytes".to_string(),
                    field_type: <Vec<
                        u8,
                    > as ::dial9_trace_format::TraceField>::field_type(),
                },
                ::dial9_trace_format::schema::FieldDef {
                    name: "j_interned".to_string(),
                    field_type: <InternedString as ::dial9_trace_format::TraceField>::field_type(),
                },
                ::dial9_trace_format::schema::FieldDef {
                    name: "k_frames".to_string(),
                    field_type: <StackFrames as ::dial9_trace_format::TraceField>::field_type(),
                },
                ::dial9_trace_format::schema::FieldDef {
                    name: "l_map".to_string(),
                    field_type: <Vec<
                        (String, String),
                    > as ::dial9_trace_format::TraceField>::field_type(),
                },
            ]),
        )
    }
    fn encode_fields(&self, buf: &mut Vec<u8>) {
        ::dial9_trace_format::TraceField::encode_field(&self.a_u8, buf);
        ::dial9_trace_format::TraceField::encode_field(&self.b_u16, buf);
        ::dial9_trace_format::TraceField::encode_field(&self.c_u32, buf);
        ::dial9_trace_format::TraceField::encode_field(&self.d_u64, buf);
        ::dial9_trace_format::TraceField::encode_field(&self.e_i64, buf);
        ::dial9_trace_format::TraceField::encode_field(&self.f_f64, buf);
        ::dial9_trace_format::TraceField::encode_field(&self.g_bool, buf);
        ::dial9_trace_format::TraceField::encode_field(&self.h_string, buf);
        ::dial9_trace_format::TraceField::encode_field(&self.i_bytes, buf);
        ::dial9_trace_format::TraceField::encode_field(&self.j_interned, buf);
        ::dial9_trace_format::TraceField::encode_field(&self.k_frames, buf);
        ::dial9_trace_format::TraceField::encode_field(&self.l_map, buf);
    }
    fn decode<'a>(
        fields: &[::dial9_trace_format::types::FieldValueRef<'a>],
    ) -> Option<Self::Ref<'a>> {
        Some(AllFieldTypesRef {
            a_u8: <u8 as ::dial9_trace_format::TraceField>::decode_ref(
                fields.get(0usize)?,
            )?,
            b_u16: <u16 as ::dial9_trace_format::TraceField>::decode_ref(
                fields.get(1usize)?,
            )?,
            c_u32: <u32 as ::dial9_trace_format::TraceField>::decode_ref(
                fields.get(2usize)?,
            )?,
            d_u64: <u64 as ::dial9_trace_format::TraceField>::decode_ref(
                fields.get(3usize)?,
            )?,
            e_i64: <i64 as ::dial9_trace_format::TraceField>::decode_ref(
                fields.get(4usize)?,
            )?,
            f_f64: <f64 as ::dial9_trace_format::TraceField>::decode_ref(
                fields.get(5usize)?,
            )?,
            g_bool: <bool as ::dial9_trace_format::TraceField>::decode_ref(
                fields.get(6usize)?,
            )?,
            h_string: <String as ::dial9_trace_format::TraceField>::decode_ref(
                fields.get(7usize)?,
            )?,
            i_bytes: <Vec<
                u8,
            > as ::dial9_trace_format::TraceField>::decode_ref(fields.get(8usize)?)?,
            j_interned: <InternedString as ::dial9_trace_format::TraceField>::decode_ref(
                fields.get(9usize)?,
            )?,
            k_frames: <StackFrames as ::dial9_trace_format::TraceField>::decode_ref(
                fields.get(10usize)?,
            )?,
            l_map: <Vec<
                (String, String),
            > as ::dial9_trace_format::TraceField>::decode_ref(fields.get(11usize)?)?,
        })
    }
}
fn main() {}
