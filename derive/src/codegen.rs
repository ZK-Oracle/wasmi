//! This module generates a trait implementation for `Externals` on the target type.
//! It also generates a function called `resolve` that returns a `ModuleImportResolved`.
//!
//! The code generation is rather simple but it relies heavily on type inference.

use crate::parser::{FuncDef, ImplBlockDef};
use proc_macro2::{Ident, Span, TokenStream};
use quote::{quote, quote_spanned, ToTokens};

pub fn codegen(ext_def: &ImplBlockDef, to: &mut TokenStream) {
    let mut externals = TokenStream::new();
    let mut module_resolver = TokenStream::new();

    derive_externals(ext_def, &mut externals);
    derive_module_resolver(ext_def, &mut module_resolver);

    let (impl_generics, _, where_clause) = ext_def.generics.split_for_impl();
    let ty = &ext_def.ty;

    (quote! {
        impl #impl_generics #ty #where_clause {
            const __WASMI_DERIVE_IMPL: () = {
                extern crate wasmi as _wasmi;

                use _wasmi::{
                    Trap, RuntimeValue, RuntimeArgs, Externals, ValueType, ModuleImportResolver,
                    Signature, FuncRef, Error, FuncInstance,
                    derive_support::{
                        IntoWasmResult,
                        IntoWasmValue,
                    },
                };

                #[inline(always)]
                fn materialize_arg_ty<W: IntoWasmValue>(_w: Option<W>) -> ValueType {
                    W::VALUE_TYPE
                }

                #[inline(always)]
                fn materialize_ret_type<W: IntoWasmResult>(_w: Option<W>) -> Option<ValueType> {
                    W::VALUE_TYPE
                }

                #externals
                #module_resolver
            };
        }
    })
    .to_tokens(to);
}

fn emit_dispatch_func_arm(func: &FuncDef) -> TokenStream {
    let index = func.index as usize;
    let return_ty_span = func.return_ty.clone().unwrap_or_else(|| Span::call_site());

    let mut unmarshall_args = TokenStream::new();
    for param in &func.params {
        let param_span = param.ident.span();
        let ident = &param.ident;

        (quote_spanned! {param_span=>
            let #ident =
                args.next()
                    .and_then(|rt_val| rt_val.try_into())
                    .unwrap();
        })
        .to_tokens(&mut unmarshall_args);
    }

    let prologue = quote! {
        let mut args = args.as_ref().iter();
        #unmarshall_args
    };
    let epilogue = quote_spanned! {return_ty_span=>
        IntoWasmResult::into_wasm_result(r)
    };

    let call = {
        let params = func.params.iter().map(|param| param.ident.clone());
        let name = Ident::new(&func.name, Span::call_site());
        quote! {
            #name( #(#params),* )
        }
    };
    (quote! {
        #index => {
            #prologue
            let r = self.#call;
            #epilogue
        }
    })
}

fn derive_externals(ext_def: &ImplBlockDef, to: &mut TokenStream) {
    let (impl_generics, _, where_clause) = ext_def.generics.split_for_impl();
    let ty = &ext_def.ty;

    let mut match_arms = vec![];
    for func in &ext_def.funcs {
        match_arms.push(emit_dispatch_func_arm(func));
    }

    (quote::quote! {
        impl #impl_generics Externals for #ty #where_clause {
            fn invoke_index(
                &mut self,
                index: usize,
                args: RuntimeArgs,
            ) -> Result<Option<RuntimeValue>, Trap> {
                match index {
                    #(#match_arms),*
                    _ => panic!("fn with index {} is undefined", index),
                }
            }

            // ...
        }
    })
    .to_tokens(to);
}

fn emit_resolve_func_arm(func: &FuncDef) -> TokenStream {
    let index = func.index as usize;
    let string_ident = &func.name;
    let return_ty_span = func.return_ty.clone().unwrap_or_else(|| Span::call_site());

    let call = {
        let params = func.params.iter().map(|param| {
            let ident = param.ident.clone();
            let span = param.ident.span();
            quote_spanned! {span=> #ident.unwrap() }
        });
        let name = Ident::new(&func.name, Span::call_site());
        quote! {
            Self::#name( panic!(), #(#params),* )
        }
    };

    let init = func
        .params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            quote! {
                let #ident = None;
            }
        })
        .collect::<Vec<_>>();

    let params_materialized_tys = func
        .params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            let span = param.ident.span();
            quote_spanned! {span=> materialize_arg_ty(#ident) }
        })
        .collect::<Vec<_>>();

    let materialized_return_ty = quote_spanned! { return_ty_span=>
        materialize_ret_type(return_val)
    };

    quote! {
        if name == #string_ident {
            // initialize variables
            #(#init)*

            #[allow(unreachable_code)]
            let return_val = if false {
                // calling self for typeinference
                Some(#call)
            } else {
                None
            };

            // at this point types of all variables and return_val are inferred.
            if signature.params() != &[#(#params_materialized_tys),*]
                || signature.return_type() != #materialized_return_ty
            {
                return Err(Error::Instantiation(
                    format!("Export {} has different signature {:?}", #string_ident, signature),
                ));
            }

            return Ok(FuncInstance::alloc_host(signature.clone(), #index));
        }
    }
}

fn derive_module_resolver(ext_def: &ImplBlockDef, to: &mut TokenStream) {
    let (impl_generics, _, where_clause) = ext_def.generics.split_for_impl();
    let ty = &ext_def.ty;

    let mut match_arms = vec![];
    for func in &ext_def.funcs {
        match_arms.push(emit_resolve_func_arm(func));
    }

    (quote::quote! {
        impl #impl_generics #ty #where_clause {
            fn resolver() -> impl ModuleImportResolver {
                // Use a closure to have an ability to use `Self` type
                let resolve_func = |name: &str, signature: &Signature| -> Result<FuncRef, Error> {
                    #(#match_arms)*

                    Err(Error::Instantiation(
                        format!("Export {} not found", name),
                    ))
                };

                struct Resolver(fn(&str, &Signature) -> Result<FuncRef, Error>);
				impl ModuleImportResolver for Resolver {
                    #[inline(always)]
					fn resolve_func(&self, name: &str, signature: &Signature) -> Result<FuncRef, Error> {
                        (self.0)(name, signature)
					}
				}
				Resolver(resolve_func)
            }
        }
    }).to_tokens(to);
}