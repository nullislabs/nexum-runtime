use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "../../wit/shepherd-cow",
    world: "shepherd-module",
});

struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

// -- Stub implementations for host interfaces --

impl web3::runtime::types::Host for HostState {}

impl shepherd::cow::cow::Host for HostState {
    fn request(
        &mut self,
        _chain_id: u64,
        method: String,
        path: String,
        _body: Option<String>,
    ) -> Result<String, shepherd::cow::cow::ApiError> {
        eprintln!("[cow] {method} {path}");
        Err(shepherd::cow::cow::ApiError {
            status: 501,
            message: "not implemented".into(),
            body: None,
        })
    }
}

impl shepherd::cow::order::Host for HostState {
    fn submit(
        &mut self,
        _chain_id: u64,
        _order_data: Vec<u8>,
    ) -> Result<String, String> {
        eprintln!("[order] submit");
        Err("not implemented".into())
    }
}

impl web3::runtime::csn::Host for HostState {
    fn request(
        &mut self,
        _chain_id: u64,
        method: String,
        _params: String,
    ) -> Result<String, web3::runtime::csn::JsonRpcError> {
        eprintln!("[csn] request: {method}");
        Err(web3::runtime::csn::JsonRpcError {
            code: -32601,
            message: format!("method not implemented: {method}"),
            data: None,
        })
    }
}

impl web3::runtime::local_store::Host for HostState {
    fn get(&mut self, key: String) -> Result<Option<Vec<u8>>, String> {
        eprintln!("[local-store] get: {key}");
        Ok(None)
    }

    fn set(&mut self, key: String, _value: Vec<u8>) -> Result<(), String> {
        eprintln!("[local-store] set: {key}");
        Ok(())
    }

    fn delete(&mut self, key: String) -> Result<(), String> {
        eprintln!("[local-store] delete: {key}");
        Ok(())
    }

    fn list_keys(&mut self, prefix: String) -> Result<Vec<String>, String> {
        eprintln!("[local-store] list-keys: {prefix}");
        Ok(vec![])
    }
}

impl web3::runtime::remote_store::Host for HostState {
    fn upload(
        &mut self,
        _data: Vec<u8>,
    ) -> Result<Vec<u8>, web3::runtime::remote_store::StoreError> {
        Err(web3::runtime::remote_store::StoreError {
            code: 501,
            message: "not implemented".into(),
        })
    }

    fn download(
        &mut self,
        _reference: Vec<u8>,
    ) -> Result<Vec<u8>, web3::runtime::remote_store::StoreError> {
        Err(web3::runtime::remote_store::StoreError {
            code: 501,
            message: "not implemented".into(),
        })
    }

    fn feed_get(
        &mut self,
        _owner: Vec<u8>,
        _topic: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, web3::runtime::remote_store::StoreError> {
        Err(web3::runtime::remote_store::StoreError {
            code: 501,
            message: "not implemented".into(),
        })
    }

    fn feed_set(
        &mut self,
        _topic: Vec<u8>,
        _data: Vec<u8>,
    ) -> Result<Vec<u8>, web3::runtime::remote_store::StoreError> {
        Err(web3::runtime::remote_store::StoreError {
            code: 501,
            message: "not implemented".into(),
        })
    }
}

impl web3::runtime::msg::Host for HostState {
    fn publish(
        &mut self,
        content_topic: String,
        _payload: Vec<u8>,
    ) -> Result<(), web3::runtime::msg::MsgError> {
        eprintln!("[msg] publish: {content_topic}");
        Err(web3::runtime::msg::MsgError {
            code: 501,
            message: "not implemented".into(),
        })
    }

    fn query(
        &mut self,
        content_topic: String,
        _start_time: Option<u64>,
        _end_time: Option<u64>,
        _limit: Option<u32>,
    ) -> Result<Vec<web3::runtime::msg::Message>, web3::runtime::msg::MsgError> {
        eprintln!("[msg] query: {content_topic}");
        Ok(vec![])
    }
}

impl web3::runtime::logging::Host for HostState {
    fn log(&mut self, level: web3::runtime::logging::Level, message: String) {
        let level_str = match level {
            web3::runtime::logging::Level::Trace => "TRACE",
            web3::runtime::logging::Level::Debug => "DEBUG",
            web3::runtime::logging::Level::Info => "INFO",
            web3::runtime::logging::Level::Warn => "WARN",
            web3::runtime::logging::Level::Error => "ERROR",
        };
        eprintln!("[{level_str}] {message}");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let wasm_path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: nexum-runtime <path-to-component.wasm>"))?;

    println!("nexum-runtime: loading component from {wasm_path}");

    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;

    let component =
        Component::from_file(&engine, &wasm_path).context("failed to load component")?;

    let mut linker = Linker::<HostState>::new(&engine);
    ShepherdModule::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(
        &mut linker,
        |state| state,
    )?;
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

    let wasi = WasiCtxBuilder::new()
        .inherit_stdio()
        .build();

    let mut store = Store::new(
        &engine,
        HostState {
            wasi,
            table: ResourceTable::new(),
        },
    );

    let bindings = ShepherdModule::instantiate(&mut store, &component, &linker)
        .context("failed to instantiate component")?;

    // Call init with config
    println!("nexum-runtime: calling init...");
    let config_entries: Config = vec![
        ("name".into(), "example".into()),
    ];
    match bindings.call_init(&mut store, &config_entries)? {
        Ok(()) => println!("nexum-runtime: init succeeded"),
        Err(e) => println!("nexum-runtime: init failed: {e}"),
    }

    // Dispatch a test block event
    println!("nexum-runtime: dispatching test block event...");
    let block = web3::runtime::types::BlockData {
        chain_id: 1,
        number: 19_000_000,
        hash: vec![0xab; 32],
        timestamp: 1_700_000_000,
    };
    let event = web3::runtime::types::Event::Block(block);
    match bindings.call_on_event(&mut store, &event)? {
        Ok(()) => println!("nexum-runtime: on-event succeeded"),
        Err(e) => println!("nexum-runtime: on-event failed: {e}"),
    }

    println!("nexum-runtime: done");
    Ok(())
}

use anyhow::Context as _;
