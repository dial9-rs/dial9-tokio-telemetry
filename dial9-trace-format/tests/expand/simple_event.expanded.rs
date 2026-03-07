use dial9_trace_format::TraceEvent;
struct SimpleEvent {
    timestamp_ns: u64,
    value: u32,
}
pub struct SimpleEventRef<'a> {
    pub timestamp_ns: <u64 as ::dial9_trace_format::TraceField>::Ref<'a>,
    pub value: <u32 as ::dial9_trace_format::TraceField>::Ref<'a>,
}
#[automatically_derived]
impl<'a> ::core::fmt::Debug for SimpleEventRef<'a> {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        ::core::fmt::Formatter::debug_struct_field2_finish(
            f,
            "SimpleEventRef",
            "timestamp_ns",
            &self.timestamp_ns,
            "value",
            &&self.value,
        )
    }
}
#[automatically_derived]
impl<'a> ::core::clone::Clone for SimpleEventRef<'a> {
    #[inline]
    fn clone(&self) -> SimpleEventRef<'a> {
        SimpleEventRef {
            timestamp_ns: ::core::clone::Clone::clone(&self.timestamp_ns),
            value: ::core::clone::Clone::clone(&self.value),
        }
    }
}
impl ::dial9_trace_format::TraceEvent for SimpleEvent {
    type Ref<'a> = SimpleEventRef<'a>;
    fn event_name() -> &'static str {
        "SimpleEvent"
    }
    fn field_defs() -> Vec<::dial9_trace_format::schema::FieldDef> {
        <[_]>::into_vec(
            ::alloc::boxed::box_new([
                ::dial9_trace_format::schema::FieldDef {
                    name: "timestamp_ns".to_string(),
                    field_type: <u64 as ::dial9_trace_format::TraceField>::field_type(),
                },
                ::dial9_trace_format::schema::FieldDef {
                    name: "value".to_string(),
                    field_type: <u32 as ::dial9_trace_format::TraceField>::field_type(),
                },
            ]),
        )
    }
    fn encode_fields(&self, buf: &mut Vec<u8>) {
        ::dial9_trace_format::TraceField::encode_field(&self.timestamp_ns, buf);
        ::dial9_trace_format::TraceField::encode_field(&self.value, buf);
    }
    fn decode<'a>(
        fields: &[::dial9_trace_format::types::FieldValueRef<'a>],
    ) -> Option<Self::Ref<'a>> {
        Some(SimpleEventRef {
            timestamp_ns: <u64 as ::dial9_trace_format::TraceField>::decode_ref(
                fields.get(0usize)?,
            )?,
            value: <u32 as ::dial9_trace_format::TraceField>::decode_ref(
                fields.get(1usize)?,
            )?,
        })
    }
}
fn main() {}
