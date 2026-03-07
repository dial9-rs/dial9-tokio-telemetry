use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, parse_macro_input};

#[proc_macro_derive(TraceEvent)]
pub fn derive_trace_event(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let name_str = name.to_string();
    let ref_name = format_ident!("{}Ref", name);

    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(f) => &f.named,
            _ => panic!("TraceEvent only supports named fields"),
        },
        _ => panic!("TraceEvent can only be derived for structs"),
    };

    let mut field_def_tokens = Vec::new();
    let mut encode_tokens = Vec::new();
    let mut ref_fields = Vec::new();
    let mut decode_tokens = Vec::new();

    for (i, field) in fields.iter().enumerate() {
        let field_name = field.ident.as_ref().unwrap();
        let field_name_str = field_name.to_string();
        let ty = &field.ty;

        field_def_tokens.push(quote! {
            ::dial9_trace_format::schema::FieldDef {
                name: #field_name_str.to_string(),
                field_type: <#ty as ::dial9_trace_format::TraceField>::field_type(),
            }
        });
        encode_tokens.push(quote! {
            ::dial9_trace_format::TraceField::encode_field(&self.#field_name, buf);
        });

        // For the Ref struct, use the same type (numeric types copy fine from FieldValueRef)
        ref_fields.push(quote! {
            pub #field_name: <#ty as ::dial9_trace_format::TraceField>::Ref<'a>
        });
        decode_tokens.push(quote! {
            #field_name: <#ty as ::dial9_trace_format::TraceField>::decode_ref(fields.get(#i)?)?
        });
    }

    let phantom_field = if fields.is_empty() {
        quote! { _marker: ::std::marker::PhantomData<&'a ()>, }
    } else {
        quote! {}
    };
    let phantom_init = if fields.is_empty() {
        quote! { _marker: ::std::marker::PhantomData, }
    } else {
        quote! {}
    };

    let expanded = quote! {
        #[derive(Debug, Clone)]
        pub struct #ref_name<'a> {
            #(#ref_fields,)*
            #phantom_field
        }

        impl ::dial9_trace_format::TraceEvent for #name {
            type Ref<'a> = #ref_name<'a>;

            fn event_name() -> &'static str { #name_str }
            fn field_defs() -> Vec<::dial9_trace_format::schema::FieldDef> {
                vec![#(#field_def_tokens),*]
            }
            fn encode_fields(&self, buf: &mut Vec<u8>) {
                #(#encode_tokens)*
            }
            fn decode<'a>(fields: &[::dial9_trace_format::types::FieldValueRef<'a>]) -> Option<Self::Ref<'a>> {
                Some(#ref_name {
                    #(#decode_tokens,)*
                    #phantom_init
                })
            }
        }
    };

    TokenStream::from(expanded)
}
