use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{ItemFn, Path, Token, parse_macro_input};

struct MainArgs {
    config: Path,
}

const MISSING_CONFIG_HELP: &str = "missing required `config = <fn>` argument, \
                           e.g. #[dial9_tokio_telemetry::main(config = my_config)]";

const CONFIG_MUST_BE_ZERO_ARG_HELP: &str = "`config` must be a path to a zero-argument function, \
                           e.g. #[dial9_tokio_telemetry::main(config = my_config)]";
impl Parse for MainArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.is_empty() {
            return Err(input.error(MISSING_CONFIG_HELP));
        }
        let ident: syn::Ident = input.parse()?;
        if ident != "config" {
            return Err(syn::Error::new(ident.span(), MISSING_CONFIG_HELP));
        }
        input.parse::<Token![=]>()?;
        let config: Path = input.parse()?;
        if !input.is_empty() {
            return Err(input.error(CONFIG_MUST_BE_ZERO_ARG_HELP));
        }
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

    if !input.sig.inputs.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.sig.inputs,
            "#[dial9_tokio_telemetry::main] does not support function arguments",
        ));
    }

    if !input.sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.sig.generics,
            "#[dial9_tokio_telemetry::main] does not support generics",
        ));
    }

    if input.sig.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &input.sig.generics.where_clause,
            "#[dial9_tokio_telemetry::main] does not support where clauses",
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
            __dial9_runtime.block_on(async move {
                match __dial9_handle.spawn(async move { #(#body_stmts)* }).await {
                    Ok(output) => output,
                    Err(err) if err.is_panic() => {
                        ::std::panic::resume_unwind(err.into_panic())
                    }
                    Err(_) => unreachable!("task cannot be cancelled inside block_on"),
                }
            })
        }
    })
}

/// Instrument an async main function with dial9 telemetry.
///
/// This macro is a **replacement** for `#[tokio::main]`, not a complement —
/// do not use both attributes on the same function. It builds the Tokio
/// runtime internally and wraps the function body in a spawned task so that
/// poll events are recorded by dial9. Without this, code running directly in
/// `runtime.block_on(...)` is invisible to the telemetry hooks.
///
/// To spawn sub-tasks with wake-event tracking from anywhere inside the
/// body, call `TelemetryHandle::current()` — the handle is installed on
/// every runtime-owned thread by `on_thread_start`.
///
/// # Arguments
///
/// * `config` — path to a function returning [`Dial9Config`]. The function
///   is called at startup to obtain the runtime and writer configuration.
///
/// # Example
///
/// ```rust,ignore
/// use dial9_tokio_telemetry::{main, config::Dial9Config, telemetry::TelemetryHandle};
///
/// fn my_config() -> Dial9Config {
///     Dial9Config::new("/tmp/trace.bin", 1024 * 1024, 16 * 1024 * 1024)
/// }
///
/// #[dial9_tokio_telemetry::main(config = my_config)]
/// async fn main() {
///     let handle = TelemetryHandle::current();
///     handle
///         .spawn(async { /* instrumented sub-task */ })
///         .await
///         .unwrap();
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

    fn expand_err(attr: TokenStream2, item: TokenStream2) -> String {
        let args: MainArgs = syn::parse2(attr).expect("failed to parse args");
        let input: ItemFn = syn::parse2(item).expect("failed to parse fn");
        expand_main(args, input)
            .expect_err("expected error")
            .to_string()
    }

    #[test]
    fn error_with_arguments() {
        let msg = expand_err(
            quote! { config = my_config },
            quote! { async fn main(port: u16) {} },
        );
        assert!(
            msg.contains("does not support function arguments"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn error_with_generics() {
        let msg = expand_err(
            quote! { config = my_config },
            quote! { async fn main<T>() {} },
        );
        assert!(
            msg.contains("does not support generics"),
            "unexpected error: {msg}"
        );
    }

    fn parse_args_err(attr: TokenStream2) -> String {
        match syn::parse2::<MainArgs>(attr) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected parse error"),
        }
    }

    #[test]
    fn error_empty_args() {
        let msg = parse_args_err(quote! {});
        assert!(msg.contains("config = <fn>"), "unexpected error: {msg}");
    }

    #[test]
    fn error_wrong_arg_name() {
        let msg = parse_args_err(quote! { foo = bar });
        assert!(msg.contains("config = <fn>"), "unexpected error: {msg}");
    }

    #[test]
    fn error_config_with_args() {
        let msg = parse_args_err(quote! { config = my_config(arg) });
        assert!(
            msg.contains("zero-argument function"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn error_config_trailing_tokens() {
        let msg = parse_args_err(quote! { config = my_config, extra = stuff });
        assert!(
            msg.contains("zero-argument function"),
            "unexpected error: {msg}"
        );
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
        assert!(msg.contains("async"), "error should mention async: {msg}");
    }
}
