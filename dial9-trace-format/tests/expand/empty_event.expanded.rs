use dial9_trace_format::TraceEvent;
struct EmptyEvent {}
pub struct EmptyEventRef<'a> {
    _marker: ::std::marker::PhantomData<&'a ()>,
}
#[automatically_derived]
impl<'a> ::core::fmt::Debug for EmptyEventRef<'a> {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        ::core::fmt::Formatter::debug_struct_field1_finish(
            f,
            "EmptyEventRef",
            "_marker",
            &&self._marker,
        )
    }
}
#[automatically_derived]
impl<'a> ::core::clone::Clone for EmptyEventRef<'a> {
    #[inline]
    fn clone(&self) -> EmptyEventRef<'a> {
        EmptyEventRef {
            _marker: ::core::clone::Clone::clone(&self._marker),
        }
    }
}
impl ::dial9_trace_format::TraceEvent for EmptyEvent {
    type Ref<'a> = EmptyEventRef<'a>;
    fn event_name() -> &'static str {
        "EmptyEvent"
    }
    fn field_defs() -> Vec<::dial9_trace_format::schema::FieldDef> {
        ::alloc::vec::Vec::new()
    }
    fn encode_fields(&self, buf: &mut Vec<u8>) {}
    fn decode<'a>(
        fields: &[::dial9_trace_format::types::FieldValueRef<'a>],
    ) -> Option<Self::Ref<'a>> {
        Some(EmptyEventRef {
            _marker: ::std::marker::PhantomData,
        })
    }
}
fn main() {}
