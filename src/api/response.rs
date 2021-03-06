use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned, ToTokens, TokenStreamExt};
use syn::{spanned::Spanned, Field, Ident, Lit, Meta, NestedMeta};

use crate::api::strip_serde_attrs;

pub struct Response {
    fields: Vec<ResponseField>,
}

impl Response {
    pub fn has_body_fields(&self) -> bool {
        self.fields.iter().any(|field| field.is_body())
    }

    pub fn has_fields(&self) -> bool {
        !self.fields.is_empty()
    }

    pub fn has_header_fields(&self) -> bool {
        self.fields.iter().any(|field| field.is_header())
    }

    pub fn has_body(&self) -> bool {
        self.fields.iter().any(|field| !field.is_header())
    }

    pub fn init_fields(&self) -> TokenStream {
        let fields = self
            .fields
            .iter()
            .map(|response_field| match *response_field {
                ResponseField::Body(ref field) => {
                    let field_name = field
                        .ident
                        .clone()
                        .expect("expected field to have an identifier");
                    let span = field.span();

                    quote_spanned! {span=>
                        #field_name: response_body.#field_name
                    }
                }
                ResponseField::Header(ref field, ref header) => {
                    let field_name = field
                        .ident
                        .clone()
                        .expect("expected field to have an identifier");
                    let header_name = Ident::new(header.as_ref(), Span::call_site());
                    let span = field.span();

                    quote_spanned! {span=>
                        #field_name: headers.remove(::http::header::#header_name)
                            .expect("response missing expected header")
                            .to_str()
                            .expect("failed to convert HeaderValue to str")
                            .to_owned()
                    }
                }
                ResponseField::NewtypeBody(ref field) => {
                    let field_name = field
                        .ident
                        .clone()
                        .expect("expected field to have an identifier");
                    let span = field.span();

                    quote_spanned! {span=>
                        #field_name: response_body
                    }
                }
            });

        quote! {
            #(#fields,)*
        }
    }

    pub fn apply_header_fields(&self) -> TokenStream {
        let header_calls = self.fields.iter().filter_map(|response_field| {
            if let ResponseField::Header(ref field, ref header) = *response_field {
                let field_name = field
                    .ident
                    .as_ref()
                    .expect("expected field to have an identifier");
                let header_name = Ident::new(header.as_ref(), Span::call_site());
                let span = field.span();

                Some(quote_spanned! {span=>
                    .header(::http::header::#header_name, response.#field_name)
                })
            } else {
                None
            }
        });

        quote! {
            #(#header_calls)*
        }
    }

    pub fn to_body(&self) -> TokenStream {
        if let Some(ref field) = self.newtype_body_field() {
            let field_name = field
                .ident
                .as_ref()
                .expect("expected field to have an identifier");
            let span = field.span();
            quote_spanned!(span=> response.#field_name)
        } else {
            let fields = self.fields.iter().filter_map(|response_field| {
                if let ResponseField::Body(ref field) = *response_field {
                    let field_name = field
                        .ident
                        .as_ref()
                        .expect("expected field to have an identifier");
                    let span = field.span();

                    Some(quote_spanned! {span=>
                        #field_name: response.#field_name
                    })
                } else {
                    None
                }
            });

            quote! {
                ResponseBody {
                    #(#fields),*
                }
            }
        }
    }

    pub fn newtype_body_field(&self) -> Option<&Field> {
        for response_field in self.fields.iter() {
            match *response_field {
                ResponseField::NewtypeBody(ref field) => {
                    return Some(field);
                }
                _ => continue,
            }
        }

        None
    }
}

impl From<Vec<Field>> for Response {
    fn from(fields: Vec<Field>) -> Self {
        let mut has_newtype_body = false;

        let fields = fields.into_iter().map(|mut field| {
            let mut field_kind = ResponseFieldKind::Body;
            let mut header = None;

            field.attrs = field.attrs.into_iter().filter(|attr| {
                let meta = attr.interpret_meta()
                    .expect("ruma_api! could not parse response field attributes");

                let meta_list = match meta {
                    Meta::List(meta_list) => meta_list,
                    _ => return true,
                };

                if &meta_list.ident.to_string() != "ruma_api" {
                    return true;
                }

                for nested_meta_item in meta_list.nested {
                    match nested_meta_item {
                        NestedMeta::Meta(meta_item) => {
                            match meta_item {
                                Meta::Word(ident) => {
                                    match &ident.to_string()[..] {
                                        "body" => {
                                            has_newtype_body = true;
                                            field_kind = ResponseFieldKind::NewtypeBody;
                                        }
                                        _ => panic!("ruma_api! single-word attribute on responses must be: body"),
                                    }
                                }
                                Meta::NameValue(name_value) => {
                                    match &name_value.ident.to_string()[..] {
                                        "header" => {
                                            match name_value.lit {
                                                Lit::Str(lit_str) => header = Some(lit_str.value()),
                                                _ => panic!("ruma_api! header attribute's value must be a string literal"),
                                            }

                                            field_kind = ResponseFieldKind::Header;
                                        }
                                        _ => panic!("ruma_api! name/value pair attribute on requests must be: header"),
                                    }
                                }
                                _ => panic!("ruma_api! attributes on responses must be a single word or a name/value pair"),
                            }
                        }
                        NestedMeta::Literal(_) => panic!(
                            "ruma_api! attribute meta item on responses must be: header"
                        ),
                    }
                }

                false
            }).collect();

            match field_kind {
                ResponseFieldKind::Body => {
                    if has_newtype_body {
                        panic!("ruma_api! responses cannot have both normal body fields and a newtype body field");
                    } else {
                        ResponseField::Body(field)
                    }
                }
                ResponseFieldKind::Header => ResponseField::Header(field, header.expect("missing header name")),
                ResponseFieldKind::NewtypeBody => ResponseField::NewtypeBody(field),
            }
        }).collect();

        Response { fields }
    }
}

impl ToTokens for Response {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        let response_struct_header = quote! {
            /// Data in the response from this API endpoint.
            #[derive(Debug, Clone)]
            pub struct Response
        };

        let response_struct_body = if self.fields.is_empty() {
            quote!(;)
        } else {
            let fields = self.fields.iter().map(|response_field| {
                let field = response_field.field();
                let span = field.span();

                let stripped_field = strip_serde_attrs(field);

                quote_spanned!(span=> #stripped_field)
            });

            quote! {
                {
                    #(#fields),*
                }
            }
        };

        let response_body_struct = if let Some(newtype_body_field) = self.newtype_body_field() {
            let field = newtype_body_field.clone();
            let ty = &field.ty;
            let span = field.span();

            quote_spanned! {span=>
                /// Data in the response body.
                #[derive(Debug, Deserialize, Serialize)]
                struct ResponseBody(#ty);
            }
        } else if self.has_body_fields() {
            let fields = self
                .fields
                .iter()
                .filter_map(|response_field| match *response_field {
                    ResponseField::Body(ref field) => {
                        let span = field.span();
                        Some(quote_spanned!(span=> #field))
                    }
                    _ => None,
                });

            quote! {
                /// Data in the response body.
                #[derive(Debug, Deserialize, Serialize)]
                struct ResponseBody {
                    #(#fields),*
                }
            }
        } else {
            TokenStream::new()
        };

        tokens.append_all(quote! {
            #response_struct_header
            #response_struct_body
            #response_body_struct
        });
    }
}

pub enum ResponseField {
    Body(Field),
    Header(Field, String),
    NewtypeBody(Field),
}

impl ResponseField {
    fn field(&self) -> &Field {
        match *self {
            ResponseField::Body(ref field) => field,
            ResponseField::Header(ref field, _) => field,
            ResponseField::NewtypeBody(ref field) => field,
        }
    }

    fn is_body(&self) -> bool {
        match *self {
            ResponseField::Body(..) => true,
            _ => false,
        }
    }

    fn is_header(&self) -> bool {
        match *self {
            ResponseField::Header(..) => true,
            _ => false,
        }
    }
}

enum ResponseFieldKind {
    Body,
    Header,
    NewtypeBody,
}
