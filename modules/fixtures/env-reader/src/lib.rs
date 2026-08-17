//! # env-reader (test fixture)
//!
//! Logs the number of environment variables, process arguments, and stdin
//! bytes the guest can see. Under `wasm32-wasip2` the first two route to
//! the ambient `wasi:cli/environment` interface and the read to
//! `wasi:cli/stdin`, so a test can assert the supervisor hands every guest
//! an empty environment, empty arguments, and empty stdin, and would catch
//! the store starting to inherit the host's.
//! Test-only.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: [
        "../../../wit/nexum-host",
    ],
    world: "nexum:host/trigger-module",
    generate_all,
});

use nexum::host::{logging, types};

struct EnvReader;

impl Guest for EnvReader {
    fn init(_config: Vec<(String, String)>) -> Result<(), Fault> {
        // Minimal SDK-free fixture: no tracing subscriber is installed,
        // so log through the raw host binding directly.
        logging::log(logging::Level::Info, "env-reader init");
        Ok(())
    }

    fn on_trigger(_trigger: types::Trigger) -> Result<(), Fault> {
        // Counts alone decide the assertion; the keys are logged too so a
        // failure names what leaked rather than only how much.
        let vars: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
        let args: Vec<String> = std::env::args().collect();
        // Bytes only for stdin: a count proves the leak without copying
        // whatever the host was fed into the log stream.
        let mut buf = Vec::new();
        let stdin = match std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf) {
            Ok(n) => n.to_string(),
            Err(err) => format!("unreadable ({err})"),
        };
        logging::log(
            logging::Level::Info,
            &format!("env vars {} args {} stdin {stdin}", vars.len(), args.len()),
        );
        for key in &vars {
            logging::log(logging::Level::Info, &format!("env key {key}"));
        }
        for arg in &args {
            logging::log(logging::Level::Info, &format!("env arg {arg}"));
        }
        Ok(())
    }
}

export!(EnvReader);
