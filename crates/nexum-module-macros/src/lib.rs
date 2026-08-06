//! Proc-macro glue for nexum runtime modules.
//!
//! [`module`] turns an `impl` block of named handlers into a complete
//! per-cdylib module. Reach it through `nexum_sdk::module`, not this
//! crate directly; the venue-side macros live in `videre-macros`.

use alloy_primitives::B256;
use proc_macro::TokenStream;
use quote::{ToTokens, quote};
use syn::{ImplItem, ItemImpl};

/// The handler names recognised on a `#[module]` impl. An `on_`-prefixed
/// method outside this set is a compile error; an absent handler
/// dispatches as a no-op.
const HANDLERS: [&str; 6] = [
    "init",
    "on_block",
    "on_chain_logs",
    "on_tick",
    "on_message",
    "on_custom",
];

/// Generate the per-cdylib glue for a nexum module.
///
/// Apply to an `impl` block whose associated functions are the event
/// handlers (`init`, `on_block`, `on_chain_logs`, `on_tick`,
/// `on_message`, `on_custom`); each takes its event's wit-bindgen
/// payload and returns `Result<(), Fault>`, and `init` takes the config
/// table. Undefined handlers dispatch as no-ops. Emits
/// `wit_bindgen::generate!`, the host adapter, the `Guest` impl, and
/// `export!` around the untouched impl.
///
/// The world is per module: the macro reads the crate's `module.toml`
/// and synthesizes a world importing exactly the
/// `[capabilities].required` and `optional` declarations, so the
/// load-time capability check passes by construction. An undeclared
/// capability's bindings do not exist. Requirements: the manifest sits
/// at the crate root with a `[capabilities]` section; the crate depends
/// on `wit-bindgen` directly; and the crate root must not shadow the
/// std prelude names `Result`, `Vec`, or `Ok` (the generated `Guest`
/// trait refers to them unqualified).
///
/// `subscribes(EventType, ...)` pins topic parity: the build fails
/// unless the named events' topic-0 hashes (`SolEvent::SIGNATURE_HASH`)
/// and the manifest's chain-log `event_signature` values match as sets.
/// The manifest stays authoritative; the attribute only guards it.
#[proc_macro_attribute]
pub fn module(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = syn::parse_macro_input!(attr as ModuleArgs);

    let input = syn::parse_macro_input!(item as ItemImpl);

    let self_ty = &input.self_ty;
    if !nexum_world::is_plain_type(self_ty) {
        return syn::Error::new_spanned(
            self_ty,
            "#[nexum_sdk::module] must be applied to an inherent impl of a named type",
        )
        .to_compile_error()
        .into();
    }
    if let Some((_, trait_path, _)) = &input.trait_ {
        return syn::Error::new_spanned(
            trait_path,
            "#[nexum_sdk::module] must be applied to an inherent impl, not a trait impl",
        )
        .to_compile_error()
        .into();
    }
    if !input.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &input.generics,
            "#[nexum_sdk::module] must be applied to a non-generic impl",
        )
        .to_compile_error()
        .into();
    }

    // A typo'd handler (`on_blocks`, `on_chainlogs`, ...) would otherwise
    // compile as an ordinary helper while its event silently no-ops, so
    // reserve the `on_` prefix for the recognised handler set.
    for item in &input.items {
        if let ImplItem::Fn(f) = item {
            let name = f.sig.ident.to_string();
            if name.starts_with("on_") && !HANDLERS.contains(&name.as_str()) {
                return syn::Error::new_spanned(
                    &f.sig.ident,
                    format!(
                        "`{name}` is not a recognised #[nexum_sdk::module] handler; expected one \
                         of {HANDLERS:?} (rename helpers so they do not start with `on_`)"
                    ),
                )
                .to_compile_error()
                .into();
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
        return syn::Error::new_spanned(
            self_ty,
            "#[nexum_sdk::module] found no recognised handlers on this impl; define at least one \
             of `init`, `on_block`, `on_chain_logs`, `on_tick`, `on_message`, `on_custom`",
        )
        .to_compile_error()
        .into();
    }
    let has = |name: &str| present.contains(&name);

    let facts = match derive_manifest_facts() {
        Ok(facts) => facts,
        Err(msg) => {
            return syn::Error::new(proc_macro2::Span::call_site(), msg)
                .to_compile_error()
                .into();
        }
    };
    let ManifestFacts {
        anchors,
        world: module_world,
        chain_log_topics,
        path: manifest_path,
    } = facts;
    if !args.subscribes.is_empty() && chain_log_topics.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            format!(
                "`subscribes(...)` names events, but {manifest_path} declares no chain-log \
                 subscription with an `event_signature`; add the subscription or drop the argument"
            ),
        )
        .to_compile_error()
        .into();
    }
    let parity = topic_parity_check(&args.subscribes, &chain_log_topics);
    let wit_paths = match nexum_world::manifest_wit_packages(&module_world.packages) {
        Ok(paths) => paths,
        Err(msg) => {
            return syn::Error::new(proc_macro2::Span::call_site(), msg)
                .to_compile_error()
                .into();
        }
    };
    let inline_world = &module_world.wit;
    let adapter_caps: Vec<syn::Ident> = module_world
        .adapters
        .iter()
        .map(|cap| syn::Ident::new(cap, proc_macro2::Span::call_site()))
        .collect();

    // `init` is a required export; when the handler is absent the config
    // is bound but unused, so drop it to keep the module warning-clean.
    let init_impl = if has("init") {
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
    };

    let arm = |handler: &str, variant| -> proc_macro2::TokenStream {
        let variant = syn::Ident::new(variant, proc_macro2::Span::call_site());
        if has(handler) {
            let call = syn::Ident::new(handler, proc_macro2::Span::call_site());
            quote! { nexum::host::types::Event::#variant(payload) => <#self_ty>::#call(payload), }
        } else {
            quote! { nexum::host::types::Event::#variant(_) => ::core::result::Result::Ok(()), }
        }
    };
    let block_arm = arm("on_block", "Block");
    let logs_arm = arm("on_chain_logs", "ChainLogs");
    let tick_arm = arm("on_tick", "Tick");
    let message_arm = arm("on_message", "Message");
    let custom_arm = arm("on_custom", "Custom");

    quote! {
        // Anchor a rebuild on the manifest and the extension registry:
        // the emitted world is derived from them, so an edit to either
        // must recompile the module.
        #(const _: &[u8] = ::core::include_bytes!(#anchors);)*

        wit_bindgen::generate!({
            inline: #inline_world,
            path: [#(#wit_paths),*],
            world: "nexum:module-world/module",
            generate_all,
        });

        ::nexum_sdk::bind_host_via_wit_bindgen!(caps: [#(#adapter_caps),*]);

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
                    #message_arm
                    #custom_arm
                }
            }
        }

        export!(__NexumModuleExport);
    }
    .into()
}

/// The macro's arguments: bare, or `subscribes(EventType, ...)`.
struct ModuleArgs {
    subscribes: Vec<syn::Path>,
}

impl syn::parse::Parse for ModuleArgs {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(Self {
                subscribes: Vec::new(),
            });
        }
        let ident: syn::Ident = input.parse().map_err(|e| {
            syn::Error::new(
                e.span(),
                "expected `subscribes(EventType, ...)` or no arguments",
            )
        })?;
        if ident != "subscribes" {
            return Err(syn::Error::new(
                ident.span(),
                "#[nexum_sdk::module] takes no arguments except `subscribes(EventType, ...)`",
            ));
        }
        let inner;
        syn::parenthesized!(inner in input);
        if !input.is_empty() {
            return Err(input.error("unexpected tokens after `subscribes(...)`"));
        }
        let paths = inner
            .call(syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)?;
        if paths.is_empty() {
            return Err(syn::Error::new(
                ident.span(),
                "`subscribes(...)` must name at least one event type",
            ));
        }
        Ok(Self {
            subscribes: paths.into_iter().collect(),
        })
    }
}

/// Everything the macro derives from the crate's manifests.
struct ManifestFacts {
    /// Rebuild anchor paths: the manifests the emitted world depends on.
    anchors: Vec<String>,
    world: nexum_world::ModuleWorld,
    /// Distinct chain-log `event_signature` topics, in declaration order.
    chain_log_topics: Vec<B256>,
    /// The module manifest path, for diagnostics.
    path: String,
}

/// Synthesize the per-module world from the crate's `module.toml`
/// `[capabilities]` plus the nearest ancestor `extensions.toml`.
fn derive_manifest_facts() -> Result<ManifestFacts, String> {
    let crate_dir = nexum_world::manifest_dir()?;
    let manifest_path = crate_dir.join("module.toml");
    let text = std::fs::read_to_string(&manifest_path).map_err(|e| {
        format!(
            "could not read {} ({e}); #[nexum_sdk::module] derives the component's WIT world \
             from the manifest's [capabilities] section, so the manifest must sit next to \
             Cargo.toml",
            manifest_path.display()
        )
    })?;
    let declared = nexum_world::manifest_capabilities(&text)
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let manifest_path = manifest_path.to_string_lossy().into_owned();
    let chain_log_topics = nexum_world::manifest_chain_log_topics(&text)
        .map_err(|e| format!("{manifest_path}: {e}"))?;

    let mut anchors = vec![manifest_path.clone()];
    let extensions = match nexum_world::find_extensions_manifest(&crate_dir) {
        None => Vec::new(),
        Some(registry) => {
            let text = std::fs::read_to_string(&registry)
                .map_err(|e| format!("could not read {}: {e}", registry.display()))?;
            let rows = nexum_world::manifest_extensions(&text)
                .map_err(|e| format!("{}: {e}", registry.display()))?;
            anchors.push(registry.to_string_lossy().into_owned());
            rows
        }
    };
    let world = nexum_world::synthesize(&declared, &extensions)
        .map_err(|e| format!("{manifest_path}: {e}"))?;
    Ok(ManifestFacts {
        anchors,
        world,
        chain_log_topics,
        path: manifest_path,
    })
}

/// Const assertions pinning set equality between the `subscribes(...)`
/// events' topic-0 hashes and the manifest's chain-log topics. Empty
/// `events` emits nothing: the parity check is opt-in.
fn topic_parity_check(events: &[syn::Path], topics: &[B256]) -> proc_macro2::TokenStream {
    if events.is_empty() {
        return proc_macro2::TokenStream::new();
    }
    let n = events.len();
    let m = topics.len();
    let manifest_topics = topics.iter().map(|topic| {
        let bytes = topic.0;
        quote! { ::nexum_sdk::prelude::B256::new([#(#bytes),*]) }
    });
    let declared_topics = events.iter().map(|path| {
        quote! { <#path as ::nexum_sdk::sol_types::SolEvent>::SIGNATURE_HASH }
    });
    let declared_checks = events.iter().enumerate().map(|(i, path)| {
        let msg = format!(
            "module.toml has no chain-log subscription whose event_signature is the topic-0 of \
             `{}`",
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
            "module.toml chain-log event_signature {topic} is not the topic-0 of any event named \
             in `subscribes(...)`",
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

/// A path's source spelling, without quote's token spacing.
fn path_string(path: &syn::Path) -> String {
    path.to_token_stream().to_string().replace(' ', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(tokens: proc_macro2::TokenStream) -> syn::Result<ModuleArgs> {
        syn::parse2(tokens)
    }

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
        assert!(err.to_string().contains("at least one event type"), "{err}");
    }

    #[test]
    fn unknown_argument_is_rejected() {
        let err = parse_args(quote! { emits(Foo) }).err().unwrap();
        assert!(
            err.to_string().contains("subscribes(EventType, ...)"),
            "{err}"
        );
        let err = parse_args(quote! { subscribes(Foo), extra }).err().unwrap();
        assert!(err.to_string().contains("unexpected tokens"), "{err}");
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
        // Declared-side refusal names the event.
        assert!(emitted.contains("topic-0 of `OrderPlacement`"), "{emitted}");
        // Manifest-side refusal names the unmatched topic.
        assert!(emitted.contains(&topic.to_string()), "{emitted}");
        // Topic bytes are embedded, not re-parsed at build time.
        assert!(emitted.contains("207u8"), "{emitted}");
    }
}
