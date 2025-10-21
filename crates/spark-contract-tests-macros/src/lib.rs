use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Ident, ItemMod, Meta, Result as SynResult, Token, parse_macro_input};

#[proc_macro_attribute]
pub fn spark_tck(attr: TokenStream, item: TokenStream) -> TokenStream {
    let module = parse_macro_input!(item as ItemMod);

    match parse_suites(attr).and_then(|suites| inject_tests(suites, module)) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn parse_suites(attr: TokenStream) -> SynResult<Vec<Ident>> {
    if attr.is_empty() {
        return Ok(default_suite_idents());
    }

    let meta = syn::parse::<Meta>(attr)?;
    match meta {
        Meta::List(list) if list.path.is_ident("suites") => {
            let nested: Punctuated<Meta, Token![,]> =
                list.parse_args_with(Punctuated::parse_terminated)?;
            let mut suites = Vec::new();
            for meta in nested {
                match meta {
                    Meta::Path(path) => {
                        if let Some(ident) = path.get_ident() {
                            suites.push(ident.clone());
                        } else {
                            return Err(syn::Error::new(path.span(), "suite 需为标识符"));
                        }
                    }
                    other => {
                        return Err(syn::Error::new(other.span(), "suites(...) 仅接受标识符"));
                    }
                }
            }
            if suites.is_empty() {
                Ok(default_suite_idents())
            } else {
                Ok(suites)
            }
        }
        Meta::Path(path) if path.is_ident("suites") => Ok(default_suite_idents()),
        other => Err(syn::Error::new(
            other.span(),
            "spark_tck 属性仅支持 suites(...)",
        )),
    }
}

fn default_suite_idents() -> Vec<Ident> {
    [
        "backpressure",
        "cancellation",
        "errors",
        "state_machine",
        "hot_swap",
        "hot_reload",
        "observability",
    ]
    .iter()
    .map(|name| Ident::new(name, Span::call_site()))
    .collect()
}

fn inject_tests(suites: Vec<Ident>, mut module: ItemMod) -> SynResult<proc_macro2::TokenStream> {
    let mut generated = Vec::new();
    for suite in suites {
        let test_ident = format_ident!("{}_suite", suite);
        let run_fn: syn::Path =
            syn::parse_str(&format!("spark_contract_tests::run_{}_suite", suite))?;
        let item: syn::Item = syn::parse_quote! {
            #[test]
            fn #test_ident() {
                #run_fn();
            }
        };
        generated.push(item);
    }

    if let Some((_, ref mut items)) = module.content {
        items.extend(generated);
        Ok(quote! { #module })
    } else {
        let ident = &module.ident;
        let vis = &module.vis;
        let attrs = &module.attrs;
        Ok(quote! {
            #(#attrs)*
            #vis mod #ident {
                #(#generated)*
            }
        })
    }
}
