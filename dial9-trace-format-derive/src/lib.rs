use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, parse_macro_input};

fn derive_trace_event_impl(input: DeriveInput) -> proc_macro2::TokenStream {
    let name = &input.ident;
    let vis = &input.vis;
    let name_str = name.to_string();
    let ref_name = format_ident!("{}Ref", name);

    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(f) => &f.named,
            _ => panic!("TraceEvent only supports named fields"),
        },
        _ => panic!("TraceEvent can only be derived for structs"),
    };

    // Find the field marked with #[traceevent(timestamp)]
    let mut timestamp_field_name = None;
    for field in fields.iter() {
        for attr in &field.attrs {
            if attr.path().is_ident("traceevent") {
                let _ = attr.parse_nested_meta(|meta| {
                    if meta.path.is_ident("timestamp") {
                        timestamp_field_name = Some(field.ident.as_ref().unwrap().clone());
                    }
                    Ok(())
                });
            }
        }
    }

    let mut field_def_tokens = Vec::new();
    let mut encode_tokens = Vec::new();
    let mut ref_fields = Vec::new();
    let mut decode_tokens = Vec::new();
    let mut decode_idx = 0usize;

    for field in fields.iter() {
        let field_name = field.ident.as_ref().unwrap();
        let ty = &field.ty;

        // Skip the timestamp field in schema/encode — it's in the event header
        if timestamp_field_name.as_ref() == Some(field_name) {
            continue;
        }

        let field_name_str = field_name.to_string();
        field_def_tokens.push(quote! {
            ::dial9_trace_format::schema::FieldDef {
                name: #field_name_str.to_string(),
                field_type: <#ty as ::dial9_trace_format::TraceField>::field_type(),
            }
        });
        encode_tokens.push(quote! {
            <#ty as ::dial9_trace_format::TraceField>::encode(&self.#field_name, enc)?;
        });

        ref_fields.push(quote! {
            pub #field_name: <#ty as ::dial9_trace_format::TraceField>::Ref<'a>
        });
        let idx = decode_idx;
        decode_tokens.push(quote! {
            #field_name: <#ty as ::dial9_trace_format::TraceField>::decode_ref(fields.get(#idx)?)?
        });
        decode_idx += 1;
    }

    let timestamp_impl = if let Some(ref ts_field) = timestamp_field_name {
        quote! {
            fn timestamp(&self) -> u64 { self.#ts_field }
        }
    } else {
        panic!("TraceEvent requires a field marked with #[traceevent(timestamp)]");
    };

    let has_timestamp_impl = quote! {};

    // For the Ref struct, include the timestamp field if present — populated from the decode parameter
    let ref_timestamp_field = if let Some(ref ts_field) = timestamp_field_name {
        quote! { pub #ts_field: u64, }
    } else {
        quote! {}
    };
    let decode_timestamp_init = if let Some(ref ts_field) = timestamp_field_name {
        quote! { #ts_field: timestamp_ns?, }
    } else {
        quote! {}
    };

    let phantom_field =
        if fields.is_empty() || (fields.len() == 1 && timestamp_field_name.is_some()) {
            quote! { _marker: ::std::marker::PhantomData<&'a ()>, }
        } else {
            quote! {}
        };
    let phantom_init = if fields.is_empty() || (fields.len() == 1 && timestamp_field_name.is_some())
    {
        quote! { _marker: ::std::marker::PhantomData, }
    } else {
        quote! {}
    };

    quote! {
        #[derive(Debug, Clone)]
        #vis struct #ref_name<'a> {
            #ref_timestamp_field
            #(#ref_fields,)*
            #phantom_field
        }

        impl ::dial9_trace_format::TraceEvent for #name {
            type Ref<'a> = #ref_name<'a>;

            fn event_name() -> &'static str { #name_str }
            fn field_defs() -> Vec<::dial9_trace_format::schema::FieldDef> {
                vec![#(#field_def_tokens),*]
            }
            #timestamp_impl
            #has_timestamp_impl
            fn encode_fields<W: ::std::io::Write>(&self, enc: &mut ::dial9_trace_format::EventEncoder<'_, W>) -> ::std::io::Result<()> {
                #(#encode_tokens)*
                Ok(())
            }
            fn decode<'a>(timestamp_ns: Option<u64>, fields: &[::dial9_trace_format::types::FieldValueRef<'a>]) -> Option<Self::Ref<'a>> {
                Some(#ref_name {
                    #decode_timestamp_init
                    #(#decode_tokens,)*
                    #phantom_init
                })
            }
        }
    }
}

#[proc_macro_derive(TraceEvent, attributes(traceevent))]
pub fn derive_trace_event(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    TokenStream::from(derive_trace_event_impl(input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;

    fn expand_to_string(input: proc_macro2::TokenStream) -> String {
        let input: DeriveInput = syn::parse2(input).unwrap();
        let output = derive_trace_event_impl(input);
        match syn::parse2::<syn::File>(output.clone()) {
            Ok(file) => prettyplease::unparse(&file),
            Err(_) => output.to_string(),
        }
    }

    #[test]
    fn simple_event() {
        assert_snapshot!(expand_to_string(quote! {
            struct SimpleEvent {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                value: u32,
            }
        }));
    }

    #[test]
    fn empty_event() {
        assert_snapshot!(expand_to_string(quote! {
            struct EmptyEvent {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
            }
        }));
    }

    #[test]
    fn all_field_types() {
        assert_snapshot!(expand_to_string(quote! {
            struct AllFieldTypes {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
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
        }));
    }

    #[test]
    fn timestamp_attribute() {
        assert_snapshot!(expand_to_string(quote! {
            struct PollStart {
                #[traceevent(timestamp)]
                timestamp_ns: u64,
                worker_id: u64,
                task_id: u64,
            }
        }));
    }
}
