//! # example (reference module)
//!
//! Minimal reference module: one handler per trigger, each logging a
//! one-line summary. The smallest demonstration of
//! `#[nexum_sdk::module]`, which supplies the wit-bindgen call, host
//! adapter, dispatch, and `export!`.

// wit_bindgen::generate! expands to host-import shims whose arity matches
// the WIT signatures, which can exceed clippy's too-many-arguments threshold.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![allow(clippy::too_many_arguments)]
// wit_bindgen::generate! output carries no doc comments.
#![allow(missing_docs)]

use nexum::host::{logging, types};

struct ExampleModule;

#[nexum_sdk::module]
impl ExampleModule {
    fn init(config: Vec<(String, String)>) -> Result<(), Fault> {
        let name = config
            .iter()
            .find(|(k, _)| k == "name")
            .map(|(_, v)| v.as_str())
            .unwrap_or("unknown");
        logging::log(
            logging::Level::Info,
            &format!("example module init (name={name})"),
        );
        Ok(())
    }

    fn on_block(block: types::Block) -> Result<(), Fault> {
        logging::log(
            logging::Level::Info,
            &format!(
                "block {} on chain {} (ts={}ms)",
                block.number, block.chain_id, block.timestamp
            ),
        );
        Ok(())
    }

    fn on_event(log: types::Log) -> Result<(), Fault> {
        logging::log(
            logging::Level::Info,
            &format!(
                "event with {} topics on chain {}",
                log.topics.len(),
                log.chain_id,
            ),
        );
        Ok(())
    }

    fn on_schedule(tick: types::ScheduleTick) -> Result<(), Fault> {
        logging::log(
            logging::Level::Info,
            &format!("schedule fired at {}ms", tick.fired_at),
        );
        Ok(())
    }

    fn on_extension(trigger: types::ExtensionTrigger) -> Result<(), Fault> {
        logging::log(
            logging::Level::Info,
            &format!(
                "extension trigger kind {} ({} payload bytes)",
                trigger.extension_kind,
                trigger.payload.len(),
            ),
        );
        Ok(())
    }
}
