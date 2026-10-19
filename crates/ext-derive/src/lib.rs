//! `#[derive(Schema)]` for a struct with named fields: the JSON Schema of
//! the object serde reads it from, written by `ext::Schema::schema`.
//!
//! Each field is a property, described by its doc comment. One is required
//! unless serde gives it a default (`#[serde(default)]`) or its type may be
//! left out (an `Option`). `#[serde(rename = "..")]` names it, and
//! `#[schema(min = 1, max = 600, min_items = 1)]` bounds it. No other
//! property is allowed.

use std::fmt::Write as _;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as Tokens;
use quote::quote;
use syn::meta::ParseNestedMeta;
use syn::{Data, DeriveInput, Expr, Fields, Lit, LitInt, LitStr, Meta, parse_macro_input};

#[proc_macro_derive(Schema, attributes(schema))]
pub fn schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    derive(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// One property of the object.
struct Field {
    name: String,
    ty: syn::Type,
    doc: String,
    defaulted: bool,
    bounds: Vec<(&'static str, LitInt)>,
}

fn derive(input: &DeriveInput) -> syn::Result<Tokens> {
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(input, "a schema is of a struct"));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(syn::Error::new_spanned(
            input,
            "a schema is of named fields",
        ));
    };
    let fields = named
        .named
        .iter()
        .map(field)
        .collect::<syn::Result<Vec<_>>>()?;
    let properties = fields.iter().enumerate().map(|(i, f)| {
        let key = format!("{}{}:", if i == 0 { "" } else { "," }, json_string(&f.name));
        let (ty, doc) = (&f.ty, &f.doc);
        let bounds = f.bounds.iter().map(|(k, v)| quote!((#k, #v)));
        quote! {
            out.push_str(#key);
            ::ext::schema::property::<#ty>(out, #doc, &[#(#bounds),*]);
        }
    });
    let required = fields.iter().filter(|f| !f.defaulted).map(|f| {
        let (ty, name) = (&f.ty, json_string(&f.name));
        quote! {
            if !<#ty as ::ext::Schema>::OPTIONAL {
                if !first {
                    out.push(',');
                }
                first = false;
                out.push_str(#name);
            }
        }
    });
    let ident = &input.ident;
    let (generics, ty_generics, where_clause) = input.generics.split_for_impl();
    Ok(quote! {
        impl #generics ::ext::Schema for #ident #ty_generics #where_clause {
            fn schema(out: &mut ::std::string::String) {
                out.push_str("{\"type\":\"object\",\"properties\":{");
                #(#properties)*
                out.push_str("},\"required\":[");
                let mut first = true;
                #(#required)*
                let _ = first;
                out.push_str("],\"additionalProperties\":false}");
            }
        }
    })
}

fn field(field: &syn::Field) -> syn::Result<Field> {
    let ident = field.ident.as_ref().expect("a named field");
    let mut out = Field {
        name: ident.to_string().trim_start_matches("r#").to_owned(),
        ty: field.ty.clone(),
        doc: String::new(),
        defaulted: false,
        bounds: Vec::new(),
    };
    let mut doc = Vec::new();
    for attr in &field.attrs {
        if attr.path().is_ident("doc") {
            if let Meta::NameValue(nv) = &attr.meta
                && let Expr::Lit(lit) = &nv.value
                && let Lit::Str(text) = &lit.lit
            {
                doc.push(text.value().trim().to_owned());
            }
        } else if attr.path().is_ident("serde") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("default") {
                    out.defaulted = true;
                    skip(&meta)
                } else if meta.path.is_ident("rename") {
                    out.name = meta.value()?.parse::<LitStr>()?.value();
                    Ok(())
                } else {
                    skip(&meta)
                }
            })?;
        } else if attr.path().is_ident("schema") {
            attr.parse_nested_meta(|meta| {
                let key = match meta.path.get_ident().map(ToString::to_string).as_deref() {
                    Some("min") => "minimum",
                    Some("max") => "maximum",
                    Some("min_items") => "minItems",
                    _ => return Err(meta.error("expected min, max or min_items")),
                };
                out.bounds.push((key, meta.value()?.parse()?));
                Ok(())
            })?;
        }
    }
    out.doc = doc.join(" ");
    Ok(out)
}

/// Passes over a serde setting this derive does not read.
fn skip(meta: &ParseNestedMeta<'_>) -> syn::Result<()> {
    if meta.input.peek(syn::Token![=]) {
        meta.value()?.parse::<Expr>()?;
    } else if !meta.input.is_empty() && !meta.input.peek(syn::Token![,]) {
        meta.input.parse::<Tokens>()?;
    }
    Ok(())
}

/// `text` as a JSON string, quoted.
fn json_string(text: &str) -> String {
    let mut out = String::from('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
