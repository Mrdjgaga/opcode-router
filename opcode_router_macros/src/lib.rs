use proc_macro::TokenStream;
use std::collections::HashMap;
use std::path::PathBuf;

use proc_macro_crate::{FoundCrate, crate_name};
use quote::{ToTokens, quote};
use syn::{Error, Expr, FnArg, Ident, ImplItem, Item, ItemImpl, Lit, LitStr, Pat, PatType, Path, Signature, Token, Type, Visibility, parse::Parser, parse_macro_input};

struct ReflectedMethod {
    ident: Ident,
    id: Option<Expr>,
    is_async: bool,
}

#[derive(Clone)]
struct MethodParam {
    pat: Pat,
    ident: Ident,
    ty: Type,
}

#[proc_macro_attribute]
pub fn reflect_methods(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (options, attr_errors) = parse_reflect_attr(attr);
    let mut errors = attr_errors;
    let id_ty = options.id_ty;
    let name_fn = options.name_fn;
    let id_fn = options.id_fn;
    let mut impl_block = parse_macro_input!(item as ItemImpl);

    if options.collect_only {
        strip_reflect_attrs(&mut impl_block, true);
        let error_tokens = errors.iter().map(Error::to_compile_error);

        return quote! {
            #(#error_tokens)*
            #impl_block
        }
        .into();
    }

    let self_ty = impl_block.self_ty.clone();
    let generics = impl_block.generics.clone();

    if impl_block.trait_.is_some() {
        return Error::new_spanned(impl_block.impl_token, "reflect_methods only supports inherent impl blocks")
            .to_compile_error()
            .into();
    }

    let mut reflected_methods: Vec<ReflectedMethod> = Vec::new();
    let mut dispatch_params: Option<Vec<MethodParam>> = None;
    for item in &mut impl_block.items {
        let ImplItem::Fn(method) = item else {
            continue;
        };

        let is_callable = method.attrs.iter().any(is_route_attr);
        let mut callable_id = None;

        for attr in method.attrs.iter().filter(|attr| is_route_attr(attr)) {
            if let Err(error) = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("id") {
                    if callable_id.is_some() {
                        return Err(meta.error("duplicate callable id"));
                    }

                    callable_id = Some(meta.value()?.parse::<Expr>()?);
                    return Ok(());
                }

                Err(meta.error("unsupported callable option"))
            }) {
                errors.push(error);
            }
        }

        let is_ignored = method.attrs.iter().any(|attr| attr.path().is_ident("reflect_ignore"));

        method.attrs.retain(|attr| !is_route_attr(attr) && !attr.path().is_ident("reflect_ignore"));

        if is_ignored {
            continue;
        }

        if !is_callable {
            continue;
        }

        if callable_id.is_none() {
            errors.push(Error::new_spanned(method.sig.to_token_stream(), "routed methods require an id: #[route(id = ...)]"));
            continue;
        }

        if !validate_method_qualifiers(&method.sig, &mut errors) {
            continue;
        }

        let has_receiver = method.sig.inputs.iter().any(|arg| matches!(arg, FnArg::Receiver(_)));

        if !has_receiver {
            errors.push(Error::new_spanned(method.sig.to_token_stream(), "reflected methods must take &self or &mut self"));
            continue;
        }

        let mut params = Vec::new();

        for arg in method.sig.inputs.iter() {
            match arg {
                FnArg::Receiver(receiver) => {
                    if receiver.reference.is_none() {
                        errors.push(Error::new_spanned(receiver.to_token_stream(), "reflected methods cannot take self by value"));
                    }
                }
                FnArg::Typed(PatType { pat, ty, .. }) => {
                    let Pat::Ident(pat_ident) = pat.as_ref() else {
                        errors.push(Error::new_spanned(pat.to_token_stream(), "reflected method parameters must use simple names"));
                        continue;
                    };

                    params.push(MethodParam {
                        pat: pat.as_ref().clone(),
                        ident: pat_ident.ident.clone(),
                        ty: ty.as_ref().clone(),
                    });
                }
            }
        }

        if let Some(expected_params) = &dispatch_params {
            if !same_param_types(expected_params, &params) {
                errors.push(Error::new_spanned(
                    method.sig.to_token_stream(),
                    "all reflected methods in the same impl must use the same parameter types",
                ));
                continue;
            }
        } else {
            dispatch_params = Some(params.clone());
        }

        reflected_methods.push(ReflectedMethod {
            ident: method.sig.ident.clone(),
            id: callable_id,
            is_async: method.sig.asyncness.is_some(),
        });
    }

    validate_reflected_methods(&reflected_methods, &mut errors);

    let error_tokens: Vec<_> = errors.iter().map(Error::to_compile_error).collect();
    let runtime_crate = runtime_crate_path();
    let (impl_generics, _ty_generics, where_clause) = generics.split_for_impl();
    let dispatch_params = dispatch_params.unwrap_or_default();
    let dispatch_param_defs = dispatch_params.iter().map(|param| {
        let pat = &param.pat;
        let ty = &param.ty;
        quote! { #pat: #ty }
    });
    let has_async_methods = reflected_methods.iter().any(|method| method.is_async);
    let maybe_async = has_async_methods.then(|| quote! { async });
    let name_match_arms = reflected_methods.iter().map(|method| {
        let method_ident = &method.ident;
        let dispatch_arg_idents = dispatch_params.iter().map(|param| &param.ident);
        let method_call = if method.is_async {
            quote! { self.#method_ident(#(#dispatch_arg_idents),*).await }
        } else {
            quote! { self.#method_ident(#(#dispatch_arg_idents),*) }
        };

        quote! {
            stringify!(#method_ident) => #runtime_crate::IntoCallResult::into_call_result(#method_call)
        }
    });
    let id_match_arms = reflected_methods.iter().filter_map(|method| {
        let id = method.id.as_ref()?;
        if !is_valid_route_id(id) {
            return None;
        }
        let method_ident = &method.ident;
        let dispatch_arg_idents = dispatch_params.iter().map(|param| &param.ident);
        let method_call = if method.is_async {
            quote! { self.#method_ident(#(#dispatch_arg_idents),*).await }
        } else {
            quote! { self.#method_ident(#(#dispatch_arg_idents),*) }
        };

        Some(quote! {
            #id => #runtime_crate::IntoCallResult::into_call_result(#method_call)
        })
    });
    let has_id_methods = reflected_methods.iter().any(|method| method.id.is_some());
    let call_by_id = has_id_methods.then(|| {
        let dispatch_param_defs = dispatch_params.iter().map(|param| {
            let pat = &param.pat;
            let ty = &param.ty;
            quote! { #pat: #ty }
        });

        quote! {
            #[deny(unreachable_patterns)]
            pub #maybe_async fn #id_fn(&mut self, id: #id_ty, #(#dispatch_param_defs),*) -> #runtime_crate::CallResult {
                #(#error_tokens)*

                match id {
                    #(#id_match_arms,)*
                    _ => Err(#runtime_crate::CallError::IdNotFound(format!("{id:?}"))),
                }
            }
        }
    });

    quote! {
        #impl_block

        impl #impl_generics #self_ty #where_clause {
            pub #maybe_async fn #name_fn(&mut self, name: &str, #(#dispatch_param_defs),*) -> #runtime_crate::CallResult {
                #(#error_tokens)*

                match name {
                    #(#name_match_arms,)*
                    _ => Err(#runtime_crate::CallError::NameNotFound(name.to_string())),
                }
            }

            #call_by_id
        }
    }
    .into()
}

/// Preferred name for [`reflect_methods`].
#[proc_macro_attribute]
pub fn route_methods(attr: TokenStream, item: TokenStream) -> TokenStream {
    reflect_methods(attr, item)
}

struct DistributedRoutesOptions {
    registry: Path,
    entry: Path,
}

fn parse_distributed_routes_attr(attr: TokenStream) -> syn::Result<DistributedRoutesOptions> {
    let mut registry = None;
    let mut entry = None;

    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("registry") {
            registry = Some(meta.value()?.parse::<Path>()?);
            return Ok(());
        }
        if meta.path.is_ident("entry") {
            entry = Some(meta.value()?.parse::<Path>()?);
            return Ok(());
        }
        Err(meta.error("unsupported distributed_routes option"))
    });
    parser.parse(attr)?;

    Ok(DistributedRoutesOptions {
        registry: registry.ok_or_else(|| Error::new(proc_macro2::Span::call_site(), "missing registry = ..."))?,
        entry: entry.ok_or_else(|| Error::new(proc_macro2::Span::call_site(), "missing entry = ..."))?,
    })
}

/// Registers marked methods into a `linkme` distributed route registry.
#[proc_macro_attribute]
pub fn distributed_routes(attr: TokenStream, item: TokenStream) -> TokenStream {
    let options = match parse_distributed_routes_attr(attr) {
        Ok(options) => options,
        Err(error) => return error.to_compile_error().into(),
    };
    let mut impl_block = parse_macro_input!(item as ItemImpl);
    if impl_block.trait_.is_some() {
        return Error::new_spanned(impl_block.impl_token, "distributed_routes only supports inherent impl blocks")
            .to_compile_error()
            .into();
    }

    let runtime_crate = runtime_crate_path();
    let registry = options.registry;
    let entry = options.entry;
    let self_ty = impl_block.self_ty.clone();
    let target_name = self_ty
        .to_token_stream()
        .to_string()
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
        .collect::<String>();
    let mut errors = Vec::new();
    let mut reflected_methods = Vec::new();
    let mut registrations = Vec::new();

    for item in &mut impl_block.items {
        let ImplItem::Fn(method) = item else {
            continue;
        };
        if !method.attrs.iter().any(is_route_attr) {
            continue;
        }

        let mut route_id = None;
        for attr in method.attrs.iter().filter(|attr| is_route_attr(attr)) {
            if let Err(error) = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("id") {
                    if route_id.is_some() {
                        return Err(meta.error("duplicate route id"));
                    }
                    route_id = Some(meta.value()?.parse::<Expr>()?);
                    return Ok(());
                }
                Err(meta.error("unsupported route option"))
            }) {
                errors.push(error);
            }
        }
        method.attrs.retain(|attr| !is_route_attr(attr));

        let Some(route_id) = route_id else {
            errors.push(Error::new_spanned(method.sig.to_token_stream(), "routed methods require an id: #[route(id = ...)]"));
            continue;
        };

        if !validate_method_qualifiers(&method.sig, &mut errors) {
            continue;
        }

        let Some(receiver) = method.sig.receiver() else {
            errors.push(Error::new_spanned(method.sig.to_token_stream(), "routed methods must take &self or &mut self"));
            continue;
        };
        if receiver.reference.is_none() {
            errors.push(Error::new_spanned(receiver.to_token_stream(), "routed methods cannot take self by value"));
            continue;
        }

        let mut argument_idents = Vec::new();
        for argument in method.sig.inputs.iter().skip(1) {
            let FnArg::Typed(argument) = argument else {
                continue;
            };
            let Pat::Ident(pattern) = argument.pat.as_ref() else {
                errors.push(Error::new_spanned(argument.pat.to_token_stream(), "routed method parameters must use simple names"));
                continue;
            };
            argument_idents.push(pattern.ident.clone());
        }

        let method_ident = method.sig.ident.clone();
        let static_ident = Ident::new(&format!("__OPCODE_ROUTE_{}_{}", target_name, method_ident).to_uppercase(), method_ident.span());
        let destructure_args = match argument_idents.len() {
            0 => quote! { let () = args; },
            1 => {
                let argument = &argument_idents[0];
                quote! { let (#argument,) = args; }
            }
            _ => quote! { let (#(#argument_idents),*) = args; },
        };
        let method_call = if method.sig.asyncness.is_some() {
            quote! { target.#method_ident(#(#argument_idents),*).await }
        } else {
            quote! { target.#method_ident(#(#argument_idents),*) }
        };
        let distributed_slice = quote! { #runtime_crate::__private::distributed_slice };

        registrations.push(quote! {
            #[#distributed_slice(#registry)]
            #[linkme(crate = #runtime_crate::__private::linkme)]
            #[allow(non_upper_case_globals)]
            static #static_ident: #entry = #entry::new(
                #route_id,
                concat!(module_path!(), "::", stringify!(#method_ident)),
                |target, args| {
                    Box::pin(async move {
                        #destructure_args
                        #runtime_crate::IntoRouteResult::into_route_result(#method_call)
                    })
                },
            );
        });
        reflected_methods.push(ReflectedMethod {
            ident: method_ident,
            id: Some(route_id),
            is_async: method.sig.asyncness.is_some(),
        });
    }

    validate_reflected_methods(&reflected_methods, &mut errors);
    let error_tokens = errors.iter().map(Error::to_compile_error);

    quote! {
        #(#error_tokens)*
        #impl_block
        #(#registrations)*
    }
    .into()
}

#[proc_macro]
pub fn reflect_methods_group(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as ReflectGroupInput);
    expand_reflect_group(input).into()
}

/// Preferred name for [`reflect_methods_group`].
#[proc_macro]
pub fn route_methods_group(input: TokenStream) -> TokenStream {
    reflect_methods_group(input)
}

#[proc_macro]
pub fn define_static_dispatch(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as StaticDispatchInput);
    expand_static_dispatch(input).into()
}

/// Preferred name for [`define_static_dispatch`].
#[proc_macro]
pub fn define_static_router(input: TokenStream) -> TokenStream {
    define_static_dispatch(input)
}

struct StaticDispatchInput {
    target: Type,
    id_ty: Type,
    scan: LitStr,
    name_fn: Ident,
    id_fn: Ident,
}

impl syn::parse::Parse for StaticDispatchInput {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let mut target = None;
        let mut id_ty = syn::parse_quote! { u32 };
        let mut scan = None;
        let mut name_fn = syn::parse_quote! { call_by_name };
        let mut id_fn = syn::parse_quote! { call_by_id };

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;

            if key == "target" {
                target = Some(input.parse::<Type>()?);
            } else if key == "id_type" {
                id_ty = input.parse::<Type>()?;
            } else if key == "scan" {
                scan = Some(input.parse::<LitStr>()?);
            } else if key == "name_fn" {
                name_fn = input.parse::<Ident>()?;
            } else if key == "id_fn" {
                id_fn = input.parse::<Ident>()?;
            } else {
                return Err(Error::new_spanned(key, "unsupported define_static_dispatch option"));
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(Self {
            target: target.ok_or_else(|| Error::new(proc_macro2::Span::call_site(), "missing target = ..."))?,
            id_ty,
            scan: scan.ok_or_else(|| Error::new(proc_macro2::Span::call_site(), "missing scan = \"...\""))?,
            name_fn,
            id_fn,
        })
    }
}

fn expand_static_dispatch(input: StaticDispatchInput) -> proc_macro2::TokenStream {
    let mut errors = Vec::new();
    validate_numeric_id_type(&input.id_ty, &mut errors);
    let target_tokens = input.target.to_token_stream().to_string();
    let mut reflected_methods = Vec::new();
    let mut dispatch_params = None;

    for path in scan_paths(&input.scan, &mut errors) {
        let Ok(source) = std::fs::read_to_string(&path) else {
            errors.push(Error::new(proc_macro2::Span::call_site(), format!("failed to read {}", path.display())));
            continue;
        };

        let Ok(file) = syn::parse_file(&source) else {
            errors.push(Error::new(proc_macro2::Span::call_site(), format!("failed to parse {}", path.display())));
            continue;
        };

        collect_static_items(&file.items, &target_tokens, &mut dispatch_params, &mut reflected_methods, &mut errors);
    }

    validate_reflected_methods(&reflected_methods, &mut errors);

    let options = ReflectOptions {
        id_ty: input.id_ty,
        name_fn: input.name_fn,
        id_fn: input.id_fn,
        collect_only: false,
    };

    let dispatch_impl = render_dispatch_impl(&options, &errors, &syn::Generics::default(), &input.target, dispatch_params.unwrap_or_default(), &reflected_methods);

    quote! {
        mod __asktao_static_dispatch {
            use super::*;

            #dispatch_impl
        }
    }
}

fn scan_paths(scan: &LitStr, errors: &mut Vec<Error>) -> Vec<PathBuf> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let pattern = PathBuf::from(manifest_dir).join(scan.value());
    let pattern = pattern.to_string_lossy().replace('\\', "/");

    match glob::glob(&pattern) {
        Ok(paths) => {
            let mut paths: Vec<_> = paths.filter_map(Result::ok).collect();
            paths.sort();
            paths
        }
        Err(error) => {
            errors.push(Error::new(scan.span(), format!("invalid scan glob pattern: {error}")));
            Vec::new()
        }
    }
}

fn collect_static_items(items: &[Item], target_tokens: &str, dispatch_params: &mut Option<Vec<MethodParam>>, reflected_methods: &mut Vec<ReflectedMethod>, errors: &mut Vec<Error>) {
    for item in items {
        match item {
            Item::Impl(impl_block) if impl_block.self_ty.to_token_stream().to_string() == target_tokens => {
                let mut impl_block = impl_block.clone();
                collect_callable_methods(&mut impl_block, dispatch_params, reflected_methods, errors, true);
            }
            Item::Mod(module) => {
                if let Some((_, items)) = &module.content {
                    collect_static_items(items, target_tokens, dispatch_params, reflected_methods, errors);
                }
            }
            _ => {}
        }
    }
}

struct ReflectGroupInput {
    options: ReflectOptions,
    attr_errors: Vec<Error>,
    impl_blocks: Vec<ItemImpl>,
}

impl syn::parse::Parse for ReflectGroupInput {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let mut options = ReflectOptions::default();
        let mut attr_errors = Vec::new();

        if input.peek(syn::token::Paren) {
            let content;
            syn::parenthesized!(content in input);
            let tokens: TokenStream = content.parse::<proc_macro2::TokenStream>()?.into();
            let (parsed_options, errors) = parse_reflect_attr(tokens);
            options = parsed_options;
            attr_errors = errors;
        }

        let mut impl_blocks = Vec::new();
        while !input.is_empty() {
            impl_blocks.push(input.parse::<ItemImpl>()?);
        }

        Ok(Self { options, attr_errors, impl_blocks })
    }
}

fn expand_reflect_group(input: ReflectGroupInput) -> proc_macro2::TokenStream {
    let ReflectGroupInput {
        options,
        attr_errors,
        mut impl_blocks,
    } = input;

    let mut errors = attr_errors;
    let Some(first_impl) = impl_blocks.first() else {
        return Error::new(proc_macro2::Span::call_site(), "reflect_methods_group requires at least one impl block").to_compile_error();
    };

    let self_ty = first_impl.self_ty.clone();
    let generics = first_impl.generics.clone();
    let first_self_ty_tokens = self_ty.to_token_stream().to_string();
    let first_generics_tokens = generics.to_token_stream().to_string();

    let mut reflected_methods: Vec<ReflectedMethod> = Vec::new();
    let mut dispatch_params: Option<Vec<MethodParam>> = None;

    for impl_block in &mut impl_blocks {
        if impl_block.trait_.is_some() {
            errors.push(Error::new_spanned(impl_block.impl_token, "reflect_methods_group only supports inherent impl blocks"));
            continue;
        }

        if impl_block.self_ty.to_token_stream().to_string() != first_self_ty_tokens {
            errors.push(Error::new_spanned(impl_block.self_ty.to_token_stream(), "all grouped impl blocks must target the same type"));
            continue;
        }

        if impl_block.generics.to_token_stream().to_string() != first_generics_tokens {
            errors.push(Error::new_spanned(impl_block.generics.to_token_stream(), "all grouped impl blocks must use the same generics"));
            continue;
        }

        collect_reflected_methods(impl_block, &mut dispatch_params, &mut reflected_methods, &mut errors);
    }

    validate_reflected_methods(&reflected_methods, &mut errors);

    let dispatch_impl = render_dispatch_impl(&options, &errors, &generics, &self_ty, dispatch_params.unwrap_or_default(), &reflected_methods);

    quote! {
        #(#impl_blocks)*
        #dispatch_impl
    }
}

struct ReflectOptions {
    id_ty: Type,
    name_fn: Ident,
    id_fn: Ident,
    collect_only: bool,
}

impl Default for ReflectOptions {
    fn default() -> Self {
        Self {
            id_ty: syn::parse_quote! { u32 },
            name_fn: syn::parse_quote! { call_by_name },
            id_fn: syn::parse_quote! { call_by_id },
            collect_only: false,
        }
    }
}

fn parse_reflect_attr(attr: TokenStream) -> (ReflectOptions, Vec<Error>) {
    let mut options = ReflectOptions::default();
    let mut errors = Vec::new();

    if attr.is_empty() {
        return (options, errors);
    }

    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("id_type") {
            options.id_ty = meta.value()?.parse::<Type>()?;
            return Ok(());
        }

        if meta.path.is_ident("name_fn") {
            options.name_fn = meta.value()?.parse::<Ident>()?;
            return Ok(());
        }

        if meta.path.is_ident("id_fn") {
            options.id_fn = meta.value()?.parse::<Ident>()?;
            return Ok(());
        }

        if meta.path.is_ident("collect_only") {
            options.collect_only = true;
            return Ok(());
        }

        Err(meta.error("unsupported reflect_methods option"))
    });

    if let Err(error) = parser.parse(attr) {
        errors.push(error);
    }

    validate_numeric_id_type(&options.id_ty, &mut errors);

    (options, errors)
}

fn validate_numeric_id_type(id_ty: &Type, errors: &mut Vec<Error>) {
    let Type::Path(type_path) = id_ty else {
        errors.push(Error::new_spanned(id_ty, "id_type must be an integer type"));
        return;
    };

    let Some(segment) = type_path.path.segments.last() else {
        errors.push(Error::new_spanned(id_ty, "id_type must be an integer type"));
        return;
    };

    let ident = segment.ident.to_string();
    let is_integer = matches!(ident.as_str(), "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "i8" | "i16" | "i32" | "i64" | "i128" | "isize");

    if !is_integer {
        errors.push(Error::new_spanned(id_ty, "id_type must be an integer type"));
    }
}

fn strip_reflect_attrs(impl_block: &mut ItemImpl, make_callable_pub_crate: bool) {
    for item in &mut impl_block.items {
        let ImplItem::Fn(method) = item else {
            continue;
        };

        let is_callable = method.attrs.iter().any(is_route_attr);

        method.attrs.retain(|attr| !is_route_attr(attr) && !attr.path().is_ident("reflect_ignore"));

        if make_callable_pub_crate && is_callable && matches!(method.vis, Visibility::Inherited) {
            method.vis = syn::parse_quote! { pub(crate) };
        }
    }
}

fn collect_callable_methods(impl_block: &mut ItemImpl, dispatch_params: &mut Option<Vec<MethodParam>>, reflected_methods: &mut Vec<ReflectedMethod>, errors: &mut Vec<Error>, require_callable: bool) {
    for item in &mut impl_block.items {
        let ImplItem::Fn(method) = item else {
            continue;
        };

        let is_callable = method.attrs.iter().any(is_route_attr);
        let mut callable_id = None;

        for attr in method.attrs.iter().filter(|attr| is_route_attr(attr)) {
            if let Err(error) = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("id") {
                    if callable_id.is_some() {
                        return Err(meta.error("duplicate callable id"));
                    }

                    callable_id = Some(meta.value()?.parse::<Expr>()?);
                    return Ok(());
                }

                Err(meta.error("unsupported callable option"))
            }) {
                errors.push(error);
            }
        }

        if require_callable && !is_callable {
            continue;
        }

        if is_callable && callable_id.is_none() {
            errors.push(Error::new_spanned(method.sig.to_token_stream(), "routed methods require an id: #[route(id = ...)]"));
            continue;
        }

        if !validate_method_qualifiers(&method.sig, errors) {
            continue;
        }

        let is_ignored = method.attrs.iter().any(|attr| attr.path().is_ident("reflect_ignore"));

        method.attrs.retain(|attr| !is_route_attr(attr) && !attr.path().is_ident("reflect_ignore"));

        if is_ignored {
            continue;
        }

        let has_receiver = method.sig.inputs.iter().any(|arg| matches!(arg, FnArg::Receiver(_)));

        if !has_receiver {
            errors.push(Error::new_spanned(method.sig.to_token_stream(), "reflected methods must take &self or &mut self"));
            continue;
        }

        let mut params = Vec::new();

        for arg in method.sig.inputs.iter() {
            match arg {
                FnArg::Receiver(receiver) => {
                    if receiver.reference.is_none() {
                        errors.push(Error::new_spanned(receiver.to_token_stream(), "reflected methods cannot take self by value"));
                    }
                }
                FnArg::Typed(PatType { pat, ty, .. }) => {
                    let Pat::Ident(pat_ident) = pat.as_ref() else {
                        errors.push(Error::new_spanned(pat.to_token_stream(), "reflected method parameters must use simple names"));
                        continue;
                    };

                    params.push(MethodParam {
                        pat: pat.as_ref().clone(),
                        ident: pat_ident.ident.clone(),
                        ty: ty.as_ref().clone(),
                    });
                }
            }
        }

        if let Some(expected_params) = dispatch_params {
            if !same_param_types(expected_params, &params) {
                errors.push(Error::new_spanned(
                    method.sig.to_token_stream(),
                    "all reflected methods in the same impl must use the same parameter types",
                ));
                continue;
            }
        } else {
            *dispatch_params = Some(params.clone());
        }

        reflected_methods.push(ReflectedMethod {
            ident: method.sig.ident.clone(),
            id: callable_id,
            is_async: method.sig.asyncness.is_some(),
        });
    }
}

fn collect_reflected_methods(impl_block: &mut ItemImpl, dispatch_params: &mut Option<Vec<MethodParam>>, reflected_methods: &mut Vec<ReflectedMethod>, errors: &mut Vec<Error>) {
    for item in &mut impl_block.items {
        let ImplItem::Fn(method) = item else {
            continue;
        };

        let is_callable = method.attrs.iter().any(is_route_attr);
        let mut callable_id = None;

        for attr in method.attrs.iter().filter(|attr| is_route_attr(attr)) {
            if let Err(error) = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("id") {
                    if callable_id.is_some() {
                        return Err(meta.error("duplicate callable id"));
                    }

                    callable_id = Some(meta.value()?.parse::<Expr>()?);
                    return Ok(());
                }

                Err(meta.error("unsupported callable option"))
            }) {
                errors.push(error);
            }
        }

        let is_ignored = method.attrs.iter().any(|attr| attr.path().is_ident("reflect_ignore"));

        method.attrs.retain(|attr| !is_route_attr(attr) && !attr.path().is_ident("reflect_ignore"));

        if is_ignored {
            continue;
        }

        if !is_callable {
            continue;
        }

        if callable_id.is_none() {
            errors.push(Error::new_spanned(method.sig.to_token_stream(), "routed methods require an id: #[route(id = ...)]"));
            continue;
        }

        if !validate_method_qualifiers(&method.sig, errors) {
            continue;
        }

        let has_receiver = method.sig.inputs.iter().any(|arg| matches!(arg, FnArg::Receiver(_)));

        if !has_receiver {
            errors.push(Error::new_spanned(method.sig.to_token_stream(), "reflected methods must take &self or &mut self"));
            continue;
        }

        let mut params = Vec::new();

        for arg in method.sig.inputs.iter() {
            match arg {
                FnArg::Receiver(receiver) => {
                    if receiver.reference.is_none() {
                        errors.push(Error::new_spanned(receiver.to_token_stream(), "reflected methods cannot take self by value"));
                    }
                }
                FnArg::Typed(PatType { pat, ty, .. }) => {
                    let Pat::Ident(pat_ident) = pat.as_ref() else {
                        errors.push(Error::new_spanned(pat.to_token_stream(), "reflected method parameters must use simple names"));
                        continue;
                    };

                    params.push(MethodParam {
                        pat: pat.as_ref().clone(),
                        ident: pat_ident.ident.clone(),
                        ty: ty.as_ref().clone(),
                    });
                }
            }
        }

        if let Some(expected_params) = dispatch_params {
            if !same_param_types(expected_params, &params) {
                errors.push(Error::new_spanned(
                    method.sig.to_token_stream(),
                    "all reflected methods in the same impl must use the same parameter types",
                ));
                continue;
            }
        } else {
            *dispatch_params = Some(params.clone());
        }

        reflected_methods.push(ReflectedMethod {
            ident: method.sig.ident.clone(),
            id: callable_id,
            is_async: method.sig.asyncness.is_some(),
        });
    }
}

fn render_dispatch_impl(
    options: &ReflectOptions,
    errors: &[Error],
    generics: &syn::Generics,
    self_ty: &Type,
    dispatch_params: Vec<MethodParam>,
    reflected_methods: &[ReflectedMethod],
) -> proc_macro2::TokenStream {
    let runtime_crate = runtime_crate_path();
    let id_ty = &options.id_ty;
    let name_fn = &options.name_fn;
    let id_fn = &options.id_fn;
    let error_tokens: Vec<_> = errors.iter().map(Error::to_compile_error).collect();
    let (impl_generics, _ty_generics, where_clause) = generics.split_for_impl();
    let dispatch_param_defs = dispatch_params.iter().map(|param| {
        let pat = &param.pat;
        let ty = &param.ty;
        quote! { #pat: #ty }
    });
    let has_async_methods = reflected_methods.iter().any(|method| method.is_async);
    let maybe_async = has_async_methods.then(|| quote! { async });
    let name_match_arms = reflected_methods.iter().map(|method| {
        let method_ident = &method.ident;
        let dispatch_arg_idents = dispatch_params.iter().map(|param| &param.ident);
        let method_call = if method.is_async {
            quote! { self.#method_ident(#(#dispatch_arg_idents),*).await }
        } else {
            quote! { self.#method_ident(#(#dispatch_arg_idents),*) }
        };

        quote! {
            stringify!(#method_ident) => #runtime_crate::IntoCallResult::into_call_result(#method_call)
        }
    });
    let id_match_arms = reflected_methods.iter().filter_map(|method| {
        let id = method.id.as_ref()?;
        if !is_valid_route_id(id) {
            return None;
        }
        let method_ident = &method.ident;
        let dispatch_arg_idents = dispatch_params.iter().map(|param| &param.ident);
        let method_call = if method.is_async {
            quote! { self.#method_ident(#(#dispatch_arg_idents),*).await }
        } else {
            quote! { self.#method_ident(#(#dispatch_arg_idents),*) }
        };

        Some(quote! {
            #id => #runtime_crate::IntoCallResult::into_call_result(#method_call)
        })
    });
    let has_id_methods = reflected_methods.iter().any(|method| method.id.is_some());
    let call_by_id = has_id_methods.then(|| {
        let dispatch_param_defs = dispatch_params.iter().map(|param| {
            let pat = &param.pat;
            let ty = &param.ty;
            quote! { #pat: #ty }
        });

        quote! {
            #[deny(unreachable_patterns)]
            pub #maybe_async fn #id_fn(&mut self, id: #id_ty, #(#dispatch_param_defs),*) -> #runtime_crate::CallResult {
                #(#error_tokens)*

                match id {
                    #(#id_match_arms,)*
                    _ => Err(#runtime_crate::CallError::IdNotFound(format!("{id:?}"))),
                }
            }
        }
    });

    quote! {
        impl #impl_generics #self_ty #where_clause {
            pub #maybe_async fn #name_fn(&mut self, name: &str, #(#dispatch_param_defs),*) -> #runtime_crate::CallResult {
                #(#error_tokens)*

                match name {
                    #(#name_match_arms,)*
                    _ => Err(#runtime_crate::CallError::NameNotFound(name.to_string())),
                }
            }

            #call_by_id
        }
    }
}

fn same_param_types(left: &[MethodParam], right: &[MethodParam]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.ty.to_token_stream().to_string() == right.ty.to_token_stream().to_string())
}

fn is_route_attr(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("callable") || attr.path().is_ident("route")
}

fn validate_method_qualifiers(signature: &Signature, errors: &mut Vec<Error>) -> bool {
    let mut valid = true;

    if !signature.generics.params.is_empty() || signature.generics.where_clause.is_some() {
        errors.push(Error::new_spanned(&signature.generics, "routed methods cannot be generic"));
        valid = false;
    }

    if signature.constness.is_some() || signature.unsafety.is_some() || signature.abi.is_some() || signature.variadic.is_some() {
        errors.push(Error::new_spanned(signature, "routed methods must be ordinary Rust methods"));
        valid = false;
    }

    valid
}

fn runtime_crate_path() -> proc_macro2::TokenStream {
    match crate_name("opcode-router") {
        Ok(FoundCrate::Itself) => quote! { ::opcode_router },
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(&name, proc_macro2::Span::call_site());
            quote! { ::#ident }
        }
        Err(_) => quote! { ::opcode_router },
    }
}

fn validate_reflected_methods(methods: &[ReflectedMethod], errors: &mut Vec<Error>) {
    let mut ids: HashMap<String, &Ident> = HashMap::new();

    for method in methods {
        let Some(id) = &method.id else {
            continue;
        };

        if !is_valid_route_id(id) {
            errors.push(Error::new_spanned(id, "route id must be an integer literal or a constant path"));
            continue;
        }

        let key = id.to_token_stream().to_string();
        if let Some(first_method) = ids.insert(key.clone(), &method.ident) {
            errors.push(Error::new_spanned(
                id,
                format!("duplicate route id `{key}`; first registered by `{first_method}`, duplicated by `{}`", method.ident),
            ));
        }
    }
}

fn is_valid_route_id(id: &Expr) -> bool {
    match id {
        Expr::Lit(expr) => matches!(expr.lit, Lit::Int(_)),
        Expr::Path(expr) => expr.qself.is_none(),
        _ => false,
    }
}
