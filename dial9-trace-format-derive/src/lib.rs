use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, parse_macro_input};

#[proc_macro_derive(TraceEvent)]
pub fn derive_trace_event(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let name_str = name.to_string();

    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(f) => &f.named,
            _ => panic!("TraceEvent only supports named fields"),
        },
        _ => panic!("TraceEvent can only be derived for structs"),
    };

    let mut field_def_tokens = Vec::new();
    let mut encode_tokens = Vec::new();

    for field in fields {
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
    }

    let expanded = quote! {
        impl ::dial9_trace_format::TraceEvent for #name {
            fn event_name() -> &'static str { #name_str }
            fn field_defs() -> Vec<::dial9_trace_format::schema::FieldDef> {
                vec![#(#field_def_tokens),*]
            }
            fn encode_fields(&self, buf: &mut Vec<u8>) {
                #(#encode_tokens)*
            }
        }
    };

    TokenStream::from(expanded)
}
