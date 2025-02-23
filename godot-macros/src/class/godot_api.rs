/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use proc_macro2::{Ident, TokenStream};
use quote::quote;
use quote::spanned::Spanned;
use venial::{
    Attribute, AttributeValue, Constant, Declaration, Error, FnParam, Function, Impl, ImplMember,
    TyExpr,
};

use crate::class::{make_method_registration, make_virtual_method_callback, FuncDefinition};
use crate::util;
use crate::util::{bail, KvParser};

pub fn attribute_godot_api(input_decl: Declaration) -> Result<TokenStream, Error> {
    let decl = match input_decl {
        Declaration::Impl(decl) => decl,
        _ => bail!(
            input_decl,
            "#[godot_api] can only be applied on impl blocks",
        )?,
    };

    if decl.impl_generic_params.is_some() {
        bail!(
            &decl,
            "#[godot_api] currently does not support generic parameters",
        )?;
    }

    if decl.self_ty.as_path().is_none() {
        return bail!(decl, "invalid Self type for #[godot_api] impl");
    };

    if decl.trait_ty.is_some() {
        transform_trait_impl(decl)
    } else {
        transform_inherent_impl(decl)
    }
}

// ----------------------------------------------------------------------------------------------------------------------------------------------

/// Attribute for user-declared function
enum BoundAttrType {
    Func {
        rename: Option<String>,
        has_gd_self: bool,
    },
    Signal(AttributeValue),
    Const(AttributeValue),
}

struct BoundAttr {
    attr_name: Ident,
    index: usize,
    ty: BoundAttrType,
}

impl BoundAttr {
    fn bail<R>(self, msg: &str, method: &Function) -> Result<R, Error> {
        bail!(&method.name, "#[{}]: {}", self.attr_name, msg)
    }
}

/// Holds information known from a signal's definition
struct SignalDefinition {
    /// The signal's function signature.
    signature: Function,

    /// The signal's non-gdext attributes (all except #[signal]).
    external_attributes: Vec<Attribute>,
}

/// Codegen for `#[godot_api] impl MyType`
fn transform_inherent_impl(mut decl: Impl) -> Result<TokenStream, Error> {
    let class_name = util::validate_impl(&decl, None, "godot_api")?;
    let class_name_obj = util::class_name_obj(&class_name);
    let (funcs, signals) = process_godot_fns(&mut decl)?;

    let mut signal_cfg_attrs: Vec<Vec<&Attribute>> = Vec::new();
    let mut signal_name_strs: Vec<String> = Vec::new();
    let mut signal_parameters_count: Vec<usize> = Vec::new();
    let mut signal_parameters: Vec<TokenStream> = Vec::new();

    for signal in signals.iter() {
        let SignalDefinition {
            signature,
            external_attributes,
        } = signal;
        let mut param_types: Vec<TyExpr> = Vec::new();
        let mut param_names: Vec<String> = Vec::new();

        for param in signature.params.inner.iter() {
            match &param.0 {
                FnParam::Typed(param) => {
                    param_types.push(param.ty.clone());
                    param_names.push(param.name.to_string());
                }
                FnParam::Receiver(_) => {}
            };
        }

        let signature_tuple = util::make_signature_tuple_type(&quote! { () }, &param_types);
        let indexes = 0..param_types.len();
        let param_array_decl = quote! {
            [
                // Don't use raw sys pointers directly, very easy to have objects going out of scope.
                #(
                    <#signature_tuple as godot::builtin::meta::VarcallSignatureTuple>
                        ::param_property_info(#indexes, #param_names),
                )*
            ]
        };

        // Transport #[cfg] attrs to the FFI glue to ensure signals which were conditionally
        // removed from compilation don't cause errors.
        signal_cfg_attrs.push(
            util::extract_cfg_attrs(external_attributes)
                .into_iter()
                .collect(),
        );
        signal_name_strs.push(signature.name.to_string());
        signal_parameters_count.push(param_names.len());
        signal_parameters.push(param_array_decl);
    }

    let prv = quote! { ::godot::private };

    let methods_registration = funcs
        .into_iter()
        .map(|func_def| make_method_registration(&class_name, func_def));

    let consts = process_godot_constants(&mut decl)?;
    let mut integer_constant_cfg_attrs = Vec::new();
    let mut integer_constant_names = Vec::new();
    let mut integer_constant_values = Vec::new();

    for constant in consts.iter() {
        if constant.initializer.is_none() {
            return bail!(constant, "exported const should have initializer");
        };

        let name = &constant.name;

        // Unlike with #[func] and #[signal], we don't remove the attributes from Constant
        // signatures within 'process_godot_constants'.
        let cfg_attrs = util::extract_cfg_attrs(&constant.attributes)
            .into_iter()
            .collect::<Vec<_>>();

        // Transport #[cfg] attrs to the FFI glue to ensure constants which were conditionally
        // removed from compilation don't cause errors.
        integer_constant_cfg_attrs.push(cfg_attrs);
        integer_constant_names.push(constant.name.to_string());
        integer_constant_values.push(quote! { #class_name::#name });
    }

    let register_constants = if !integer_constant_names.is_empty() {
        quote! {
            use ::godot::builtin::meta::registration::constant::*;
            use ::godot::builtin::meta::ClassName;
            use ::godot::builtin::StringName;

            #(
                #(#integer_constant_cfg_attrs)*
                ExportConstant::new(
                    #class_name_obj,
                    ConstantKind::Integer(
                        IntegerConstant::new(
                            StringName::from(#integer_constant_names),
                            #integer_constant_values
                        )
                    )
                ).register();
            )*
        }
    } else {
        quote! {}
    };

    let result = quote! {
        #decl

        impl ::godot::obj::cap::ImplementsGodotApi for #class_name {
            fn __register_methods() {
                #(
                    #methods_registration
                )*

                unsafe {
                    use ::godot::sys;

                    #(
                        #(#signal_cfg_attrs)*
                        {
                            let parameters_info: [::godot::builtin::meta::PropertyInfo; #signal_parameters_count] = #signal_parameters;

                            let mut parameters_info_sys: [::godot::sys::GDExtensionPropertyInfo; #signal_parameters_count] =
                                std::array::from_fn(|i| parameters_info[i].property_sys());

                            let signal_name = ::godot::builtin::StringName::from(#signal_name_strs);

                            sys::interface_fn!(classdb_register_extension_class_signal)(
                                sys::get_library(),
                                #class_name_obj.string_sys(),
                                signal_name.string_sys(),
                                parameters_info_sys.as_ptr(),
                                sys::GDExtensionInt::from(#signal_parameters_count as i64),
                            );
                        };
                    )*
                }
            }

            fn __register_constants() {
                #register_constants
            }
        }

        impl ::godot::private::Cannot_export_without_godot_api_impl for #class_name {}

        ::godot::sys::plugin_add!(__GODOT_PLUGIN_REGISTRY in #prv; #prv::ClassPlugin {
            class_name: #class_name_obj,
            component: #prv::PluginComponent::UserMethodBinds {
                generated_register_fn: #prv::ErasedRegisterFn {
                    raw: #prv::callbacks::register_user_binds::<#class_name>,
                },
            },
            init_level: <#class_name as ::godot::obj::GodotClass>::INIT_LEVEL,
        });
    };

    Ok(result)
}

fn process_godot_fns(
    decl: &mut Impl,
) -> Result<(Vec<FuncDefinition>, Vec<SignalDefinition>), Error> {
    let mut func_definitions = vec![];
    let mut signal_definitions = vec![];

    let mut removed_indexes = vec![];
    for (index, item) in decl.body_items.iter_mut().enumerate() {
        let method = if let ImplMember::Method(method) = item {
            method
        } else {
            continue;
        };

        if let Some(attr) = extract_attributes(&method, &method.attributes)? {
            // Remaining code no longer has attribute -- rest stays
            method.attributes.remove(attr.index);

            if method.qualifiers.tk_default.is_some()
                || method.qualifiers.tk_const.is_some()
                || method.qualifiers.tk_async.is_some()
                || method.qualifiers.tk_unsafe.is_some()
                || method.qualifiers.tk_extern.is_some()
                || method.qualifiers.extern_abi.is_some()
            {
                return attr.bail("fn qualifiers are not allowed", method);
            }

            if method.generic_params.is_some() {
                return attr.bail("generic fn parameters are not supported", method);
            }

            match &attr.ty {
                BoundAttrType::Func {
                    rename,
                    has_gd_self,
                } => {
                    let external_attributes = method.attributes.clone();
                    // Signatures are the same thing without body
                    let mut sig = util::reduce_to_signature(method);
                    if *has_gd_self {
                        if sig.params.is_empty() {
                            return attr.bail("with attribute key `gd_self`, the method must have a first parameter of type Gd<Self>", method);
                        } else {
                            sig.params.inner.remove(0);
                        }
                    }
                    func_definitions.push(FuncDefinition {
                        func: sig,
                        external_attributes,
                        rename: rename.clone(),
                        has_gd_self: *has_gd_self,
                    });
                }
                BoundAttrType::Signal(ref _attr_val) => {
                    if method.return_ty.is_some() {
                        return attr.bail("return types are not supported", method);
                    }
                    let external_attributes = method.attributes.clone();
                    let sig = util::reduce_to_signature(method);

                    signal_definitions.push(SignalDefinition {
                        signature: sig,
                        external_attributes,
                    });
                    removed_indexes.push(index);
                }
                BoundAttrType::Const(_) => {
                    return attr.bail(
                        "#[constant] can only be used on associated constant",
                        method,
                    )
                }
            }
        }
    }

    // Remove some elements (e.g. signals) from impl.
    // O(n^2); alternative: retain(), but elements themselves don't have the necessary information.
    for index in removed_indexes.into_iter().rev() {
        decl.body_items.remove(index);
    }

    Ok((func_definitions, signal_definitions))
}

fn process_godot_constants(decl: &mut Impl) -> Result<Vec<Constant>, Error> {
    let mut constant_signatures = vec![];

    for item in decl.body_items.iter_mut() {
        let ImplMember::Constant(constant) = item else {
            continue;
        };

        if let Some(attr) = extract_attributes(&constant, &constant.attributes)? {
            // Remaining code no longer has attribute -- rest stays
            constant.attributes.remove(attr.index);

            match attr.ty {
                BoundAttrType::Func { .. } => {
                    return bail!(constant, "#[func] can only be used on functions")
                }
                BoundAttrType::Signal(_) => {
                    return bail!(constant, "#[signal] can only be used on functions")
                }
                BoundAttrType::Const(_) => {
                    if constant.initializer.is_none() {
                        return bail!(constant, "exported constant must have initializer");
                    }
                    constant_signatures.push(constant.clone());
                }
            }
        }
    }

    Ok(constant_signatures)
}

fn extract_attributes<T>(
    error_scope: T,
    attributes: &[Attribute],
) -> Result<Option<BoundAttr>, Error>
where
    for<'a> &'a T: Spanned,
{
    let mut found = None;
    for (index, attr) in attributes.iter().enumerate() {
        let Some(attr_name) = attr.get_single_path_segment() else {
            // Attribute of the form #[segmented::path] can't be what we are looking for
            continue;
        };

        let new_found = match attr_name {
            name if name == "func" => {
                // TODO you-win (August 8, 2023): handle default values here as well?

                // Safe unwrap since #[func] must be present if we got to this point
                let mut parser = KvParser::parse(attributes, "func")?.unwrap();

                let rename = parser.handle_expr("rename")?.map(|ts| ts.to_string());
                let has_gd_self = parser.handle_alone("gd_self")?;

                BoundAttr {
                    attr_name: attr_name.clone(),
                    index,
                    ty: BoundAttrType::Func {
                        rename,
                        has_gd_self,
                    },
                }
            }
            name if name == "signal" => {
                // TODO once parameters are supported, this should probably be moved to the struct definition
                // E.g. a zero-sized type Signal<(i32, String)> with a provided emit(i32, String) method
                // This could even be made public (callable on the struct obj itself)
                BoundAttr {
                    attr_name: attr_name.clone(),
                    index,
                    ty: BoundAttrType::Signal(attr.value.clone()),
                }
            }
            name if name == "constant" => BoundAttr {
                attr_name: attr_name.clone(),
                index,
                ty: BoundAttrType::Const(attr.value.clone()),
            },
            // Ignore unknown attributes
            _ => continue,
        };

        // Validate at most 1 attribute
        if found.is_some() {
            bail!(
                &error_scope,
                "at most one #[func], #[signal], or #[constant] attribute per declaration allowed",
            )?;
        }

        found = Some(new_found);
    }

    Ok(found)
}

// ----------------------------------------------------------------------------------------------------------------------------------------------

/// Expects either Some(quote! { () => A, () => B, ... }) or None as the 'tokens' parameter.
/// The idea is that the () => ... arms can be annotated by cfg attrs, so, if any of them compiles (and assuming the cfg
/// attrs only allow one arm to 'survive' compilation), their return value (Some(...)) will be prioritized over the
/// 'None' from the catch-all arm at the end. If, however, none of them compile, then None is returned from the last
/// match arm.
fn convert_to_match_expression_or_none(tokens: Option<TokenStream>) -> TokenStream {
    if let Some(tokens) = tokens {
        quote! {
            {
                // When one of the () => ... arms is present, the last arm intentionally won't ever match.
                #[allow(unreachable_patterns)]
                // Don't warn when only _ => None is present as all () => ... arms were removed from compilation.
                #[allow(clippy::match_single_binding)]
                match () {
                    #tokens
                    _ => None,
                }
            }
        }
    } else {
        quote! { None }
    }
}

/// Codegen for `#[godot_api] impl GodotExt for MyType`
fn transform_trait_impl(original_impl: Impl) -> Result<TokenStream, Error> {
    let (class_name, trait_name) = util::validate_trait_impl_virtual(&original_impl, "godot_api")?;
    let class_name_obj = util::class_name_obj(&class_name);

    let mut godot_init_impl = TokenStream::new();
    let mut to_string_impl = TokenStream::new();
    let mut register_class_impl = TokenStream::new();
    let mut on_notification_impl = TokenStream::new();

    let mut register_fn = None;
    let mut create_fn = None;
    let mut recreate_fn = None;
    let mut to_string_fn = None;
    let mut on_notification_fn = None;

    let mut virtual_methods = vec![];
    let mut virtual_method_cfg_attrs = vec![];
    let mut virtual_method_names = vec![];

    let prv = quote! { ::godot::private };

    for item in original_impl.body_items.iter() {
        let method = if let ImplMember::Method(f) = item {
            f
        } else {
            continue;
        };

        // Transport #[cfg] attributes to the virtual method's FFI glue, to ensure it won't be
        // registered in Godot if conditionally removed from compilation.
        let cfg_attrs = util::extract_cfg_attrs(&method.attributes)
            .into_iter()
            .collect::<Vec<_>>();
        let method_name = method.name.to_string();
        match method_name.as_str() {
            "register_class" => {
                // Implements the trait once for each implementation of this method, forwarding the cfg attrs of each
                // implementation to the generated trait impl. If the cfg attrs allow for multiple implementations of
                // this method to exist, then Rust will generate an error, so we don't have to worry about the multiple
                // trait implementations actually generating an error, since that can only happen if multiple
                // implementations of the same method are kept by #[cfg] (due to user error).
                // Thus, by implementing the trait once for each possible implementation of this method (depending on
                // what #[cfg] allows), forwarding the cfg attrs, we ensure this trait impl will remain in the code if
                // at least one of the method impls are kept.
                register_class_impl = quote! {
                    #register_class_impl

                    #(#cfg_attrs)*
                    impl ::godot::obj::cap::GodotRegisterClass for #class_name {
                        fn __godot_register_class(builder: &mut ::godot::builder::GodotBuilder<Self>) {
                            <Self as #trait_name>::register_class(builder)
                        }
                    }
                };

                // Adds a match arm for each implementation of this method, transferring its respective cfg attrs to
                // the corresponding match arm (see explanation for the match after this loop).
                // In principle, the cfg attrs will allow only either 0 or 1 of a function with this name to exist,
                // unless there are duplicate implementations for the same method, which should error anyway.
                // Thus, in any correct program, the match arms (which are, in principle, identical) will be reduced to
                // a single one at most, since we forward the cfg attrs. The idea here is precisely to keep this
                // specific match arm 'alive' if at least one implementation of the method is also kept (hence why all
                // the match arms are identical).
                register_fn = Some(quote! {
                    #register_fn
                    #(#cfg_attrs)*
                    () => Some(#prv::ErasedRegisterFn {
                        raw: #prv::callbacks::register_class_by_builder::<#class_name>
                    }),
                });
            }

            "init" => {
                godot_init_impl = quote! {
                    #godot_init_impl

                    #(#cfg_attrs)*
                    impl ::godot::obj::cap::GodotInit for #class_name {
                        fn __godot_init(base: ::godot::obj::Base<Self::Base>) -> Self {
                            <Self as #trait_name>::init(base)
                        }
                    }
                };
                create_fn = Some(quote! {
                    #create_fn
                    #(#cfg_attrs)*
                    () => Some(#prv::callbacks::create::<#class_name>),
                });
                if cfg!(since_api = "4.2") {
                    recreate_fn = Some(quote! {
                        #recreate_fn
                        #(#cfg_attrs)*
                        () => Some(#prv::callbacks::recreate::<#class_name>),
                    });
                }
            }

            "to_string" => {
                to_string_impl = quote! {
                    #to_string_impl

                    #(#cfg_attrs)*
                    impl ::godot::obj::cap::GodotToString for #class_name {
                        fn __godot_to_string(&self) -> ::godot::builtin::GString {
                            <Self as #trait_name>::to_string(self)
                        }
                    }
                };

                to_string_fn = Some(quote! {
                    #to_string_fn
                    #(#cfg_attrs)*
                    () => Some(#prv::callbacks::to_string::<#class_name>),
                });
            }

            "on_notification" => {
                on_notification_impl = quote! {
                    #on_notification_impl

                    #(#cfg_attrs)*
                    impl ::godot::obj::cap::GodotNotification for #class_name {
                        fn __godot_notification(&mut self, what: i32) {
                            if ::godot::private::is_class_inactive(Self::__config().is_tool) {
                                return;
                            }

                            <Self as #trait_name>::on_notification(self, what.into())
                        }
                    }
                };

                on_notification_fn = Some(quote! {
                    #on_notification_fn
                    #(#cfg_attrs)*
                    () => Some(#prv::callbacks::on_notification::<#class_name>),
                });
            }

            // Other virtual methods, like ready, process etc.
            _ => {
                let method = util::reduce_to_signature(method);

                // Godot-facing name begins with underscore
                //
                // Note: godot-codegen special-cases the virtual
                // method called _init (which exists on a handful of
                // classes, distinct from the default constructor) to
                // init_ext, to avoid Rust-side ambiguity. See
                // godot_codegen::class_generator::virtual_method_name.
                let virtual_method_name = if method_name == "init_ext" {
                    String::from("_init")
                } else {
                    format!("_{method_name}")
                };
                // Note that, if the same method is implemented multiple times (with different cfg attr combinations),
                // then there will be multiple match arms annotated with the same cfg attr combinations, thus they will
                // be reduced to just one arm (at most, if the implementations aren't all removed from compilation) for
                // each distinct method.
                virtual_method_cfg_attrs.push(cfg_attrs);
                virtual_method_names.push(virtual_method_name);
                virtual_methods.push(method);
            }
        }
    }

    let virtual_method_callbacks: Vec<TokenStream> = virtual_methods
        .iter()
        .map(|method| make_virtual_method_callback(&class_name, method))
        .collect();

    // Use 'match' as a way to only emit 'Some(...)' if the given cfg attrs allow.
    // This permits users to conditionally remove virtual method impls from compilation while also removing their FFI
    // glue which would otherwise make them visible to Godot even if not really implemented.
    // Needs '#[allow(unreachable_patterns)]' to avoid warnings about the last match arm.
    // Also requires '#[allow(clippy::match_single_binding)]' for similar reasons.
    let register_fn = convert_to_match_expression_or_none(register_fn);
    let create_fn = convert_to_match_expression_or_none(create_fn);
    let recreate_fn = convert_to_match_expression_or_none(recreate_fn);
    let to_string_fn = convert_to_match_expression_or_none(to_string_fn);
    let on_notification_fn = convert_to_match_expression_or_none(on_notification_fn);

    let result = quote! {
        #original_impl
        #godot_init_impl
        #to_string_impl
        #on_notification_impl
        #register_class_impl

        impl ::godot::private::You_forgot_the_attribute__godot_api for #class_name {}

        impl ::godot::obj::cap::ImplementsGodotVirtual for #class_name {
            fn __virtual_call(name: &str) -> ::godot::sys::GDExtensionClassCallVirtual {
                //println!("virtual_call: {}.{}", std::any::type_name::<Self>(), name);

                if ::godot::private::is_class_inactive(Self::__config().is_tool) {
                    return None;
                }

                match name {
                    #(
                       #(#virtual_method_cfg_attrs)*
                       #virtual_method_names => #virtual_method_callbacks,
                    )*
                    _ => None,
                }
            }
        }

        ::godot::sys::plugin_add!(__GODOT_PLUGIN_REGISTRY in #prv; #prv::ClassPlugin {
            class_name: #class_name_obj,
            component: #prv::PluginComponent::UserVirtuals {
                user_register_fn: #register_fn,
                user_create_fn: #create_fn,
                user_recreate_fn: #recreate_fn,
                user_to_string_fn: #to_string_fn,
                user_on_notification_fn: #on_notification_fn,
                get_virtual_fn: #prv::callbacks::get_virtual::<#class_name>,
            },
            init_level: <#class_name as ::godot::obj::GodotClass>::INIT_LEVEL,
        });
    };

    Ok(result)
}
