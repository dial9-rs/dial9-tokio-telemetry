use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{ItemFn, Path, Token, parse_macro_input};

struct MainArgs {
    config: Path,
}

impl Parse for MainArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let ident: syn::Ident = input.parse()?;
        if ident != "config" {
            return Err(syn::Error::new(ident.span(), "expected `config`"));
        }
        input.parse::<Token![=]>()?;
        let config: Path = input.parse()?;
        Ok(MainArgs { config })
    }
}

fn expand_main(args: MainArgs, input: ItemFn) -> Result<TokenStream2, syn::Error> {
    if input.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            input.sig.fn_token,
            "the `async` keyword is missing from the function declaration",
        ));
    }

    let config_fn = &args.config;
    let attrs = &input.attrs;
    let vis = &input.vis;
    let name = &input.sig.ident;
    let ret = &input.sig.output;
    let body_stmts = &input.block.stmts;

    Ok(quote! {
        #(#attrs)*
        #vis fn #name() #ret {
            let __dial9_config = #config_fn();
            let (__dial9_runtime, __dial9_guard) = __dial9_config
                .build()
                .expect("failed to initialize dial9 runtime");
            let __dial9_handle = __dial9_guard.handle();
            let __dial9_body_handle = __dial9_handle.clone();
            __dial9_runtime.block_on(async move {
                __dial9_handle
                    .spawn(async move {
                        let handle = __dial9_body_handle;
                        #(#body_stmts)*
                    })
                    .await
                    .expect("dial9 root task panicked")
            })
        }
    })
}

/// Instrument an async main function with dial9 telemetry.
///
/// The macro wraps the function body in a spawned task so that poll events
/// are recorded by dial9. Without this, code running directly in
/// `runtime.block_on(...)` is invisible to the telemetry hooks.
///
/// A `handle` binding of type [`TelemetryHandle`] is available inside the
/// function body for spawning sub-tasks with wake-event tracking.
///
/// # Arguments
///
/// * `config` — path to a function returning [`Dial9Config`]. The function
///   is called at startup to obtain the runtime and writer configuration.
///
/// # Example
///
/// ```rust,ignore
/// use dial9_tokio_telemetry::{main, config::Dial9Config};
///
/// fn my_config() -> Dial9Config {
///     Dial9Config::builder()
///         .base_path("/tmp/trace.bin")
///         .max_file_size(1024 * 1024)
///         .max_total_size(16 * 1024 * 1024)
///         .build()
/// }
///
/// #[dial9_tokio_telemetry::main(config = my_config)]
/// async fn main() {
///     // `handle` is automatically available for spawning sub-tasks.
///     handle.spawn(async { /* instrumented sub-task */ }).await.unwrap();
/// }
/// ```
#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as MainArgs);
    let input = parse_macro_input!(item as ItemFn);

    match expand_main(args, input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    fn expand(attr: TokenStream2, item: TokenStream2) -> String {
        let args: MainArgs = syn::parse2(attr).expect("failed to parse args");
        let input: ItemFn = syn::parse2(item).expect("failed to parse fn");
        let expanded = expand_main(args, input).expect("expansion failed");
        let file = syn::parse2(expanded).expect("failed to parse expansion");
        prettyplease::unparse(&file)
    }

    #[test]
    fn expand_basic() {
        let output = expand(
            quote! { config = my_config },
            quote! {
                async fn main() {
                    do_work().await;
                }
            },
        );
        insta::assert_snapshot!(output);
    }

    #[test]
    fn expand_with_return_type() {
        let output = expand(
            quote! { config = my_config },
            quote! {
                async fn main() -> Result<(), Box<dyn std::error::Error>> {
                    do_work().await?;
                    Ok(())
                }
            },
        );
        insta::assert_snapshot!(output);
    }

    #[test]
    fn expand_with_attributes() {
        let output = expand(
            quote! { config = my_config },
            quote! {
                #[allow(unused)]
                async fn main() {
                    let _ = 42;
                }
            },
        );
        insta::assert_snapshot!(output);
    }

    #[test]
    fn error_not_async() {
        let args: MainArgs =
            syn::parse2(quote! { config = my_config }).expect("failed to parse args");
        let input: ItemFn = syn::parse2(quote! {
            fn main() {}
        })
        .expect("failed to parse fn");
        let err = expand_main(args, input).expect_err("expected error for non-async fn");
        let msg = err.to_string();
        assert!(
            msg.contains("async"),
            "error should mention async: {msg}"
        );
    }
}
