use proc_macro::TokenStream;
use proc_macro2::Span;
use proc_macro2::{Ident, TokenStream as TokenStream2};
use quote::{format_ident, quote, ToTokens};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{
    parse_quote, Attribute, Error, Field, Fields, ItemEnum, ItemStruct, LitStr, Path, Token,
};

mod attr;
mod util;

use attr::*;
use util::*;

#[proc_macro_attribute]
pub fn flat_path(attr: TokenStream, input: TokenStream) -> TokenStream {
    match flat_path_impl(attr, input) {
        Ok(v) => v.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn flat_path_impl(attr: TokenStream, input: TokenStream) -> syn::Result<TokenStream2> {
    if !attr.is_empty() {
        panic!("This macro does not accept any inputs when in this position")
    }

    if let Ok(input) = syn::parse::<ItemStruct>(input.clone()) {
        flat_path_struct_impl(input)
    } else if let Ok(input) = syn::parse::<ItemEnum>(input) {
        flat_path_enum_impl(input)
    } else {
        Err(Error::new(
            Span::call_site(),
            "Can not be applied to this type of member",
        ))
    }
}

fn flat_path_struct_impl(mut item: ItemStruct) -> syn::Result<TokenStream2> {
    let module_name = format_ident!("__serde_flat_path_{}", item.ident);

    let named_fields = match named_fields(&mut item.fields)? {
        Some(v) => v,
        None => return Ok(quote!(#item)),
    };

    let module_path = parse_quote!(#module_name);
    let flat_path_conversions = perform_simple_flat_path_addition(&module_path, named_fields)?;

    Ok(quote! {
        #item

        #[doc(hidden)]
        #[allow(non_snake_case)]
        #[automatically_derived]
        mod #module_path {
            #flat_path_conversions
        }
    })
}

fn flat_path_enum_impl(mut item: ItemEnum) -> syn::Result<TokenStream2> {
    let module_name = format_ident!("__serde_flat_path_{}", item.ident);

    let mut variant_impls = Vec::new();
    for variant in item.variants.iter_mut() {
        let named_fields = match named_fields(&mut variant.fields)? {
            Some(v) => v,
            None => return Ok(quote!(#item)),
        };

        let variant_name = &variant.ident;
        let module_path = parse_quote!(#module_name::#variant_name);
        let flat_path_conversions = perform_simple_flat_path_addition(&module_path, named_fields)?;

        variant_impls.push(quote! {
            pub mod #variant_name {
                #flat_path_conversions
            }
        });
    }

    Ok(quote! {
        #item

        #[doc(hidden)]
        #[allow(non_snake_case)]
        #[automatically_derived]
        mod #module_name {
            #(#variant_impls)*
        }
    })
}

fn perform_simple_flat_path_addition(
    module_name: &Path,
    named_fields: &mut Punctuated<Field, Token![,]>,
) -> syn::Result<TokenStream2> {
    let mut paths = Vec::new();

    for field in named_fields.iter_mut() {
        let flat_path_attributes = extract_attributes_by_path(&mut field.attrs, "flat_path");
        if flat_path_attributes.is_empty() {
            continue;
        }

        if flat_path_attributes.len() > 1 {
            return Err(Error::new(
                flat_path_attributes[1].span(),
                "flat_path can only be applied once",
            ));
        }

        let mut flat_path = parse_attr(&flat_path_attributes[0])?;

        let field_name = field
            .ident
            .clone()
            .ok_or_else(|| Error::new(field.span(), "Unable to apply flat_path to tuple fields"))?;

        let module_ref = LitStr::new(
            &format!(
                "{}::{}",
                module_name.clone().into_token_stream(),
                field_name
            ),
            field_name.span(),
        );

        let first_name = flat_path.remove(0);
        let serde_attributes = extract_attributes_by_path(&mut field.attrs, "serde");
        field
            .attrs
            .push(parse_quote!(#[serde(rename=#first_name, with=#module_ref)]));

        paths.push(FlatField {
            ident: field_name,
            flat_path,
            serde_attributes,
        });
    }

    Ok(generate_flat_path_module(paths))
}

fn named_fields(fields: &mut Fields) -> syn::Result<Option<&mut Punctuated<Field, Token![,]>>> {
    match fields {
        Fields::Unit => Ok(None),
        Fields::Named(named) => Ok(Some(&mut named.named)),
        Fields::Unnamed(unnamed) => {
            for field in &mut unnamed.unnamed {
                if !extract_attributes_by_path(&mut field.attrs, "flat_path").is_empty() {
                    return Err(Error::new(
                        field.span(),
                        "Unable to apply flat_path to unnamed tuple fields",
                    ));
                }
            }

            Ok(None)
        }
    }
}

fn generate_flat_path_module(flat_fields: Vec<FlatField>) -> TokenStream2 {
    let mut tokens = TokenStream2::new();

    for flat_field in flat_fields {
        let contents = flat_field.generate_serialize_with();
        let field = flat_field.ident;

        tokens.extend(quote! {
            pub mod #field {
                #contents
            }
        });
    }

    // let module_name = Ident::new(module_name, Span::call_site());

    tokens
    // quote! {
    //     #[doc(hidden)]
    //     #[allow(non_snake_case)]
    //     #[automatically_derived]
    //     mod #module_name {
    //         #tokens
    //     }
    // }
}

struct FlatField {
    ident: Ident,
    flat_path: Vec<LitStr>,
    serde_attributes: Vec<Attribute>,
}

impl FlatField {
    fn generate_serialize_with(&self) -> TokenStream2 {
        self.with_structural_derive()
    }

    fn with_structural_derive(&self) -> TokenStream2 {
        let mut tokens = TokenStream2::new();

        let path_length = self.flat_path.len();
        let placeholders = placeholder_idents(path_length);
        for (index, field_name) in self.flat_path[..path_length - 1].iter().enumerate() {
            let ident = &placeholders[index];
            let next = &placeholders[index + 1];

            tokens.extend(quote! {
                #[repr(transparent)]
                #[derive(::serde::Serialize, ::serde::Deserialize)]
                struct #ident<T: ?Sized> {
                    #[serde(rename=#field_name)]
                    _0: #next<T>
                }
            });
        }

        let last_ident = &placeholders[path_length - 1];
        let last_field_name = &self.flat_path[path_length - 1];
        let serde_attributes = &self.serde_attributes;

        let chain = std::iter::repeat(format_ident!("_0")).take(path_length);
        tokens.extend(quote! {
            #[repr(transparent)]
            #[derive(::serde::Serialize, ::serde::Deserialize)]
            struct #last_ident<T: ?Sized> {
                #[serde(rename=#last_field_name)]
                #(#serde_attributes)*
                _0: T
            }

            #[inline(always)]
            pub fn deserialize<'de, D, T>(deserializer: D) -> Result<T, D::Error>
                where T: ?Sized + ::serde::Deserialize<'de>,
                      D: ::serde::Deserializer<'de>,
            {
                match <_0<T> as ::serde::Deserialize>::deserialize(deserializer) {
                    Ok(value) => Ok(value #(.#chain)*),
                    Err(e) => Err(e)
                }
            }

            #[inline(always)]
            pub fn serialize<S, T>(this: &T, serializer: S) -> Result<S::Ok, S::Error>
                where T: ?Sized + ::serde::Serialize,
                      S: ::serde::Serializer
            {
                // # Safety
                // This is safe as all members within the chain use repr(transparent) to a value of
                // T. Furthermore, data is not accessed via this reference until it is converted
                // back to &T at the end of the chain.
                let chain_ref = unsafe { ::std::mem::transmute::<&T, &_0<T>>(this) };
                ::serde::Serialize::serialize(chain_ref, serializer)
            }
        });

        tokens
    }

    // /// This approach has a couple of advantages and disadvantages which prevent it from being used
    // /// everywhere.
    // ///
    // /// Pros:
    // ///  - No copying/cloning fields
    // ///  - No unsafe code
    // ///  - Field names do not need to be valid rust identifiers
    // ///  - Does not require information about the struct it was used in
    // /// Cons:
    // ///  - Unable to propogate serde attributes to field
    // ///  - Will not attempt to merge related paths on a single struct
    // ///
    // /// When supplied with `#[flat_path(path = ["a", "b", "c"])]`, it will generate the following
    // /// code.
    // /// ```rust,norun
    // /// struct _0<'a, T: ?Sized>(&'a T);
    // /// struct _1<'a, T: ?Sized>(&'a T);
    // /// struct _2<'a, T: ?Sized>(&'a T);
    // /// impl<'a, T: ?Sized + ::serde::Serialize> ::serde::Serialize for _0<'a, T> {
    // ///     #[inline]
    // ///     fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    // ///         where
    // ///             S: ::serde::Serializer,
    // ///     {
    // ///         let mut state = ::serde::Serializer::serialize_struct(serializer, "", 1)?;
    // ///         ::serde::ser::SerializeStruct::serialize_field(&mut state, "a", &_1(self.0))?;
    // ///         ::serde::ser::SerializeStruct::end(state)
    // ///     }
    // /// }
    // /// impl<'a, T: ?Sized + ::serde::Serialize> ::serde::Serialize for _1<'a, T> {
    // ///     #[inline]
    // ///     fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    // ///         where
    // ///             S: ::serde::Serializer,
    // ///     {
    // ///         let mut state = ::serde::Serializer::serialize_struct(serializer, "", 1)?;
    // ///         ::serde::ser::SerializeStruct::serialize_field(&mut state, "b", &_2(self.0))?;
    // ///         ::serde::ser::SerializeStruct::end(state)
    // ///     }
    // /// }
    // /// impl<'a, T: ?Sized + ::serde::Serialize> ::serde::Serialize for _2<'a, T> {
    // ///     #[inline]
    // ///     fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    // ///         where
    // ///             S: ::serde::Serializer,
    // ///     {
    // ///         let mut state = ::serde::Serializer::serialize_struct(serializer, "", 1)?;
    // ///         ::serde::ser::SerializeStruct::serialize_field(&mut state, "c", self.0)?;
    // ///         ::serde::ser::SerializeStruct::end(state)
    // ///     }
    // /// }
    // /// pub fn serialize<S, T>(this: &T, serializer: S) -> Result<S::Ok, S::Error>
    // ///     where
    // ///         T: ?Sized + ::serde::Serialize,
    // ///         S: ::serde::Serializer,
    // /// {
    // ///     ::serde::Serialize::serialize(&_0(this), serializer)
    // /// }
    // /// ```
    // fn serialize_with_ref_handoff(&self) -> TokenStream2 {
    //     let mut tokens = TokenStream2::new();
    //
    //     let mut placeholder_structs = Vec::new();
    //     for level in 0..self.flat_path.len() {
    //         let ident = format_ident!("_{}", level);
    //
    //         tokens.extend(quote! {
    //             struct #ident<'a, T: ?Sized>(&'a T);
    //         });
    //         placeholder_structs.push(ident);
    //     }
    //
    //     for n in 0..placeholder_structs.len() {
    //         let current = &placeholder_structs[n];
    //         let item_name = &self.flat_path[n];
    //
    //         let next = if n < placeholder_structs.len() - 1 {
    //             let next = &placeholder_structs[n + 1];
    //             quote!(&#next(self.0))
    //         } else {
    //             quote!(self.0)
    //         };
    //
    //         tokens.extend(quote! {
    //             impl<'a, T: ?Sized + ::serde::Serialize> ::serde::Serialize for #current<'a, T> {
    //                 #[inline]
    //                 fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    //                     where S: ::serde::Serializer
    //                 {
    //                     let mut state = ::serde::Serializer::serialize_struct(serializer, "", 1)?;
    //                     ::serde::ser::SerializeStruct::serialize_field(&mut state, #item_name, #next)?;
    //                     ::serde::ser::SerializeStruct::end(state)
    //                 }
    //             }
    //         });
    //     }
    //
    //     tokens.extend(quote! {
    //         pub fn serialize<S, T>(this: &T, serializer: S) -> Result<S::Ok, S::Error>
    //             where T: ?Sized + ::serde::Serialize,
    //                   S: ::serde::Serializer
    //         {
    //             ::serde::Serialize::serialize(&_0(this), serializer)
    //         }
    //     });
    //
    //     tokens
    // }
}
