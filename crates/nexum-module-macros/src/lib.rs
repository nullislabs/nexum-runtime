//! Proc-macro glue for nexum runtime modules.
//!
//! [`module`] turns an `impl` block of named handlers into a complete
//! per-cdylib module. Reach it through `nexum_sdk::module`, not this
//! crate directly; a downstream layer supplies its own venue-side macros.

#![forbid(unsafe_code)]

use alloy_primitives::B256;
use proc_macro::TokenStream;
use quote::{ToTokens, quote};
use syn::{ImplItem, ItemImpl};

/// The handler names recognized on a `#[module]` impl. An `on_`-prefixed
/// method outside this set is a compile error; an absent handler
/// dispatches as a no-op.
const HANDLERS: [&str; 5] = ["init", "on_block", "on_chain_logs", "on_tick", "on_custom"];

/// Generate the per-cdylib glue for a nexum module.
///
/// Apply to an `impl` block whose associated functions are the event
/// handlers (`init`, `on_block`, `on_chain_logs`, `on_tick`,
/// `on_custom`); each takes its event's wit-bindgen
/// payload and returns `Result<(), Fault>`, and `init` takes the config
/// table. Undefined handlers dispatch as no-ops. Emits
/// `wit_bindgen::generate!`, the host adapter, the `Guest` impl, and
/// `export!` around the untouched impl.
///
/// The world is per module: the macro reads the crate's `component.toml`
/// and synthesizes a world importing exactly the
/// `[dependencies]` keys, so the
/// load-time capability check passes by construction. An undeclared
/// capability's bindings do not exist. Requirements: the manifest sits
/// at the crate root with a `[dependencies]` table; the crate depends
/// on `wit-bindgen` directly; and the crate root must not shadow the
/// std prelude names `Result`, `Vec`, or `Ok` (the generated `Guest`
/// trait refers to them unqualified).
///
/// `subscribes(EventType, ...)` fails the build unless the named events'
/// `SolEvent::SIGNATURE_HASH` values and the manifest's chain-log
/// `event_signature` values match as sets; the manifest stays authoritative.
#[proc_macro_attribute]
pub fn module(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = match parse_args(attr.into()) {
        Ok(args) => args,
        Err(error) => return error.into_syn_error().to_compile_error().into(),
    };
    let input = syn::parse_macro_input!(item as ItemImpl);
    match expand(args, input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_syn_error().to_compile_error().into(),
    }
}

/// The fallible body of [`module`]; every refusal is a [`MacroError`]
/// the entry point renders once.
fn expand(args: ModuleArgs, input: ItemImpl) -> Result<proc_macro2::TokenStream, MacroError> {
    let self_ty: &syn::Type = &input.self_ty;
    if !nexum_world::is_plain_type(self_ty) {
        return Err(MacroError::UnnamedSelfType {
            self_ty: self_ty.to_token_stream(),
        });
    }
    if let Some((_, trait_path, _)) = &input.trait_ {
        return Err(MacroError::TraitImpl {
            trait_path: trait_path.to_token_stream(),
        });
    }
    if !input.generics.params.is_empty() {
        return Err(MacroError::GenericImpl {
            generics: input.generics.to_token_stream(),
        });
    }

    // A typo'd handler (`on_blocks`, `on_chainlogs`, ...) would otherwise
    // compile as an ordinary helper while its event silently no-ops, so
    // reserve the `on_` prefix for the recognized handler set.
    for item in &input.items {
        if let ImplItem::Fn(f) = item {
            let name = f.sig.ident.to_string();
            if name.starts_with("on_") && !HANDLERS.contains(&name.as_str()) {
                return Err(MacroError::UnknownHandler {
                    ident: f.sig.ident.to_token_stream(),
                });
            }
        }
    }

    let present: Vec<&str> = input
        .items
        .iter()
        .filter_map(|item| match item {
            ImplItem::Fn(f) => {
                let name = f.sig.ident.to_string();
                HANDLERS.into_iter().find(|h| *h == name)
            }
            _ => None,
        })
        .collect();
    if present.is_empty() {
        return Err(MacroError::NoHandlers {
            self_ty: self_ty.to_token_stream(),
        });
    }
    let has = |name: &str| present.contains(&name);

    let ManifestFacts {
        anchors,
        world: module_world,
        chain_log_topics,
    } = nexum_world::manifest_dir()
        .map_err(|source| MacroError::NoManifestDir { source })
        .and_then(|dir| derive_manifest_facts(&dir, !args.subscribes.is_empty()))?;
    let parity = topic_parity_check(&args.subscribes, &chain_log_topics);
    let wit_paths = nexum_world::manifest_wit_packages(&module_world.packages)
        .map_err(|source| MacroError::WitResolution { source })?;
    let inline_world = &module_world.wit;
    let adapter_bind = adapter_bind(&module_world.adapters);

    let init_impl = init_export(self_ty, has("init"));

    let block_arm = dispatch_arm(self_ty, &present, "on_block", "Block");
    let logs_arm = dispatch_arm(self_ty, &present, "on_chain_logs", "ChainLogs");
    let tick_arm = dispatch_arm(self_ty, &present, "on_tick", "Tick");
    let custom_arm = dispatch_arm(self_ty, &present, "on_custom", "Custom");

    let anchors = rebuild_anchors(&anchors);
    Ok(quote! {
        #anchors

        wit_bindgen::generate!({
            inline: #inline_world,
            path: [#(#wit_paths),*],
            world: "nexum:module-world/module",
            generate_all,
        });

        #adapter_bind

        #parity

        #input

        #[doc(hidden)]
        struct __NexumModuleExport;

        impl Guest for __NexumModuleExport {
            #init_impl

            fn on_event(event: nexum::host::types::Event) -> ::core::result::Result<(), Fault> {
                match event {
                    #block_arm
                    #logs_arm
                    #tick_arm
                    #custom_arm
                }
            }
        }

        export!(__NexumModuleExport);
    })
}

/// A refusal from the macro's own rules. `Display` is the module-author
/// contract: the entry point renders it verbatim as the compile error,
/// so each variant's wording is pinned by test. Spanned variants carry
/// the span, or the tokens the diagnostic underlines, so conversion
/// reproduces today's placement.
#[derive(Debug, thiserror::Error)]
enum MacroError {
    /// The attribute's arguments start with something other than an
    /// identifier.
    #[error("expected `subscribes(EventType, ...)` or no arguments")]
    NonIdentArgument {
        /// The offending token.
        span: proc_macro2::Span,
    },
    /// The attribute names an argument other than `subscribes`.
    #[error("#[nexum_sdk::module] takes no arguments except `subscribes(EventType, ...)`")]
    UnknownArgument {
        /// The argument's name token.
        span: proc_macro2::Span,
    },
    /// Tokens follow the closing parenthesis of `subscribes(...)`.
    #[error("unexpected tokens after `subscribes(...)`")]
    TrailingTokens {
        /// The first trailing token.
        span: proc_macro2::Span,
    },
    /// `subscribes()` names no event type.
    #[error("`subscribes(...)` must name at least one event type")]
    EmptySubscribes {
        /// The `subscribes` identifier.
        span: proc_macro2::Span,
    },
    /// The arguments broke syn's own grammar (an unbalanced list, a
    /// malformed path); the diagnostic is syn's, passed through intact.
    #[error(transparent)]
    MalformedArgs(syn::Error),
    /// The impl's self type is not a plain named type.
    #[error("#[nexum_sdk::module] must be applied to an inherent impl of a named type")]
    UnnamedSelfType {
        /// The self type's tokens.
        self_ty: proc_macro2::TokenStream,
    },
    /// The attribute sits on a trait impl.
    #[error("#[nexum_sdk::module] must be applied to an inherent impl, not a trait impl")]
    TraitImpl {
        /// The trait path's tokens.
        trait_path: proc_macro2::TokenStream,
    },
    /// The impl is generic.
    #[error("#[nexum_sdk::module] must be applied to a non-generic impl")]
    GenericImpl {
        /// The generics' tokens.
        generics: proc_macro2::TokenStream,
    },
    /// An `on_`-prefixed method outside the recognized handler set.
    #[error(
        "`{}` is not a recognized #[nexum_sdk::module] handler; expected one \
         of {:?} (rename helpers so they do not start with `on_`)",
        .ident,
        HANDLERS
    )]
    UnknownHandler {
        /// The method's name token.
        ident: proc_macro2::TokenStream,
    },
    /// No recognized handler on the impl.
    #[error(
        "#[nexum_sdk::module] found no recognized handlers on this impl; define at least one \
         of `init`, `on_block`, `on_chain_logs`, `on_tick`, `on_custom`"
    )]
    NoHandlers {
        /// The self type's tokens.
        self_ty: proc_macro2::TokenStream,
    },
    /// No `CARGO_MANIFEST_DIR`, so there is no crate root to read.
    #[error("{source}")]
    NoManifestDir {
        /// The refusal from `nexum_world::manifest_dir`.
        source: nexum_world::WorldError,
    },
    /// The crate's `component.toml` could not be read.
    #[error(
        "could not read {path} ({source}); #[nexum_sdk::module] derives the component's WIT \
         world from the manifest's [dependencies] table, so the manifest must sit next to \
         Cargo.toml"
    )]
    ManifestUnreadable {
        /// The manifest path.
        path: String,
        /// The read failure.
        source: std::io::Error,
    },
    /// A manifest rule refused by nexum-world, prefixed with the
    /// offending manifest's path.
    #[error("{path}: {source}")]
    Manifest {
        /// The manifest path.
        path: String,
        /// The refused rule, boxed to keep the enum under clippy's
        /// `result_large_err` limit.
        source: Box<nexum_world::WorldError>,
    },
    /// The `extensions.toml` registry could not be read.
    #[error("could not read {path}: {source}")]
    RegistryUnreadable {
        /// The registry path.
        path: String,
        /// The read failure.
        source: std::io::Error,
    },
    /// The declared capabilities' WIT packages could not be resolved.
    #[error("{source}")]
    WitResolution {
        /// The refusal from `nexum_world::manifest_wit_packages`.
        source: nexum_world::WorldError,
    },
    /// `subscribes(...)` names events, but the manifest declares no
    /// chain-log subscription.
    #[error(
        "`subscribes(...)` names events, but {path} declares no chain-log subscription with \
         an `event_signature`; add the subscription or drop the argument"
    )]
    SubscribesWithoutChainLog {
        /// The manifest path.
        path: String,
    },
}

impl MacroError {
    /// The single conversion to the rendered diagnostic. Token-carrying
    /// variants re-attach exactly the tokens they carry, span-carrying
    /// ones their span; manifest variants sit at the call site because
    /// no attribute token names them.
    fn into_syn_error(self) -> syn::Error {
        let message = self.to_string();
        match self {
            Self::MalformedArgs(error) => error,
            Self::NonIdentArgument { span }
            | Self::UnknownArgument { span }
            | Self::TrailingTokens { span }
            | Self::EmptySubscribes { span } => syn::Error::new(span, message),
            Self::UnnamedSelfType { self_ty: tokens }
            | Self::TraitImpl { trait_path: tokens }
            | Self::GenericImpl { generics: tokens }
            | Self::UnknownHandler { ident: tokens }
            | Self::NoHandlers { self_ty: tokens } => syn::Error::new_spanned(tokens, message),
            Self::NoManifestDir { .. }
            | Self::ManifestUnreadable { .. }
            | Self::Manifest { .. }
            | Self::RegistryUnreadable { .. }
            | Self::WitResolution { .. }
            | Self::SubscribesWithoutChainLog { .. } => {
                syn::Error::new(proc_macro2::Span::call_site(), message)
            }
        }
    }
}

/// The macro's arguments: bare, or `subscribes(EventType, ...)`.
struct ModuleArgs {
    subscribes: Vec<syn::Path>,
}

/// Parse the attribute arguments. The macro's own rules refuse with a
/// [`MacroError`]; a failure of syn's own grammar passes through as
/// [`MacroError::MalformedArgs`].
fn parse_args(tokens: proc_macro2::TokenStream) -> Result<ModuleArgs, MacroError> {
    match syn::parse::Parser::parse2(parse_args_inner, tokens) {
        Ok(parsed) => parsed,
        Err(error) => Err(MacroError::MalformedArgs(error)),
    }
}

/// The stream-level grammar behind [`parse_args`]: `Ok(Err(_))` is one
/// of the macro's refusals, `Err(_)` is syn's.
fn parse_args_inner(
    input: syn::parse::ParseStream<'_>,
) -> syn::Result<Result<ModuleArgs, MacroError>> {
    if input.is_empty() {
        return Ok(Ok(ModuleArgs {
            subscribes: Vec::new(),
        }));
    }
    let span = input.span();
    let Ok(ident) = input.parse::<syn::Ident>() else {
        return refuse(input, MacroError::NonIdentArgument { span });
    };
    if ident != "subscribes" {
        return refuse(input, MacroError::UnknownArgument { span: ident.span() });
    }
    let inner;
    syn::parenthesized!(inner in input);
    if !input.is_empty() {
        let span = input.span();
        inner.parse::<proc_macro2::TokenStream>()?;
        return refuse(input, MacroError::TrailingTokens { span });
    }
    let paths =
        inner.call(syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)?;
    if paths.is_empty() {
        return refuse(input, MacroError::EmptySubscribes { span: ident.span() });
    }
    Ok(Ok(ModuleArgs {
        subscribes: paths.into_iter().collect(),
    }))
}

/// Refuse with the macro's own error, draining `input` first so
/// `Parser::parse2` does not report the unconsumed rest instead.
fn refuse(
    input: syn::parse::ParseStream<'_>,
    error: MacroError,
) -> syn::Result<Result<ModuleArgs, MacroError>> {
    input.parse::<proc_macro2::TokenStream>()?;
    Ok(Err(error))
}

struct ManifestFacts {
    /// Rebuild anchor paths: the manifests the emitted world depends on.
    anchors: Vec<String>,
    world: nexum_world::ModuleWorld,
    /// Distinct chain-log `event_signature` topics, in declaration order.
    chain_log_topics: Vec<B256>,
}

/// Synthesize the per-module world from the crate's `component.toml`
/// `[dependencies]` plus the nearest ancestor `extensions.toml`.
/// Topics are read only for `want_topics`, so a manifest field no
/// opted-in module names can never fail a build; a `want_topics` manifest
/// with no chain-log subscription refuses.
fn derive_manifest_facts(
    crate_dir: &std::path::Path,
    want_topics: bool,
) -> Result<ManifestFacts, MacroError> {
    let file = crate_dir.join("component.toml");
    let manifest_path = file.to_string_lossy().into_owned();
    let text = std::fs::read_to_string(&file).map_err(|source| MacroError::ManifestUnreadable {
        path: manifest_path.clone(),
        source,
    })?;
    let declared =
        nexum_world::manifest_capabilities(&text).map_err(|source| MacroError::Manifest {
            path: manifest_path.clone(),
            source: Box::new(source),
        })?;
    let chain_log_topics = if want_topics {
        nexum_world::manifest_chain_log_topics(&text).map_err(|source| MacroError::Manifest {
            path: manifest_path.clone(),
            source: Box::new(source),
        })?
    } else {
        Vec::new()
    };

    let mut anchors = vec![manifest_path.clone()];
    let extensions = match nexum_world::find_extensions_manifest(crate_dir) {
        None => Vec::new(),
        Some(registry) => {
            let registry_path = registry.to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&registry).map_err(|source| {
                MacroError::RegistryUnreadable {
                    path: registry_path.clone(),
                    source,
                }
            })?;
            let rows =
                nexum_world::manifest_extensions(&text).map_err(|source| MacroError::Manifest {
                    path: registry_path.clone(),
                    source: Box::new(source),
                })?;
            anchors.push(registry_path);
            rows
        }
    };
    let world =
        nexum_world::synthesize(&declared, &extensions).map_err(|source| MacroError::Manifest {
            path: manifest_path.clone(),
            source: Box::new(source),
        })?;
    // Last, so a manifest rule refusal above keeps surfacing first.
    if want_topics && chain_log_topics.is_empty() {
        return Err(MacroError::SubscribesWithoutChainLog {
            path: manifest_path,
        });
    }
    Ok(ManifestFacts {
        anchors,
        world,
        chain_log_topics,
    })
}

/// Const assertions pinning set equality between the `subscribes(...)`
/// events' topic-0 hashes and the manifest's chain-log topics. Const
/// eval stops at the first failure, so every message names both sides:
/// a `SIGNATURE_HASH` cannot be formatted into one.
fn topic_parity_check(events: &[syn::Path], topics: &[B256]) -> proc_macro2::TokenStream {
    if events.is_empty() {
        return proc_macro2::TokenStream::new();
    }
    let n = events.len();
    let m = topics.len();
    let declared_list = events
        .iter()
        .map(path_string)
        .collect::<Vec<_>>()
        .join(", ");
    let manifest_list = topics
        .iter()
        .map(B256::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let manifest_topics = topics.iter().map(|topic| {
        let bytes = topic.0;
        quote! { ::nexum_sdk::prelude::B256::new([#(#bytes),*]) }
    });
    let declared_topics = events.iter().map(|path| {
        quote! { <#path as ::nexum_sdk::sol_types::SolEvent>::SIGNATURE_HASH }
    });
    let declared_checks = events.iter().enumerate().map(|(i, path)| {
        let msg = format!(
            "topic drift: `{}`'s topic-0 is not among the component.toml chain-log event_signature \
             values [{manifest_list}]",
            path_string(path),
        );
        quote! {
            ::core::assert!(
                ::nexum_sdk::events::contains_topic(&DECLARED[#i], &MANIFEST),
                #msg,
            );
        }
    });
    let manifest_checks = topics.iter().enumerate().map(|(j, topic)| {
        let msg = format!(
            "topic drift: component.toml chain-log event_signature {topic} is not the topic-0 of any \
             of subscribes({declared_list})",
        );
        quote! {
            ::core::assert!(
                ::nexum_sdk::events::contains_topic(&MANIFEST[#j], &DECLARED),
                #msg,
            );
        }
    });
    quote! {
        const _: () = {
            const MANIFEST: [::nexum_sdk::prelude::B256; #m] = [#(#manifest_topics),*];
            const DECLARED: [::nexum_sdk::prelude::B256; #n] = [#(#declared_topics),*];
            #(#declared_checks)*
            #(#manifest_checks)*
        };
    }
}

/// Binds only the world's declared adapters, so an undeclared
/// capability's adapter is never emitted.
fn adapter_bind(adapters: &[&str]) -> proc_macro2::TokenStream {
    let caps: Vec<syn::Ident> = adapters
        .iter()
        .map(|cap| syn::Ident::new(cap, proc_macro2::Span::call_site()))
        .collect();
    quote! { ::nexum_sdk::bind_host_via_wit_bindgen!(caps: [#(#caps),*]); }
}

/// `init` is required by the world, so it is emitted even with no
/// handler; the config binding is dropped then to stay warning-clean.
fn init_export(self_ty: &syn::Type, has_init: bool) -> proc_macro2::TokenStream {
    if has_init {
        quote! {
            fn init(
                config: ::std::vec::Vec<(::std::string::String, ::std::string::String)>,
            ) -> ::core::result::Result<(), Fault> {
                <#self_ty>::init(config)
            }
        }
    } else {
        quote! {
            fn init(
                _config: ::std::vec::Vec<(::std::string::String, ::std::string::String)>,
            ) -> ::core::result::Result<(), Fault> {
                ::core::result::Result::Ok(())
            }
        }
    }
}

/// One `on_event` arm; an absent handler dispatches as a no-op.
fn dispatch_arm(
    self_ty: &syn::Type,
    present: &[&str],
    handler: &str,
    variant: &str,
) -> proc_macro2::TokenStream {
    let variant = syn::Ident::new(variant, proc_macro2::Span::call_site());
    if present.contains(&handler) {
        let call = syn::Ident::new(handler, proc_macro2::Span::call_site());
        quote! { nexum::host::types::Event::#variant(payload) => <#self_ty>::#call(payload), }
    } else {
        quote! { nexum::host::types::Event::#variant(_) => ::core::result::Result::Ok(()), }
    }
}

/// Cargo reruns a build on an `include_bytes!` target's mtime, which is the
/// only thing making a manifest edit retrigger expansion.
fn rebuild_anchors(anchors: &[String]) -> proc_macro2::TokenStream {
    quote! { #(const _: &[u8] = ::core::include_bytes!(#anchors);)* }
}

/// A path's source spelling, without quote's token spacing.
fn path_string(path: &syn::Path) -> String {
    path.to_token_stream().to_string().replace(' ', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_attribute_names_no_events() {
        assert!(parse_args(quote! {}).unwrap().subscribes.is_empty());
    }

    #[test]
    fn subscribes_parses_paths_in_order() {
        let args = parse_args(quote! { subscribes(OrderPlacement, events::Refund) }).unwrap();
        let names: Vec<String> = args.subscribes.iter().map(path_string).collect();
        assert_eq!(names, ["OrderPlacement", "events::Refund"]);
    }

    #[test]
    fn empty_subscribes_is_rejected() {
        let err = parse_args(quote! { subscribes() }).err().unwrap();
        assert!(matches!(err, MacroError::EmptySubscribes { .. }), "{err:?}");
    }

    #[test]
    fn non_ident_argument_is_rejected() {
        let err = parse_args(quote! { 42 }).err().unwrap();
        assert!(
            matches!(err, MacroError::NonIdentArgument { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn unknown_argument_is_rejected() {
        let err = parse_args(quote! { emits(Foo) }).err().unwrap();
        assert!(matches!(err, MacroError::UnknownArgument { .. }), "{err:?}");
    }

    #[test]
    fn trailing_tokens_are_rejected() {
        let err = parse_args(quote! { subscribes(Foo), extra }).err().unwrap();
        assert!(matches!(err, MacroError::TrailingTokens { .. }), "{err:?}");
    }

    /// A failure of syn's own grammar keeps syn's diagnostic.
    #[test]
    fn malformed_args_pass_through_syn_diagnostics() {
        let err = parse_args(quote! { subscribes(1) }).err().unwrap();
        assert!(matches!(err, MacroError::MalformedArgs(_)), "{err:?}");
    }

    /// Strips quote's token spacing so an assertion reads as source.
    fn flat(tokens: proc_macro2::TokenStream) -> String {
        tokens.to_string().replace(' ', "")
    }

    #[test]
    fn present_handler_arm_calls_the_impl() {
        let ty: syn::Type = syn::parse_quote!(Watcher);
        let arm = flat(dispatch_arm(&ty, &["on_block"], "on_block", "Block"));
        assert_eq!(
            arm,
            "nexum::host::types::Event::Block(payload)=><Watcher>::on_block(payload),",
        );
    }

    #[test]
    fn absent_handler_arm_is_a_no_op() {
        let ty: syn::Type = syn::parse_quote!(Watcher);
        let arm = flat(dispatch_arm(&ty, &["on_block"], "on_tick", "Tick"));
        assert_eq!(
            arm,
            "nexum::host::types::Event::Tick(_)=>::core::result::Result::Ok(()),",
        );
    }

    /// Every handler dispatches on its own event variant: a payload can
    /// never reach another handler's arm.
    #[test]
    fn each_handler_binds_its_own_variant() {
        let ty: syn::Type = syn::parse_quote!(Watcher);
        let pairs = [
            ("on_block", "Block"),
            ("on_chain_logs", "ChainLogs"),
            ("on_tick", "Tick"),
            ("on_custom", "Custom"),
        ];
        let all: Vec<&str> = pairs.iter().map(|(h, _)| *h).collect();
        for (handler, variant) in pairs {
            let arm = flat(dispatch_arm(&ty, &all, handler, variant));
            assert!(arm.contains(&format!("Event::{variant}(payload)")), "{arm}");
            assert!(
                arm.contains(&format!("<Watcher>::{handler}(payload)")),
                "{arm}"
            );
        }
    }

    #[test]
    fn defined_init_export_forwards_the_config() {
        let ty: syn::Type = syn::parse_quote!(Watcher);
        let emitted = flat(init_export(&ty, true));
        assert!(emitted.contains("<Watcher>::init(config)"), "{emitted}");
    }

    #[test]
    fn absent_init_export_is_a_no_op() {
        let ty: syn::Type = syn::parse_quote!(Watcher);
        let emitted = flat(init_export(&ty, false));
        assert!(emitted.contains("_config"), "{emitted}");
        assert!(
            emitted.contains("::core::result::Result::Ok(())"),
            "{emitted}",
        );
        assert!(!emitted.contains("Watcher>::init"), "{emitted}");
    }

    /// An accepted manifest's declared capabilities come out as the caps
    /// list of the emitted adapter binding, and nothing else does.
    #[test]
    fn declared_capabilities_become_bound_adapters() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("component.toml"), MANIFEST).expect("write manifest");
        let facts = derive_manifest_facts(dir.path(), false).expect("facts");
        let emitted = flat(adapter_bind(&facts.world.adapters));
        assert_eq!(
            emitted,
            "::nexum_sdk::bind_host_via_wit_bindgen!(caps:[logging]);",
        );
    }

    #[test]
    fn no_events_emits_no_parity_check() {
        assert!(topic_parity_check(&[], &[B256::ZERO]).is_empty());
    }

    #[test]
    fn parity_check_pins_both_directions() {
        let event: syn::Path = syn::parse_quote!(OrderPlacement);
        let topic: B256 = "0xcf5f9de2984132265203b5c335b25727702ca77262ff622e136baa7362bf1da9"
            .parse()
            .unwrap();
        let emitted = topic_parity_check(&[event], &[topic]).to_string();
        assert!(emitted.contains("SIGNATURE_HASH"), "{emitted}");
        assert!(emitted.contains("contains_topic"), "{emitted}");
        assert!(
            emitted.contains("`OrderPlacement`'s topic-0 is not among"),
            "{emitted}",
        );
        assert!(
            emitted.contains("is not the topic-0 of any of subscribes(OrderPlacement)"),
            "{emitted}",
        );
        // Topic bytes are embedded, not re-parsed at build time.
        assert!(emitted.contains("207u8"), "{emitted}");
    }

    /// Const eval stops at the first failing assert, so whichever fires must
    /// carry the code-side event and the manifest-side topics together.
    #[test]
    fn either_refusal_alone_names_both_sides() {
        let events: Vec<syn::Path> = vec![syn::parse_quote!(Placed), syn::parse_quote!(Filled)];
        let topics = [B256::with_last_byte(1), B256::with_last_byte(2)];
        let emitted = topic_parity_check(&events, &topics).to_string();
        let msgs = refusal_messages(&emitted);
        assert_eq!(msgs.len(), 4, "one per event plus one per topic");
        for msg in msgs {
            assert!(msg.contains("Placed") || msg.contains("Filled"), "{msg}");
            assert!(
                msg.contains(&topics[0].to_string()) || msg.contains(&topics[1].to_string()),
                "{msg}",
            );
        }
    }

    /// Both manifests the world is derived from are `include_bytes!`ed, so
    /// editing either retriggers expansion.
    #[test]
    fn every_manifest_read_is_a_rebuild_anchor() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("component.toml"),
            format!("{MANIFEST}{SUBSCRIPTION}"),
        )
        .expect("write manifest");
        std::fs::write(
            dir.path().join("extensions.toml"),
            "[extensions.acme]\nimport = \"acme:host/acme@0.1.0\"\n",
        )
        .expect("write registry");

        let facts = derive_manifest_facts(dir.path(), true).expect("facts");
        let emitted = rebuild_anchors(&facts.anchors).to_string();
        for manifest in ["component.toml", "extensions.toml"] {
            let anchor = dir.path().join(manifest);
            assert!(
                facts
                    .anchors
                    .contains(&anchor.to_string_lossy().into_owned()),
                "{manifest} is not anchored",
            );
            assert!(emitted.contains(manifest), "{emitted}");
        }
        assert_eq!(emitted.matches("include_bytes").count(), 2, "{emitted}");
    }

    /// A manifest field only `subscribes(...)` reads must not fail the build
    /// of a module that does not name it.
    #[test]
    fn topics_are_read_only_when_the_attribute_names_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = format!(
            "{MANIFEST}\n[[subscription]]\nkind = \"chain-log\"\nchain_id = 1\n\
             event_signature = \"not-a-topic\"\n"
        );
        std::fs::write(dir.path().join("component.toml"), manifest).expect("write manifest");

        assert!(derive_manifest_facts(dir.path(), false).is_ok());
        let err = derive_manifest_facts(dir.path(), true).err().unwrap();
        let MacroError::Manifest { source, .. } = &err else {
            panic!("unexpected refusal: {err:?}");
        };
        assert!(
            matches!(
                &**source,
                nexum_world::WorldError::InvalidTopic { topic, .. } if topic == "not-a-topic"
            ),
            "{err:?}",
        );
    }

    /// A missing `component.toml` refuses with the guidance-bearing
    /// variant, not a bare io error.
    #[test]
    fn missing_manifest_is_refused_as_unreadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = derive_manifest_facts(dir.path(), false).err().unwrap();
        assert!(
            matches!(err, MacroError::ManifestUnreadable { .. }),
            "{err:?}"
        );
    }

    /// An `extensions.toml` that exists but cannot be read (here: not
    /// UTF-8, which fails `read_to_string` even when running as root)
    /// refuses with the registry's own variant.
    #[test]
    fn unreadable_registry_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("component.toml"), MANIFEST).expect("write manifest");
        std::fs::write(dir.path().join("extensions.toml"), b"\xff\xfe").expect("write registry");
        let err = derive_manifest_facts(dir.path(), false).err().unwrap();
        assert!(
            matches!(err, MacroError::RegistryUnreadable { .. }),
            "{err:?}"
        );
    }

    /// `subscribes(...)` needs a manifest chain-log subscription; one with
    /// an `event_signature` satisfies it.
    #[test]
    fn subscribes_without_chain_log_subscription_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("component.toml"), MANIFEST).expect("write manifest");
        let err = derive_manifest_facts(dir.path(), true).err().unwrap();
        assert!(
            matches!(err, MacroError::SubscribesWithoutChainLog { .. }),
            "{err:?}"
        );

        std::fs::write(
            dir.path().join("component.toml"),
            format!("{MANIFEST}{SUBSCRIPTION}"),
        )
        .expect("write manifest");
        assert!(derive_manifest_facts(dir.path(), true).is_ok());
    }

    const MANIFEST: &str = "[component]\nname = \"t\"\n\n[dependencies]\nlogging = {}\n";

    /// A chain-log subscription with a valid `event_signature`, appended
    /// to [`MANIFEST`] where a test wants topics.
    const SUBSCRIPTION: &str = "\n[[subscription]]\nkind = \"chain-log\"\nchain_id = 1\n\
         event_signature = \"0xcf5f9de2984132265203b5c335b25727702ca77262ff622e136baa7362bf1da9\"\n";

    /// The string literals the emitted `assert!`s carry.
    fn refusal_messages(emitted: &str) -> Vec<String> {
        emitted
            .split("topic drift: ")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap_or_default().to_owned())
            .collect()
    }

    /// One pin per variant: the rendered text is the module-author
    /// contract, so a rewording fails exactly one of these.
    mod wording {
        use super::*;

        fn site() -> proc_macro2::Span {
            proc_macro2::Span::call_site()
        }

        #[test]
        fn non_ident_argument() {
            assert_eq!(
                MacroError::NonIdentArgument { span: site() }.to_string(),
                "expected `subscribes(EventType, ...)` or no arguments",
            );
        }

        #[test]
        fn unknown_argument() {
            assert_eq!(
                MacroError::UnknownArgument { span: site() }.to_string(),
                "#[nexum_sdk::module] takes no arguments except `subscribes(EventType, ...)`",
            );
        }

        #[test]
        fn trailing_tokens() {
            assert_eq!(
                MacroError::TrailingTokens { span: site() }.to_string(),
                "unexpected tokens after `subscribes(...)`",
            );
        }

        #[test]
        fn empty_subscribes() {
            assert_eq!(
                MacroError::EmptySubscribes { span: site() }.to_string(),
                "`subscribes(...)` must name at least one event type",
            );
        }

        #[test]
        fn malformed_args() {
            let inner = syn::Error::new(site(), "expected identifier");
            assert_eq!(
                MacroError::MalformedArgs(inner).to_string(),
                "expected identifier",
            );
        }

        #[test]
        fn unnamed_self_type() {
            assert_eq!(
                MacroError::UnnamedSelfType {
                    self_ty: quote!(&Alerts),
                }
                .to_string(),
                "#[nexum_sdk::module] must be applied to an inherent impl of a named type",
            );
        }

        #[test]
        fn trait_impl() {
            assert_eq!(
                MacroError::TraitImpl {
                    trait_path: quote!(Handler),
                }
                .to_string(),
                "#[nexum_sdk::module] must be applied to an inherent impl, not a trait impl",
            );
        }

        #[test]
        fn generic_impl() {
            assert_eq!(
                MacroError::GenericImpl {
                    generics: quote!(<T>),
                }
                .to_string(),
                "#[nexum_sdk::module] must be applied to a non-generic impl",
            );
        }

        #[test]
        fn unknown_handler() {
            assert_eq!(
                MacroError::UnknownHandler {
                    ident: quote!(on_blocks),
                }
                .to_string(),
                "`on_blocks` is not a recognized #[nexum_sdk::module] handler; expected one of \
                 [\"init\", \"on_block\", \"on_chain_logs\", \"on_tick\", \"on_custom\"] (rename \
                 helpers so they do not start with `on_`)",
            );
        }

        #[test]
        fn no_handlers() {
            assert_eq!(
                MacroError::NoHandlers {
                    self_ty: quote!(Alerts),
                }
                .to_string(),
                "#[nexum_sdk::module] found no recognized handlers on this impl; define at least \
                 one of `init`, `on_block`, `on_chain_logs`, `on_tick`, `on_custom`",
            );
        }

        #[test]
        fn no_manifest_dir() {
            assert_eq!(
                MacroError::NoManifestDir {
                    source: nexum_world::WorldError::NoManifestDir,
                }
                .to_string(),
                "CARGO_MANIFEST_DIR is not set",
            );
        }

        #[test]
        fn manifest_unreadable() {
            let err = MacroError::ManifestUnreadable {
                path: "/m/component.toml".into(),
                source: std::io::Error::other("denied"),
            };
            assert_eq!(
                err.to_string(),
                "could not read /m/component.toml (denied); #[nexum_sdk::module] derives the \
                 component's WIT world from the manifest's [dependencies] table, so the manifest \
                 must sit next to Cargo.toml",
            );
        }

        #[test]
        fn manifest_rule() {
            let err = MacroError::Manifest {
                path: "/m/component.toml".into(),
                source: Box::new(nexum_world::WorldError::DependenciesNotATable),
            };
            assert_eq!(
                err.to_string(),
                "/m/component.toml: [dependencies] must be a table",
            );
        }

        #[test]
        fn registry_unreadable() {
            let err = MacroError::RegistryUnreadable {
                path: "/m/extensions.toml".into(),
                source: std::io::Error::other("denied"),
            };
            assert_eq!(err.to_string(), "could not read /m/extensions.toml: denied");
        }

        #[test]
        fn wit_resolution() {
            let err = MacroError::WitResolution {
                source: nexum_world::WorldError::NoWitTree { start: "/m".into() },
            };
            assert_eq!(
                err.to_string(),
                "no `wit/` tree exists under /m or any ancestor",
            );
        }

        #[test]
        fn subscribes_without_chain_log() {
            let err = MacroError::SubscribesWithoutChainLog {
                path: "/m/component.toml".into(),
            };
            assert_eq!(
                err.to_string(),
                "`subscribes(...)` names events, but /m/component.toml declares no chain-log \
                 subscription with an `event_signature`; add the subscription or drop the argument",
            );
        }
    }
}
