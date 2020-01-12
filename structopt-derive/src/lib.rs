// Copyright 2018 Guillaume Pinot (@TeXitoi) <texitoi@texitoi.eu>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! This crate is custom derive for `StructOpt`. It should not be used
//! directly. See [structopt documentation](https://docs.rs/structopt)
//! for the usage of `#[derive(StructOpt)]`.

#![allow(clippy::large_enum_variant)]

extern crate proc_macro;

mod attrs;
mod doc_comments;
mod parse;
mod spanned;
mod ty;

use crate::{
    attrs::{Attrs, CasingStyle, Kind, Name, ParserKind},
    spanned::Sp,
    ty::{is_simple_ty, sub_type, subty_if_name, Ty},
};

use proc_macro2::{Span, TokenStream};
use proc_macro_error::{abort, abort_call_site, proc_macro_error, set_dummy};
use quote::{quote, quote_spanned};
use syn::{punctuated::Punctuated, spanned::Spanned, token::Comma, *};

/// Default casing style for generated arguments.
const DEFAULT_CASING: CasingStyle = CasingStyle::Kebab;

/// Default casing style for environment variables
const DEFAULT_ENV_CASING: CasingStyle = CasingStyle::ScreamingSnake;

/// Output for the `gen_xxx()` methods were we need more than a simple stream of tokens.
///
/// The output of a generation method is not only the stream of new tokens but also the attribute
/// information of the current element. These attribute information may contain valuable information
/// for any kind of child arguments.
struct GenOutput {
    tokens: TokenStream,
    attrs: Attrs,
}

/// Generates the `StructOpt` impl.
#[proc_macro_derive(StructOpt, attributes(structopt))]
#[proc_macro_error]
pub fn structopt(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input: DeriveInput = syn::parse(input).unwrap();
    let gen = impl_structopt(&input);
    gen.into()
}

/// Generate a block of code to add arguments/subcommands corresponding to
/// the `fields` to an app.
fn gen_augmentation(
    fields: &Punctuated<Field, Comma>,
    app_var: &Ident,
    parent_attribute: &Attrs,
) -> TokenStream {
    let mut subcmds = fields.iter().filter_map(|field| {
        let attrs = Attrs::from_field(
            field,
            Some(parent_attribute),
            parent_attribute.casing(),
            parent_attribute.env_casing(),
        );
        let kind = attrs.kind();
        if let Kind::Subcommand(ty) = &*kind {
            let subcmd_type = match (**ty, sub_type(&field.ty)) {
                (Ty::Option, Some(sub_type)) => sub_type,
                _ => &field.ty,
            };
            let required = if **ty == Ty::Option {
                quote!()
            } else {
                quote_spanned! { kind.span()=>
                    let #app_var = #app_var.setting(
                        ::structopt::clap::AppSettings::SubcommandRequiredElseHelp
                    );
                }
            };

            let span = field.span();
            let ts = quote! {
                let #app_var = <#subcmd_type as ::structopt::StructOptInternal>::augment_clap(
                    #app_var
                );
                #required
            };
            Some((span, ts))
        } else {
            None
        }
    });

    let subcmd = subcmds.next().map(|(_, ts)| ts);
    if let Some((span, _)) = subcmds.next() {
        abort!(
            span,
            "multiple subcommand sets are not allowed, that's the second"
        );
    }

    let args = fields.iter().filter_map(|field| {
        let attrs = Attrs::from_field(
            field,
            Some(parent_attribute),
            parent_attribute.casing(),
            parent_attribute.env_casing(),
        );
        let kind = attrs.kind();
        match &*kind {
            Kind::ExternalSubcommand => abort!(
                kind.span(),
                "`external_subcommand` is only allowed on enum variants"
            ),
            Kind::Subcommand(_) | Kind::Skip(_) => None,
            Kind::FlattenStruct => {
                let ty = &field.ty;
                Some(quote_spanned! { kind.span()=>
                    let #app_var = <#ty as ::structopt::StructOptInternal>::augment_clap(#app_var);
                    let #app_var = if <#ty as ::structopt::StructOptInternal>::is_subcommand() {
                        #app_var.setting(::structopt::clap::AppSettings::SubcommandRequiredElseHelp)
                    } else {
                        #app_var
                    };
                })
            }
            Kind::Arg(ty) => {
                let convert_type = match **ty {
                    Ty::Vec | Ty::Option => sub_type(&field.ty).unwrap_or(&field.ty),
                    Ty::OptionOption | Ty::OptionVec => {
                        sub_type(&field.ty).and_then(sub_type).unwrap_or(&field.ty)
                    }
                    _ => &field.ty,
                };

                let occurrences = *attrs.parser().kind == ParserKind::FromOccurrences;
                let flag = *attrs.parser().kind == ParserKind::FromFlag;

                let parser = attrs.parser();
                let func = &parser.func;
                let validator = match *parser.kind {
                    ParserKind::TryFromStr => quote_spanned! { func.span()=>
                        .validator(|s| {
                            #func(s.as_str())
                            .map(|_: #convert_type| ())
                            .map_err(|e| e.to_string())
                        })
                    },
                    ParserKind::TryFromOsStr => quote_spanned! { func.span()=>
                        .validator_os(|s| #func(&s).map(|_: #convert_type| ()))
                    },
                    _ => quote!(),
                };

                let modifier = match **ty {
                    Ty::Bool => quote_spanned! { ty.span()=>
                        .takes_value(false)
                        .multiple(false)
                    },

                    Ty::Option => quote_spanned! { ty.span()=>
                        .takes_value(true)
                        .multiple(false)
                        #validator
                    },

                    Ty::OptionOption => quote_spanned! { ty.span()=>
                            .takes_value(true)
                            .multiple(false)
                            .min_values(0)
                            .max_values(1)
                            #validator
                    },

                    Ty::OptionVec => quote_spanned! { ty.span()=>
                        .takes_value(true)
                        .multiple(true)
                        .min_values(0)
                        #validator
                    },

                    Ty::Vec => quote_spanned! { ty.span()=>
                        .takes_value(true)
                        .multiple(true)
                        #validator
                    },

                    Ty::Other if occurrences => quote_spanned! { ty.span()=>
                        .takes_value(false)
                        .multiple(true)
                    },

                    Ty::Other if flag => quote_spanned! { ty.span()=>
                        .takes_value(false)
                        .multiple(false)
                    },

                    Ty::Other => {
                        let required = !attrs.has_method("default_value");
                        quote_spanned! { ty.span()=>
                            .takes_value(true)
                            .multiple(false)
                            .required(#required)
                            #validator
                        }
                    }
                };

                let name = attrs.cased_name();
                let methods = attrs.field_methods();

                Some(quote_spanned! { field.span()=>
                    let #app_var = #app_var.arg(
                        ::structopt::clap::Arg::with_name(#name)
                            #modifier
                            #methods
                    );
                })
            }
        }
    });

    let app_methods = parent_attribute.top_level_methods();
    quote! {{
        let #app_var = #app_var#app_methods;
        #( #args )*
        #subcmd
        #app_var
    }}
}

fn gen_constructor(fields: &Punctuated<Field, Comma>, parent_attribute: &Attrs) -> TokenStream {
    let fields = fields.iter().map(|field| {
        let attrs = Attrs::from_field(
            field,
            Some(parent_attribute),
            parent_attribute.casing(),
            parent_attribute.env_casing(),
        );
        let field_name = field.ident.as_ref().unwrap();
        let kind = attrs.kind();
        match &*kind {
            Kind::ExternalSubcommand => abort!(
                kind.span(),
                "`external_subcommand` is allowed only on enum variants"
            ),

            Kind::Subcommand(ty) => {
                let subcmd_type = match (**ty, sub_type(&field.ty)) {
                    (Ty::Option, Some(sub_type)) => sub_type,
                    _ => &field.ty,
                };
                let unwrapper = match **ty {
                    Ty::Option => quote!(),
                    _ => quote_spanned!( ty.span()=> .unwrap() ),
                };
                quote_spanned! { kind.span()=>
                    #field_name: <#subcmd_type as ::structopt::StructOptInternal>::from_subcommand(
                        matches.subcommand())
                        #unwrapper
                }
            }

            Kind::FlattenStruct => quote_spanned! { kind.span()=>
                #field_name: ::structopt::StructOpt::from_clap(matches)
            },

            Kind::Skip(val) => match val {
                None => quote_spanned!(kind.span()=> #field_name: Default::default()),
                Some(val) => quote_spanned!(kind.span()=> #field_name: (#val).into()),
            },

            Kind::Arg(ty) => {
                use crate::attrs::ParserKind::*;

                let parser = attrs.parser();
                let func = &parser.func;
                let span = parser.kind.span();
                let (value_of, values_of, parse) = match *parser.kind {
                    FromStr => (
                        quote_spanned!(span=> value_of),
                        quote_spanned!(span=> values_of),
                        func.clone(),
                    ),
                    TryFromStr => (
                        quote_spanned!(span=> value_of),
                        quote_spanned!(span=> values_of),
                        quote_spanned!(func.span()=> |s| #func(s).unwrap()),
                    ),
                    FromOsStr => (
                        quote_spanned!(span=> value_of_os),
                        quote_spanned!(span=> values_of_os),
                        func.clone(),
                    ),
                    TryFromOsStr => (
                        quote_spanned!(span=> value_of_os),
                        quote_spanned!(span=> values_of_os),
                        quote_spanned!(func.span()=> |s| #func(s).unwrap()),
                    ),
                    FromOccurrences => (
                        quote_spanned!(span=> occurrences_of),
                        quote!(),
                        func.clone(),
                    ),
                    FromFlag => (quote!(), quote!(), func.clone()),
                };

                let flag = *attrs.parser().kind == ParserKind::FromFlag;
                let occurrences = *attrs.parser().kind == ParserKind::FromOccurrences;
                let name = attrs.cased_name();
                let field_value = match **ty {
                    Ty::Bool => quote_spanned!(ty.span()=> matches.is_present(#name)),

                    Ty::Option => quote_spanned! { ty.span()=>
                        matches.#value_of(#name)
                            .map(#parse)
                    },

                    Ty::OptionOption => quote_spanned! { ty.span()=>
                        if matches.is_present(#name) {
                            Some(matches.#value_of(#name).map(#parse))
                        } else {
                            None
                        }
                    },

                    Ty::OptionVec => quote_spanned! { ty.span()=>
                        if matches.is_present(#name) {
                            Some(matches.#values_of(#name)
                                 .map_or_else(Vec::new, |v| v.map(#parse).collect()))
                        } else {
                            None
                        }
                    },

                    Ty::Vec => quote_spanned! { ty.span()=>
                        matches.#values_of(#name)
                            .map_or_else(Vec::new, |v| v.map(#parse).collect())
                    },

                    Ty::Other if occurrences => quote_spanned! { ty.span()=>
                        #parse(matches.#value_of(#name))
                    },

                    Ty::Other if flag => quote_spanned! { ty.span()=>
                        #parse(matches.is_present(#name))
                    },

                    Ty::Other => quote_spanned! { ty.span()=>
                        matches.#value_of(#name)
                            .map(#parse)
                            .unwrap()
                    },
                };

                quote_spanned!(field.span()=> #field_name: #field_value )
            }
        }
    });

    quote! {{
        #( #fields ),*
    }}
}

fn gen_from_clap(
    struct_name: &Ident,
    fields: &Punctuated<Field, Comma>,
    parent_attribute: &Attrs,
) -> TokenStream {
    let field_block = gen_constructor(fields, parent_attribute);

    quote! {
        fn from_clap(matches: &::structopt::clap::ArgMatches) -> Self {
            #struct_name #field_block
        }
    }
}

fn gen_clap(attrs: &[Attribute]) -> GenOutput {
    let name = std::env::var("CARGO_PKG_NAME").ok().unwrap_or_default();

    let attrs = Attrs::from_struct(
        Span::call_site(),
        attrs,
        Name::Assigned(LitStr::new(&name, Span::call_site())),
        None,
        Sp::call_site(DEFAULT_CASING),
        Sp::call_site(DEFAULT_ENV_CASING),
    );
    let tokens = {
        let name = attrs.cased_name();
        quote!(::structopt::clap::App::new(#name))
    };

    GenOutput { tokens, attrs }
}

fn gen_clap_struct(struct_attrs: &[Attribute]) -> GenOutput {
    let initial_clap_app_gen = gen_clap(struct_attrs);
    let clap_tokens = initial_clap_app_gen.tokens;

    let augmented_tokens = quote! {
        fn clap<'a, 'b>() -> ::structopt::clap::App<'a, 'b> {
            let app = #clap_tokens;
            <Self as ::structopt::StructOptInternal>::augment_clap(app)
        }
    };

    GenOutput {
        tokens: augmented_tokens,
        attrs: initial_clap_app_gen.attrs,
    }
}

fn gen_augment_clap(fields: &Punctuated<Field, Comma>, parent_attribute: &Attrs) -> TokenStream {
    let app_var = Ident::new("app", Span::call_site());
    let augmentation = gen_augmentation(fields, &app_var, parent_attribute);
    quote! {
        fn augment_clap<'a, 'b>(
            #app_var: ::structopt::clap::App<'a, 'b>
        ) -> ::structopt::clap::App<'a, 'b> {
            #augmentation
        }
    }
}

fn gen_clap_enum(enum_attrs: &[Attribute]) -> GenOutput {
    let initial_clap_app_gen = gen_clap(enum_attrs);
    let clap_tokens = initial_clap_app_gen.tokens;

    let tokens = quote! {
        fn clap<'a, 'b>() -> ::structopt::clap::App<'a, 'b> {
            let app = #clap_tokens
                .setting(::structopt::clap::AppSettings::SubcommandRequiredElseHelp);
            <Self as ::structopt::StructOptInternal>::augment_clap(app)
        }
    };

    GenOutput {
        tokens,
        attrs: initial_clap_app_gen.attrs,
    }
}

fn gen_augment_clap_enum(
    variants: &Punctuated<Variant, Comma>,
    parent_attribute: &Attrs,
) -> TokenStream {
    use syn::Fields::*;

    let subcommands = variants.iter().map(|variant| {
        let attrs = Attrs::from_struct(
            variant.span(),
            &variant.attrs,
            Name::Derived(variant.ident.clone()),
            Some(parent_attribute),
            parent_attribute.casing(),
            parent_attribute.env_casing(),
        );

        if let Kind::ExternalSubcommand = *attrs.kind() {
            return quote_spanned! { attrs.kind().span()=>
                .setting(::structopt::clap::AppSettings::AllowExternalSubcommands)
            };
        }

        let app_var = Ident::new("subcommand", Span::call_site());
        let arg_block = match variant.fields {
            Named(ref fields) => gen_augmentation(&fields.named, &app_var, &attrs),
            Unit => quote!( #app_var ),
            Unnamed(FieldsUnnamed { ref unnamed, .. }) if unnamed.len() == 1 => {
                let ty = &unnamed[0];
                quote_spanned! { ty.span()=>
                    {
                        let #app_var = <#ty as ::structopt::StructOptInternal>::augment_clap(
                            #app_var
                        );
                        if <#ty as ::structopt::StructOptInternal>::is_subcommand() {
                            #app_var.setting(
                                ::structopt::clap::AppSettings::SubcommandRequiredElseHelp
                            )
                        } else {
                            #app_var
                        }
                    }
                }
            }
            Unnamed(..) => abort_call_site!("{}: tuple enums are not supported", variant.ident),
        };

        let name = attrs.cased_name();
        let from_attrs = attrs.top_level_methods();

        quote! {
            .subcommand({
                let #app_var = ::structopt::clap::SubCommand::with_name(#name);
                let #app_var = #arg_block;
                #app_var#from_attrs
            })
        }
    });

    let app_methods = parent_attribute.top_level_methods();

    quote! {
        fn augment_clap<'a, 'b>(
            app: ::structopt::clap::App<'a, 'b>
        ) -> ::structopt::clap::App<'a, 'b> {
            app #app_methods #( #subcommands )*
        }
    }
}

fn gen_from_clap_enum(name: &Ident) -> TokenStream {
    quote! {
        fn from_clap(matches: &::structopt::clap::ArgMatches) -> Self {
            <#name as ::structopt::StructOptInternal>::from_subcommand(matches.subcommand())
                .unwrap()
        }
    }
}

fn gen_from_subcommand(
    name: &Ident,
    variants: &Punctuated<Variant, Comma>,
    parent_attribute: &Attrs,
) -> TokenStream {
    use syn::Fields::*;

    let mut ext_subcmd = None;

    let match_arms: Vec<_> = variants
        .iter()
        .filter_map(|variant| {
            let attrs = Attrs::from_struct(
                variant.span(),
                &variant.attrs,
                Name::Derived(variant.ident.clone()),
                Some(parent_attribute),
                parent_attribute.casing(),
                parent_attribute.env_casing(),
            );

            let sub_name = attrs.cased_name();
            let variant_name = &variant.ident;

            if let Kind::ExternalSubcommand = *attrs.kind() {
                if ext_subcmd.is_some() {
                    abort!(
                        attrs.kind().span(),
                        "Only one variant can be marked with `external_subcommand`, \
                         this is the second"
                    );
                }

                let ty = match variant.fields {
                    Unnamed(ref fields) if fields.unnamed.len() == 1 => &fields.unnamed[0].ty,

                    _ => abort!(
                        variant.span(),
                        "The enum variant marked with `external_attribute` must be \
                         a single-typed tuple, and the type must be either `Vec<String>` \
                         or `Vec<OsString>`."
                    ),
                };

                let (span, str_ty, values_of) = match subty_if_name(ty, "Vec") {
                    Some(subty) => {
                        if is_simple_ty(subty, "String") {
                            (
                                subty.span(),
                                quote!(::std::string::String),
                                quote!(values_of),
                            )
                        } else {
                            (
                                subty.span(),
                                quote!(::std::ffi::OsString),
                                quote!(values_of_os),
                            )
                        }
                    }

                    None => abort!(
                        ty.span(),
                        "The type must be either `Vec<String>` or `Vec<OsString>` \
                         to be used with `external_subcommand`."
                    ),
                };

                ext_subcmd = Some((span, variant_name, str_ty, values_of));
                None
            } else {
                let constructor_block = match variant.fields {
                    Named(ref fields) => gen_constructor(&fields.named, &attrs),
                    Unit => quote!(),
                    Unnamed(ref fields) if fields.unnamed.len() == 1 => {
                        let ty = &fields.unnamed[0];
                        quote!( ( <#ty as ::structopt::StructOpt>::from_clap(matches) ) )
                    }
                    Unnamed(..) => {
                        abort_call_site!("{}: tuple enums are not supported", variant.ident)
                    }
                };

                Some(quote! {
                    (#sub_name, Some(matches)) =>
                        Some(#name :: #variant_name #constructor_block)
                })
            }
        })
        .collect();

    let wildcard = match ext_subcmd {
        Some((span, var_name, str_ty, values_of)) => quote_spanned! { span=>
            ("", ::std::option::Option::None) => None,

            (external, Some(matches)) => {
                ::std::option::Option::Some(#name::#var_name(
                    ::std::iter::once(#str_ty::from(external))
                    .chain(
                        matches.#values_of("").unwrap().map(#str_ty::from)
                    )
                    .collect::<::std::vec::Vec<_>>()
                ))
            }

            (external, None) => {
                ::std::option::Option::Some(#name::#var_name({
                    let mut v = ::std::vec::Vec::with_capacity(1);
                    v.push(#str_ty::from(external));
                    v
                }))
            }
        },

        None => quote!(_ => None),
    };

    quote! {
        fn from_subcommand<'a, 'b>(
            sub: (&'b str, Option<&'b ::structopt::clap::ArgMatches<'a>>)
        ) -> Option<Self> {
            match sub {
                #( #match_arms, )*
                #wildcard
            }
        }
    }
}

#[cfg(feature = "paw")]
fn gen_paw_impl(name: &Ident) -> TokenStream {
    quote! {
        impl paw::ParseArgs for #name {
            type Error = std::io::Error;

            fn parse_args() -> std::result::Result<Self, Self::Error> {
                Ok(<#name as ::structopt::StructOpt>::from_args())
            }
        }
    }
}
#[cfg(not(feature = "paw"))]
fn gen_paw_impl(_: &Ident) -> TokenStream {
    TokenStream::new()
}

fn impl_structopt_for_struct(
    name: &Ident,
    fields: &Punctuated<Field, Comma>,
    attrs: &[Attribute],
) -> TokenStream {
    let basic_clap_app_gen = gen_clap_struct(attrs);
    let augment_clap = gen_augment_clap(fields, &basic_clap_app_gen.attrs);
    let from_clap = gen_from_clap(name, fields, &basic_clap_app_gen.attrs);
    let paw_impl = gen_paw_impl(name);

    let clap_tokens = basic_clap_app_gen.tokens;
    quote! {
        #[allow(unused_variables)]
        #[allow(unknown_lints)]
        #[allow(clippy::all)]
        #[allow(dead_code, unreachable_code)]
        impl ::structopt::StructOpt for #name {
            #clap_tokens
            #from_clap
        }

        #[allow(unused_variables)]
        #[allow(unknown_lints)]
        #[allow(clippy::all)]
        #[allow(dead_code, unreachable_code)]
        impl ::structopt::StructOptInternal for #name {
            #augment_clap
            fn is_subcommand() -> bool { false }
        }

        #paw_impl
    }
}

fn impl_structopt_for_enum(
    name: &Ident,
    variants: &Punctuated<Variant, Comma>,
    attrs: &[Attribute],
) -> TokenStream {
    let basic_clap_app_gen = gen_clap_enum(attrs);
    let clap_tokens = basic_clap_app_gen.tokens;
    let attrs = basic_clap_app_gen.attrs;

    let augment_clap = gen_augment_clap_enum(variants, &attrs);
    let from_clap = gen_from_clap_enum(name);
    let from_subcommand = gen_from_subcommand(name, variants, &attrs);
    let paw_impl = gen_paw_impl(name);

    quote! {
        #[allow(unknown_lints)]
        #[allow(unused_variables, dead_code, unreachable_code)]
        #[allow(clippy)]
        impl ::structopt::StructOpt for #name {
            #clap_tokens
            #from_clap
        }

        #[allow(unused_variables)]
        #[allow(unknown_lints)]
        #[allow(clippy::all)]
        #[allow(dead_code, unreachable_code)]
        impl ::structopt::StructOptInternal for #name {
            #augment_clap
            #from_subcommand
            fn is_subcommand() -> bool { true }
        }

        #paw_impl
    }
}

fn impl_structopt(input: &DeriveInput) -> TokenStream {
    use syn::Data::*;

    let struct_name = &input.ident;

    set_dummy(quote! {
        impl ::structopt::StructOpt for #struct_name {
            fn clap<'a, 'b>() -> ::structopt::clap::App<'a, 'b> {
                unimplemented!()
            }
            fn from_clap(_matches: &::structopt::clap::ArgMatches) -> Self {
                unimplemented!()
            }
        }

        impl ::structopt::StructOptInternal for #struct_name {}
    });

    match input.data {
        Struct(DataStruct {
            fields: syn::Fields::Named(ref fields),
            ..
        }) => impl_structopt_for_struct(struct_name, &fields.named, &input.attrs),
        Enum(ref e) => impl_structopt_for_enum(struct_name, &e.variants, &input.attrs),
        _ => abort_call_site!("structopt only supports non-tuple structs and enums"),
    }
}
