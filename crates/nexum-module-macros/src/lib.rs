//! Proc-macro glue for nexum runtime modules.
//!
//! [`module`] turns an `impl` block of named handlers into a complete
//! per-cdylib module. Reach it through `nexum_sdk::module`, not this
//! crate directly; a downstream layer supplies its own venue-side macros.

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
    let args = syn::parse_macro_input!(attr as ModuleArgs);

    let input = syn::parse_macro_input!(item as ItemImpl);

    let self_ty: &syn::Type = &input.self_ty;
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
    // reserve the `on_` prefix for the recognized handler set.
    for item in &input.items {
        if let ImplItem::Fn(f) = item {
            let name = f.sig.ident.to_string();
            if name.starts_with("on_") && !HANDLERS.contains(&name.as_str()) {
                return syn::Error::new_spanned(
                    &f.sig.ident,
                    format!(
                        "`{name}` is not a recognized #[nexum_sdk::module] handler; expected one \
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
            "#[nexum_sdk::module] found no recognized handlers on this impl; define at least one \
             of `init`, `on_block`, `on_chain_logs`, `on_tick`, `on_custom`",
        )
        .to_compile_error()
        .into();
    }
    let has = |name: &str| present.contains(&name);

    let facts = match nexum_world::manifest_dir()
        .and_then(|dir| derive_manifest_facts(&dir, !args.subscribes.is_empty()))
    {
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
    let adapter_bind = adapter_bind(&module_world.adapters);

    let init_impl = init_export(self_ty, has("init"));

    let block_arm = dispatch_arm(self_ty, &present, "on_block", "Block");
    let logs_arm = dispatch_arm(self_ty, &present, "on_chain_logs", "ChainLogs");
    let tick_arm = dispatch_arm(self_ty, &present, "on_tick", "Tick");
    let custom_arm = dispatch_arm(self_ty, &present, "on_custom", "Custom");

    let anchors = rebuild_anchors(&anchors);
    quote! {
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

struct ManifestFacts {
    /// Rebuild anchor paths: the manifests the emitted world depends on.
    anchors: Vec<String>,
    world: nexum_world::ModuleWorld,
    /// Distinct chain-log `event_signature` topics, in declaration order.
    chain_log_topics: Vec<B256>,
    path: String,
}

/// Synthesize the per-module world from the crate's `component.toml`
/// `[dependencies]` plus the nearest ancestor `extensions.toml`.
/// Topics are read only for `want_topics`, so a manifest field no
/// opted-in module names can never fail a build.
fn derive_manifest_facts(
    crate_dir: &std::path::Path,
    want_topics: bool,
) -> Result<ManifestFacts, String> {
    let manifest_path = crate_dir.join("component.toml");
    let text = std::fs::read_to_string(&manifest_path).map_err(|e| {
        format!(
            "could not read {} ({e}); #[nexum_sdk::module] derives the component's WIT world \
             from the manifest's [dependencies] table, so the manifest must sit next to \
             Cargo.toml",
            manifest_path.display()
        )
    })?;
    let declared = nexum_world::manifest_capabilities(&text)
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let manifest_path = manifest_path.to_string_lossy().into_owned();
    let chain_log_topics = if want_topics {
        nexum_world::manifest_chain_log_topics(&text)
            .map_err(|e| format!("{manifest_path}: {e}"))?
    } else {
        Vec::new()
    };

    let mut anchors = vec![manifest_path.clone()];
    let extensions = match nexum_world::find_extensions_manifest(crate_dir) {
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
        // Foreign syn::Error; pins our macro message.
        assert!(err.to_string().contains("at least one event type"), "{err}");
    }

    #[test]
    fn unknown_argument_is_rejected() {
        let err = parse_args(quote! { emits(Foo) }).err().unwrap();
        // Foreign syn::Error; pins our macro message.
        assert!(
            err.to_string().contains("subscribes(EventType, ...)"),
            "{err}"
        );
        let err = parse_args(quote! { subscribes(Foo), extra }).err().unwrap();
        // Foreign syn::Error; pins our macro message.
        assert!(err.to_string().contains("unexpected tokens"), "{err}");
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
        std::fs::write(dir.path().join("component.toml"), MANIFEST).expect("write manifest");
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
        assert!(err.contains("invalid topic \"not-a-topic\""), "{err}");
    }

    const MANIFEST: &str = "[component]\nname = \"t\"\n\n[dependencies]\nlogging = {}\n";

    /// The string literals the emitted `assert!`s carry.
    fn refusal_messages(emitted: &str) -> Vec<String> {
        emitted
            .split("topic drift: ")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap_or_default().to_owned())
            .collect()
    }
}
